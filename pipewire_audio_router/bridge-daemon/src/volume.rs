//! Native node volume get/set via the node's SPA `Props` param
//! (`channelVolumes`), replacing `wpctl get-volume`/`set-volume` subprocesses.
//!
//! Like player.rs, each operation runs a short-lived PipeWire loop on a
//! blocking thread (volume ops are infrequent — the HA integration polls, and
//! announce ducks a handful of sources), which keeps it fully isolated from
//! the long-lived registry/command thread and avoids threading `!Send` proxies
//! around.
//!
//! Scale and pod encoding live in the shared `pw-control` crate (the agent needs
//! exactly the same ones — docs/receiver-agent-plan.md §12): `channelVolumes` are
//! linear gain, while the user-facing value `wpctl` and HA's `volume_level` use is
//! **cubic**, `channelVolumes = V³`.
//!
//! Note this is the *node* `Props` lever, which is right for the daemon's own
//! virtual sinks but **not** for a real device sink — there the master volume lives
//! in the device's `Route` param (plan §6.1). The daemon only ever drives its own
//! graph, so it does not need the route lever; the agent does.

use pipewire as pw;
use pw::spa::param::ParamType;
use pw::spa::pod::Pod;
use pw_control::pods;
use std::cell::RefCell;
use std::rc::Rc;

/// Extracts the `channelVolumes` array (linear) from a `Props` param pod.
fn parse_channel_volumes(pod: &Pod) -> Option<Vec<f32>> {
    pods::parse_props(pod).map(|p| p.channel_volumes).filter(|v| !v.is_empty())
}

/// A short-lived PipeWire client connection with a synchronous roundtrip
/// helper, used to bind a single node and read/write its params.
struct Session {
    mainloop: pw::main_loop::MainLoopRc,
    _context: pw::context::ContextRc,
    core: pw::core::CoreRc,
    registry: pw::registry::RegistryRc,
}

impl Session {
    fn connect() -> Result<Self, String> {
        pw::init();
        let mainloop = pw::main_loop::MainLoopRc::new(None).map_err(|e| format!("mainloop: {e}"))?;
        let context = pw::context::ContextRc::new(&mainloop, None).map_err(|e| format!("context: {e}"))?;
        let core = context.connect_rc(None).map_err(|e| format!("connect to PipeWire: {e}"))?;
        let registry = core.get_registry_rc().map_err(|e| format!("get registry: {e}"))?;
        Ok(Self { mainloop, _context: context, core, registry })
    }

    /// Runs the loop until a `core.sync` roundtrip completes — enough to have
    /// received all pending registry globals / param events queued before it.
    fn roundtrip(&self) {
        let done = Rc::new(std::cell::Cell::new(false));
        let pending = match self.core.sync(0) {
            Ok(seq) => seq,
            Err(_) => return,
        };
        let _listener = self
            .core
            .add_listener_local()
            .done({
                let done = done.clone();
                let mainloop = self.mainloop.clone();
                move |id, seq| {
                    if id == pw::core::PW_ID_CORE && seq == pending {
                        done.set(true);
                        mainloop.quit();
                    }
                }
            })
            .register();
        while !done.get() {
            self.mainloop.run();
        }
    }

    /// Binds the node with `node_id`, or `None` if no such node exists.
    fn bind_node(&self, node_id: u32) -> Option<pw::node::Node> {
        let found: Rc<RefCell<Option<pw::node::Node>>> = Rc::new(RefCell::new(None));
        let _listener = self
            .registry
            .add_listener_local()
            .global({
                let registry = self.registry.clone();
                let found = found.clone();
                move |global| {
                    if global.id == node_id && global.type_ == pw::types::ObjectType::Node {
                        if let Ok(node) = registry.bind::<pw::node::Node, _>(global) {
                            *found.borrow_mut() = Some(node);
                        }
                    }
                }
            })
            .register();
        self.roundtrip();
        let node = found.borrow_mut().take();
        node
    }

