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
//! Identity is *machine id + user*, not the hostname: one agent per logged-in
//! session (§13.2), so two users on one machine are two independent targets, and a
//! renamed host keeps its pairing.

use crate::config::{slugify, PWSINK_DEV_PREFIX};
use crate::pw_thread::ChangeNotifier;
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

// ---- wire protocol (mirror of pwrouter-agent/src/proto.rs) ------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentMsg {
    Hello {
        protocol: u32,
        agent_version: String,
        machine_id: String,
        hostname: String,
        user: String,
        token: Option<String>,
    },
    State(HostState),
    ForeignSession { session: String },
    Pong,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DaemonMsg {
    PairPending { code: String },
    Paired { token: String },
    Denied { reason: String },
    Welcome {
        session_name: String,
        ifname: Option<String>,
        jitter_ms: Option<u32>,
        keepalive_secs: u64,
    },
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
    /// Short code shown in the UI *and* logged by the agent, so the person
    /// approving can tell two simultaneous requests apart.
    code: String,
    tx: mpsc::UnboundedSender<DaemonMsg>,
}

/// A live, approved connection.
struct Live {
    label: String,
    state: HostState,
    tx: mpsc::UnboundedSender<DaemonMsg>,
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

/// What a `Hello` resolves to.
pub enum HelloOutcome {
    /// Approved: this is the host's node name and the session it should receive.
    Welcome { node_name: String, label: String },
    /// Waiting for approval; the code was already queued to the agent.
    Pending,
    Denied(String),
}

/// One row for the API/UI.
#[derive(Debug, Clone, Serialize)]
pub struct AgentInfo {
    pub identity: String,
    pub label: String,
    /// `None` for a pending request — it has no routing identity until approved.
    pub node_name: Option<String>,
    pub paired: bool,
    pub connected: bool,
    /// Pairing code, only for pending requests.
    pub code: Option<String>,
    pub state: Option<HostState>,
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

