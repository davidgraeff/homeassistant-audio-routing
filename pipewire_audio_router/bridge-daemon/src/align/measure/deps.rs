//! The seams a run is driven through, so it can be tested without a room.
//!
//! A run needs three things from the outside: a session that plays the pattern and
//! holds a group ([`SessionControl`]), a microphone that yields analysis windows
//! ([`MicFeed`]), and somewhere to write the delays it decides on
//! ([`DelayWriter`]). [`MeasureDeps`] bundles them, [`LiveMic`] is the real
//! microphone, and the `SessionControl` impl here is the real session — the
//! alignment manager in `align/calibrate.rs`.
//!
//! That is the whole reason the run is testable: the fakes in `tests/harness.rs`
//! implement these three traits, so an entire measurement can happen in a
//! paused-clock test with no hardware and no waiting.

use super::*;

// ---------------------------------------------------------------- injection

/// Boxed future, so the traits below stay object-safe without an `async_trait`
/// dependency.
pub type Fut<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SessionMember {
    pub node_name: String,
    pub kind: MemberKind,
}

#[derive(Debug, Clone)]
pub struct SessionSnapshot {
    pub active: bool,
    pub sources: Vec<String>,
    pub members: Vec<SessionMember>,
    /// The session's calibration playback level (0–100), and the default for any
    /// member [`Self::levels`] has no entry for.
    pub level: u8,
    /// **Per-member** level as the session last applied it (`calibrate`'s W19 map).
    ///
    /// Near field's whole level model, and the reason this is read rather than
    /// decided here (plan §12.2): the user stands at a speaker, moves the slider
    /// until the signal check goes green, and *that* is the level the arrival must be
    /// measured at. A level chosen anywhere else — including one this module picked
    /// for the group — is wrong at arm's length.
    pub levels: HashMap<String, u8>,
}

impl SessionSnapshot {
    /// The level to measure `node_name` at: what the session last applied to it,
    /// falling back to the session default for a member never soloed yet.
    pub(crate) fn level_for(&self, node_name: &str) -> u8 {
        self.levels.get(node_name).copied().unwrap_or(self.level)
    }
}

/// What the orchestration needs from the alignment session (`align/calibrate.rs`).
///
/// Deliberately two methods: this module must **not** own muting, volume,
/// playback or teardown — that machinery already exists and is live-tested, and a
/// second copy of it would be a second way to leave a room muted with a click
/// looping. Being a trait is what lets the state machine be unit-tested without a
/// PipeWire graph.
pub trait SessionControl: Send + Sync {
    fn snapshot(&self) -> Fut<'_, SessionSnapshot>;
    /// Make exactly one member audible at `level`, muting every other member.
    fn solo(&self, node_name: String, level: u8) -> Fut<'_, Result<(), String>>;
    /// Silence every member without releasing the hold or stopping the click track.
    ///
    /// What a **parked** run owes the room. The hold has to stay (`apply` measures
    /// through it and writes into it), but the tick/tack must not keep looping while
    /// the user reads a proposal, and it must fall silent even if the tab is closed —
    /// so this is the daemon's decision, not an empty audible set posted by the panel
    /// (plan §12.3.2's principle: the party holding the numbers decides). Resuming
    /// needs no counterpart: every measuring step begins with [`Self::solo`].
    fn silence(&self) -> Fut<'_, Result<(), String>>;
    /// Drain the exclusivity violations recorded since the last call (plan §12.3).
    ///
    /// Draining rather than peeking, because every entry is consumed here: one for the
    /// member being measured aborts its window with the cause named, and one for any
    /// other member still becomes a warning, so nothing is silently dropped.
    fn take_interference(&self) -> Fut<'_, Vec<crate::align::group::Interference>>;
}

/// The mic ingest, as this module consumes it (`align_mic`'s W3 hand-off).
pub trait MicFeed: Send + Sync {
    fn status(&self) -> MicStatus;
    fn window_from(&self, first_frame: u64, frames: usize) -> Option<MicWindow>;
}

/// The process-global ingest.
pub struct LiveMic;

impl MicFeed for LiveMic {
    fn status(&self) -> MicStatus {
        crate::align::mic::shared().status()
    }

    fn window_from(&self, first_frame: u64, frames: usize) -> Option<MicWindow> {
        crate::align::mic::shared().window_from(first_frame, frames)
    }
}

/// Writes one member's delay knob.
///
/// Implemented in `api/measure.rs` **on top of the existing endpoint handlers** rather
/// than on `sync_settings`, so the persistence order, the clamping, the live push
/// and — the one that matters — the per-device reconnect and its group-wide
/// high-water exception are not duplicated here (plan §9.3).
pub trait DelayWriter: Send + Sync {
    fn write(&self, node_name: String, kind: MemberKind, delay_ms: u16) -> Fut<'_, Result<String, String>>;
}

