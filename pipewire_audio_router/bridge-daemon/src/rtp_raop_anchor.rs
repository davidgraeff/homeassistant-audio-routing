//! RTP-source → RAOP routing via a null-sink "anchor".
//!
//! A direct link from the Bluetooth-bridge RTP source (`module-rtp-source`) to
//! a RAOP output (`module-raop-sink`) **stalls the whole graph**: both are
//! `node.network` nodes that can't drive a PipeWire cycle, so the pair is a
//! cycle with no possible driver and PipeWire can't schedule it (audio +
//! metering freeze). Full analysis in
//! pipewire_audio_router/docs/rtp-source-to-raop-routing.md.
//!
//! The fix mirrors sendspin_group.rs: insert one shared `support.null-audio-sink`
//! (a real, driver-capable node) as a clock anchor. The RTP source feeds the
//! anchor as a follower; each RAOP target is fed from the anchor's **monitor**
//! and follows the anchor's clock — exactly as a RAOP sink follows the AirPlay
//! source (which works). One anchor fans out to every RAOP target.
//!
//! Only sources for which `routing::source_needs_raop_anchor` is true are routed
//! this way; driver-capable sources (the AirPlay receive source) link to RAOP
//! directly and are untouched. `routing::reconcile` skips the anchor pairs so it
//! never creates the stalling direct link.
//!
//! Stateless, like `routing::reconcile`: it derives everything from the routing
//! intent + live registry on each call, so it's safe to run on every change
//! event and it recovers a leftover anchor after a daemon restart. Reconciles
//! are serialized in the single reconciler task (main.rs), so create-and-wire
//! completes within one call — no double-create race.

use crate::locks::LockRecover;
use crate::pw_thread::{PwCommand, PwCommandSender, SharedState};
use crate::routing::{self, is_raop_output, node_id_for, source_needs_raop_anchor};
use crate::routing_store::{self, SharedRouting};
use std::collections::BTreeSet;
use std::time::Duration;
use tokio::sync::oneshot;

/// The single shared anchor sink node name.
pub const ANCHOR_NODE_NAME: &str = "rtp-raop-anchor";

/// Reconcile the anchor against the routing intent:
/// - if any anchor-needing source is routed to a RAOP output, ensure the anchor
///   exists and wire `source → anchor` and `anchor.monitor → each RAOP target`;
/// - otherwise, tear the anchor down.
pub async fn reconcile(pw: &SharedState, pw_cmd: &PwCommandSender, routing: &SharedRouting) {
    let mut sources: BTreeSet<String> = BTreeSet::new();
    let mut raop_targets: BTreeSet<String> = BTreeSet::new();
    for link in routing_store::snapshot(routing) {
        if source_needs_raop_anchor(&link.source) && is_raop_output(&link.output) {
            sources.insert(link.source);
            raop_targets.insert(link.output);
        }
    }

    let anchor_id = node_id_for(&pw.lock_recover(), ANCHOR_NODE_NAME);

    // Nothing routed through the anchor → tear it down (its links go with it).
    if raop_targets.is_empty() {
        if let Some(id) = anchor_id {
            let (tx, rx) = oneshot::channel();
            if pw_cmd.send(PwCommand::DestroySinkNode { node_id: id, reply: tx }).is_ok() {
                let _ = rx.await;
                tracing::info!("tore down RTP→RAOP anchor (no routed RAOP outputs)");
            }
        }
        return;
    }

    // Ensure the anchor exists (create + wait for it to appear, within this
    // call, so the wiring below finds it and we don't re-create next tick).
    if anchor_id.is_none() {
        let (tx, rx) = oneshot::channel();
        if pw_cmd.send(PwCommand::CreateSinkNode { node_name: ANCHOR_NODE_NAME.to_string(), reply: tx }).is_err() {
            return;
        }
        match rx.await {
            Ok(Ok(())) => tracing::info!("created RTP→RAOP anchor '{ANCHOR_NODE_NAME}'"),
            _ => {
                tracing::warn!("failed to create RTP→RAOP anchor");
                return;
            }
        }
        if !wait_for_node(pw, ANCHOR_NODE_NAME).await {
            tracing::warn!("RTP→RAOP anchor did not appear in the graph in time");
            return; // a later reconcile will wire it once it shows up
        }
    }

    // source → anchor (normal outputs → the null sink's playback inputs).
    for source in &sources {
        routing::ensure_link_by_name(pw, pw_cmd, source, ANCHOR_NODE_NAME).await;
    }
    // anchor.monitor → each RAOP target.
    for target in &raop_targets {
        routing::ensure_monitor_link_by_name(pw, pw_cmd, ANCHOR_NODE_NAME, target).await;
    }
}

/// Poll until `node_name` is present in the live registry (or give up). Mirrors
/// sendspin_server's wait-for-node before linking a freshly-created sink.
async fn wait_for_node(pw: &SharedState, node_name: &str) -> bool {
    for _ in 0..40 {
        if node_id_for(&pw.lock_recover(), node_name).is_some() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}
