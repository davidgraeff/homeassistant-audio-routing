//! What the alignment session reports: the shape the UI renders and the API
//! serializes.
//!
//! [`AlignState`] is the whole picture — which group is being aligned, which members
//! are audible, what each one's knob is set to, and the cost notes that tell the user
//! what an action will *do* before they take it ("this forms a group", "this rescopes
//! the run"). Those notes are part of the contract, not decoration: forming a group
//! costs a device reconnect, and the user deserves to know before clicking.
//!
//! A wire format. The frontend's `align.svelte.ts` is written against these field
//! names.

use super::*;

#[derive(Debug, Clone, Serialize)]
pub struct AlignMember {
    pub node_name: String,
    pub kind: MemberKind,
    /// Unused for the current member kinds (sendspin + AP2 are both virtual and
    /// muted in-band, not by PipeWire node id). Always `None`; kept for the API
    /// shape.
    pub node_id: Option<u32>,
}

/// One alignable group (a source-set with ≥1 present member), for the picker.
#[derive(Debug, Clone, Serialize)]
pub struct AlignGroup {
    pub sources: Vec<String>,
    pub members: Vec<AlignMember>,
}

/// Current calibration state, echoed to the UI.
#[derive(Debug, Clone, Serialize)]
pub struct AlignState {
    pub active: bool,
    /// The session's stable identity, as the UI compares it: the **source set** for
    /// a session started from a source-card group (`start`), the **selected output
    /// set** for one started from a selection (`start_outputs`) — in both cases the
    /// thing the user picked, so "is this session mine?" is answerable.
    ///
    /// With the union hold (plan §12.3.1) this is the *latest* `start`'s selection,
    /// which may be a subset of [`Self::outputs`] — the speakers actually held.
    pub sources: Vec<String>,
    /// The fixed member everything is aligned against.
    pub reference: Option<String>,
    /// The member currently being tuned (audible alongside the reference).
    pub target: Option<String>,
    pub members: Vec<AlignMember>,
    /// Playback level (0–100) most recently applied to the audible members — and the
    /// **default** for any member [`Self::levels`] has no entry for.
    pub volume: u8,
    /// **Per-member** calibration level (0–100), keyed by node name: what was last applied
    /// to that speaker, for every speaker this session has applied a level to (W19).
    ///
    /// Read it as `levels[node] ?? volume` — a member the run has not reached yet is simply
    /// absent, so a consumer never has to distinguish "not set" from "set to something".
    /// The session owns this (not the browser), which is the whole point: reloading the page
    /// in the middle of a walk still knows what each speaker was set to.
    ///
    /// **Not persisted**, deliberately (see the module docs): the correct level depends on
    /// where the phone is, so the same speaker wants a different one at a different position
    /// of the same run. It also has nothing to do with the *restore* obligation — what
    /// teardown puts back is the user's pre-session level, never one of these.
    pub levels: BTreeMap<String, u8>,
    /// Which acoustic promise this session is making (plan §1).
    pub mode: AlignMode,
    /// The outputs held exclusively for the session (its temporary group) — the run's
    /// **whole scope**, not one position's (plan §12.3.1).
    pub outputs: Vec<String>,
    /// Identity of the exclusive hold behind this session. **Unchanged across a
    /// `start` that re-selects a subset**, which is how a caller can see that nothing
    /// re-formed and nothing reconnected; a new number means a new group, a new anchor
    /// and a fresh reconnect wave.
    pub hold_id: u64,
    /// Did the most recent `start` reuse the hold it found (free) instead of forming
    /// one (expensive)?
    pub hold_reused: bool,
    /// What the most recent `start` cost, in words — forming a group reconnects every
    /// member, changing which held members are audible does not. Said here because it
    /// is the one thing about this API that surprises people (plan §12.3.1).
    pub hold_cost: String,
    /// Held members with **no level knob this daemon can reach** (plan §7): they constrain
    /// the others' levels instead of being tuned.
    ///
    /// A **per-output** answer since W20, not a list of `pwsink-dev-*`: a pw-sink host whose
    /// receiver agent is answering is levelled out of band like any other member, so what
    /// lands here is a host with no agent, a sink with no volume lever, or a future kind
    /// with nothing of its own. Re-resolved on every audibility change, so a member appears
    /// here the moment its agent drops mid-walk — see [`Self::level_channels`].
    pub unlevellable: Vec<String>,
    /// One sentence saying what that means — including the part that is not obvious:
    /// such a member sets the clip ceiling, so if *it* clips, turning every other
    /// speaker down cannot rescue the measurement. `None` when every member has a
    /// level knob.
    pub level_note: Option<String>,
    /// How each member's level is reached this position ([`LevelChannel`]), keyed by node
    /// name — the resolved answer [`Self::unlevellable`] is derived from, published because
    /// a consumer building a level solve needs the knob (`LevelChannel::knob`) and must not
    /// re-derive it from the member's kind.
    pub level_channels: BTreeMap<String, LevelChannel>,
    /// The members currently audible — one for level-setting/measurement, two for
    /// the by-ear comparison, N for §7's all-play round.
    pub audible: Vec<String>,
    /// **Which wire channels each member emits** while this session runs, keyed by node
    /// name, for every member that is not on the default `both`
    /// ([`crate::align::relay_delay::MeasureChannels`]).
    ///
    /// The remedy for a member that drives a **stereo pair**: the click is identical on
    /// both channels, so such a member is two acoustic sources and its arrival time is
    /// not a single number — the estimator refuses it as an ambiguous peak (observed on a
    /// desktop pair, 1.1× between the two arrivals). Emitting one channel makes it one
    /// source.
    ///
    /// Absent ⇒ `both`, so a consumer never distinguishes "not set" from "set to the
    /// default", exactly like [`Self::levels`]. Session-owned and not persisted: it is a
    /// property of where the microphone is, and teardown puts both channels back.
    pub channels: BTreeMap<String, crate::align::relay_delay::MeasureChannels>,
    /// Exclusivity violations recorded so far (plan §12.3), newest last. A **peek**:
    /// the measurement state machine drains these, so the status endpoint must not.
    pub interference: Vec<Interference>,
    /// The routing intent the session is displacing while it holds these speakers —
    /// what the UI shows as "these will stop playing what they are playing now".
    pub displaced: Vec<crate::store::routing::RoutingLink>,
    /// How much longer this session may sit **idle** before the daemon tears it down and
    /// gives the speakers back — in whole seconds **relative to this frame**, `None`
    /// when nothing is running.
    ///
    /// Relative, never an absolute instant: the browser's clock and the daemon's differ
    /// by an unknown amount, and a client only needs to count down locally from what it
    /// was told and re-sync on the next frame.
    ///
    /// **`Some(0)` does not mean the session is gone.** It means the idle deadline has
    /// passed and the watchdog will take it at its next check, which is up to
    /// [`Self::timeout_slack_s`] away — so a UI must not render the disappearance until
    /// it is told about it (a frame with `active: false`).
    ///
    /// What refreshes it is *doing something to the run*, not looking at it: making
    /// members audible or changing a level ([`AlignManager::set_audible`],
    /// [`AlignManager::solo`], [`AlignManager::select`], [`AlignManager::set_level`]), a
    /// re-scoping `start`, or the deliberate [`AlignManager::still_here`]. Reading a
    /// proposal refreshes nothing, and neither does holding a socket open — see
    /// `still_here`'s docs for why that asymmetry is the whole point.
    pub closes_in_s: Option<u64>,
    /// The whole idle allowance ([`SESSION_TIMEOUT`]) in seconds — what
    /// [`Self::closes_in_s`] counts down from, so a client can phrase the rule ("15
    /// minutes without a change") without hard-coding the daemon's number.
    pub idle_timeout_s: u64,
    /// How much later than [`Self::closes_in_s`] the close can actually happen
    /// ([`TIMEOUT_POLL`]), because the watchdog is a poller.
    ///
    /// Published so a client is not left inventing a fudge factor: the honest rendering
    /// of `closes_in_s` is "in about N", and this is the size of "about".
    pub timeout_slack_s: u64,
}

