//! Daemon side of the **pwrouter-agent** control plane
//! (docs/receiver-agent-plan.md §4-§9).
//!
//! A pw-sink target *is* a paired agent (§3): the helper on the receiver dials in
//! here, is approved once by a human, and from then on this module is the only way
//! the daemon touches that host — volume, mute, duck, unduck, and "become the
//! receiver for session X". There is no fallback path: nothing about this protocol
//! lets the daemon run or configure anything else on the host, which is the whole
//! reason it exists instead of a PulseAudio TCP port (§1.1).
//!
//! Three things live here:
//!
//! * **the token store** (`/data/agents.json`) — identity → token, so an approved
//!   host reconnects unattended across restarts;
//! * **the registry** — pending pair requests, live connections, each host's last
//!   reported state, and the outgoing command channel per host;
//! * **the WebSocket endpoint** — one connection per agent, pinged on a timer.
//!
//! A host asking to pair is not a separate kind of thing from an output: it is a
//! *discovered* one, and pairing it is the "Add" (`hosts`, and `api::adopt_output`).
//! Unpairing puts it back to discovered rather than making it vanish, because its
//! agent keeps dialling in — exactly how an unadopted speaker that is still on the
//! network behaves. Nothing the daemon answers ever makes an agent give up, so a
//! lost `agents.json` costs a click per host, not a login per host.
//!
//! Identity is *machine id + user*, not the hostname: one agent per logged-in
//! session (§13.2), so two users on one machine are two independent targets, and a
//! renamed host keeps its pairing.

use crate::pw::thread::ChangeNotifier;
use crate::util::locks::LockRecover;
use crate::util::node_names::{slugify, PWSINK_DEV_PREFIX};
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::IntoResponse;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, Mutex};

/// Protocol version this daemon speaks; `Hello` from another major is refused
/// rather than half-understood (the agent's `proto.rs` mirrors this).
pub const PROTOCOL_VERSION: u32 = 1;

/// How often the daemon pings a connected agent. The agent's own deadline is
/// twice this, so one missed ping is tolerated but a dead daemon is not (§9.2).
const PING_INTERVAL: Duration = Duration::from_secs(15);

/// Depth of one agent's outgoing command queue.
///
/// **Bounded, and generously** — the two halves of the no-unbounded-queues rule
/// pull in opposite directions here. This is a *control* lane (volume, mute, duck,
/// welcome, release), where dropping a message is worse than queueing one: a lost
/// unduck leaves a host quiet indefinitely. So it gets far more room than any
/// plausible burst (a UI drag is tens of messages, coalesced by the socket write
/// that follows) instead of the tight depth an audio queue wants — but a ceiling
/// all the same, because a host that has stopped reading its socket must not grow
/// this queue until the daemon dies. Reaching it means that connection is wedged,
/// and [`Agents::send`] says so and reports failure rather than dropping quietly.
const AGENT_MSG_DEPTH: usize = 64;

/// Depth of the process-global announcement duck queue (`duck_output` → the relay
/// task). Two messages per announcement per host, and the relay only blocks on the
/// registry lock, so this is orders of magnitude more than a burst of overlapping
/// announcements can produce.
const REMOTE_DUCK_DEPTH: usize = 64;

// ---- wire protocol (mirror of pwrouter-agent/src/proto.rs) ------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentMsg {
    /// `pair_code` is the code the agent's *process* offers (minted at its startup
    /// and logged there), so it survives reconnects. `#[serde(default)]` keeps an
    /// older agent working — the daemon mints one itself then. Never trusted as an
    /// authenticator: see [`valid_pair_code`].
    Hello {
        protocol: u32,
        agent_version: String,
        machine_id: String,
        hostname: String,
        user: String,
        token: Option<String>,
        #[serde(default)]
        pair_code: Option<String>,
    },
    State(HostState),
    ForeignSession {
        session: String,
    },
    Pong,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DaemonMsg {
    PairPending { code: String },
    Paired { token: String },
    Denied { reason: String },
    Welcome { session_name: String, ifname: Option<String>, jitter_ms: Option<u32>, keepalive_secs: u64 },
    Release,
    SetVolume { volume: f32 },
    SetMute { muted: bool },
    Duck { depth: f32, ramp_ms: u64 },
    Unduck { ramp_ms: u64 },
    Ping,
}

/// What a host reports about itself. `volume` is cubic 0.0-1.0 — the same scale
/// `wpctl` shows and HA's `volume_level` expects.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct HostState {
    pub volume: Option<f32>,
    pub muted: Option<bool>,
    pub sink_name: Option<String>,
    pub receiving: bool,
    pub ducked: bool,
}

// ---- store -----------------------------------------------------------------

/// One approved host, persisted.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PairedAgent {
    /// `<machine-id>:<user>`.
    pub identity: String,
    /// Display label, e.g. `david-local (david)`.
    pub label: String,
    /// Routing-matrix identity, `pwsink-dev-<slug>` — the name HA entities and
    /// the routing store use, so it must stay stable across re-pairings.
    pub node_name: String,
    /// Bearer token this host authenticates with.
    pub token: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct StoreFile {
    #[serde(default)]
    agents: Vec<PairedAgent>,
}

