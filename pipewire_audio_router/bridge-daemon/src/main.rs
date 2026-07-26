mod airplay_clients;
mod airplay_source;
mod announce;
mod announce_arbiter;
mod ap2_discovery;
mod ap2_liveness;
mod ap2_ptp;
mod ap2_server;
mod ap2_spike;
mod ap2_volume;
mod api;
mod calibrate;
mod config;
mod decode;
mod discovery_supervisor;
mod groups_store;
mod host_assessment;
mod locks;
mod metering;
mod overlay_mixer;
mod per_device_spike;
mod player;
mod pw_module;
mod pw_thread;
mod resample;
mod routing;
mod routing_store;
mod rtp_source;
mod sendspin_capture;
mod sendspin_discovery;
mod sendspin_liveness;
mod sendspin_server;
mod sendspin_volume;
mod raop_migration;
mod settings_store;
mod sources_store;
mod sync_group;
mod sync_settings;
mod volume;
mod wav;
mod wyoming;

use crate::locks::LockRecover;
use clap::{Parser, Subcommand};
use std::path::{Path, PathBuf};

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
    /// instance, bring up the stored AirPlay and RTP sources, and serve the REST
    /// API — all of which can be reconfigured live (api.rs's
    /// `/api/source/airplay`, `/api/source/rtp`). Sendspin devices and AirPlay-2
    /// receivers are discovered and grouped dynamically, so there's nothing
    /// stored for them.
    Serve {
        /// Persistent, runtime-managed store for the AirPlay and RTP sources.
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
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "bridge_daemon=info,shairplay=info,airplay_client=info,airplay_audio=info,libairptp=info".into()),
        )
        .init();

    let cli = Cli::parse();
    match cli.command {
        Command::Serve { sources, routing, static_dir, listen } => serve(&sources, &routing, &static_dir, &listen),
    }
}

