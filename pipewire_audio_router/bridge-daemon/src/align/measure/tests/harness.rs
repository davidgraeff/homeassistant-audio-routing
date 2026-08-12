//! Fixtures and fakes shared by every subject module beside this one.
//!
//! `FakeMic` injects arrivals a real room would produce, `FakeSession` stands in
//! for the alignment session, `FakeWriter` records what a run would write to a
//! device, and `Rig` wires the three together with a paused tokio clock — which is
//! what makes a run whose settling waits are tens of seconds long testable at all.

use super::super::*;
use std::f64::consts::TAU;
use std::sync::atomic::AtomicU64;

pub(super) fn clean(elapsed_ms: u64, periods: usize, peak: f32) -> GateSample {
    GateSample {
        elapsed: Duration::from_millis(elapsed_ms),
        connected: true,
        reconnected: false,
        gap: false,
        clipped: false,
        peak,
        periods_used: periods,
        quality: Quality::Accepted,
        interference: None,
    }
}

pub(super) fn rejected(reason: RejectReason, msg: &str) -> Quality {
    Quality::Rejected { reason, message: msg.to_string() }
}

pub(super) fn member(name: &str) -> SessionMember {
    SessionMember { node_name: name.to_string(), kind: MemberKind::Sendspin }
}

/// One observation with an explicit A phase; B follows A by the nominal 1 s
/// unless `band_bias` says otherwise.
pub(super) fn obs(name: &str, pass: usize, centre: f64, phase_a: f64, band_bias: f64) -> MemberObservation {
    MemberObservation {
        node_name: name.to_string(),
        pass,
        grid_epoch: 0,
        period_centre: centre,
        m: MemberMeasurement {
            phase_a_ms: phase_a.rem_euclid(2000.0),
            phase_b_ms: (phase_a + 1000.0 + band_bias).rem_euclid(2000.0),
            std_error_ms: 0.05,
            peak_snr_db: 40.0,
            second_peak_ratio: 20.0,
            drift_ppm: 0.0,
            periods_used: 4,
        },
    }
}

pub(super) fn solve_of(members: &[SessionMember], observations: &[MemberObservation], delays: &[(&str, u16)]) -> Result<Proposal, Refusal> {
    let current: HashMap<String, u16> = delays.iter().map(|(n, d)| ((*n).to_string(), *d)).collect();
    let ctx = SendAheadContext::default();
    solve(&SolveInput { timing: Timing::real(), members, observations, current_delays: &current, send_ahead: &ctx, closure: None })
}

/// A session that records what is soloed. No PipeWire, no mute protocol — the
/// real `AlignManager` impl above is three lines over `select`/`set_level`, and
/// what needs testing is the orchestration's decisions.
pub(super) struct FakeSession {
    pub(super) members: Vec<SessionMember>,
    pub(super) soloed: Arc<Mutex<Option<String>>>,
    pub(super) active: Arc<AtomicBool>,
    /// Exclusivity violations to hand out on the next drain (plan §12.3).
    pub(super) interference: Arc<Mutex<Vec<crate::align::group::Interference>>>,
    /// The session's per-member level map (`calibrate`'s W19 field), which is where
    /// a near-field arrival's level comes from when the request does not override
    /// it (plan §12.2).
    pub(super) levels: Arc<Mutex<HashMap<String, u8>>>,
}

impl SessionControl for FakeSession {
    fn take_interference(&self) -> Fut<'_, Vec<crate::align::group::Interference>> {
        Box::pin(async move { std::mem::take(&mut *self.interference.lock_recover()) })
    }

    fn snapshot(&self) -> Fut<'_, SessionSnapshot> {
        Box::pin(async move {
            SessionSnapshot {
                active: self.active.load(Ordering::Relaxed),
                sources: vec!["src".to_string()],
                members: self.members.clone(),
                level: 50,
                levels: self.levels.lock_recover().clone(),
            }
        })
    }

    fn solo(&self, node_name: String, level: u8) -> Fut<'_, Result<(), String>> {
        Box::pin(async move {
            // The real `AlignManager::solo` applies the level in the same call and
            // records it in `AlignState::levels`; both halves matter to near field,
            // so the fake does both.
            self.levels.lock_recover().insert(node_name.clone(), level);
            *self.soloed.lock_recover() = Some(node_name);
            Ok(())
        })
    }
}

