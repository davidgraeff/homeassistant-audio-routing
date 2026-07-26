//! Routing-driven **sync grouping** reconciler — sendspin multi-room grouping
//! and per-device AirPlay-2 senders in one model, so speakers routed from the
//! same sources play the same audio off one clock.
//!
//! ## The model
//!
//! Grouping is derived from routing intent, not declared: **every output routed
//! from the same set of sources belongs to one group**. Each group is backed by
//! one real `support.null-audio-sink` — the *sync anchor* (`SYNC_GRP_PREFIX`) —
//! which is the group's shared clock/timeline:
//!
//! - the group's sources are linked **into** the anchor;
//! - a filtered sendspin server (sendspin_server) captures **from** the anchor
//!   and dials exactly the group's sendspin devices, pushing one timestamped
//!   stream so they sync (see sendspin's `Group`);
//! - the group's AP2 receivers are driven by in-process senders (ap2_server.rs)
//!   that capture from the same anchor and stream realtime ALAC with libairptp
//!   PTP timing, so they share the same timeline.
//!
//! Because the anchor is one stable node per source-set, devices can come and
//! go — and the sendspin server / AP2 senders can be restarted when their
//! dialed set changes — without disturbing the anchor or the other members fed
//! from it.
//!
//! ## Reconcile
//!
//! Stateful (owns the running anchors/servers/AP2 senders) and serialized in the
//! single reconciler task (main.rs). On each change it diffs desired groups (from
//! intent + live devices) against running ones: tears down groups that are gone
//! (dropping the server + AP2 senders, destroying the anchor — its links go with
//! it), creates new anchors, and restarts a group's sendspin server / AP2 senders
//! when their dialed set (or the AP2 wire rate) changes.

use crate::config::SYNC_GRP_PREFIX;
use crate::locks::LockRecover;
use crate::pw_thread::{PwCommand, PwCommandSender, SharedState};
use crate::routing::{self, node_id_for};
use crate::routing_store::{self, RoutingLink, SharedRouting};
use crate::sendspin_discovery::{SendspinDevice, SharedSendspinDevices};
use crate::sendspin_server::{self, SendspinServerHandle};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::time::Duration;
use tokio::sync::oneshot;

/// Ports for group servers' embedded sendspin listeners are allocated upward
/// from here (distinct from any manual-output base so they never collide).
const GROUP_BASE_PORT: u16 = 8930;

/// Separator joining a sorted source-set into a group key (a control char that
/// can't appear in a node name, so the join is unambiguous).
const KEY_SEP: char = '\u{1f}';

/// A group the current intent + live graph call for.
struct DesiredGroup {
    /// Sources feeding the group (sorted, unique) — linked into the anchor.
    sources: Vec<String>,
    /// PRESENT sendspin device node names (sorted). Identity for "did the dialed
    /// set change?" (the server's dial filter is fixed at start).
    sendspin_node_names: Vec<String>,
    /// PRESENT sendspin device mDNS fullnames — the server's dial filter.
    sendspin_fullnames: HashSet<String>,
    /// PRESENT AP2 receivers in this group: (output node_name, resolved IP,
    /// per-output render delay override in ms — `None` = sender default), sorted
    /// by node_name. Identity for "did the receiver set *or its delay* change?"
    /// — a delay edit thus triggers the same drop-and-restart as a membership
    /// change, reconnecting the RTSP session with the new render buffer.
    ap2_members: Vec<(String, std::net::IpAddr, Option<u16>)>,
    /// Negotiated wire/capture rate for this group's AP2 senders (Hz): 48000 iff
    /// every AP2 member's effective rate is 48000, else 44100. Part of the AP2
    /// restart identity, so a rate change (e.g. a 48 kHz downgrade or a UI mode
    /// switch) restarts the senders + re-spawns the capture at the new rate.
    ap2_rate: u32,
}