/// Reads `BRIDGE_DISCOVERY` for the fresh-install seed of the persisted
/// discovery flag: unset or `on`/`1`/`true`/`yes` → enabled, `off`/`0`/`false`/
/// `no` → disabled. Anything else → enabled (with a warning).
fn discovery_enabled_env() -> bool {
    match std::env::var("BRIDGE_DISCOVERY").ok().as_deref().map(str::trim) {
        Some("off") | Some("0") | Some("false") | Some("no") => false,
        None | Some("") | Some("on") | Some("1") | Some("true") | Some("yes") => true,
        Some(other) => {
            tracing::warn!("unrecognized BRIDGE_DISCOVERY='{other}'; defaulting to enabled");
            true
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

fn serve(sources_path: &Path, routing_path: &Path, static_dir: &Path, listen: &str) -> anyhow::Result<()> {
    let started = std::time::Instant::now();
    let version = addon_version();
    tracing::info!("PipeWire Audio Router add-on v{version}");

    // One-time migration: drop-raop 2026-07. Rewrites any persisted
    // `raop-out-<slug>` routing/group reference to `ap2-dev-<slug>` before the
    // reconcilers read them, so a link saved under the old RAOP output path
    // points at the AP2 output that replaced it (same slug). Idempotent (a boot
    // that finds no `raop-out-*` does nothing), so it can be removed after the
    // deployment has booted once. Stale per-output RAOP latency / settings keys
    // are dropped automatically by serde (the fields no longer exist).
    let groups_config_path = routing_path.with_file_name("groups.json");
    raop_migration::migrate_raop_prefixes(routing_path, &groups_config_path);

    let sources = sources_store::SourcesStore::load(sources_path)?;
    tracing::info!(
        "sources: airplay={:?}, rtp={:?} in {}",
        sources.airplay_source_name(),
        sources.rtp_source().map(|c| c.port),
        sources_path.display()
    );
    // Live anti-takeover flag, seeded from the store; the API toggles it without
    // restarting the receiver (airplay_source's `authorize_session` reads it).
    let airplay_prevent_takeover: airplay_source::SharedPreventTakeover =
        std::sync::Arc::new(std::sync::atomic::AtomicBool::new(sources.airplay_prevent_takeover()));
    let sources = std::sync::Arc::new(std::sync::Mutex::new(sources));

    let routing = routing_store::RoutingStore::load(routing_path)?;
    tracing::info!("{} persisted routing link(s) in {}", routing.links().count(), routing_path.display());
    let routing: routing_store::SharedRouting = std::sync::Arc::new(std::sync::Mutex::new(routing));

    // Sync/latency tuning (group lead + per-device static delays), kept next to
    // routing.json in /data. Derived from the routing path so there's no extra
    // CLI flag to thread through.
    let sync_settings_path = routing_path.with_file_name("sync-settings.json");
    let sync_settings = sync_settings::SyncSettings::load(&sync_settings_path)?;
    tracing::info!(
        "sync settings: group lead {} ms, {} device delay(s) in {}",
        sync_settings.group_lead_ms(),
        sync_settings.sendspin_delays().len(),
        sync_settings_path.display()
    );
    let sync_settings: sync_settings::SharedSyncSettings = std::sync::Arc::new(std::sync::Mutex::new(sync_settings));

    // General app settings (announce duck default, discovery on/off), beside the
    // other /data stores. On a fresh install the discovery flag is seeded from
    // BRIDGE_DISCOVERY so an env-off is honored on first boot; after that the
    // persisted value is authoritative.
    let settings_path = routing_path.with_file_name("settings.json");
    let settings_fresh = !settings_path.exists();
    let mut settings = settings_store::SettingsStore::load(&settings_path)?;
    if settings_fresh {
        settings.set_discovery_enabled(discovery_enabled_env())?;
    }
    tracing::info!(
        "settings: duck default {:.2}, discovery {} in {}",
        settings.default_duck(),
        if settings.discovery_enabled() { "on" } else { "off" },
        settings_path.display()
    );
    let settings: settings_store::SharedSettings = std::sync::Arc::new(std::sync::Mutex::new(settings));

    // Named music/announcement groups (groups_store.rs), beside the other /data
    // stores. The MG/AG data model behind the two-tier grouping design.
    let groups_config = groups_store::GroupsStore::load(&groups_config_path)?;
    tracing::info!(
        "{} music group(s), {} announcement group(s) in {}",
        groups_config.music().len(),
        groups_config.announcement().len(),
        groups_config_path.display()
    );
    let groups_config: groups_store::SharedGroupsStore = std::sync::Arc::new(std::sync::Mutex::new(groups_config));

    let airplay: api::SharedAirplay = std::sync::Arc::new(tokio::sync::Mutex::new(None));

    // Remembered AirPlay senders (Sources-tab connection list). Lives beside
    // sources.json in /data; loads with everyone marked disconnected.
    let airplay_clients_path = sources_path.with_file_name("airplay_clients.json");
    let airplay_clients = airplay_clients::AirplayClientRegistry::load(&airplay_clients_path)?;
    tracing::info!("{} remembered AirPlay client(s) in {}", airplay_clients.list().len(), airplay_clients_path.display());
    let airplay_clients = airplay_clients.shared();
    let meters = metering::MeterHub::new();
    let sendspin_control = sendspin_volume::shared();
    let ap2_control = ap2_volume::shared();
    let sendspin_devices: sendspin_discovery::SharedSendspinDevices =
        std::sync::Arc::new(std::sync::Mutex::new(std::collections::BTreeMap::new()));
    // Discovered AirPlay-2 receivers (ap2_discovery.rs) + the host-global PTP
    // grandmaster (ap2_ptp.rs) they register with. The RAOP-output replacement.
    let ap2_devices: ap2_discovery::SharedAp2Devices =
        std::sync::Arc::new(std::sync::Mutex::new(std::collections::BTreeMap::new()));
    let ap2_ptp = ap2_ptp::Ap2PtpService::new();

    let (pw_state, changes, pw_cmd) = pw_thread::spawn()?;

    // mDNS auto-discovery (sendspin devices + AirPlay-2 receivers), runtime
    // toggleable from the Settings page (discovery_supervisor.rs). Discovery only
    // populates the shared registries — the grouping reconciler (sync_group.rs)
    // builds the audio path from the routing intent. Whether discovery runs comes
    // from the persisted settings flag. The supervisor is held for the process
    // lifetime (serve never returns in practice) so the daemons stay alive.
    let discovery = discovery_supervisor::DiscoverySupervisor::new(
        sendspin_devices.clone(),
        ap2_devices.clone(),
        ap2_ptp.clone(),
        changes.clone(),
    );
    if settings.lock_recover().discovery_enabled() {
        if let Err(e) = discovery.start() {
            tracing::warn!("mDNS discovery unavailable ({e}); continuing without it");
        }
    } else {
        tracing::info!("mDNS discovery disabled (settings)");
    }

    // Modestly bound the tokio worker pool: this daemon does async I/O (HTTP/WS,
    // discovery, control channels) but NO time-critical audio work — that lives on
    // dedicated SCHED_FIFO threads (PipeWire data-loop, ap2/sendspin relays, the
    // AP2 rt-sender, libairptp). `Runtime::new()` sizes workers to the logical-core
    // count; 4 is ample for the I/O load and trims a couple of idle threads.
    //
    // Do NOT cap `max_blocking_threads`: tokio's blocking pool serves `tokio::fs`,
    // and `ServeDir` (the whole web UI) reads files through it. A low cap
    // (previously 4) throttled static-file serving — under a hard-reload storm the
    // UI failed to load. The pool is on-demand and idles out, so leaving it at the
    // default is not a persistent-thread cost.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()?;
    rt.block_on(async {
        // Start every stored source/adapter process (AirPlay source via
        // Supervisor) and the RTP source. A failed spawn is logged, not fatal
        // — the rest still start and it can be re-enabled live.
        spawn_stored_sources(&sources, &airplay, &airplay_clients, &airplay_prevent_takeover, pw_cmd.clone()).await;

        // Own sendspin device online/offline (and eventual removal) from the
        // live connection state + an active TCP probe — mDNS only ever adds.
        // Without this, an mDNS TTL flap would tear down live groups.
        sendspin_liveness::spawn(sendspin_devices.clone(), sendspin_control.clone(), changes.clone());
        // Same contract for AP2 receivers: mDNS (ap2_discovery) only adds; this
        // task TCP-probes each and demotes/removes a powered-off receiver so its
        // sender is torn down (and its PTP peer released).
        ap2_liveness::spawn(ap2_devices.clone(), ap2_ptp.clone(), changes.clone());

        // Let device-reported sendspin volume changes (sendspin_server.rs) nudge
        // the routing-matrix WebSocket, so the UI slider syncs live to a physical
        // volume change without polling. AP2 volume/mute is UI-driven (no receiver
        // feedback yet) but uses the same notifier so a set pushes immediately.
        sendspin_control.lock().await.set_change_notifier(changes.clone());
        ap2_control.lock().await.set_change_notifier(changes.clone());

        // Seed persisted per-device static delays so they re-apply when each
        // device (re)connects, exactly like stored volumes (sendspin_volume.rs).
        {
            let delays = sync_settings.lock_recover().sendspin_delays();
            if !delays.is_empty() {
                sendspin_control.lock().await.seed_delays(delays.into_iter().collect());
            }
        }

        // Reconcile persisted routing intent onto the live graph, now and on
        // every registry/device change: a node that (re)appears — a reloaded
        // raop-sink, a rediscovered device — gets its saved links recreated
        // (routing::reconcile, additive only), and sendspin devices sharing a
        // source are (re)formed into synchronized groups (sync_group).
        //
        // The reconciler is shared (SharedGroups) so the alignment API
        // (calibrate.rs) can read the live group layout — anchor + members — to
        // drive the latency-alignment wizard.
        let groups: sync_group::SharedGroups = std::sync::Arc::new(tokio::sync::Mutex::new(sync_group::GroupReconciler::new()));

        // Latency-alignment session manager (calibrate.rs): reads the live group
        // layout, plays a click into a group's anchor, and mutes non-audible
        // members while the user tunes offsets by ear.
        let align = calibrate::AlignManager::new(sendspin_control.clone(), ap2_control.clone(), groups.clone());
        {
            let pw = pw_state.clone();
            let cmd = pw_cmd.clone();
            let routing = routing.clone();
            let devices = sendspin_devices.clone();
            let control = sendspin_control.clone();
            let ap2_devices = ap2_devices.clone();
            let ap2_ptp = ap2_ptp.clone();
            let ap2_control = ap2_control.clone();
            let settings = sync_settings.clone();
            let groups = groups.clone();
            let mut rx = changes.subscribe();
            tokio::spawn(async move {
                // routing::reconcile first (direct links for any real-node output),
                // then sync_group (which owns the sendspin + AP2 groups) — the group
                // lead comes from the live sync settings so a change takes effect on
                // the next tick.
                let lead = sync_settings::group_lead_us(&settings);
                routing::reconcile(&pw, &cmd, &routing).await;
                groups.lock().await.reconcile(&pw, &cmd, &routing, &devices, &control, lead, &ap2_devices, &ap2_ptp, &settings, &ap2_control).await;
                // Loops until the change channel closes (RecvError::Closed ends the
                // `while let`); a Lagged wakeup reconciles just like a normal one.
                use tokio::sync::broadcast::error::RecvError;
                // Coalescing window: after a change wakes us, wait for a brief quiet
                // period (draining any further changes) before reconciling, so a
                // burst of routing/liveness changes produces ONE reconcile instead of
                // back-to-back teardown+reconnect — the latter trips an AP2 receiver's
                // transient "Pairing error M2" when a new session is opened before the
                // prior one is released. 400 ms is imperceptible for routing yet
                // absorbs a rapid click-storm or liveness flap.
                const RECONCILE_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(400);
                while let Ok(()) | Err(RecvError::Lagged(_)) = rx.recv().await {
                    // Drain the burst: keep resetting the quiet timer while changes
                    // keep arriving; reconcile once it goes quiet. Exit if closed.
                    loop {
                        match tokio::time::timeout(RECONCILE_DEBOUNCE, rx.recv()).await {
                            Ok(Ok(())) | Ok(Err(RecvError::Lagged(_))) => continue,
                            Ok(Err(RecvError::Closed)) => return,
                            Err(_elapsed) => break, // quiet window elapsed
                        }
                    }
                    let lead = sync_settings::group_lead_us(&settings);
                    routing::reconcile(&pw, &cmd, &routing).await;
                    groups.lock().await.reconcile(&pw, &cmd, &routing, &devices, &control, lead, &ap2_devices, &ap2_ptp, &settings, &ap2_control).await;
                }
            });
        }

        // Announce coordinator tick: complete finished per-device overlays (start
        // the next queued clip / end the duck) and expire stale queued
        // announcements. Cheap no-op when nothing is announcing.
        tokio::spawn(async move {
            let coordinator = announce::AnnounceCoordinator::global();
            let mut ticker = tokio::time::interval(std::time::Duration::from_millis(150));
            loop {
                ticker.tick().await;
                coordinator.poll();
            }
        });

        let app = api::router(
            pw_state,
            changes,
            pw_cmd,
            sources,
            airplay.clone(),
            airplay_clients,
            airplay_prevent_takeover,
            meters,
            sendspin_devices,
            ap2_devices,
            ap2_ptp.clone(),
            routing,
            sendspin_control,
            ap2_control,
            sync_settings,
            settings,
            discovery,
            align,
            groups,
            groups_config,
            version,
            started,
            static_dir.to_path_buf(),
        );
        let listener = tokio::net::TcpListener::bind(listen).await?;
        tracing::info!("listening on {listen}");
        axum::serve(listener, app).with_graceful_shutdown(shutdown_signal()).await?;

        // On SIGTERM/SIGINT (run.sh's trap / container stop), stop the AirPlay
        // receiver cleanly (unregister its mDNS, close listeners). Sendspin
        // group servers and the RTP module tear down with the process.
        tracing::info!("shutting down; stopping AirPlay source");
        if let Some(handle) = airplay.lock().await.take() {
            handle.stop().await;
        }
        Ok::<(), anyhow::Error>(())
    })
}

/// Starts the persisted sources: the native AirPlay receiver (airplay_source.rs)
/// and the RTP source (a PipeWire module). (Sendspin devices aren't started
/// here — they're auto-discovered and grouped from the routing intent; see
/// sync_group.rs.)
async fn spawn_stored_sources(
    sources: &api::SharedSources,
    airplay: &api::SharedAirplay,
    airplay_clients: &airplay_clients::SharedAirplayClients,
    airplay_prevent_takeover: &airplay_source::SharedPreventTakeover,
    pw_cmd: pw_thread::PwCommandSender,
) {
    // Snapshot the config and drop the (std) lock before awaiting anything.
    let (airplay_name, airplay_latency, airplay_auth_setup, rtp) = {
        let s = sources.lock_recover();
        (s.airplay_source_name().map(str::to_string), s.airplay_latency_msec(), s.airplay_auth_setup(), s.rtp_source())
    };

    // The RTP source (bt-bridge) is a native PipeWire module, not a subprocess
    // — load it via the PipeWire thread like a RAOP sink, not the supervisor. A
    // failed load is logged, not fatal; it can be re-enabled live via the API.
    if let Some(rtp) = rtp {
        let args = rtp_source::rtp_source_module_args(rtp.port, rtp.latency_msec, &rtp.source_addr, rtp.ignore_ssrc);
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
                Ok(Ok(())) => {
                    tracing::info!("started RTP source on port {} ({} ms jitter buffer)", rtp.port, rtp.latency_msec)
                }
                Ok(Err(e)) => tracing::warn!("failed to start RTP source: {e}"),
                Err(_) => tracing::warn!("no reply loading RTP source"),
            }
        }
    }

    if let Some(name) = airplay_name {
        match airplay_source::start(name.clone(), airplay_latency, airplay_auth_setup, airplay_clients.clone(), airplay_prevent_takeover.clone())
            .await
        {
            Ok(handle) => {
                *airplay.lock().await = Some(handle);
                tracing::info!("started AirPlay source '{name}'");
            }
            Err(e) => tracing::warn!("failed to start AirPlay source '{name}': {e}"),
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
