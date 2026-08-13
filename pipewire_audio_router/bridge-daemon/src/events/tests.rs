//! The events socket's protocol: topic names, the control messages, the frame shapes,
//! the dedupe rule and the metering hand-off.
//!
//! Everything here is the part that can be wrong without a daemon: a topic name that
//! does not round-trip, a frame whose payload key changed under a consumer, a dedupe
//! that suppresses the *first* frame, or an arm/disarm pairing that leaks a metering tap.

use super::*;

#[test]
fn topic_names_round_trip_and_unknown_ones_are_refused() {
    for t in Topic::ALL {
        let wire = serde_json::to_value(t).unwrap();
        let name = wire.as_str().expect("a topic serialises as a string");
        assert_eq!(Topic::parse(name), Some(t), "'{name}' must parse back to {t:?}");
    }
    // The names are the wire contract with two consumers, so they are pinned literally
    // rather than derived — a rename here is a rename there.
    assert_eq!(Topic::parse("now_playing"), Some(Topic::NowPlaying));
    assert_eq!(Topic::parse("nowPlaying"), None, "camelCase is not the wire form");
    assert_eq!(Topic::parse("routing"), None, "the old socket's name is not a topic");
    // `all` is handled by the caller (it expands), so the parser must not claim it.
    assert_eq!(Topic::parse("all"), None);
    assert_eq!(Topic::parse(""), None);
}

