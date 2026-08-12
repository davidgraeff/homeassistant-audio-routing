//! Owns the PipeWire connection on a dedicated OS thread.
//!
//! `pipewire-rs`'s core types (`MainLoopRc`/`ContextRc`/`CoreRc`) are
//! `Rc`-based, not `Send` — they cannot cross threads. So this thread runs
//! its own blocking main loop for the process's entire lifetime, and
//! publishes plain, thread-safe snapshots of the registry state into a
//! shared `Arc<Mutex<..>>` that the axum side (running on tokio's
//! multi-threaded runtime) reads from.
//!
//! This thread does two things: it *observes* the registry (nodes/ports/links)
//! into a shared snapshot, and it *mutates the graph* on request — loading and
//! unloading modules into its own context (how RAOP outputs are added/removed
//! live — `pw_control::module`, api/outputs.rs) and creating/destroying links
//! natively via `Core::create_object`/`Registry::destroy_global` (api/nodes.rs,
//! routing.rs). All mutations arrive over a `pipewire::channel` (`PwCommand`),
//! which attaches to the loop as an IO source and is the thread-safe way to
//! run code on this non-`Send` thread from the axum side; each command carries
//! a `oneshot` reply so the caller learns the outcome.
//!
//! Links go through this channel rather than a `pw-link` subprocess so the
//! whole daemon speaks one native PipeWire API: spike 4
//! (spikes/04-graph-control.md) measured native link creation at 0.07ms vs.
//! ~16ms for `pw-link`. Because the core/registry proxies are `!Send` and only
//! valid on this thread, the axum handlers resolve names to object ids from
//! the shared snapshot, then hand a `CreateLinks`/`DestroyLinks` command here
//! to execute where the proxies live.

use crate::util::locks::LockRecover;
use pipewire as pw;
use pipewire::link::Link;
use pipewire::properties::PropertiesBox;
use pw_control::module::LoadedModule;
use serde::Serialize;
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use tokio::sync::oneshot;

/// One output→input port pairing to link, resolved to PipeWire object ids by
/// the caller (which holds the registry snapshot) before it reaches this
/// thread.
#[derive(Debug, Clone, Copy)]
pub struct LinkSpec {
    pub out_node: u32,
    pub out_port: u32,
    pub in_node: u32,
    pub in_port: u32,
}

/// A request run on the PipeWire thread. Each variant carries a `oneshot`
/// reply so the async caller (api/nodes.rs) can await success/failure.
pub enum PwCommand {
    /// Load `module_name` with `args` and remember it under `node_name` so it
    /// can be unloaded later. `Err` if it's already loaded or libpipewire
    /// refuses the load.
    Load { node_name: String, module_name: String, args: String, reply: oneshot::Sender<Result<(), String>> },
    /// Unload the module previously loaded under `node_name`. Idempotent:
    /// unloading something not loaded is `Ok` (the desired end state — gone —
    /// already holds).
    Unload { node_name: String, reply: oneshot::Sender<Result<(), String>> },
    /// Create every given link natively via the `link-factory`. Idempotent per
    /// channel: a port pairing already present in the observed registry is
    /// reported as success rather than recreated. Reply is Ok(summary) on full
    /// success, Err(summary) if any create failed.
    CreateLinks { specs: Vec<LinkSpec>, reply: oneshot::Sender<Result<String, String>> },
    /// Destroy links by their registry global id. Missing/already-gone ids are
    /// fine — the caller wants them not to exist, and they don't.
    DestroyLinks { link_ids: Vec<u32>, reply: oneshot::Sender<Result<String, String>> },
    /// Create a `support.null-audio-sink` adapter node named `node_name` —
    /// the native equivalent of `pw-cli create-node adapter "{ ... }"`
    /// (outputs/sendspin/server.rs's capture target; replaces adapter.py's one-time
    /// `pw-cli` shellout). Fire-and-forget like `Load`: the new node shows up
    /// through the `global` listener same as any other, so the caller
    /// resolves `node_name` to a node id from the shared registry snapshot
    /// afterward rather than getting it back synchronously here.
    CreateSinkNode { node_name: String, reply: oneshot::Sender<Result<(), String>> },
    /// Destroy a node by registry global id (resolved by the caller from the
    /// shared snapshot, same division of labor as `DestroyLinks`). Missing/
    /// already-gone ids are fine, same reasoning as `DestroyLinks`.
    DestroySinkNode { node_id: u32, reply: oneshot::Sender<Result<(), String>> },
    /// Turn per-node xrun profiling on/off. On `true`, bind the `module-profiler`
    /// global and start populating the shared xrun map; on `false`, drop the
    /// subscription (so an idle install with the routing UI closed pays nothing —
    /// pw/profiler.rs). Fire-and-forget: the routing WS toggles it on the first/last
    /// matrix watcher, mirroring the peak-meter gating. No-op if the profiler
    /// global isn't present (module not loaded).
    SetProfiling(bool),
}