impl AlignState {
    pub(crate) fn inactive() -> Self {
        Self {
            active: false,
            sources: Vec::new(),
            reference: None,
            target: None,
            members: Vec::new(),
            volume: DEFAULT_ALIGN_LEVEL,
            levels: BTreeMap::new(),
            mode: AlignMode::default(),
            outputs: Vec::new(),
            hold_id: 0,
            hold_reused: false,
            hold_cost: String::new(),
            unlevellable: Vec::new(),
            level_note: None,
            level_channels: BTreeMap::new(),
            audible: Vec::new(),
            channels: BTreeMap::new(),
            interference: Vec::new(),
            displaced: Vec::new(),
            // No session, so nothing is counting down — but the two *rules* are still
            // worth stating, so a client that has only ever seen the inactive frame can
            // already say what the timeout is and how precise it is not.
            closes_in_s: None,
            idle_timeout_s: SESSION_TIMEOUT.as_secs(),
            timeout_slack_s: TIMEOUT_POLL.as_secs(),
        }
    }
}

/// What forming a hold costs, for [`AlignState::hold_cost`]. `why` is
/// `align_group::HoldPlan`'s reason.
pub(crate) fn form_cost_note(members: usize, why: &str) -> String {
    format!(
        "this start formed a new exclusive group around {members} speaker(s) ({why}) — every sendspin member reconnected for it \
         and will reconnect once more when the session ends (tens of seconds each way, plan §2.3). \
         The selection is the run's WHOLE scope: to work one position at a time, keep this session and POST /api/align/audible \
         with the speakers you can hear from where you stand. Mutes are live and cost nothing; starting again does not."
    )
}

/// What re-selecting inside the union costs: nothing.
pub(crate) fn scope_cost_note(selected: usize, held: usize) -> String {
    format!(
        "no speaker reconnected: the {selected} selected speaker(s) are already inside the exclusive group held for {held}, \
         so this start only changed mutes. Re-selecting held speakers is free — forming a group is what is expensive (plan §12.3.1)."
    )
}