#[test]
fn the_control_protocol_is_op_plus_topics() {
    let sub: Control = serde_json::from_str(r#"{"op":"subscribe","topics":["matrix","meters"]}"#).unwrap();
    match sub {
        Control::Subscribe { topics } => assert_eq!(topics, vec!["matrix".to_string(), "meters".to_string()]),
        other => panic!("expected a subscribe, got {other:?}"),
    }
    let unsub: Control = serde_json::from_str(r#"{"op":"unsubscribe","topics":["meters"]}"#).unwrap();
    assert!(matches!(unsub, Control::Unsubscribe { .. }));
    // Anything else is ignored by the caller rather than fatal — killing the socket over
    // a typo would take every other topic down with it.
    assert!(serde_json::from_str::<Control>(r#"{"op":"resubscribe","topics":[]}"#).is_err());
    assert!(serde_json::from_str::<Control>(r#"{"topics":["matrix"]}"#).is_err());
}

/// The matrix frame historically *was* the whole frame, so `type` may be added beside
/// its fields but must never nest them; every other frame carries its payload under a
/// key a consumer reads by name.
#[test]
fn frames_are_tagged_and_the_matrix_stays_flat() {
    let matrix = crate::routing::RoutingMatrix { sources: Vec::new(), outputs: Vec::new(), links: Vec::new() };
    let json = serde_json::to_value(Frame::Matrix(&matrix)).unwrap();
    assert_eq!(json["type"], "matrix");
    for field in ["sources", "outputs", "links"] {
        assert!(json.get(field).is_some(), "'{field}' must stay at the top level: {json}");
    }
    assert!(json.get("matrix").is_none(), "the matrix must not be nested under a key");

    let empty: Vec<crate::outputs::listing::OutputInfo> = Vec::new();
    let agents: Vec<crate::outputs::pwsink::agent::AgentInfo> = Vec::new();
    for (frame, ty, key) in [
        (Frame::Outputs { outputs: &empty }, "outputs", "outputs"),
        (Frame::Discovered { outputs: &empty }, "discovered", "outputs"),
        (Frame::Agents { agents: &agents }, "agents", "agents"),
    ] {
        let json = serde_json::to_value(frame).unwrap();
        assert_eq!(json["type"], ty);
        assert!(json[key].is_array(), "{ty} carries its payload under '{key}': {json}");
    }

    // The three former sockets. Their payload is *wrapped* now — it used to be the whole
    // frame — so the key is part of the contract, not incidental.
    let align = crate::align::calibrate::AlignState::inactive();
    let json = serde_json::to_value(Frame::Align { state: &align }).unwrap();
    assert_eq!(json["type"], "align");
    assert_eq!(json["state"]["active"], false, "the whole session state rides under 'state': {json}");
    let measure = crate::align::measure::shared().status();
    let json = serde_json::to_value(Frame::Measure { status: &measure }).unwrap();
    assert_eq!(json["type"], "measure");
    assert!(json["status"].is_object(), "{json}");
    let eq = crate::align::measure::equivalence().status();
    let json = serde_json::to_value(Frame::Equivalence { status: &eq }).unwrap();
    assert_eq!(json["type"], "equivalence");
    assert!(json["status"].is_object(), "{json}");
}

/// The fast lane carries **only what moves**: a node with nothing to report is absent
/// (the client reads absent as zero), and none of the matrix's static payload rides
/// along. Both halves matter — this frame exists because the matrix was 2 210 bytes
/// carrying 36 bytes of peaks four times a second.
#[test]
fn the_meters_frame_carries_only_what_moves() {
    let nodes = vec!["airplay-in".to_string(), "ap2-dev-dusche".to_string()];
    let mut xruns = std::collections::HashMap::new();
    xruns.insert("airplay-in".to_string(), 7);
    let samples = crate::routing::build_meter_samples(&nodes, |n| if n == "airplay-in" { 0.5 } else { 0.0 }, &xruns);

    let json = serde_json::to_value(Frame::Meters { nodes: &samples }).unwrap();
    assert_eq!(json["type"], "meters");
    assert_eq!(json["nodes"]["airplay-in"]["peak"], 0.5);
    assert_eq!(json["nodes"]["airplay-in"]["xruns"], 7);
    assert!(json["nodes"].get("ap2-dev-dusche").is_none(), "a node with nothing to report must be left out: {json}");
    let frame = serde_json::to_string(&Frame::Meters { nodes: &samples }).unwrap();
    for leaked in ["display_name", "links", "latency_ms", "configured", "present"] {
        assert!(!frame.contains(leaked), "'{leaked}' must not be on the fast lane: {frame}");
    }

    // A silent house is an empty payload, not a payload of zeros.
    let quiet = crate::routing::build_meter_samples(&nodes, |_| 0.0, &std::collections::HashMap::new());
    assert!(quiet.is_empty());
    assert_eq!(serde_json::to_string(&Frame::Meters { nodes: &quiet }).unwrap(), r#"{"type":"meters","nodes":{}}"#);
}

/// Metadata is its own topic, keyed by source node name — deliberately *not* a field on
/// the matrix, which is large, mostly static and re-read in full by every consumer: a
/// new song must not cost a graph rebuild.
#[test]
fn the_now_playing_frame_is_separate_and_keyed_by_node_name() {
    let mut sources = BTreeMap::new();
    sources.insert(
        "airplay-in".to_string(),
        crate::sources::now_playing::NowPlaying {
            state: crate::sources::now_playing::PlaybackState::Playing,
            title: Some("Song".into()),
            artist: Some("Artist".into()),
            album: None,
            duration_ms: Some(200_000),
            position_ms: Some(1000),
            position_updated_at: Some(crate::sources::now_playing::UnixMillis(1_700_000_000_000)),
            artwork: None,
        },
    );
    let json = serde_json::to_value(Frame::NowPlaying { sources: &sources }).unwrap();
    assert_eq!(json["type"], "now_playing");
    assert_eq!(json["sources"]["airplay-in"]["title"], "Song");
    assert_eq!(json["sources"]["airplay-in"]["state"], "playing");
    assert!(json["sources"]["airplay-in"].get("album").is_none(), "absent fields are omitted, not null");
}

/// The subscribe acknowledgement exists so that a client naming a topic this daemon does
/// not have finds out immediately instead of waiting forever for a frame.
#[test]
fn the_subscribe_ack_names_what_was_not_understood() {
    let json = serde_json::to_value(Frame::Subscribed { topics: vec![Topic::Matrix], unknown: vec!["routing"] }).unwrap();
    assert_eq!(json["type"], "subscribed");
    assert_eq!(json["topics"], serde_json::json!(["matrix"]));
    assert_eq!(json["unknown"], serde_json::json!(["routing"]));
}

/// Rule 2: identical payloads send nothing. The first frame must always go, though —
/// a dedupe that starts "already sent" is a page that never renders.
#[test]
fn the_dedupe_suppresses_repeats_but_never_the_first_frame() {
    let mut s = Session::default();
    assert!(s.record(Topic::Matrix, "{a:1}"), "the first payload is always new");
    assert!(!s.record(Topic::Matrix, "{a:1}"), "an identical payload sends nothing");
    assert!(s.record(Topic::Matrix, "{a:2}"), "a changed payload sends");
    // Per topic, not per socket: two topics never mask each other.
    assert!(s.record(Topic::Meters, "{a:2}"));

    // Unsubscribing forgets it, so re-subscribing resends the current state even if it
    // has not changed since — which is what makes a subscription behave like a connect.
    s.on.insert(Topic::Matrix);
    s.sent.remove(&Topic::Matrix);
    assert!(s.record(Topic::Matrix, "{a:2}"), "a fresh subscription must not be deduped away");
}

#[test]
fn the_listing_topics_are_the_ones_rebuilt_together() {
    let mut s = Session::default();
    assert!(!s.wants_any_listing());
    s.on.insert(Topic::Align);
    assert!(!s.wants_any_listing(), "align is not a listing");
    s.on.insert(Topic::NowPlaying);
    assert!(s.wants_any_listing());
    for t in [Topic::Outputs, Topic::Discovered, Topic::Agents, Topic::NowPlaying] {
        assert!(t.is_listing(), "{t:?}");
    }
    for t in [Topic::Matrix, Topic::Meters, Topic::Align, Topic::Measure, Topic::Equivalence] {
        assert!(!t.is_listing(), "{t:?}");
    }
}

/// Metering and the profiler are armed by the **first** subscriber and disarmed by the
/// **last**, and the pairing has to survive a socket that subscribes twice or drops
/// without unsubscribing — the two ways a tap gets leaked or a profiler left armed on a
/// 4-core Pi.
#[test]
fn metering_is_armed_by_the_first_subscriber_and_disarmed_by_the_last() {
    let meters = crate::pw::metering::MeterHub::new();
    let watchers = std::sync::atomic::AtomicUsize::new(0);
    // Every profiling toggle the refcount decided to send, in order.
    let toggles = std::cell::RefCell::new(Vec::new());
    let record = |on: bool| toggles.borrow_mut().push(on);

    arm(&meters, &watchers, record, true);
    assert_eq!(*toggles.borrow(), vec![true], "the first subscriber arms the profiler");
    arm(&meters, &watchers, record, true); // a second socket
    assert_eq!(*toggles.borrow(), vec![true], "the second must not re-arm it");
    arm(&meters, &watchers, record, false);
    assert_eq!(*toggles.borrow(), vec![true], "…and must not disarm it while one is left");
    arm(&meters, &watchers, record, false);
    assert_eq!(*toggles.borrow(), vec![true, false], "the last one out disarms it");
    assert_eq!(watchers.load(Ordering::SeqCst), 0, "and the count comes back to zero");
}

/// The `metering` flag is what keeps that pairing honest per socket: subscribing to
/// `meters` twice must arm once, and a disconnect must disarm exactly what it armed.
#[test]
fn one_socket_arms_metering_at_most_once() {
    let mut s = Session::default();
    assert!(!s.metering);
    assert!(!std::mem::replace(&mut s.metering, true), "first subscribe: arms");
    assert!(std::mem::replace(&mut s.metering, true), "second subscribe: already armed");
    assert!(std::mem::replace(&mut s.metering, false), "unsubscribe: disarms");
    assert!(!std::mem::replace(&mut s.metering, false), "teardown after unsubscribe: nothing to do");
}
