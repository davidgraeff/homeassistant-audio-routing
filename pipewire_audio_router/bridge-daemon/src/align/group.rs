//! Temporary **exclusive** speaker group for alignment
//! (docs/mic-alignment-plan.md §12.1, §12.3).
//!
//! Alignment used to resolve a group from an existing *source set* and required one to
//! already exist (`align/calibrate/mod.rs`), which is why its panel lived on source cards. The
//! target scenario inverts that: the user picks speakers on the Outputs page and
//! alignment forms a group around them.
//!
//! "Exclusive" is the load-bearing word. While a group is held for alignment, no other
//! audio may reach its members — not another music source, not an announcement, not a
//! ducking hold. Group membership alone does **not** achieve that (plan §12.3 records
//! why: the announce arbiter's occupancy is derived purely from in-flight
//! announcements, and `barge_in` preempts regardless), so exclusivity has to be
//! asserted, and the cases where it is legitimately overridden — a barge-in alarm —
//! have to be *reported* rather than silently corrupting a measurement.
//!
//! Everything this takes over is restored on teardown: routing, levels and mutes.
//!
//! ## How the group is formed, and why it is formed this way
//!
//! Grouping in this daemon is *derived*, not declared: `routing/sync_group/mod.rs` groups every
//! output that shares a source set, and one group == one sync anchor == one clock.
//! So "form a group around an arbitrary output set" means one thing only — make those
//! outputs share a source set that nothing else does.
//!
//! It is done with an **in-memory intent override** ([`GroupReconciler::set_align_hold`],
//! `routing/sync_group/mod.rs`), not by editing the routing store:
//!
//! - every intent link whose output is held is dropped for the duration, so no music
//!   source reaches it (the *displacement*);
//! - one synthetic link [`ALIGN_HOLD_SOURCE`] → each held output is injected, so the
//!   reconciler materialises exactly one group with exactly those members and gives it
//!   an anchor. [`ALIGN_HOLD_SOURCE`] is not a real node, so nothing is linked *into*
//!   that anchor and it is silent until `pw::player::play_loop_to_target` writes the click
//!   into it — which is also what pumps every per-device relay.
//!
//! The reason it is an override rather than a store edit is the restore guarantee:
//! **nothing that has to be restored is ever destroyed.** The user's routing is
//! untouched on disk and in memory, so teardown is "stop overriding" — an operation
//! that cannot half-succeed, is idempotent, and does the right thing even if the
//! process dies (a restart reads the unmodified store). The only state that genuinely
//! *is* mutated, and therefore genuinely needs snapshot/restore, is per-device level
//! and mute; `align/calibrate/mod.rs` owns that snapshot, and it is a small, bounded set.
//!
//! ## What exclusivity covers, and what it does not
//!
//! Covered:
//!
//! - **other music sources** — displaced by the override, for exactly the held set;
//! - **ordinary announcements** — the hold is a [`reservation`](crate::announce_arbiter)
//!   in the announce arbiter, so they queue (default) or are rejected per `OnBusy`;
//! - **the audio the daemon itself mixes** for those outputs — a held output's frames
//!   come only from the align group's anchor.
//!
//! Not covered, deliberately:
//!
//! - **barge-in announcements** and **voice duck holds** — a fire alarm outranks a
//!   calibration (plan §12.3). They play, and the holder is *told*: see
//!   [`Interference`], which the measurement state machine drains to discard the
//!   affected member's data with a reason naming the cause. Without that report this
//!   is the §2.3.2 bug class again — the gate blames the user's hand for a doorbell;
//! - **audio that never passes through this daemon** — a pw-sink host playing its own
//!   music, or someone AirPlaying straight to a receiver from a phone. The daemon can
//!   duck the first (`pwsink_agent`) but cannot own either, and an AP2 receiver that
//!   accepts a foreign session is simply gone from our group;
//! - **hand-made PipeWire links** into a held output's path (`pw-link` by hand).
//!
//! ## One hold for the whole run: the selection is the run's *scope* (plan §12.3.1)
//!
//! **Decided 2026-08-11 — option 1 of §12.3.1.** Forming a hold gives its outputs a
//! source set nothing else has, which means a new group, a new anchor and new
//! per-device senders: **every selected sendspin member reconnects when the hold
//! forms, and again when it releases** — tens of seconds each way (plan §2.3). Paid
//! once that is tolerable; paid per *position* of a multi-position run it is exactly
//! the cost §1.1.1 removed from the write path, reintroduced in the formation path.
//!
//! So the hold is formed **once, over the union** of everything the run will touch,
//! and each position is scoped by **audibility** instead:
//!
//! - `start` takes the run's *entire scope* — every speaker the walk will visit, not
//!   the first position's subset. This reads counter-intuitively next to a UI that
//!   then works on a subset, which is why it is spelled out here, on
//!   [`plan_hold`], on `calibrate::AlignManager::start_outputs` and on the API's
//!   `AlignStartRequest`;
//! - a later `start` whose speakers are **already held** (the same union, or any
//!   subset of it) does **not** re-form anything: [`plan_hold`] returns
//!   [`HoldPlan::Scope`], the hold — with its id, its anchor and every sender —
//!   stays exactly as it is, and only mutes move. Nothing reconnects;
//! - only a genuinely different union (a speaker the hold does not cover) re-forms,
//!   and that is the one case that pays the wave again;
//! - per-position audibility is `set_audible`/`solo` (`POST /api/align/audible`).
//!   Mutes are live and free, so a five-position walk costs **one** form and **one**
//!   release.
//!
//! Deliberately *not* done: growing an existing hold to admit an extra speaker in
//! place. The reconciler would cope (the align group's key is constant, so adding a
//! member dials only that member), but the session's snapshot/restore set, its member
//! list and its level state would all have to grow mid-run, and the added speaker
//! dials anyway — so a superset re-forms and says so, rather than acquiring a second
//! formation mechanism with its own restore obligations.
//!
//! ## Intended call sequence (the seam `align/measure/mod.rs` is wired to)
//!
//! 1. `calibrate::AlignManager::start_outputs(&HoldDeps, outputs, mode)` —
//!    validates the selection ([`validate_selection`]), forms and holds the group
//!    ([`ExclusiveHold::form`]), snapshots levels/mutes, starts the click loop into
//!    the anchor and arms the safety timeout. One session at a time; a session whose
//!    hold does not cover the new selection is torn down first — one that **does** is
//!    kept and re-scoped instead (§12.3.1 above), which is the whole point.
//! 2. `AlignManager::solo(node, level)` for the level-setting round (plan §12.2:
//!    exactly **one** member audible, default level [`crate::align::calibrate::DEFAULT_ALIGN_LEVEL`]),
//!    or `AlignManager::set_audible(members, level)` for §7's all-play round. Both go
//!    through the same set-based audibility, so "reference + target" is no longer a
//!    special case — it is `set_audible([reference, target])`, which is what
//!    `AlignManager::select` now is.
//! 3. Per measured member, before accepting a window:
//!    `AlignManager::take_interference()` (→ [`HoldRegistry::take_interference`]).
//!    Non-empty ⇒ discard that member's measurement, quoting
//!    [`Interference::reason`]. Drain semantics: each report is handed out once.
//! 4. `AlignManager::stop()` — or the safety timeout, or a superseding `start` — tears
//!    the session down: click stopped, levels/mutes restored, hold released
//!    ([`ExclusiveHold::release`]), reservation dropped so queued announcements play,
//!    override cleared so the user's routing comes back.
//!
//! Steps 2–4 work in any state and at any point, including while formation is still
//! settling; that is required by plan §12.2's "stop must work at every point".

