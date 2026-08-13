//! Tests for group layout: anchors, writers, identity and reconciliation.

use super::*;
// Named explicitly rather than picked up from `super::*`: the reconciler itself now
// addresses these kinds through `OutputKind`, so its imports no longer re-export the
// raw prefixes these assertions compare against.
use crate::util::node_names::{PWSINK_DEV_PREFIX, SENDSPIN_DEV_PREFIX};

/// A running group with the given members and no live handles — enough to
/// exercise the "is something streaming this output?" bookkeeping.
fn running_group(sendspin: &[&str], ap2: &[&str], pwsink: &[&str]) -> RunningGroup {
    RunningGroup {
        anchor_node_name: "sync-grp-test".into(),
        anchor_node_id: 1,
        port: GROUP_BASE_PORT,
        server: None,
        server_devices: sendspin.iter().map(|s| s.to_string()).collect(),
        server_members: Vec::new(),
        server_codec: "pcm",
        server_send_ahead_us: 0,
        force_device_reconnect: BTreeSet::new(),
        ap2_sender: None,
        ap2_members: ap2.iter().map(|s| s.to_string()).collect(),
        ap2_rate: 48_000,
        pwsink_server: None,
        pwsink_members: pwsink.iter().map(|s| s.to_string()).collect(),
        pwsink_ports: Vec::new(),
    }
}

/// The rule the 2026-07-28 churn report came down to: adding or removing a
/// speaker must not restart the group's server, because a restart costs every
/// *other* member a full reconnect and a re-anchored stream.
#[test]
fn membership_alone_does_not_restart_the_sendspin_server() {
    let running = |config_changed| sendspin_server_action(SendspinServerState { routed: true, have_server: true, config_changed });
    // A join or a departure changes neither the codec nor the send-ahead, so the
    // stream config is unchanged — and the server keeps running.
    assert_eq!(running(false), ServerAction::KeepRunning);
    // A codec or send-ahead change is a genuinely different `stream/start`; the
    // shared timeline fixes both at construction, so this one has to restart.
    assert_eq!(running(true), ServerAction::Start);
    // (There is no third case any more: a static-delay edit used to force a
    // whole-group restart from here, and now scopes itself to the one device —
    // see `a_static_delay_change_within_the_running_lead_touches_only_that_device`.)
}

// --- the alignment hold (align/group.rs, plan §12.1) ---

fn link(source: &str, output: &str) -> RoutingLink {
    RoutingLink { source: source.to_string(), output: output.to_string() }
}

fn held(outputs: &[&str]) -> BTreeSet<String> {
    outputs.iter().map(|s| s.to_string()).collect()
}

/// No hold ⇒ the reconciler sees exactly the stored intent. This is the property
/// that makes the whole mechanism low-risk: the ordinary path is untouched.
#[test]
fn without_a_hold_the_intent_passes_through_unchanged() {
    let r = GroupReconciler::new();
    let intent = vec![link("airplay-in", "sendspin-dev-kitchen"), link("bt-rtp", "ap2-dev-dusche")];
    assert_eq!(r.effective_intent(intent.clone()), intent);
    assert!(r.align_hold_outputs().is_empty());
}