fn load_store(path: &Path) -> Vec<PairedAgent> {
    match std::fs::read(path) {
        Ok(bytes) => match serde_json::from_slice::<StoreFile>(&bytes) {
            Ok(file) => file.agents,
            Err(e) => {
                tracing::warn!("ignoring unreadable {}: {e}", path.display());
                Vec::new()
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => {
            tracing::warn!("could not read {}: {e}", path.display());
            Vec::new()
        }
    }
}

/// Writes the store atomically. Tokens are credentials, so the file is created
/// `0600` *before* any bytes reach it.
fn save_store(path: &Path, agents: &[PairedAgent]) {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;

    let json = match serde_json::to_vec_pretty(&StoreFile { agents: agents.to_vec() }) {
        Ok(json) => json,
        Err(e) => {
            tracing::error!("serialising the agent store failed: {e}");
            return;
        }
    };
    let tmp = path.with_extension("json.tmp");
    let write = || -> std::io::Result<()> {
        let mut file = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
        file.write_all(&json)?;
        file.sync_all()?;
        std::fs::rename(&tmp, path)
    };
    if let Err(e) = write() {
        tracing::error!("could not persist {}: {e}", path.display());
    }
}

/// Length of a pairing code, in characters: 3 random bytes as hex.
const PAIR_CODE_CHARS: usize = 6;

/// Accepts an agent-offered pairing code, or `None` if it isn't one.
///
/// The code is only ever *compared by a human* across two channels — the host's
/// own log and the add-on UI — so letting the agent choose it costs nothing:
/// approval is still a click, and the token is still minted here. What would cost
/// something is rendering whatever arrived: a rogue agent could offer a long
/// string, or one dressed up to be mistaken for the desktop next to it. So the
/// shape is pinned to exactly what this daemon would have generated, and anything
/// else is replaced by a code of our own rather than rejected — the host is not at
/// fault for running an older agent.
fn valid_pair_code(offered: Option<&str>) -> Option<String> {
    let code = offered?.trim();
    let ok = code.len() == PAIR_CODE_CHARS && code.chars().all(|c| c.is_ascii_hexdigit() && !c.is_lowercase());
    ok.then(|| code.to_ascii_uppercase())
}

/// Random hex from the kernel. Used for tokens and pairing codes — a
/// time-derived value would be guessable by anyone who can see the LAN.
fn random_hex(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    match std::fs::File::open("/dev/urandom").and_then(|mut f| std::io::Read::read_exact(&mut f, &mut buf)) {
        Ok(()) => buf.iter().map(|b| format!("{b:02x}")).collect(),
        Err(e) => {
            // Refusing is safer than a predictable token.
            tracing::error!("cannot read /dev/urandom: {e}");
            String::new()
        }
    }
}

// ---- discovery advert ------------------------------------------------------

/// mDNS service type agents browse for (mirrored in the agent's `client.rs`).
const CONTROL_SERVICE_TYPE: &str = "_pwrouter-ctl._tcp.local.";

/// Advertises this daemon's HTTP port so agents can find it without being told an
/// address (plan §8). Registered on the shared, LAN-restricted advertise daemon —
/// the same one the sendspin/AirPlay adverts use, so a `host_network` container
/// doesn't multicast across every Docker `veth` (the mDNS-storm fix).
///
/// Failure is not fatal: `pwrouter-agent run --daemon host:port` works without any
/// discovery at all.
pub fn advertise(port: u16) {
    let Some(daemon) = crate::discovery_supervisor::shared_advertise_daemon() else {
        tracing::warn!("no shared mDNS daemon; agents will need --daemon <host:port>");
        return;
    };
    let instance = "pwrouter";
    let host = format!("{instance}.local.");
    let props: [(&str, &str); 0] = [];
    match mdns_sd::ServiceInfo::new(CONTROL_SERVICE_TYPE, instance, &host, "", port, &props[..]) {
        Ok(info) => {
            let info = info.enable_addr_auto();
            match daemon.register(info) {
                Ok(()) => tracing::info!("advertising {CONTROL_SERVICE_TYPE} on port {port} for receiver agents"),
                Err(e) => tracing::warn!("could not advertise the agent control endpoint: {e}"),
            }
        }
        Err(e) => tracing::warn!("could not build the agent control advert: {e}"),
    }
}

// ---- registry --------------------------------------------------------------

/// A pending pair request: an agent is connected and waiting for a human.
struct Pending {
    identity: String,
    label: String,
    /// Resolved at `hello`, not at approval, because a pending host **is** a
    /// discovered output on the Outputs page and that page keys every row by node
    /// name. Deriving it early is what lets pairing be an ordinary "Add".
    node_name: String,
    /// Short code shown in the UI *and* logged by the agent, so the person
    /// approving can tell two simultaneous requests apart.
    code: String,
    tx: mpsc::Sender<DaemonMsg>,
}

/// A live, approved connection.
struct Live {
    label: String,
    state: HostState,
    tx: mpsc::Sender<DaemonMsg>,
}

pub struct Agents {
    path: PathBuf,
    paired: Vec<PairedAgent>,
    pending: Vec<Pending>,
    /// Keyed by node name — the routing-matrix identity.
    live: HashMap<String, Live>,
    changes: ChangeNotifier,
}

pub type SharedAgents = Arc<Mutex<Agents>>;

/// The identifying half of a `Hello`, borrowed from the message as it arrived.
///
/// One struct rather than six parameters: the fields travel together, and `hello`
/// derives the identity, label and node name from them in one place (§5) — which is
/// exactly why they are passed raw instead of pre-combined by the caller.
pub struct HelloClaim<'a> {
    pub protocol: u32,
    pub machine_id: &'a str,
    pub hostname: &'a str,
    pub user: &'a str,
    pub token: Option<&'a str>,
    /// The code the agent's process offers; validated, never trusted.
    pub pair_code: Option<&'a str>,
}

/// What a `Hello` resolves to.
pub enum HelloOutcome {
    /// Approved: this is the host's node name and the session it should receive.
    Welcome {
        node_name: String,
        label: String,
    },
    /// Waiting for approval; the code was already queued to the agent.
    Pending,
    Denied(String),
}

/// One row for the API/UI.
#[derive(Debug, Clone, Serialize)]
pub struct AgentInfo {
    pub identity: String,
    pub label: String,
    /// Known for a pending request too (see [`Pending::node_name`]), so the row can
    /// be matched to the output card it will become.
    pub node_name: String,
    pub paired: bool,
    pub connected: bool,
    /// Pairing code, only for pending requests.
    pub code: Option<String>,
    pub state: Option<HostState>,
}

/// One host for the Outputs listing: paired or merely asking to be. A pending host
/// is a *discovered* output — pairing it is what adds it (plan §7/§8) — so both
/// come from one call rather than the page stitching two lists together.
pub struct HostRow {
    pub node_name: String,
    pub label: String,
    /// An agent is on the socket right now.
    pub connected: bool,
    pub paired: bool,
    /// `Some` only while the host is waiting to be paired.
    pub pair_code: Option<String>,
}

impl Agents {
    pub fn new(path: PathBuf, changes: ChangeNotifier) -> Self {
        let paired = load_store(&path);
        if !paired.is_empty() {
            tracing::info!("loaded {} paired agent(s) from {}", paired.len(), path.display());
        }
        Self { path, paired, pending: Vec::new(), live: HashMap::new(), changes }
    }

    pub fn shared(path: PathBuf, changes: ChangeNotifier) -> SharedAgents {
        Arc::new(Mutex::new(Self::new(path, changes)))
    }

    /// Node name for a host. Includes the user because pairing is per session
    /// (§13.2); two users on one host must not collide.
    fn node_name_for(hostname: &str, user: &str) -> String {
        format!("{PWSINK_DEV_PREFIX}{}_{}", slugify(hostname), slugify(user))
    }

    fn label_for(hostname: &str, user: &str) -> String {
        format!("{hostname} ({user})")
    }

    /// The node name this identity should use — resolved for pending requests too,
    /// so a host is a listable output from its first hello.
    ///
    /// Three rules, in order. A host that paired before keeps its stored name, or
    /// its routing and HA entity ids would break on a re-pairing. Otherwise it gets
    /// the name derived from hostname + user. And if *another* identity already
    /// holds that name — two machines that share both a hostname and a username,
    /// which `node_name_for` alone cannot tell apart — the machine id disambiguates,
    /// because two hosts folded into one card would silently share a card, a routing
    /// row and an HA entity.
    fn node_name_for_identity(&self, identity: &str, hostname: &str, user: &str) -> String {
        if let Some(agent) = self.paired.iter().find(|a| a.identity == identity) {
            return agent.node_name.clone();
        }
        let base = Self::node_name_for(hostname, user);
        let taken = |candidate: &str| {
            self.paired.iter().any(|a| a.node_name == candidate)
                || self.pending.iter().any(|p| p.identity != identity && p.node_name == candidate)
        };
        if !taken(&base) {
            return base;
        }
        let machine = slugify(identity.split(':').next().unwrap_or(identity));
        for len in [6, 12, machine.len()] {
            if len == 0 || len > machine.len() {
                continue;
            }
            let candidate = format!("{base}_{}", &machine[..len]);
            if !taken(&candidate) {
                return candidate;
            }
        }
        // Same machine id *and* hostname *and* user as an existing entry but a
        // different identity is not reachable; fall back rather than loop.
        format!("{base}_{}", random_hex(3))
    }

    /// Handles a `Hello`. `tx` is this connection's outgoing queue.
    pub fn hello(&mut self, claim: HelloClaim<'_>, tx: mpsc::Sender<DaemonMsg>) -> HelloOutcome {
        let HelloClaim { protocol, machine_id, hostname, user, token, pair_code: offered_code } = claim;
        if protocol != PROTOCOL_VERSION {
            return HelloOutcome::Denied(format!("protocol {protocol} is not {PROTOCOL_VERSION}; update the agent or the add-on"));
        }
        let identity = format!("{machine_id}:{user}");
        let label = Self::label_for(hostname, user);

        if let Some(token) = token {
            let Some(agent) = self.paired.iter().find(|a| a.token == token && a.identity == identity).cloned() else {
                // Either the token was revoked here or it belongs to another
                // identity; in both cases the agent must pair again deliberately.
                return HelloOutcome::Denied("token not recognised; remove the agent's config and pair again".into());
            };
            // Label can change (hostname edit) — keep the stored one current, but
            // never the node name, which routing and HA entity ids depend on.
            if agent.label != label {
                if let Some(stored) = self.paired.iter_mut().find(|a| a.identity == identity) {
                    stored.label = label.clone();
                }
                save_store(&self.path, &self.paired);
            }
            self.live.insert(agent.node_name.clone(), Live { label: label.clone(), state: HostState::default(), tx });
            let _ = self.changes.send(());
            return HelloOutcome::Welcome { node_name: agent.node_name, label };
        }

        // No token: a pair request. Note what this does *not* do — it never disturbs
        // an existing pairing for the same identity. A tokenless hello is unauthenticated
        // and the identity in it is not a secret (machine id + user), so treating one as
        // "this host lost its token, re-pair it" would let anyone on the LAN knock a
        // paired desktop out of the outputs. Such a request becomes a pending row that
        // the already-paired card hides; the way out is Unpair, then Pair.
        //
        // The code is the agent's own where it offered a usable one — that is what keeps
        // it stable while it reconnects — else the one an earlier request from this
        // identity already showed, else ours.
        let previous = self.pending.iter().find(|p| p.identity == identity).map(|p| p.code.clone());
        let code = match valid_pair_code(offered_code).or(previous) {
            Some(code) => code,
            None => {
                let minted = random_hex(3).to_uppercase();
                if minted.is_empty() {
                    return HelloOutcome::Denied("daemon could not generate a pairing code".into());
                }
                minted
            }
        };
        let node_name = self.node_name_for_identity(&identity, hostname, user);
        // Replace any earlier request from the same identity (a restarted agent) so
        // the list can't fill up with stale rows.
        self.pending.retain(|p| p.identity != identity);
        let _ = tx.try_send(DaemonMsg::PairPending { code: code.clone() });
        tracing::info!("pairing requested by '{label}' ({identity}) as {node_name}, code {code}");
        self.pending.push(Pending { identity, label, node_name, code, tx });
        let _ = self.changes.send(());
        HelloOutcome::Pending
    }

    /// Approves a pending request: mints a token, persists it, and pushes it to
    /// the waiting agent (which reconnects with it).
    ///
    /// The caller adopts the output in the same breath (`api::adopt_output`):
    /// pairing a host *is* adding it, since a human ran the agent there and a human
    /// approved it here — there is no third intention left to express.
    pub fn approve(&mut self, identity: &str) -> Result<PairedAgent, String> {
        let Some(index) = self.pending.iter().position(|p| p.identity == identity) else {
            return Err("no pending pair request for that identity".into());
        };
        let pending = self.pending.remove(index);
        let token = random_hex(24);
        if token.is_empty() {
            return Err("daemon could not generate a token".into());
        }
        // The name was settled at `hello` (and a re-pairing host kept its old one),
        // so existing routing and HA entities keep working.
        let node_name = pending.node_name;

        let agent = PairedAgent { identity: identity.to_string(), label: pending.label, node_name, token: token.clone() };
        self.paired.retain(|a| a.identity != identity);
        self.paired.push(agent.clone());
        save_store(&self.path, &self.paired);
        let _ = pending.tx.try_send(DaemonMsg::Paired { token });
        tracing::info!("approved agent '{}' as {}", agent.label, agent.node_name);
        let _ = self.changes.send(());
        Ok(agent)
    }

    pub fn is_paired(&self, identity: &str) -> bool {
        self.paired.iter().any(|a| a.identity == identity)
    }

    /// The identity behind an output's node name, paired or pending. Lets the
    /// Outputs API act on a card without the UI having to know about identities.
    pub fn identity_for_node(&self, node_name: &str) -> Option<String> {
        self.paired
            .iter()
            .find(|a| a.node_name == node_name)
            .map(|a| a.identity.clone())
            .or_else(|| self.pending.iter().find(|p| p.node_name == node_name).map(|p| p.identity.clone()))
    }

    /// Revokes a pairing: the token stops working and the live connection (if
    /// any) is told it is no longer a receiver.
    ///
    /// The agent does **not** die of this. It drops the token it can no longer use
    /// and goes back to asking, so the host reappears as a pairable (discovered)
    /// output — the same thing an unadopted AirPlay speaker does when it is still on
    /// the network. Getting it back is a click here, never a login there.
    pub fn unpair(&mut self, identity: &str) -> Result<(), String> {
        let Some(agent) = self.paired.iter().find(|a| a.identity == identity).cloned() else {
            return Err("no such paired agent".into());
        };
        self.paired.retain(|a| a.identity != identity);
        save_store(&self.path, &self.paired);
        if let Some(live) = self.live.remove(&agent.node_name) {
            let _ = live.tx.try_send(DaemonMsg::Release);
            let _ = live.tx.try_send(DaemonMsg::Denied { reason: "pairing was removed".into() });
        }
        tracing::info!("unpaired '{}' ({})", agent.label, agent.node_name);
        let _ = self.changes.send(());
        Ok(())
    }

    /// Drops a live connection (socket closed).
    pub fn disconnected(&mut self, node_name: &str) {
        if self.live.remove(node_name).is_some() {
            let _ = self.changes.send(());
        }
    }

    pub fn update_state(&mut self, node_name: &str, state: HostState) {
        if let Some(live) = self.live.get_mut(node_name) {
            if live.state != state {
                live.state = state;
                let _ = self.changes.send(());
            }
        }
    }

    /// Queues one command for a host. False = it did not get through (not
    /// connected, or its queue is full).
    ///
    /// `try_send` rather than an awaited `send`: callers hold the registry lock,
    /// so waiting on one wedged host's socket here would block every other host's
    /// commands behind it. [`AGENT_MSG_DEPTH`] is deep enough that Full means the
    /// connection is broken rather than busy, which is worth a log — the ping loop
    /// hits the same wall a moment later and closes the socket.
    fn send(&self, node_name: &str, msg: DaemonMsg) -> bool {
        let Some(live) = self.live.get(node_name) else {
            return false;
        };
        match live.tx.try_send(msg) {
            Ok(()) => true,
            Err(mpsc::error::TrySendError::Full(_)) => {
                tracing::warn!(
                    "agent '{}' ({node_name}) is not draining its control queue ({AGENT_MSG_DEPTH} deep); \
                     dropping this command — the connection is wedged and will be dropped by the ping deadline",
                    live.label
                );
                false
            }
            Err(mpsc::error::TrySendError::Closed(_)) => false,
        }
    }

    /// Sets a host's master volume (cubic 0.0-1.0). False = not connected.
    pub fn set_volume(&self, node_name: &str, volume: f32) -> bool {
        self.send(node_name, DaemonMsg::SetVolume { volume: volume.clamp(0.0, 1.0) })
    }

    pub fn set_mute(&self, node_name: &str, muted: bool) -> bool {
        self.send(node_name, DaemonMsg::SetMute { muted })
    }

    /// Ducks *other* applications on the host's sink — our own stream is excluded
    /// by the agent, so an announcement stays at full level (§6.1, §11 P3).
    pub fn duck(&self, node_name: &str, depth: f32, ramp_ms: u64) -> bool {
        self.send(node_name, DaemonMsg::Duck { depth: depth.clamp(0.0, 1.0), ramp_ms })
    }

    pub fn unduck(&self, node_name: &str, ramp_ms: u64) -> bool {
        self.send(node_name, DaemonMsg::Unduck { ramp_ms })
    }

    /// Tells a host to stop receiving (target unrouted or removed).
    pub fn release(&self, node_name: &str) -> bool {
        self.send(node_name, DaemonMsg::Release)
    }

    /// Re-issues this host's `welcome` with a new playout delay (`jitter_ms` =
    /// the receiver's `sess.latency.msec`). False = not connected, in which case
    /// the stored value applies at its next hello instead.
    ///
    /// A second `welcome` is deliberately how this is done rather than a new
    /// message type: the agent already reloads its receiver on every `welcome`,
    /// unconditionally and by design (a reconnect is exactly when a
    /// resumed-from-suspend host needs rebuilding — plan §13.4), so retuning
    /// needs no protocol change and works with agents already deployed.
    pub fn retune(&self, node_name: &str, jitter_ms: u16) -> bool {
        self.send(node_name, welcome(node_name, jitter_ms))
    }

    /// Last reported state of a host, if connected.
    pub fn state(&self, node_name: &str) -> Option<HostState> {
        self.live.get(node_name).map(|l| l.state.clone())
    }

    pub fn is_connected(&self, node_name: &str) -> bool {
        self.live.contains_key(node_name)
    }

    /// Every host the Outputs page shows: paired ones (real pw-sink targets, §3)
    /// and hosts still asking to pair, which are *discovered* outputs. This is the
    /// source of truth for pw-sink outputs — discovery no longer is.
    pub fn hosts(&self) -> Vec<HostRow> {
        let mut rows: Vec<HostRow> = self
            .paired
            .iter()
            .map(|a| HostRow {
                node_name: a.node_name.clone(),
                label: self.live.get(&a.node_name).map(|l| l.label.clone()).unwrap_or_else(|| a.label.clone()),
                connected: self.live.contains_key(&a.node_name),
                paired: true,
                pair_code: None,
            })
            .collect();
        rows.extend(self.pending.iter().map(|p| HostRow {
            node_name: p.node_name.clone(),
            label: p.label.clone(),
            // A pending row only exists while its socket does.
            connected: true,
            paired: false,
            pair_code: Some(p.code.clone()),
        }));
        rows
    }

    /// Connected paired hosts, `node_name → label`. **This is the gate for the audio
    /// path** (§3: no agent, no target): `sync_group::compute_desired` builds a
    /// group's pw-sink members from it and `routing::build_matrix` reads presence
    /// from it, so both agree with `/api/outputs` about which hosts exist and what
    /// they are called.
    ///
    /// It used to be `pw_target_discovery`'s mDNS registry, which keys hosts as
    /// `pwsink-dev-<host>` while a pairing is `pwsink-dev-<host>_<user>` — so the
    /// reconciler asked about a name no routing link could ever carry, and *no
    /// pw-sink output was ever routable*. The registry is diagnostic now.
    pub fn connected_targets(&self) -> std::collections::BTreeMap<String, String> {
        self.paired
            .iter()
            .filter_map(|a| {
                let live = self.live.get(&a.node_name)?;
                Some((a.node_name.clone(), live.label.clone()))
            })
            .collect()
    }

    /// Paired agents plus pending requests, for `/api/agents` (diagnostics; the
    /// Outputs page reads the output listings instead).
    pub fn snapshot(&self) -> Vec<AgentInfo> {
        let mut rows: Vec<AgentInfo> = self
            .paired
            .iter()
            .map(|a| AgentInfo {
                identity: a.identity.clone(),
                label: a.label.clone(),
                node_name: a.node_name.clone(),
                paired: true,
                connected: self.live.contains_key(&a.node_name),
                code: None,
                state: self.state(&a.node_name),
            })
            .collect();
        rows.extend(self.pending.iter().map(|p| AgentInfo {
            identity: p.identity.clone(),
            label: p.label.clone(),
            node_name: p.node_name.clone(),
            paired: false,
            connected: true,
            code: Some(p.code.clone()),
            state: None,
        }));
        rows
    }
}

/// The `welcome` for a host: which session to receive and how much to buffer.
///
/// `jitter_ms` is always `Some` — the *daemon* decides what a target runs at, not
/// the module's built-in default, so that the figure the UI shows and the figure
/// the receiver uses are the same one. `ifname` stays `None`: the agent knows the
/// route to us better than we do.
fn welcome(node_name: &str, jitter_ms: u16) -> DaemonMsg {
    DaemonMsg::Welcome {
        session_name: crate::pwsink_server::session_name_for(node_name),
        ifname: None,
        jitter_ms: Some(u32::from(jitter_ms)),
        keepalive_secs: PING_INTERVAL.as_secs(),
    }
}

// ---- remote duck relay -----------------------------------------------------
//
// The announce coordinator (announce.rs) is synchronous and process-global, while
// sending to an agent needs the async registry lock. So announcements post duck
// requests to this relay instead of blocking: a small task drains it and talks to
// the agents.
//
// Two ducks compose per announcement, deliberately (plan §11 P3): `overlay_mixer`
// attenuates *our own* music inside the stream we send, and this attenuates the
// *other* applications playing on the receiver's sink. Neither touches the
// announcement itself.

/// Ramp for an announcement duck: fast enough not to talk over the clip's start,
/// slow enough not to click.
pub const ANNOUNCE_DUCK_RAMP_MS: u64 = 200;
/// Coming back up may be leisurely — nothing is waiting on it.
pub const ANNOUNCE_UNDUCK_RAMP_MS: u64 = 400;

enum RemoteDuck {
    Duck { node_name: String, depth: f32 },
    Unduck { node_name: String },
}

static DUCK_TX: std::sync::OnceLock<mpsc::Sender<RemoteDuck>> = std::sync::OnceLock::new();

/// Starts the relay. Called once from `main.rs` inside the runtime.
pub fn spawn_duck_relay(agents: SharedAgents) {
    let (tx, mut rx) = mpsc::channel::<RemoteDuck>(REMOTE_DUCK_DEPTH);
    if DUCK_TX.set(tx).is_err() {
        return; // already running
    }
    tokio::spawn(async move {
        while let Some(request) = rx.recv().await {
            let agents = agents.lock().await;
            match request {
                RemoteDuck::Duck { node_name, depth } => {
                    if !agents.duck(&node_name, depth, ANNOUNCE_DUCK_RAMP_MS) {
                        tracing::debug!("duck for '{node_name}' dropped: no agent connected");
                    }
                }
                RemoteDuck::Unduck { node_name } => {
                    agents.unduck(&node_name, ANNOUNCE_UNDUCK_RAMP_MS);
                }
            }
        }
    });
}

/// Asks a host's agent to duck the *other* applications on its sink. Silently does
/// nothing for non-pw-sink outputs, and never blocks the caller.
pub fn duck_output(node_name: &str, depth: f32) {
    if !node_name.starts_with(PWSINK_DEV_PREFIX) {
        return;
    }
    if let Some(tx) = DUCK_TX.get() {
        // Full = the relay task is not draining; an announcement's duck is worth a
        // line, not a wait (the announcement still plays, just without the duck).
        if tx.try_send(RemoteDuck::Duck { node_name: node_name.to_string(), depth }).is_err() {
            tracing::warn!("remote-duck queue full or closed; '{node_name}' will not duck for this announcement");
        }
    }
}

/// Releases a duck started by [`duck_output`].
pub fn unduck_output(node_name: &str) {
    if !node_name.starts_with(PWSINK_DEV_PREFIX) {
        return;
    }
    if let Some(tx) = DUCK_TX.get() {
        if tx.try_send(RemoteDuck::Unduck { node_name: node_name.to_string() }).is_err() {
            tracing::warn!("remote-duck queue full or closed; '{node_name}' may stay ducked until its next announcement");
        }
    }
}

// ---- WebSocket endpoint ----------------------------------------------------

/// `GET /api/agent/ws` — one connection per agent, dialled *by* the agent.
pub async fn agent_ws(ws: WebSocketUpgrade, State(state): State<crate::api::AppState>) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, state))
}

