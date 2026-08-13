//! Session lifetime: forming and re-scoping a group, the idle watchdog, and what a
//! socket sees when it connects.

use super::super::*;
use super::harness::*;

/// The safety timeout must be an **idle** timeout, not a deadline from `start`.
///
/// A near-field walk round a large apartment legitimately outlasts
/// `SESSION_TIMEOUT`, and the one-shot `sleep(SESSION_TIMEOUT)` version tore the
/// session down mid-walk as a lost session — which made §1.2's advice to keep one
/// continuous session for everything that should be coherent impossible to honour.
/// Each arrival re-solos its speaker, so the walk refreshes this for free.
#[tokio::test]
async fn an_arrival_refreshes_the_idle_timeout() {
    let f = UnionFixture::new("idle", &[("sendspin-dev-idlea", MemberKind::Sendspin), ("sendspin-dev-idleb", MemberKind::Sendspin)]).await;

    // Stand in for a long walk: idle past the point a deadline-based watchdog fired.
    f.go_idle(SESSION_TIMEOUT * 2).await;
    assert!(f.idle().await.expect("session") >= SESSION_TIMEOUT, "precondition: looks abandoned");

    // Arriving at the next speaker is a solo, and that must postpone the teardown.
    f.mgr.solo("sendspin-dev-idleb".to_string(), 30).await.expect("solo");
    assert!(f.idle().await.expect("session") < SESSION_TIMEOUT, "an arrival must postpone the teardown");
}

#[test]
fn same_set_ignores_order_and_dupes_only_by_value() {
    assert!(same_set(&["a".into(), "b".into()], &["b".into(), "a".into()]));
    assert!(!same_set(&["a".into()], &["a".into(), "b".into()]));
}

/// The core claim of plan §12.3.1: re-selecting speakers that are already held is
/// **not** a restart. A re-form means a new group, a new anchor and new per-device
/// senders — every sendspin member reconnecting twice — so the assertion is on the
/// hold's *identity*, not on some visible side effect.
#[tokio::test]
async fn re_selecting_held_speakers_keeps_the_hold_its_anchor_and_its_senders() {
    let f = UnionFixture::new(
        "subset",
        &[
            ("sendspin-dev-uniona", MemberKind::Sendspin),
            ("sendspin-dev-unionb", MemberKind::Sendspin),
            ("ap2-dev-unionc", MemberKind::Airplay2),
        ],
    )
    .await;
    let before = f.identity().await.unwrap();
    assert_eq!(before, (f.hold_id, f.anchor));

    // A position that hears two of the three — and a different mode, which is a
    // property of the run rather than of the group.
    let state = f
        .mgr
        .start_outputs(&f.deps(), vec!["sendspin-dev-unionb".into(), "sendspin-dev-uniona".into()], AlignMode::NearField)
        .await
        .expect("a subset of the held union starts inside the hold");
    assert_eq!(f.identity().await, Some(before), "same hold, same anchor: nothing re-formed, nothing reconnected");
    assert_eq!(state.hold_id, f.hold_id);
    assert!(state.hold_reused);
    assert!(state.hold_cost.contains("no speaker reconnected"), "{}", state.hold_cost);
    assert_eq!(state.mode, AlignMode::NearField);
    // The hold is still the whole union; the *selection* is this position's.
    assert_eq!(state.outputs.len(), 3);
    // `start_outputs` normalises the key it echoes (sorted, deduped).
    assert_eq!(state.sources, vec!["sendspin-dev-uniona".to_string(), "sendspin-dev-unionb".to_string()]);
    assert_eq!(state.audible, vec!["sendspin-dev-uniona".to_string(), "sendspin-dev-unionb".to_string()]);
    assert_eq!(state.volume, 33, "a re-scope must not reset a tuned level");
    // …and it records that level against the members it just applied it to, so the map
    // and the speakers agree on every path that applies a level (W19).
    assert_eq!(state.levels.get("sendspin-dev-uniona"), Some(&33));
    assert_eq!(state.levels.get("sendspin-dev-unionb"), Some(&33));
    assert!(!state.levels.contains_key("ap2-dev-unionc"), "the member this position cannot hear was given nothing");
    assert_eq!(f.groups.lock().await.align_hold_outputs().len(), 3, "the routing override still covers the union");

    // The same union again, unsorted and with a duplicate (the caller is a UI).
    let state = f
        .mgr
        .start_outputs(
            &f.deps(),
            vec!["ap2-dev-unionc".into(), "sendspin-dev-uniona".into(), "ap2-dev-unionc".into(), "sendspin-dev-unionb".into()],
            AlignMode::MultiPosition,
        )
        .await
        .unwrap();
    assert_eq!(f.identity().await, Some(before), "the unchanged union does not re-form either");
    assert!(state.hold_reused);
    f.mgr.stop().await;
}

