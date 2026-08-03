//! Graph inspection + node volume for the agent side.
//!
//! The volume half is the same mechanism as `bridge-daemon/src/volume.rs` —
//! `channelVolumes` on a node's SPA `Props`, with the cubic↔linear conversion
//! wpctl and HA's `volume_level` both use (`channelVolumes = V³`). P1 factors
//! this into the shared `pw-control` crate (docs/receiver-agent-plan.md §12);
//! until then it is a copy, kept deliberately small.
//!
//! The graph half is agent-specific: to control the *master out* we first have to
//! know which sink our received audio lands in. Rather than reading
//! `default.audio.sink` metadata (pipewire-rs 0.10 has no typed Metadata proxy),
//! we follow the link out of our own receive stream — authoritative by
//! construction, and it tracks the user moving the stream to another device
//! (plan §6).
//!
//! One trap the S1 spike surfaced: `module-rtp-session` creates **two** stream
//! nodes per session with the same `node.name` (a send stream in the INPUT
//! direction and a receive stream in the OUTPUT direction, see `make_session` in
//! module-rtp-session.c). Only the latter feeds a sink, so
//! [`Graph::find_receive_stream`] prefers the candidate that actually has an
//! outgoing link.

use anyhow::anyhow;
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
fn channel_volumes_props_pod(linear: &[f32]) -> anyhow::Result<Vec<u8>> {
    let object = Object {
        type_: pw::spa::sys::SPA_TYPE_OBJECT_Props,
        id: pw::spa::sys::SPA_PARAM_Props,
        properties: vec![Property::new(
            pw::spa::sys::SPA_PROP_channelVolumes,
            Value::ValueArray(ValueArray::Float(linear.to_vec())),
        )],
    };
    let bytes = PodSerializer::serialize(std::io::Cursor::new(Vec::new()), &Value::Object(object))
        .map_err(|e| anyhow!("serialize Props pod: {e}"))?
        .0
        .into_inner();
    Ok(bytes)
}

/// A short-lived PipeWire client connection with a synchronous roundtrip helper.
struct Session {
    mainloop: pw::main_loop::MainLoopRc,
    _context: pw::context::ContextRc,
    core: pw::core::CoreRc,
    registry: pw::registry::RegistryRc,
}

impl Session {
    fn connect() -> anyhow::Result<Self> {
        pw::init();
        let mainloop = pw::main_loop::MainLoopRc::new(None).map_err(|e| anyhow!("mainloop: {e}"))?;
        let context = pw::context::ContextRc::new(&mainloop, None).map_err(|e| anyhow!("context: {e}"))?;
        let core = context.connect_rc(None).map_err(|e| anyhow!("connect to PipeWire: {e}"))?;
        let registry = core.get_registry_rc().map_err(|e| anyhow!("get registry: {e}"))?;
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
        // Two-step on purpose: returning the temporary directly keeps the
        // `RefMut` alive past `found`'s drop (E0597).
        let node = found.borrow_mut().take();
        node
    }

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

/// One node in the snapshot.
#[derive(Debug, Clone)]
pub struct NodeInfo {
    pub id: u32,
    pub name: String,
    pub media_class: Option<String>,
    /// `rtp.session` — set per discovered session by `module-rtp-session`, so it
    /// identifies *which* router session a stream belongs to.
    pub session: Option<String>,
}

/// A one-shot snapshot of nodes and links, enough to walk stream → sink.
pub struct Graph {
    nodes: Vec<NodeInfo>,
    /// `(output_node, input_node)` per link.
    links: Vec<(u32, u32)>,
}

/// A playback-stream node bound during the registry pass so its *full* props
/// (which carry `rtp.session`) arrive on the same connection.
///
/// Binding has to happen inside the `global` callback: registry globals are
/// emitted once per proxy, so a listener registered after the first roundtrip
/// never sees the nodes that already went past (S1 spike finding — the reason
/// this isn't a simple two-pass snapshot).
struct StreamProbe {
    id: u32,
    session: Rc<RefCell<Option<String>>>,
    _node: pw::node::Node,
    _listener: pw::node::NodeListener,
}

/// Nodes plus `(output_node, input_node)` links, as collected by the registry
/// callback.
type Collected = Rc<RefCell<(Vec<NodeInfo>, Vec<(u32, u32)>)>>;

impl Graph {
    pub fn snapshot() -> anyhow::Result<Self> {
        let session = Session::connect()?;
        let collected: Collected = Rc::new(RefCell::new((Vec::new(), Vec::new())));
        let probes: Rc<RefCell<Vec<StreamProbe>>> = Rc::new(RefCell::new(Vec::new()));
        let _listener = session
            .registry
            .add_listener_local()
            .global({
                let collected = collected.clone();
                let probes = probes.clone();
                let registry = session.registry.clone();
                move |global| {
                    let Some(props) = global.props else { return };
                    match global.type_ {
                        pw::types::ObjectType::Node => {
                            let media_class = props.get("media.class").map(str::to_string);
                            // Only playback streams can be a receive stream, and on
                            // a desktop there are a handful — so probing just these
                            // keeps the snapshot cheap.
                            if media_class.as_deref() == Some("Stream/Output/Audio") {
                                if let Ok(node) = registry.bind::<pw::node::Node, _>(global) {
                                    let value: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
                                    let listener = node
                                        .add_listener_local()
                                        .info({
                                            let value = value.clone();
                                            move |info| {
                                                if let Some(p) = info.props() {
                                                    if let Some(v) = p.get("rtp.session") {
                                                        *value.borrow_mut() = Some(v.to_string());
                                                    }
                                                }
                                            }
                                        })
                                        .register();
                                    probes.borrow_mut().push(StreamProbe {
                                        id: global.id,
                                        session: value,
                                        _node: node,
                                        _listener: listener,
                                    });
                                }
                            }
                            collected.borrow_mut().0.push(NodeInfo {
                                id: global.id,
                                name: props.get("node.name").unwrap_or_default().to_string(),
                                media_class,
                                session: None, // filled from the probes below
                            });
                        }
                        pw::types::ObjectType::Link => {
                            let out = props.get("link.output.node").and_then(|v| v.parse::<u32>().ok());
                            let inp = props.get("link.input.node").and_then(|v| v.parse::<u32>().ok());
                            if let (Some(out), Some(inp)) = (out, inp) {
                                collected.borrow_mut().1.push((out, inp));
                            }
                        }
                        _ => {}
                    }
                }
            })
            .register();
        // First roundtrip: registry globals (and the binds they triggered).
        session.roundtrip();
        // Second: the bound nodes' info events, carrying the full props.
        session.roundtrip();

        let (mut nodes, links) = collected.borrow().clone();
        for probe in probes.borrow().iter() {
            let value = probe.session.borrow().clone();
            if let Some(node) = nodes.iter_mut().find(|n| n.id == probe.id) {
                node.session = value;
            }
        }
        Ok(Self { nodes, links })
    }

