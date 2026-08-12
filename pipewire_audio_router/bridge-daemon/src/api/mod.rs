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
        .route("/api/outputs/{node_name}/latency", put(set_output_latency))
        .route("/api/outputs/{node_name}/ap2-rate", put(set_ap2_rate_mode))
        .route("/api/outputs/{node_name}/sendspin-codec", put(set_sendspin_codec))
        // Per-receiver AirPlay client/policy routes. `{id}` is the source id;
        // each receiver has its own client list + anti-takeover flag.
        .route("/api/sources/{id}/clients", get(list_source_clients))
        .route("/api/sources/{id}/clients/forget", post(forget_source_client))
        .route("/api/sources/{id}/clients/ban", post(ban_source_client))
        .route("/api/sources/{id}/clients/priority", post(set_source_client_priority))
        .route("/api/sources/{id}/clients/disconnect", post(disconnect_source_client))
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
        .route("/api/sendspin/volume", put(set_sendspin_volume))
        .route("/api/sendspin/mute", put(set_sendspin_mute))
        .route("/api/sendspin/clear", post(clear_sendspin_stream))
        .route("/api/ap2/volume", put(set_ap2_volume))
        .route("/api/ap2/mute", put(set_ap2_mute))
        .route("/api/sendspin/delays", get(get_sendspin_delays))
        .route("/api/sendspin/delay", put(set_sendspin_delay_handler))
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
        .route("/api/align/start", post(align_start))
        .route("/api/align/select", post(align_select))
        // Plan §12.2's solo: "these members are audible" — one for level-setting and
        // for the sequential measurement, all of them for §7's all-play round.
        .route("/api/align/audible", post(align_audible))
        .route("/api/align/volume", post(align_volume))
        // Microphone-assisted alignment (align/mic.rs): the phone's capture socket
        // and the status the UI's level meter reads.
        .route("/api/align/mic/ws", get(crate::align::mic::mic_ws))
        .route("/api/align/mic", get(crate::align::mic::mic_status))
        // Whether the level is good enough to measure — the meter cannot say.
        .route("/api/align/mic/signal", get(mic_signal))
        // Measurement orchestration (align/measure.rs, plan §11). `apply` is a
        // separate, explicit step: the user sees the proposed deltas and the
        // confidence before a single delay is written.
        .route("/api/align/measure", get(measure_status).delete(measure_abandon))
        // Pushed run status (plan §11): one full `MeasureStatus` on connect, then one
        // per change. The polling `GET` above stays, and the UI falls back to it —
        // a run that looks frozen because a socket dropped is worse than a poll.
        .route("/api/align/measure/ws", get(crate::align::measure::measure_ws))
        // The relay-vs-device equivalence experiment (plan §1.1.1). Separate from a
        // measurement run — and it refuses while one is live, since both drive the same
        // session.
        .route("/api/align/equivalence", get(equivalence_status).post(equivalence_start).delete(equivalence_abandon))
        .route("/api/align/equivalence/ws", get(crate::align::measure::equivalence_ws))
        .route("/api/align/measure/start", post(measure_start))
        // Near field only (plan §1, W8a). The daemon cannot see where the phone is, so
        // the walk is driven by the user: one `arrival` per speaker while standing at
        // it, then `close` back at the first one for the drift measurement.
        .route("/api/align/measure/arrival", post(measure_arrival))
        .route("/api/align/measure/close", post(measure_close))
        // Multi-position chaining (plan §1.1): one position per listening spot, then
        // finish — which renormalises the whole chain and proposes the single write.
        .route("/api/align/measure/position", post(measure_position))
        .route("/api/align/measure/finish", post(measure_finish))
        .route("/api/align/measure/apply", post(measure_apply))
        .route("/api/align/measure/revert", post(measure_revert))
        .route("/api/routing", get(routing::get_routing))
        .route("/api/routing/link", post(routing::link))
        .route("/api/routing/unlink", post(routing::unlink))
        .route("/api/routing/entity/{node_name}", delete(routing::forget_entity))
        .route("/api/routing/ws", get(routing::routing_ws))
        // The receiver agents' own control plane: the agent dials in here.
        // Pairing decisions are *output* operations (`/adopt`, `/ignore`, `/unpair`)
        // — a host asking to pair is a discovered output, so it is decided where
        // every other output is. This listing is left for diagnostics.
        .route("/api/agent/ws", get(crate::outputs::pwsink::agent::agent_ws))
        .route("/api/agents", get(get_agents))
        .route("/api/pwsink/volume", put(set_pwsink_volume))
        .route("/api/pwsink/mute", put(set_pwsink_mute))
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
pub(crate) mod groups;
pub(crate) mod measure;
pub(crate) mod nodes;
pub(crate) mod now_playing;
pub(crate) mod outputs;
pub(crate) mod settings;
pub(crate) mod sources;
pub(crate) mod spike;
pub(crate) mod sync;
pub(crate) mod volume;