use crate::align::calibrate::{AlignMember, MemberKind};
use crate::announce::arbiter::ReservationId;
use crate::pw::thread::ChangeNotifier;
use crate::routing::sync_group::SharedGroups;
use crate::store::routing::{RoutingLink, SharedRouting};
use crate::util::locks::LockRecover;
use crate::util::node_names::{AP2_DEV_PREFIX, PWSINK_DEV_PREFIX, SENDSPIN_DEV_PREFIX};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// The synthetic source name a held group is keyed by. Not a real node, so nothing
/// is ever linked into the group's anchor — the calibration player is the only
/// producer. Deliberately unlike any discovered source name so it cannot collide
/// with routing intent the user could have created.
pub const ALIGN_HOLD_SOURCE: &str = "align-hold-source";

/// How long [`ExclusiveHold::form`] waits for the reconciler to materialise the
/// group's anchor. Generous: the pass that creates it also stops the previous
/// group's senders and dials the new ones, and a sendspin dial is not instant.
const FORMATION_TIMEOUT: Duration = Duration::from_secs(25);

/// Poll interval while waiting for the anchor (the reconciler is change-driven and
/// has no completion signal to await).
const FORMATION_POLL: Duration = Duration::from_millis(100);

/// Cap on retained [`Interference`] reports, so a doorbell stuck in a retry loop
/// cannot grow this without bound. Reports are drained by the state machine; the
/// cap only matters when nobody is draining.
const MAX_INTERFERENCE: usize = 64;