impl DesiredGroup {
    fn new(sources: &BTreeSet<&str>) -> Self {
        Self {
            sources: sources.iter().map(|s| s.to_string()).collect(),
            sendspin_node_names: Vec::new(),
            sendspin_fullnames: HashSet::new(),
            ap2_members: Vec::new(),
            ap2_rate: 48_000,
        }
    }
}

/// A group currently running.
struct RunningGroup {
    anchor_node_name: String,
    anchor_node_id: u32,
    port: u16,
    /// Live sendspin server (dropping it stops capture/dial but leaves the
    /// anchor intact); `None` when the group has no present sendspin devices.
    server: Option<SendspinServerHandle>,
    /// Snapshot of the sendspin device set the running server was started for.
    server_devices: Vec<String>,
    /// Live AP2 senders (ap2_server.rs) for this group; drop = TEARDOWN each
    /// receiver session. `None` when the group has no present AP2 receivers.
    ap2_sender: Option<crate::ap2_server::Ap2ServerHandle>,
    /// AP2 receiver node names the running senders were started for — the restart
    /// identity. NOTE: render delay is deliberately NOT part of this: a delay change
    /// is applied LIVE (ap2_control → SetRenderDelay), never by a reconnect (that
    /// churn could silence a flaky receiver). Only membership/rate changes restart.
    ap2_members: Vec<String>,
    /// The AP2 capture/wire rate (Hz) the running senders were started at — part
    /// of the restart identity alongside `ap2_members`.
    ap2_rate: u32,
}

/// A standalone per-device sender for an *ungrouped* (idle) sendspin device,
/// kept alive in per-device mode so the device is always reachable — e.g. for an
/// announcement to an idle speaker — without a cold start. It owns its own silent
/// `null-audio-sink` (nothing routed in → its monitor is silence), captured and
/// streamed to the one device; the overlay mixer replaces that silence with a
/// ducked announcement when one is targeted at it. Superseded by the device's
/// group sender the moment it's routed into a group.
struct IdleSender {
    sink_node_name: String,
    sink_node_id: u32,
    port: u16,
    _server: SendspinServerHandle,
}

#[derive(Default)]
pub struct GroupReconciler {
    /// Keyed by the group's source-set (sorted sources joined by `KEY_SEP`).
    running: HashMap<String, RunningGroup>,
    /// Standalone senders for ungrouped devices (per-device mode only), keyed by
    /// device node name.
    idle_senders: HashMap<String, IdleSender>,
}

/// Distinctive sink-name prefix for an idle device's private sink. Deliberately
/// not `sendspin-dev-`/`ap2-dev-`/`sync-grp-` so routing never treats it as an
/// output or anchor.
const IDLE_SINK_PREFIX: &str = "idle-dev-";

/// Shared handle so the alignment API (calibrate.rs) can read the live group
/// layout the reconcile task owns.
pub type SharedGroups = std::sync::Arc<tokio::sync::Mutex<GroupReconciler>>;

/// Read-only view of one running group, for the alignment wizard.
#[derive(Debug, Clone)]
pub struct GroupSnapshot {
    /// Source node names feeding this group (its stable identity).
    pub sources: Vec<String>,
    /// The group's sync-anchor node id — where calibration audio is injected so
    /// every member hears it off the one clock.
    pub anchor_node_id: u32,
    /// Present sendspin device node names in the group.
    pub sendspin_members: Vec<String>,
    /// Present AP2 receiver node names in the group (alignable by muting +
    /// tuning each one's live render delay).
    pub ap2_members: Vec<String>,
}

impl GroupReconciler {
    /// Force the sendspin server of the group containing `sendspin_node_name` to
    /// restart on the next reconcile, by dropping its handle and clearing the
    /// remembered dialed set (so the `sendspin_node_names != prev_devices` check
    /// fires). The devices reconnect and re-apply their static delay on connect
    /// — the only way current ESPHome firmware picks up a delay change (it reads
    /// `SetStaticDelay` at stream start, not live). Returns true if a group was
    /// found; the caller must nudge a reconcile (ChangeNotifier) afterwards.
    pub fn force_server_restart(&mut self, sendspin_node_name: &str) -> bool {
        for g in self.running.values_mut() {
            if g.server_devices.iter().any(|d| d == sendspin_node_name) {
                g.server = None;
                g.server_devices.clear();
                return true;
            }
        }
        false
    }

