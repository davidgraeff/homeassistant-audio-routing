//! Tests for the temporary exclusive alignment group.

use super::*;

fn adopted(names: &[&str]) -> BTreeSet<String> {
    names.iter().map(|s| s.to_string()).collect()
}

/// Held-output labels for a test registry: the derived name, as an un-renamed
/// output resolves to.
fn labels(names: &[&str]) -> HoldLabels {
    names.iter().map(|n| (n.to_string(), crate::routing::output_display_name(n))).collect()
}

#[test]
fn a_selection_becomes_sorted_members_with_their_knob_kinds() {
    let all = adopted(&["sendspin-dev-kitchen", "ap2-dev-dusche"]);
    // Deliberately unsorted and with a duplicate: the caller is a UI.
    let sel = vec!["sendspin-dev-kitchen".into(), "ap2-dev-dusche".into(), "sendspin-dev-kitchen".into()];
    let members = validate_selection(&sel, &all).unwrap();
    assert_eq!(
        members.iter().map(|m| (m.node_name.as_str(), m.kind)).collect::<Vec<_>>(),
        vec![("ap2-dev-dusche", MemberKind::Airplay2), ("sendspin-dev-kitchen", MemberKind::Sendspin)]
    );
}

#[test]
fn a_selection_is_independent_of_routing() {
    // No routing is consulted at all: the whole point of W10 is that the group is
    // formed from the *selection*, not from a source set that must already exist.
    let all = adopted(&["sendspin-dev-a", "sendspin-dev-b"]);
    let sel = vec!["sendspin-dev-a".into(), "sendspin-dev-b".into()];
    assert!(validate_selection(&sel, &all).is_ok());
}

#[test]
fn one_speaker_is_refused_with_the_reason() {
    let all = adopted(&["sendspin-dev-a"]);
    let err = validate_selection(&["sendspin-dev-a".into()], &all).unwrap_err();
    assert!(err.contains("at least two"), "{err}");
}

#[test]
fn an_unadopted_or_unalignable_output_is_named_in_the_refusal() {
    let all = adopted(&["sendspin-dev-a"]);
    let err = validate_selection(&["sendspin-dev-a".into(), "sendspin-dev-b".into()], &all).unwrap_err();
    assert!(err.contains("sendspin-dev-b") && err.contains("Outputs page"), "{err}");
    // A plain graph node is refused, with the reason.
    let all = adopted(&["sendspin-dev-a", "some-local-sink"]);
    let err = validate_selection(&["sendspin-dev-a".into(), "some-local-sink".into()], &all).unwrap_err();
    assert!(err.contains("not an alignable speaker"), "{err}");
}

/// W15: a pw-sink host is a member, not a refusal.
///
/// What such a member can be *levelled* to is deliberately **not** asserted here any
/// more: it is a per-output capability resolved in `calibrate` (`LevelChannel`), not a
/// property of the kind, so it is tested where it is decided (W20).
#[test]
fn a_pwsink_member_is_admitted_rather_than_refused() {
    let all = adopted(&["sendspin-dev-kitchen", "pwsink-dev-office"]);
    let members = validate_selection(&["sendspin-dev-kitchen".into(), "pwsink-dev-office".into()], &all).unwrap();
    assert_eq!(
        members.iter().map(|m| (m.node_name.as_str(), m.kind)).collect::<Vec<_>>(),
        vec![("pwsink-dev-office", MemberKind::PwSink), ("sendspin-dev-kitchen", MemberKind::Sendspin)]
    );
}