/// Which acoustic promise the run makes (plan §1). Chosen on the wizard's first
/// page and carried at the API boundary even where only the shared parts are
/// implemented, so a mode is never silently treated as another one — the three make
/// *different promises to the user*.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum AlignMode {
    /// Align a locally-audible set, reposition, align the next set through an
    /// overlap (plan §1.1). The default: a single-position group is just this with
    /// one step.
    #[default]
    MultiPosition,
    /// Walk to each speaker in turn holding the phone at it — aligns the *wire*
    /// (plan §1). One continuous capture, no chaining.
    NearField,
    /// Today's by-ear wizard: the fallback when the mic is unusable (§4.1) or the
    /// estimator refuses (§5.5). Uses the same temporary group, which is what stops
    /// it being a special case.
    Manual,
}

impl AlignMode {
    /// Whether this mode drives the microphone measurement state machine at all.
    pub fn is_measured(self) -> bool {
        !matches!(self, AlignMode::Manual)
    }
}

/// Why a member's measurement has to be thrown away: something legitimately
/// outranked the hold (plan §12.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum InterferenceCause {
    /// A barge-in announcement played on the member (the arbiter's admission order
    /// puts `barge_in` ahead of any reservation — see `announce_arbiter`).
    BargeIn { announcement: u64 },
    /// A voice-duck hold attenuated the member's music. A second, independent
    /// interferer: it has no clip of its own, so nothing in the announce path sees
    /// it — `outputs::overlay_mixer::start_duck` reports it directly.
    DuckHold { hold: u64 },
}

impl InterferenceCause {
    /// A sentence naming the cause, for the refusal the user reads. The whole point
    /// of the report is that this does not say "hold the phone still".
    ///
    /// `member` is the **display name** — the user's own name for the speaker where
    /// they set one, otherwise the one derived from the node name (see
    /// [`resolve_labels`]). It used to be the raw node name, so the sentence said
    /// `'sendspin-dev-kitchen'` while the chip next to it in the UI said "Kitchen";
    /// the node name is still carried structurally, on [`Interference::member`].
    pub fn reason(&self, member: &str) -> String {
        match self {
            Self::BargeIn { announcement } => format!(
                "an urgent announcement (#{announcement}) played on '{member}' during the measurement — a barge-in outranks the alignment hold, so this member's reading was discarded"
            ),
            Self::DuckHold { hold } => format!(
                "'{member}' was ducked by a voice assistant turn (hold #{hold}) during the measurement, so its reading was discarded"
            ),
        }
    }
}

/// One recorded violation of exclusivity on one member.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Interference {
    /// The held output it happened on — the stable `node_name`, so a consumer can
    /// match it against a member without parsing prose.
    pub member: String,
    /// What the user calls that speaker ([`resolve_labels`]). Carried beside the node
    /// name so a consumer that renders its own sentence agrees with [`Self::reason`].
    pub member_label: String,
    pub cause: InterferenceCause,
    /// Milliseconds since the hold was formed (a monotonic, log-friendly stamp; the
    /// consumer only needs ordering and "was this during my window").
    pub at_ms: u64,
    /// [`InterferenceCause::reason`], pre-rendered so every consumer quotes the same
    /// sentence.
    pub reason: String,
}

/// The live hold, as the rest of the daemon sees it.
///
/// A process-global single slot: exactly one alignment session exists at a time
/// (`align/calibrate/mod.rs` enforces that), and the two reporters — the announce path and the
/// overlay mixer — need a cheap, lock-light "is this output reserved?" that does not
/// reach into the reconciler or the arbiter.
/// Every access goes through [`LockRecover`], never `unwrap()`, and that is not
/// defensive habit: since `RoutingNode::held` landed, this mutex is read on **every
/// routing-matrix frame**. A panic anywhere else in the process poisons it, and
/// `unwrap()` would then turn a poisoned registry into a dead routing WebSocket —
/// taking the matrix, the graph and the Outputs page down over a lock that guards one
/// `Option`. Recovering reads a possibly-stale slot instead, which is the right trade
/// for a "which speakers are held" hint.
#[derive(Default)]
pub struct HoldRegistry {
    inner: Mutex<Option<LiveHold>>,
}

struct LiveHold {
    id: ReservationId,
    /// The held outputs **and what the user calls them** ([`HoldLabels`]). One map
    /// rather than a set plus a lookup table, so "held" and "has a label" cannot
    /// disagree: every held output has an entry, resolved once at formation.
    held: HoldLabels,
    formed: Instant,
    interference: Vec<Interference>,
}

