mod api;
mod config;
mod decode;
mod discovery;
mod locks;
mod outputs_store;
mod player;
mod pw_module;
mod pw_thread;
mod raop;
mod routing;
mod routing_store;
mod rtp_source;
mod sendspin_capture;
mod sendspin_discovery;
mod sendspin_server;
mod sources_store;
mod supervisor;
mod volume;
mod wav;
mod wyoming;

use crate::locks::LockRecover;
use clap::{Parser, Subcommand};
use std::path::{Path, PathBuf};

const DEFAULT_STORE_PATH: &str = "/data/raop-outputs.json";
const DEFAULT_SOURCES_PATH: &str = "/data/sources.json";
const DEFAULT_ROUTING_PATH: &str = "/data/routing.json";
// Not 8080: with host_network the daemon binds a real host port, and 8080 is a
// very common add-on port (collisions are likely). 8099 is distinctive and
// rarely used; override with --listen (and config.yaml ingress_port) if needed.
const DEFAULT_HTTP_ADDR: &str = "0.0.0.0:8099";
const DEFAULT_STATIC_DIR: &str = "/app/www";

#[derive(Parser)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the long-lived daemon: connect to the already-running PipeWire
    /// instance, load a `raop-sink` module for every stored RAOP output, start
    /// every stored source/adapter process, and serve the REST API — all of
    /// which can be reconfigured live (api.rs's `/api/outputs`,
    /// `/api/source/airplay`, `/api/sendspin_outputs`).
    Serve {
        /// Persistent, runtime-managed RAOP output store.
        #[arg(long, default_value = DEFAULT_STORE_PATH)]
        store: PathBuf,
        /// Persistent, runtime-managed store for the supervised source/adapter
        /// processes (AirPlay source, sendspin outputs).
        #[arg(long, default_value = DEFAULT_SOURCES_PATH)]
        sources: PathBuf,
        /// Persistent routing intent (links by stable node name), reconciled
        /// onto the live graph — survives node reloads and device churn.
        #[arg(long, default_value = DEFAULT_ROUTING_PATH)]
        routing: PathBuf,
        /// Directory of the built web UI (frontend/dist) served as static files.
        #[arg(long, default_value = DEFAULT_STATIC_DIR)]
        static_dir: PathBuf,
        #[arg(long, default_value = DEFAULT_HTTP_ADDR)]
        listen: String,
    },
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "bridge_daemon=info".into()),
        )
        .init();

    let cli = Cli::parse();
    match cli.command {
        Command::Serve { store, sources, routing, static_dir, listen } => {
            serve(&store, &sources, &routing, &static_dir, &listen)
        }
    }
}

/// Reads `BRIDGE_DISCOVERY`: unset or `on`/`1`/`true` → load discovered
/// receivers, `log` → dry-run (report only), `off`/`0`/`false`/`no` → disabled
/// (returns `None`).
fn discovery_mode() -> Option<discovery::Mode> {
    match std::env::var("BRIDGE_DISCOVERY").ok().as_deref().map(str::trim) {
        None | Some("") | Some("on") | Some("1") | Some("true") | Some("yes") => Some(discovery::Mode::Load),
        Some("log") | Some("dry-run") | Some("dryrun") => Some(discovery::Mode::DryRun),
        Some("off") | Some("0") | Some("false") | Some("no") => None,
        Some(other) => {
            tracing::warn!("unrecognized BRIDGE_DISCOVERY='{other}'; defaulting to enabled");
            Some(discovery::Mode::Load)
        }
    }
}

/// The Home Assistant add-on version, baked into the image at build time as the
/// `ADDON_VERSION` env (from the `--build-arg` in scripts/deploy-dev.sh and the
/// CI workflow, i.e. config.yaml's `version` / the dev tag). Read at runtime,
/// not compile time, so bumping the version doesn't force a Rust rebuild. Falls
/// back to the crate version when built/run outside the add-on image.
fn addon_version() -> String {
    match std::env::var("ADDON_VERSION") {
        Ok(v) if !v.trim().is_empty() && v != "dev" => v,
        _ => env!("CARGO_PKG_VERSION").to_string(),
    }
}

