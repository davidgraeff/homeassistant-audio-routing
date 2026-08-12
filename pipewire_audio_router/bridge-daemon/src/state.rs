//! The shared axum state, and nothing else.
//!
//! `AppState` is the daemon's one wide handle: every live store, registry
//! snapshot, control channel and manager the HTTP layer can reach. It lives in
//! its own module because both the API and the subsystems need it — `routing/`,
//! `outputs/pwsink/agent.rs` and `outputs/listing.rs` all take an `&AppState`.
//! Keeping it out of `api/` is what lets that module stay a leaf: nothing in the
//! crate depends on the API except `main.rs`, which wires the server up.
//!
//! Handlers extract just the piece they need via `FromRef`, so adding a field
//! here does not touch them.

use crate::outputs::ap2::discovery::SharedAp2Devices;
use crate::outputs::ap2::ptp::SharedAp2Ptp;
use crate::outputs::sendspin::discovery::SharedSendspinDevices;
use crate::pw::thread::PwCommandSender;
use crate::pw::thread::{ChangeNotifier, SharedState};
use crate::sources::airplay_clients::AirplayClientStore;
use crate::sources::SourcesStore;
use crate::store::outputs::SharedOutputs;
use crate::store::routing::SharedRouting;
use crate::store::settings::SharedSettings;
use axum::extract::FromRef;
use std::sync::{Arc, Mutex};
/// Runtime config for the AirPlay and RTP sources.
pub type SharedSources = Arc<Mutex<SourcesStore>>;

/// The running AirPlay-receive sources (sources/airplay.rs), keyed by source id —
/// each a native embedded RAOP server feeding its own PipeWire source node.
/// Phase 4: multiple concurrent receivers, reconciled against the store.
pub type SharedAirplay = crate::sources::airplay::SharedAirplayMap;

/// Shared axum state: the live PipeWire registry snapshot, the routing UI's
/// change-notification channel (routing/mod.rs), and the command sender for runtime
/// module load/unload (pw/thread.rs). Existing handlers extract just the piece
/// they need via `FromRef` — they don't need to know this type grew more fields.
#[derive(Clone)]
pub struct AppState {
    pub pw: SharedState,
    pub changes: ChangeNotifier,
    pub pw_cmd: PwCommandSender,
    pub sources: SharedSources,
    pub airplay: SharedAirplay,
    /// Remembered AirPlay senders (sources/airplay_clients.rs), per-receiver — the
    /// backing store for each source's connection list + ban/priority controls.
    /// Per-source views are taken via `.registry(id)`. Anti-takeover is now
    /// per-receiver too (each running `AirplayHandle` owns its flag), so there is
    /// no process-wide flag here anymore.
    pub airplay_clients: AirplayClientStore,
    /// Per-source now-playing metadata (sources/now_playing.rs) — what each input is
    /// currently playing, from whichever producer can say: the AirPlay receiver's
    /// DMAP callbacks locally, a Pi reporter over `/api/sources/{id}/now_playing`
    /// remotely. Read into the routing socket's `now_playing` frame.
    pub now_playing: crate::sources::now_playing::NowPlayingStore,
    /// On-demand source peak meters (pw/metering.rs); taps live only while a
    /// routing-matrix WS client is connected.
    pub meters: crate::pw::metering::SharedMeters,
    /// Per-node xrun counts from the PipeWire profiler (pw/profiler.rs), written by
    /// the PipeWire thread while profiling is armed and read into the routing
    /// snapshot. Empty when the routing UI is closed.
    pub xruns: crate::pw::profiler::SharedXruns,
    /// Count of open routing-matrix WebSockets. The first arms profiling
    /// (`PwCommand::SetProfiling(true)`), the last disarms it — same "pay only
    /// while watched" gating as the peak meters.
    pub profiler_watchers: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    /// Live mDNS-discovered sendspin devices (outputs/sendspin/discovery.rs), surfaced
    /// as virtual routing outputs.
    pub sendspin_devices: SharedSendspinDevices,
    /// Live mDNS-discovered AirPlay-2 receivers (outputs/ap2/discovery.rs), surfaced as
    /// virtual routing outputs (`ap2-dev-*`). The RAOP-output replacement.
    pub ap2_devices: SharedAp2Devices,
    /// Paired receiver agents (outputs/pwsink/agent.rs) — the source of truth for pw-sink
    /// outputs (there is no mDNS registry behind them: `pw_target_discovery` is a
    /// diagnostic that nothing here reads), their volume/mute control channel, and
    /// the pairing queue.
    pub agents: crate::outputs::pwsink::agent::SharedAgents,
    /// Live mDNS-discovered Bluetooth→RTP bridges (sources/bt_bridge.rs).
    /// Unlike the other discoveries these are *senders*, not outputs: they build
    /// no audio path, they annotate an RTP source with which bridge feeds it and
    /// offer that bridge's diagnostics page. Unconfigured ones are offered for
    /// adoption on the Sources tab.
    pub bt_bridges: crate::sources::bt_bridge::SharedBtBridges,
    /// The daemon's single host-global AirPlay-2 PTP grandmaster (outputs/ap2/ptp.rs),
    /// reused by the AP2 tone spike (spike/ap2.rs) so it shares 319/320 rather
    /// than double-binding.
    pub ap2_ptp: SharedAp2Ptp,
    pub sendspin_control: crate::outputs::sendspin::volume::SharedSendspinControl,
    pub ap2_control: crate::outputs::ap2::volume::SharedAp2Control,
    /// Persistent routing intent (store/routing.rs): links by stable node
    /// name, reconciled onto the live graph so routing survives node reloads
    /// and device disappearance/reappearance.
    pub routing: SharedRouting,
    /// Which discovered outputs the user adopted (store/outputs.rs). Discovery
    /// only *offers* a device; until it's added here it stays out of the routing
    /// matrix, out of Home Assistant and out of the group reconciler.
    pub outputs: SharedOutputs,
    /// Persistent sync/latency tuning (routing/sync_settings.rs): the group presentation
    /// lead + per-sendspin-device static delays.
    pub sync_settings: crate::routing::sync_settings::SharedSyncSettings,
    /// General app settings (store/settings.rs): announce default duck, mDNS
    /// discovery on/off.
    pub settings: SharedSettings,
    /// Runtime mDNS on/off, driven by the discovery flag above.
    pub discovery: crate::supervisor::DiscoverySupervisor,
    /// Latency-alignment session manager (align/calibrate/mod.rs) for the alignment panel.
    pub align: crate::align::calibrate::AlignManager,
    /// Live sync-group layout (routing/sync_group/mod.rs) — used to restart a group's
    /// sendspin stream when a static-delay change needs it to take effect.
    pub groups: crate::routing::sync_group::SharedGroups,
    /// Named music/announcement groups (store/groups.rs) — the MG/AG data model.
    pub groups_config: crate::store::groups::SharedGroupsStore,
    /// Add-on version string (main.rs `addon_version()`), for `/api/status`.
    pub version: String,
    /// Process start instant, for the `/api/status` uptime.
    pub started: std::time::Instant,
}

impl FromRef<AppState> for SharedState {
    fn from_ref(state: &AppState) -> SharedState {
        state.pw.clone()
    }
}

impl FromRef<AppState> for ChangeNotifier {
    fn from_ref(state: &AppState) -> ChangeNotifier {
        state.changes.clone()
    }
}