/// Held output `node_name` → the name the user reads for it.
///
/// Resolved **once, when the hold forms**, and carried with the hold because the two
/// reporters — the announce path and the overlay mixer — run where the outputs store
/// is not reachable (and, for the mixer, on a path that must not take another lock to
/// render a sentence). See [`resolve_labels`].
pub type HoldLabels = BTreeMap<String, String>;

/// Resolve display names for `held` the same way the Outputs page and the routing
/// matrix do: the user's own name from the rename store (`outputs_store`), else the
/// one derived from the node name (`routing::output_display_name`).
///
/// The rename store is the source of truth, so an interference sentence says
/// "Kitchen" exactly when the chip beside it does.
pub fn resolve_labels(outputs: &crate::store::outputs::SharedOutputs, held: &BTreeSet<String>) -> HoldLabels {
    let names = crate::store::outputs::names_snapshot(outputs);
    held.iter().map(|n| (n.clone(), names.get(n).cloned().unwrap_or_else(|| crate::routing::output_display_name(n)))).collect()
}

impl HoldRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// The process-global registry.
    pub fn global() -> &'static HoldRegistry {
        static R: OnceLock<HoldRegistry> = OnceLock::new();
        R.get_or_init(HoldRegistry::new)
    }

    /// Record the hold. Replaces any previous one (a stale entry would keep
    /// reserving outputs nothing is aligning).
    fn open(&self, id: ReservationId, held: HoldLabels) {
        *self.inner.lock_recover() = Some(LiveHold { id, held, formed: Instant::now(), interference: Vec::new() });
    }

    /// Forget the hold, if `id` is still the live one. Id-guarded so a late teardown
    /// cannot clear a *newer* session's hold.
    fn close(&self, id: ReservationId) {
        let mut guard = self.inner.lock_recover();
        if guard.as_ref().is_some_and(|h| h.id == id) {
            *guard = None;
        }
    }

    /// Id of the live hold, if any. Part of the observation seam (and what the
    /// lifecycle test asserts on); no production reader yet.
    #[allow(dead_code)]
    pub fn holder(&self) -> Option<ReservationId> {
        self.inner.lock_recover().as_ref().map(|h| h.id)
    }

    /// Is `output` currently held for alignment?
    pub fn is_reserved(&self, output: &str) -> bool {
        self.inner.lock_recover().as_ref().is_some_and(|h| h.held.contains_key(output))
    }

    /// Every held output (empty when nothing is aligning).
    ///
    /// Read once per routing-matrix frame (`routing::held_for_alignment`) to fill
    /// `RoutingNode::held` — one lock take and a consistent answer for
    /// the whole frame, where a per-output [`Self::is_reserved`] could see a release
    /// land mid-build and emit a frame in which only some of the held rows say so.
    pub fn reserved(&self) -> BTreeSet<String> {
        self.inner.lock_recover().as_ref().map(|h| h.held.keys().cloned().collect()).unwrap_or_default()
    }

    /// Report that something outranked the hold on `member`. A no-op when `member`
    /// is not held, so callers on the hot-ish paths (a duck hold, an admitted
    /// barge-in) can report unconditionally without first asking.
    pub fn note(&self, member: &str, cause: InterferenceCause) {
        let mut guard = self.inner.lock_recover();
        let Some(hold) = guard.as_mut() else { return };
        // The label comes from the hold, not from the caller: `note` is called from the
        // announce scheduler and the overlay mixer, neither of which can reach the
        // rename store — and the hold resolved every held output's name at formation
        // for exactly this.
        let Some(label) = hold.held.get(member).cloned() else { return };
        if hold.interference.len() >= MAX_INTERFERENCE {
            return;
        }
        let reason = cause.reason(&label);
        tracing::warn!("alignment exclusivity violated: {reason}");
        hold.interference.push(Interference {
            member: member.to_string(),
            member_label: label,
            cause,
            at_ms: hold.formed.elapsed().as_millis() as u64,
            reason,
        });
    }

    /// Drain hold `id`'s reports. **Drain**, because the consumer is the measurement
    /// state machine deciding which member's window to throw away: each report must
    /// be acted on exactly once.
    ///
    /// Id-guarded like [`Self::close`]: only the live holder may read its own
    /// reports, so a hold that has been superseded can never consume — or be
    /// confused by — another session's.
    pub fn take_interference(&self, id: ReservationId) -> Vec<Interference> {
        let mut guard = self.inner.lock_recover();
        match guard.as_mut() {
            Some(hold) if hold.id == id => std::mem::take(&mut hold.interference),
            _ => Vec::new(),
        }
    }

    /// Peek without draining — for the status endpoint, which must not steal a
    /// report the state machine has not seen yet.
    pub fn interference(&self, id: ReservationId) -> Vec<Interference> {
        let guard = self.inner.lock_recover();
        match guard.as_ref() {
            Some(hold) if hold.id == id => hold.interference.clone(),
            _ => Vec::new(),
        }
    }
}

