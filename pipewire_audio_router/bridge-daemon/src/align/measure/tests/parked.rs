//! What a run owes the room when it stops driving it, and what it owes the *record*
//! afterwards.
//!
//! Two subjects, together because both are about the moment a run stops measuring:
//! the click track must fall silent when the run parks (and come back when it
//! resumes), and the transcript must survive the run whatever the run did — including
//! a run that never got a speaker rendering, which is the case this was built for.

use super::super::*;
use super::harness::*;

/// A parked run is one the user *reads*, not one they listen to. The hold and the
/// click track stay (apply measures through them); every member is muted.
#[tokio::test(start_paused = true)]
async fn parking_on_a_proposal_silences_the_members_and_resuming_makes_them_audible_again() {
    let rig = Rig::new(&[("early", 0.0), ("late", 9.0)], Mode::SweetSpot, 0.0);
    let (silenced, soloed) = (rig.silenced.clone(), rig.deps.session.clone());
    let inner = Arc::new(Mutex::new(Inner::idle()));
    let cancel = AtomicBool::new(false);
    let phase = run_measure(&rig.deps, &inner, &cancel, None).await.expect("a proposal");
    assert_eq!(phase, Phase::Proposed);
    assert_eq!(silenced.load(Ordering::Relaxed), 0, "the run itself must keep soloing while it measures");

    // `finish` is what parks the state machine, and the silencing belongs to it rather
    // than to the API handler — a closed tab must fall silent too.
    finish(&rig.deps, &inner, &cancel, Ok(phase)).await;
    assert_eq!(inner.lock_recover().phase, Phase::Proposed);
    assert_eq!(silenced.load(Ordering::Relaxed), 1, "nothing may still be audible while the proposal is being read");

    // Resuming needs no counterpart: the verification pass solos each member, which is
    // what makes it audible again.
    let proposal = inner.lock_recover().proposal.clone().expect("a proposal");
    let after = Rig::new(&[("early", 0.0), ("late", 0.0)], Mode::SweetSpot, 0.0);
    run_apply(&after.deps, &inner, &cancel, &proposal, None).await.expect("verified");
    assert!(after.silenced.load(Ordering::Relaxed) == 0 && soloed.snapshot().await.active);
    // …and the *applied* run parks silent in its turn.
    finish(&after.deps, &inner, &cancel, Ok(Phase::Done)).await;
    assert_eq!(after.silenced.load(Ordering::Relaxed), 1);
}

/// A refused run is parked too — arguably more so, since the user is reading a page
/// of numbers to work out what went wrong.
#[tokio::test(start_paused = true)]
async fn a_refused_run_falls_silent_as_well() {
    let rig = Rig::new(&[("a", 0.0), ("b", 5.0)], Mode::SweetSpot, 0.0);
    let inner = Arc::new(Mutex::new(Inner::idle()));
    let cancel = AtomicBool::new(false);
    finish(&rig.deps, &inner, &cancel, Err(Refusal::new(RefusalKind::GateTimeout, "no tone"))).await;
    assert_eq!(inner.lock_recover().phase, Phase::Refused);
    assert_eq!(rig.silenced.load(Ordering::Relaxed), 1);
}

/// Abandoning is the closed-tab case, and it is the one path where the run task will
/// never solo — or silence — anything again.
#[tokio::test]
async fn abandoning_a_run_silences_the_group_but_leaves_the_session_running() {
    let rig = Rig::new(&[("a", 0.0), ("b", 5.0)], Mode::SweetSpot, 0.0);
    let m = manager();
    m.inner.lock_recover().session = Some(rig.deps.session.clone());
    m.abandon().await;
    assert_eq!(rig.silenced.load(Ordering::Relaxed), 1);
    // The hold and the click track are the session's, and `abandon` does not stop it:
    // `revert` and the by-ear panel both still need it.
    assert!(rig.deps.session.snapshot().await.active);
}

/// The by-ear path's whole point is that two speakers are audible while the user
/// nudges one of them. Nothing here may silence that — and a manager that never ran
/// anything holds no session, which is what makes the rule structural rather than a
/// condition someone has to remember.
#[tokio::test]
async fn a_manual_session_is_never_silenced() {
    let rig = Rig::new(&[("a", 0.0), ("b", 5.0)], Mode::SweetSpot, 0.0);
    // Exactly what `AlignWizardManual` does: pick two, make them audible, listen.
    rig.deps.session.solo("a".to_string(), 50).await.expect("audible");
    let m = manager();
    m.abandon().await;
    assert_eq!(rig.silenced.load(Ordering::Relaxed), 0, "a by-ear session no run ever drove must be left exactly as the user set it up");
}

// ---- the transcript, through a whole run ---------------------------------