async fn handle_socket(socket: WebSocket, state: crate::api::AppState) {
    let (mut sink, mut stream) = {
        use futures_util::StreamExt as _;
        socket.split()
    };
    let (tx, mut rx) = mpsc::channel::<DaemonMsg>(AGENT_MSG_DEPTH);

    // Outgoing pump: everything the daemon sends a host goes through this queue,
    // so no caller ever awaits a socket write while holding the registry lock.
    let writer = tokio::spawn(async move {
        use futures_util::SinkExt as _;
        while let Some(msg) = rx.recv().await {
            let Ok(json) = serde_json::to_string(&msg) else { continue };
            if sink.send(Message::Text(json.into())).await.is_err() {
                return;
            }
        }
    });

    // First message must be Hello.
    let hello = {
        use futures_util::StreamExt as _;
        match stream.next().await {
            Some(Ok(Message::Text(text))) => serde_json::from_str::<AgentMsg>(&text).ok(),
            _ => None,
        }
    };
    let Some(AgentMsg::Hello { protocol, agent_version, machine_id, hostname, user, token, pair_code }) = hello else {
        let _ = tx.try_send(DaemonMsg::Denied { reason: "expected a hello message".into() });
        writer.abort();
        return;
    };

    let claim = HelloClaim {
        protocol,
        machine_id: &machine_id,
        hostname: &hostname,
        user: &user,
        token: token.as_deref(),
        pair_code: pair_code.as_deref(),
    };
    let outcome = state.agents.lock().await.hello(claim, tx.clone());

    let (node_name, label) = match outcome {
        HelloOutcome::Welcome { node_name, label } => (node_name, label),
        HelloOutcome::Pending => {
            // Hold the socket open until the agent gives up or a human decides;
            // `approve` pushes the token down `tx`, after which the agent
            // reconnects with it and this connection ends.
            pump_until_closed(&mut stream).await;
            let identity = format!("{machine_id}:{user}");
            state.agents.lock().await.pending.retain(|p| p.identity != identity);
            let _ = state.changes.send(());
            writer.abort();
            return;
        }
        HelloOutcome::Denied(reason) => {
            tracing::warn!("agent {hostname} ({user}) denied: {reason}");
            let _ = tx.try_send(DaemonMsg::Denied { reason });
            // Give the writer a moment to flush the denial before dropping it.
            tokio::time::sleep(Duration::from_millis(200)).await;
            writer.abort();
            return;
        }
    };

    tracing::info!("agent '{label}' connected as {node_name} (agent {agent_version})");
    // The playout delay this host is configured for (its override, else the
    // module's own default made explicit — sync_settings.rs). Sent on every hello,
    // so a host that reconnects after the value changed comes up on the new one.
    let jitter_ms = state.sync_settings.lock_recover().pwsink_jitter_effective(&node_name);
    let _ = tx.try_send(welcome(&node_name, jitter_ms));

    let mut ping = tokio::time::interval(PING_INTERVAL);
    ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        use futures_util::StreamExt as _;
        tokio::select! {
            incoming = stream.next() => {
                let Some(Ok(message)) = incoming else { break };
                match message {
                    Message::Text(text) => match serde_json::from_str::<AgentMsg>(&text) {
                        Ok(AgentMsg::State(host_state)) => {
                            state.agents.lock().await.update_state(&node_name, host_state);
                        }
                        Ok(AgentMsg::ForeignSession { session }) => {
                            tracing::warn!(
                                "host '{label}' also receives session '{session}' from another router \
                                 (cross-talk; see receiver-agent-plan.md §7.1)"
                            );
                        }
                        Ok(AgentMsg::Pong) => {}
                        Ok(AgentMsg::Hello { .. }) => {
                            tracing::warn!("agent '{label}' sent a second hello; ignoring");
                        }
                        Err(e) => tracing::debug!("unparsable message from '{label}': {e}"),
                    },
                    Message::Close(_) => break,
                    _ => {}
                }
            }
            _ = ping.tick() => {
                if tx.try_send(DaemonMsg::Ping).is_err() {
                    break;
                }
            }
        }
    }

    tracing::info!("agent '{label}' ({node_name}) disconnected");
    state.agents.lock().await.disconnected(&node_name);
    writer.abort();
}