/// A capture that renders the *real* click track for whichever member is
/// soloed, delayed by that member's true arrival time, and hands out windows
/// on demand from virtual time.
///
/// This is a transport stand-in, not a stand-in for the thing under test: the
/// estimator, the gate, the drift fit and the solve all run for real. What it
/// cannot stand in for is a room — no reflections, no noise floor, no phone
/// microphone, no AGC. Those need W0's device (see the report).
pub(super) struct FakeMic {
    pub(super) rate: u32,
    pub(super) pattern_ms: f64,
    pub(super) soloed: Arc<Mutex<Option<String>>>,
    /// True arrival per member, in ms. Behind a lock so a test can *move a
    /// speaker* mid-walk, which is the failure the closure check exists to catch.
    pub(super) arrivals: Arc<Mutex<HashMap<String, f64>>>,
    pub(super) start: Instant,
    pub(super) frames: AtomicU64,
    pub(super) connected: Arc<AtomicBool>,
    /// Mic-vs-audio clock offset, as ms of phase per second of capture. This is
    /// what a phone's clock running at a different rate looks like from here: every
    /// member's measured arrival creeps at the same rate, indistinguishable from a
    /// real offset within any single reading.
    pub(super) drift_ms_per_s: f64,
    /// Absolute frame at which the capture "reconnects" (0 = never).
    ///
    /// Scheduled rather than immediate on purpose: a reconnect is only *detectable*
    /// as the frame counter going backwards past what a reader has already consumed
    /// ([`Feeder::pull`]), so it has to happen while a reading is in progress. A
    /// test sets this a few seconds ahead, which lands it inside the next reading —
    /// after the mute guard, before the gate has its periods.
    pub(super) reconnect_at: Arc<AtomicU64>,
    /// A *mechanism* under test, moving the soloed member's arrival on top of its
    /// fixed `arrivals` entry. [`NoShift`] for everything that is not W21.
    pub(super) shift: Arc<dyn ArrivalShift>,
}

/// How something the daemon does moves a member's arrival — the seam a test injects
/// physics through. Two implementations: [`RelayShift`], where a provisional delay
/// simply delays that speaker (the chain's premise, plan §1.1), and [`EquivPhysics`],
/// where the delay line's gain, the knob's *sign* and a reconnect's ε are all
/// injectable, because whether those match is exactly what W21 measures.
pub(super) trait ArrivalShift: Send + Sync {
    fn shift_ms(&self, member: &str) -> f64;

    /// Whether this member is currently producing no sound at all — a wedged device
    /// (plan §2.3.2), and the only honest way to make a gate genuinely time out.
    fn silent(&self, _member: &str) -> bool {
        false
    }
}

/// How far the frame counter jumps back at a simulated reconnect: ten seconds,
/// which is comfortably more than any reader can have consumed.
pub(super) const FAKE_RECONNECT_SHIFT_S: u64 = 10;

impl FakeMic {
    pub(super) fn frames_now(&self) -> u64 {
        let raw = (self.start.elapsed().as_secs_f64() * f64::from(self.rate)) as u64;
        let at = self.reconnect_at.load(Ordering::Relaxed);
        let shift = match at != 0 && raw >= at {
            true => u64::from(self.rate) * FAKE_RECONNECT_SHIFT_S,
            false => 0,
        };
        let f = raw.saturating_sub(shift);
        self.frames.store(f, Ordering::Relaxed);
        f
    }

