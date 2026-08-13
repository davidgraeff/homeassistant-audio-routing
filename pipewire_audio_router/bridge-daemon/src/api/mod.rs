//! REST API: health check, live PipeWire registry state, and manual link
//! creation

use crate::outputs::ap2::discovery::SharedAp2Devices;
use crate::outputs::ap2::ptp::SharedAp2Ptp;
use crate::outputs::sendspin::discovery::SharedSendspinDevices;
use crate::pw::thread::{ChangeNotifier, LinkSpec, PwCommand, PwCommandSender, SharedState};
use crate::routing;
use crate::sources::airplay_clients::AirplayClientStore;
use crate::sources::rtp::{DEFAULT_RTP_IGNORE_SSRC, DEFAULT_RTP_LATENCY_MSEC, DEFAULT_RTP_PORT, DEFAULT_RTP_RATE, DEFAULT_RTP_SOURCE_ADDR};
use crate::sources::{AirplaySourceConfig, RtpSourceConfig, SourceConfig, SourceEntry, SourceKind};
use crate::state::{AppState, SharedAirplay, SharedSources};
use crate::store::outputs::SharedOutputs;
use crate::store::routing::SharedRouting;
use crate::store::settings::SharedSettings;
use crate::util::locks::LockRecover;
use crate::util::node_names::{AP2_DEV_PREFIX, PWSINK_DEV_PREFIX, SENDSPIN_DEV_PREFIX};
use axum::{
    body::{Body, Bytes},
    extract::{Extension, Path, Query, State},
    http::{header, StatusCode, Uri},
    response::{IntoResponse, Response},
    routing::{delete, get, post, put},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path as FsPath, PathBuf};
use std::sync::Arc;
use tokio::sync::oneshot;