/// A hold forms a group around an arbitrary selection: the held outputs are cut
/// out of whatever was feeding them and given one source set of their own.
#[test]
fn a_hold_displaces_the_held_outputs_and_gives_them_one_group() {
    let mut r = GroupReconciler::new();
    let intent = vec![
        link("airplay-in", "sendspin-dev-kitchen"),
        link("airplay-in", "sendspin-dev-bath"), // same group as kitchen today
        link("bt-rtp", "ap2-dev-dusche"),        // a different group
        link("bt-rtp", "sendspin-dev-office"),   // not selected: must be untouched
    ];
    // A selection spanning two existing groups and leaving members behind — the
    // case an existing source set could never express.
    r.set_align_hold(1, held(&["sendspin-dev-kitchen", "ap2-dev-dusche"]));
    let eff = r.effective_intent(intent.clone());

    // Nothing feeds a held output but the synthetic source...
    for output in ["sendspin-dev-kitchen", "ap2-dev-dusche"] {
        let sources = routing::source_set_of(&eff, output);
        assert_eq!(sources.into_iter().collect::<Vec<_>>(), vec![crate::align::group::ALIGN_HOLD_SOURCE], "{output}");
    }
    // ...and the outputs left behind keep exactly the routing they had.
    assert_eq!(routing::source_set_of(&eff, "sendspin-dev-bath"), routing::source_set_of(&intent, "sendspin-dev-bath"));
    assert_eq!(routing::source_set_of(&eff, "sendspin-dev-office"), routing::source_set_of(&intent, "sendspin-dev-office"));

    // And that source set materialises as ONE group holding exactly the selection.
    let devices: BTreeMap<String, SendspinDevice> = [(
        "sendspin-dev-kitchen".to_string(),
        SendspinDevice {
            fullname: "kitchen._sendspin._tcp.local.".into(),
            display_name: "Kitchen".into(),
            addr: None,
            present: true,
            url: Some("ws://k".into()),
            supported_codecs: Vec::new(),
            min_buffer_ms: None,
            required_lead_time_ms: None,
            out_of_sync: false,
            sync_error_count: 0,
        },
    )]
    .into_iter()
    .collect();
    let ap2: BTreeMap<String, crate::outputs::ap2::discovery::Ap2Device> = [(
        "ap2-dev-dusche".to_string(),
        crate::outputs::ap2::discovery::Ap2Device {
            fullname: "Dusche._airplay._tcp.local.".into(),
            display_name: "Dusche".into(),
            model: None,
            features: None,
            addr: Some("10.0.0.5:7000".parse().unwrap()),
            present: true,
        },
    )]
    .into_iter()
    .collect();
    let desired = compute_desired(&eff, &devices, &ap2, &BTreeMap::new(), &PwsinkHosts::new());
    let align = desired.get(crate::align::group::ALIGN_HOLD_SOURCE).expect("the held group exists");
    assert_eq!(align.sendspin_node_names, vec!["sendspin-dev-kitchen".to_string()]);
    assert_eq!(align.ap2_members.iter().map(|(n, _, _)| n.as_str()).collect::<Vec<_>>(), vec!["ap2-dev-dusche"]);
    // The kitchen speaker is in NO other group, which is what "exclusive" means
    // for music: its old group can't reach it.
    for (key, g) in &desired {
        if key != crate::align::group::ALIGN_HOLD_SOURCE {
            assert!(!g.sendspin_node_names.contains(&"sendspin-dev-kitchen".to_string()), "still in group {key}");
        }
    }
}

/// Restore is "stop overriding", so it has to be *exactly* the identity again —
/// the stored intent was never edited, so there is nothing to replay.
#[test]
fn clearing_the_hold_restores_the_intent_exactly() {
    let mut r = GroupReconciler::new();
    let intent = vec![link("airplay-in", "sendspin-dev-kitchen"), link("bt-rtp", "ap2-dev-dusche")];
    r.set_align_hold(1, held(&["sendspin-dev-kitchen"]));
    assert_ne!(r.effective_intent(intent.clone()), intent);
    r.clear_align_hold(1);
    assert_eq!(r.effective_intent(intent.clone()), intent);
    assert!(r.align_hold_outputs().is_empty());
    // Idempotent: every teardown path may call it, including twice.
    r.clear_align_hold(1);
    assert_eq!(r.effective_intent(intent.clone()), intent);
}

#[test]
fn clearing_is_id_guarded_and_an_empty_set_is_no_hold() {
    let mut r = GroupReconciler::new();
    r.set_align_hold(1, held(&["sendspin-dev-kitchen"]));
    r.set_align_hold(2, held(&["ap2-dev-dusche"]));
    // The first session's safety timeout firing late must not free the second's
    // speakers while it is still measuring them.
    r.clear_align_hold(1);
    assert_eq!(r.align_hold_outputs(), held(&["ap2-dev-dusche"]));
    r.clear_align_hold(2);
    assert!(r.align_hold_outputs().is_empty());
    r.set_align_hold(3, BTreeSet::new());
    assert!(r.align_hold_outputs().is_empty(), "holding nothing is not a hold");
}

#[test]
fn the_server_follows_whether_anything_is_routed() {
    let state = |routed, have_server| SendspinServerState { routed, have_server, config_changed: false };
    // First device routed here ⇒ stand a server up.
    assert_eq!(sendspin_server_action(state(true, false)), ServerAction::Start);
    // Last device unrouted ⇒ take it down (and release its port + advert).
    assert_eq!(sendspin_server_action(state(false, true)), ServerAction::Stop);
    // Nothing either way ⇒ nothing to do; a group can be AP2-only.
    assert_eq!(sendspin_server_action(state(false, false)), ServerAction::Idle);
}