/// Drains a pending-pair connection until it closes, ignoring its messages: a
/// not-yet-approved agent has no permissions, so nothing it says is acted on.
async fn pump_until_closed(stream: &mut futures_util::stream::SplitStream<WebSocket>) {
    use futures_util::StreamExt as _;
    while let Some(Ok(message)) = stream.next().await {
        if matches!(message, Message::Close(_)) {
            break;
        }
    }
}

/// Silences **and levels** a receiver host for the alignment session, over the agent's
/// control lane (`calibrate::OutOfBandMute`).
///
/// This is the *preferred* way to silence a pw-sink member during a solo, and it is
/// better than the relay fallback where it works: only the receiver's own sink volume
/// changes, so the stream keeps flowing and its jitter buffer never re-anchors —
/// unmuting therefore cannot introduce the discontinuity the estimator would read as a
/// real offset (plan §12.3.2).
///
/// Either getter answering `None` is the capability query *and* the snapshot in one: a
/// host with no live agent, or one whose sink has neither a device route nor node volume
/// (the agent's own diagnostic calls that "lever: none"), reports nothing — and the
/// session then falls back to the relay mute rather than assuming a speaker is silent
/// when it is not, and treats the member as un-levellable rather than pretending it drove
/// a level.
///
/// ## Mute and level are asked separately because they really are separate
///
/// [`HostState`] carries them as two independent `Option`s, and the agent fills them from
/// one probe that can genuinely answer one and not the other: `pw::thread::master_props`
/// falls back to the sink **node**'s `Props` when the sink has no device route (a virtual
/// sink), and that path reports `channel_volumes` with `mute: None`. Such a host is
/// levellable out of band while its *mute* still needs the relay. Both answers are also
/// `None` whenever the host is not receiving our stream at all, since the lever is found
/// through the receive stream's target sink.
///
/// ## Why no "transient" write and no "forget", unlike `ap2_volume`
///
/// `ap2_volume` needed [`set_volume_transient`](crate::outputs::ap2::volume::Ap2Control::set_volume_transient)
/// and [`forget_volume`](crate::outputs::ap2::volume::Ap2Control::forget_volume) because it *stores*
/// a desired level and re-applies a user-set one on every reconnect — so a calibration
/// level written through the ordinary path outlived the session. [`Agents`] stores
/// nothing: `set_volume` queues one `SetVolume` for the host and returns, `HostState` is
/// what the host *reports back*, and no level is ever re-applied on a reconnect. A write
/// therefore leaves no daemon-side mark for a teardown to erase, and "leave an unknown
/// level alone" is implemented by simply not writing.
pub struct AgentSilencer(pub SharedAgents);

