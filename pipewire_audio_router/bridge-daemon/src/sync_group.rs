//! Routing-driven **sync grouping** reconciler — unifies sendspin multi-room
//! grouping and RTP→RAOP anchoring into one model, so sendspin speakers and
//! RAOP/AirPlay receivers routed from the same sources play the same audio off
//! one clock.
//!
//! ## The model
//!
//! Grouping is derived from routing intent, not declared: **every output routed
//! from the same set of sources belongs to one group**. Each group is backed by
//! one real `support.null-audio-sink` — the *sync anchor* (`SYNC_GRP_PREFIX`) —
//! which is the group's shared clock/timeline:
//!
//! - the group's sources are linked **into** the anchor;
//! - a filtered sendspin server (sendspin_server::start_server) captures **from**
//!   the anchor and dials exactly the group's sendspin devices, pushing one
//!   timestamped stream so they sync (see sendspin's `Group`);
//! - every RAOP output in the group is fed from the anchor's **monitor**, so it
//!   follows the same clock (a RAOP sink is a `node.network` follower and can't
//!   drive a cycle — the anchor is also what lets a non-driver source like the
//!   RTP bridge reach RAOP at all; a direct link would stall the graph).
//!
//! Because the anchor is one stable node per source-set, sendspin devices and
//! RAOP outputs can come and go — and the sendspin server can be restarted when
//! its dialed set changes — without disturbing the anchor or the other members
//! fed from it.
//!
//! ## What is *not* anchored
//!
//! A lone RAOP output fed only by driver-capable sources (e.g. the AirPlay
//! receive source), with no sendspin groupmate, is left as a **direct** link by
//! routing::reconcile — snappier, one fewer buffer. `routing::raop_uses_anchor`
//! is the single predicate deciding this, shared so the two reconcilers never
//! both feed the same RAOP output.
//!
//! ## Reconcile
//!
//! Stateful (owns the running anchors/servers) and serialized in the single
//! reconciler task (main.rs). On each change it diffs desired groups (from
//! intent + live devices/outputs) against running ones: tears down groups that
//! are gone (dropping the server, destroying the anchor — its links go with it),
//! creates new anchors, restarts a group's sendspin server when its dialed-device
//! set changes, and adds/removes RAOP monitor links as outputs join/leave.

use crate::config::SYNC_GRP_PREFIX;
use crate::locks::LockRecover;
use crate::pw_thread::{PwCommand, PwCommandSender, SharedState};
use crate::raop::RAOP_NODE_PREFIX;
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
    /// PRESENT RAOP output node names in this group (sorted) — monitor-linked.
    raop_node_names: Vec<String>,
}