    /// The click track's shape: an 8 ms Hann-enveloped 3 kHz burst at the
    /// period start and a 1.5 kHz one at the half point, exactly as
    /// `calibrate::click_wav` lays it out — on the pattern the test is using.
    pub(super) fn sample(&self, frame: u64, arrival_ms: f64) -> f32 {
        let rate = f64::from(self.rate);
        let period = self.pattern_ms / 1000.0 * rate;
        let burst = 0.008 * rate;
        let t = (frame as f64 - arrival_ms / 1000.0 * rate).rem_euclid(period);
        let mut v = 0.0;
        for (offset, hz) in [(0.0, 3000.0), (period / 2.0, 1500.0)] {
            let k = t - offset;
            if k >= 0.0 && k <= burst {
                let env = 0.5 - 0.5 * (TAU * k / burst).cos();
                v += 0.3 * env * (TAU * hz * k / rate).sin();
            }
        }
        v as f32
    }
}

impl MicFeed for FakeMic {
    fn status(&self) -> MicStatus {
        MicStatus {
            connected: self.connected.load(Ordering::Relaxed),
            sample_rate: self.rate,
            frames_received: self.frames_now(),
            blocks_received: 0,
            gap_count: 0,
            peak: 0.3,
            clipped: false,
            clip_count: 0,
            buffered_frames: self.rate as usize * 10,
            capacity_frames: self.rate as usize * 10,
        }
    }

    fn window_from(&self, first_frame: u64, frames: usize) -> Option<MicWindow> {
        let head = self.frames_now();
        if first_frame + frames as u64 > head {
            return None;
        }
        let soloed = self.soloed.lock_recover().clone();
        if soloed.as_deref().is_some_and(|s| self.shift.silent(s)) {
            return Some(MicWindow { samples: vec![0.0; frames], first_frame, sample_rate: self.rate, gap: false, clipped: false });
        }
        let arrival =
            soloed.as_deref().map(|s| self.arrivals.lock_recover().get(s).copied().unwrap_or(0.0) + self.shift.shift_ms(s)).unwrap_or(0.0);
        // The clock offset is applied per sample from the absolute frame, so it is a
        // genuine linear phase ramp rather than a per-window constant.
        let samples = (0..frames)
            .map(|i| {
                let f = first_frame + i as u64;
                let drift = self.drift_ms_per_s * (f as f64 / f64::from(self.rate));
                self.sample(f, arrival + drift)
            })
            .collect();
        Some(MicWindow { samples, first_frame, sample_rate: self.rate, gap: false, clipped: false })
    }
}

/// A synthetic capture of the click track at a chosen burst amplitude over a
/// chosen noise floor, for the signal check's verdict logic.
///
/// `first_frame` is the absolute position the window claims, i.e. where it sits
/// on the estimator's analysis grid. It matters for a short window: the
/// estimator only keeps a pattern period it saw *whole*, so the number of
/// analysed periods depends on the alignment as well as the length.
pub(super) fn signal_window_at(first_frame: u64, rate: u32, pattern_ms: f64, periods: usize, amp: f64, noise: f64) -> MicWindow {
    let mut w = signal_window(rate, pattern_ms, periods, amp, noise);
    w.first_frame = first_frame;
    w
}

pub(super) fn signal_window(rate: u32, pattern_ms: f64, periods: usize, amp: f64, noise: f64) -> MicWindow {
    let r = f64::from(rate);
    let period = pattern_ms / 1000.0 * r;
    let burst = 0.008 * r;
    let frames = (period * periods as f64) as usize;
    // Deterministic pseudo-noise (an LCG), so the test cannot flake.
    let mut seed = 0x2545_F491_4F6C_DD1Du64;
    let samples = (0..frames)
        .map(|i| {
            let t = (i as f64).rem_euclid(period);
            let mut v = 0.0;
            for (offset, hz) in [(0.0, 3000.0), (period / 2.0, 1500.0)] {
                let k = t - offset;
                if (0.0..=burst).contains(&k) {
                    let env = 0.5 - 0.5 * (TAU * k / burst).cos();
                    v += amp * env * (TAU * hz * k / r).sin();
                }
            }
            seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
            let n = ((seed >> 33) as f64 / f64::from(u32::MAX >> 1)) - 1.0;
            (v + n * noise) as f32
        })
        .collect();
    MicWindow { samples, first_frame: 0, sample_rate: rate, gap: false, clipped: false }
}

#[derive(Default)]
pub(super) struct FakeWriter {
    pub(super) writes: Mutex<Vec<(String, u16)>>,
}