/// The process-global hold registry (shorthand for [`HoldRegistry::global`]).
pub fn registry() -> &'static HoldRegistry {
    HoldRegistry::global()
}

/// Shared handles [`ExclusiveHold::form`] needs. Passed per call rather than stored
/// on `AlignManager`, so forming a group needs no change to how main.rs builds it.
pub struct HoldDeps<'a> {
    /// The live group layout the reconcile task owns — where the override is set and
    /// where the formed group's anchor is read from.
    pub groups: &'a SharedGroups,
    /// Nudges the reconcile task (it is change-driven, with no periodic tick).
    pub changes: &'a ChangeNotifier,
    /// Read-only here: the intent the hold displaces, recorded for reporting.
    pub routing: &'a SharedRouting,
    /// Adoption verdicts — a discovered-but-not-added output must not be dialed
    /// (see docs: the output adoption gate).
    pub outputs: &'a crate::store::outputs::SharedOutputs,
}

/// A formed, held, exclusive alignment group.
///
/// Owns three pieces of daemon state, all released together by [`Self::release`]:
/// the reconciler's intent override, the announce arbiter's reservation, and the
/// global [`HoldRegistry`] entry. It carries the handles it needs to release
/// itself, so teardown works from any path — including the safety timeout — without
/// the caller having to thread dependencies to it.
pub struct ExclusiveHold {
    id: ReservationId,
    mode: AlignMode,
    members: Vec<AlignMember>,
    /// The held output names, as [`Self::covers`] and `plan_hold` compare them. Kept
    /// beside `members` (from which it is derived) because every `start` asks "is this
    /// selection already inside the union?" and that question must be cheap.
    held: BTreeSet<String>,
    /// What the user calls each held output, resolved once at formation
    /// ([`resolve_labels`]).
    labels: HoldLabels,
    anchor_node_id: u32,
    /// The intent links this hold displaced, as they were at formation. Recorded for
    /// reporting only: restore is "stop overriding", never a replay of these.
    displaced: Vec<RoutingLink>,
    groups: SharedGroups,
    changes: ChangeNotifier,
    released: bool,
}