impl DesiredGroup {
    fn new(sources: &BTreeSet<&str>) -> Self {
        Self {
            sources: sources.iter().map(|s| s.to_string()).collect(),
            sendspin_node_names: Vec::new(),
            sendspin_fullnames: HashSet::new(),
            raop_node_names: Vec::new(),
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
    /// RAOP outputs currently monitor-linked to the anchor.
    raop_members: Vec<String>,
}

#[derive(Default)]
pub struct GroupReconciler {
    /// Keyed by the group's source-set (sorted sources joined by `KEY_SEP`).
    running: HashMap<String, RunningGroup>,
}

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
    /// Present RAOP output node names in the group.
    pub raop_members: Vec<String>,
}

impl GroupReconciler {
    /// Snapshot every running group (anchor + members) for the alignment API.
    pub fn snapshot(&self) -> Vec<GroupSnapshot> {
        self.running
            .iter()
            .map(|(key, g)| GroupSnapshot {
                sources: key.split(KEY_SEP).map(str::to_string).collect(),
                anchor_node_id: g.anchor_node_id,
                sendspin_members: g.server_devices.clone(),
                raop_members: g.raop_members.clone(),
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

/// Compute the groups the current intent + live devices/outputs call for.
///
/// A group is materialized (gets an anchor) as soon as it has a live consumer
/// that needs one: a present sendspin device, or a present RAOP output that
/// `routing::raop_uses_anchor` puts on the anchor. Members are grouped by their
/// exact source-set, so a sendspin device and a RAOP output fed from the same
/// sources land in one group and share its clock.
fn compute_desired(
    intent: &[RoutingLink],
    devices: &BTreeMap<String, SendspinDevice>,
    present_raop: &BTreeSet<String>,
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

    // Present RAOP outputs that use the anchor → members of their group.
    let raop_outputs: BTreeSet<&str> =
        intent.iter().map(|l| l.output.as_str()).filter(|o| o.starts_with(RAOP_NODE_PREFIX)).collect();
    for raop in raop_outputs {
        if !present_raop.contains(raop) || !routing::raop_uses_anchor(intent, raop) {
            continue;
        }
        let sources = routing::source_set_of(intent, raop);
        if sources.is_empty() {
            continue;
        }
        let g = groups.entry(source_key(&sources)).or_insert_with(|| DesiredGroup::new(&sources));
        g.raop_node_names.push(raop.to_string());
    }

    for g in groups.values_mut() {
        g.sendspin_node_names.sort();
        g.raop_node_names.sort();
    }
    groups
}

impl GroupReconciler {
    pub fn new() -> Self {
        Self::default()
    }

    /// Lowest free port at/above the base not used by a running group.
    fn alloc_port(&self) -> u16 {
        let used: HashSet<u16> = self.running.values().map(|g| g.port).collect();
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
    ) {
        let intent = routing_store::snapshot(routing);
        let devices_map = devices.lock_recover().clone();
        let present_raop: BTreeSet<String> = {
            let st = pw.lock_recover();
            st.nodes.values().filter(|n| n.node_name.starts_with(RAOP_NODE_PREFIX)).map(|n| n.node_name.clone()).collect()
        };
        let desired = compute_desired(&intent, &devices_map, &present_raop);

        // 1. Tear down groups no longer desired (server first, then the anchor —
        //    destroying the anchor node takes its source/monitor links with it).
        let stale: Vec<String> = self.running.keys().filter(|k| !desired.contains_key(*k)).cloned().collect();
        for key in stale {
            if let Some(rg) = self.running.remove(&key) {
                tracing::info!(
                    "tearing down sync group {} ({} sendspin, {} raop)",
                    rg.anchor_node_name,
                    rg.server_devices.len(),
                    rg.raop_members.len()
                );
                drop(rg.server);
                let (tx, rx) = oneshot::channel();
                if pw_cmd.send(PwCommand::DestroySinkNode { node_id: rg.anchor_node_id, reply: tx }).is_ok() {
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
                let port = self.alloc_port();
                tracing::info!("created sync anchor '{anchor_node_name}' (id {anchor_node_id}) for source(s) {:?}", d.sources);
                self.running.insert(
                    key.clone(),
                    RunningGroup {
                        anchor_node_name,
                        anchor_node_id,
                        port,
                        server: None,
                        server_devices: Vec::new(),
                        raop_members: Vec::new(),
                    },
                );
            }

            // Snapshot what we need so no borrow of `self.running` is held across
            // an await (the async link/server calls below).
            let (anchor_name, anchor_id, port, prev_devices, prev_raop) = {
                let rg = self.running.get(key).expect("just inserted");
                (rg.anchor_node_name.clone(), rg.anchor_node_id, rg.port, rg.server_devices.clone(), rg.raop_members.clone())
            };

            // b. Wire each source into the anchor (idempotent).
            for source in &d.sources {
                routing::ensure_link_by_name(pw, pw_cmd, source, &anchor_name).await;
            }

            // c. (Re)start the sendspin server when the dialed-device set changes.
            //    The dial filter is fixed at start, so a membership change means
            //    drop-and-recreate — but only the server, not the anchor, so the
            //    RAOP outputs fed from the same anchor never blip.
            if d.sendspin_node_names != prev_devices {
                if let Some(rg) = self.running.get_mut(key) {
                    rg.server = None; // drop old server (stops its capture/dial)
                    rg.server_devices = Vec::new();
                }
                if !d.sendspin_node_names.is_empty() {
                    match sendspin_server::start_server(
                        &anchor_name,
                        &group_display(d),
                        port,
                        anchor_id,
                        Some(d.sendspin_fullnames.clone()),
                        Some(send_ahead_us),
                        control.clone(),
                        devices.clone(),
                    )
                    .await
                    {
                        Ok(handle) => {
                            tracing::info!(
                                "sync group '{anchor_name}': sendspin server on port {port} dialing {} device(s)",
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

            // d. RAOP monitor links: attach newly-joined outputs, detach departed
            //    ones (idempotent both ways).
            for raop in &d.raop_node_names {
                if !prev_raop.contains(raop) {
                    tracing::info!("sync group '{anchor_name}': attaching RAOP output '{raop}'");
                }
                routing::ensure_monitor_link_by_name(pw, pw_cmd, &anchor_name, raop).await;
            }
            for raop in &prev_raop {
                if !d.raop_node_names.contains(raop) {
                    tracing::info!("sync group '{anchor_name}': detaching RAOP output '{raop}'");
                    routing::destroy_links_between(pw, pw_cmd, &anchor_name, raop).await;
                }
            }
            if let Some(rg) = self.running.get_mut(key) {
                rg.raop_members = d.raop_node_names.clone();
            }
        }
    }
}

/// A short human label for a group's embedded server / logs.
fn group_display(d: &DesiredGroup) -> String {
    let names: Vec<String> = d
        .sendspin_node_names
        .iter()
        .chain(d.raop_node_names.iter())
        .map(|n| routing::output_display_name(n))
        .collect();
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
