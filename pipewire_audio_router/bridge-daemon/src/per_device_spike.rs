//! S3 spike — **one per-device PipeWire node end-to-end**, on demand, without
//! touching the production sync-group reconciler.
//!
//! This validates the O-B building block (see docs/spike-results-and-status.md):
//! a single sendspin device driven through *its own* `support.null-audio-sink`
//! node + its own capture + a single-member sendspin sender — proving the device
//! is reachable as a first-class, independently-routable node, not only as a
//! member of the shared group stream.
//!
//! It is inert until `POST /api/spike/per-device` is called, and fully torn down
//! by `DELETE /api/spike/per-device` (or on daemon exit). Because every sendspin
//! device is normally dialed by its production group's server, the spike first
//! **frees** the target device by removing its routing links (remembered and
//! restored on teardown) and nudging a reconcile, so the production server drops
//! it and this single-member server wins the dial. One spike at a time.
//!
//! What this spike does NOT do: share a timeline across devices (that is S1, for
//! multi-device sync) or wire per-device nodes into the reconciler (that is the
//! productionization after S1). It is deliberately the smallest thing that plays
//! audio to a real device through a per-device node.

use crate::outputs::sendspin;
use crate::outputs::sendspin::discovery::SharedSendspinDevices;
use crate::outputs::sendspin::server::SendspinServerHandle;
use crate::outputs::sendspin::volume::SharedSendspinControl;
use crate::pw::thread::{ChangeNotifier, PwCommand, PwCommandSender, SharedState};
use crate::routing::{self, node_id_for};
use crate::store;
use crate::store::routing::SharedRouting;
use crate::util::locks::LockRecover;
use serde::Serialize;
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tokio::sync::oneshot;
use tokio::sync::Mutex;

/// Distinctive sink-name prefix. Deliberately NOT `sendspin-dev-`/`raop-out-` so
/// the routing/group reconciler never treats this scratch sink as an output.
const PERDEV_PREFIX: &str = "perdev-";

/// Port for the spike's embedded sendspin listener. Above the group range
/// (`GROUP_BASE_PORT` = 8930, growing upward) so it never collides.
const SPIKE_PORT: u16 = 8999;

/// Port for the multi-device (shared-timeline) spike's listener.
const MULTI_PORT: u16 = 8998;

/// One running per-device spike. Dropping it stops the sender/capture; the sink
/// node and the freed routing links are cleaned up by [`stop`] (Drop can't be
/// async), so always tear down through `stop`, not by dropping this.
struct PerDeviceSpike {
    device_node_name: String,
    sink_node_name: String,
    sink_node_id: u32,
    /// Production routing links removed to free the device, to restore on stop.
    freed_links: Vec<(String, String)>,
    _server: SendspinServerHandle,
}

fn slot() -> &'static Arc<Mutex<Option<PerDeviceSpike>>> {
    static SLOT: OnceLock<Arc<Mutex<Option<PerDeviceSpike>>>> = OnceLock::new();
    SLOT.get_or_init(|| Arc::new(Mutex::new(None)))
}

/// What the start endpoint reports back.
#[derive(Debug, Serialize)]
pub struct SpikeInfo {
    pub device_node_name: String,
    pub device_fullname: String,
    /// The per-device sink node created (a real, routable PipeWire node).
    pub sink_node_name: String,
    pub sink_node_id: u32,
    /// Source linked into the per-device sink (its audio path), if any.
    pub source: Option<String>,
    /// How many production routing links were removed to free the device.
    pub freed_links: usize,
    pub message: String,
}

