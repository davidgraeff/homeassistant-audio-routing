//! Routing-driven sendspin grouping reconciler.
//!
//! Sendspin multi-room sync requires the co-playing devices to share ONE
//! `Group` fed by ONE audio stream on a shared clock — independent per-device
//! streams would drift out of sync. So grouping is derived from the routing
//! intent rather than declared: **the set of sendspin devices routed from the
//! same set of sources becomes one group**, backed by one real PipeWire sink
//! and one filtered sendspin server (sendspin_server::start_server) that dials
//! exactly those devices. The group's sources are linked into that sink.
//!
//! This is what turns "route one source to devices A + B" (two matrix cells)
//! into a synchronized A+B group automatically — the UX the user asked for.
//!
//! On each registry/device/intent change, `reconcile` diffs the desired groups
//! against the running ones: it tears down groups that are gone or whose device
//! set changed (the dial filter is fixed at start, so membership changes mean
//! recreate), starts new ones, and (idempotently) wires each group's sources
//! into its sink. Group sinks use `SENDSPIN_GRP_PREFIX` and are hidden from the
//! matrix — only the member devices appear there.

use crate::config::{SENDSPIN_DEV_PREFIX, SENDSPIN_GRP_PREFIX};
use crate::locks::LockRecover;
use crate::pw_thread::{PwCommandSender, SharedState};
use crate::routing;
use crate::routing_store::{self, SharedRouting};
use crate::sendspin_discovery::SharedSendspinDevices;
use crate::sendspin_server::{self, SendspinServerHandle};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::hash::{Hash, Hasher};

/// Ports for group sinks' embedded servers are allocated from here upward
/// (distinct from the manual-output base so they never collide).
const GROUP_BASE_PORT: u16 = 8930;

/// A group we want to exist, derived from the routing intent.
struct DesiredGroup {
    /// Sources feeding the group (sorted, unique) — linked into its sink.
    sources: Vec<String>,
    /// Member device virtual node names (sorted) — identity for "did membership
    /// change?".
    device_node_names: Vec<String>,
    /// Member device mDNS fullnames — the dial filter for the group server.
    device_fullnames: HashSet<String>,
}

/// A group currently running: its server handle (dropping it tears down the
/// sink + capture + dialer), the sink node name its sources link into, and the
/// membership snapshot used to detect changes.
struct RunningGroup {
    _handle: SendspinServerHandle,
    sink_node_name: String,
    port: u16,
    sources: Vec<String>,
    device_node_names: Vec<String>,
}

#[derive(Default)]
pub struct GroupReconciler {
    /// Keyed by the group's source-set (sorted sources joined) — devices with
    /// the same source-set share a group.
    running: HashMap<String, RunningGroup>,
}

/// Stable, deterministic short id for a group key (avoids rng/time, which
/// aren't available; `DefaultHasher` has fixed keys so it's reproducible).
fn group_hash(key: &str) -> String {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    key.hash(&mut h);
    format!("{:016x}", h.finish())
}

/// Compute the groups the current intent + live devices call for.
fn compute_desired(routing: &SharedRouting, devices: &SharedSendspinDevices) -> BTreeMap<String, DesiredGroup> {
    let intent = routing_store::snapshot(routing);
    let devices = devices.lock_recover().clone();

    // device virtual node name -> set of sources feeding it (present devices only).
    let mut dev_sources: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for link in &intent {
        if link.output.starts_with(SENDSPIN_DEV_PREFIX) && devices.contains_key(&link.output) {
            dev_sources.entry(link.output.clone()).or_default().insert(link.source.clone());
        }
    }

    let mut groups: BTreeMap<String, DesiredGroup> = BTreeMap::new();
    for (dev_node, sources) in dev_sources {
        let sorted: Vec<String> = sources.into_iter().collect();
        let key = sorted.join("\u{1f}");
        let entry = groups.entry(key).or_insert_with(|| DesiredGroup {
            sources: sorted,
            device_node_names: Vec::new(),
            device_fullnames: HashSet::new(),
        });
        if let Some(dev) = devices.get(&dev_node) {
            entry.device_fullnames.insert(dev.fullname.clone());
        }
        entry.device_node_names.push(dev_node);
    }
    for g in groups.values_mut() {
        g.device_node_names.sort();
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

    pub async fn reconcile(&mut self, pw: &SharedState, pw_cmd: &PwCommandSender, routing: &SharedRouting, devices: &SharedSendspinDevices) {
        let desired = compute_desired(routing, devices);

        // Tear down running groups that are gone or whose membership changed
        // (the dial filter is fixed at start → membership change means recreate).
        let stale: Vec<String> = self
            .running
            .iter()
            .filter(|(key, rg)| match desired.get(*key) {
                Some(d) => d.device_node_names != rg.device_node_names,
                None => true,
            })
            .map(|(key, _)| key.clone())
            .collect();
        for key in stale {
            if let Some(rg) = self.running.remove(&key) {
                tracing::info!("tearing down sendspin group {} ({} device(s))", rg.sink_node_name, rg.device_node_names.len());
                // rg dropped here → SendspinServerHandle::drop destroys the sink
                // + stops the dialer; its source links go with the sink.
            }
        }

        // Create newly-desired groups; (re)ensure source links for all.
        for (key, d) in &desired {
            if !self.running.contains_key(key) {
                let sink_node_name = format!("{SENDSPIN_GRP_PREFIX}{}", group_hash(key));
                let port = self.alloc_port();
                match sendspin_server::start_server(
                    &sink_node_name,
                    &group_display(d),
                    port,
                    Some(d.device_fullnames.clone()),
                    pw.clone(),
                    pw_cmd.clone(),
                )
                .await
                {
                    Ok(handle) => {
                        tracing::info!(
                            "started sendspin group {sink_node_name} on port {port} dialing {} device(s) from source(s) {:?}",
                            d.device_node_names.len(),
                            d.sources
                        );
                        self.running.insert(
                            key.clone(),
                            RunningGroup {
                                _handle: handle,
                                sink_node_name,
                                port,
                                sources: d.sources.clone(),
                                device_node_names: d.device_node_names.clone(),
                            },
                        );
                    }
                    Err(e) => {
                        tracing::warn!("failed to start sendspin group for sources {:?}: {e}", d.sources);
                        continue;
                    }
                }
            }
            // Wire each source into the group sink (idempotent).
            if let Some(rg) = self.running.get(key) {
                let sink = rg.sink_node_name.clone();
                for source in &rg.sources {
                    routing::ensure_link_by_name(pw, pw_cmd, source, &sink).await;
                }
            }
        }
    }
}

/// A short human label for a group's embedded server / logs.
fn group_display(d: &DesiredGroup) -> String {
    let names: Vec<String> =
        d.device_node_names.iter().map(|n| n.strip_prefix(SENDSPIN_DEV_PREFIX).unwrap_or(n).replace(['_', '-'], " ")).collect();
    format!("group: {}", names.join(", "))
}