    /// Reads a bound node's linear `channelVolumes`, or `None` if it exposes no
    /// Props/channelVolumes.
    fn read_channel_volumes(&self, node: &pw::node::Node) -> Option<Vec<f32>> {
        let volumes: Rc<RefCell<Option<Vec<f32>>>> = Rc::new(RefCell::new(None));
        let _listener = node
            .add_listener_local()
            .param({
                let volumes = volumes.clone();
                move |_seq, param_type, _index, _next, pod| {
                    if param_type == ParamType::Props {
                        if let Some(pod) = pod {
                            if let Some(v) = parse_channel_volumes(pod) {
                                *volumes.borrow_mut() = Some(v);
                            }
                        }
                    }
                }
            })
            .register();
        node.enum_params(0, Some(ParamType::Props), 0, u32::MAX);
        self.roundtrip();
        let vols = volumes.borrow_mut().take();
        vols
    }
}

/// Reads `node_id`'s volume on the cubic 0.0-1.0 scale, `None` if the node has
/// no volume control (or is gone). Blocks; call on a blocking thread.
pub fn get_volume_blocking(node_id: u32) -> Result<Option<f32>, String> {
    let session = Session::connect()?;
    let Some(node) = session.bind_node(node_id) else {
        return Ok(None);
    };
    Ok(session.read_channel_volumes(&node).and_then(|v| v.first().copied()).map(pods::linear_to_cubic))
}

/// Sets `node_id`'s volume (cubic 0.0-1.0), applied to every channel. Blocks;
/// call on a blocking thread.
pub fn set_volume_blocking(node_id: u32, volume: f32) -> Result<(), String> {
    let session = Session::connect()?;
    let Some(node) = session.bind_node(node_id) else {
        return Err(format!("no such node: {node_id}"));
    };
    // Match the node's channel count so we don't shrink a >2ch node's array;
    // default to stereo if it doesn't report one yet.
    let channels = session.read_channel_volumes(&node).map(|v| v.len()).filter(|n| *n > 0).unwrap_or(2);
    let linear = pods::linear_channels(volume, channels);
    let bytes = pods::node_props_pod(&linear, None).map_err(|e| e.to_string())?;
    let pod = Pod::from_bytes(&bytes).ok_or("invalid Props pod")?;
    node.set_param(ParamType::Props, 0, pod);
    // Flush the set request to the server before the connection drops.
    session.roundtrip();
    Ok(())
}

/// Async wrapper: reads volume on a blocking thread.
pub async fn get_volume(node_id: u32) -> Result<Option<f32>, String> {
    tokio::task::spawn_blocking(move || get_volume_blocking(node_id)).await.map_err(|e| format!("volume task panicked: {e}"))?
}

/// Async wrapper: sets volume on a blocking thread.
pub async fn set_volume(node_id: u32, volume: f32) -> Result<(), String> {
    tokio::task::spawn_blocking(move || set_volume_blocking(node_id, volume)).await.map_err(|e| format!("volume task panicked: {e}"))?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_props_pod_round_trips_through_the_shared_encoder() {
        // The cubic maths itself is tested in pw-control; what matters here is that
        // this module reads back what it writes (an empty channelVolumes array is
        // reported as "no volume control", not as silence).
        let bytes = pods::node_props_pod(&pods::linear_channels(0.5, 2), None).unwrap();
        let pod = Pod::from_bytes(&bytes).unwrap();
        let volumes = parse_channel_volumes(pod).expect("channelVolumes present");
        assert_eq!(volumes.len(), 2);
        assert!((pods::linear_to_cubic(volumes[0]) - 0.5).abs() < 1e-6);

        let empty = pods::node_props_pod(&[], None).unwrap();
        assert_eq!(parse_channel_volumes(Pod::from_bytes(&empty).unwrap()), None);
    }
}