    /// Snapshot every running group (anchor + members) for the alignment API.
    pub fn snapshot(&self) -> Vec<GroupSnapshot> {
        self.running
            .iter()
            .map(|(key, g)| GroupSnapshot {
                sources: key.split(KEY_SEP).map(str::to_string).collect(),
                anchor_node_id: g.anchor_node_id,
                sendspin_members: g.server_devices.clone(),
                ap2_members: g.ap2_members.clone(),
            })
            .collect()
    }
}

/// Stable, deterministic short id for a group key (no rng/time — those aren't
/// available and would break determinism; `DefaultHasher` has fixed keys).
fn group_hash(key: &str) -> String {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    key.hash(&mut h);
    format!("{:016x}", h.finish())
}

/// The group key for a sorted source-set.
fn source_key(sources: &BTreeSet<&str>) -> String {
    sources.iter().copied().collect::<Vec<_>>().join(&KEY_SEP.to_string())
}

/// Compute the groups the current intent + live devices call for.
///
/// A group is materialized (gets an anchor) as soon as it has a live consumer
/// that needs one: a present sendspin device or a present AP2 receiver. Members
/// are grouped by their exact source-set, so a sendspin device and an AP2
/// receiver fed from the same sources land in one group and share its clock.
fn compute_desired(
    intent: &[RoutingLink],
    devices: &BTreeMap<String, SendspinDevice>,
    ap2_devices: &BTreeMap<String, crate::ap2_discovery::Ap2Device>,
    ap2_latencies: &BTreeMap<String, u16>,
) -> BTreeMap<String, DesiredGroup> {
    let mut groups: BTreeMap<String, DesiredGroup> = BTreeMap::new();

    // Present sendspin devices → members of their source-set's group.
    for (dev_node, dev) in devices {
        let sources = routing::source_set_of(intent, dev_node);
        if sources.is_empty() {
            continue;
        }
        let g = groups.entry(source_key(&sources)).or_insert_with(|| DesiredGroup::new(&sources));
        g.sendspin_node_names.push(dev_node.clone());
        g.sendspin_fullnames.insert(dev.fullname.clone());
    }

    // Present AP2 receivers (with a resolved address) → members of their group.
    // Mirrors the sendspin loop; the audio path is built in reconcile step (e).
    for (dev_node, dev) in ap2_devices {
        let Some(addr) = dev.addr else { continue };
        let sources = routing::source_set_of(intent, dev_node);
        if sources.is_empty() {
            continue;
        }
        let g = groups.entry(source_key(&sources)).or_insert_with(|| DesiredGroup::new(&sources));
        g.ap2_members.push((dev_node.clone(), addr.ip(), ap2_latencies.get(dev_node).copied()));
    }

    for g in groups.values_mut() {
        g.sendspin_node_names.sort();
        g.ap2_members.sort();
    }
    groups
}

impl GroupReconciler {
    pub fn new() -> Self {
        Self::default()
    }

    /// Lowest free port at/above the base not used by a running group or idle
    /// sender. `extra` lets a caller reserve ports it's about to assign in the
    /// same reconcile pass (before they land in `running`/`idle_senders`).
    fn alloc_port(&self, extra: &HashSet<u16>) -> u16 {
        let mut used: HashSet<u16> = self.running.values().map(|g| g.port).collect();
        used.extend(self.idle_senders.values().map(|s| s.port));
        used.extend(extra.iter().copied());
        let mut port = GROUP_BASE_PORT;
        while used.contains(&port) {
            port += 1;
        }
        port
    }

