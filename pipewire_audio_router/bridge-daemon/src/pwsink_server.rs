//! Per-target **pw-sink senders** for a sync group — the audio path to remote
//! PipeWire hosts running `libpipewire-module-rtp-session`.
//!
//! Mirrors `ap2_server::start`, but the transport is the AppleMIDI/RTP audio
//! sender (`applemidi_sender::AppleMidiSender`) instead of the vendored AirPlay-2
//! sender. For each pw-sink target in a group it advertises one
//! `_pipewire-audio._udp` session (`pwrouter-<slug>`) over the daemon's shared
//! storm-safe mDNS daemon; the target's `module-rtp-session` discovers it,
//! initiates the AppleMIDI handshake, and the sender streams L16 RTP to it. All
//! targets in the group are fed from the group's single captured anchor-monitor
//! PCM, so they play the same audio off the one clock.
//!
//! ## One session per target (per-device announce/duck)
//! Each target gets its **own** `AppleMidiSender`, fed a **per-device-mixed**
//! copy of the capture: the relay applies `overlay_mixer::mix_into(node_name,…)`
//! before handing the PCM to that target's sender, so an announcement can duck
//! one target's music independently — the must-have per-device overlay, obtained
//! exactly as the AP2 relay does. (A single shared session couldn't duck one
//! member without ducking all.)
//!
//! ## Deferred: multi-target routing scoping
//! Stock `module-rtp-session` in discover mode connects to *every* discovered
//! session of the matching media type — it does not filter by session name. So
//! with two+ pw-sink targets on one LAN, each receiver would connect to *both*
//! advertised sessions (cross-talk). The single-target separate-room case (the
//! primary use case) is unaffected. Scoping which receiver binds to which session
//! is a deliberately deferred decision (see docs/pipewire-sink-roadmap.md §4).
#![allow(dead_code)] // wired into sync_group.rs in the same phase

use std::sync::{Arc, Weak};
use std::time::Duration;

use crate::applemidi_sender::{AppleMidiSender, PcmChunk, SessionConfig, SessionFormat};
use crate::config::{PWSINK_DEV_PREFIX, PWSINK_SESSION_PREFIX};
use crate::pw_sink_liveness::{PwSinkLiveness, PwSinkStatus};

/// One target this group streams to: the virtual output node name
/// (`pwsink-dev-<slug>`) plus the concrete control port the daemon binds +
/// advertises for its session (data port = `control_port + 1`). The port is
/// allocated by the reconciler (sync_group.rs) so ports never collide across
/// groups.
#[derive(Clone, Debug)]
pub struct PwSinkMember {
    pub node_name: String,
    pub control_port: u16,
    /// This target's configured playout delay in ms — the `sess.latency.msec` its
    /// agent was told to run (`sync_settings::pwsink_jitter_effective`). Passed down
    /// because the sender sizes its catch-up burst and backlog ceiling against the
    /// far end's buffer; see [`crate::applemidi_sender::BacklogLimits`]. It is read
    /// when the session starts, so a change to the knob reaches this on the next
    /// session (re)start — which is also when the receiver reloads.
    pub playout_ms: u16,
}

/// A running group's pw-sink senders. Dropping it stops every target's session
/// (each `AppleMidiSender`'s Drop sends `BY`, withdraws its mDNS advert, and
/// closes its sockets) and drops the capture handle — which closes the capture
/// channel so the relay thread exits.
pub struct PwSinkServerHandle {
    /// One sender per target, kept alive here; drop = tear the session down. Held
    /// behind `Arc` only so the liveness poll task can hold a `Weak` and read
    /// `status()` without keeping the session alive — when this handle drops its
    /// `Arc`, the strong count hits zero and the sender tears down *immediately*
    /// (the poll task's `Weak` can't resurrect it), so `BY`/advert-withdraw are
    /// not delayed by the poll task's lifetime. Paired with the node name.
    senders: Vec<(String, Arc<AppleMidiSender>)>,
    /// Owned here (not by the relay) so this Drop closes the capture channel →
    /// the relay's `blocking_recv` returns `None` → the relay thread exits.
    _capture: crate::sendspin_capture::CaptureHandle,
    /// The RT relay thread; exits on its own once `_capture` closes the channel.
    _relay: std::thread::JoinHandle<()>,
    /// Liveness poll task (publishes each sender's `status()` into
    /// pw_sink_liveness). Aborted on drop; also self-exits once every `Weak` is dead.
    _status_task: tokio::task::JoinHandle<()>,
}

impl PwSinkServerHandle {
    /// Snapshot each target's current handshake status (for logging / tests).
    pub fn statuses(&self) -> Vec<(String, PwSinkStatus)> {
        self.senders
            .iter()
            .map(|(name, s)| {
                let st = s.status();
                (name.clone(), PwSinkStatus { established: st.established, peer_count: st.peer_count })
            })
            .collect()
    }
}