/// Send end of the command channel, handed to the axum side. Cheap to clone;
/// `send` is synchronous and safe to call from any thread (it wakes the
/// PipeWire loop via an eventfd).
pub type PwCommandSender = pw::channel::Sender<PwCommand>;

#[derive(Debug, Clone, Serialize)]
pub struct NodeInfo {
    pub node_id: u32,
    pub node_name: String,
    pub media_class: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PortInfo {
    pub port_id: u32,
    pub node_id: u32,
    pub port_name: String,
    pub direction: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct LinkInfo {
    pub link_id: u32,
    pub output_node: u32,
    pub input_node: u32,
    /// Port-level endpoints (`link.output.port`/`link.input.port`), tracked in
    /// addition to the node ids so link creation can be made idempotent per
    /// channel. 0 if the registry global omitted them.
    pub output_port: u32,
    pub input_port: u32,
}

#[derive(Debug, Default)]
pub struct RegistryState {
    pub nodes: BTreeMap<u32, NodeInfo>,
    pub ports: BTreeMap<u32, PortInfo>,
    pub links: BTreeMap<u32, LinkInfo>,
}

impl RegistryState {
    /// Whether any link currently feeds into this node — the basis for a
    /// media_player entity's playing/idle state: an output with an active
    /// incoming link is "playing," one with none is "idle."
    pub fn node_has_incoming_link(&self, node_id: u32) -> bool {
        self.links.values().any(|l| l.input_node == node_id)
    }
}

pub type SharedState = Arc<Mutex<RegistryState>>;

/// Fires (empty payload, just a ping) whenever the registry state changes —
/// node/port/link added or removed. The routing UI's WebSocket (routing.rs)
/// subscribes to this to push live matrix updates instead of
/// the client having to poll. `send` is synchronous and callable from any
/// thread (including this module's own dedicated PipeWire OS thread, which
/// isn't running inside a tokio runtime) — no async plumbing needed to
/// notify from here.
pub type ChangeNotifier = tokio::sync::broadcast::Sender<()>;

/// Spawns the PipeWire thread and returns the shared state handle, its change
/// notifier, the command sender, and the shared xrun map (populated by the
/// profiler while profiling is enabled — pw/profiler.rs) immediately; the thread
/// connects and starts populating the state in the background.
pub fn spawn() -> anyhow::Result<(SharedState, ChangeNotifier, PwCommandSender, crate::pw::profiler::SharedXruns)> {
    let state: SharedState = Arc::new(Mutex::new(RegistryState::default()));
    // Capacity is a lag buffer, not a queue depth requirement: subscribers
    // only ever care about "something changed, re-fetch the snapshot," so
    // a slow subscriber missing a few pings and catching up on the next one
    // is fine — this is not an event log.
    let (changes, _) = tokio::sync::broadcast::channel(16);
    let (cmd_tx, cmd_rx) = pw::channel::channel::<PwCommand>();
    let xruns = crate::pw::profiler::new_xruns();
    let state_for_thread = state.clone();
    let changes_for_thread = changes.clone();
    let xruns_for_thread = xruns.clone();
    std::thread::Builder::new().name("pipewire".into()).spawn(move || {
        if let Err(e) = run(state_for_thread, changes_for_thread, cmd_rx, xruns_for_thread) {
            tracing::error!("pipewire thread exited with error: {e:#}");
        }
    })?;
    Ok((state, changes, cmd_tx, xruns))
}

fn run(
    state: SharedState,
    changes: ChangeNotifier,
    cmd_rx: pw::channel::Receiver<PwCommand>,
    xruns: crate::pw::profiler::SharedXruns,
) -> anyhow::Result<()> {
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None)?;
    let context = pw::context::ContextRc::new(&mainloop, None)?;
    let core = context.connect_rc(None)?;
    // `_rc` variant so the registry can be cloned into the command callback
    // (for native `destroy_global`) as well as used to register listeners.
    let registry = core.get_registry_rc()?;

    let state_add = state.clone();
    let state_remove = state.clone();
    let changes_add = changes.clone();
    let changes_remove = changes.clone();
    // The `module-profiler` global's id, learned from the registry, so the
    // `SetProfiling` command can bind it on demand. And the live subscription
    // (pw/profiler.rs) while profiling is on — both `!Send`, on this thread only.
    let profiler_id: Rc<RefCell<Option<u32>>> = Rc::new(RefCell::new(None));
    let profiler_listener: Rc<RefCell<Option<crate::pw::profiler::ProfilerListener>>> = Rc::new(RefCell::new(None));
    let profiler_id_add = profiler_id.clone();
    let profiler_id_remove = profiler_id.clone();
    let profiler_listener_remove = profiler_listener.clone();
    let _listener = registry
        .add_listener_local()
        .global(move |global| {
            // The profiler global carries no node.name/port/link props we index
            // below — record its id (for on-demand binding) and move on.
            if global.type_ == pw::types::ObjectType::Profiler {
                tracing::info!("discovered profiler global (id={})", global.id);
                *profiler_id_add.borrow_mut() = Some(global.id);
                return;
            }
            let Some(props) = global.props else { return };

            if let Some(name) = props.get("node.name") {
                let info = NodeInfo {
                    node_id: global.id,
                    node_name: name.to_string(),
                    media_class: props.get("media.class").map(|s| s.to_string()),
                };
                tracing::info!("discovered node: {name} (id={})", global.id);
                state_add.lock_recover().nodes.insert(global.id, info);
                // No receivers (e.g. no UI open) is a normal, not an error,
                // case for a broadcast channel — hence the discarded result.
                let _ = changes_add.send(());
                return;
            }

            if let (Some(port_name), Some(node_id_str), Some(direction)) =
                (props.get("port.name"), props.get("node.id"), props.get("port.direction"))
            {
                if let Ok(node_id) = node_id_str.parse::<u32>() {
                    let info = PortInfo { port_id: global.id, node_id, port_name: port_name.to_string(), direction: direction.to_string() };
                    state_add.lock_recover().ports.insert(global.id, info);
                    let _ = changes_add.send(());
                    return;
                }
            }

            if let (Some(out_node_str), Some(in_node_str)) = (props.get("link.output.node"), props.get("link.input.node")) {
                if let (Ok(output_node), Ok(input_node)) = (out_node_str.parse::<u32>(), in_node_str.parse::<u32>()) {
                    let output_port = props.get("link.output.port").and_then(|s| s.parse().ok()).unwrap_or(0);
                    let input_port = props.get("link.input.port").and_then(|s| s.parse().ok()).unwrap_or(0);
                    let info = LinkInfo { link_id: global.id, output_node, input_node, output_port, input_port };
                    tracing::info!("discovered link: node {output_node} -> node {input_node} (id={})", global.id);
                    state_add.lock_recover().links.insert(global.id, info);
                    let _ = changes_add.send(());
                }
            }
        })
        .global_remove(move |id| {
            // Profiler gone (module unloaded / server restart): forget its id and
            // drop any live subscription so we don't hold a dead proxy.
            if *profiler_id_remove.borrow() == Some(id) {
                *profiler_id_remove.borrow_mut() = None;
                profiler_listener_remove.borrow_mut().take();
            }
            let mut state = state_remove.lock_recover();
            if let Some(removed) = state.nodes.remove(&id) {
                tracing::info!("node removed: {} (id={id})", removed.node_name);
            }
            state.ports.remove(&id);
            state.links.remove(&id);
            drop(state);
            let _ = changes_remove.send(());
        })
        .register();

    // Modules we've loaded into *this* context, keyed by the RAOP node name.
    // Lives entirely on this thread (LoadedModule is !Send); the RefCell gives
    // the `Fn` command callback interior mutability. Dropping an entry here
    // unloads its module and destroys its node — the live "remove output" path.
    let modules: RefCell<HashMap<String, LoadedModule>> = RefCell::new(HashMap::new());
    let context_for_cmds = context.clone();
    // Link mutations run here too; capture what they need (core to create,
    // registry to destroy, state for the per-channel idempotency check).
    let core_for_cmds = core.clone();
    let registry_for_cmds = registry.clone();
    let state_for_cmds = state.clone();
    let xruns_for_cmds = xruns.clone();
    let _cmd_receiver = cmd_rx.attach(mainloop.loop_(), move |cmd| match cmd {
        PwCommand::Load { node_name, module_name, args, reply } => {
            let mut modules = modules.borrow_mut();
            if modules.contains_key(&node_name) {
                let _ = reply.send(Err(format!("a module for '{node_name}' is already loaded")));
                return;
            }
            // SAFETY: we're on the PipeWire thread and context_for_cmds is the
            // live context this thread owns — exactly pw_control::module's contract.
            let result = unsafe { LoadedModule::load(context_for_cmds.as_raw_ptr(), &module_name, &args) };
            match result {
                Ok(module) => {
                    tracing::info!("loaded {module_name} for node '{node_name}'");
                    modules.insert(node_name, module);
                    let _ = reply.send(Ok(()));
                }
                Err(e) => {
                    tracing::warn!("failed to load {module_name} for '{node_name}': {e}");
                    let _ = reply.send(Err(e));
                }
            }
        }
        PwCommand::Unload { node_name, reply } => {
            // Drop (and thus destroy) happens here, on this thread. Idempotent.
            if modules.borrow_mut().remove(&node_name).is_some() {
                tracing::info!("unloaded module for node '{node_name}'");
            }
            let _ = reply.send(Ok(()));
        }
        PwCommand::CreateLinks { specs, reply } => {
            let _ = reply.send(create_links(&core_for_cmds, &state_for_cmds, &specs));
        }
        PwCommand::DestroyLinks { link_ids, reply } => {
            let _ = reply.send(destroy_links(&registry_for_cmds, &link_ids));
        }
        PwCommand::CreateSinkNode { node_name, reply } => {
            let _ = reply.send(create_sink_node(&core_for_cmds, &node_name));
        }
        PwCommand::DestroySinkNode { node_id, reply } => {
            let _ = registry_for_cmds.destroy_global(node_id);
            let _ = reply.send(Ok(()));
        }
        PwCommand::SetProfiling(enabled) => {
            if enabled {
                if profiler_listener.borrow().is_some() {
                    return; // already profiling
                }
                match *profiler_id.borrow() {
                    Some(id) => match crate::pw::profiler::subscribe(&registry_for_cmds, id, xruns_for_cmds.clone()) {
                        Some(listener) => {
                            *profiler_listener.borrow_mut() = Some(listener);
                            tracing::debug!("profiling enabled (bound profiler global {id})");
                        }
                        None => tracing::warn!("failed to bind profiler global {id}"),
                    },
                    None => tracing::debug!("SetProfiling(true) but no profiler global present (module-profiler not loaded)"),
                }
            } else if profiler_listener.borrow_mut().take().is_some() {
                xruns_for_cmds.lock_recover().clear();
                tracing::debug!("profiling disabled");
            }
        }
    });

    tracing::info!("connected to PipeWire, watching the registry");
    // Blocks dispatching registry/core/command events for the rest of the
    // process's lifetime; this IS the intended steady-state, not a one-shot
    // roundtrip. `_cmd_receiver` and `_listener` must outlive this call.
    mainloop.run();
    Ok(())
}

/// Creates each link natively via the `link-factory`. Runs on the PipeWire
/// thread. A `create_object` request is enqueued and flushed on the next loop
/// iteration; the resulting link then shows up through the `global` listener,
/// which fires the change notifier that updates any open UI.
fn create_links(core: &pw::core::CoreRc, state: &SharedState, specs: &[LinkSpec]) -> Result<String, String> {
    let mut messages = Vec::with_capacity(specs.len());
    let mut ok = true;
    for spec in specs {
        // Idempotency without stderr string-matching: if a link between these
        // exact ports is already in the observed registry, don't recreate it.
        let already_linked = {
            let st = state.lock_recover();
            st.links.values().any(|l| l.output_port == spec.out_port && l.input_port == spec.in_port)
        };
        if already_linked {
            messages.push(format!("ports {} -> {} already linked", spec.out_port, spec.in_port));
            continue;
        }

        let mut props = PropertiesBox::new();
        props.insert("link.output.node", spec.out_node.to_string());
        props.insert("link.output.port", spec.out_port.to_string());
        props.insert("link.input.node", spec.in_node.to_string());
        props.insert("link.input.port", spec.in_port.to_string());
        // Linger so the link survives us dropping the proxy (and a daemon
        // restart), matching the persistence `pw-link` gave.
        props.insert("object.linger", "1");

        match core.create_object::<Link>("link-factory", &props) {
            Ok(link) => {
                // The remote object persists (linger); we don't hold the proxy
                // — destroy goes by registry id via `destroy_links`.
                drop(link);
                messages.push(format!("linked port {} -> {}", spec.out_port, spec.in_port));
            }
            Err(e) => {
                ok = false;
                messages.push(format!("failed to link port {} -> {}: {e}", spec.out_port, spec.in_port));
            }
        }
    }
    let summary = messages.join("; ");
    if ok {
        Ok(summary)
    } else {
        Err(summary)
    }
}

/// Destroys links by registry global id. Runs on the PipeWire thread. A gone
/// link returns an error from `destroy_global`; it's ignored, because the
/// desired end state — "not linked" — holds regardless.
fn destroy_links(registry: &pw::registry::RegistryRc, link_ids: &[u32]) -> Result<String, String> {
    let mut messages = Vec::with_capacity(link_ids.len());
    for &id in link_ids {
        let _ = registry.destroy_global(id);
        messages.push(format!("removed link {id}"));
    }
    Ok(messages.join("; "))
}

/// Creates a `support.null-audio-sink` adapter node — the native equivalent
/// of `pw-cli create-node adapter "{ factory.name=support.null-audio-sink
/// node.name=<name> media.class=Audio/Sink object.linger=true
/// audio.position=[FL,FR] }"` (outputs/sendspin/server.rs's capture target). Runs on
/// the PipeWire thread. Like `create_links`, the proxy is dropped
/// immediately — the new node shows up through the `global` listener same as
/// anything else, and destruction always goes through `destroy_global` by
/// registry id regardless (a dropped proxy alone doesn't destroy the remote
/// object), so there's nothing gained by holding it here.
fn create_sink_node(core: &pw::core::CoreRc, node_name: &str) -> Result<(), String> {
    let mut props = pw::properties::PropertiesBox::new();
    props.insert("factory.name", "support.null-audio-sink");
    props.insert("node.name", node_name);
    props.insert("media.class", "Audio/Sink");
    // Survives this creating connection going away (it won't, while the
    // daemon runs, but matches RAOP links' own reasoning: resilient to a
    // daemon restart without other things that reference this node breaking
    // transiently in between).
    props.insert("object.linger", "true");
    props.insert("audio.position", "[FL,FR]");
    core.create_object::<pw::node::Node>("adapter", &props).map(drop).map_err(|e| format!("failed to create sink node '{node_name}': {e}"))
}