impl FakeWriter {
    /// The knob value this member last had written to it — what a device would be
    /// rendering at.
    pub(super) fn last(&self, node_name: &str) -> Option<u16> {
        self.writes.lock_recover().iter().rev().find(|(n, _)| n == node_name).map(|(_, ms)| *ms)
    }

    /// How many times this member's knob has been written, i.e. how many reconnects
    /// it has been through.
    pub(super) fn count(&self, node_name: &str) -> usize {
        self.writes.lock_recover().iter().filter(|(n, _)| n == node_name).count()
    }
}

impl DelayWriter for FakeWriter {
    fn write(&self, node_name: String, _kind: MemberKind, delay_ms: u16) -> Fut<'_, Result<String, String>> {
        Box::pin(async move {
            self.writes.lock_recover().push((node_name.clone(), delay_ms));
            // Worded like the real sendspin handler's reply, because W21 reads it:
            // whether a write said it forced a reconnect is evidence about what the
            // two device-arm baselines are separated by (`api/measure.rs`'s
            // `set_sendspin_delay_handler`).
            Ok(format!("set '{node_name}' static delay to {delay_ms} ms (reconnecting just this speaker to apply)"))
        })
    }
}

/// Production timing throughout — the async tests below run on `tokio`'s
/// **paused** clock (`start_paused`), so a 3 s mute guard and four 2 s pattern
/// periods per member cost no wall-clock time. The fake capture derives its
/// frame count from `tokio::time::Instant`, so virtual time produces real
/// audio: the estimator, the gate, the drift fit and the solve all run against
/// the same numbers a real run uses.
pub(super) fn deps_for(arrivals: &[(&str, f64)]) -> (MeasureDeps, Arc<FakeWriter>, Arc<AtomicBool>, Arc<AtomicBool>) {
    let rig = Rig::new(arrivals, Mode::SweetSpot, 0.0);
    let (writer, active, connected) = (rig.writer.clone(), rig.active.clone(), rig.connected.clone());
    (rig.deps, writer, active, connected)
}

/// Everything a test needs to reach into a run: the injected truth, the transport's
/// failure levers, and the deps the run is driven with.
pub(super) struct Rig {
    pub(super) deps: MeasureDeps,
    pub(super) writer: Arc<FakeWriter>,
    pub(super) active: Arc<AtomicBool>,
    pub(super) connected: Arc<AtomicBool>,
    /// The true per-member arrival the fake capture renders. Mutable mid-run.
    pub(super) arrivals: Arc<Mutex<HashMap<String, f64>>>,
    /// Absolute frame at which the capture should look like it reconnected
    /// (plan §1.2). See [`FakeMic::reconnect_at`].
    pub(super) reconnect_at: Arc<AtomicU64>,
    /// The fake capture, so a test can read its frame clock to schedule the above.
    pub(super) mic: Arc<FakeMic>,
    pub(super) levels: Arc<Mutex<HashMap<String, u8>>>,
    /// The provisional delay line the chain applies through (plan §1.1.1), and the
    /// thing that makes an overlap's *next* reading include the delay it is carrying.
    pub(super) relay: Arc<FakeRelay>,
}

/// A provisional delay moves the speaker it is applied to — the physics the whole
/// chain rests on, since a later position measures each overlap *through* the line it
/// is carrying (plan §1.1).
///
/// Deliberately nothing else: the line's own sample arithmetic and its cap are
/// `relay_delay`'s tests, and the relay-vs-device *equivalence* is W21's.
pub(super) struct RelayShift {
    pub(super) relay: Arc<FakeRelay>,
}

impl ArrivalShift for RelayShift {
    fn shift_ms(&self, member: &str) -> f64 {
        self.relay.applied_ms(member)
    }
}

impl Rig {
    pub(super) fn new(arrivals: &[(&str, f64)], mode: Mode, drift_ms_per_s: f64) -> Self {
        Self::of_kinds(&arrivals.iter().map(|(n, a)| (*n, MemberKind::Sendspin, *a)).collect::<Vec<_>>(), mode, drift_ms_per_s)
    }