/// Spelled out rather than borrowed from `calibrate`: its alias is private, and this
/// must stay structurally identical to the trait's signature.
type OobFut<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;

impl crate::align::calibrate::OutOfBandMute for AgentSilencer {
    fn muted<'a>(&'a self, output: &'a str) -> OobFut<'a, Option<bool>> {
        Box::pin(async move { self.0.lock().await.state(output).and_then(|s| s.muted) })
    }

    fn set_muted<'a>(&'a self, output: &'a str, muted: bool) -> OobFut<'a, bool> {
        // `set_mute` returns false when the host is not connected. Propagating that
        // rather than swallowing it is what lets the caller keep the member
        // relay-muted instead of leaving it audible.
        Box::pin(async move { self.0.lock().await.set_mute(output, muted) })
    }

    fn level<'a>(&'a self, output: &'a str) -> OobFut<'a, Option<f32>> {
        // The host's own cubic 0.0–1.0 master level, exactly as `/api/outputs` reports it
        // — the snapshot is kept on the host's scale so that putting it back is exact.
        Box::pin(async move { self.0.lock().await.state(output).and_then(|s| s.volume) })
    }

    fn set_level<'a>(&'a self, output: &'a str, level: f32) -> OobFut<'a, bool> {
        // Same honesty as `set_muted`: false = the command did not reach the host, and the
        // level has *no* fallback (the relay has a mute, not a gain), so the caller has to
        // report the member as un-levellable rather than assume the write landed.
        Box::pin(async move { self.0.lock().await.set_volume(output, level) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry() -> Agents {
        let path = std::env::temp_dir().join(format!("agents-test-{}.json", random_hex(6)));
        Agents::new(path, tokio::sync::broadcast::channel(1).0)
    }

    fn channel() -> mpsc::Sender<DaemonMsg> {
        mpsc::channel(AGENT_MSG_DEPTH).0
    }

    /// A well-formed hello from `machine`/`user`, tokenless unless given one. Tests
    /// that care about a field override it: `HelloClaim { pair_code: .., ..claim(..) }`.
    fn claim<'a>(machine: &'a str, user: &'a str, token: Option<&'a str>) -> HelloClaim<'a> {
        HelloClaim { protocol: PROTOCOL_VERSION, machine_id: machine, hostname: "host", user, token, pair_code: None }
    }

    #[test]
    fn node_name_includes_the_user_so_two_sessions_dont_collide() {
        let a = Agents::node_name_for("david-local", "david");
        let b = Agents::node_name_for("david-local", "guest");
        assert_ne!(a, b);
        assert!(a.starts_with(PWSINK_DEV_PREFIX));
    }

    #[test]
    fn a_tokenless_hello_becomes_a_pending_request() {
        let mut agents = registry();
        let outcome = agents.hello(claim("m1", "dave", None), channel());
        assert!(matches!(outcome, HelloOutcome::Pending));
        let snapshot = agents.snapshot();
        assert_eq!(snapshot.len(), 1);
        assert!(!snapshot[0].paired);
        assert!(snapshot[0].code.is_some());
        // Listed as a host from the first hello — that is the discovered output the
        // user pairs — but not paired, so nothing can route to it yet.
        let hosts = agents.hosts();
        assert_eq!(hosts.len(), 1);
        assert!(!hosts[0].paired);
        assert!(hosts[0].pair_code.is_some());
        assert!(hosts[0].node_name.starts_with(PWSINK_DEV_PREFIX));
    }

    #[test]
    fn the_pending_node_name_is_the_one_approval_uses() {
        // The Outputs page keys its rows by node name, so the card the user pairs
        // and the target it becomes must be the same row.
        let mut agents = registry();
        agents.hello(claim("m1", "dave", None), channel());
        let pending_name = agents.hosts()[0].node_name.clone();
        let paired = agents.approve("m1:dave").unwrap();
        assert_eq!(paired.node_name, pending_name);
    }

    #[test]
    fn two_hosts_sharing_a_hostname_and_user_get_distinct_node_names() {
        // Same derived name, different machines: folding them together would share
        // one card, one routing row and one HA entity between two hosts.
        let mut agents = registry();
        agents.hello(claim("m1", "dave", None), channel());
        agents.approve("m1:dave").unwrap();
        agents.hello(claim("m2", "dave", None), channel());
        let names: Vec<String> = agents.hosts().into_iter().map(|h| h.node_name).collect();
        assert_eq!(names.len(), 2);
        assert_ne!(names[0], names[1]);
    }

    #[test]
    fn the_agents_own_code_is_kept_and_survives_a_reconnect() {
        // The code the host logged at startup is the one the approver must see, so a
        // reconnect must not rotate it.
        let mut agents = registry();
        agents.hello(HelloClaim { pair_code: Some("A1B2C3"), ..claim("m1", "dave", None) }, channel());
        assert_eq!(agents.hosts()[0].pair_code.as_deref(), Some("A1B2C3"));
        agents.hello(HelloClaim { pair_code: Some("A1B2C3"), ..claim("m1", "dave", None) }, channel());
        assert_eq!(agents.hosts().len(), 1);
        assert_eq!(agents.hosts()[0].pair_code.as_deref(), Some("A1B2C3"));
    }

    #[test]
    fn a_malformed_offered_code_is_replaced_not_shown() {
        // Anything that isn't the shape we'd have generated is ours instead: the code
        // is rendered in the UI next to a host's name, so a rogue agent must not get
        // to write that text.
        assert_eq!(valid_pair_code(Some("A1B2C3")).as_deref(), Some("A1B2C3"));
        assert_eq!(valid_pair_code(Some("a1b2c3")), None);
        assert_eq!(valid_pair_code(Some("A1B2C3 — Kitchen")), None);
        assert_eq!(valid_pair_code(Some("ZZZZZZ")), None);
        assert_eq!(valid_pair_code(Some("A1B2")), None);
        assert_eq!(valid_pair_code(None), None);

        let mut agents = registry();
        agents.hello(HelloClaim { pair_code: Some("nope, not a code"), ..claim("m1", "dave", None) }, channel());
        let code = agents.hosts()[0].pair_code.clone().unwrap();
        assert_eq!(code.len(), PAIR_CODE_CHARS);
        assert!(code.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn approval_mints_a_token_that_then_authenticates() {
        let mut agents = registry();
        agents.hello(claim("m1", "dave", None), channel());
        let paired = agents.approve("m1:dave").expect("approve");
        assert!(!paired.token.is_empty());
        assert_eq!(agents.hosts().len(), 1);
        assert!(agents.hosts()[0].paired);

        // Reconnect with the token: welcomed, and now connected.
        let outcome = agents.hello(claim("m1", "dave", Some(&paired.token)), channel());
        match outcome {
            HelloOutcome::Welcome { node_name, .. } => {
                assert_eq!(node_name, paired.node_name);
                assert!(agents.is_connected(&node_name));
            }
            _ => panic!("expected a welcome"),
        }
    }

    #[test]
    fn a_wrong_token_is_denied() {
        let mut agents = registry();
        agents.hello(claim("m1", "dave", None), channel());
        agents.approve("m1:dave").unwrap();
        let outcome = agents.hello(claim("m1", "dave", Some("not-the-token")), channel());
        assert!(matches!(outcome, HelloOutcome::Denied(_)));
    }

    #[test]
    fn a_token_from_another_identity_is_denied() {
        // Same token, different user: pairing is per session, so this must fail
        // even though the secret is genuine.
        let mut agents = registry();
        agents.hello(claim("m1", "dave", None), channel());
        let paired = agents.approve("m1:dave").unwrap();
        let outcome = agents.hello(claim("m1", "eve", Some(&paired.token)), channel());
        assert!(matches!(outcome, HelloOutcome::Denied(_)));
    }

    #[test]
    fn protocol_mismatch_is_denied() {
        let mut agents = registry();
        let outcome = agents.hello(HelloClaim { protocol: PROTOCOL_VERSION + 1, ..claim("m1", "dave", None) }, channel());
        assert!(matches!(outcome, HelloOutcome::Denied(_)));
    }

    #[test]
    fn re_pairing_keeps_the_node_name_so_routing_survives() {
        let mut agents = registry();
        agents.hello(claim("m1", "dave", None), channel());
        let first = agents.approve("m1:dave").unwrap();
        // Agent lost its config and pairs again.
        agents.hello(claim("m1", "dave", None), channel());
        let second = agents.approve("m1:dave").unwrap();
        assert_eq!(first.node_name, second.node_name);
        assert_ne!(first.token, second.token);
    }

    #[test]
    fn unpairing_revokes_the_token_and_leaves_the_host_pairable_again() {
        let mut agents = registry();
        agents.hello(claim("m1", "dave", None), channel());
        let paired = agents.approve("m1:dave").unwrap();
        agents.unpair("m1:dave").unwrap();
        assert!(agents.hosts().is_empty());

        // The revoked token is refused...
        let outcome = agents.hello(claim("m1", "dave", Some(&paired.token)), channel());
        assert!(matches!(outcome, HelloOutcome::Denied(_)));
        // ...and the agent, which drops it rather than dying, comes back as a
        // discovered host that keeps the node name its routing used.
        agents.hello(claim("m1", "dave", None), channel());
        let hosts = agents.hosts();
        assert_eq!(hosts.len(), 1);
        assert!(!hosts[0].paired);
        assert_eq!(hosts[0].node_name, paired.node_name);
    }

    #[test]
    fn an_output_card_resolves_to_an_identity_whether_paired_or_pending() {
        let mut agents = registry();
        agents.hello(claim("m1", "dave", None), channel());
        let node_name = agents.hosts()[0].node_name.clone();
        assert_eq!(agents.identity_for_node(&node_name).as_deref(), Some("m1:dave"));
        agents.approve("m1:dave").unwrap();
        assert_eq!(agents.identity_for_node(&node_name).as_deref(), Some("m1:dave"));
        assert_eq!(agents.identity_for_node("pwsink-dev-nobody"), None);
    }

    #[test]
    fn commands_to_a_disconnected_host_report_failure_rather_than_queueing() {
        let mut agents = registry();
        agents.hello(claim("m1", "dave", None), channel());
        let paired = agents.approve("m1:dave").unwrap();
        assert!(!agents.set_volume(&paired.node_name, 0.5));
        assert!(!agents.set_mute(&paired.node_name, true));
        assert!(!agents.duck(&paired.node_name, 0.2, 200));
    }

    #[test]
    fn state_updates_only_apply_to_connected_hosts() {
        let mut agents = registry();
        agents.hello(claim("m1", "dave", None), channel());
        let paired = agents.approve("m1:dave").unwrap();
        agents.update_state(&paired.node_name, HostState { volume: Some(0.4), ..Default::default() });
        assert_eq!(agents.state(&paired.node_name), None);

        let (tx, _rx) = mpsc::channel(AGENT_MSG_DEPTH);
        agents.hello(claim("m1", "dave", Some(&paired.token)), tx);
        agents.update_state(&paired.node_name, HostState { volume: Some(0.4), ..Default::default() });
        assert_eq!(agents.state(&paired.node_name).and_then(|s| s.volume), Some(0.4));
    }

    /// The alignment seam (W17 for the mute, W20 for the level), through the real
    /// registry: both getters answer `None` — "cannot" — until a host is actually
    /// connected *and* has reported the lever, and both setters say whether the command
    /// reached the host. That honesty is the whole contract: `None`/`false` is what makes
    /// the session fall back to the relay mute and report the member as un-levellable
    /// instead of believing it silenced or levelled a speaker it did not.
    #[tokio::test]
    async fn the_alignment_seam_answers_cannot_until_a_host_reports_a_lever() {
        use crate::align::calibrate::OutOfBandMute as _;

        let mut agents = registry();
        agents.hello(claim("m1", "dave", None), channel());
        let paired = agents.approve("m1:dave").unwrap();
        let node = paired.node_name.clone();
        let shared: SharedAgents = Arc::new(Mutex::new(agents));
        let seam = AgentSilencer(shared.clone());

        // Paired but not connected: no capability either way, and no write lands.
        assert_eq!(seam.muted(&node).await, None);
        assert_eq!(seam.level(&node).await, None);
        assert!(!seam.set_muted(&node, true).await);
        assert!(!seam.set_level(&node, 0.2).await);

        // Connected, but nothing reported yet — still "cannot", because a state we have
        // not read is not a snapshot we can put back.
        let (tx, mut rx) = mpsc::channel(AGENT_MSG_DEPTH);
        shared.lock().await.hello(claim("m1", "dave", Some(&paired.token)), tx);
        assert_eq!(seam.level(&node).await, None);

        // A sink with a level but no mute lever is a real case, not a hypothetical: the
        // agent's node-`Props` fallback reports `channel_volumes` with `mute: None`. Such a
        // member is levellable out of band while its mute still needs the relay.
        shared.lock().await.update_state(&node, HostState { volume: Some(0.7), muted: None, receiving: true, ..Default::default() });
        assert_eq!(seam.level(&node).await, Some(0.7), "the host's own cubic scale, kept exact for the restore");
        assert_eq!(seam.muted(&node).await, None, "no mute lever ⇒ the relay has to hold this one down");

        // And a write now reaches the host, on its own scale.
        assert!(seam.set_level(&node, 0.2).await);
        assert!(seam.set_muted(&node, true).await);
        let mut sent = Vec::new();
        while let Ok(msg) = rx.try_recv() {
            sent.push(msg);
        }
        assert!(sent.contains(&DaemonMsg::SetVolume { volume: 0.2 }), "{sent:?}");
        assert!(sent.contains(&DaemonMsg::SetMute { muted: true }), "{sent:?}");
    }

    #[test]
    fn tokens_are_not_predictable() {
        let a = random_hex(24);
        let b = random_hex(24);
        assert_eq!(a.len(), 48);
        assert_ne!(a, b);
    }
}
