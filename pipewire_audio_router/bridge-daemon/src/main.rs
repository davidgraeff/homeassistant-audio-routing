mod align_estimator;
mod align_group;
mod align_levels;
mod align_measure;
mod align_mic;
mod airplay_clients;
mod airplay_source;
mod announce;
mod announce_arbiter;
mod ap2_discovery;
mod ap2_health;
mod ap2_liveness;
mod ap2_probe;
mod ap2_ptp;
mod ap2_server;
mod ap2_spike;
mod ap2_volume;
mod api;
mod applemidi_sender;
mod bt_bridge_discovery;
mod calibrate;
mod config;
mod decode;
mod discovery_supervisor;
mod groups_store;
mod host_assessment;
mod locks;
mod metering;
mod now_playing;
mod outputs_store;
mod overlay_mixer;
mod per_device_spike;
mod player;
mod profiler;
mod pw_sink;
mod pw_sink_liveness;
mod pw_sink_spike;
mod pw_target_discovery;
mod pw_target_liveness;
mod pw_thread;
mod pwsink_agent;
mod pwsink_server;
mod raop_migration;
mod relay_delay;
mod resample;
mod routing;
mod routing_store;
mod rtp_source;
mod sendspin_capture;
mod sendspin_codec;
mod sendspin_discovery;
mod sendspin_liveness;
mod sendspin_server;
mod sendspin_volume;
mod settings_store;
mod sources_store;
mod sync_group;
mod sync_settings;
mod wav;

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
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(
            |_| // `sendspin=info` matters more than it looks: the vendored server role logs its
            // dial loop (attempt, failure, retry backoff, goodbye reason) through the `log`
            // crate, and without its target enabled every one of those lines is dropped —
            // which left "why is this speaker not connected?" unanswerable from the log.
            "bridge_daemon=info,sendspin=info,shairplay=info,airplay_client=info,airplay_audio=info,libairptp=info".into(),
        ))
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

    // Which discovered outputs the user has adopted (outputs_store.rs) — the
    // gate that keeps a freshly discovered device out of the routing matrix and
    // out of Home Assistant until it's added on the Outputs page. Beside the
    // other /data stores; empty on a fresh install *and* on upgrade, so an
    // existing deployment comes up with its routing intact but dormant until
    // each device is added once.
    let outputs_path = routing_path.with_file_name("outputs.json");
    let outputs = outputs_store::OutputsStore::load(&outputs_path)?;
    tracing::info!("{} adopted output(s) in {}", outputs.adopted().len(), outputs_path.display());
    let outputs: outputs_store::SharedOutputs = std::sync::Arc::new(std::sync::Mutex::new(outputs));

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
    let ap2_devices: ap2_discovery::SharedAp2Devices = std::sync::Arc::new(std::sync::Mutex::new(std::collections::BTreeMap::new()));
    let ap2_ptp = ap2_ptp::Ap2PtpService::new();
    // Hosts advertising an RTP session over mDNS (pw_target_discovery.rs).
    // **Diagnostic only**: pw-sink outputs come from paired agents (plan §3), and
    // these adverts cannot serve that role — they are keyed by hostname alone, while
    // a pairing (and therefore every routing link and HA entity) carries the user
    // too. Kept because "that host advertises a session" is worth a log line; read
    // by nothing but its own liveness bookkeeping.
    let pw_targets: pw_target_discovery::SharedPwTargets = std::sync::Arc::new(std::sync::Mutex::new(std::collections::BTreeMap::new()));
    // Discovered Bluetooth→RTP bridges (bt_bridge_discovery.rs): Pis advertising
    // `_pwrouter-btbridge._tcp`. These are input *senders*, so unlike the four
    // registries above they drive no audio path — they let the Sources tab offer
    // one-click adoption and a link to a bridge's diagnostics page.
    let bt_bridges: bt_bridge_discovery::SharedBtBridges = std::sync::Arc::new(std::sync::Mutex::new(std::collections::BTreeMap::new()));

    let (pw_state, changes, pw_cmd, xruns) = pw_thread::spawn()?;

    // Per-source now-playing metadata (now_playing.rs). Purely in-memory: it
    // describes what is playing *right now*, so there is nothing worth persisting
    // across a restart — a live producer re-reports within seconds, and a stale
    // track restored from disk would be a lie.
    let now_playing = now_playing::NowPlayingStore::new(changes.clone());

    // Paired receiver agents (pwsink_agent.rs): the token store *and* the source
    // of truth for pw-sink targets — a pw-sink output exists because a helper on
    // that host paired, not because something answered an mDNS browse
    // (docs/receiver-agent-plan.md §3).
    let agents_path = routing_path.with_file_name("agents.json");
    let agents = pwsink_agent::Agents::shared(agents_path.clone(), changes.clone());
    let agents_for_duck = agents.clone();

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
        bt_bridges.clone(),
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
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(4).enable_all().build()?;
    rt.block_on(async {
        // Start every stored source/adapter process (AirPlay source via
        // Supervisor) and the RTP source. A failed spawn is logged, not fatal
        // — the rest still start and it can be re-enabled live.
        spawn_stored_sources(&sources, &airplay, &airplay_clients, &now_playing, &pw_state, pw_cmd.clone()).await;

        // NOTE: a multicast-IGMP self-heal watchdog used to run here
        // (`rtp_membership.rs`, removed 2026-07-28). `module-rtp-source` does this
        // itself since PipeWire 1.6.2 — `on_igmp_recovery_timer_event` checks every
        // 5 s (`DEFAULT_IGMP_CHECK_INTERVAL_SEC`) whether ≥30 s have passed since
        // its last packet (`DEFAULT_IGMP_DEADLINE_SEC`) and, if so, re-joins by
        // DROP+ADD_MEMBERSHIP on its own socket — no module reload, no audible gap.
        // See docs/rtp-input-dropouts-plan.md §5.

        // Own sendspin device online/offline (and eventual removal) from the
        // live connection state + an active TCP probe — mDNS only ever adds.
        // Without this, an mDNS TTL flap would tear down live groups.
        sendspin_liveness::spawn(sendspin_devices.clone(), sendspin_control.clone(), changes.clone());
        // Same contract for AP2 receivers: mDNS (ap2_discovery) only adds; this
        // task TCP-probes each and demotes/removes a powered-off receiver so its
        // sender is torn down (and its PTP peer released).
        ap2_liveness::spawn(ap2_devices.clone(), ap2_ptp.clone(), changes.clone());
        // And for pw-sink targets, which have nothing to probe (the receiver dials
        // us): presence follows the advert, debounced, with an established session
        // as proof of life. Without this a target seen once stayed "online" forever.
        pw_target_liveness::spawn(pw_targets.clone(), changes.clone());

        // Let device-reported sendspin volume changes (sendspin_server.rs) nudge
        // the routing-matrix WebSocket, so the UI slider syncs live to a physical
        // volume change without polling. AP2 volume/mute is UI-driven (no receiver
        // feedback yet) but uses the same notifier so a set pushes immediately.
        sendspin_control.lock().await.set_change_notifier(changes.clone());
        ap2_control.lock().await.set_change_notifier(changes.clone());
        // Same for the pw-sink handshake: the matrix reports it as each output's
        // `streaming` (whether a route is really being carried), so a receiver
        // attaching or dropping has to push a frame of its own.
        pw_sink_liveness::PwSinkLiveness::global().set_change_notifier(changes.clone());

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
        // How a *pw-sink* member is silenced during a solo. Alignment resolves the
        // mechanism per output rather than per kind (calibrate.rs `SilenceChannel`):
        // sendspin and AP2 have in-band mutes, and a receiver host is silenced over its
        // agent's control lane — preferred, because only the remote sink's volume moves,
        // so the stream keeps flowing and unmuting cannot introduce a discontinuity the
        // estimator would measure as a real offset. Without this the session falls back
        // to zero-filling in the relay, which also works; with it, the better mechanism
        // is used wherever an agent is actually there.
        align.set_out_of_band_mute(std::sync::Arc::new(pwsink_agent::AgentSilencer(agents.clone())));
        {
            let pw = pw_state.clone();
            let cmd = pw_cmd.clone();
            let routing = routing.clone();
            let adopted = outputs.clone();
            let devices = sendspin_devices.clone();
            let control = sendspin_control.clone();
            let ap2_devices = ap2_devices.clone();
            let ap2_ptp = ap2_ptp.clone();
            let ap2_control = ap2_control.clone();
            let settings = sync_settings.clone();
            let groups = groups.clone();
            // Receiver hosts, for the pw-sink half of the reconcile. Snapshotted per
            // pass here rather than reached for inside `reconcile`, which is where the
            // rest of its inputs come from too — and the registry is an async mutex,
            // so it cannot be read from the sync section that builds the groups.
            let agents_for_groups = agents.clone();
            let mut rx = changes.subscribe();
            tokio::spawn(async move {
                // routing::reconcile first (direct links for any real-node output),
                // then sync_group (which owns the sendspin + AP2 groups) — the group
                // lead comes from the live sync settings so a change takes effect on
                // the next tick.
                use tokio::sync::broadcast::error::RecvError;
                // Coalescing window: after a change wakes us, wait for a brief quiet
                // period (draining any further changes) before reconciling, so a
                // burst of routing/liveness changes produces ONE reconcile instead of
                // back-to-back teardown+reconnect — the latter trips an AP2 receiver's
                // transient "Pairing error M2" when a new session is opened before the
                // prior one is released. 400 ms is imperceptible for routing yet
                // absorbs a rapid click-storm or liveness flap.
                const RECONCILE_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(400);
                // How soon to come back when a pass left work undone (a sender that
                // failed to start, a node that hadn't appeared yet, a device whose
                // URL hadn't resolved). Without this the loop is purely change-driven,
                // so a transient failure left a group silent until some unrelated
                // event happened along — and the events that would have helped are
                // exactly the ones a failing group doesn't produce.
                const RECONCILE_RETRY: std::time::Duration = std::time::Duration::from_secs(3);
                loop {
                    let lead = sync_settings::group_lead_us(&settings);
                    routing::reconcile(&pw, &cmd, &routing).await;
                    let pwsink_hosts = agents_for_groups.lock().await.connected_targets();
                    let retry = {
                        let mut g = groups.lock().await;
                        g.reconcile(
                            &pw,
                            &cmd,
                            &routing,
                            &adopted,
                            &devices,
                            &control,
                            lead,
                            &ap2_devices,
                            &ap2_ptp,
                            &settings,
                            &ap2_control,
                            &pwsink_hosts,
                        )
                        .await;
                        g.retry_wanted()
                    };
                    // Wait for the next change — or for the retry delay, whichever
                    // comes first. Closed channel ends the task.
                    if retry {
                        match tokio::time::timeout(RECONCILE_RETRY, rx.recv()).await {
                            Ok(Ok(())) | Ok(Err(RecvError::Lagged(_))) => {}
                            Ok(Err(RecvError::Closed)) => return,
                            Err(_elapsed) => continue, // retry now, nothing to coalesce
                        }
                    } else {
                        match rx.recv().await {
                            Ok(()) | Err(RecvError::Lagged(_)) => {}
                            Err(RecvError::Closed) => return,
                        }
                    }
                    // Drain the burst: keep resetting the quiet timer while changes
                    // keep arriving; reconcile once it goes quiet. Exit if closed.
                    loop {
                        match tokio::time::timeout(RECONCILE_DEBOUNCE, rx.recv()).await {
                            Ok(Ok(())) | Ok(Err(RecvError::Lagged(_))) => continue,
                            Ok(Err(RecvError::Closed)) => return,
                            Err(_elapsed) => break, // quiet window elapsed
                        }
                    }
                }
            });
        }

        // Announce coordinator tick: complete finished per-device overlays (start
        // the next queued clip / end the duck), release overlays no sender is
        // consuming, expire stale queued announcements, and enforce duck-hold
        // leases (voice ducking). Cheap no-op when nothing is announcing or
        // ducked.
        tokio::spawn(async move {
            let coordinator = announce::AnnounceCoordinator::global();
            let mut ticker = tokio::time::interval(std::time::Duration::from_millis(150));
            loop {
                ticker.tick().await;
                coordinator.poll();
            }
        });

        // On-demand AP2 announce sessions (sync_group.rs): an unrouted receiver gets
        // a temporary sender so an announcement can reach it, kept on a short lease
        // afterwards. This tick hands the receiver's single AirPlay session back once
        // the lease runs out. Slow on purpose (nothing is time-critical) and
        // `try_lock`, so it never queues behind a reconcile pass — a skipped tick
        // just tears down a second later.
        {
            let groups = groups.clone();
            let pw_cmd = pw_cmd.clone();
            tokio::spawn(async move {
                let mut ticker = tokio::time::interval(std::time::Duration::from_secs(1));
                loop {
                    ticker.tick().await;
                    if let Ok(mut g) = groups.try_lock() {
                        g.poll_announce_sessions(&pw_cmd).await;
                    }
                }
            });
        }

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
            now_playing,
            meters,
            xruns,
            sendspin_devices,
            ap2_devices,
            agents,
            bt_bridges,
            ap2_ptp.clone(),
            routing,
            outputs,
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
        // Tell receiver agents where to dial in (docs/receiver-agent-plan.md §8).
        // After bind, so the port in the advert is one that is actually listening.
        if let Ok(addr) = listener.local_addr() {
            pwsink_agent::advertise(addr.port());
        }
        // Relay for announcement ducks on agent-backed hosts: announce.rs is
        // synchronous, so it posts requests here instead of taking the async
        // registry lock (pwsink_agent.rs).
        pwsink_agent::spawn_duck_relay(agents_for_duck);
        tracing::info!("listening on {listen}");
        axum::serve(listener, app).with_graceful_shutdown(shutdown_signal()).await?;

        // On SIGTERM/SIGINT (run.sh's trap / container stop), stop every AirPlay
        // receiver cleanly (unregister its mDNS, close listeners). The RTP module
        // tears down with the process; everything that a *device* holds state for is
        // closed deliberately below.
        tracing::info!("shutting down; stopping AirPlay sources");
        {
            let mut groups = groups_for_shutdown.lock().await;
            // End pw-sink sessions cleanly so remote module-rtp-session receivers get
            // a BY and drop the session now, instead of holding a stale one until it
            // times out (which otherwise blocks a prompt reconnect after restart).
            groups.shutdown_pwsink();
            // Same reasoning for AirPlay-2, and it also matters for the *next* start:
            // a receiver accepts one session and holds an unclosed one until it times
            // out, which is what makes the first connect after a restart fail. Awaits
            // the RTSP TEARDOWNs (bounded), unlike a plain drop.
            groups.shutdown_ap2().await;
            // And sendspin: a device whose socket is killed mid-stream answers the
            // next daemon's reconnect with tens of seconds of silence (2026-07-28
            // hardware test), which is most of why a restart used to be so audible.
            groups.shutdown_sendspin().await;
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
    now_playing: &now_playing::NowPlayingStore,
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
    airplay_source::reconcile(airplay, &entries, airplay_clients, now_playing).await;
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
