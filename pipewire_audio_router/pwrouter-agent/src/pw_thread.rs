//! The agent's PipeWire worker thread — the only thread that touches
//! libpipewire.
//!
//! Mirrors `bridge-daemon/src/pw_thread.rs`: one `pw_context` owned by one
//! thread, commands arriving over a `pw::channel` attached to its loop, and
//! everything `!Send` (proxies, the loaded module) confined here by the compiler.
//! Events go back to the async side over a tokio channel.
//!
//! What it owns (docs/receiver-agent-plan.md §4, §6, §7):
//!
//! * the loaded `rtp-session` module — the receive side, so no config file;
//! * the graph view needed to answer *"which sink plays our audio"*: our receive
//!   stream (matched by `rtp.session`), its outgoing link, and that sink's device;
//! * the master volume/mute lever for that sink — the device's `Route` param, or
//!   the node's `Props` when the sink has no device (a virtual sink);
//! * the per-stream duck of *foreign* playback streams, ramped in steps paced by
//!   the async side, with baselines restored on unduck, on stop, and on drop.

use crate::pods;
use crate::pw_module::LoadedModule;
use crate::receiver;
use anyhow::anyhow;
use pipewire as pw;
use pw::spa::param::ParamType;
use pw::spa::pod::Pod;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Duration;

/// How often a duck/unduck ramp steps. 20 ms is inaudible as a stair-step while
/// keeping the loop wake-ups negligible.
const RAMP_STEP: Duration = Duration::from_millis(20);

/// Commands from the control plane (client.rs) to this thread.
pub enum Cmd {
    /// Become the receiver for `session`: load `rtp-session` with args the agent
    /// builds itself (the daemon supplies parameters, never a module argument
    /// string — see plan §5.1).
    LoadReceiver {
        session: String,
        ifname: Option<String>,
        jitter_ms: Option<u32>,
        reply: std::sync::mpsc::Sender<Result<(), String>>,
    },
    /// Unload the receiver (and undo any duck).
    UnloadReceiver,
    SetMasterVolume(f32),
    SetMasterMute(bool),
    /// Attenuate every *foreign* playback stream on our target sink to
    /// `depth` × its current volume, ramped over `ramp_ms`.
    DuckOthers { depth: f32, ramp_ms: u64 },
    /// Ramp foreign streams back to their pre-duck volumes.
    Unduck { ramp_ms: u64 },
    /// Advance an in-flight ramp by one step; a no-op when nothing is ramping.
    ///
    /// Ticks are driven from the async side rather than a loop timer: a
    /// `TimerSource` borrows the loop, which cannot be captured by the `'static`
    /// command callback, and an always-armed 50 Hz timer would wake an otherwise
    /// idle desktop for nothing.
    RampTick,
    /// Restore everything and quit the loop.
    Stop,
}

/// Steps a ramp of `ramp_ms` takes, and the interval between them — the async
/// side uses these to schedule [`Cmd::RampTick`].
pub fn ramp_schedule(ramp_ms: u64) -> (u32, Duration) {
    (ramp_steps(ramp_ms), RAMP_STEP)
}

/// What the control plane learns about this host.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MasterState {
    /// Cubic 0.0-1.0, `None` while unknown (no receive stream / no lever).
    pub volume: Option<f32>,
    pub muted: Option<bool>,
    /// The sink our audio lands in, for display.
    pub sink_name: Option<String>,
    /// Whether our receive stream exists and is linked.
    pub receiving: bool,
    /// True while foreign streams are ducked.
    pub ducked: bool,
}

#[derive(Debug, Clone)]
pub enum Event {
    /// Pushed on *every* change, including ones the user made locally — the agent
    /// controls the value but never owns it (plan §9.4).
    Master(MasterState),
    /// A session other than ours was discovered on this host: cross-talk that the
    /// agent deliberately does not touch yet (plan §7.1).
    ForeignSession(String),
}