fn serve(store_path: &Path, sources_path: &Path, routing_path: &Path, static_dir: &Path, listen: &str) -> anyhow::Result<()> {
    tracing::info!("PipeWire Audio Router add-on v{}", addon_version());

    // No options.json seeding: both stores start empty on a fresh install and
    // are populated entirely at runtime via the API (and mDNS discovery).
    let store = outputs_store::OutputsStore::load(store_path)?;
    tracing::info!("{} RAOP output(s) in store {}", store.list().len(), store_path.display());
    let store = std::sync::Arc::new(std::sync::Mutex::new(store));

    let sources = sources_store::SourcesStore::load(sources_path)?;
    tracing::info!(
        "sources: airplay={:?}, rtp={:?}, {} sendspin output(s) in {}",
        sources.airplay_source_name(),
        sources.rtp_source().map(|c| c.port),
        sources.sendspin_outputs().len(),
        sources_path.display()
    );
    let sources = std::sync::Arc::new(std::sync::Mutex::new(sources));

    let routing = routing_store::RoutingStore::load(routing_path)?;
    tracing::info!("{} persisted routing link(s) in {}", routing.links().count(), routing_path.display());
    let routing: routing_store::SharedRouting = std::sync::Arc::new(std::sync::Mutex::new(routing));

    let supervisor = std::sync::Arc::new(tokio::sync::Mutex::new(supervisor::Supervisor::new()));
    let sendspin_servers: api::SharedSendspinServers =
        std::sync::Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new()));
    let sendspin_devices: sendspin_discovery::SharedSendspinDevices =
        std::sync::Arc::new(std::sync::Mutex::new(std::collections::BTreeMap::new()));

    let (pw_state, changes, pw_cmd) = pw_thread::spawn()?;

    // mDNS auto-discovery of RAOP receivers (overrides-only: store-managed
    // outputs win, everything else is picked up live). `BRIDGE_DISCOVERY`
    // controls it: unset/`on` loads discovered receivers, `log` only reports
    // what it finds (safe dry-run), `off` disables it. The daemon handle must
    // stay alive for discovery to keep running, so it's held for the whole
    // process via `_discovery` (serve never returns in practice).
    let _discovery = match discovery_mode() {
        Some(mode) => match discovery::spawn(pw_cmd.clone(), store.clone(), sources.clone(), mode) {
            Ok(daemon) => {
                tracing::info!("mDNS RAOP discovery started ({mode:?})");
                Some(daemon)
            }
            Err(e) => {
                tracing::warn!("mDNS discovery unavailable ({e}); continuing without it");
                None
            }
        },
        None => {
            tracing::info!("mDNS RAOP discovery disabled (BRIDGE_DISCOVERY=off)");
            None
        }
    };

    // mDNS auto-discovery of sendspin devices (same BRIDGE_DISCOVERY gate).
    // Only populates the shared registry — no per-device sink is loaded here;
    // the grouping reconciler (sendspin_group.rs) builds the audio path from
    // the routing intent. Handle held for the process lifetime.
    let _sendspin_discovery = if discovery_mode().is_some() {
        match sendspin_discovery::spawn(sendspin_devices.clone()) {
            Ok(daemon) => {
                tracing::info!("mDNS sendspin device discovery started");
                Some(daemon)
            }
            Err(e) => {
                tracing::warn!("sendspin discovery unavailable ({e}); continuing without it");
                None
            }
        }
    } else {
        None
    };

    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async {
        // Load a module for every stored RAOP output before serving. The
        // PipeWire thread processes these once its loop is running (and the
        // context is connected), so awaiting each reply here is safe.
        let outputs: Vec<config::RaopOutputConfig> = store.lock_recover().list().to_vec();
        for output in outputs {
            let node_name = raop::raop_node_name(&output.name);
            let args = raop::raop_module_args(&output);
            let (tx, rx) = tokio::sync::oneshot::channel();
            let sent = pw_cmd.send(pw_thread::PwCommand::Load {
                node_name: node_name.clone(),
                module_name: raop::RAOP_MODULE_NAME.to_string(),
                args,
                reply: tx,
            });
            if sent.is_err() {
                tracing::error!("PipeWire thread unavailable while loading '{node_name}'");
                continue;
            }
            match rx.await {
                Ok(Ok(())) => tracing::info!("loaded RAOP output '{node_name}'"),
                // A bad device must not abort startup — the others still load,
                // and it can be fixed/re-added live via the API.
                Ok(Err(e)) => tracing::warn!("skipping RAOP output '{node_name}': {e}"),
                Err(_) => tracing::warn!("no reply loading RAOP output '{node_name}'"),
            }
        }

        // Start every stored source/adapter process (AirPlay source via
        // Supervisor; sendspin outputs as native embedded servers). A failed
        // spawn is logged, not fatal — the rest still start and it can be
        // re-added live.
        spawn_stored_sources(&sources, &supervisor, &sendspin_servers, pw_state.clone(), pw_cmd.clone()).await;

        // Reconcile persisted routing intent onto the live graph, now and on
        // every registry change: a node that (re)appears — a reloaded
        // raop-sink, a rediscovered device — gets its saved links recreated.
        // Additive only: it never removes links, so a manual unlink (which
        // also drops the intent) stays gone. See routing_store.rs / routing.rs.
        {
            let pw = pw_state.clone();
            let cmd = pw_cmd.clone();
            let routing = routing.clone();
            let mut rx = changes.subscribe();
            tokio::spawn(async move {
                routing::reconcile(&pw, &cmd, &routing).await;
                loop {
                    match rx.recv().await {
                        Ok(()) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                            routing::reconcile(&pw, &cmd, &routing).await;
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }
            });
        }

        let app = api::router(
            pw_state,
            changes,
            pw_cmd,
            store,
            sources,
            supervisor.clone(),
            sendspin_servers.clone(),
            sendspin_devices,
            routing,
            static_dir.to_path_buf(),
        );
        let listener = tokio::net::TcpListener::bind(listen).await?;
        tracing::info!("listening on {listen}");
        axum::serve(listener, app).with_graceful_shutdown(shutdown_signal()).await?;

        // On SIGTERM/SIGINT (run.sh's trap / container stop), kill the child
        // processes and tear down the native sendspin servers we own before
        // exiting so nothing is orphaned.
        tracing::info!("shutting down; stopping supervised processes");
        supervisor.lock().await.stop_all().await;
        sendspin_servers.lock().await.clear();
        Ok::<(), anyhow::Error>(())
    })
}