    /// Our receive stream: matched by `rtp.session` when known (authoritative per
    /// session), otherwise by node name. Prefers a candidate with an outgoing
    /// link, because `module-rtp-session` publishes a same-named send stream too.
    pub fn find_receive_stream(&self, node_name: &str, session: Option<&str>) -> Option<&NodeInfo> {
        let matches: Vec<&NodeInfo> = self
            .nodes
            .iter()
            .filter(|n| match session {
                Some(s) => n.session.as_deref() == Some(s),
                None => n.name == node_name,
            })
            .collect();
        matches
            .iter()
            .copied()
            .find(|n| self.links.iter().any(|(out, _)| *out == n.id))
            .or_else(|| matches.first().copied())
    }

    /// The node our stream's output is linked into — the sink that actually plays
    /// it, and therefore the "master out" the agent controls.
    pub fn linked_sink(&self, stream_id: u32) -> Option<u32> {
        self.links.iter().find(|(out, _)| *out == stream_id).map(|(_, inp)| *inp)
    }

    pub fn node(&self, id: u32) -> Option<&NodeInfo> {
        self.nodes.iter().find(|n| n.id == id)
    }
}

/// Reads `node_id`'s volume on the cubic 0.0-1.0 scale, `None` if it has no
/// volume control.
pub fn get_volume(node_id: u32) -> anyhow::Result<Option<f32>> {
    let session = Session::connect()?;
    let Some(node) = session.bind_node(node_id) else {
        return Ok(None);
    };
    Ok(session.read_channel_volumes(&node).and_then(|v| v.first().copied()).map(linear_to_cubic))
}

/// Sets `node_id`'s volume (cubic 0.0-1.0) on every channel.
pub fn set_volume(node_id: u32, volume: f32) -> anyhow::Result<()> {
    let session = Session::connect()?;
    let Some(node) = session.bind_node(node_id) else {
        return Err(anyhow!("no such node: {node_id}"));
    };
    let channels = session.read_channel_volumes(&node).map(|v| v.len()).filter(|n| *n > 0).unwrap_or(2);
    let linear = vec![cubic_to_linear(volume); channels];
    let bytes = channel_volumes_props_pod(&linear)?;
    let pod = Pod::from_bytes(&bytes).ok_or_else(|| anyhow!("invalid Props pod"))?;
    node.set_param(ParamType::Props, 0, pod);
    // Flush the set request to the server before the connection drops.
    session.roundtrip();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(id: u32, name: &str, session: Option<&str>) -> NodeInfo {
        NodeInfo { id, name: name.into(), media_class: None, session: session.map(str::to_string) }
    }

    #[test]
    fn cubic_matches_wpctl_reference_points() {
        assert!((cubic_to_linear(0.5) - 0.125).abs() < 1e-6);
        assert!((cubic_to_linear(0.25) - 0.015625).abs() < 1e-6);
        for v in [0.0f32, 0.1, 0.5, 1.0] {
            assert!((linear_to_cubic(cubic_to_linear(v)) - v).abs() < 1e-5);
        }
    }

    #[test]
    fn prefers_the_linked_twin_of_a_same_named_stream_pair() {
        // module-rtp-session publishes a send and a receive stream with the same
        // node.name; only the receive one is linked to a sink.
        let graph = Graph {
            nodes: vec![node(40, "pwsink-in", None), node(41, "pwsink-in", None), node(50, "alsa-sink", None)],
            links: vec![(41, 50)],
        };
        assert_eq!(graph.find_receive_stream("pwsink-in", None).unwrap().id, 41);
        assert_eq!(graph.linked_sink(41), Some(50));
        assert_eq!(graph.node(50).unwrap().name, "alsa-sink");
    }

    #[test]
    fn session_match_wins_over_name_so_two_routers_stay_apart() {
        let graph = Graph {
            nodes: vec![node(40, "pwsink-in", Some("pwrouter-a")), node(41, "pwsink-in", Some("pwrouter-b"))],
            links: vec![(40, 50), (41, 51)],
        };
        assert_eq!(graph.find_receive_stream("pwsink-in", Some("pwrouter-b")).unwrap().id, 41);
    }

    #[test]
    fn unlinked_stream_reports_no_sink() {
        let graph = Graph { nodes: vec![node(40, "pwsink-in", None)], links: vec![] };
        assert_eq!(graph.find_receive_stream("pwsink-in", None).unwrap().id, 40);
        assert_eq!(graph.linked_sink(40), None);
    }
}