/// The §12.3.1 rule, on its own: same union or subset ⇒ no re-form.
#[test]
fn a_subset_or_the_same_union_never_re_forms_the_hold() {
    let held: BTreeSet<String> = adopted(&["sendspin-dev-a", "sendspin-dev-b", "ap2-dev-c"]);
    let all = vec!["sendspin-dev-a".to_string(), "sendspin-dev-b".to_string(), "ap2-dev-c".to_string()];
    // The same union, in another order and with a duplicate (the caller is a UI).
    let mut shuffled = vec!["ap2-dev-c".to_string(), "sendspin-dev-a".to_string(), "sendspin-dev-a".to_string()];
    shuffled.push("sendspin-dev-b".to_string());
    assert_eq!(plan_hold(Some(&held), &all), HoldPlan::Scope);
    assert_eq!(plan_hold(Some(&held), &shuffled), HoldPlan::Scope);
    // Any subset of ≥2 — this is what each position of a walk re-selects.
    assert_eq!(plan_hold(Some(&held), &all[..2]), HoldPlan::Scope);
    assert_eq!(plan_hold(Some(&held), &["ap2-dev-c".to_string(), "sendspin-dev-b".to_string()]), HoldPlan::Scope);

    // A speaker outside the union forms — and names the ones it has to add.
    let plan = plan_hold(Some(&held), &["sendspin-dev-a".to_string(), "sendspin-dev-d".to_string()]);
    let why = plan.form_reason().expect("a different union re-forms");
    assert!(why.contains("sendspin-dev-d") && why.contains("does not cover"), "{why}");
    // A superset forms too: growing a hold in place is deliberately not a thing.
    assert!(plan_hold(Some(&held), &[all[0].clone(), all[1].clone(), all[2].clone(), "sendspin-dev-d".to_string()])
        .form_reason()
        .is_some());
    // Nothing held, or fewer than two speakers ⇒ the formation path (which owns the
    // "at least two" refusal).
    assert!(plan_hold(None, &all).form_reason().is_some());
    assert!(plan_hold(Some(&held), &all[..1]).form_reason().unwrap().contains("fewer than two"));
}

#[test]
fn the_registry_only_records_violations_for_held_members() {
    let r = HoldRegistry::new();
    r.open(1, labels(&["sendspin-dev-a"]));
    assert!(r.is_reserved("sendspin-dev-a") && !r.is_reserved("sendspin-dev-b"));
    r.note("sendspin-dev-b", InterferenceCause::BargeIn { announcement: 5 });
    assert!(r.interference(1).is_empty(), "an output nobody is aligning is not our business");
    r.note("sendspin-dev-a", InterferenceCause::BargeIn { announcement: 5 });
    r.note("sendspin-dev-a", InterferenceCause::DuckHold { hold: 9 });
    let reports = r.interference(1);
    assert_eq!(reports.len(), 2);
    // The reason names the cause — NOT "hold the phone still" (plan §12.3).
    // ...and it names the speaker the way the user does ("a", from the node name,
    // since nothing renamed it) rather than quoting the node name at them.
    assert!(reports[0].reason.contains("announcement") && reports[0].reason.contains("'a'"), "{:?}", reports[0]);
    assert_eq!(reports[0].member, "sendspin-dev-a", "the node name is still carried structurally");
    assert_eq!(reports[0].member_label, "a");
    assert!(reports[1].reason.contains("voice assistant"), "{:?}", reports[1]);
}

