//! Latency-alignment ("calibration") session — the backend for the Align page.
//!
//! Aligning a sync group is done **by ear** (the daemon has no latency
//! *measurement*): a test signal plays on every member off the group's one
//! clock, the user makes exactly two members audible — a fixed **reference** and
//! the **target** being tuned — and drags the target's delay until the two
//! coincide, then moves to the next target.
//!
//! ## Test signal
//!
//! An **alternating two-tone click** (a high "A" click, then a low "B" click,
//! one second apart → a 2 s loop, [`click_wav`]). A single uniform click would
//! be ambiguous once a member's delay approaches the click spacing — you can't
//! tell which click you're lining up. The A/B alternation disambiguates: a
//! target that has slipped a whole click lands its A on the reference's B, which
//! is audibly wrong, so offsets up to ~2 s (covering RAOP's ~1.5 s max) are
//! unmistakable.
//!
//! ## Why it's server-owned
//!
//! The session mutes the non-audible members (per-device sendspin volume / RAOP
//! node volume) and runs a looping player into the group's sync anchor. If that
//! lived in the browser, a closed tab would leave speakers muted and a click
//! looping forever. So the daemon owns it: it snapshots volumes on start,
//! restores them on stop, and arms a safety timeout that tears the session down
//! if the UI goes away.
//!
//! Adjusting a member's offset reuses the existing knobs (sendspin static delay
//! — live; RAOP `raop.latency.ms` — reloads the sink, a brief gap), so this
//! module only owns playback + muting, not the persisted offsets.

use crate::locks::LockRecover;
use crate::pw_thread::SharedState;
use crate::sendspin_volume::SharedSendspinControl;
use crate::sync_group::SharedGroups;
use serde::Serialize;
use std::collections::HashMap;
use std::f64::consts::PI;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

const RATE: u32 = 44_100;
const CHANNELS: u16 = 2;
/// Full A→B loop length. One click per second → a 2 s pattern.
const PATTERN_SECS: f64 = 2.0;
const CLICK_MS: f64 = 8.0;
const FREQ_A: f64 = 3000.0;
const FREQ_B: f64 = 1500.0;
const CLICK_AMP: f64 = 0.5;

/// Default level (0–100) audible members play at during calibration — the same
/// for both compared members so the ear judges timing, not loudness. Kept
/// modest (calibration clicks are loud and it's usually a hands-on, close-range
/// task); the user can raise/lower it live from the Align page.
const DEFAULT_CAL_VOLUME: u8 = 50;

/// Safety net: never leave a group muted with a click looping if the UI
/// vanishes. The session tears itself down after this if not stopped.
const SESSION_TIMEOUT: Duration = Duration::from_secs(15 * 60);

/// Builds the alternating two-tone click WAV (S16LE stereo, one 2 s loop).
pub fn click_wav() -> Vec<u8> {
    let total = (PATTERN_SECS * RATE as f64) as usize; // frames
    let half = total / 2;
    let click_frames = (CLICK_MS / 1000.0 * RATE as f64) as usize;
    let mut pcm = Vec::with_capacity(total * CHANNELS as usize * 2);
    for i in 0..total {
        // Click A burst at the pattern start, click B burst at the half point.
        let s = if i < click_frames {
            click_sample(i, click_frames, FREQ_A)
        } else if i >= half && i < half + click_frames {
            click_sample(i - half, click_frames, FREQ_B)
        } else {
            0.0
        };
        let v = (s * CLICK_AMP * f64::from(i16::MAX)) as i16;
        let le = v.to_le_bytes();
        pcm.extend_from_slice(&le); // FL
        pcm.extend_from_slice(&le); // FR
    }
    crate::wav::build_wav(&pcm, RATE, 16, CHANNELS)
}