// Internal wiring, not a public API surface with a stability concern — each
// param is a distinct shared handle `AppState` needs, not something a struct
// wrapper would make clearer to call sites (main.rs's one call site).
#[allow(clippy::too_many_arguments)]
pub fn router(
    pw_state: SharedState,
    changes: ChangeNotifier,
    pw_cmd: PwCommandSender,
    sources: SharedSources,
    airplay: SharedAirplay,
    airplay_clients: AirplayClientStore,
    now_playing: crate::sources::now_playing::NowPlayingStore,
    meters: crate::pw::metering::SharedMeters,
    xruns: crate::pw::profiler::SharedXruns,
    sendspin_devices: SharedSendspinDevices,
    ap2_devices: SharedAp2Devices,
    agents: crate::outputs::pwsink::agent::SharedAgents,
    bt_bridges: crate::sources::bt_bridge::SharedBtBridges,
    ap2_ptp: SharedAp2Ptp,
    routing: SharedRouting,
    outputs: SharedOutputs,
    sendspin_control: crate::outputs::sendspin::volume::SharedSendspinControl,
    ap2_control: crate::outputs::ap2::volume::SharedAp2Control,
    sync_settings: crate::routing::sync_settings::SharedSyncSettings,
    settings: SharedSettings,
    discovery: crate::supervisor::DiscoverySupervisor,
    align: crate::align::calibrate::AlignManager,
    groups: crate::routing::sync_group::SharedGroups,
    groups_config: crate::store::groups::SharedGroupsStore,
    version: String,
    started: std::time::Instant,
    static_dir: PathBuf,
) -> Router {
    let state = AppState {
        pw: pw_state,
        changes,
        pw_cmd,
        sources,
        airplay,
        airplay_clients,
        now_playing,
        meters,
        xruns,
        profiler_watchers: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        sendspin_devices,
        ap2_devices,
        agents,
        bt_bridges,
        ap2_ptp,
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
    };
    Router::new()
        .route("/health", get(health))
        // THE push socket (events.rs). One connection, topics subscribed by message —
        // four separate status sockets used to eat four of a browser's six per-host
        // HTTP/1.1 connections, leaving the pages that opened them no headroom for
        // their own REST calls.
        .route("/api/events", get(crate::events::events_ws))
        .route("/api/nodes", get(list_nodes))
        .route("/api/links", post(create_link))
        .route("/api/outputs", get(list_outputs))
        // Discovery only *offers* a device; these three are how the user decides.
        // `/discovered` is a static sibling of the `{node_name}` remove route below;
        // matchit prefers the static segment, and registering both is accepted (see
        // `discovered_listing_and_remove_routes_coexist` — a route conflict would
        // panic here at startup, where nothing would catch it).
        .route("/api/outputs/discovered", get(list_discovered_outputs))
        .route("/api/outputs/{node_name}", delete(remove_output))
        .route("/api/outputs/{node_name}/adopt", post(adopt_output))
        .route("/api/outputs/{node_name}/ignore", post(ignore_output))
        // pw-sink only: the removal that also revokes the pairing (plan §8).
        .route("/api/outputs/{node_name}/unpair", post(unpair_output))
        .route("/api/outputs/{node_name}/name", put(set_output_name))
        // The output's own knobs, addressed by the output and dispatched to whatever
        // transport it has (api/level.rs). One scale for volume (0.0–1.0), one name for
        // the timing knob whatever its polarity, one intent for "recover".
        .route("/api/outputs/{node_name}/volume", put(set_output_volume))
        .route("/api/outputs/{node_name}/mute", put(set_output_mute))
        .route("/api/outputs/{node_name}/delay", put(set_output_delay))
        .route("/api/outputs/{node_name}/resync", post(resync_output))
        .route("/api/outputs/{node_name}/ap2-rate", put(set_ap2_rate_mode))
        .route("/api/outputs/{node_name}/sendspin-codec", put(set_sendspin_codec))
        // Per-receiver AirPlay client/policy routes. `{id}` is the source id;
        // each receiver has its own client list + anti-takeover flag.
        .route("/api/sources/{id}/clients", get(list_source_clients))
        // One sender, addressed by its key: forgetting it is a DELETE of that
        // sub-resource, the two flags are PUTs on it, and disconnecting is the one real
        // action.
        .route("/api/sources/{id}/clients/{key}", delete(forget_source_client))
        .route("/api/sources/{id}/clients/{key}/ban", put(ban_source_client))
        .route("/api/sources/{id}/clients/{key}/priority", put(set_source_client_priority))
        .route("/api/sources/{id}/clients/{key}/disconnect", post(disconnect_source_client))
        .route("/api/sources/{id}/policy", put(set_source_policy))
        // Per-source now-playing metadata (sources/now_playing.rs). Keyed by source NODE
        // NAME, not by source id: that is the key the routing matrix, the routing
        // intent and the HA integration all already share, and it is what the
        // `now_playing` WebSocket frame is keyed by — so a consumer never has to
        // hold both keys. `report` is the self-identifying entry point for a
        // remote producer that knows its RTP port but not our source ids.
        .route("/api/now_playing", get(list_now_playing))
        .route("/api/now_playing/report", post(report_now_playing))
        .route("/api/now_playing/{node_name}", get(get_now_playing).put(put_now_playing).delete(clear_now_playing))
        .route("/api/now_playing/{node_name}/artwork", get(get_now_playing_artwork))
        // Multi-source collection CRUD — the sole source-management API.
        .route("/api/sources", get(list_sources).post(create_source))
        .route("/api/sources/{id}", get(get_source).put(update_source).delete(delete_source))
        .route("/api/sendspin/volumes", get(get_sendspin_volumes))
        .route("/api/sendspin/delays", get(get_sendspin_delays))
        .route("/api/sync/settings", get(get_sync_settings).put(set_sync_settings))
        .route("/api/settings", get(get_settings).put(set_settings))
        .route("/api/status", get(get_status))
        .route("/api/spike/per-device", post(spike_per_device_start).delete(spike_per_device_stop))
        .route("/api/spike/multi-device", post(spike_multi_device_start).delete(spike_per_device_stop))
        .route("/api/spike/overlay", post(spike_overlay_start).delete(spike_overlay_stop))
        .route("/api/spike/ap2", post(spike_ap2_start).delete(spike_ap2_stop))
        .route("/api/spike/pw-sink", post(spike_pwsink_start).delete(spike_pwsink_stop))
        .route("/api/announce", post(ag_announce))
        .route("/api/duck", get(duck_list).post(duck_start))
        .route("/api/duck/{hold_id}", post(duck_renew).delete(duck_release))
        .route("/api/groups/music", get(list_music_groups).post(create_music_group))
        .route("/api/groups/music/{id}", put(update_music_group).delete(delete_music_group))
        .route("/api/groups/music/{id}/route", post(route_music_group).delete(unroute_music_group))
        .route("/api/groups/announcement", get(list_announcement_groups).post(create_announcement_group))
        .route("/api/groups/announcement/{id}", put(update_announcement_group).delete(delete_announcement_group))
        .route("/api/align/groups", get(align_groups))
        .route("/api/align", get(align_status).delete(align_stop))
        // Pushed **session** state: one full `AlignState` on connect, then one per
        // change — and one on teardown, which is the frame no client could have
        // predicted. The session's hold is exclusive and its safety timeout is an idle
        // one, so an abandoned session hands the speakers back on its own; without this
        // the wizard would keep describing a session that no longer exists. The polling
        // `GET` above stays and the UI falls back to it.
        .route("/api/align/start", post(align_start))
        // "I am still here": the one way to postpone that idle teardown. A deliberate
        // click, never a heartbeat — an open socket or a status poll counts for nothing,
        // because a forgotten tab is exactly the hazard the timeout exists for.
        .route("/api/align/still-here", post(align_still_here))
        .route("/api/align/select", post(align_select))
        // Plan §12.2's solo: "these members are audible" — one for level-setting and
        // for the sequential measurement, all of them for §7's all-play round.
        .route("/api/align/audible", post(align_audible))
        .route("/api/align/volume", post(align_volume))
        // One member through one channel of its stereo pair (plan §12.2): a pair is two
        // acoustic sources, so its arrival time is not a single number.
        .route("/api/align/members/{node_name}/channel", post(align_channels))
        // Microphone-assisted alignment (align/mic.rs): the phone's capture socket
        // and the status the UI's level meter reads.
        .route("/api/align/mic/ws", get(crate::align::mic::mic_ws))
        .route("/api/align/mic", get(crate::align::mic::mic_status))
        // Whether the level is good enough to measure — the meter cannot say.
        .route("/api/align/mic/signal", get(mic_signal))
        // Measurement orchestration (align/measure/mod.rs, plan §11). `apply` is a
        // separate, explicit step: the user sees the proposed deltas and the
        // confidence before a single delay is written.
        .route("/api/align/measure", get(measure_status).delete(measure_abandon))
        // Pushed run status (plan §11): one full `MeasureStatus` on connect, then one
        // per change. The polling `GET` above stays, and the UI falls back to it —
        // a run that looks frozen because a socket dropped is worse than a poll.
        // The relay-vs-device equivalence experiment (plan §1.1.1). Separate from a
        // measurement run — and it refuses while one is live, since both drive the same
        // session.
        .route("/api/align/equivalence", get(equivalence_status).post(equivalence_start).delete(equivalence_abandon))
        .route("/api/align/equivalence/{node_name}", post(equivalence_start_member))
        .route("/api/align/measure/start", post(measure_start))
        // Near field only (plan §1, W8a). The daemon cannot see where the phone is, so
        // the walk is driven by the user: one `arrival` per speaker while standing at
        // it, then `close` back at the first one for the drift measurement.
        .route("/api/align/measure/arrival/{node_name}", post(measure_arrival))
        .route("/api/align/measure/close", post(measure_close))
        // Multi-position chaining (plan §1.1): one position per listening spot, then
        // finish — which renormalises the whole chain and proposes the single write.
        .route("/api/align/measure/position", post(measure_position))
        .route("/api/align/measure/finish", post(measure_finish))
        .route("/api/align/measure/apply", post(measure_apply))
        .route("/api/align/measure/revert", post(measure_revert))
        // Per-output band-split calibration (plan §10.2): measured at the speaker,
        // subtracted from that speaker's cross-band split so a mixed-model group is
        // not refused for its hardware.
        .route("/api/align/measure/split", get(measure_splits))
        .route("/api/align/measure/split/{node_name}", post(measure_split_calibrate).delete(measure_split_clear))
        // The persisted run transcripts (plan §11): the listing, or one whole run as
        // one document with `?run=<id>` / `?run=latest`.
        .route("/api/align/measure/log", get(measure_log))
        .route("/api/routing", get(routing::get_routing))
        .route("/api/routing/link", post(routing::link))
        .route("/api/routing/unlink", post(routing::unlink))
        .route("/api/routing/entity/{node_name}", delete(routing::forget_entity))
        // The receiver agents' own control plane: the agent dials in here.
        // Pairing decisions are *output* operations (`/adopt`, `/ignore`, `/unpair`)
        // — a host asking to pair is a discovered output, so it is decided where
        // every other output is. This listing is left for diagnostics.
        .route("/api/agent/ws", get(crate::outputs::pwsink::agent::agent_ws))
        .route("/api/agents", get(get_agents))
        // Everything else (`/`, `/assets/*`, favicon, …) is the built Svelte SPA,
        // read into memory ONCE at startup (below) and served from RAM. This
        // deliberately does NOT use `ServeDir`: the add-on's `/data` lives on a USB
        // stick whose filesystem gets slow (and slower as it fills), and a per-
        // request `tokio::fs` read there — with no read timeout — stalls the
        // blocking pool and the UI won't load. In-RAM serving does one boot-time
        // read, then never touches the disk per request.
        .fallback(static_fallback)
        .layer(Extension(Arc::new(StaticAssets::load(&static_dir))))
        .with_state(state)
}