fn next_hold_id() -> ReservationId {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

impl ExclusiveHold {
    /// Form a group around `outputs`, independent of how they are routed now, and
    /// hold it exclusively.
    ///
    /// Returns once the group's **anchor** exists (so the caller can start the click
    /// loop). Members may still be connecting at that point — a sendspin device
    /// answers a reconnect with tens of seconds of silence (plan §2.3), which is
    /// what the per-member gate is for; waiting for all of them here would only move
    /// that wait somewhere it cannot be reported.
    ///
    /// On **any** failure the partial hold is released before returning, so a failed
    /// formation never leaves routing overridden.
    ///
    /// `outputs` is the run's **whole scope** (plan §12.3.1): forming this is the
    /// expensive step, so it is done once and each position is scoped by audibility.
    /// Callers reach it through `calibrate::AlignManager`, which consults
    /// [`plan_hold`] first and does not call this at all when the existing hold
    /// already covers the request.
    pub async fn form(deps: &HoldDeps<'_>, outputs: Vec<String>, mode: AlignMode) -> Result<Self, String> {
        let adopted = crate::store::outputs::adopted_snapshot(deps.outputs);
        let members = validate_selection(&outputs, &adopted)?;
        let held: BTreeSet<String> = members.iter().map(|m| m.node_name.clone()).collect();
        let labels = resolve_labels(deps.outputs, &held);
        let displaced: Vec<RoutingLink> =
            crate::store::routing::snapshot(deps.routing).into_iter().filter(|l| held.contains(&l.output)).collect();
        let mut hold = Self::acquire(deps.groups, deps.changes, members, labels, mode, displaced).await;
        match hold.await_anchor().await {
            Some(anchor) => {
                hold.anchor_node_id = anchor;
                Ok(hold)
            }
            None => {
                hold.release().await;
                Err(format!(
                    "the temporary alignment group did not come up within {} s (its sync anchor never appeared) — nothing was changed",
                    FORMATION_TIMEOUT.as_secs()
                ))
            }
        }
    }

    /// Take the three pieces of exclusivity, without waiting for the audio path.
    ///
    /// Exclusivity is asserted **before** the group is materialised on purpose: in
    /// the other order an announcement admitted during formation would play into the
    /// group being built and be invisible to the report.
    async fn acquire(
        groups: &SharedGroups,
        changes: &ChangeNotifier,
        members: Vec<AlignMember>,
        labels: HoldLabels,
        mode: AlignMode,
        displaced: Vec<RoutingLink>,
    ) -> Self {
        let id = next_hold_id();
        let held: BTreeSet<String> = members.iter().map(|m| m.node_name.clone()).collect();
        // Every held output must have a label, even if the caller's map is short (a
        // test's, or an output renamed away between two snapshots).
        let labels: HoldLabels =
            held.iter().map(|n| (n.clone(), labels.get(n).cloned().unwrap_or_else(|| crate::routing::output_display_name(n)))).collect();
        registry().open(id, labels.clone());
        crate::announce::AnnounceCoordinator::global().reserve_outputs(id, held.iter().cloned().collect());
        groups.lock().await.set_align_hold(id, held.clone());
        let _ = changes.send(());
        tracing::info!(
            "alignment hold {id} ({mode:?}): {} output(s) [{}] taken exclusively; {} routing link(s) displaced. \
             This is the run's whole scope — every member reconnected for it and will reconnect again on release (plan §12.3.1), \
             so scope each position with audibility rather than by starting again",
            members.len(),
            members.iter().map(|m| m.node_name.as_str()).collect::<Vec<_>>().join(", "),
            displaced.len()
        );
        Self {
            id,
            mode,
            members,
            held,
            labels,
            anchor_node_id: 0,
            displaced,
            groups: groups.clone(),
            changes: changes.clone(),
            released: false,
        }
    }

    /// A hold that has taken **only** the routing override on the reconciler it is
    /// given — no process-global registry entry, no announce reservation.
    ///
    /// For tests in other modules that need a `Session` to exist (`align/calibrate/mod.rs`'s
    /// union-hold tests) without a PipeWire graph to produce an anchor. Deliberately
    /// keeps its hands off the two process-global singletons: exactly one test —
    /// `a_hold_takes_exclusivity_reports_violations_and_gives_everything_back` — drives
    /// those, and a second test touching that single slot in parallel would fight it.
    #[cfg(test)]
    pub(crate) async fn for_test(
        groups: &SharedGroups,
        changes: &ChangeNotifier,
        members: Vec<AlignMember>,
        labels: HoldLabels,
        mode: AlignMode,
    ) -> Self {
        let id = next_hold_id();
        let held: BTreeSet<String> = members.iter().map(|m| m.node_name.clone()).collect();
        let labels: HoldLabels =
            held.iter().map(|n| (n.clone(), labels.get(n).cloned().unwrap_or_else(|| crate::routing::output_display_name(n)))).collect();
        groups.lock().await.set_align_hold(id, held.clone());
        Self {
            id,
            mode,
            members,
            held,
            labels,
            anchor_node_id: 7,
            displaced: Vec::new(),
            groups: groups.clone(),
            changes: changes.clone(),
            released: false,
        }
    }

    /// Poll the reconciler for the held group's anchor.
    async fn await_anchor(&self) -> Option<u32> {
        let deadline = Instant::now() + FORMATION_TIMEOUT;
        loop {
            if let Some(id) = self.anchor_from_snapshot().await {
                return Some(id);
            }
            if Instant::now() >= deadline {
                return None;
            }
            tokio::time::sleep(FORMATION_POLL).await;
        }
    }

    async fn anchor_from_snapshot(&self) -> Option<u32> {
        let snap = self.groups.lock().await.snapshot();
        snap.into_iter().find(|g| g.sources == [ALIGN_HOLD_SOURCE]).map(|g| g.anchor_node_id).filter(|id| *id != 0)
    }

    pub fn id(&self) -> ReservationId {
        self.id
    }

    pub fn mode(&self) -> AlignMode {
        self.mode
    }

    /// Re-label the promise this hold is serving (plan §1) **without re-forming it**.
    ///
    /// The mode is a statement about how the run will be conducted, not a property of
    /// the group: the same union of speakers serves a by-ear pass and a measured walk
    /// alike. So a `start` that reuses the hold ([`HoldPlan::Scope`]) may change it,
    /// and no speaker reconnects for the change.
    pub fn set_mode(&mut self, mode: AlignMode) {
        if self.mode != mode {
            tracing::info!("alignment hold {}: mode {:?} → {mode:?} (the hold is unchanged)", self.id, self.mode);
            self.mode = mode;
        }
    }

    /// The held members, with the kind that decides which knobs they have.
    pub fn members(&self) -> &[AlignMember] {
        &self.members
    }

    /// The held union, as [`plan_hold`] compares it.
    pub fn held(&self) -> &BTreeSet<String> {
        &self.held
    }

    /// What the user calls `output` (the node name itself if it is not held).
    pub fn label<'a>(&'a self, output: &'a str) -> &'a str {
        self.labels.get(output).map(String::as_str).unwrap_or(output)
    }

    // `unlevellable()` / `level_constraint()` used to live here, deriving "has a level
    // knob" from the member *kind*. W20 established that is wrong: a pw-sink host whose
    // receiver agent answers **is** levellable (`pwsink_agent`'s `SetVolume`), and one
    // whose agent is gone is not — so it is a per-output capability, re-resolved on every
    // audibility change, exactly like W17's silence channel. `calibrate`'s `LevelChannel`
    // owns that decision now, and `AlignState.level_channels` reports it; a kind-derived
    // second answer here is precisely how a member ends up "adjustable" in the solver and
    // "unadjustable" in the UI.

    /// The held output names.
    pub fn outputs(&self) -> Vec<String> {
        self.held.iter().cloned().collect()
    }

    /// The anchor the calibration click is played into.
    pub fn anchor_node_id(&self) -> u32 {
        self.anchor_node_id
    }

    /// The intent links this hold displaced, for reporting. Restore is "stop
    /// overriding", never a replay of these.
    pub fn displaced(&self) -> &[RoutingLink] {
        &self.displaced
    }

    /// Exclusivity violations recorded against this hold, without draining them.
    pub fn interference(&self) -> Vec<Interference> {
        registry().interference(self.id)
    }

    /// Drain them — each report is handed out once, so the measurement state machine
    /// can attribute one to the member it was measuring (plan §12.3).
    pub fn take_interference(&self) -> Vec<Interference> {
        registry().take_interference(self.id)
    }

    /// Release everything this hold took. **Idempotent**, infallible, and safe to
    /// call from any path (normal stop, safety timeout, a superseding start, a
    /// failed formation).
    ///
    /// Order is deliberate: the calibration mutes go first (so nothing is left silent
    /// once music can reach these speakers again), then the user's routing comes back,
    /// then the announcement queue is opened, then the registry entry goes. Nothing here
    /// can fail in a way that leaves the house half-restored — each step is a removal.
    ///
    /// The `changes` notification comes **last**, after the registry entry is gone: it is
    /// what pushes the routing matrix, and the matrix reports the hold per output
    /// (`RoutingNode::held`). Notifying before the close would push a
    /// frame that still claims the hold — with nothing scheduled to correct it, so the
    /// "held" badge would outlive the hold until something unrelated changed the graph.
    pub async fn release(&mut self) {
        if self.released {
            return;
        }
        self.released = true;
        self.unmute_relay();
        self.groups.lock().await.clear_align_hold(self.id);
        crate::announce::AnnounceCoordinator::global().release_reservation(self.id);
        registry().close(self.id);
        let _ = self.changes.send(());
        tracing::info!("alignment hold {}: released; displaced routing restored", self.id);
    }

    /// Drop the **calibration mutes** this hold's audibility took at the relay hook
    /// (`relay_delay`, plan §12.3.2/W17), for exactly the outputs it held.
    ///
    /// Kept in the same shape as the rest of teardown: synchronous, infallible,
    /// idempotent and a pure *removal*, so it can be the first step of [`Self::release`]
    /// without any risk of skipping the ones after it (`relay_delay` recovers from a
    /// poisoned lock for this reason). Scoped to [`Self::held`] rather than clearing every
    /// mute, which is this mechanism's equivalent of the id-guard on
    /// [`HoldRegistry::close`]: a hold that releases late can only ever unmute an output it
    /// held itself, never a newer session's member.
    fn unmute_relay(&self) {
        let cleared = crate::align::relay_delay::RelayDelay::global().unmute_all(self.held.iter().map(String::as_str));
        if cleared > 0 {
            tracing::debug!("alignment hold {}: dropped {cleared} relay calibration mute(s)", self.id);
        }
    }
}

impl Drop for ExclusiveHold {
    /// Last resort. Every real path calls [`Self::release`] (which makes this a
    /// no-op), but a hold that is dropped un-released would leave routing overridden
    /// for as long as the daemon runs, so it is worth a loud log and a best-effort
    /// async release rather than a silent leak.
    fn drop(&mut self) {
        if self.released {
            return;
        }
        tracing::error!("alignment hold {} dropped without release — releasing it now (this is a bug)", self.id);
        let (id, groups, changes) = (self.id, self.groups.clone(), self.changes.clone());
        // The synchronous halves happen here regardless, so exclusivity is gone even
        // if there is no runtime to finish the routing half on. The relay mutes are one of
        // them: leaving a speaker silent for as long as the daemon runs would be the worst
        // failure of this path, and unmuting needs neither a runtime nor a lock of ours.
        self.unmute_relay();
        crate::announce::AnnounceCoordinator::global().release_reservation(id);
        registry().close(id);
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                groups.lock().await.clear_align_hold(id);
                let _ = changes.send(());
            });
        } else {
            tracing::error!("alignment hold {id}: no tokio runtime in Drop — the routing override could not be cleared");
        }
    }
}

