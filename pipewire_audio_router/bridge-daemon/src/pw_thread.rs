//! Owns the PipeWire connection on a dedicated OS thread.
//!
//! `pipewire-rs`'s core types (`MainLoopRc`/`ContextRc`/`CoreRc`) are
//! `Rc`-based, not `Send` — they cannot cross threads. So this thread runs
//! its own blocking main loop for the process's entire lifetime, and
//! publishes plain, thread-safe snapshots of the registry state into a
//! shared `Arc<Mutex<..>>` that the axum side (running on tokio's
//! multi-threaded runtime) reads from.
//!
//! Deliberate scope for this phase: this thread only *observes* the
//! registry (nodes and ports) — it doesn't create/destroy links itself.
//! `pipewire-rs` can do that too (proven in spike 4,
//! spikes/04-graph-control.md — 0.07ms per link via a persistent
//! connection), but wiring a fully thread-safe command channel into a
//! `pw_loop`'s event source correctly (lifetime/`Send` constraints on
//! `EventSource`) is real additional work not justified yet. Mutations go
//! through `pw-link` subprocess calls instead (api.rs) — spike 4 already
//! measured that at ~16ms, "fine for a human clicking a button." Revisit
//! if link-creation frequency ever stops being human-paced.

use pipewire as pw;
use serde::Serialize;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

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
}

#[derive(Debug, Default)]
pub struct RegistryState {
    pub nodes: BTreeMap<u32, NodeInfo>,
    pub ports: BTreeMap<u32, PortInfo>,
    pub links: BTreeMap<u32, LinkInfo>,
}

impl RegistryState {
    /// Whether any link currently feeds into this node — the basis for a
    /// media_player entity's playing/idle state (Section 6/9): an output
    /// with an active incoming link is "playing," one with none is "idle."
    pub fn node_has_incoming_link(&self, node_id: u32) -> bool {
        self.links.values().any(|l| l.input_node == node_id)
    }
}

pub type SharedState = Arc<Mutex<RegistryState>>;

/// Fires (empty payload, just a ping) whenever the registry state changes —
/// node/port/link added or removed. The routing UI's WebSocket (Section 8,
/// routing.rs) subscribes to this to push live matrix updates instead of
/// the client having to poll. `send` is synchronous and callable from any
/// thread (including this module's own dedicated PipeWire OS thread, which
/// isn't running inside a tokio runtime) — no async plumbing needed to
/// notify from here.
pub type ChangeNotifier = tokio::sync::broadcast::Sender<()>;

/// Spawns the PipeWire thread and returns the shared state handle and its
/// change notifier immediately; the thread connects and starts populating
/// the state in the background.
pub fn spawn() -> anyhow::Result<(SharedState, ChangeNotifier)> {
    let state: SharedState = Arc::new(Mutex::new(RegistryState::default()));
    // Capacity is a lag buffer, not a queue depth requirement: subscribers
    // only ever care about "something changed, re-fetch the snapshot," so
    // a slow subscriber missing a few pings and catching up on the next one
    // is fine — this is not an event log.
    let (changes, _) = tokio::sync::broadcast::channel(16);
    let state_for_thread = state.clone();
    let changes_for_thread = changes.clone();
    std::thread::Builder::new()
        .name("pipewire".into())
        .spawn(move || {
            if let Err(e) = run(state_for_thread, changes_for_thread) {
                tracing::error!("pipewire thread exited with error: {e:#}");
            }
        })?;
    Ok((state, changes))
}

fn run(state: SharedState, changes: ChangeNotifier) -> anyhow::Result<()> {
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None)?;
    let context = pw::context::ContextRc::new(&mainloop, None)?;
    let core = context.connect_rc(None)?;
    let registry = core.get_registry()?;

    let state_add = state.clone();
    let state_remove = state.clone();
    let changes_add = changes.clone();
    let changes_remove = changes.clone();
    let _listener = registry
        .add_listener_local()
        .global(move |global| {
            let Some(props) = global.props else { return };

            if let Some(name) = props.get("node.name") {
                let info = NodeInfo {
                    node_id: global.id,
                    node_name: name.to_string(),
                    media_class: props.get("media.class").map(|s| s.to_string()),
                };
                tracing::info!("discovered node: {name} (id={})", global.id);
                state_add.lock().unwrap().nodes.insert(global.id, info);
                // No receivers (e.g. no UI open) is a normal, not an error,
                // case for a broadcast channel — hence the discarded result.
                let _ = changes_add.send(());
                return;
            }

            if let (Some(port_name), Some(node_id_str), Some(direction)) = (
                props.get("port.name"),
                props.get("node.id"),
                props.get("port.direction"),
            ) {
                if let Ok(node_id) = node_id_str.parse::<u32>() {
                    let info = PortInfo {
                        port_id: global.id,
                        node_id,
                        port_name: port_name.to_string(),
                        direction: direction.to_string(),
                    };
                    state_add.lock().unwrap().ports.insert(global.id, info);
                    let _ = changes_add.send(());
                    return;
                }
            }

            if let (Some(out_node_str), Some(in_node_str)) =
                (props.get("link.output.node"), props.get("link.input.node"))
            {
                if let (Ok(output_node), Ok(input_node)) =
                    (out_node_str.parse::<u32>(), in_node_str.parse::<u32>())
                {
                    let info = LinkInfo {
                        link_id: global.id,
                        output_node,
                        input_node,
                    };
                    tracing::info!("discovered link: node {output_node} -> node {input_node} (id={})", global.id);
                    state_add.lock().unwrap().links.insert(global.id, info);
                    let _ = changes_add.send(());
                }
            }
        })
        .global_remove(move |id| {
            let mut state = state_remove.lock().unwrap();
            if let Some(removed) = state.nodes.remove(&id) {
                tracing::info!("node removed: {} (id={id})", removed.node_name);
            }
            state.ports.remove(&id);
            state.links.remove(&id);
            drop(state);
            let _ = changes_remove.send(());
        })
        .register();

    tracing::info!("connected to PipeWire, watching the registry");
    // Blocks dispatching registry/core events for the rest of the process's
    // lifetime; this IS the intended steady-state, not a one-shot roundtrip.
    mainloop.run();
    Ok(())
}