/// A two-member Opus group's send-ahead, from the real resolver: `bath` reports a
/// 300 ms buffer requirement and sets the group's floor, `kitchen` only 100 ms.
/// Both members' static delays are inputs, because the spec makes a player's
/// send-ahead `min_buffer_ms + static_delay_ms` and a group's the maximum of those
/// (see `sendspin::server::required_send_ahead_us`).
fn group_lead_us(kitchen_delay_ms: u16, bath_delay_ms: u16) -> i64 {
    sendspin::server::required_send_ahead_us(
        100_000, // the user's configured group lead
        "opus",
        crate::outputs::sendspin::codec::DEFAULT_OPUS_FLOOR_MS,
        // Both report, so the Opus floor above is bypassed either way.
        [(Some(100), kitchen_delay_ms), (Some(300), bath_delay_ms)],
    )
}

/// §4.10, the half that fixes the bug: editing ONE speaker's static delay must not
/// restart the group's server, because that drops every member — 219 ms of daemon
/// work, then tens of seconds of firmware-side silence per speaker (§4.9) for a
/// one-device calibration tweak.
#[test]
fn a_static_delay_change_within_the_running_lead_touches_only_that_device() {
    let running_lead = group_lead_us(0, 0);
    assert_eq!(running_lead, 300_000, "bath's 300 ms requirement sets the group floor");

    // Giving kitchen 40 ms leaves its own requirement (100 + 40) well under bath's,
    // so the group's send-ahead — the one thing every member shares — is untouched.
    assert_eq!(group_lead_us(40, 0), running_lead);
    assert!(!sendspin_config_changed("opus", running_lead, "opus", group_lead_us(40, 0)));

    // A *reduction* can't move it either, because the running lead is a high-water
    // mark (§4.6): dropping bath to 0 lowers the requirement, and we keep the
    // larger lead rather than reconnect everyone to save latency.
    assert!(!sendspin_config_changed("opus", group_lead_us(0, 200), "opus", group_lead_us(0, 0)));

    // So the reconcile keeps the server exactly as it is...
    assert_eq!(
        sendspin_server_action(SendspinServerState { routed: true, have_server: true, config_changed: false }),
        ServerAction::KeepRunning
    );
    // ...and the edit is carried by a per-device reconnect instead.
    let mut r = GroupReconciler::new();
    r.running.insert("src".into(), running_group(&["sendspin-dev-kitchen", "sendspin-dev-bath"], &[], &[]));
    assert!(r.force_device_reconnect("sendspin-dev-kitchen"));
    let marked = &r.running["src"].force_device_reconnect;
    assert_eq!(marked.iter().map(String::as_str).collect::<Vec<_>>(), vec!["sendspin-dev-kitchen"], "bath must be left streaming");
}

/// §4.10's constraint: the group *lead* genuinely is group-wide, so the delay
/// change that raises it must still re-arm every member. This is the guard that
/// stops the scoping above from being applied too eagerly.
#[test]
fn a_static_delay_change_that_raises_the_group_lead_re_arms_every_member() {
    let running_lead = group_lead_us(0, 0);
    // 250 ms on kitchen puts its own requirement (100 + 250) above bath's 300, so
    // the high-water mark moves — and the send-ahead is fixed when the shared
    // timeline is constructed, so every member's timing changes with it.
    let raised = group_lead_us(250, 0);
    assert_eq!(raised, 350_000);
    assert!(sendspin_config_changed("opus", running_lead, "opus", raised));
    assert_eq!(sendspin_server_action(SendspinServerState { routed: true, have_server: true, config_changed: true }), ServerAction::Start);
}

/// The other half of that high-water rule, and the reason it needed one: with only
/// "a raise restarts", the lead could never come **down** without restarting the
/// add-on. On 2026-08-12 this instance ran at 930 ms — the leftover of one static-delay
/// nudge — while the configured requirement was 130 ms, and lowering the knob did
/// nothing at all. An explicit re-arm is what makes the knob two-way, and the
/// difference-check is what keeps it from reconnecting a group that already runs at the
/// value being asked for.
#[test]
fn a_lead_rearm_is_the_only_thing_that_lowers_a_running_send_ahead() {
    let running = 930_000;
    let wanted = 130_000;
    // The ordinary rule ignores a drop, on purpose (§4.6/§4.9).
    assert!(!sendspin_config_changed("opus", running, "opus", wanted));
    // An explicit re-arm honours it.
    assert!(sendspin_lead_rearm(true, running, wanted));
    // Without one, nothing changes — the flag is what carries the user's intent.
    assert!(!sendspin_lead_rearm(false, running, wanted));
    // A re-arm whose value matches what is already streaming must NOT reconnect
    // anyone: the API re-arms on every sync-settings write, including ones that don't
    // move this group's requirement at all.
    assert!(!sendspin_lead_rearm(true, running, running));
    // It works upward too, which costs nothing extra — that case restarts anyway.
    assert!(sendspin_lead_rearm(true, wanted, running));
}