    /// `send_ahead_us` is the group presentation lead from the sync settings
    /// (sync_settings.rs), applied to every group's sendspin server.
    pub async fn reconcile(
        &mut self,
        pw: &SharedState,
        pw_cmd: &PwCommandSender,
        routing: &SharedRouting,
        devices: &SharedSendspinDevices,
        control: &crate::sendspin_volume::SharedSendspinControl,
        send_ahead_us: i64,
        ap2_devices: &crate::ap2_discovery::SharedAp2Devices,
        ap2_ptp: &crate::ap2_ptp::SharedAp2Ptp,
        sync_settings: &crate::sync_settings::SharedSyncSettings,
        ap2_control: &crate::ap2_volume::SharedAp2Control,
    ) {
        let intent = routing_store::snapshot(routing);
        let devices_map = devices.lock_recover().clone();
        let ap2_map = ap2_devices.lock_recover().clone();
        let ap2_latencies = sync_settings.lock_recover().ap2_latencies();
        let mut desired = compute_desired(&intent, &devices_map, &ap2_map, &ap2_latencies);

        // Resolve each group's AP2 capture/wire rate from the per-output rate mode
        // + learned capability cache (48000 iff every member's effective rate is
        // 48000, else 44100). Done here (not in compute_desired) so the rate logic
        // stays with the settings store.
        {
            let ss = sync_settings.lock_recover();
            for d in desired.values_mut() {
                if !d.ap2_members.is_empty() {
                    d.ap2_rate = ss.ap2_group_rate(d.ap2_members.iter().map(|(n, _, _)| n.as_str()));
                }
            }
        }

        // 1. Tear down groups no longer desired (server first, then the anchor —
        //    destroying the anchor node takes its source/monitor links with it).
        let stale: Vec<String> = self.running.keys().filter(|k| !desired.contains_key(*k)).cloned().collect();
        for key in stale {
            if let Some(rg) = self.running.remove(&key) {
                tracing::info!(
                    "tearing down sync group {} ({} sendspin, {} ap2)",
                    rg.anchor_node_name,
                    rg.server_devices.len(),
                    rg.ap2_members.len()
                );
                drop(rg.server);
                drop(rg.ap2_sender); // signals AP2 senders to TEARDOWN their receivers
                let (tx, rx) = oneshot::channel();
                if pw_cmd.send(PwCommand::DestroySinkNode { node_id: rg.anchor_node_id, reply: tx }).is_ok() {
                    let _ = rx.await;
                }
            }
        }

        // 1b. Idle-sender teardown. Every discovered device that isn't in a group
        //     keeps a standalone sender (so it's always reachable — e.g.
        //     announcements to an idle speaker). Drop the sender of any device that
        //     is now grouped or gone, BEFORE the group servers below dial, so a
        //     newly-grouped device isn't dialed by both its idle sender and its
        //     group at once.
        let grouped: HashSet<String> = desired.values().flat_map(|d| d.sendspin_node_names.iter().cloned()).collect();
        let want_idle: HashSet<String> = devices_map.keys().filter(|d| !grouped.contains(*d)).cloned().collect();
        let drop_idle: Vec<String> = self.idle_senders.keys().filter(|d| !want_idle.contains(*d)).cloned().collect();
        for dev in drop_idle {
            if let Some(s) = self.idle_senders.remove(&dev) {
                tracing::info!("idle sender '{}' torn down (device grouped or gone)", s.sink_node_name);
                drop(s._server);
                let (tx, rx) = oneshot::channel();
                if pw_cmd.send(PwCommand::DestroySinkNode { node_id: s.sink_node_id, reply: tx }).is_ok() {
                    let _ = rx.await;
                }
            }
        }

        // 2. Create/steer each desired group.
        for (key, d) in &desired {
            // a. Ensure the anchor sink exists (create + wait, within this call,
            //    so the wiring below finds it and we don't re-create next tick).
            if !self.running.contains_key(key) {
                let anchor_node_name = format!("{SYNC_GRP_PREFIX}{}", group_hash(key));
                let (tx, rx) = oneshot::channel();
                if pw_cmd.send(PwCommand::CreateSinkNode { node_name: anchor_node_name.clone(), reply: tx }).is_err() {
                    continue;
                }
                match rx.await {
                    Ok(Ok(())) => {}
                    _ => {
                        tracing::warn!("failed to create sync anchor '{anchor_node_name}'");
                        continue;
                    }
                }
                let Some(anchor_node_id) = wait_for_node(pw, &anchor_node_name).await else {
                    tracing::warn!("sync anchor '{anchor_node_name}' did not appear in the graph in time");
                    continue; // a later reconcile retries once it shows up
                };
                let port = self.alloc_port(&HashSet::new());
                tracing::info!("created sync anchor '{anchor_node_name}' (id {anchor_node_id}) for source(s) {:?}", d.sources);
                self.running.insert(
                    key.clone(),
                    RunningGroup {
                        anchor_node_name,
                        anchor_node_id,
                        port,
                        server: None,
                        server_devices: Vec::new(),
                        ap2_sender: None,
                        ap2_members: Vec::new(),
                        ap2_rate: 48_000,
                    },
                );
            }

            // Snapshot what we need so no borrow of `self.running` is held across
            // an await (the async link/server calls below).
            let (anchor_name, anchor_id, port, prev_devices, prev_ap2, prev_ap2_rate) = {
                let rg = self.running.get(key).expect("just inserted");
                (rg.anchor_node_name.clone(), rg.anchor_node_id, rg.port, rg.server_devices.clone(), rg.ap2_members.clone(), rg.ap2_rate)
            };

            // b. Wire each source into the anchor (idempotent).
            for source in &d.sources {
                routing::ensure_link_by_name(pw, pw_cmd, source, &anchor_name).await;
            }

            // c. (Re)start the group's sendspin server when its dialed-device set
            //    changes. Each device is its own single-member sender sharing one
            //    timeline off the anchor capture, so a device can be ducked/overlaid
            //    independently while staying in sync (see sendspin_server). The dial
            //    filter is fixed at start, so a membership change means drop-and-
            //    recreate — the server only, not the anchor, so RAOP outputs fed
            //    from the same anchor never blip.
            if d.sendspin_node_names != prev_devices {
                if let Some(rg) = self.running.get_mut(key) {
                    rg.server = None; // drop old server (stops its capture/dial)
                    rg.server_devices = Vec::new();
                }
                if !d.sendspin_node_names.is_empty() {
                    match sendspin_server::start_server_per_device(
                        &anchor_name,
                        &group_display(d),
                        port,
                        anchor_id,
                        d.sendspin_fullnames.clone(),
                        send_ahead_us,
                        control.clone(),
                        devices.clone(),
                    )
                    .await
                    {
                        Ok(handle) => {
                            tracing::info!(
                                "sync group '{anchor_name}': per-device senders on port {port} dialing {} device(s)",
                                d.sendspin_node_names.len()
                            );
                            if let Some(rg) = self.running.get_mut(key) {
                                rg.server = Some(handle);
                                rg.server_devices = d.sendspin_node_names.clone();
                            }
                        }
                        Err(e) => tracing::warn!("sync group '{anchor_name}': failed to start sendspin server: {e}"),
                    }
                }
            }

            // d. (Re)start AP2 senders when the receiver set changes. Like sendspin,
            //    each per-device Connection is fixed at start, so a change means
            //    drop-and-recreate — only the senders, never the shared anchor.
            // Identity is the receiver SET + the negotiated group rate — a
            // membership change or a rate change (UI mode switch / cached 48→44.1
            // downgrade) restarts the senders. Render delay is intentionally NOT in
            // the identity: it's retuned live (ap2_control → SetRenderDelay), so a
            // delay edit never reconnects (that churn could silence a flaky receiver).
            let ap2_identity: Vec<String> =
                d.ap2_members.iter().map(|(n, _, _)| n.clone()).collect();
            if ap2_identity != prev_ap2 || d.ap2_rate != prev_ap2_rate {
                if let Some(rg) = self.running.get_mut(key) {
                    rg.ap2_sender = None; // drop → TEARDOWN old receiver sessions
                    rg.ap2_members = Vec::new();
                }
                if !d.ap2_members.is_empty() {
                    // Receivers are already PTP peers of the host-global grandmaster
                    // (registered at discovery); ensure it's up and get its clock id.
                    match ap2_ptp.ensure_started() {
                        Ok(clock_id) => match crate::ap2_server::start(d.ap2_members.clone(), anchor_id, clock_id, ap2_control.clone(), d.ap2_rate, sync_settings.clone()) {
                            Ok(handle) => {
                                tracing::info!(
                                    "sync group '{anchor_name}': AP2 senders streaming to {} receiver(s) @ {} Hz",
                                    d.ap2_members.len(), d.ap2_rate
                                );
                                if let Some(rg) = self.running.get_mut(key) {
                                    rg.ap2_sender = Some(handle);
                                    rg.ap2_members = ap2_identity;
                                    rg.ap2_rate = d.ap2_rate;
                                }
                            }
                            Err(e) => tracing::warn!("sync group '{anchor_name}': failed to start AP2 senders: {e}"),
                        },
                        Err(e) => tracing::warn!("sync group '{anchor_name}': AP2 PTP grandmaster unavailable: {e}"),
                    }
                }
            }
        }

        // 3. Idle-sender creation (per-device mode): stand up a standalone sender
        //    for every ungrouped device that doesn't have one, so it's always
        //    reachable. Its own silent sink → it streams silence until the overlay
        //    mixer injects an announcement, then falls back to silence.
        for dev in &want_idle {
            if self.idle_senders.contains_key(dev) {
                continue;
            }
            let Some(fullname) = devices_map.get(dev).map(|d| d.fullname.clone()) else {
                continue;
            };
            let suffix = dev.strip_prefix(crate::config::SENDSPIN_DEV_PREFIX).unwrap_or(dev);
            let sink_node_name = format!("{IDLE_SINK_PREFIX}{suffix}");
            let (tx, rx) = oneshot::channel();
            if pw_cmd.send(PwCommand::CreateSinkNode { node_name: sink_node_name.clone(), reply: tx }).is_err() {
                continue;
            }
            match rx.await {
                Ok(Ok(())) => {}
                _ => {
                    tracing::warn!("idle sender: failed to create sink '{sink_node_name}'");
                    continue;
                }
            }
            let Some(sink_node_id) = wait_for_node(pw, &sink_node_name).await else {
                tracing::warn!("idle sender: sink '{sink_node_name}' did not appear");
                continue;
            };
            let port = self.alloc_port(&HashSet::new());
            let filter = std::collections::HashSet::from([fullname]);
            match sendspin_server::start_server_per_device(
                &sink_node_name,
                &format!("idle: {}", routing::output_display_name(dev)),
                port,
                sink_node_id,
                filter,
                send_ahead_us,
                control.clone(),
                devices.clone(),
            )
            .await
            {
                Ok(server) => {
                    tracing::info!("idle sender for '{dev}' up on port {port} (silence until announced)");
                    self.idle_senders.insert(dev.clone(), IdleSender { sink_node_name, sink_node_id, port, _server: server });
                }
                Err(e) => {
                    tracing::warn!("idle sender for '{dev}': failed to start: {e}");
                    let (t, r) = oneshot::channel();
                    if pw_cmd.send(PwCommand::DestroySinkNode { node_id: sink_node_id, reply: t }).is_ok() {
                        let _ = r.await;
                    }
                }
            }
        }
    }
}

/// A short human label for a group's embedded server / logs.
fn group_display(d: &DesiredGroup) -> String {
    let names: Vec<String> = d.sendspin_node_names.iter().map(|n| routing::output_display_name(n)).collect();
    format!("group: {}", names.join(", "))
}

/// Poll until `node_name` is present in the live registry (or give up). Mirrors
/// sendspin_server's old wait-for-node before linking a freshly-created sink.
async fn wait_for_node(pw: &SharedState, node_name: &str) -> Option<u32> {
    for _ in 0..40 {
        if let Some(id) = node_id_for(&pw.lock_recover(), node_name) {
            return Some(id);
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    None
}
