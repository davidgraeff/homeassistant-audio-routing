//! Native node volume get/set via the node's SPA `Props` param
//! (`channelVolumes`), replacing `wpctl get-volume`/`set-volume` subprocesses.
//!
//! Like player.rs, each operation runs a short-lived PipeWire loop on a
//! blocking thread (volume ops are infrequent — the HA integration polls, and
//! announce ducks a handful of sources), which keeps it fully isolated from
//! the long-lived registry/command thread and avoids threading `!Send` proxies
//! around.
//!
//! Scale: PipeWire's `channelVolumes` are linear gain, but the user-facing
//! value `wpctl` shows/sets is **cubic** — `channelVolumes = V³` (confirmed
//! empirically: `wpctl 0.5` → `0.125`, `0.25` → `0.015625`). We keep the same
//! `V` contract as before (and as HA's `volume_level` expects), cubing on set
//! and cube-rooting on read, so behaviour is identical to the old wpctl path.

use pipewire as pw;
use pw::spa;
use spa::param::ParamType;
use spa::pod::{deserialize::PodDeserializer, serialize::PodSerializer, Object, Pod, Property, Value, ValueArray};
use std::cell::RefCell;
use std::rc::Rc;

/// Converts a linear `channelVolume` to the cubic 0.0-1.0 scale wpctl/HA use.
fn linear_to_cubic(linear: f32) -> f32 {
    linear.max(0.0).cbrt()
}

/// Converts a cubic 0.0-1.0 volume to the linear gain `channelVolumes` wants.
fn cubic_to_linear(v: f32) -> f32 {
    v.clamp(0.0, 1.0).powi(3)
}

/// Extracts the `channelVolumes` array (linear) from a `Props` param pod.
fn parse_channel_volumes(pod: &Pod) -> Option<Vec<f32>> {
    let (_, value) = PodDeserializer::deserialize_any_from(pod.as_bytes()).ok()?;
    let Value::Object(obj) = value else { return None };
    for prop in obj.properties {
        if prop.key == pw::spa::sys::SPA_PROP_channelVolumes {
            if let Value::ValueArray(ValueArray::Float(vols)) = prop.value {
                return Some(vols);
            }
        }
    }
    None
}

/// Serializes a `Props` object carrying just `channelVolumes`.
fn channel_volumes_props_pod(linear: &[f32]) -> Result<Vec<u8>, String> {
    let object = Object {
        type_: pw::spa::sys::SPA_TYPE_OBJECT_Props,
        id: pw::spa::sys::SPA_PARAM_Props,
        properties: vec![Property::new(
            pw::spa::sys::SPA_PROP_channelVolumes,
            Value::ValueArray(ValueArray::Float(linear.to_vec())),
        )],
    };
    let bytes = PodSerializer::serialize(std::io::Cursor::new(Vec::new()), &Value::Object(object))
        .map_err(|e| format!("serialize Props pod: {e}"))?
        .0
        .into_inner();
    Ok(bytes)
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
    Ok(session
        .read_channel_volumes(&node)
        .and_then(|v| v.first().copied())
        .map(linear_to_cubic))
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
    let linear = vec![cubic_to_linear(volume); channels];
    let bytes = channel_volumes_props_pod(&linear)?;
    let pod = Pod::from_bytes(&bytes).ok_or("invalid Props pod")?;
    node.set_param(ParamType::Props, 0, pod);
    // Flush the set request to the server before the connection drops.
    session.roundtrip();
    Ok(())
}

/// Async wrapper: reads volume on a blocking thread.
pub async fn get_volume(node_id: u32) -> Result<Option<f32>, String> {
    tokio::task::spawn_blocking(move || get_volume_blocking(node_id))
        .await
        .map_err(|e| format!("volume task panicked: {e}"))?
}

/// Async wrapper: sets volume on a blocking thread.
pub async fn set_volume(node_id: u32, volume: f32) -> Result<(), String> {
    tokio::task::spawn_blocking(move || set_volume_blocking(node_id, volume))
        .await
        .map_err(|e| format!("volume task panicked: {e}"))?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cubic_round_trips_wpctl_reference_points() {
        // Empirically: wpctl 0.5 -> 0.125, 0.25 -> 0.015625, 0.8 -> 0.512.
        assert!((cubic_to_linear(0.5) - 0.125).abs() < 1e-6);
        assert!((cubic_to_linear(0.25) - 0.015625).abs() < 1e-6);
        assert!((cubic_to_linear(0.8) - 0.512).abs() < 1e-6);
        for v in [0.0f32, 0.1, 0.25, 0.5, 0.7, 1.0] {
            assert!((linear_to_cubic(cubic_to_linear(v)) - v).abs() < 1e-5);
        }
    }
}
