//! Tests for route-table shape: the paths and methods the router actually exposes.

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

/// The alignment measurement's own siblings under one prefix: `split` and `log` are
/// static segments beside `apply`/`revert`/`start`, and `split/{node_name}` adds a
/// parameter one level deeper. Same trap as the two above — a conflict panics at
/// daemon startup, where nothing would catch it.
#[test]
fn the_measure_split_and_log_routes_coexist_with_the_rest() {
    let app: Router = Router::new()
        .route("/api/align/measure", get(|| async { "status" }).delete(|| async { "abandoned" }))
        .route("/api/align/measure/start", post(|| async { "started" }))
        .route("/api/align/measure/apply", post(|| async { "applied" }))
        .route("/api/align/measure/split", get(|| async { "listed" }).post(|| async { "calibrated" }))
        .route("/api/align/measure/split/{node_name}", delete(|| async { "cleared" }))
        .route("/api/align/measure/log", get(|| async { "transcripts" }));
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

/// The per-output knobs are parameterised siblings of the *static* `discovered` listing
/// and of each other, and axum panics on a genuine conflict at build time — i.e. at
/// daemon startup, where nothing would catch it. Registering the whole family must be
/// accepted.
#[test]
fn the_per_output_knob_routes_coexist_with_the_static_siblings() {
    let app: Router = Router::new()
        .route("/api/outputs", get(|| async { "all" }))
        .route("/api/outputs/discovered", get(|| async { "offered" }))
        .route("/api/outputs/{node_name}", delete(|| async { "removed" }))
        .route("/api/outputs/{node_name}/volume", put(|| async { "level" }))
        .route("/api/outputs/{node_name}/mute", put(|| async { "mute" }))
        .route("/api/outputs/{node_name}/delay", put(|| async { "delay" }))
        .route("/api/outputs/{node_name}/resync", post(|| async { "resync" }))
        .route("/api/outputs/{node_name}/adopt", post(|| async { "added" }));
    drop(app);
}

/// A sender is addressed by its key one level below the listing, so `{key}` must not
/// conflict with the collection above it — nor its own sub-paths with the key itself.
#[test]
fn the_client_key_routes_nest_under_the_listing() {
    let app: Router = Router::new()
        .route("/api/sources/{id}/clients", get(|| async { "list" }))
        .route("/api/sources/{id}/clients/{key}", delete(|| async { "forget" }))
        .route("/api/sources/{id}/clients/{key}/ban", put(|| async { "ban" }))
        .route("/api/sources/{id}/clients/{key}/priority", put(|| async { "priority" }))
        .route("/api/sources/{id}/clients/{key}/disconnect", post(|| async { "kick" }));
    drop(app);
}

/// Two alignment paths where a *node name* and a bare collection share a prefix: the
/// band-split listing beside one output's calibration, and the equivalence experiment
/// with or without a named member.
#[test]
fn the_align_member_paths_coexist_with_their_collections() {
    let app: Router = Router::new()
        .route("/api/align/measure/split", get(|| async { "all" }))
        .route("/api/align/measure/split/{node_name}", post(|| async { "one" }).delete(|| async { "clear" }))
        .route("/api/align/measure/arrival/{node_name}", post(|| async { "here" }))
        .route("/api/align/equivalence", get(|| async { "status" }).post(|| async { "auto" }))
        .route("/api/align/equivalence/{node_name}", post(|| async { "member" }))
        .route("/api/align/members/{node_name}/channel", post(|| async { "channel" }));
    drop(app);
}

/// The convention, through a real handler rather than the type alone: a name whose kind
/// has no such knob is a typed `bad_request`, an unknown name a `not_found`, and neither
/// is a 200 with a flag saying otherwise.
///
/// `level::kind_of` is where every per-output handler starts, so this pins the two
/// refusals every one of them can produce without needing an `AppState`.
#[test]
fn a_per_output_write_refuses_by_kind_and_says_which() {
    let unknown = level::kind_of("what-even-is-this").expect_err("not an output");
    assert_eq!(unknown.kind, error::ErrorKind::NotFound);
    assert!(unknown.message.contains("not an output this daemon drives"), "{}", unknown.message);

    // And the three real kinds resolve, so the dispatch below them is reachable.
    for name in ["sendspin-dev-kitchen", "ap2-dev-dusche", "pwsink-dev-desk"] {
        assert!(level::kind_of(name).is_ok(), "{name}");
    }
}