impl Drop for PwSinkServerHandle {
    fn drop(&mut self) {
        self._status_task.abort();
        for (name, _sender) in &self.senders {
            PwSinkLiveness::global().remove(name);
            crate::overlay_mixer::OverlayMixer::global().clear_output_rate(name);
        }
        // `senders` then drop (field order): each Arc<AppleMidiSender>'s strong
        // count hits zero → the sender's Drop tears down its session + advert.
        // `_capture` drops after → capture channel closes → relay exits.
    }
}

/// The AppleMIDI session name the daemon advertises for a target — the target's
/// virtual node name (`pwsink-dev-<slug>`) re-prefixed to `pwrouter-<slug>` so
/// discovery (pw_target_discovery.rs) filters it out of the target list.
pub fn session_name_for(node_name: &str) -> String {
    let slug = node_name.strip_prefix(PWSINK_DEV_PREFIX).unwrap_or(node_name);
    format!("{PWSINK_SESSION_PREFIX}{slug}")
}

/// Depth of one target's relay→sender PCM feed, in captured chunks.
///
/// **Bounded on purpose** (the rule the capture channel already follows —
/// `sendspin_capture::CAPTURE_CHANNEL_CAP`): an unbounded feed is unbounded
/// latency, since every chunk queued in it is audio the receiver will hear late.
/// At a ~21 ms quantum, 8 chunks is ~170 ms of absolute worst case, reached only
/// while a sender thread is not running at all; steady-state occupancy is 0-1.
/// Past it the relay drops the chunk and says so, rather than growing a queue
/// nobody can hear the end of.
const PCM_FEED_DEPTH: usize = 8;

/// Best-effort real-time scheduling for the capture→feed relay thread. Same
/// rationale + priority (40) as ap2_server's relay: the hop from capture to each
/// sender's PCM channel must never queue behind the daemon's general-purpose
/// async work, or a scheduling gap drops captured PCM before it is packetized.
fn set_relay_realtime_priority() {
    #[cfg(target_os = "linux")]
    unsafe {
        let mut param: libc::sched_param = std::mem::zeroed();
        param.sched_priority = 40;
        if libc::sched_setscheduler(0, libc::SCHED_FIFO, &param) == 0 {
            tracing::info!("pwsink relay: real-time priority set (SCHED_FIFO, priority 40)");
        } else {
            tracing::debug!("pwsink relay: could not set RT priority (need CAP_SYS_NICE); normal priority");
        }
    }
}