    /// The same rig with an explicit kind per member, for the mixed-polarity cases
    /// §2.4.2 is about.
    pub(super) fn of_kinds(arrivals: &[(&str, MemberKind, f64)], mode: Mode, drift_ms_per_s: f64) -> Self {
        let timing = Timing::real();
        let members: Vec<SessionMember> =
            arrivals.iter().map(|(n, k, _)| SessionMember { node_name: (*n).to_string(), kind: *k }).collect();
        let soloed = Arc::new(Mutex::new(None));
        let active = Arc::new(AtomicBool::new(true));
        let connected = Arc::new(AtomicBool::new(true));
        let levels = Arc::new(Mutex::new(HashMap::new()));
        let arrivals: Arc<Mutex<HashMap<String, f64>>> =
            Arc::new(Mutex::new(arrivals.iter().map(|(n, _, a)| ((*n).to_string(), *a)).collect()));
        let reconnect_at = Arc::new(AtomicU64::new(0));
        let session = Arc::new(FakeSession {
            members,
            soloed: soloed.clone(),
            active: active.clone(),
            interference: Arc::new(Mutex::new(Vec::new())),
            levels: levels.clone(),
        });
        // Every rig carries a line, and an unused one applies nothing — so the tests
        // that predate W12 measure exactly what they measured before.
        let relay = Arc::new(FakeRelay::new());
        let mic = Arc::new(FakeMic {
            rate: 48_000,
            pattern_ms: timing.pattern_ms,
            soloed,
            arrivals: arrivals.clone(),
            start: Instant::now(),
            frames: AtomicU64::new(0),
            connected: connected.clone(),
            drift_ms_per_s,
            reconnect_at: reconnect_at.clone(),
            shift: Arc::new(RelayShift { relay: relay.clone() }),
        });
        let writer = Arc::new(FakeWriter::default());
        let deps = MeasureDeps {
            mode,
            chained: false,
            link_to: Vec::new(),
            session,
            mic: mic.clone(),
            writer: writer.clone(),
            relay: relay.clone(),
            current_delays: HashMap::new(),
            send_ahead: SendAheadContext::default(),
            timing,
        };
        Rig { deps, writer, active, connected, arrivals, reconnect_at, levels, mic, relay }
    }
}

pub(super) fn manager() -> MeasureManager {
    MeasureManager { inner: Arc::new(Mutex::new(Inner::idle())) }
}