/// `running_sendspin_leads` is the number the UI shows as "in force now", so it must
/// report only groups that really have a server streaming. A group with no server has no
/// lead in force, whatever its bookkeeping says.
#[test]
fn only_a_group_with_a_live_server_reports_a_running_lead() {
    let mut r = GroupReconciler::new();
    let mut g = running_group(&["sendspin-dev-kitchen"], &[], &[]);
    g.server_send_ahead_us = 930_000; // what it *was* started with
    r.running.insert("src".into(), g);
    // `server: None` (this group's server is stopped) ⇒ nothing is in force.
    assert!(r.running_sendspin_leads().is_empty());
    // And a re-arm reports how many servers it would restart — none here, so the API
    // can stay quiet about a cost nobody is paying.
    assert_eq!(r.rearm_lead(), 0);
}

/// The request is addressed to one device in one group: a delay edit must not
/// disturb a co-existing group (a different source-set, its own anchor and server),
/// and an unknown device must not silently mark anything.
#[test]
fn a_forced_reconnect_is_scoped_to_one_device_and_one_group() {
    let mut r = GroupReconciler::new();
    r.running.insert("radio".into(), running_group(&["sendspin-dev-kitchen"], &[], &[]));
    r.running.insert("tv".into(), running_group(&["sendspin-dev-office"], &["ap2-dev-dusche"], &[]));

    assert!(r.force_device_reconnect("sendspin-dev-office"));
    assert!(r.running["radio"].force_device_reconnect.is_empty(), "the other group must not be touched");
    assert_eq!(r.running["tv"].force_device_reconnect.len(), 1);

    // A device no running group has (offline, unrouted, or never discovered): the
    // delay is still persisted by the caller and applied on its next connect, so
    // there is nothing to mark and the caller is told so.
    assert!(!r.force_device_reconnect("sendspin-dev-nowhere"));
    assert_eq!(r.running["tv"].force_device_reconnect.len(), 1);
}

#[test]
fn announce_sink_names_are_distinct_from_outputs_and_anchors() {
    for (output, expected) in [("ap2-dev-dusche", "idle-dev-ap2-dusche"), ("pwsink-dev-office", "idle-dev-pwsink-office")] {
        let name = announce_sink_name(output);
        assert_eq!(name, expected);
        // Routing must never mistake it for an output or a sync anchor.
        assert!(!name.starts_with(AP2_DEV_PREFIX));
        assert!(!name.starts_with(SENDSPIN_DEV_PREFIX));
        assert!(!name.starts_with(PWSINK_DEV_PREFIX));
        assert!(!name.starts_with(SYNC_GRP_PREFIX));
    }
}

#[test]
fn has_live_sender_uses_membership_for_sendspin_and_real_state_for_dialed_backends() {
    let mut r = GroupReconciler::new();
    r.running
        .insert("src".into(), running_group(&["sendspin-dev-kitchen"], &["ap2-dev-dusche", "ap2-dev-pioneer"], &["pwsink-dev-office"]));
    let connected: HashSet<String> = ["ap2-dev-dusche".to_string()].into_iter().collect();
    assert!(r.has_live_sender("sendspin-dev-kitchen", &connected));
    assert!(r.has_live_sender("ap2-dev-dusche", &connected));
    // Routed (a dialed group member) but its session never came up: an overlay
    // dropped on it would go nowhere, so this must NOT read as live.
    assert!(!r.has_live_sender("ap2-dev-pioneer", &connected));
    // Same for pw-sink, whose handshake is receiver-initiated: group membership
    // means we advertise a session, not that anyone attached to it. (The global
    // liveness registry is empty in this test = nobody attached.)
    assert!(!r.has_live_sender("pwsink-dev-office", &connected));
    // Unrouted endpoints have no sender at all — the case that used to make
    // announcements silently disappear.
    assert!(!r.has_live_sender("ap2-dev-bad", &connected));
    assert!(!r.has_live_sender("pwsink-dev-bad", &connected));
}