/// Handle to the worker. Dropping it stops the thread and restores state.
pub struct Handle {
    cmd_tx: pw::channel::Sender<Cmd>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl Handle {
    /// A cloneable sender, for the ramp driver task.
    pub fn sender(&self) -> pw::channel::Sender<Cmd> {
        self.cmd_tx.clone()
    }

    pub fn send(&self, cmd: Cmd) {
        // A closed channel means the thread is gone; the caller's next command
        // will fail the same way, and the WS side reconnects/exits on its own.
        let _ = self.cmd_tx.send(cmd);
    }

    /// Load the receiver and wait for the module to report success.
    pub fn load_receiver(&self, session: &str, ifname: Option<String>, jitter_ms: Option<u32>) -> anyhow::Result<()> {
        let (tx, rx) = std::sync::mpsc::channel();
        self.send(Cmd::LoadReceiver { session: session.to_string(), ifname, jitter_ms, reply: tx });
        rx.recv_timeout(Duration::from_secs(5))
            .map_err(|_| anyhow!("PipeWire thread did not answer the receiver load"))?
            .map_err(|e| anyhow!(e))
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        let _ = self.cmd_tx.send(Cmd::Stop);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

/// One tracked node. Proxy and listener are kept alive so param/info events keep
/// arriving for as long as the node exists.
struct NodeState {
    name: String,
    media_class: Option<String>,
    /// `rtp.session` — only set on `rtp-session` streams (plan §7.1).
    session: Option<String>,
    /// `device.id` + `card.profile.device`: the device route this sink maps to.
    device_id: Option<u32>,
    card_device: Option<i32>,
    /// Last known per-channel linear gains (the duck baseline source).
    channel_volumes: Vec<f32>,
    proxy: pw::node::Node,
    _listener: pw::node::NodeListener,
}

struct DeviceState {
    routes: Vec<pods::RouteEntry>,
    proxy: pw::device::Device,
    _listener: pw::device::DeviceListener,
}

/// An in-flight ramp: per node, where each channel started and where it ends.
struct Ramp {
    /// node id → (from linear, to linear)
    targets: HashMap<u32, (Vec<f32>, Vec<f32>)>,
    step: u32,
    steps: u32,
}

struct State {
    /// Session name we are the receiver for; `None` until `LoadReceiver`.
    our_session: Option<String>,
    nodes: HashMap<u32, NodeState>,
    /// link id → (output node, input node)
    links: HashMap<u32, (u32, u32)>,
    devices: HashMap<u32, DeviceState>,
    module: Option<LoadedModule>,
    /// Pre-duck linear gains per foreign node, so a restore is always possible —
    /// including from `Drop` (plan §9.1).
    duck_baseline: HashMap<u32, Vec<f32>>,
    ramp: Option<Ramp>,
    last_published: Option<MasterState>,
    events: tokio::sync::mpsc::UnboundedSender<Event>,
    /// Foreign sessions already reported, so the log/event isn't repeated.
    reported_foreign: Vec<String>,
}

impl State {
    /// Our receive stream: the node carrying our `rtp.session` that has an
    /// outgoing link. `module-rtp-session` publishes a same-named send twin with
    /// no useful link (plan §7.2), so the link is what disambiguates.
    fn receive_stream(&self) -> Option<u32> {
        let session = self.our_session.as_deref()?;
        self.nodes
            .iter()
            .filter(|(_, n)| n.session.as_deref() == Some(session))
            .map(|(id, _)| *id)
            .find(|id| self.links.values().any(|(out, _)| out == id))
    }

    /// The sink that plays our audio.
    fn target_sink(&self) -> Option<u32> {
        let stream = self.receive_stream()?;
        self.links.values().find(|(out, _)| *out == stream).map(|(_, inp)| *inp)
    }

    /// Master volume/mute of the target sink, read from whichever lever applies.
    fn master(&self) -> MasterState {
        let Some(sink_id) = self.target_sink() else {
            return MasterState {
                receiving: false,
                ducked: !self.duck_baseline.is_empty(),
                ..Default::default()
            };
        };
        let sink = self.nodes.get(&sink_id);
        let props = self.master_props(sink_id);
        MasterState {
            volume: props.as_ref().and_then(|p| p.cubic()),
            muted: props.as_ref().and_then(|p| p.mute),
            sink_name: sink.map(|n| n.name.clone()),
            receiving: true,
            ducked: !self.duck_baseline.is_empty(),
        }
    }

    /// The device `Route` entry for a sink node, when it has one.
    fn route_for(&self, sink_id: u32) -> Option<(u32, pods::RouteEntry)> {
        let node = self.nodes.get(&sink_id)?;
        let device_id = node.device_id?;
        let card_device = node.card_device?;
        let device = self.devices.get(&device_id)?;
        let entry = device.routes.iter().find(|r| r.device == card_device)?;
        Some((device_id, entry.clone()))
    }

    /// Volume/mute of the sink: the device route if there is one (a real device),
    /// else the node's own `Props` (a virtual sink). Plan §6.1.
    fn master_props(&self, sink_id: u32) -> Option<pods::VolumeProps> {
        if let Some((_, entry)) = self.route_for(sink_id) {
            return Some(entry.props);
        }
        let node = self.nodes.get(&sink_id)?;
        if node.channel_volumes.is_empty() {
            return None;
        }
        Some(pods::VolumeProps { channel_volumes: node.channel_volumes.clone(), mute: None })
    }

    /// Writes volume and/or mute to the correct lever.
    fn apply_master(&self, volume: Option<f32>, mute: Option<bool>) -> anyhow::Result<()> {
        let sink_id = self.target_sink().ok_or_else(|| anyhow!("no target sink (not receiving)"))?;
        let current = self.master_props(sink_id);
        let channels = current.as_ref().map(|p| p.channel_volumes.len()).filter(|n| *n > 0).unwrap_or(2);
        // Keep the current level when only mute changes, so unmuting cannot
        // resurrect a stale volume.
        let cubic = volume.or_else(|| current.as_ref().and_then(|p| p.cubic())).unwrap_or(1.0);
        let linear = pods::linear_channels(cubic, channels);

        if let Some((device_id, entry)) = self.route_for(sink_id) {
            let device = self.devices.get(&device_id).ok_or_else(|| anyhow!("device {device_id} vanished"))?;
            let bytes = pods::route_pod(entry.index, entry.device, &linear, mute)?;
            let pod = Pod::from_bytes(&bytes).ok_or_else(|| anyhow!("invalid Route pod"))?;
            device.proxy.set_param(ParamType::Route, 0, pod);
        } else {
            let node = self.nodes.get(&sink_id).ok_or_else(|| anyhow!("sink {sink_id} vanished"))?;
            let bytes = pods::node_props_pod(&linear, mute)?;
            let pod = Pod::from_bytes(&bytes).ok_or_else(|| anyhow!("invalid Props pod"))?;
            node.proxy.set_param(ParamType::Props, 0, pod);
        }
        Ok(())
    }

    /// Foreign playback streams on our target sink: everything linked into it
    /// that is not one of our own `rtp-session` streams.
    fn foreign_streams(&self) -> Vec<u32> {
        let Some(sink_id) = self.target_sink() else { return Vec::new() };
        let our_session = self.our_session.as_deref();
        self.links
            .values()
            .filter(|(_, inp)| *inp == sink_id)
            .map(|(out, _)| *out)
            .filter(|id| {
                self.nodes
                    .get(id)
                    .map(|n| {
                        // Playback streams only: a monitor or another sink linked
                        // into this one is not an application to duck.
                        n.media_class.as_deref() == Some("Stream/Output/Audio")
                            && n.session.as_deref() != our_session
                            && n.channel_volumes.iter().any(|v| *v > 0.0)
                    })
                    .unwrap_or(false)
            })
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect()
    }

    /// Sets a node's per-stream gain (the duck lever — invisible to the user's
    /// device slider, plan §6.1).
    fn set_node_volumes(&self, node_id: u32, linear: &[f32]) {
        let Some(node) = self.nodes.get(&node_id) else { return };
        let Ok(bytes) = pods::node_props_pod(linear, None) else { return };
        let Some(pod) = Pod::from_bytes(&bytes) else { return };
        node.proxy.set_param(ParamType::Props, 0, pod);
    }

    /// Restores every ducked stream immediately, no ramp. Used by `Unduck` with
    /// `ramp_ms = 0`, by `Stop`, and by the thread's teardown.
    fn restore_now(&mut self) {
        let baseline = std::mem::take(&mut self.duck_baseline);
        for (node_id, linear) in baseline {
            self.set_node_volumes(node_id, &linear);
        }
        self.ramp = None;
    }

    fn publish(&mut self) {
        let state = self.master();
        if self.last_published.as_ref() != Some(&state) {
            self.last_published = Some(state.clone());
            let _ = self.events.send(Event::Master(state));
        }
    }

    /// Reports a session on this host that isn't ours — the cross-talk case stock
    /// `rtp-session` cannot filter (plan §7.1). Reported once per session name.
    fn note_foreign_session(&mut self, session: &str) {
        if self.reported_foreign.iter().any(|s| s == session) {
            return;
        }
        self.reported_foreign.push(session.to_string());
        tracing::warn!(
            "another router's session '{session}' is also being received on this host; \
             the agent leaves it alone (see receiver-agent-plan.md §7.1)"
        );
        let _ = self.events.send(Event::ForeignSession(session.to_string()));
    }
}

/// Spawns the worker. Events are pushed to `events`; the returned handle stops
/// the thread (restoring ducked volumes and unloading the module) when dropped.
pub fn spawn(events: tokio::sync::mpsc::UnboundedSender<Event>) -> anyhow::Result<Handle> {
    let (cmd_tx, cmd_rx) = pw::channel::channel::<Cmd>();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), String>>();

    let join = std::thread::Builder::new().name("pw-agent".into()).spawn(move || {
        let result = run(cmd_rx, events, &ready_tx);
        if let Err(e) = result {
            tracing::error!("PipeWire thread stopped: {e}");
            let _ = ready_tx.send(Err(e.to_string()));
        }
    })?;

    match ready_rx.recv_timeout(Duration::from_secs(5)) {
        Ok(Ok(())) => Ok(Handle { cmd_tx, join: Some(join) }),
        Ok(Err(e)) => Err(anyhow!("PipeWire thread failed to start: {e}")),
        Err(_) => Err(anyhow!("PipeWire thread did not come up within 5s")),
    }
}

fn run(
    cmd_rx: pw::channel::Receiver<Cmd>,
    events: tokio::sync::mpsc::UnboundedSender<Event>,
    ready: &std::sync::mpsc::Sender<Result<(), String>>,
) -> anyhow::Result<()> {
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None).map_err(|e| anyhow!("mainloop: {e}"))?;
    let context = pw::context::ContextRc::new(&mainloop, None).map_err(|e| anyhow!("context: {e}"))?;
    let core = context.connect_rc(None).map_err(|e| anyhow!("connect to PipeWire: {e}"))?;
    let registry = core.get_registry_rc().map_err(|e| anyhow!("get registry: {e}"))?;

    let state = Rc::new(RefCell::new(State {
        our_session: None,
        nodes: HashMap::new(),
        links: HashMap::new(),
        devices: HashMap::new(),
        module: None,
        duck_baseline: HashMap::new(),
        ramp: None,
        last_published: None,
        events,
        reported_foreign: Vec::new(),
    }));

    let _registry_listener = registry
        .add_listener_local()
        .global({
            let state = state.clone();
            let registry = registry.clone();
            move |global| on_global(&state, &registry, global)
        })
        .global_remove({
            let state = state.clone();
            move |id| {
                let mut st = state.borrow_mut();
                st.nodes.remove(&id);
                st.links.remove(&id);
                st.devices.remove(&id);
                st.duck_baseline.remove(&id);
                if let Some(ramp) = st.ramp.as_mut() {
                    ramp.targets.remove(&id);
                }
                st.publish();
            }
        })
        .register();

    let _cmd_receiver = cmd_rx.attach(mainloop.loop_(), {
        let state = state.clone();
        let mainloop = mainloop.clone();
        let context = context.clone();
        move |cmd| match cmd {
            Cmd::LoadReceiver { session, ifname, jitter_ms, reply } => {
                let args = receiver::rtp_session_module_args(
                    ifname.as_deref(),
                    receiver::RECEIVE_NODE_NAME,
                    receiver::RECEIVE_NODE_DESCRIPTION,
                    jitter_ms,
                );
                let mut st = state.borrow_mut();
                st.our_session = Some(session.clone());
                if st.module.is_some() {
                    // Reload rather than stack a second receiver: the daemon may
                    // re-send `welcome` after a reconnect (plan §13.4).
                    st.restore_now();
                    st.module = None;
                }
                tracing::info!("loading receiver for session '{session}': {args}");
                // SAFETY: we are on the thread that owns `context`, which is
                // pw_module's contract; the module is dropped on this thread too.
                let loaded = unsafe { LoadedModule::load(context.as_raw_ptr(), receiver::RTP_SESSION_MODULE_NAME, &args) };
                let result = match loaded {
                    Ok(module) => {
                        st.module = Some(module);
                        Ok(())
                    }
                    Err(e) => Err(e),
                };
                let _ = reply.send(result);
            }
            Cmd::UnloadReceiver => {
                let mut st = state.borrow_mut();
                st.restore_now();
                st.module = None;
                st.our_session = None;
                st.publish();
            }
            Cmd::SetMasterVolume(v) => {
                let st = state.borrow();
                if let Err(e) = st.apply_master(Some(v), None) {
                    tracing::warn!("set volume {v:.3} failed: {e}");
                }
            }
            Cmd::SetMasterMute(m) => {
                let st = state.borrow();
                if let Err(e) = st.apply_master(None, Some(m)) {
                    tracing::warn!("set mute {m} failed: {e}");
                }
            }
            Cmd::DuckOthers { depth, ramp_ms } => start_duck(&state, depth, ramp_ms),
            Cmd::Unduck { ramp_ms } => start_unduck(&state, ramp_ms),
            Cmd::RampTick => ramp_tick(&state),
            Cmd::Stop => {
                let mut st = state.borrow_mut();
                st.restore_now();
                st.module = None;
                drop(st);
                mainloop.quit();
            }
        }
    });

    let _ = ready.send(Ok(()));
    mainloop.run();

    // Belt and braces: if the loop ever exits without a `Stop` (a fatal core
    // error), still put the user's streams back.
    state.borrow_mut().restore_now();
    Ok(())
}

/// Binds the objects the agent needs and starts tracking them.
fn on_global(state: &Rc<RefCell<State>>, registry: &pw::registry::RegistryRc, global: &pw::registry::GlobalObject<&pw::spa::utils::dict::DictRef>) {
    match global.type_ {
        pw::types::ObjectType::Node => {
            let Some(props) = global.props else { return };
            let media_class = props.get("media.class").map(str::to_string);
            // Only sinks (candidate masters) and playback streams (ours + duck
            // candidates) matter; ignoring the rest keeps the bind count small.
            let interesting = matches!(media_class.as_deref(), Some("Audio/Sink") | Some("Stream/Output/Audio"));
            if !interesting {
                return;
            }
            let Ok(node) = registry.bind::<pw::node::Node, _>(global) else { return };
            let id = global.id;
            let listener = node
                .add_listener_local()
                .info({
                    let state = state.clone();
                    move |info| {
                        let mut st = state.borrow_mut();
                        if let (Some(entry), Some(props)) = (st.nodes.get_mut(&id), info.props()) {
                            // `rtp.session`, `device.id` and `card.profile.device`
                            // are *not* in the registry global's reduced prop set —
                            // they only arrive here (plan §7.1 caveat).
                            if let Some(v) = props.get("rtp.session") {
                                entry.session = Some(v.to_string());
                            }
                            if let Some(v) = props.get("device.id").and_then(|v| v.parse().ok()) {
                                entry.device_id = Some(v);
                            }
                            if let Some(v) = props.get("card.profile.device").and_then(|v| v.parse().ok()) {
                                entry.card_device = Some(v);
                            }
                            if let Some(v) = props.get("node.name") {
                                entry.name = v.to_string();
                            }
                        }
                        let foreign = {
                            let st = st;
                            let ours = st.our_session.clone();
                            st.nodes
                                .get(&id)
                                .and_then(|n| n.session.clone())
                                .filter(|s| ours.is_some() && Some(s.as_str()) != ours.as_deref())
                        };
                        if let Some(session) = foreign {
                            state.borrow_mut().note_foreign_session(&session);
                        }
                        state.borrow_mut().publish();
                    }
                })
                .param({
                    let state = state.clone();
                    move |_seq, param_type, _index, _next, pod| {
                        if param_type != ParamType::Props {
                            return;
                        }
                        let Some(props) = pod.and_then(pods::parse_props) else { return };
                        let mut st = state.borrow_mut();
                        if let Some(entry) = st.nodes.get_mut(&id) {
                            if !props.channel_volumes.is_empty() {
                                entry.channel_volumes = props.channel_volumes;
                            }
                        }
                        st.publish();
                    }
                })
                .register();
            node.subscribe_params(&[ParamType::Props]);
            state.borrow_mut().nodes.insert(
                id,
                NodeState {
                    name: props.get("node.name").unwrap_or_default().to_string(),
                    media_class,
                    session: None,
                    device_id: None,
                    card_device: None,
                    channel_volumes: Vec::new(),
                    proxy: node,
                    _listener: listener,
                },
            );
        }
        pw::types::ObjectType::Link => {
            let Some(props) = global.props else { return };
            let out = props.get("link.output.node").and_then(|v| v.parse::<u32>().ok());
            let inp = props.get("link.input.node").and_then(|v| v.parse::<u32>().ok());
            if let (Some(out), Some(inp)) = (out, inp) {
                let mut st = state.borrow_mut();
                st.links.insert(global.id, (out, inp));
                st.publish();
            }
        }
        pw::types::ObjectType::Device => {
            let Ok(device) = registry.bind::<pw::device::Device, _>(global) else { return };
            let id = global.id;
            let listener = device
                .add_listener_local()
                .param({
                    let state = state.clone();
                    move |_seq, param_type, _index, _next, pod| {
                        if param_type != ParamType::Route {
                            return;
                        }
                        let Some(entry) = pod.and_then(pods::parse_route) else { return };
                        let mut st = state.borrow_mut();
                        if let Some(dev) = st.devices.get_mut(&id) {
                            match dev.routes.iter_mut().find(|r| r.device == entry.device) {
                                Some(existing) => *existing = entry,
                                None => dev.routes.push(entry),
                            }
                        }
                        // A route change is how a *locally* made volume change
                        // reaches us (plan §9.4).
                        st.publish();
                    }
                })
                .register();
            device.subscribe_params(&[ParamType::Route]);
            state
                .borrow_mut()
                .devices
                .insert(id, DeviceState { routes: Vec::new(), proxy: device, _listener: listener });
        }
        _ => {}
    }
}

/// Builds the ramp from current volumes down to `depth` × baseline.
fn start_duck(state: &Rc<RefCell<State>>, depth: f32, ramp_ms: u64) {
    let mut st = state.borrow_mut();
    let depth = depth.clamp(0.0, 1.0);
    let streams = st.foreign_streams();
    if streams.is_empty() {
        tracing::debug!("duck requested but no foreign streams are on the target sink");
    }
    let mut targets = HashMap::new();
    for id in streams {
        let Some(node) = st.nodes.get(&id) else { continue };
        let current = node.channel_volumes.clone();
        if current.is_empty() {
            continue;
        }
        // Baseline is captured once: a duck arriving while already ducked must not
        // treat the ducked level as the level to return to.
        let baseline = st.duck_baseline.entry(id).or_insert_with(|| current.clone()).clone();
        let to: Vec<f32> = baseline.iter().map(|v| v * depth).collect();
        targets.insert(id, (current, to));
    }
    let steps = ramp_steps(ramp_ms);
    st.ramp = Some(Ramp { targets, step: 0, steps });
    let ducked = !st.duck_baseline.is_empty();
    if ducked {
        st.publish();
    }
}

/// Builds the ramp back up to the stored baselines.
fn start_unduck(state: &Rc<RefCell<State>>, ramp_ms: u64) {
    let mut st = state.borrow_mut();
    if st.duck_baseline.is_empty() {
        return;
    }
    let mut targets = HashMap::new();
    for (id, baseline) in st.duck_baseline.clone() {
        let current = st.nodes.get(&id).map(|n| n.channel_volumes.clone()).unwrap_or_else(|| baseline.clone());
        if current.is_empty() {
            continue;
        }
        targets.insert(id, (current, baseline));
    }
    let steps = ramp_steps(ramp_ms);
    st.ramp = Some(Ramp { targets, step: 0, steps });
    // Baselines are cleared by the final step, so an interrupted unduck can still
    // be restored by `restore_now`.
}

fn ramp_steps(ramp_ms: u64) -> u32 {
    ((ramp_ms as f64 / RAMP_STEP.as_millis() as f64).round() as u32).max(1)
}

/// One ramp step; a tick with no ramp in flight does nothing, so the async driver
/// can over-send ticks without coordination.
fn ramp_tick(state: &Rc<RefCell<State>>) {
    let mut st = state.borrow_mut();
    let Some(ramp) = st.ramp.as_mut() else { return };
    ramp.step += 1;
    let t = (ramp.step as f32 / ramp.steps as f32).min(1.0);
    let done = ramp.step >= ramp.steps;
    let targets: Vec<(u32, Vec<f32>, Vec<f32>)> =
        ramp.targets.iter().map(|(id, (from, to))| (*id, from.clone(), to.clone())).collect();
    if done {
        st.ramp = None;
    }
    // Whether this ramp lands *on* the baselines decides whether they may be
    // dropped — i.e. whether this was an unduck or a duck.
    let ends_at_baseline = targets
        .iter()
        .all(|(id, _, to)| st.duck_baseline.get(id).map(|b| b == to).unwrap_or(false));
    for (id, from, to) in &targets {
        let vols: Vec<f32> = from
            .iter()
            .zip(to.iter().chain(std::iter::repeat(&0.0)))
            .map(|(f, target)| f + (target - f) * t)
            .collect();
        st.set_node_volumes(*id, &vols);
    }
    if done && ends_at_baseline {
        st.duck_baseline.clear();
        st.publish();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ramp_steps_round_to_at_least_one() {
        assert_eq!(ramp_steps(0), 1);
        assert_eq!(ramp_steps(20), 1);
        assert_eq!(ramp_steps(200), 10);
        assert_eq!(ramp_steps(1000), 50);
    }
}