/// What a `start` has to do about the hold (plan §12.3.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HoldPlan {
    /// **Free.** The hold already in place covers the request, so keep it — its id,
    /// its anchor and every per-device sender — and scope this position by changing
    /// which held members are audible. Nothing reconnects.
    Scope,
    /// **Expensive.** A new hold has to be formed; the string says why, phrased for
    /// the log line and for the cost note the user reads.
    Form(String),
}

impl HoldPlan {
    /// Why a new hold is needed (`None` when none is).
    pub fn form_reason(&self) -> Option<&str> {
        match self {
            Self::Scope => None,
            Self::Form(why) => Some(why),
        }
    }
}

/// Decide whether a `start` for `requested` can run inside the hold over `held`.
///
/// This is the whole of the §12.3.1 decision, in one pure function: **the same union,
/// or any subset of it, is a no-op on the hold.** Re-selecting a subset per position
/// must never re-form, because a re-form is two reconnect waves — one to give the
/// speakers back and one to take them again — and a multi-position run would pay that
/// at every position.
///
/// A *superset* forms, deliberately: see the module docs on why an existing hold is
/// not grown in place.
pub fn plan_hold(held: Option<&BTreeSet<String>>, requested: &[String]) -> HoldPlan {
    let unique: BTreeSet<&str> = requested.iter().map(String::as_str).collect();
    let Some(held) = held else {
        return HoldPlan::Form("no speakers were held yet".to_string());
    };
    if unique.len() < 2 {
        // Not reusable and not formable either — `validate_selection` owns the
        // sentence the user sees, so this only has to route the request there.
        return HoldPlan::Form("fewer than two speakers were selected".to_string());
    }
    let extra: Vec<&str> = unique.iter().copied().filter(|n| !held.contains(*n)).collect();
    if extra.is_empty() {
        return HoldPlan::Scope;
    }
    HoldPlan::Form(format!("the selection needs {} speaker(s) the current hold does not cover ({})", extra.len(), extra.join(", ")))
}