/// One burst sample: a sine at `freq` under a Hann envelope over the `n`-sample
/// burst, so it starts/ends at zero (no pop that would itself smear timing).
fn click_sample(i: usize, n: usize, freq: f64) -> f64 {
    let t = i as f64 / f64::from(RATE);
    let env = 0.5 - 0.5 * (2.0 * PI * i as f64 / n as f64).cos();
    (2.0 * PI * freq * t).sin() * env
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MemberKind {
    Sendspin,
    Raop,
}

#[derive(Debug, Clone, Serialize)]
pub struct AlignMember {
    pub node_name: String,
    pub kind: MemberKind,
    /// Live PipeWire node id (RAOP members only) — for muting; `None` sendspin.
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
    pub sources: Vec<String>,
    /// The fixed member everything is aligned against.
    pub reference: Option<String>,
    /// The member currently being tuned (audible alongside the reference).
    pub target: Option<String>,
    pub members: Vec<AlignMember>,
    /// Playback level (0–100) of the audible members.
    pub volume: u8,
}

impl AlignState {
    fn inactive() -> Self {
        Self { active: false, sources: Vec::new(), reference: None, target: None, members: Vec::new(), volume: DEFAULT_CAL_VOLUME }
    }
}

struct Session {
    sources: Vec<String>,
    members: Vec<AlignMember>,
    reference: Option<String>,
    target: Option<String>,
    /// Playback level (0–100) applied to the audible members.
    volume: u8,
    /// Set to stop the looping player thread.
    stop: Arc<AtomicBool>,
    /// Volumes captured on start, restored on teardown.
    saved_sendspin: HashMap<String, u8>,
    saved_raop: HashMap<u32, f32>,
}

impl Session {
    fn state(&self) -> AlignState {
        AlignState {
            active: true,
            sources: self.sources.clone(),
            reference: self.reference.clone(),
            target: self.target.clone(),
            members: self.members.clone(),
            volume: self.volume,
        }
    }

    fn is_member(&self, node_name: &str) -> bool {
        self.members.iter().any(|m| m.node_name == node_name)
    }
}

/// The alignment session manager (one at a time). Cloneable — holds shared
/// handles + the cached click WAV.
#[derive(Clone)]
pub struct AlignManager {
    session: Arc<tokio::sync::Mutex<Option<Session>>>,
    pw: SharedState,
    sendspin: SharedSendspinControl,
    groups: SharedGroups,
    click: Arc<Vec<u8>>,
}

fn same_set(a: &[String], b: &[String]) -> bool {
    let mut a: Vec<&str> = a.iter().map(String::as_str).collect();
    let mut b: Vec<&str> = b.iter().map(String::as_str).collect();
    a.sort_unstable();
    b.sort_unstable();
    a == b
}

impl AlignManager {
    pub fn new(pw: SharedState, sendspin: SharedSendspinControl, groups: SharedGroups) -> Self {
        Self { session: Arc::new(tokio::sync::Mutex::new(None)), pw, sendspin, groups, click: Arc::new(click_wav()) }
    }

    /// Build member lists for every running group (the picker's source of truth).
    pub async fn groups(&self) -> Vec<AlignGroup> {
        let snap = self.groups.lock().await.snapshot();
        let pw = self.pw.lock_recover();
        snap.into_iter()
            .map(|g| {
                let mut members = Vec::new();
                for n in g.sendspin_members {
                    members.push(AlignMember { node_name: n, kind: MemberKind::Sendspin, node_id: None });
                }
                for n in g.raop_members {
                    let node_id = crate::routing::node_id_for(&pw, &n);
                    members.push(AlignMember { node_name: n, kind: MemberKind::Raop, node_id });
                }
                AlignGroup { sources: g.sources, members }
            })
            .collect()
    }

    /// Current state (inactive if no session is running).
    pub async fn status(&self) -> AlignState {
        self.session.lock().await.as_ref().map(Session::state).unwrap_or_else(AlignState::inactive)
    }

    /// Start a session for the group identified by its `sources` set. Any
    /// existing session is torn down first. Reference/target default to the
    /// first two members; the caller re-selects from there.
    pub async fn start(&self, sources: Vec<String>) -> Result<AlignState, String> {
        let mut guard = self.session.lock().await;
        if let Some(old) = guard.take() {
            self.teardown(old).await;
        }

        // Resolve the group + members from the live layout.
        let group = {
            let snap = self.groups.lock().await.snapshot();
            snap.into_iter().find(|g| same_set(&g.sources, &sources)).ok_or("no such active group")?
        };
        let anchor = group.anchor_node_id;
        let mut members = Vec::new();
        for n in &group.sendspin_members {
            members.push(AlignMember { node_name: n.clone(), kind: MemberKind::Sendspin, node_id: None });
        }
        {
            let pw = self.pw.lock_recover();
            for n in &group.raop_members {
                members.push(AlignMember { node_name: n.clone(), kind: MemberKind::Raop, node_id: crate::routing::node_id_for(&pw, n) });
            }
        }
        if members.len() < 2 {
            return Err("a group needs at least two present members to align".to_string());
        }

        // Snapshot volumes so teardown can restore them.
        let mut saved_sendspin = HashMap::new();
        {
            let vols = self.sendspin.lock().await.volumes();
            for m in &members {
                if m.kind == MemberKind::Sendspin {
                    saved_sendspin.insert(m.node_name.clone(), vols.get(&m.node_name).copied().unwrap_or(100));
                }
            }
        }
        let mut saved_raop = HashMap::new();
        for m in &members {
            if let (MemberKind::Raop, Some(id)) = (m.kind, m.node_id) {
                if let Ok(Some(v)) = crate::volume::get_volume(id).await {
                    saved_raop.insert(id, v);
                }
            }
        }

        // Loop the click into the anchor until stopped.
        let stop = Arc::new(AtomicBool::new(false));
        {
            let click = self.click.clone();
            let stop = stop.clone();
            tokio::task::spawn_blocking(move || {
                if let Err(e) = crate::player::play_loop_to_target(anchor, &click, stop) {
                    tracing::warn!("alignment playback ended with error: {e}");
                }
            });
        }

        let reference = Some(members[0].node_name.clone());
        let target = Some(members[1].node_name.clone());
        let session = Session {
            sources: group.sources,
            members,
            reference,
            target,
            volume: DEFAULT_CAL_VOLUME,
            stop: stop.clone(),
            saved_sendspin,
            saved_raop,
        };
        self.apply_audibility(&session.members, session.reference.as_deref(), session.target.as_deref(), session.volume).await;
        let state = session.state();
        *guard = Some(session);
        drop(guard);

        // Safety timeout: tear down if still the same session after the deadline.
        self.arm_timeout(stop);
        tracing::info!("alignment session started for group {:?}", state.sources);
        Ok(state)
    }

    /// Set which two members are audible (reference vs. the target being tuned).
    pub async fn select(&self, reference: String, target: String) -> Result<AlignState, String> {
        let (members, reference, target, volume, state) = {
            let mut guard = self.session.lock().await;
            let session = guard.as_mut().ok_or("no alignment session is running")?;
            if !session.is_member(&reference) || !session.is_member(&target) {
                return Err("reference and target must both be members of the active group".to_string());
            }
            session.reference = Some(reference);
            session.target = Some(target);
            (session.members.clone(), session.reference.clone(), session.target.clone(), session.volume, session.state())
        };
        // Re-solo against the new pair (lock released — apply_audibility awaits).
        self.apply_audibility(&members, reference.as_deref(), target.as_deref(), volume).await;
        Ok(state)
    }

    /// Set the audible members' playback level (0–100) live.
    pub async fn set_level(&self, volume: u8) -> Result<AlignState, String> {
        let volume = volume.min(100);
        let (members, reference, target, state) = {
            let mut guard = self.session.lock().await;
            let session = guard.as_mut().ok_or("no alignment session is running")?;
            session.volume = volume;
            (session.members.clone(), session.reference.clone(), session.target.clone(), session.state())
        };
        self.apply_audibility(&members, reference.as_deref(), target.as_deref(), volume).await;
        Ok(state)
    }

    /// Stop the session and restore volumes.
    pub async fn stop(&self) -> AlignState {
        let session = self.session.lock().await.take();
        if let Some(s) = session {
            tracing::info!("alignment session stopped for group {:?}", s.sources);
            self.teardown(s).await;
        }
        AlignState::inactive()
    }

    /// Solo the reference + target at `volume`; mute everything else. Sendspin
    /// uses the protocol Mute command (leaving the stored volume intact); RAOP
    /// uses the sink node volume (0 to mute).
    async fn apply_audibility(&self, members: &[AlignMember], reference: Option<&str>, target: Option<&str>, volume: u8) {
        for m in members {
            let audible = reference == Some(m.node_name.as_str()) || target == Some(m.node_name.as_str());
            match m.kind {
                MemberKind::Sendspin => {
                    let mut c = self.sendspin.lock().await;
                    c.set_mute(&m.node_name, !audible).await;
                    if audible {
                        c.set_volume(&m.node_name, volume).await;
                    }
                }
                MemberKind::Raop => {
                    if let Some(id) = self.raop_node_id(m) {
                        let v = if audible { f32::from(volume) / 100.0 } else { 0.0 };
                        let _ = crate::volume::set_volume(id, v).await;
                    }
                }
            }
        }
    }

    /// A RAOP member's live node id, re-resolving against the graph if it wasn't
    /// known at session start (e.g. the sink was reloading).
    fn raop_node_id(&self, m: &AlignMember) -> Option<u32> {
        m.node_id.or_else(|| crate::routing::node_id_for(&self.pw.lock_recover(), &m.node_name))
    }

    /// Stop playback, unmute every sendspin member, and restore saved volumes.
    async fn teardown(&self, session: Session) {
        session.stop.store(true, Ordering::Relaxed);
        {
            let mut c = self.sendspin.lock().await;
            for m in &session.members {
                if m.kind == MemberKind::Sendspin {
                    c.set_mute(&m.node_name, false).await;
                }
            }
            for (n, v) in &session.saved_sendspin {
                c.set_volume(n, *v).await;
            }
        }
        for (id, v) in &session.saved_raop {
            let _ = crate::volume::set_volume(*id, *v).await;
        }
    }

    /// Spawn a watchdog that tears the session down after `SESSION_TIMEOUT`,
    /// but only if it's still the very session identified by `stop` (a newer
    /// session has its own `stop`, so a restart doesn't get killed early).
    fn arm_timeout(&self, stop: Arc<AtomicBool>) {
        let session = self.session.clone();
        let this = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(SESSION_TIMEOUT).await;
            if stop.load(Ordering::Relaxed) {
                return; // already stopped
            }
            let taken = {
                let mut guard = session.lock().await;
                match guard.as_ref() {
                    Some(s) if Arc::ptr_eq(&s.stop, &stop) => guard.take(),
                    _ => None,
                }
            };
            if let Some(s) = taken {
                tracing::info!("alignment session timed out; restoring volumes");
                this.teardown(s).await;
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn click_wav_is_a_valid_two_second_stereo_pattern() {
        let wav = click_wav();
        // RIFF/WAVE header present.
        assert_eq!(&wav[0..4], b"RIFF");
        assert_eq!(&wav[8..12], b"WAVE");
        // Stereo, 44100, 16-bit.
        assert_eq!(u16::from_le_bytes(wav[22..24].try_into().unwrap()), CHANNELS);
        assert_eq!(u32::from_le_bytes(wav[24..28].try_into().unwrap()), RATE);
        // data length = 2 s * rate * channels * 2 bytes.
        let expect = (PATTERN_SECS * RATE as f64) as usize * CHANNELS as usize * 2;
        assert_eq!(u32::from_le_bytes(wav[40..44].try_into().unwrap()) as usize, expect);
    }

    #[test]
    fn same_set_ignores_order_and_dupes_only_by_value() {
        assert!(same_set(&["a".into(), "b".into()], &["b".into(), "a".into()]));
        assert!(!same_set(&["a".into()], &["a".into(), "b".into()]));
    }
}