pub(crate) mod agents;
pub(crate) mod align;
pub(crate) mod announce;
pub(crate) mod clients;
pub(crate) mod duck;
/// The one way to fail (and to say a write worked) — see its module docs.
pub(crate) mod error;
pub(crate) mod groups;
mod level;
pub(crate) mod measure;
pub(crate) mod nodes;
pub(crate) mod now_playing;
pub(crate) mod outputs;
pub(crate) mod settings;
pub(crate) mod sources;
pub(crate) mod spike;
pub(crate) mod sync;
pub(crate) mod volume;

// Each submodule owns one resource's handlers and DTOs, re-exported here so the
// route table above and the handlers themselves address each other by bare name.
// The file boundaries organise the code; they are not a visibility boundary, and
// a handler that needs a sibling's DTO just uses it.
pub(crate) use agents::*;
pub(crate) use align::*;
pub(crate) use announce::*;
pub(crate) use clients::*;
pub(crate) use duck::*;
pub(crate) use error::*;
pub(crate) use groups::*;
pub(crate) use level::*;
pub(crate) use measure::*;
// The output listing model lives in outputs/listing.rs, because the routing matrix
// consumes it too; re-exported so the handlers address it by bare name as well.
pub(crate) use crate::outputs::listing::*;
pub(crate) use nodes::*;
pub(crate) use now_playing::*;
pub(crate) use outputs::*;
pub(crate) use settings::*;
pub(crate) use sources::*;
pub(crate) use spike::*;
pub(crate) use sync::*;
pub(crate) use volume::*;

#[cfg(test)]
mod tests;
