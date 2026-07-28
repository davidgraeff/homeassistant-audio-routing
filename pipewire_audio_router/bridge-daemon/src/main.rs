mod airplay_clients;
mod airplay_source;
mod applemidi_sender;
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
mod pw_sink;
mod pw_sink_liveness;
mod pw_sink_spike;
mod pw_target_discovery;
mod pw_thread;
mod pwsink_server;
mod resample;
mod routing;
mod routing_store;
mod rtp_membership;
mod rtp_source;
mod sendspin_capture;
mod sendspin_codec;
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
    /// `/api/sources`). Sendspin devices and AirPlay-2
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
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| // `sendspin=info` matters more than it looks: the vendored server role logs its
            // dial loop (attempt, failure, retry backoff, goodbye reason) through the `log`
            // crate, and without its target enabled every one of those lines is dropped —
            // which left "why is this speaker not connected?" unanswerable from the log.
            "bridge_daemon=info,sendspin=info,shairplay=info,airplay_client=info,airplay_audio=info,libairptp=info".into()),
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
    {
        let list = sources.list();
        let airplay = list.iter().filter(|e| matches!(e.config, sources_store::SourceConfig::Airplay(_))).count();
        let rtp = list.iter().filter(|e| matches!(e.config, sources_store::SourceConfig::Rtp(_))).count();
        tracing::info!("sources: {} configured ({airplay} AirPlay, {rtp} RTP) in {}", list.len(), sources_path.display());
    }
    // Anti-takeover is now per-receiver: each running AirplayHandle owns its own
    // flag (seeded from that source's stored config by airplay_source::reconcile),
    // so there's no single process-wide flag here anymore.
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

    // Running AirPlay receivers, keyed by source id (one per configured AirPlay
    // source), reconciled against the store below and via the API.
    let airplay: api::SharedAirplay = std::sync::Arc::new(tokio::sync::Mutex::new(std::collections::BTreeMap::new()));

    // Remembered AirPlay senders (Sources-tab connection list), per-receiver.
    // One backing file beside sources.json in /data holds a client list per
    // source id; loads with everyone marked disconnected.
    let airplay_clients_path = sources_path.with_file_name("airplay_clients.json");
    let airplay_clients = airplay_clients::AirplayClientStore::load(&airplay_clients_path)?;
    tracing::info!("{} remembered AirPlay client(s) in {}", airplay_clients.total_clients(), airplay_clients_path.display());
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
    // Discovered pw-sink targets (pw_target_discovery.rs): remote PipeWire hosts
    // running module-rtp-session, surfaced as virtual routing outputs
    // (`pwsink-dev-*`) and driven by per-target AppleMIDI senders (pwsink_server.rs).
    let pw_targets: pw_target_discovery::SharedPwTargets =
        std::sync::Arc::new(std::sync::Mutex::new(std::collections::BTreeMap::new()));

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
        pw_targets.clone(),
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
        spawn_stored_sources(&sources, &airplay, &airplay_clients, &pw_state, pw_cmd.clone()).await;

        // Self-heal the multicast RTP source: the stock module can silently drop
        // its IGMP group join after a sender pause and never rejoin (audio goes
        // silent with the node idle). This watchdog holds its own group
        // membership and reloads the module when it sees audio arriving that the
        // module isn't picking up. No-op for a unicast/disabled source.
        rtp_membership::spawn(pw_state.clone(), pw_cmd.clone(), sources.clone());

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
            let pw_targets = pw_targets.clone();
            let mut rx = changes.subscribe();
            tokio::spawn(async move {
                // routing::reconcile first (direct links for any real-node output),
                // then sync_group (which owns the sendspin + AP2 groups) — the group
                // lead comes from the live sync settings so a change takes effect on
                // the next tick.
                let lead = sync_settings::group_lead_us(&settings);
                routing::reconcile(&pw, &cmd, &routing).await;
                groups.lock().await.reconcile(&pw, &cmd, &routing, &devices, &control, lead, &ap2_devices, &ap2_ptp, &settings, &ap2_control, &pw_targets).await;
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
                    groups.lock().await.reconcile(&pw, &cmd, &routing, &devices, &control, lead, &ap2_devices, &ap2_ptp, &settings, &ap2_control, &pw_targets).await;
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

        // Kept for the graceful-shutdown path below (the reconcile task's own
        // Drop isn't guaranteed to run on process exit, so pw-sink sessions are
        // torn down explicitly to send each receiver a clean BY).
        let groups_for_shutdown = groups.clone();

        let app = api::router(
            pw_state,
            changes,
            pw_cmd,
            sources,
            airplay.clone(),
            airplay_clients,
            meters,
            sendspin_devices,
            ap2_devices,
            pw_targets,
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

        // On SIGTERM/SIGINT (run.sh's trap / container stop), stop every AirPlay
        // receiver cleanly (unregister its mDNS, close listeners). Sendspin
        // group servers and the RTP module tear down with the process.
        tracing::info!("shutting down; stopping AirPlay sources");
        {
            let mut groups = groups_for_shutdown.lock().await;
            // End pw-sink sessions cleanly so remote module-rtp-session receivers get
            // a BY and drop the session now, instead of holding a stale one until it
            // times out (which otherwise blocks a prompt reconnect after restart).
            groups.shutdown_pwsink();
        }
        for (_id, handle) in std::mem::take(&mut *airplay.lock().await) {
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
    airplay_clients: &airplay_clients::AirplayClientStore,
    pw: &pw_thread::SharedState,
    pw_cmd: pw_thread::PwCommandSender,
) {
    // Snapshot the source list and drop the (std) lock before awaiting anything.
    let entries = {
        let s = sources.lock_recover();
        s.list()
    };

    // The RTP sources (bt-bridge et al.) are native PipeWire modules, not
    // subprocesses — loaded via the PipeWire thread like a RAOP sink, not the
    // supervisor. reconcile loads one module per stored RTP source and unloads
    // any orphans; a failed load is logged inside, not fatal — each can be
    // re-enabled live via the API.
    rtp_source::reconcile(&entries, &pw_cmd, pw).await;

    // Every configured AirPlay receiver (one per AirPlay source with a name):
    // the reconciler starts each on its own node/port/mDNS name with its own
    // per-source client registry + anti-takeover flag.
    airplay_source::reconcile(airplay, &entries, airplay_clients).await;
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
