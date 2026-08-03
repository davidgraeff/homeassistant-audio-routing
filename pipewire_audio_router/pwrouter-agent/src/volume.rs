//! One-shot graph inspection and volume control, used by the `spike-*` commands.
//!
//! The service path does not go through here — it uses the long-lived
//! `pw_thread`. This module keeps the *diagnostic* path: connect, look, act,
//! disconnect, so a host can be inspected by hand without a paired daemon. Pod
//! encoding and the cubic scale are shared with the service path via `pods`.
//!
//! Two traps this encodes, both found in the S1/S2 spikes (plan §6.1, §7.1):
//!
//! * a device sink's master volume lives in the **device's `Route`** param, not in
//!   the node's `Props` — writes to the latter are invisible to `wpctl` and get
//!   re-synced away by WirePlumber. Virtual sinks have no route, so the node is
//!   the lever there;
//! * `rtp.session` is not in a registry `global`'s reduced prop set, and registry
//!   globals are emitted once per proxy — so nodes must be bound *inside* the
//!   registry callback.

use crate::pods::{self, VolumeProps};
use anyhow::anyhow;
use pipewire as pw;
use pw::spa::param::ParamType;
use pw::spa::pod::Pod;
use std::cell::RefCell;
use std::rc::Rc;

/// A short-lived PipeWire client connection with a synchronous roundtrip helper.
pub(crate) struct Session {
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

/// A playback-stream node bound during the registry pass so its *full* props
/// (which carry `rtp.session`) arrive on the same connection.
struct StreamProbe {
    id: u32,
    session: Rc<RefCell<Option<String>>>,
    _node: pw::node::Node,
    _listener: pw::node::NodeListener,
}

/// Nodes plus `(output_node, input_node)` links, as collected by the registry
/// callback.
type Collected = Rc<RefCell<(Vec<NodeInfo>, Vec<(u32, u32)>)>>;

/// A one-shot snapshot of nodes and links, enough to walk stream → sink.
pub struct Graph {
    nodes: Vec<NodeInfo>,
    /// `(output_node, input_node)` per link.
    links: Vec<(u32, u32)>,
}

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

/// Where a sink's master volume actually lives.
pub enum Lever {
    /// A real device sink: the device's `Route` param (what `wpctl` uses).
    Route {
        session: Session,
        device: pw::device::Device,
        index: i32,
        card_device: i32,
        channels: usize,
    },
    /// A virtual sink (loopback, null-sink): its own node `Props`.
    NodeProps { session: Session, node: pw::node::Node, channels: usize },
}

impl Lever {
    pub fn describe(&self) -> String {
        match self {
            Lever::Route { index, card_device, channels, .. } => {
                format!("device Route (index={index}, device={card_device}, {channels}ch)")
            }
            Lever::NodeProps { channels, .. } => format!("node Props ({channels}ch, virtual sink)"),
        }
    }

    pub fn read(&self) -> anyhow::Result<Option<VolumeProps>> {
        match self {
            Lever::Route { session, device, card_device, .. } => {
                Ok(read_routes(session, device).into_iter().find(|r| r.device == *card_device).map(|r| r.props))
            }
            Lever::NodeProps { session, node, .. } => Ok(read_node_props(session, node)),
        }
    }