/// Start pw-sink senders for a group's `members`, capturing from the group's
/// anchor sink (`sink_node_id`, 48 kHz / S16 / stereo) and advertising one
/// session per target for its `module-rtp-session` to connect to. Non-blocking:
/// the sessions come up (advert + bound sockets) immediately; audio flows once a
/// receiver completes the handshake.
pub fn start(members: Vec<PwSinkMember>, sink_node_id: u32) -> anyhow::Result<PwSinkServerHandle> {
    if members.is_empty() {
        anyhow::bail!("pwsink_server::start called with no members");
    }

    // Capture the anchor monitor at the fixed 48 kHz / S16 / stereo bus rate —
    // pw-sink's wire format is fixed L16/48k (applemidi_sender byte-swaps to BE),
    // so unlike AP2 there is no rate negotiation.
    let (capture, mut pcm_rx) = crate::sendspin_capture::spawn("pwsink", sink_node_id)
        .map_err(|e| anyhow::anyhow!("failed to start capture for pw-sink group: {e}"))?;

    // One AppleMIDI sender per target, each with its own std::mpsc PCM channel the
    // relay feeds. The shared advertise daemon is fetched by AppleMidiSender when
    // `advertise_daemon` is Some (storm-safe, LAN-restricted); None only in tests.
    let advertise_daemon = crate::discovery_supervisor::shared_advertise_daemon();
    let format = SessionFormat::default(); // 48 kHz / 2ch
    let mut senders: Vec<(String, Arc<AppleMidiSender>)> = Vec::with_capacity(members.len());
    // (node_name, PCM sender) list moved into the relay thread for fan-out.
    let mut feeds: Vec<(String, std::sync::mpsc::SyncSender<PcmChunk>)> = Vec::with_capacity(members.len());

    for m in &members {
        let (pcm_tx, pcm_rx_sender) = std::sync::mpsc::sync_channel::<PcmChunk>(PCM_FEED_DEPTH);
        let config = SessionConfig {
            session_name: session_name_for(&m.node_name),
            control_port: m.control_port,
            ifname: None,
            format,
            playout_ms: m.playout_ms,
            advertise_daemon: advertise_daemon.clone(),
        };
        match AppleMidiSender::start(config, pcm_rx_sender) {
            Ok(sender) => {
                // Overlay clips are 48 kHz stereo → matches this capture's rate,
                // but publish it anyway so overlay_mixer::start rate-matches
                // correctly (and clears cleanly on teardown).
                crate::overlay_mixer::OverlayMixer::global().set_output_rate(&m.node_name, format.rate);
                tracing::info!(
                    "pw-sink: advertising session '{}' on control port {} for target '{}'",
                    session_name_for(&m.node_name),
                    m.control_port,
                    m.node_name
                );
                senders.push((m.node_name.clone(), Arc::new(sender)));
                feeds.push((m.node_name.clone(), pcm_tx));
            }
            Err(e) => {
                tracing::warn!("pw-sink: failed to start sender for '{}': {e}", m.node_name);
            }
        }
    }

    if senders.is_empty() {
        anyhow::bail!("pw-sink group: no senders started");
    }

    // Capture→feed relay on a dedicated SCHED_FIFO OS thread (NOT a tokio task) —
    // mirrors ap2_server's relay. It converts each captured S16LE chunk to native
    // i16 samples and fans them to every target's sender, applying that target's
    // announcement overlay (duck) first so ducking is per-device.
    let relay = std::thread::Builder::new()
        .name("pwsink-relay".into())
        .spawn(move || {
            set_relay_realtime_priority();
            let mixer = crate::overlay_mixer::OverlayMixer::global();
            let mut mix_buf: Vec<u8> = Vec::new();
            // Per-target dropped-chunk counters + one shared report cadence: a
            // blocked sender drops many chunks in a row, and one line per burst is
            // the diagnostic (mirrors the sendspin relay's backlog-full report).
            let mut dropped: Vec<u32> = vec![0; feeds.len()];
            let mut last_drop_log = std::time::Instant::now();
            while let Some(pcm) = pcm_rx.blocking_recv() {
                for (index, (name, tx)) in feeds.iter().enumerate() {
                    // Per-device overlay: `mix_into` returns false on the plain
                    // music path (no work); when a device is being announced to it
                    // returns duck(music)+overlay in `mix_buf`. Both are S16LE at
                    // 48 kHz here, so the mix is a plain sample add.
                    let src: &[u8] = if mixer.mix_into(name, &pcm, &mut mix_buf) { &mix_buf } else { &pcm };
                    // S16LE bytes → native i16 (applemidi_sender byte-swaps to L16
                    // big-endian on the wire).
                    let samples: PcmChunk = src.chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]])).collect();
                    // `try_send`, never `send`: this is an RT thread feeding N
                    // targets, so blocking on one full queue would stall the audio
                    // of every *other* target too. A full queue means that sender
                    // is not draining, and the realtime-correct answer is to drop.
                    if tx.try_send(samples).is_err() {
                        dropped[index] += 1;
                    }
                }
                if dropped.iter().any(|&n| n > 0) && last_drop_log.elapsed() >= std::time::Duration::from_secs(5) {
                    let detail: Vec<String> = feeds
                        .iter()
                        .zip(dropped.iter())
                        .filter(|(_, &n)| n > 0)
                        .map(|((name, _), n)| format!("{name}: {n}"))
                        .collect();
                    tracing::warn!(
                        "pw-sink relay: audio DISCARDED because a target's feed was full — {}. \
                         That sender thread is not draining (host CPU starvation, or its session is wedged), \
                         so it is missing this audio while the other targets are unaffected.",
                        detail.join(", ")
                    );
                    dropped.iter_mut().for_each(|n| *n = 0);
                    last_drop_log = std::time::Instant::now();
                }
            }
            tracing::debug!("pwsink relay thread exiting");
        })
        .map_err(|e| anyhow::anyhow!("failed to spawn pwsink relay thread: {e}"))?;

    // Liveness poll: publish each sender's handshake status into the global
    // registry so `/api/outputs` can show present-vs-streaming. The task holds a
    // `Weak` to each sender so it never keeps a session alive — once the handle
    // drops (senders' strong count → 0) every upgrade fails and the task exits
    // after clearing liveness. Cheap: a mutex read per sender each second.
    let weak_senders: Vec<(String, Weak<AppleMidiSender>)> = senders.iter().map(|(n, s)| (n.clone(), Arc::downgrade(s))).collect();
    let status_task = tokio::spawn(async move {
        let liveness = PwSinkLiveness::global();
        loop {
            let mut any_alive = false;
            for (name, weak) in &weak_senders {
                match weak.upgrade() {
                    Some(sender) => {
                        any_alive = true;
                        let st = sender.status();
                        liveness.set(name, PwSinkStatus { established: st.established, peer_count: st.peer_count });
                    }
                    None => liveness.remove(name),
                }
            }
            if !any_alive {
                return; // handle dropped — all senders gone.
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    });

    Ok(PwSinkServerHandle { senders, _capture: capture, _relay: relay, _status_task: status_task })
}
