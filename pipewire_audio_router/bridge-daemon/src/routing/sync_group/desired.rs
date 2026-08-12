//! Computing the desired group set from the routing intent.
//!
//! [`compute_desired`] turns "these sources are linked to these outputs" into the
//! set of groups that should exist, and [`group_hash`]/[`source_key`] give each one a
//! stable identity.
//!
//! **The identity is the source set, never the membership.** A group whose identity
//! included its members changed identity whenever one was added or removed, which
//! restarted the whole group's streams — audible on every member, to add one. The
//! members are state *inside* a group; the sources are what the group is.

use super::*;

/// Stable, deterministic short id for a group key (no rng/time — those aren't
/// available and would break determinism; `DefaultHasher` has fixed keys).
pub(crate) fn group_hash(key: &str) -> String {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    key.hash(&mut h);
    format!("{:016x}", h.finish())
}

/// The group key for a sorted source-set.
pub(crate) fn source_key(sources: &BTreeSet<&str>) -> String {
    sources.iter().copied().collect::<Vec<_>>().join(&KEY_SEP.to_string())
}

/// Compute the groups the current intent + live devices call for.
///
/// A group is materialized (gets an anchor) as soon as it has a live consumer
/// that needs one: a present sendspin device or a present AP2 receiver. Members
/// are grouped by their exact source-set, so a sendspin device and an AP2
/// receiver fed from the same sources land in one group and share its clock.
pub(crate) fn compute_desired(
    intent: &[RoutingLink],
    devices: &BTreeMap<String, SendspinDevice>,
    ap2_devices: &BTreeMap<String, crate::outputs::ap2::discovery::Ap2Device>,
    ap2_latencies: &BTreeMap<String, u16>,
    pwsink_hosts: &PwsinkHosts,
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
        // A device with no resolved URL yet can't be dialed; it joins on a later
        // reconcile once mDNS has resolved it (the reconciler is nudged then).
        if let Some(url) = &dev.url {
            g.sendspin_members.push((dev.fullname.clone(), url.clone()));
        }
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

    // Connected receiver hosts (remote PipeWire machines) → members of their group.
    // The audio path (per-target AppleMIDI sender) is built in reconcile step (e).
    // Membership is the agent being connected: it is the thing that will be told to
    // receive the session, and its node name is the one routing intent carries.
    for node in pwsink_hosts.keys() {
        let sources = routing::source_set_of(intent, node);
        if sources.is_empty() {
            continue;
        }
        let g = groups.entry(source_key(&sources)).or_insert_with(|| DesiredGroup::new(&sources));
        g.pwsink_members.push(node.clone());
    }

    for g in groups.values_mut() {
        g.sendspin_node_names.sort();
        g.sendspin_members.sort();
        g.ap2_members.sort();
        g.pwsink_members.sort();
    }
    groups
}