    /// Writes volume (cubic) and/or mute. Omitting `volume` keeps the current
    /// level, so a mute toggle cannot resurrect a stale one.
    pub fn write(&self, volume: Option<f32>, mute: Option<bool>) -> anyhow::Result<()> {
        let current = self.read()?;
        let cubic = volume.or_else(|| current.as_ref().and_then(|p| p.cubic())).unwrap_or(1.0);
        match self {
            Lever::Route { session, device, index, card_device, channels } => {
                let linear = pods::linear_channels(cubic, *channels);
                let bytes = pods::route_pod(*index, *card_device, &linear, mute)?;
                let pod = Pod::from_bytes(&bytes).ok_or_else(|| anyhow!("invalid Route pod"))?;
                device.set_param(ParamType::Route, 0, pod);
                session.roundtrip();
            }
            Lever::NodeProps { session, node, channels } => {
                let linear = pods::linear_channels(cubic, *channels);
                let bytes = pods::node_props_pod(&linear, mute)?;
                let pod = Pod::from_bytes(&bytes).ok_or_else(|| anyhow!("invalid Props pod"))?;
                node.set_param(ParamType::Props, 0, pod);
                session.roundtrip();
            }
        }
        Ok(())
    }
}

/// Binds `sink_id` and works out which lever controls it.
pub fn master_lever(sink_id: u32) -> anyhow::Result<Option<Lever>> {
    let session = Session::connect()?;
    // Bind the sink and the devices in one registry pass (globals are emitted once
    // per proxy, so a second pass would see nothing).
    let sink: Rc<RefCell<Option<pw::node::Node>>> = Rc::new(RefCell::new(None));
    let devices: Rc<RefCell<Vec<(u32, pw::device::Device)>>> = Rc::new(RefCell::new(Vec::new()));
    let sink_props: Rc<RefCell<Option<(Option<u32>, Option<i32>)>>> = Rc::new(RefCell::new(None));
    let listeners: Rc<RefCell<Vec<pw::node::NodeListener>>> = Rc::new(RefCell::new(Vec::new()));

    let _listener = session
        .registry
        .add_listener_local()
        .global({
            let registry = session.registry.clone();
            let sink = sink.clone();
            let devices = devices.clone();
            let sink_props = sink_props.clone();
            let listeners = listeners.clone();
            move |global| match global.type_ {
                pw::types::ObjectType::Node if global.id == sink_id => {
                    if let Ok(node) = registry.bind::<pw::node::Node, _>(global) {
                        let listener = node
                            .add_listener_local()
                            .info({
                                let sink_props = sink_props.clone();
                                move |info| {
                                    if let Some(p) = info.props() {
                                        *sink_props.borrow_mut() = Some((
                                            p.get("device.id").and_then(|v| v.parse().ok()),
                                            p.get("card.profile.device").and_then(|v| v.parse().ok()),
                                        ));
                                    }
                                }
                            })
                            .register();
                        node.subscribe_params(&[ParamType::Props]);
                        listeners.borrow_mut().push(listener);
                        *sink.borrow_mut() = Some(node);
                    }
                }
                pw::types::ObjectType::Device => {
                    if let Ok(device) = registry.bind::<pw::device::Device, _>(global) {
                        devices.borrow_mut().push((global.id, device));
                    }
                }
                _ => {}
            }
        })
        .register();
    session.roundtrip(); // globals + binds
    session.roundtrip(); // info events

    let Some(node) = sink.borrow_mut().take() else {
        return Ok(None);
    };
    let (device_id, card_device) = sink_props.borrow().unwrap_or((None, None));

    if let (Some(device_id), Some(card_device)) = (device_id, card_device) {
        // Two statements on purpose: nesting the `remove` inside the `position`
        // closure borrows `devices` twice.
        let position = devices.borrow().iter().position(|(id, _)| *id == device_id);
        let device = position.map(|i| devices.borrow_mut().remove(i));
        if let Some((_, device)) = device {
            if let Some(entry) = read_routes(&session, &device).into_iter().find(|r| r.device == card_device) {
                let channels = entry.props.channel_volumes.len().max(1);
                return Ok(Some(Lever::Route { session, device, index: entry.index, card_device, channels }));
            }
        }
    }

    let channels = read_node_props(&session, &node).map(|p| p.channel_volumes.len()).filter(|n| *n > 0);
    match channels {
        Some(channels) => Ok(Some(Lever::NodeProps { session, node, channels })),
        None => Ok(None),
    }
}

fn read_routes(session: &Session, device: &pw::device::Device) -> Vec<pods::RouteEntry> {
    let routes: Rc<RefCell<Vec<pods::RouteEntry>>> = Rc::new(RefCell::new(Vec::new()));
    let _listener = device
        .add_listener_local()
        .param({
            let routes = routes.clone();
            move |_seq, param_type, _index, _next, pod| {
                if param_type == ParamType::Route {
                    if let Some(entry) = pod.and_then(pods::parse_route) {
                        routes.borrow_mut().push(entry);
                    }
                }
            }
        })
        .register();
    device.enum_params(0, Some(ParamType::Route), 0, u32::MAX);
    session.roundtrip();
    let collected = routes.borrow().clone();
    collected
}

fn read_node_props(session: &Session, node: &pw::node::Node) -> Option<VolumeProps> {
    let props: Rc<RefCell<Option<VolumeProps>>> = Rc::new(RefCell::new(None));
    let _listener = node
        .add_listener_local()
        .param({
            let props = props.clone();
            move |_seq, param_type, _index, _next, pod| {
                if param_type == ParamType::Props {
                    if let Some(parsed) = pod.and_then(pods::parse_props) {
                        *props.borrow_mut() = Some(parsed);
                    }
                }
            }
        })
        .register();
    node.enum_params(0, Some(ParamType::Props), 0, u32::MAX);
    session.roundtrip();
    let collected = props.borrow_mut().take();
    collected
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(id: u32, name: &str, session: Option<&str>) -> NodeInfo {
        NodeInfo { id, name: name.into(), media_class: None, session: session.map(str::to_string) }
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