/// Poll until `node_name` is present in the live registry (or give up).
async fn wait_for_node(pw: &SharedState, node_name: &str) -> Option<u32> {
    for _ in 0..40 {
        if let Some(id) = node_id_for(&pw.lock_recover(), node_name) {
            return Some(id);
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    None
}

/// Start the per-device spike for `device_node_name`, optionally linking `source`
/// into the per-device sink to give it audio.
#[allow(clippy::too_many_arguments)]
pub async fn start(
    device_node_name: &str,
    source: Option<&str>,
    pw: &SharedState,
    pw_cmd: &PwCommandSender,
    changes: &ChangeNotifier,
    routing: &SharedRouting,
    devices: &SharedSendspinDevices,
    control: &SharedSendspinControl,
    send_ahead_us: i64,
) -> Result<SpikeInfo, String> {
    let mut guard = slot().lock().await;
    if guard.is_some() {
        return Err("a per-device spike is already running; DELETE it first".to_string());
    }

    // Resolve the device's mDNS fullname (the sendspin dial filter key).
    let fullname = devices
        .lock_recover()
        .get(device_node_name)
        .map(|d| d.fullname.clone())
        .ok_or_else(|| format!("no discovered sendspin device named '{device_node_name}'"))?;

    // 1. Free the device from production routing so only our single-member server
    //    dials it. Remembered for restore on teardown.
    let freed_links: Vec<(String, String)> =
        store::routing::snapshot(routing).into_iter().filter(|l| l.output == device_node_name).map(|l| (l.source, l.output)).collect();
    if !freed_links.is_empty() {
        // Mutate the store in a *sync* helper so the (non-Send) std MutexGuard
        // never enters this async fn's state across the await below.
        remove_links(routing, changes, &freed_links);
        // Give the reconciler a moment to drop the device from its group server.
        tokio::time::sleep(Duration::from_millis(900)).await;
    }

    // 2. Create the per-device sink and wait for it in the graph.
    let suffix = device_node_name.strip_prefix(crate::util::node_names::SENDSPIN_DEV_PREFIX).unwrap_or(device_node_name);
    let sink_node_name = format!("{PERDEV_PREFIX}{suffix}");
    let (tx, rx) = oneshot::channel();
    if pw_cmd.send(PwCommand::CreateSinkNode { node_name: sink_node_name.clone(), reply: tx }).is_err() {
        restore_links(routing, changes, &freed_links);
        return Err("pipewire thread unavailable".to_string());
    }
    match rx.await {
        Ok(Ok(())) => {}
        other => {
            restore_links(routing, changes, &freed_links);
            return Err(format!("failed to create per-device sink '{sink_node_name}': {other:?}"));
        }
    }
    let Some(sink_node_id) = wait_for_node(pw, &sink_node_name).await else {
        restore_links(routing, changes, &freed_links);
        return Err(format!("per-device sink '{sink_node_name}' did not appear in the graph"));
    };

    // 3. Single-member sendspin server: capture the per-device sink, dial only
    //    this device.
    let filter = std::collections::HashSet::from([fullname.clone()]);
    let display = format!("per-device spike: {}", routing::output_display_name(device_node_name));
    let server = match sendspin::server::start_server_per_device(
        &sink_node_name,
        &display,
        SPIKE_PORT,
        sink_node_id,
        spike_members(devices, &filter),
        send_ahead_us,
        control.clone(),
        devices.clone(),
        sendspin::server::StreamPolicy::Always,
        "pcm", // spike: fixed, uncompressed — not the user's per-output choice
    )
    .await
    {
        Ok(h) => h,
        Err(e) => {
            let (t, r) = oneshot::channel();
            if pw_cmd.send(PwCommand::DestroySinkNode { node_id: sink_node_id, reply: t }).is_ok() {
                let _ = r.await;
            }
            restore_links(routing, changes, &freed_links);
            return Err(format!("failed to start per-device sender: {e}"));
        }
    };

    // 4. Give the device its audio: link the requested source into the sink.
    if let Some(src) = source {
        routing::ensure_link_by_name(pw, pw_cmd, src, &sink_node_name).await;
    }

    let info = SpikeInfo {
        device_node_name: device_node_name.to_string(),
        device_fullname: fullname,
        sink_node_name: sink_node_name.clone(),
        sink_node_id,
        source: source.map(str::to_string),
        freed_links: freed_links.len(),
        message: format!(
            "per-device node '{sink_node_name}' up, dialing 1 device{}",
            source.map(|s| format!(", fed from '{s}'")).unwrap_or_default()
        ),
    };
    *guard =
        Some(PerDeviceSpike { device_node_name: device_node_name.to_string(), sink_node_name, sink_node_id, freed_links, _server: server });
    Ok(info)
}

/// Multi-device variant (spike S1): ONE anchor sink + ONE capture + ONE
/// [`sendspin::server::SharedTimeline`] driving one single-member sender per
/// device (via [`sendspin::server::start_server_per_device`]). Proves per-device
/// senders on a shared timeline stay coincident, and avoids the per-device
/// null-sink dropout by feeding from one steady anchor monitor. Frees all target
/// devices from routing first (restored on teardown).
#[allow(clippy::too_many_arguments)]
pub async fn start_multi(
    device_node_names: &[String],
    source: Option<&str>,
    pw: &SharedState,
    pw_cmd: &PwCommandSender,
    changes: &ChangeNotifier,
    routing: &SharedRouting,
    devices: &SharedSendspinDevices,
    control: &SharedSendspinControl,
    send_ahead_us: i64,
) -> Result<SpikeInfo, String> {
    let mut guard = slot().lock().await;
    if guard.is_some() {
        return Err("a per-device spike is already running; DELETE it first".to_string());
    }
    if device_node_names.len() < 2 {
        return Err("multi-device spike needs at least 2 devices".to_string());
    }

    // Resolve every device's fullname up front.
    let mut fullnames: std::collections::HashSet<String> = std::collections::HashSet::new();
    {
        let dmap = devices.lock_recover();
        for d in device_node_names {
            match dmap.get(d) {
                Some(dev) => {
                    fullnames.insert(dev.fullname.clone());
                }
                None => return Err(format!("no discovered sendspin device named '{d}'")),
            }
        }
    }

    // Free all target devices from production routing (restored on teardown).
    let targets: std::collections::HashSet<&str> = device_node_names.iter().map(String::as_str).collect();
    let freed_links: Vec<(String, String)> = store::routing::snapshot(routing)
        .into_iter()
        .filter(|l| targets.contains(l.output.as_str()))
        .map(|l| (l.source, l.output))
        .collect();
    if !freed_links.is_empty() {
        remove_links(routing, changes, &freed_links);
        tokio::time::sleep(Duration::from_millis(900)).await;
    }

    // One anchor sink for the whole group.
    let sink_node_name = "perdev-multi".to_string();
    let (tx, rx) = oneshot::channel();
    if pw_cmd.send(PwCommand::CreateSinkNode { node_name: sink_node_name.clone(), reply: tx }).is_err() {
        restore_links(routing, changes, &freed_links);
        return Err("pipewire thread unavailable".to_string());
    }
    match rx.await {
        Ok(Ok(())) => {}
        other => {
            restore_links(routing, changes, &freed_links);
            return Err(format!("failed to create anchor sink '{sink_node_name}': {other:?}"));
        }
    }
    let Some(sink_node_id) = wait_for_node(pw, &sink_node_name).await else {
        restore_links(routing, changes, &freed_links);
        return Err(format!("anchor sink '{sink_node_name}' did not appear in the graph"));
    };

    let display = format!("per-device sync spike: {} devices", device_node_names.len());
    let server = match sendspin::server::start_server_per_device(
        &sink_node_name,
        &display,
        MULTI_PORT,
        sink_node_id,
        spike_members(devices, &fullnames),
        send_ahead_us,
        control.clone(),
        devices.clone(),
        sendspin::server::StreamPolicy::Always,
        "pcm", // spike: fixed, uncompressed — not the user's per-output choice
    )
    .await
    {
        Ok(h) => h,
        Err(e) => {
            let (t, r) = oneshot::channel();
            if pw_cmd.send(PwCommand::DestroySinkNode { node_id: sink_node_id, reply: t }).is_ok() {
                let _ = r.await;
            }
            restore_links(routing, changes, &freed_links);
            return Err(format!("failed to start per-device senders: {e}"));
        }
    };

    if let Some(src) = source {
        routing::ensure_link_by_name(pw, pw_cmd, src, &sink_node_name).await;
    }

    let info = SpikeInfo {
        device_node_name: device_node_names.join(","),
        device_fullname: fullnames.into_iter().collect::<Vec<_>>().join(","),
        sink_node_name: sink_node_name.clone(),
        sink_node_id,
        source: source.map(str::to_string),
        freed_links: freed_links.len(),
        message: format!(
            "shared-timeline anchor '{sink_node_name}' up, {} per-device senders{}",
            device_node_names.len(),
            source.map(|s| format!(", fed from '{s}'")).unwrap_or_default()
        ),
    };
    *guard =
        Some(PerDeviceSpike { device_node_name: device_node_names.join(","), sink_node_name, sink_node_id, freed_links, _server: server });
    Ok(info)
}

/// Tear down the running spike: stop the sender, destroy the per-device sink
/// (its links go with it), and restore the device's production routing links.
pub async fn stop(pw_cmd: &PwCommandSender, changes: &ChangeNotifier, routing: &SharedRouting) -> Result<String, String> {
    let spike = slot().lock().await.take().ok_or("no per-device spike is running")?;
    // Drop the server first (stops dial + capture), then destroy the sink.
    drop(spike._server);
    let (tx, rx) = oneshot::channel();
    if pw_cmd.send(PwCommand::DestroySinkNode { node_id: spike.sink_node_id, reply: tx }).is_ok() {
        let _ = rx.await;
    }
    restore_links(routing, changes, &spike.freed_links);
    Ok(format!(
        "per-device spike for '{}' stopped; sink '{}' destroyed; {} routing link(s) restored",
        spike.device_node_name,
        spike.sink_node_name,
        spike.freed_links.len()
    ))
}

/// Remove the given links from the store and nudge a reconcile. Sync (holds the
/// std MutexGuard only within this non-async fn).
fn remove_links(routing: &SharedRouting, changes: &ChangeNotifier, links: &[(String, String)]) {
    {
        let mut store = routing.lock_recover();
        for (src, out) in links {
            if let Err(e) = store.remove(src, out) {
                tracing::warn!("spike: failed to remove link {src}->{out}: {e}");
            }
        }
    }
    let _ = changes.send(());
}

/// Restore previously-removed links and nudge a reconcile. Sync, same reasoning.
fn restore_links(routing: &SharedRouting, changes: &ChangeNotifier, freed: &[(String, String)]) {
    if freed.is_empty() {
        return;
    }
    {
        let mut store = routing.lock_recover();
        for (src, out) in freed {
            if let Err(e) = store.add(src, out) {
                tracing::warn!("spike: failed to restore link {src}->{out}: {e}");
            }
        }
    }
    let _ = changes.send(());
}

/// The `(fullname, url)` pairs a spike server should supervise, from the discovery
/// registry — the same source the group reconciler uses (see sendspin_server: the
/// servers no longer browse for themselves).
fn spike_members(
    devices: &crate::outputs::sendspin::discovery::SharedSendspinDevices,
    fullnames: &std::collections::HashSet<String>,
) -> Vec<(String, String)> {
    use crate::util::locks::LockRecover;
    devices
        .lock_recover()
        .values()
        .filter(|d| fullnames.contains(&d.fullname))
        .filter_map(|d| d.url.clone().map(|url| (d.fullname.clone(), url)))
        .collect()
}