// Each submodule owns one resource's handlers and DTOs. The route table above
// and the handlers themselves address each other by bare name, exactly as they
// did when this was one file — the split is a filing change, not an API change.
pub(crate) use agents::*;
pub(crate) use align::*;
pub(crate) use announce::*;
pub(crate) use clients::*;
pub(crate) use duck::*;
pub(crate) use groups::*;
pub(crate) use measure::*;
// The output listing model lives in outputs/, since the routing matrix consumes
// it too (plan §6); re-exported here so the handlers keep addressing it by name.
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
mod tests {
    use super::*;
    use crate::sources::airplay::DEFAULT_AIRPLAY_LATENCY_MSEC;
    use crate::sources::LEGACY_RTP_ID;

    /// `/api/outputs/discovered` is a *static* sibling of `/api/outputs/{node_name}`
    /// (the remove route) — same segment count. axum panics on genuinely conflicting
    /// routes at build time, i.e. at daemon startup where nothing would catch it, so
    /// pin the pairing down here: registering both must be accepted, and the static
    /// path must win for the listing.
    #[test]
    fn discovered_listing_and_remove_routes_coexist() {
        let app: Router = Router::new()
            .route("/api/outputs", get(|| async { "all" }))
            .route("/api/outputs/discovered", get(|| async { "offered" }))
            .route("/api/outputs/{node_name}", delete(|| async { "removed" }))
            .route("/api/outputs/{node_name}/adopt", post(|| async { "added" }));
        // Building it is the assertion (a conflict would have panicked above).
        drop(app);
    }

    /// `/api/now_playing/report` is a *static* sibling of
    /// `/api/now_playing/{node_name}` — the same trap as the discovered/remove
    /// pairing above, and a conflict would panic at daemon startup. It also pins
    /// the precedence: a source could in principle be named `report`.
    #[test]
    fn now_playing_report_and_node_routes_coexist() {
        let app: Router = Router::new()
            .route("/api/now_playing", get(|| async { "all" }))
            .route("/api/now_playing/report", post(|| async { "reported" }))
            .route("/api/now_playing/{node_name}", get(|| async { "one" }).put(|| async { "updated" }))
            .route("/api/now_playing/{node_name}/artwork", get(|| async { "art" }));
        drop(app);
    }