/// Turn a user selection into alignment members, or say exactly what is wrong.
///
/// Pure, so the rules are testable without a graph. Rejections name the output at
/// fault: the caller shows this to a user who picked speakers on a page.
///
/// `outputs` is the run's whole scope, not one position's (plan §12.3.1) — the caller
/// only reaches here when a new hold really has to be formed.
pub fn validate_selection(outputs: &[String], adopted: &BTreeSet<String>) -> Result<Vec<AlignMember>, String> {
    let mut names: Vec<String> = outputs.to_vec();
    names.sort();
    names.dedup();
    if names.len() < 2 {
        return Err("alignment needs at least two speakers — there is nothing to align one against".to_string());
    }
    let mut members = Vec::with_capacity(names.len());
    for name in names {
        let kind = member_kind(&name).ok_or_else(|| unsupported_reason(&name))?;
        if !adopted.contains(&name) {
            return Err(format!("'{name}' has not been added on the Outputs page, so the daemon must not stream to it"));
        }
        members.push(AlignMember { node_name: name, kind, node_id: None });
    }
    Ok(members)
}

/// The alignment member kind of an output, by its stable-name prefix, or `None` for
/// a kind that cannot be an alignment member.
pub fn member_kind(output: &str) -> Option<MemberKind> {
    if output.starts_with(SENDSPIN_DEV_PREFIX) {
        Some(MemberKind::Sendspin)
    } else if output.starts_with(AP2_DEV_PREFIX) {
        Some(MemberKind::Airplay2)
    } else if output.starts_with(PWSINK_DEV_PREFIX) {
        // Admitted since W15. It has a delay knob (with a hard 15 ms floor, plan
        // §1.1.2) but **no level knob**, so it constrains the other members instead of
        // being tuned — reported by `ExclusiveHold::level_constraint`, never silently
        // skipped, and never mis-typed as sendspin.
        Some(MemberKind::PwSink)
    } else {
        None
    }
}

/// Why an output cannot be an alignment member.
fn unsupported_reason(output: &str) -> String {
    format!(
        "'{output}' is not an alignable speaker (only sendspin devices, AirPlay-2 receivers and PipeWire receiver hosts have a delay knob)"
    )
}

#[cfg(test)]
mod tests;