/// The other side of the same rule: a speaker the hold does not cover *must*
/// re-form, which tears the running session down. Here the re-form then fails on
/// the adoption gate (nothing is adopted in the fixture), which is exactly what
/// makes it observable without a graph — a reuse would have returned `Ok` and left
/// the session running.
#[tokio::test]
async fn a_genuinely_different_union_re_forms_and_gives_the_old_hold_back() {
    let f = UnionFixture::new("reform", &[("sendspin-dev-reforma", MemberKind::Sendspin), ("sendspin-dev-reformb", MemberKind::Sendspin)])
        .await;
    assert!(f.identity().await.is_some());
    let err = f
        .mgr
        .start_outputs(&f.deps(), vec!["sendspin-dev-reforma".into(), "sendspin-dev-elsewhere".into()], AlignMode::MultiPosition)
        .await
        .expect_err("the fixture adopts nothing, so the re-form is refused at the adoption gate");
    assert!(err.contains("Outputs page"), "{err}");
    assert!(f.identity().await.is_none(), "the previous session was torn down: this was a re-form, not a re-scope");
    assert!(f.groups.lock().await.align_hold_outputs().is_empty(), "and its routing override is gone");
}

// ---- The idle timeout, as something the user can see coming -------------------
//
// The hold is exclusive (plan §12.3), so when the idle timeout fires the speakers go
// back to normal and any wizard on screen is describing a session that no longer
// exists. A real multi-position run walked into exactly that, because reading a review
// page is *quiet*. These tests pin the two halves of the fix: the remaining time is in
// the status, and its disappearance is pushed.

/// `closes_in_s` has to be a live reading of the idleness the watchdog decides on —
/// shrinking while nothing happens and jumping back the moment something does.
///
/// Asserted through the public status rather than through `activity`, because the
/// number a user counts down and the number the teardown is decided on being the same
/// number is the actual claim (they are one function, `Session::idle`).
#[tokio::test]
async fn the_reported_remaining_time_shrinks_while_idle_and_jumps_back_on_activity() {
    let (a, b) = ("sendspin-dev-cdowna", "sendspin-dev-cdownb");
    let f = UnionFixture::new("closesin", &[(a, MemberKind::Sendspin), (b, MemberKind::Sendspin)]).await;

    let fresh = f.mgr.status().await.closes_in_s.expect("a live session says when it would close");
    assert!(fresh <= SESSION_TIMEOUT.as_secs(), "never more than the whole allowance, got {fresh}");
    assert!(fresh + 5 >= SESSION_TIMEOUT.as_secs(), "a fresh session has nearly all of it, got {fresh}");

    // Five minutes of a user reading a proposal: silent, so the clock runs down.
    f.go_idle(Duration::from_secs(5 * 60)).await;
    let idled = f.mgr.status().await.closes_in_s.unwrap();
    assert!(idled < fresh, "idling has to shrink it: {idled} vs {fresh}");
    assert!(idled.abs_diff(SESSION_TIMEOUT.as_secs() - 5 * 60) <= 5, "it counts the idleness, got {idled}");

    // Soloing a speaker is the audibility change §1.2 relies on a walk making.
    f.mgr.solo(b.to_string(), 30).await.expect("solo");
    let refreshed = f.mgr.status().await.closes_in_s.unwrap();
    assert!(refreshed > idled, "an audibility change must give the whole allowance back: {refreshed} vs {idled}");
    assert!(refreshed + 5 >= SESSION_TIMEOUT.as_secs(), "…all of it, got {refreshed}");

    // The two constants that let a client phrase it honestly: it may say "15 minutes
    // without a change", and it must say "about", because the watchdog only looks every
    // `TIMEOUT_POLL`.
    let st = f.mgr.status().await;
    assert_eq!(st.idle_timeout_s, SESSION_TIMEOUT.as_secs());
    assert_eq!(st.timeout_slack_s, TIMEOUT_POLL.as_secs());
    assert!(st.timeout_slack_s > 0, "a slack of zero would invite a UI to promise a precise second");

    // Past the deadline it saturates at zero rather than wrapping — and zero means
    // "awaiting teardown", not "gone": the session is still here, and still says so.
    f.go_idle(SESSION_TIMEOUT * 2).await;
    let st = f.mgr.status().await;
    assert_eq!(st.closes_in_s, Some(0));
    assert!(st.active, "the watchdog has not looked yet, so the session is still real");

    // No session, nothing counting down — but the rules are still stated.
    f.mgr.stop().await;
    let st = f.mgr.status().await;
    assert_eq!(st.closes_in_s, None, "an inactive state has no deadline to report");
    assert_eq!(st.idle_timeout_s, SESSION_TIMEOUT.as_secs());
    assert_eq!(AlignState::inactive().closes_in_s, None);
}