/// Everything a run needs from the daemon, assembled by the API handler so this
/// module never sees `AppState`.
pub struct MeasureDeps {
    pub mode: Mode,
    /// Run the multi-position mode as a **chain** (plan §1.1): park between positions
    /// and take one [`MeasureManager::position`] call per listening spot, each naming
    /// the speakers audible there plus the overlaps from the already-aligned set.
    ///
    /// Opt-in rather than inferred, because it changes who drives the run. `false` is
    /// the single-position case — which is a chain with one step, and behaves exactly as
    /// it did before W12: the daemon measures every member itself from wherever the
    /// phone is sitting. Ignored for [`Mode::NearField`], which has its own acquisition
    /// (a walk needs no overlaps at all, §1.2).
    pub chained: bool,
    /// Speakers from an **earlier**, already-aligned run that this one should be made
    /// coherent with, through a shared overlap member (plan §1.2's cross-session case,
    /// §12.1's "link this set or keep it independent?").
    ///
    /// Empty means independent, which is the only thing implemented. Chaining *within*
    /// one run exists ([`Self::chained`]); linking across **runs** does not, because
    /// nothing stores a finished run's aligned set with its delays — that is W8b. A
    /// non-empty list is refused as [`RefusalKind::ModeUnsupported`] rather than
    /// accepted and quietly ignored: a run that *said* it linked but did not would leave
    /// the user believing in a coherence that does not exist.
    pub link_to: Vec<String>,
    pub session: Arc<dyn SessionControl>,
    pub mic: Arc<dyn MicFeed>,
    pub writer: Arc<dyn DelayWriter>,
    /// The provisional delay line (plan §1.1.1). A chain applies its per-step delays
    /// here and writes the real knobs once, at the end; a single-position run never
    /// touches it.
    pub relay: Arc<dyn RelayControl>,
    /// Each member's currently persisted delay, keyed by node name.
    pub current_delays: HashMap<String, u16>,
    pub send_ahead: SendAheadContext,
    /// Each member's persisted band-split calibration in ms, keyed by node name
    /// (`sync_settings::BandSplit::split_ms`). Subtracted before the cross-band check
    /// compares members, so a mixed-model group is not refused for its hardware
    /// (plan §10.2). Empty is the uncalibrated case and behaves exactly as before.
    pub band_splits: BandSplits,
    /// Where this run's forensic transcript goes (`align/transcript.rs`). A run opens
    /// one file here and appends to it; `Transcripts::disabled()` records nothing,
    /// which is what a unit test and a daemon without `/data` both get.
    pub transcript: Arc<crate::align::transcript::Transcripts>,
    pub timing: Timing,
}

/// Inputs for the plan §9.2 send-ahead warning.
///
/// A sendspin group's send-ahead is a high-water mark over its members'
/// `min_buffer_ms + static_delay_ms` (`outputs::sendspin::server::required_send_ahead_us`).
/// Raising it reconfigures the *group's* stream — every speaker in the room goes
/// quiet for tens of seconds — where a smaller change reconnects only the one
/// device. So the solve warns before crossing that line.
///
/// **What feeds the mark, after §2.4.1.** `static_delay_ms` is an **advance**, and
/// it is added to the group lead *because* the device plays that much earlier. So
/// the quantity that lifts the high-water mark is an advance, not a delay: an AP2
/// render delay or a pw-sink playout delay happens inside that member's own
/// sender and never touches the sendspin lead. [`Self::mark_ms`] therefore takes
/// **advances**, and [`solve`] passes it only the members whose polarity is
/// [`KnobPolarity::Advance`].
#[derive(Debug, Clone, Default)]
pub struct SendAheadContext {
    /// The floor from everything that is not a member advance: the configured
    /// group lead, and the codec's decode floor.
    pub floor_ms: u32,
    /// What a member that reports no `min_buffer_ms` is assumed to need.
    pub unreported_floor_ms: u32,
    /// Per-sendspin-member `min_buffer_ms` as the device reported it.
    pub min_buffer_ms: HashMap<String, Option<u32>>,
}

impl SendAheadContext {
    /// The send-ahead mark a given set of **advances** implies, in ms. Anything in
    /// `advances` that this context does not know a `min_buffer_ms` for is ignored,
    /// which is what keeps a delay-polarity member from being counted into a lead
    /// it has no part in.
    pub(crate) fn mark_ms(&self, advances: &HashMap<String, u16>) -> u32 {
        self.min_buffer_ms
            .iter()
            .map(|(node, min_buffer)| min_buffer.unwrap_or(self.unreported_floor_ms) + u32::from(advances.get(node).copied().unwrap_or(0)))
            .fold(self.floor_ms, u32::max)
    }
}

impl SessionControl for crate::align::calibrate::AlignManager {
    fn snapshot(&self) -> Fut<'_, SessionSnapshot> {
        Box::pin(async move {
            let s = self.status().await;
            SessionSnapshot {
                active: s.active,
                sources: s.sources,
                members: s.members.iter().map(|m| SessionMember { node_name: m.node_name.clone(), kind: m.kind }).collect(),
                level: s.volume,
                levels: s.levels.into_iter().collect(),
            }
        })
    }

    fn take_interference(&self) -> Fut<'_, Vec<crate::align::group::Interference>> {
        Box::pin(async move { crate::align::calibrate::AlignManager::take_interference(self).await })
    }

    fn solo(&self, node_name: String, level: u8) -> Fut<'_, Result<(), String>> {
        Box::pin(async move {
            // `AlignManager::solo` is the one-element case of the session's set-based
            // audibility, and it applies the level in the same call.
            //
            // Deliberately not `select(x, x)`, which the earlier version used: that
            // reaches the same audible set, but it also overwrites the session's
            // reference/target with `x`. Those two belong to the *by-ear* panel, and a
            // measurement run silently rewriting them would leave the manual path
            // pointing at whichever member happened to be measured last.
            self.solo(node_name, level).await.map(|_| ())
        })
    }

    fn silence(&self) -> Fut<'_, Result<(), String>> {
        Box::pin(async move {
            // The session's own set-based audibility with an empty set. Its levels,
            // its hold and its player thread are untouched, so `apply`'s verification
            // walk can still measure through exactly the same session.
            crate::align::calibrate::AlignManager::silence(self).await.map(|_| ())
        })
    }
}