#[test]
fn only_the_dialed_backends_get_an_on_demand_transport() {
    assert!(supports_on_demand_announce("ap2-dev-dusche"));
    assert!(supports_on_demand_announce("pwsink-dev-office"));
    assert!(!supports_on_demand_announce("sendspin-dev-kitchen"));
    assert!(!supports_on_demand_announce("some-local-sink"));
    // …and the kinds that don't must explain themselves rather than drop the clip.
    for (output, needle) in [("sendspin-dev-kitchen", "offline"), ("some-local-sink", "no per-device sender")] {
        let msg = no_transport_reason(output);
        assert!(msg.contains(needle), "{output}: {msg:?} lacks {needle:?}");
    }
}

/// A receiver host joins the group of whatever feeds it — keyed by the name its
/// **pairing** carries, obtained from the registry exactly as `reconcile` gets it.
///
/// This is the invariant whose absence made every pw-sink output silent: members
/// used to be built from `pw_target_discovery`, which keys hosts
/// `pwsink-dev-<host>`, while pairing, adoption, routing intent and the HA entity
/// all carry `pwsink-dev-<host>_<user>`. `source_set_of` was therefore asked about
/// a name no link could hold, no member was ever added, no session was ever
/// advertised, and the agent waited forever for one. Nothing covered
/// `compute_desired`, so nothing noticed.
#[test]
fn a_connected_receiver_host_joins_the_group_of_whatever_feeds_it() {
    use crate::outputs::pwsink::agent::{Agents, HelloClaim, PROTOCOL_VERSION};

    let path = std::env::temp_dir().join(format!("sync-group-agents-{}.json", std::process::id()));
    let mut agents = Agents::new(path.clone(), tokio::sync::broadcast::channel(1).0);
    let claim = || HelloClaim {
        protocol: PROTOCOL_VERSION,
        agent_version: "0.1.0 (test)",
        machine_id: "m1",
        hostname: "david-local",
        user: "david",
        token: None,
        pair_code: None,
    };
    // Pair, then reconnect with the token — only a welcomed connection is a target.
    agents.hello(claim(), tokio::sync::mpsc::channel(1).0);
    let paired = agents.approve("m1:david").expect("approve");
    agents.hello(HelloClaim { token: Some(&paired.token), ..claim() }, tokio::sync::mpsc::channel(1).0);

    let hosts = agents.connected_targets();
    assert_eq!(hosts.keys().collect::<Vec<_>>(), vec![&paired.node_name], "the registry's key is the pairing's node name");

    // Unrouted: a connected host on its own forms no group and gets no session.
    assert!(compute_desired(&[], &BTreeMap::new(), &BTreeMap::new(), &BTreeMap::new(), &hosts).is_empty());

    // Routed: it becomes a member, which is what starts the pw-sink sender.
    let intent = vec![RoutingLink { source: "bt-bridge-rtp".into(), output: paired.node_name.clone() }];
    let desired = compute_desired(&intent, &BTreeMap::new(), &BTreeMap::new(), &BTreeMap::new(), &hosts);
    assert_eq!(desired.len(), 1, "a routed host must form a group");
    assert_eq!(desired.values().next().unwrap().pwsink_members, vec![paired.node_name]);

    let _ = std::fs::remove_file(path);
}

#[test]
fn pwsink_ports_step_past_pairs_already_taken() {
    // Nothing taken → consecutive control/data pairs from the base.
    assert_eq!(next_free_pwsink_ports([], 2), vec![PWSINK_BASE_PORT, PWSINK_BASE_PORT + 2]);
    // A group's session holds the first pair (control + data) → step past both.
    assert_eq!(next_free_pwsink_ports([PWSINK_BASE_PORT], 1), vec![PWSINK_BASE_PORT + 2]);
    // An on-demand announce session's port is fed in the same way, so a group
    // starting afterwards can't be handed the port it's already bound to.
    assert_eq!(next_free_pwsink_ports([PWSINK_BASE_PORT, PWSINK_BASE_PORT + 2], 2), vec![PWSINK_BASE_PORT + 4, PWSINK_BASE_PORT + 6]);
}

#[test]
fn on_demand_transports_get_the_longer_stall_grace() {
    assert!(AnnounceTransport::Starting.is_on_demand());
    assert!(AnnounceTransport::Warm.is_on_demand());
    assert!(!AnnounceTransport::Live.is_on_demand());
    assert!(!AnnounceTransport::Unavailable("x".into()).is_on_demand());
}