/// The by-ear path's two steps refresh the timer too.
///
/// `Session::activity`'s doc comment always claimed `select` and `set_level` did this
/// and only `set_audible` ever called `note_activity` — so a by-ear session being
/// compared pair by pair for a quarter of an hour was torn down as abandoned. The UI now
/// *tells* the user that changing what they hear or its level refreshes the timer, which
/// makes the discrepancy a promise rather than a doc bug.
#[tokio::test]
async fn the_by_ear_steps_refresh_the_idle_timeout_as_the_docs_always_claimed() {
    let (a, b) = ("sendspin-dev-byeara", "sendspin-dev-byearb");
    let f = UnionFixture::new("byearidle", &[(a, MemberKind::Sendspin), (b, MemberKind::Sendspin)]).await;

    // Picking a reference/target pair.
    f.go_idle(Duration::from_secs(10 * 60)).await;
    f.mgr.select(a.to_string(), b.to_string()).await.expect("select");
    assert!(f.idle().await.expect("session") < Duration::from_secs(60), "`select` refreshes the idle mark");

    // Dragging the level.
    f.go_idle(Duration::from_secs(10 * 60)).await;
    f.mgr.set_level(42).await.expect("set_level");
    assert!(f.idle().await.expect("session") < Duration::from_secs(60), "`set_level` refreshes the idle mark");

    // The explicit "I am still here", which is the only remedy on a page that is
    // otherwise silent by nature (a proposal being read).
    f.go_idle(Duration::from_secs(14 * 60)).await;
    let st = f.mgr.still_here().await.expect("a live session can be kept open");
    assert!(st.closes_in_s.unwrap() + 5 >= SESSION_TIMEOUT.as_secs(), "one whole fresh allowance, got {:?}", st.closes_in_s);

    // …and *parking the run* is not activity, on purpose: `silence()` is what a run does
    // when it stops playing to let the user read, which is exactly the case the countdown
    // exists for. If it refreshed the timer there would be nothing to warn about.
    f.go_idle(Duration::from_secs(10 * 60)).await;
    let before = f.mgr.status().await.closes_in_s.unwrap();
    f.mgr.silence().await.expect("silence");
    let after = f.mgr.status().await.closes_in_s.unwrap();
    assert!(after <= before, "parking a run must keep the watchdog counting: {after} vs {before}");

    f.mgr.stop().await;
    assert!(f.mgr.still_here().await.is_err(), "there is nothing to keep open once it has stopped");
}

/// The push half: every exit path bumps the notifier, so a client hears about the
/// teardown instead of noticing it at the next poll.
///
/// The notifier lives on the **manager** for exactly this reason — one owned by the
/// session would be dropped by the event it exists to report.
#[tokio::test]
async fn the_notifier_fires_on_teardown_so_the_disappearance_is_pushed() {
    let (a, b) = ("sendspin-dev-pusha", "sendspin-dev-pushb");
    let f = UnionFixture::new("pushstop", &[(a, MemberKind::Sendspin), (b, MemberKind::Sendspin)]).await;
    let mut rx = f.mgr.subscribe();
    assert!(!rx.has_changed().unwrap(), "a fresh subscription is up to date, not pre-armed");

    // An ordinary change first, so the test cannot pass by never distinguishing them.
    f.mgr.solo(b.to_string(), 25).await.expect("solo");
    assert!(rx.has_changed().unwrap(), "a change to a live session is pushed");
    rx.borrow_and_update();

    f.mgr.stop().await;
    assert!(rx.has_changed().unwrap(), "the teardown is pushed");
    rx.borrow_and_update();
    assert!(!f.mgr.status().await.active, "…and what a subscriber then reads is the inactive state");

    // The *other* teardown paths go through the same `teardown`, which is where the bump
    // is: a superseding start that re-forms (and here fails at the adoption gate, leaving
    // no session) has to reach a subscriber too.
    let f = UnionFixture::new("pushreform", &[(a, MemberKind::Sendspin), (b, MemberKind::Sendspin)]).await;
    let rx = f.mgr.subscribe();
    let _ = f
        .mgr
        .start_outputs(&f.deps(), vec![a.into(), "sendspin-dev-elsewhere".into()], AlignMode::MultiPosition)
        .await
        .expect_err("the fixture adopts nothing, so the re-form is refused");
    assert!(rx.has_changed().unwrap(), "a superseding start's teardown is pushed as well");
    assert!(!f.mgr.status().await.active);
}