/// Spawns the AirPlay source (via `Supervisor`) and every sendspin output
/// (as a native embedded server — see sendspin_server.rs) from the persisted
/// sources store.
async fn spawn_stored_sources(
    sources: &api::SharedSources,
    supervisor: &api::SharedSupervisor,
    sendspin_servers: &api::SharedSendspinServers,
    pw_state: pw_thread::SharedState,
    pw_cmd: pw_thread::PwCommandSender,
) {
    // Snapshot the config and drop the (std) lock before awaiting anything.
    let (airplay, sendspins, rtp) = {
        let s = sources.lock_recover();
        (s.airplay_source_name().map(str::to_string), s.sendspin_outputs().to_vec(), s.rtp_source())
    };

    // The RTP source (bt-bridge) is a native PipeWire module, not a subprocess
    // — load it via the PipeWire thread like a RAOP sink, not the supervisor. A
    // failed load is logged, not fatal; it can be re-enabled live via the API.
    if let Some(rtp) = rtp {
        let args = rtp_source::rtp_source_module_args(rtp.port);
        let (tx, rx) = tokio::sync::oneshot::channel();
        let sent = pw_cmd.send(pw_thread::PwCommand::Load {
            node_name: rtp_source::RTP_SOURCE_NODE_NAME.to_string(),
            module_name: rtp_source::RTP_SOURCE_MODULE_NAME.to_string(),
            args,
            reply: tx,
        });
        if sent.is_err() {
            tracing::error!("PipeWire thread unavailable while loading RTP source");
        } else {
            match rx.await {
                Ok(Ok(())) => tracing::info!("started RTP source on port {}", rtp.port),
                Ok(Err(e)) => tracing::warn!("failed to start RTP source: {e}"),
                Err(_) => tracing::warn!("no reply loading RTP source"),
            }
        }
    }

    if let Some(name) = airplay {
        match supervisor.lock().await.respawn(sources_store::AIRPLAY_KEY, &sources_store::airplay_spec(&name)).await {
            Ok(()) => tracing::info!("started AirPlay source '{name}'"),
            Err(e) => tracing::warn!("failed to start AirPlay source '{name}': {e}"),
        }
    }
    for output in sendspins {
        let node_name = output.node_name();
        match sendspin_server::start(&output, pw_state.clone(), pw_cmd.clone()).await {
            Ok(handle) => {
                sendspin_servers.lock().await.insert(node_name, handle);
                tracing::info!("started sendspin output '{}' (port {})", output.name, output.port);
            }
            Err(e) => tracing::warn!("failed to start sendspin output '{}': {e}", output.name),
        }
    }
}

/// Completes on SIGTERM or SIGINT — the trigger for axum's graceful shutdown.
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut sig) => {
                sig.recv().await;
            }
            Err(e) => tracing::warn!("could not install SIGTERM handler: {e}"),
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }
}