/// The reported case: "I needed to bring the speakers manually online, they didn't
/// work initially." The run cannot succeed, and what has to survive is *why*.
#[tokio::test(start_paused = true)]
async fn a_run_whose_speaker_never_rendered_leaves_a_transcript_that_says_so() {
    let (store, dir) = scratch_transcripts("never-online");
    let mut rig = Rig::new(&[("a", 0.0), ("offline-speaker", 5.0)], Mode::SweetSpot, 0.0);
    rig.deps.transcript = store.clone();
    // The speaker is configured, held, soloed — and renders nothing at all.
    rig.offline.lock_recover().push("offline-speaker".to_string());
    let m = manager();
    m.start(rig.deps).await.expect("the run starts: nothing here is knowable up front");
    let status = wait_for(&m, "a refusal", |s| s.phase == Phase::Refused).await;
    assert_eq!(status.refusal.as_ref().map(|r| r.kind), Some(RefusalKind::GateTimeout), "{}", status.message);

    let doc = store.list().first().and_then(|r| store.document(&r.id)).expect("a transcript for the refused run");
    let kinds: Vec<&str> = doc.events.iter().map(|e| e["kind"].as_str().unwrap_or("")).collect();
    assert_eq!(kinds.first(), Some(&"run_started"));
    assert!(kinds.contains(&"phase"), "the phases it got through: {kinds:?}");
    assert!(kinds.contains(&"gate_failed"), "the gate giving up is the whole finding: {kinds:?}");
    assert!(kinds.contains(&"refusal"));
    assert_eq!(kinds.last(), Some(&"run_finished"));
    // Named, and named as the speaker rather than as "a member".
    let failed = doc.events.iter().find(|e| e["kind"] == "gate_failed").unwrap();
    assert_eq!(failed["member"], "offline-speaker");
    // **A refusal carries its numbers.** It used to carry only the sentence, which left a
    // real 2026-08-13 refusal unanswerable afterwards: an unstable arrival cannot be told
    // from two arrivals swapping places without the estimate and the per-period series,
    // and those have opposite remedies (move the microphone vs suspect the speaker).
    let detail = &failed["detail"];
    assert!(detail["estimate"]["channels"].is_array(), "the estimate the gate judged: {detail}");
    assert!(detail["estimate"]["periods_seen"].is_number(), "{detail}");
    let series = detail["period_series"].as_array().expect("the per-period arrivals");
    assert_eq!(series.len(), 2, "one series per measurement channel: {series:?}");
    assert!(series.iter().all(|s| s["label"].is_string() && s["points"].is_array()));
    // The other speaker was measured, and its numbers are in the file — which is what
    // makes "one speaker was dead" distinguishable from "nothing worked".
    let measured: Vec<&serde_json::Value> = doc.events.iter().filter(|e| e["kind"] == "measurement").collect();
    assert!(!measured.is_empty(), "the healthy speaker's readings must still be recorded");
    assert_eq!(measured[0]["member"], "a");
    assert!(measured[0]["detail"]["observation"]["phase_a_ms"].is_number());
    assert!(measured[0]["detail"]["split_ms"].is_number(), "the cross-band split is the §5.6.1 data W22 has to read");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A successful run's transcript has to carry the decisions, not just the outcome:
/// the proposal, every write with the endpoint's **verbatim** reply, and the
/// verification.
#[tokio::test(start_paused = true)]
async fn a_successful_run_records_the_proposal_the_writes_and_the_verification() {
    let (store, dir) = scratch_transcripts("full-run");
    let mut rig = Rig::new(&[("early", 0.0), ("late", 7.0)], Mode::SweetSpot, 0.0);
    rig.deps.transcript = store.clone();
    let inner = Arc::new(Mutex::new(Inner::idle()));
    let cancel = AtomicBool::new(false);
    let log = store.begin();
    inner.lock_recover().log = log.clone();
    run_measure(&rig.deps, &inner, &cancel, None).await.expect("a proposal");
    let proposal = inner.lock_recover().proposal.clone().expect("a proposal");
    let mut after = Rig::new(&[("early", 0.0), ("late", 0.0)], Mode::SweetSpot, 0.0);
    after.deps.transcript = store.clone();
    run_apply(&after.deps, &inner, &cancel, &proposal, None).await.expect("verified");

    let doc = store.document(log.id()).expect("the transcript");
    let by_kind = |k: &str| doc.events.iter().filter(|e| e["kind"] == k).cloned().collect::<Vec<_>>();
    let props = by_kind("proposal");
    assert_eq!(props.len(), 1);
    assert_eq!(props[0]["detail"]["reference"], "early");
    let writes = by_kind("write");
    assert_eq!(writes.len(), 1, "only the member whose knob changed is written");
    assert_eq!(writes[0]["member"], "late");
    assert_eq!(writes[0]["detail"]["to_ms"], 7);
    // Verbatim, because whether the write forced a reconnect is in that sentence and
    // nowhere else (plan §2.3).
    assert!(writes[0]["message"].as_str().unwrap().contains("reconnecting just this speaker"), "{}", writes[0]["message"]);
    let verif = by_kind("verification");
    assert_eq!(verif.len(), 1);
    assert_eq!(verif[0]["detail"]["passed"], true);
    assert!(verif[0]["detail"]["transitivity"]["splits"].is_array(), "the per-member splits ride with the check");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Retention is applied when a run *begins*, so the directory never holds more than
/// the bound even while a run is live.
#[tokio::test]
async fn starting_runs_keeps_the_transcript_directory_bounded() {
    let (store, dir) = scratch_transcripts("retain-runs");
    for _ in 0..(crate::align::transcript::MAX_RUNS + 2) {
        let rig = Rig::new(&[("a", 0.0), ("b", 5.0)], Mode::SweetSpot, 0.0);
        let m = manager();
        let mut deps = rig.deps;
        deps.transcript = store.clone();
        m.start(deps).await.expect("starts");
        m.abandon().await;
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    assert_eq!(store.list().len(), crate::align::transcript::MAX_RUNS);
    let _ = std::fs::remove_dir_all(&dir);
}