    /// The wire shape a remote reporter sends: its port alongside the metadata
    /// fields *flattened*, not nested. The Pi's reporter is written against this.
    #[test]
    fn a_report_body_is_the_port_plus_flattened_metadata() {
        let req: NowPlayingReportRequest =
            serde_json::from_str(r#"{"rtp_port":46000,"title":"Song","artist":"Artist","state":"playing","position_ms":1234}"#)
                .expect("parses");
        assert_eq!(req.rtp_port, 46000);
        assert_eq!(req.metadata.title.as_deref(), Some("Song"));
        assert_eq!(req.metadata.artist.as_deref(), Some("Artist"));
        assert_eq!(req.metadata.state, Some(crate::sources::now_playing::PlaybackState::Playing));
        assert_eq!(req.metadata.position_ms, Some(1234));
    }

    /// A bare port with no fields is how a reporter says "nothing is playing" —
    /// `report_now_playing` turns that into a clear, so it must parse, and
    /// `is_empty()` must recognize it.
    #[test]
    fn a_report_body_may_carry_no_metadata_at_all() {
        let req: NowPlayingReportRequest = serde_json::from_str(r#"{"rtp_port":46001}"#).expect("parses");
        assert!(req.metadata.is_empty());
    }

    fn airplay_entry() -> SourceEntry {
        SourceEntry {
            id: "kitchen-airplay".to_string(),
            label: "Kitchen AirPlay".to_string(),
            config: SourceConfig::Airplay(AirplaySourceConfig { latency_msec: 100, auth_setup: false, prevent_takeover: true, port: 5000 }),
        }
    }

    fn rtp_entry() -> SourceEntry {
        SourceEntry {
            id: "garage-bridge".to_string(),
            label: "Garage Bridge".to_string(),
            config: SourceConfig::Rtp(RtpSourceConfig {
                port: 47000,
                latency_msec: 200,
                source_addr: "0.0.0.0".to_string(),
                ignore_ssrc: true,
                rate: 48000,
            }),
        }
    }

    #[test]
    fn source_view_airplay_shape() {
        let view = source_view(&airplay_entry(), true, &[]);
        assert_eq!(view.id, "kitchen-airplay");
        assert_eq!(view.kind, SourceKind::Airplay);
        assert!(view.present); // passed through verbatim
        assert_eq!(view.node_name, "airplay-in-kitchen-airplay");
        assert!(view.airplay.is_some());
        assert!(view.rtp.is_none()); // exactly one config populated

        // Exact JSON: nested `airplay` object (flat 4 knobs), `rtp` null.
        let json = serde_json::to_value(&view).unwrap();
        assert_eq!(json["kind"], "airplay");
        assert_eq!(json["present"], true);
        assert_eq!(json["node_name"], "airplay-in-kitchen-airplay");
        assert_eq!(json["rtp"], serde_json::Value::Null);
        assert_eq!(json["airplay"]["latency_msec"], 100);
        assert_eq!(json["airplay"]["auth_setup"], false);
        assert_eq!(json["airplay"]["prevent_takeover"], true);
        assert_eq!(json["airplay"]["port"], 5000);
        // The nested config must NOT carry the `kind` tag (that's flat-shape only).
        assert!(json["airplay"].get("kind").is_none());
    }

    #[test]
    fn source_view_rtp_shape() {
        let view = source_view(&rtp_entry(), false, &[]);
        assert_eq!(view.kind, SourceKind::Rtp);
        assert!(!view.present);
        assert_eq!(view.node_name, "rtp-in-garage-bridge");
        assert!(view.airplay.is_none());
        assert!(view.rtp.is_some());

        let json = serde_json::to_value(&view).unwrap();
        assert_eq!(json["kind"], "rtp");
        assert_eq!(json["present"], false);
        assert_eq!(json["airplay"], serde_json::Value::Null);
        assert_eq!(json["rtp"]["port"], 47000);
        assert_eq!(json["rtp"]["latency_msec"], 200);
        assert_eq!(json["rtp"]["source_addr"], "0.0.0.0");
        assert_eq!(json["rtp"]["ignore_ssrc"], true);
        assert_eq!(json["rtp"]["rate"], 48000);
        // No discovered bridges passed in → no diagnostics offer.
        assert_eq!(json["bridge"], serde_json::Value::Null);
    }

    /// A discovered bridge for `rtp_entry()`'s port (47000, unicast `0.0.0.0`).
    fn discovered_bridge(name: &str, port: u16, dest: &str, diag_ok: bool) -> crate::sources::bt_bridge::BtBridge {
        crate::sources::bt_bridge::BtBridge {
            fullname: format!("{name}._pwrouter-btbridge._tcp.local."),
            display_name: name.to_string(),
            hostname: "bridge.local.".into(),
            addr: Some(std::net::IpAddr::V4(std::net::Ipv4Addr::new(192, 168, 178, 78))),
            diag_port: 8080,
            diag_path: "/".into(),
            stream: crate::sources::bt_bridge::BridgeStream {
                rtp_port: port,
                rtp_dest: dest.into(),
                rate: 48_000,
                channels: 2,
                format: "S16LE".into(),
            },
            probe: Some((std::time::Instant::now(), diag_ok)),
        }
    }

    #[test]
    fn source_view_attaches_a_matching_bridge() {
        let bridges = vec![discovered_bridge("Bathroom", 47000, "239.255.42.42", true)];
        let view = source_view(&rtp_entry(), true, &bridges);
        let json = serde_json::to_value(&view).unwrap();
        assert_eq!(json["bridge"]["display_name"], "Bathroom");
        assert_eq!(json["bridge"]["rtp_port"], 47000);
        assert_eq!(json["bridge"]["hostname"], "bridge.local", "trailing mDNS dot trimmed for display");
        assert_eq!(json["bridge"]["diag_url"], "http://192.168.178.78:8080/");
        assert_eq!(json["bridge"]["diag_ok"], true);
    }

    #[test]
    fn source_view_reports_a_bridge_whose_page_is_down() {
        // Still matched (so the UI can name the sender) but not linkable: the
        // advert is installed by setup_pi_bridge.py and outlives the app.
        let bridges = vec![discovered_bridge("Bathroom", 47000, "0.0.0.0", false)];
        let view = source_view(&rtp_entry(), true, &bridges);
        assert_eq!(serde_json::to_value(&view).unwrap()["bridge"]["diag_ok"], false);
    }

    #[test]
    fn source_view_ignores_a_bridge_on_another_port() {
        let bridges = vec![discovered_bridge("Elsewhere", 46000, "0.0.0.0", true)];
        assert!(source_view(&rtp_entry(), true, &bridges).bridge.is_none());
    }

    #[test]
    fn airplay_sources_never_get_a_bridge() {
        let bridges = vec![discovered_bridge("Bathroom", 47000, "0.0.0.0", true)];
        assert!(source_view(&airplay_entry(), true, &bridges).bridge.is_none());
    }

    #[test]
    fn adopted_bridges_drop_out_of_the_discovered_offer() {
        let bridges = vec![discovered_bridge("Bathroom", 47000, "0.0.0.0", true), discovered_bridge("Garage", 46000, "0.0.0.0", true)];
        // rtp_entry() listens on 47000, so it claims "Bathroom".
        let sources = vec![source_view(&rtp_entry(), true, &bridges)];
        let offered = unmatched_bridges(&bridges, &sources);
        assert_eq!(
            offered.iter().map(|b| b.display_name.as_str()).collect::<Vec<_>>(),
            ["Garage"],
            "an already-configured bridge must not be offered again on a taken port"
        );
    }

    #[test]
    fn every_bridge_is_offered_when_nothing_is_configured() {
        let bridges = vec![discovered_bridge("Bathroom", 47000, "0.0.0.0", true)];
        assert_eq!(unmatched_bridges(&bridges, &[]).len(), 1);
    }

    #[test]
    fn source_view_uses_legacy_node_name() {
        // Legacy ids collapse to the bare node names so routing links resolve.
        let mut e = rtp_entry();
        e.id = LEGACY_RTP_ID.to_string();
        assert_eq!(source_view(&e, true, &[]).node_name, "bt-bridge-rtp");
    }

    #[test]
    fn create_request_config_defaults() {
        // `airplay`/`rtp` omitted → full defaults; a partial object fills the rest.
        let full_default: CreateSourceRequest = serde_json::from_str(r#"{"label":"X","kind":"airplay"}"#).unwrap();
        assert!(full_default.airplay.is_none()); // handler applies default
        assert_eq!(AirplaySourceConfig::default().latency_msec, DEFAULT_AIRPLAY_LATENCY_MSEC);

        let partial: CreateSourceRequest = serde_json::from_str(r#"{"label":"X","kind":"rtp","rtp":{"port":46000}}"#).unwrap();
        let rtp = partial.rtp.unwrap();
        assert_eq!(rtp.port, 46000);
        assert_eq!(rtp.latency_msec, DEFAULT_RTP_LATENCY_MSEC); // filled by serde default
        assert_eq!(rtp.rate, DEFAULT_RTP_RATE);

        // The omitted-object fallback the handler uses.
        assert_eq!(default_rtp_config().port, DEFAULT_RTP_PORT);
    }
}