/// Poll the run's own status until it says what the test is waiting for. On the
/// paused clock these sleeps cost nothing and the fake capture's frame clock
/// advances with them, so a whole walk runs in milliseconds of real time.
pub(super) async fn wait_for(m: &MeasureManager, what: &str, pred: impl Fn(&MeasureStatus) -> bool) -> MeasureStatus {
    for _ in 0..40_000u32 {
        let s = m.status();
        if pred(&s) {
            return s;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let s = m.status();
    panic!("timed out waiting for {what}; phase {:?}, walk {:?}, message {}", s.phase, s.walk.map(|w| w.next), s.message);
}

pub(super) async fn wait_ready(m: &MeasureManager, next: WalkAction) -> MeasureStatus {
    wait_for(m, "the walk to be ready", |s| s.walk.as_ref().is_some_and(|w| w.next == next)).await
}

pub(super) async fn wait_terminal(m: &MeasureManager) -> MeasureStatus {
    wait_for(m, "the run to reach a terminal state", |s| matches!(s.phase, Phase::Proposed | Phase::Refused | Phase::Done)).await
}

/// Drive a whole walk exactly as the UI would: one arrival per speaker in the
/// user's own order, then the closure back at the first one.
pub(super) async fn walk_all(m: &MeasureManager, order: &[&str]) {
    for name in order {
        wait_ready(m, WalkAction::Arrival).await;
        m.arrival((*name).to_string(), None).unwrap_or_else(|r| panic!("arrival at '{name}' refused: {}", r.message));
    }
    let s = wait_ready(m, WalkAction::Close).await;
    assert!(s.walk.as_ref().is_some_and(|w| w.remaining.is_empty()));
    m.close().unwrap_or_else(|r| panic!("close refused: {}", r.message));
}

pub(super) fn proposed(s: &MeasureStatus, node: &str) -> ProposedDelay {
    s.proposal.as_ref().expect("a proposal").members.iter().find(|m| m.node_name == node).expect("the member").clone()
}

/// A chained run over synthetic audio, with the provisional delay line wired to the
/// physics ([`RelayShift`]): a delay the chain applies really does move that speaker,
/// which is what makes an overlap's *next* reading include it (plan §1.1).
pub(super) fn chain_rig(members: &[&str]) -> Rig {
    let mut rig = Rig::new(&members.iter().map(|n| (*n, 0.0)).collect::<Vec<_>>(), Mode::SweetSpot, 0.0);
    rig.deps.chained = true;
    rig
}

pub(super) fn chain_rig_of(members: &[(&str, MemberKind)]) -> Rig {
    let mut rig = Rig::of_kinds(&members.iter().map(|(n, k)| (*n, *k, 0.0)).collect::<Vec<_>>(), Mode::SweetSpot, 0.0);
    rig.deps.chained = true;
    rig
}

pub(super) async fn wait_chain(m: &MeasureManager) -> MeasureStatus {
    wait_for(m, "the chain to be waiting for a position", |s| {
        s.chain.as_ref().is_some_and(|c| matches!(c.next, ChainAction::Position | ChainAction::Finish))
    })
    .await
}

pub(super) fn chain_of(s: &MeasureStatus) -> ChainProgress {
    s.chain.clone().unwrap_or_else(|| panic!("a chain; phase {:?}, message {}", s.phase, s.message))
}

pub(super) fn applied(s: &MeasureStatus, node: &str) -> u16 {
    chain_of(s).provisional.iter().find(|p| p.node_name == node).map(|p| p.applied_ms).unwrap_or_else(|| {
        panic!(
            "'{node}' has no provisional delay; lines are {:?}",
            chain_of(s).provisional.iter().map(|p| &p.node_name).collect::<Vec<_>>()
        )
    })
}

/// Stand at a listening position: tell the fake capture what is audible from there —
/// wire delay **plus that spot's path** per speaker, exactly as a mic in one place
/// hears it (plan §1) — and then post the position.
///
/// The provisional delays are *not* part of these numbers: the rig's [`RelayShift`]
/// adds whatever the line is carrying, which is the whole point.
pub(super) async fn at_position(
    m: &MeasureManager,
    arrivals: &Arc<Mutex<HashMap<String, f64>>>,
    heard: &[(&str, f64)],
    members: &[&str],
    overlaps: &[&str],
) {
    wait_chain(m).await;
    *arrivals.lock_recover() = heard.iter().map(|(n, a)| ((*n).to_string(), *a)).collect();
    m.position(members.iter().map(|s| (*s).to_string()).collect(), overlaps.iter().map(|s| (*s).to_string()).collect())
        .unwrap_or_else(|r| panic!("position {members:?} through {overlaps:?} refused: {}", r.message));
}

pub(super) fn walk_obs(name: &str, centre: f64, phase_a: f64) -> MemberObservation {
    obs(name, 0, centre, phase_a, 0.0)
}

/// The provisional delay line as the experiment drives it: a **real**
/// [`crate::align::relay_delay::RelayDelay`] for the value arithmetic, the cap and — the
/// part that matters here — the real priming state, plus a stand-in for the relay
/// thread, because a line only fills from audio that actually flows and there is no
/// PipeWire graph in a unit test.
///
/// The pushed audio is silence: this fake's job is the line's *state*, and the
/// arrival shift a delay produces is injected separately ([`EquivPhysics`]) so a
/// test can inject one that disagrees with it.
pub(super) struct FakeRelay {
    pub(super) rd: crate::align::relay_delay::RelayDelay,
    /// Frames the stand-in relay thread pushes per status poll. 480 (10 ms) is
    /// deliberately less than the 20 ms step, so priming takes more than one poll and
    /// the wait is a tested path rather than a no-op.
    pub(super) frames_per_poll: usize,
    pub(super) buf: Mutex<Vec<u8>>,
}

impl FakeRelay {
    pub(super) fn new() -> Self {
        Self { rd: crate::align::relay_delay::RelayDelay::new(), frames_per_poll: 480, buf: Mutex::new(Vec::new()) }
    }

    /// What the line is applying right now, in ms — read *without* pumping, because
    /// the arrival physics reads it once per capture window.
    pub(super) fn applied_ms(&self, output: &str) -> f64 {
        self.rd.status(output).map(|s| crate::align::relay_delay::us_for_frames(s.delay_frames, s.rate) as f64 / 1000.0).unwrap_or(0.0)
    }

    pub(super) fn pump(&self, output: &str) {
        let mut buf = self.buf.lock_recover();
        let src = vec![0u8; self.frames_per_poll * 4];
        let _ = self.rd.delay_into(output, crate::align::relay_delay::PcmFormat::new(48_000, 2), &src, &mut buf);
    }
}

impl RelayControl for FakeRelay {
    fn set_delay_ms(&self, output: &str, delay_ms: u16) -> Result<(), String> {
        self.rd.set_delay_us(output, u64::from(delay_ms) * 1_000).map(|_| ()).map_err(|e| e.to_string())
    }

    fn status(&self, output: &str) -> Option<crate::align::relay_delay::DelayStatus> {
        self.pump(output); // the relay thread ran between two polls
        self.rd.status(output)
    }

    fn clear(&self, output: &str) {
        self.rd.clear(output);
    }
}

/// The physics the experiment exists to discover, injected: how far a provisional
/// delay moves this speaker, how far and **which way** its knob does, and what a
/// reconnect does on its own.
pub(super) struct EquivPhysics {
    pub(super) member: String,
    pub(super) relay: Arc<FakeRelay>,
    pub(super) writer: Arc<FakeWriter>,
    pub(super) baseline_knob: u16,
    pub(super) inject: EquivInject,
}

/// Everything the W21 tests vary. The defaults are a speaker that behaves exactly as
/// plan §2.4.1 and §1.1.1 say it should.
#[derive(Debug, Clone, Copy)]
pub(super) struct EquivInject {
    /// ms of arrival shift per ms of provisional delay. `1.0` = the delay line does
    /// what it says.
    pub(super) relay_per_ms: f64,
    /// ms of arrival shift per ms the knob is **raised**. `-1.0` is §2.4.1's advance,
    /// `+1.0` a delay, `-1.15` a 15 % scale error, `0.0` a knob nobody honours.
    pub(super) device_per_ms: f64,
    /// A constant this speaker's arrival gains the moment its knob is written at all
    /// — the ε of §1.1.2 item 3, and the constant a two-point device arm must cancel.
    pub(super) reconnect_eps_ms: f64,
    /// Mic-vs-audio clock drift, ms of phase per second (0.1 = 100 ppm, §5.4.1's
    /// realistic phone).
    pub(super) drift_ms_per_s: f64,
    /// The speaker goes silent once this many writes have landed (plan §2.3.2's
    /// wedged member).
    pub(super) silent_after_writes: Option<usize>,
    /// Shorten **only** the gate timeouts. Every other quantity stays production —
    /// see the test that uses it for why.
    pub(super) short_timeouts: bool,
}

impl Default for EquivInject {
    fn default() -> Self {
        Self {
            relay_per_ms: 1.0,
            device_per_ms: -1.0,
            reconnect_eps_ms: 0.0,
            drift_ms_per_s: 0.0,
            silent_after_writes: None,
            short_timeouts: false,
        }
    }
}

impl ArrivalShift for EquivPhysics {
    fn shift_ms(&self, member: &str) -> f64 {
        if member != self.member {
            return 0.0;
        }
        let knob = self.writer.last(member).unwrap_or(self.baseline_knob);
        let raised = f64::from(knob) - f64::from(self.baseline_knob);
        let reconnected = self.writer.count(member) > 0;
        self.relay.applied_ms(member) * self.inject.relay_per_ms
            + raised * self.inject.device_per_ms
            + if reconnected { self.inject.reconnect_eps_ms } else { 0.0 }
    }

    fn silent(&self, member: &str) -> bool {
        member == self.member && self.inject.silent_after_writes.is_some_and(|n| self.writer.count(member) >= n)
    }
}

pub(super) struct EquivRig {
    pub(super) deps: EquivalenceDeps,
    pub(super) relay: Arc<FakeRelay>,
    pub(super) writer: Arc<FakeWriter>,
}

/// Assemble the experiment over one group. `members` carries each member's kind and
/// its **current** knob value; `target` is the speaker whose physics is injected —
/// which is the one the planner is expected to pick.
pub(super) fn equiv_rig(members: &[(&str, MemberKind, u16)], target: &str, inject: EquivInject) -> EquivRig {
    let timing = match inject.short_timeouts {
        true => Timing { gate_settle_timeout: Duration::from_secs(20), gate_reconnect_timeout: Duration::from_secs(30), ..Timing::real() },
        false => Timing::real(),
    };
    let soloed = Arc::new(Mutex::new(None));
    let session_members: Vec<SessionMember> =
        members.iter().map(|(n, k, _)| SessionMember { node_name: (*n).to_string(), kind: *k }).collect();
    // Deliberately not zero: an advance of 20 ms on a speaker arriving at 0 ms would
    // wrap the pattern, and this experiment is about the *shift*, not the arrival.
    let arrivals: Arc<Mutex<HashMap<String, f64>>> =
        Arc::new(Mutex::new(members.iter().enumerate().map(|(i, (n, _, _))| ((*n).to_string(), 30.0 + i as f64 * 4.0)).collect()));
    let current_delays: HashMap<String, u16> = members.iter().map(|(n, _, ms)| ((*n).to_string(), *ms)).collect();
    let session = Arc::new(FakeSession {
        members: session_members,
        soloed: soloed.clone(),
        active: Arc::new(AtomicBool::new(true)),
        interference: Arc::new(Mutex::new(Vec::new())),
        levels: Arc::new(Mutex::new(HashMap::new())),
    });
    let relay = Arc::new(FakeRelay::new());
    let writer = Arc::new(FakeWriter::default());
    let physics = Arc::new(EquivPhysics {
        member: target.to_string(),
        relay: relay.clone(),
        writer: writer.clone(),
        baseline_knob: current_delays.get(target).copied().unwrap_or(0),
        inject,
    });
    let mic = Arc::new(FakeMic {
        rate: 48_000,
        pattern_ms: timing.pattern_ms,
        soloed,
        arrivals,
        start: Instant::now(),
        frames: AtomicU64::new(0),
        connected: Arc::new(AtomicBool::new(true)),
        drift_ms_per_s: inject.drift_ms_per_s,
        reconnect_at: Arc::new(AtomicU64::new(0)),
        shift: physics,
    });
    let deps = EquivalenceDeps {
        base: MeasureDeps {
            mode: Mode::SweetSpot,
            chained: false,
            link_to: Vec::new(),
            session,
            mic,
            writer: writer.clone(),
            relay: relay.clone(),
            current_delays,
            send_ahead: SendAheadContext::default(),
            timing,
        },
        member: None,
    };
    EquivRig { deps, relay, writer }
}

/// Run the whole experiment, restore included, and hand back its final status.
pub(super) async fn run_equiv(rig: EquivRig) -> (EquivalenceStatus, Arc<FakeWriter>, Arc<FakeRelay>) {
    let EquivRig { deps, relay, writer } = rig;
    let st = EquivState::new();
    let cancel = Arc::new(AtomicBool::new(false));
    drive_equivalence(deps, st.clone(), cancel).await;
    (st.status(), writer, relay)
}

pub(super) fn report_of(s: &EquivalenceStatus) -> EquivalenceReport {
    s.report.clone().unwrap_or_else(|| panic!("a report; phase {:?}, message {}", s.phase, s.message))
}

pub(super) fn equiv_members(kinds: &[(&str, MemberKind)]) -> Vec<SessionMember> {
    kinds.iter().map(|(n, k)| SessionMember { node_name: (*n).to_string(), kind: *k }).collect()
}