    /// Handles a `Hello`. `tx` is this connection's outgoing queue.
    pub fn hello(
        &mut self,
        protocol: u32,
        machine_id: &str,
        hostname: &str,
        user: &str,
        token: Option<&str>,
        tx: mpsc::UnboundedSender<DaemonMsg>,
    ) -> HelloOutcome {
        if protocol != PROTOCOL_VERSION {
            return HelloOutcome::Denied(format!(
                "protocol {protocol} is not {PROTOCOL_VERSION}; update the agent or the add-on"
            ));
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

        // No token: a pair request. Replace any earlier request from the same
        // identity (a restarted agent) so the list can't fill up with stale rows.
        let code = random_hex(3).to_uppercase();
        if code.is_empty() {
            return HelloOutcome::Denied("daemon could not generate a pairing code".into());
        }
        self.pending.retain(|p| p.identity != identity);
        let _ = tx.send(DaemonMsg::PairPending { code: code.clone() });
        tracing::info!("pairing requested by '{label}' ({identity}), code {code}");
        self.pending.push(Pending { identity, label, code, tx });
        let _ = self.changes.send(());
        HelloOutcome::Pending
    }

    /// Approves a pending request: mints a token, persists it, and pushes it to
    /// the waiting agent (which reconnects with it).
    pub fn approve(&mut self, identity: &str) -> Result<PairedAgent, String> {
        let Some(index) = self.pending.iter().position(|p| p.identity == identity) else {
            return Err("no pending pair request for that identity".into());
        };
        let pending = self.pending.remove(index);
        let token = random_hex(24);
        if token.is_empty() {
            return Err("daemon could not generate a token".into());
        }
        // A host that is re-pairing keeps its node name, so existing routing and
        // HA entities keep working.
        let (hostname, user) = split_label(&pending.label);
        let node_name = self
            .paired
            .iter()
            .find(|a| a.identity == identity)
            .map(|a| a.node_name.clone())
            .unwrap_or_else(|| Self::node_name_for(&hostname, &user));

        let agent = PairedAgent { identity: identity.to_string(), label: pending.label, node_name, token: token.clone() };
        self.paired.retain(|a| a.identity != identity);
        self.paired.push(agent.clone());
        save_store(&self.path, &self.paired);
        let _ = pending.tx.send(DaemonMsg::Paired { token });
        tracing::info!("approved agent '{}' as {}", agent.label, agent.node_name);
        let _ = self.changes.send(());
        Ok(agent)
    }

    /// Denies (and drops) a pending request.
    pub fn deny(&mut self, identity: &str) -> Result<(), String> {
        let Some(index) = self.pending.iter().position(|p| p.identity == identity) else {
            return Err("no pending pair request for that identity".into());
        };
        let pending = self.pending.remove(index);
        let _ = pending.tx.send(DaemonMsg::Denied { reason: "pairing was declined".into() });
        let _ = self.changes.send(());
        Ok(())
    }

    /// Revokes a pairing: the token stops working and the live connection (if
    /// any) is told it is no longer a receiver.
    pub fn forget(&mut self, identity: &str) -> Result<(), String> {
        let Some(agent) = self.paired.iter().find(|a| a.identity == identity).cloned() else {
            return Err("no such paired agent".into());
        };
        self.paired.retain(|a| a.identity != identity);
        save_store(&self.path, &self.paired);
        if let Some(live) = self.live.remove(&agent.node_name) {
            let _ = live.tx.send(DaemonMsg::Release);
            let _ = live.tx.send(DaemonMsg::Denied { reason: "pairing was removed".into() });
        }
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

    fn send(&self, node_name: &str, msg: DaemonMsg) -> bool {
        match self.live.get(node_name) {
            Some(live) => live.tx.send(msg).is_ok(),
            None => false,
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

    /// Last reported state of a host, if connected.
    pub fn state(&self, node_name: &str) -> Option<HostState> {
        self.live.get(node_name).map(|l| l.state.clone())
    }

    pub fn is_connected(&self, node_name: &str) -> bool {
        self.live.contains_key(node_name)
    }

    /// Every paired host: `(node_name, label, connected)`. This is the source of
    /// truth for pw-sink outputs (§3) — discovery no longer is.
    pub fn targets(&self) -> Vec<(String, String, bool)> {
        self.paired
            .iter()
            .map(|a| {
                let label = self.live.get(&a.node_name).map(|l| l.label.clone()).unwrap_or_else(|| a.label.clone());
                (a.node_name.clone(), label, self.live.contains_key(&a.node_name))
            })
            .collect()
    }

    /// Paired agents plus pending requests, for the API/UI.
    pub fn snapshot(&self) -> Vec<AgentInfo> {
        let mut rows: Vec<AgentInfo> = self
            .paired
            .iter()
            .map(|a| AgentInfo {
                identity: a.identity.clone(),
                label: a.label.clone(),
                node_name: Some(a.node_name.clone()),
                paired: true,
                connected: self.live.contains_key(&a.node_name),
                code: None,
                state: self.state(&a.node_name),
            })
            .collect();
        rows.extend(self.pending.iter().map(|p| AgentInfo {
            identity: p.identity.clone(),
            label: p.label.clone(),
            node_name: None,
            paired: false,
            connected: true,
            code: Some(p.code.clone()),
            state: None,
        }));
        rows
    }
}

/// Splits `hostname (user)` back into its parts. The label is built by this
/// module, so this is a round-trip, not a parse of foreign input.
fn split_label(label: &str) -> (String, String) {
    match label.rsplit_once(" (") {
        Some((host, rest)) => (host.to_string(), rest.trim_end_matches(')').to_string()),
        None => (label.to_string(), "unknown".to_string()),
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
    let (tx, mut rx) = mpsc::unbounded_channel::<DaemonMsg>();

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
    let Some(AgentMsg::Hello { protocol, agent_version, machine_id, hostname, user, token }) = hello else {
        let _ = tx.send(DaemonMsg::Denied { reason: "expected a hello message".into() });
        writer.abort();
        return;
    };

    let outcome = state
        .agents
        .lock()
        .await
        .hello(protocol, &machine_id, &hostname, &user, token.as_deref(), tx.clone());

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
            let _ = tx.send(DaemonMsg::Denied { reason });
            // Give the writer a moment to flush the denial before dropping it.
            tokio::time::sleep(Duration::from_millis(200)).await;
            writer.abort();
            return;
        }
    };

    tracing::info!("agent '{label}' connected as {node_name} (agent {agent_version})");
    let _ = tx.send(DaemonMsg::Welcome {
        session_name: crate::pwsink_server::session_name_for(&node_name),
        // The agent picks its own interface (it knows the route to us) and its own
        // jitter buffer default; both stay `None` unless a reason appears to
        // override them from here.
        ifname: None,
        jitter_ms: None,
        keepalive_secs: PING_INTERVAL.as_secs(),
    });

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
                if tx.send(DaemonMsg::Ping).is_err() {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn registry() -> Agents {
        let path = std::env::temp_dir().join(format!("agents-test-{}.json", random_hex(6)));
        Agents::new(path, tokio::sync::broadcast::channel(1).0)
    }

    fn channel() -> mpsc::UnboundedSender<DaemonMsg> {
        mpsc::unbounded_channel().0
    }

    #[test]
    fn node_name_includes_the_user_so_two_sessions_dont_collide() {
        let a = Agents::node_name_for("david-local", "david");
        let b = Agents::node_name_for("david-local", "guest");
        assert_ne!(a, b);
        assert!(a.starts_with(PWSINK_DEV_PREFIX));
    }

    #[test]
    fn label_round_trips_through_split() {
        let label = Agents::label_for("david-local", "david");
        assert_eq!(split_label(&label), ("david-local".to_string(), "david".to_string()));
    }

    #[test]
    fn a_tokenless_hello_becomes_a_pending_request() {
        let mut agents = registry();
        let outcome = agents.hello(PROTOCOL_VERSION, "m1", "host", "dave", None, channel());
        assert!(matches!(outcome, HelloOutcome::Pending));
        let snapshot = agents.snapshot();
        assert_eq!(snapshot.len(), 1);
        assert!(!snapshot[0].paired);
        assert!(snapshot[0].code.is_some());
        // Not a target until approved (plan §3: discovered ≠ usable).
        assert!(agents.targets().is_empty());
    }

    #[test]
    fn approval_mints_a_token_that_then_authenticates() {
        let mut agents = registry();
        agents.hello(PROTOCOL_VERSION, "m1", "host", "dave", None, channel());
        let paired = agents.approve("m1:dave").expect("approve");
        assert!(!paired.token.is_empty());
        assert_eq!(agents.targets().len(), 1);

        // Reconnect with the token: welcomed, and now connected.
        let outcome = agents.hello(PROTOCOL_VERSION, "m1", "host", "dave", Some(&paired.token), channel());
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
        agents.hello(PROTOCOL_VERSION, "m1", "host", "dave", None, channel());
        agents.approve("m1:dave").unwrap();
        let outcome = agents.hello(PROTOCOL_VERSION, "m1", "host", "dave", Some("not-the-token"), channel());
        assert!(matches!(outcome, HelloOutcome::Denied(_)));
    }

    #[test]
    fn a_token_from_another_identity_is_denied() {
        // Same token, different user: pairing is per session, so this must fail
        // even though the secret is genuine.
        let mut agents = registry();
        agents.hello(PROTOCOL_VERSION, "m1", "host", "dave", None, channel());
        let paired = agents.approve("m1:dave").unwrap();
        let outcome = agents.hello(PROTOCOL_VERSION, "m1", "host", "eve", Some(&paired.token), channel());
        assert!(matches!(outcome, HelloOutcome::Denied(_)));
    }

    #[test]
    fn protocol_mismatch_is_denied() {
        let mut agents = registry();
        let outcome = agents.hello(PROTOCOL_VERSION + 1, "m1", "host", "dave", None, channel());
        assert!(matches!(outcome, HelloOutcome::Denied(_)));
    }

    #[test]
    fn re_pairing_keeps_the_node_name_so_routing_survives() {
        let mut agents = registry();
        agents.hello(PROTOCOL_VERSION, "m1", "host", "dave", None, channel());
        let first = agents.approve("m1:dave").unwrap();
        // Agent lost its config and pairs again.
        agents.hello(PROTOCOL_VERSION, "m1", "host", "dave", None, channel());
        let second = agents.approve("m1:dave").unwrap();
        assert_eq!(first.node_name, second.node_name);
        assert_ne!(first.token, second.token);
    }

    #[test]
    fn forget_revokes_the_token() {
        let mut agents = registry();
        agents.hello(PROTOCOL_VERSION, "m1", "host", "dave", None, channel());
        let paired = agents.approve("m1:dave").unwrap();
        agents.forget("m1:dave").unwrap();
        assert!(agents.targets().is_empty());
        let outcome = agents.hello(PROTOCOL_VERSION, "m1", "host", "dave", Some(&paired.token), channel());
        assert!(matches!(outcome, HelloOutcome::Denied(_)));
    }

    #[test]
    fn commands_to_a_disconnected_host_report_failure_rather_than_queueing() {
        let mut agents = registry();
        agents.hello(PROTOCOL_VERSION, "m1", "host", "dave", None, channel());
        let paired = agents.approve("m1:dave").unwrap();
        assert!(!agents.set_volume(&paired.node_name, 0.5));
        assert!(!agents.set_mute(&paired.node_name, true));
        assert!(!agents.duck(&paired.node_name, 0.2, 200));
    }

    #[test]
    fn state_updates_only_apply_to_connected_hosts() {
        let mut agents = registry();
        agents.hello(PROTOCOL_VERSION, "m1", "host", "dave", None, channel());
        let paired = agents.approve("m1:dave").unwrap();
        agents.update_state(&paired.node_name, HostState { volume: Some(0.4), ..Default::default() });
        assert_eq!(agents.state(&paired.node_name), None);

        let (tx, _rx) = mpsc::unbounded_channel();
        agents.hello(PROTOCOL_VERSION, "m1", "host", "dave", Some(&paired.token), tx);
        agents.update_state(&paired.node_name, HostState { volume: Some(0.4), ..Default::default() });
        assert_eq!(agents.state(&paired.node_name).and_then(|s| s.volume), Some(0.4));
    }

    #[test]
    fn tokens_are_not_predictable() {
        let a = random_hex(24);
        let b = random_hex(24);
        assert_eq!(a.len(), 48);
        assert_ne!(a, b);
    }
}
