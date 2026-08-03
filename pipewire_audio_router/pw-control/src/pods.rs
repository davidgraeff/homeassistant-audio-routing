//! SPA pod encode/decode for the two volume levers, plus the cubic↔linear scale.
//!
//! ## Two levers, deliberately
//!
//! * **Device `Route`** — the master volume/mute of a real device sink. This is
//!   what `wpctl` and the desktop applet read and write, so it is what a
//!   user-facing volume must drive (receiver-agent-plan.md §6.1: node `Props`
//!   writes on a device sink are invisible *and* get re-synced away by
//!   WirePlumber).
//! * **Node `Props`** — per-node/per-stream gain. What the daemon uses for its own
//!   virtual sinks, and what the agent uses to duck *other* applications' streams,
//!   where staying out of the user's device slider is a feature, not a bug.
//!
//! Scale: PipeWire's `channelVolumes` are linear gain; the value wpctl shows and
//! HA's `volume_level` expects is **cubic** (`channelVolumes = V³`).

use anyhow::anyhow;
use pipewire as pw;
use pw::spa::pod::{deserialize::PodDeserializer, serialize::PodSerializer, Object, Pod, Property, Value, ValueArray};
use pw::spa::sys as spa_sys;

/// Converts a linear `channelVolume` to the cubic 0.0-1.0 scale wpctl/HA use.
pub fn linear_to_cubic(linear: f32) -> f32 {
    linear.max(0.0).cbrt()
}

/// Converts a cubic 0.0-1.0 volume to the linear gain `channelVolumes` wants.
pub fn cubic_to_linear(v: f32) -> f32 {
    v.clamp(0.0, 1.0).powi(3)
}

/// The volume/mute payload carried by a `Props` object, wherever it appears —
/// standalone on a node, or nested inside a `Route`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct VolumeProps {
    /// Linear per-channel gains.
    pub channel_volumes: Vec<f32>,
    pub mute: Option<bool>,
}

impl VolumeProps {
    /// First channel's gain on the cubic scale.
    pub fn cubic(&self) -> Option<f32> {
        self.channel_volumes.first().copied().map(linear_to_cubic)
    }
}

/// Reads `channelVolumes`/`mute` out of a `Props` object's properties.
fn volume_props_from_object(obj: &Object) -> VolumeProps {
    let mut out = VolumeProps::default();
    for prop in &obj.properties {
        match prop.key {
            spa_sys::SPA_PROP_channelVolumes => {
                if let Value::ValueArray(ValueArray::Float(vols)) = &prop.value {
                    out.channel_volumes = vols.clone();
                }
            }
            spa_sys::SPA_PROP_mute => {
                if let Value::Bool(m) = &prop.value {
                    out.mute = Some(*m);
                }
            }
            _ => {}
        }
    }
    out
}

/// Parses a node's `Props` param pod.
pub fn parse_props(pod: &Pod) -> Option<VolumeProps> {
    let (_, value) = PodDeserializer::deserialize_any_from(pod.as_bytes()).ok()?;
    let Value::Object(obj) = value else { return None };
    Some(volume_props_from_object(&obj))
}

/// One entry of a device's `Route` param: which route (`index`) applies to which
/// of the card's devices (`device`), and its volume/mute.
#[derive(Debug, Clone, PartialEq)]
pub struct RouteEntry {
    pub index: i32,
    pub device: i32,
    pub props: VolumeProps,
}

/// Parses one `Route` param pod (devices emit one per configured route).
pub fn parse_route(pod: &Pod) -> Option<RouteEntry> {
    let (_, value) = PodDeserializer::deserialize_any_from(pod.as_bytes()).ok()?;
    let Value::Object(obj) = value else { return None };
    let mut index = None;
    let mut device = None;
    let mut props = VolumeProps::default();
    for prop in &obj.properties {
        match prop.key {
            spa_sys::SPA_PARAM_ROUTE_index => {
                if let Value::Int(v) = prop.value {
                    index = Some(v);
                }
            }
            spa_sys::SPA_PARAM_ROUTE_device => {
                if let Value::Int(v) = prop.value {
                    device = Some(v);
                }
            }
            spa_sys::SPA_PARAM_ROUTE_props => {
                if let Value::Object(inner) = &prop.value {
                    props = volume_props_from_object(inner);
                }
            }
            _ => {}
        }
    }
    Some(RouteEntry { index: index?, device: device?, props })
}

fn serialize(value: &Value) -> anyhow::Result<Vec<u8>> {
    Ok(PodSerializer::serialize(std::io::Cursor::new(Vec::new()), value).map_err(|e| anyhow!("serialize pod: {e}"))?.0.into_inner())
}