/// The sentence the user reads names the speaker the way *they* do. It used to
/// quote the node name, so a refusal said `'sendspin-dev-kitchen'` while the chip
/// beside it in the UI said "Küche".
#[test]
fn an_interference_sentence_carries_the_users_name_for_the_speaker() {
    let path = std::env::temp_dir().join(format!("align-group-labels-{}.json", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let mut store = crate::store::outputs::OutputsStore::load(&path).unwrap();
    store.set_name("sendspin-dev-kitchen", Some("Küche")).unwrap();
    let outputs: crate::store::outputs::SharedOutputs = std::sync::Arc::new(Mutex::new(store));

    // The rename store is the source of truth; an output nobody renamed falls back
    // to the derived name, exactly as the Outputs page and the matrix resolve it.
    let resolved = resolve_labels(&outputs, &adopted(&["sendspin-dev-kitchen", "ap2-dev-bath"]));
    assert_eq!(resolved.get("sendspin-dev-kitchen").map(String::as_str), Some("Küche"));
    assert_eq!(resolved.get("ap2-dev-bath").map(String::as_str), Some("bath"));
    let _ = std::fs::remove_file(&path);

    let r = HoldRegistry::new();
    r.open(1, resolved);
    r.note("sendspin-dev-kitchen", InterferenceCause::BargeIn { announcement: 4 });
    r.note("ap2-dev-bath", InterferenceCause::DuckHold { hold: 9 });
    let reports = r.interference(1);
    assert!(reports[0].reason.contains("'Küche'"), "{}", reports[0].reason);
    assert!(!reports[0].reason.contains("sendspin-dev"), "no node name in the sentence: {}", reports[0].reason);
    assert_eq!(reports[0].member, "sendspin-dev-kitchen", "the node name is still carried, structurally");
    assert_eq!(reports[0].member_label, "Küche");
    assert!(reports[1].reason.contains("'bath'"), "{}", reports[1].reason);
}

#[test]
fn reports_are_drained_once_and_peeking_does_not_steal_them() {
    let r = HoldRegistry::new();
    r.open(1, labels(&["k"]));
    r.note("k", InterferenceCause::DuckHold { hold: 3 });
    assert_eq!(r.interference(1).len(), 1, "peek leaves it in place");
    assert_eq!(r.take_interference(1).len(), 1);
    assert!(r.take_interference(1).is_empty(), "each report is acted on exactly once");
    assert!(r.take_interference(2).is_empty(), "only the live holder may read its own reports");
}

#[test]
fn closing_is_id_guarded_so_a_late_teardown_cannot_clear_a_newer_hold() {
    let r = HoldRegistry::new();
    r.open(1, labels(&["k"]));
    r.open(2, labels(&["bath"]));
    r.close(1); // the previous session's timeout firing late
    assert_eq!(r.holder(), Some(2), "the newer hold survives");
    assert!(r.is_reserved("bath"));
    r.close(2);
    assert_eq!(r.holder(), None);
    assert!(r.reserved().is_empty());
    // Nothing held ⇒ nothing recorded, and no panic.
    r.note("bath", InterferenceCause::DuckHold { hold: 1 });
    assert!(r.take_interference(2).is_empty());
}

#[test]
fn the_interference_log_is_bounded() {
    let r = HoldRegistry::new();
    r.open(1, labels(&["k"]));
    for i in 0..(MAX_INTERFERENCE + 10) {
        r.note("k", InterferenceCause::BargeIn { announcement: i as u64 });
    }
    assert_eq!(r.interference(1).len(), MAX_INTERFERENCE);
}

/// Everything the hold takes, and gives back, in one test — deliberately **one**
/// test rather than several, because it drives the process-global registry,
/// announce arbiter and overlay mixer (the real ones, not mocks), and two tests
/// touching that single hold slot in parallel would fight over it.
///
/// The outputs are named so they cannot collide with any other test's.
#[tokio::test]
async fn a_hold_takes_exclusivity_reports_violations_and_gives_everything_back() {
    use crate::announce::arbiter::{Admission, OnBusy};
    let a = "sendspin-dev-holdtest-a".to_string();
    let b = "sendspin-dev-holdtest-b".to_string();
    let members = vec![
        AlignMember { node_name: a.clone(), kind: MemberKind::Sendspin, node_id: None },
        AlignMember { node_name: b.clone(), kind: MemberKind::Sendspin, node_id: None },
    ];
    let groups: SharedGroups = std::sync::Arc::new(tokio::sync::Mutex::new(crate::routing::sync_group::GroupReconciler::new()));
    let (changes, _rx) = tokio::sync::broadcast::channel(4);
    let displaced = vec![RoutingLink { source: "airplay-in".into(), output: a.clone() }];

    // `a` carries a user-chosen name, so the reports below can be checked to use it.
    let names: HoldLabels = [(a.clone(), "Küche".to_string())].into_iter().collect();
    let mut hold = ExclusiveHold::acquire(&groups, &changes, members, names, AlignMode::MultiPosition, displaced.clone()).await;

    // 1. The reconciler is displacing exactly the selection.
    assert_eq!(groups.lock().await.align_hold_outputs(), [a.clone(), b.clone()].into_iter().collect());
    assert_eq!(hold.outputs(), vec![a.clone(), b.clone()]);
    assert_eq!(hold.held(), &[a.clone(), b.clone()].into_iter().collect::<BTreeSet<String>>());
    assert_eq!(hold.label(&a), "Küche");
    assert_eq!(hold.label(&b), "holdtest b", "an un-renamed output falls back to the derived name");
    assert_eq!(hold.displaced(), displaced.as_slice());
    assert_eq!(hold.mode(), AlignMode::MultiPosition);
    assert!(registry().is_reserved(&a));
    // ...and the routing matrix's input says so, so both speakers' rows can explain
    // why they are silent (`RoutingNode::held`).
    let held_now = crate::routing::held_for_alignment();
    assert!(held_now.contains(&a) && held_now.contains(&b), "{held_now:?}");

    // 2. An ordinary announcement to a held speaker queues instead of playing.
    let announce = crate::announce::AnnounceCoordinator::global();
    let clip = vec![0u8; 8];
    let adm = announce.announce(vec![a.clone()], clip.clone(), 0.3, 0, OnBusy::Queue, false, None, Duration::from_secs(1));
    assert!(matches!(adm, Admission::Queued { .. }), "an ordinary announcement must not play over a calibration: {adm:?}");
    assert!(hold.interference().is_empty(), "queueing is not a violation");

    // 3. A barge-in wins, and the holder is told which member and why.
    let adm = announce.announce(vec![a.clone()], clip, 0.3, 100, OnBusy::Queue, true, None, Duration::from_secs(1));
    assert_eq!(adm, Admission::Playing, "a barge-in is never suppressed by a calibration");
    let reports = hold.interference();
    assert_eq!(reports.len(), 1, "{reports:?}");
    assert_eq!(reports[0].member, a);
    assert!(matches!(reports[0].cause, InterferenceCause::BargeIn { .. }));
    assert!(reports[0].reason.contains("'Küche'"), "the sentence names the speaker as the user does: {}", reports[0].reason);

    // 4. A voice-duck hold is the second interferer, and it is reported too — the
    //    one nothing in the announce path can see.
    crate::outputs::overlay_mixer::OverlayMixer::global().start_duck(std::slice::from_ref(&b), 0.2, Duration::from_secs(1));
    let reports = hold.take_interference();
    assert_eq!(reports.len(), 2, "{reports:?}");
    let duck = reports.iter().find(|r| matches!(r.cause, InterferenceCause::DuckHold { .. })).expect("duck reported");
    assert_eq!(duck.member, b);
    assert!(duck.reason.contains("voice assistant"), "{}", duck.reason);
    assert!(hold.take_interference().is_empty(), "drained");

    // 5. Release gives back all three pieces of state, and is idempotent.
    hold.release().await;
    assert!(groups.lock().await.align_hold_outputs().is_empty(), "routing override cleared");
    assert!(!registry().is_reserved(&a) && registry().holder().is_none());
    // The matrix input is clear too — the "held" badge on those rows is driven by this
    // set, so a hold that released while the page was open must not keep claiming them
    // (the notice this replaced had exactly that bug: it went on naming speakers the
    // idle timeout had already given back).
    let held_now = crate::routing::held_for_alignment();
    assert!(!held_now.contains(&a) && !held_now.contains(&b), "{held_now:?}");
    hold.release().await; // no panic, no double-release
                          // With the reservation gone, an announcement to those speakers plays again.
    let adm = announce.announce(vec![b.clone()], vec![0u8; 8], 0.3, 0, OnBusy::Queue, false, None, Duration::from_secs(1));
    assert_eq!(adm, Admission::Playing);
}

/// The restore obligation W17 adds: the calibration mutes this hold's audibility took
/// at the relay hook go back on **release** *and* on a bare `Drop`. Leaving a speaker
/// silent for as long as the daemon runs would be the worst outcome of a bug here — so
/// both paths are asserted, and so is the scoping that keeps a late teardown from
/// unmuting a *newer* session's member.
#[tokio::test]
async fn releasing_or_dropping_a_hold_drops_its_relay_calibration_mutes() {
    let relay = crate::align::relay_delay::RelayDelay::global();
    let groups: SharedGroups = std::sync::Arc::new(tokio::sync::Mutex::new(crate::routing::sync_group::GroupReconciler::new()));
    let (changes, _rx) = tokio::sync::broadcast::channel(4);
    let others = "pwsink-dev-someone-elses-mute";
    relay.set_muted(others, true);

    // The normal path.
    let all = adopted(&["sendspin-dev-relmute", "pwsink-dev-relmute"]);
    let members = validate_selection(&["sendspin-dev-relmute".into(), "pwsink-dev-relmute".into()], &all).unwrap();
    let mut hold = ExclusiveHold::for_test(&groups, &changes, members, HoldLabels::new(), AlignMode::MultiPosition).await;
    relay.set_muted("pwsink-dev-relmute", true);
    hold.release().await;
    assert!(!relay.is_muted("pwsink-dev-relmute"), "release gives audibility back");
    hold.release().await; // idempotent, and not a panic

    // The last resort: dropped without release.
    let all = adopted(&["sendspin-dev-dropmute", "pwsink-dev-dropmute"]);
    let members = validate_selection(&["sendspin-dev-dropmute".into(), "pwsink-dev-dropmute".into()], &all).unwrap();
    let hold = ExclusiveHold::for_test(&groups, &changes, members, HoldLabels::new(), AlignMode::MultiPosition).await;
    relay.set_muted("pwsink-dev-dropmute", true);
    drop(hold);
    assert!(!relay.is_muted("pwsink-dev-dropmute"), "even a hold dropped un-released must not leave a speaker silent");

    // Neither path touched an output it did not hold.
    assert!(relay.is_muted(others), "the clearing is scoped to the hold's own outputs");
    relay.unmute_all([others]);
}

#[test]
fn manual_mode_is_the_only_unmeasured_one() {
    assert!(AlignMode::MultiPosition.is_measured());
    assert!(AlignMode::NearField.is_measured());
    assert!(!AlignMode::Manual.is_measured());
    assert_eq!(AlignMode::default(), AlignMode::MultiPosition);
    assert_eq!(serde_json::to_string(&AlignMode::NearField).unwrap(), "\"near_field\"");
}

/// The promise can change without the group changing, so it must not cost a
/// re-form (a by-ear pass and a measured walk hold the same union).
#[tokio::test]
async fn the_mode_can_change_without_re_forming_the_hold() {
    let groups: SharedGroups = std::sync::Arc::new(tokio::sync::Mutex::new(crate::routing::sync_group::GroupReconciler::new()));
    let (changes, _rx) = tokio::sync::broadcast::channel(4);
    let all = adopted(&["sendspin-dev-modea", "sendspin-dev-modeb"]);
    let members = validate_selection(&["sendspin-dev-modea".into(), "sendspin-dev-modeb".into()], &all).unwrap();
    let mut hold = ExclusiveHold::for_test(&groups, &changes, members, HoldLabels::new(), AlignMode::Manual).await;
    let (id, anchor) = (hold.id(), hold.anchor_node_id());
    hold.set_mode(AlignMode::NearField);
    assert_eq!(hold.mode(), AlignMode::NearField);
    assert_eq!((hold.id(), hold.anchor_node_id()), (id, anchor), "same hold, same anchor");
    // Through the real decision path, not a predicate beside it.
    let subset = ["sendspin-dev-modeb".to_string(), "sendspin-dev-modea".to_string()];
    assert!(matches!(plan_hold(Some(hold.held()), &subset), HoldPlan::Scope), "a subset re-scopes, nothing reconnects");
    let outside = ["sendspin-dev-modea".to_string(), "sendspin-dev-elsewhere".to_string()];
    assert!(matches!(plan_hold(Some(hold.held()), &outside), HoldPlan::Form(_)), "a speaker outside the union re-forms");
    hold.release().await;
}