/// The watchdog end to end: an idle session is taken, torn down, and the teardown is
/// pushed — the sequence a review page left open overnight actually produces.
///
/// Paused clock, so the 15 minutes cost nothing. This is also what pins the watchdog
/// being a *loop*: it sleeps in `TIMEOUT_POLL` slices and only fires once idleness has
/// really accumulated, so a session refreshed in between survives the same task.
#[tokio::test(start_paused = true)]
async fn an_idle_session_is_torn_down_by_its_watchdog_and_the_close_is_pushed() {
    let (a, b) = ("sendspin-dev-wdoga", "sendspin-dev-wdogb");
    let f = UnionFixture::new("watchdog", &[(a, MemberKind::Sendspin), (b, MemberKind::Sendspin)]).await;
    let mut rx = f.mgr.subscribe();
    let stop = f.mgr.session.lock().await.as_ref().expect("session").stop.clone();
    f.mgr.arm_timeout(stop);

    // One slice in, with the user still working: it must not fire.
    f.mgr.solo(b.to_string(), 20).await.expect("solo");
    rx.borrow_and_update();
    tokio::time::sleep(TIMEOUT_POLL * 2).await;
    assert!(f.mgr.status().await.active, "a session in use survives the watchdog's checks");

    // Now stop touching it. Rather than sleeping fifteen paused minutes with the session
    // lock changing hands, backdate the mark the watchdog reads — which is precisely what
    // a quarter of an hour of reading does to it.
    f.go_idle(SESSION_TIMEOUT).await;
    tokio::time::timeout(SESSION_TIMEOUT, rx.changed()).await.expect("the teardown is pushed").expect("notifier alive");

    let st = f.mgr.status().await;
    assert!(!st.active, "the watchdog gave the speakers back");
    assert_eq!(st.closes_in_s, None);
    assert!(f.groups.lock().await.align_hold_outputs().is_empty(), "…and released the exclusive hold with them");
}

/// Plan §11's requirement for the session status: **one full state on demand, and a
/// frame for every change including the last one** — a client must need no separate
/// initial fetch, and must be *told* when the session ends rather than discovering it.
///
/// The transport that carries this is now the one events socket (`events.rs`, one
/// connection for every topic); what belongs here is the half that is this module's:
/// `status()` answers in full at any moment, and `subscribe()` bumps on every change up
/// to and including the teardown. A version that only bumped on *some* changes would
/// leave a wizard describing a session that no longer exists.
#[tokio::test]
async fn the_session_answers_in_full_and_announces_every_change_including_its_end() {
    let (a, b) = ("sendspin-dev-wsa", "sendspin-dev-wsb");
    let f = UnionFixture::new("sessionws", &[(a, MemberKind::Sendspin), (b, MemberKind::Sendspin)]).await;
    f.mgr.solo(b.to_string(), 31).await.expect("solo");
    // Subscribed *after* that solo: what a client gets first is the state as it stands,
    // not a replay of how it got there.
    let mut changes = f.mgr.subscribe();

    let first = serde_json::to_value(f.mgr.status().await).expect("the state serialises");
    assert_eq!(first["active"], serde_json::json!(true), "the status describes the session that is already running");
    assert_eq!(first["audible"], serde_json::json!([b]), "…including where the run currently is");
    assert_eq!(first["volume"], serde_json::json!(31));
    assert!(first["closes_in_s"].as_u64().is_some(), "and when it would close: {}", first["closes_in_s"]);
    assert_eq!(first["timeout_slack_s"], serde_json::json!(TIMEOUT_POLL.as_secs()));
    // Full state, not a delta: a client must not need a separate initial fetch.
    for field in ["members", "outputs", "levels", "level_channels", "hold_id", "displaced"] {
        assert!(!first[field].is_null(), "the status is a whole state; '{field}' is missing");
    }

    // Then one bump per change …
    f.mgr.solo(a.to_string(), 44).await.expect("solo");
    changes.changed().await.expect("a change is announced");
    assert_eq!(serde_json::to_value(f.mgr.status().await).unwrap()["audible"], serde_json::json!([a]));

    // … ending with the one that says it is over, which is the frame the socket exists for.
    f.mgr.stop().await;
    changes.changed().await.expect("the teardown is announced too");
    let closed = serde_json::to_value(f.mgr.status().await).unwrap();
    assert_eq!(closed["active"], serde_json::json!(false));
    assert_eq!(closed["closes_in_s"], serde_json::Value::Null);
}