/// A `Props` object carrying `channelVolumes` (and `mute`, when given) — the pod
/// for a **node**'s volume, i.e. per-stream gain.
pub fn node_props_pod(linear: &[f32], mute: Option<bool>) -> anyhow::Result<Vec<u8>> {
    let mut properties = vec![Property::new(spa_sys::SPA_PROP_channelVolumes, Value::ValueArray(ValueArray::Float(linear.to_vec())))];
    if let Some(m) = mute {
        properties.push(Property::new(spa_sys::SPA_PROP_mute, Value::Bool(m)));
    }
    serialize(&Value::Object(Object { type_: spa_sys::SPA_TYPE_OBJECT_Props, id: spa_sys::SPA_PARAM_Props, properties }))
}

/// A `Route` object setting a device route's volume/mute — the pod for a
/// **device**'s master volume.
///
/// `save = true` mirrors what `wpctl set-volume` does, so WirePlumber persists the
/// value across restarts instead of the agent's change evaporating on the next
/// profile switch.
pub fn route_pod(index: i32, device: i32, linear: &[f32], mute: Option<bool>) -> anyhow::Result<Vec<u8>> {
    let mut inner = vec![Property::new(spa_sys::SPA_PROP_channelVolumes, Value::ValueArray(ValueArray::Float(linear.to_vec())))];
    if let Some(m) = mute {
        inner.push(Property::new(spa_sys::SPA_PROP_mute, Value::Bool(m)));
    }
    let props = Value::Object(Object { type_: spa_sys::SPA_TYPE_OBJECT_Props, id: spa_sys::SPA_PARAM_Props, properties: inner });
    serialize(&Value::Object(Object {
        type_: spa_sys::SPA_TYPE_OBJECT_ParamRoute,
        id: spa_sys::SPA_PARAM_Route,
        properties: vec![
            Property::new(spa_sys::SPA_PARAM_ROUTE_index, Value::Int(index)),
            Property::new(spa_sys::SPA_PARAM_ROUTE_device, Value::Int(device)),
            Property::new(spa_sys::SPA_PARAM_ROUTE_props, props),
            Property::new(spa_sys::SPA_PARAM_ROUTE_save, Value::Bool(true)),
        ],
    }))
}

/// `n` copies of `volume` (cubic) as linear gains — the shape both pods want.
pub fn linear_channels(volume: f32, channels: usize) -> Vec<f32> {
    vec![cubic_to_linear(volume); channels.max(1)]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cubic_matches_wpctl_reference_points() {
        assert!((cubic_to_linear(0.5) - 0.125).abs() < 1e-6);
        assert!((cubic_to_linear(0.25) - 0.015625).abs() < 1e-6);
        assert!((cubic_to_linear(0.8) - 0.512).abs() < 1e-6);
        for v in [0.0f32, 0.1, 0.25, 0.5, 0.7, 1.0] {
            assert!((linear_to_cubic(cubic_to_linear(v)) - v).abs() < 1e-5);
        }
    }

    #[test]
    fn cubic_is_clamped_both_ways() {
        assert_eq!(cubic_to_linear(-1.0), 0.0);
        assert_eq!(cubic_to_linear(2.0), 1.0);
        assert_eq!(linear_to_cubic(-0.5), 0.0);
    }

    #[test]
    fn node_props_pod_round_trips() {
        let bytes = node_props_pod(&[0.125, 0.125], Some(true)).unwrap();
        let pod = Pod::from_bytes(&bytes).unwrap();
        let parsed = parse_props(pod).unwrap();
        assert_eq!(parsed.channel_volumes, vec![0.125, 0.125]);
        assert_eq!(parsed.mute, Some(true));
        assert!((parsed.cubic().unwrap() - 0.5).abs() < 1e-6);
    }

    #[test]
    fn node_props_pod_can_omit_mute() {
        let bytes = node_props_pod(&[1.0], None).unwrap();
        let parsed = parse_props(Pod::from_bytes(&bytes).unwrap()).unwrap();
        assert_eq!(parsed.mute, None);
    }

    #[test]
    fn route_pod_round_trips_with_nested_props() {
        let bytes = route_pod(3, 4, &linear_channels(0.5, 2), Some(false)).unwrap();
        let entry = parse_route(Pod::from_bytes(&bytes).unwrap()).unwrap();
        assert_eq!(entry.index, 3);
        assert_eq!(entry.device, 4);
        assert_eq!(entry.props.mute, Some(false));
        assert!((entry.props.cubic().unwrap() - 0.5).abs() < 1e-6);
        assert_eq!(entry.props.channel_volumes.len(), 2);
    }

    #[test]
    fn linear_channels_never_yields_an_empty_array() {
        // An empty channelVolumes array would be silently ignored by PipeWire.
        assert_eq!(linear_channels(1.0, 0).len(), 1);
    }
}
