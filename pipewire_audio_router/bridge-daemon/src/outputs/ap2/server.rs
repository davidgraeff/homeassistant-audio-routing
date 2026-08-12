//! Per-device AirPlay-2 sender for a sync group — the audio path.
//!
//! Mirrors `outputs::sendspin::server::start_server_per_device`, but the transport is the
//! vendored AirPlay-2 sender (`airplay_client::Connection`) instead of sendspin.
//! For each receiver in a group it opens an independent RTSP session (transient
//! pairing, PIN 3939) + RECORD + realtime-ALAC RTP stream, then fans the group's
//! single captured PCM feed to every receiver. PT=87 anchors carry the daemon's
//! host-global libairptp grandmaster `clock_id` (injected via
//! `Connection::set_ptp_clock_id`) on the shared `CLOCK_MONOTONIC` timeline, so
//! the receivers stay coincident — the standalone spike proved this shape works
//! on real Yamaha + Pioneer hardware.
//!
//! Capture is the same anchor-monitor source the sendspin per-device path uses
//! (48 kHz / S16 / stereo); `LiveAudioDecoder` resamples 48k→44.1k internally.
#![allow(dead_code)] // wired into routing/sync_group/mod.rs in the same phase

use std::net::IpAddr;
use std::sync::{Arc, Mutex};

use airplay_client::{Connection, LiveAudioDecoder, LiveFrameSender, LivePcmFrame};
use airplay_core::codec::{AudioFormat, SampleRate};
use airplay_core::device::{Device, DeviceId};
use airplay_core::features::Features;
use airplay_core::stream::{PtpMode, StreamConfig, TimingProtocol};
use tokio::sync::{mpsc, oneshot};

use crate::outputs::ap2::volume::{Ap2Command, SharedAp2Control, AP2_CMD_DEPTH};
use crate::routing::sync_settings::SharedSyncSettings;
use crate::util::locks::LockRecover;

const AP2_PORT: u16 = 7000;

/// Per-attempt timeout for AP2 connect/pair — a hung receiver must not stall the
/// sequential connect loop (blocking teardown → leaking sender SCHED_FIFO threads).
const AP2_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(12);
/// Timeout for SETUP/RECORD and for starting the live stream.
const AP2_SETUP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
/// Fresh whole-connect attempts before giving up — the initial cold/stale session
/// often fails once (events/RECORD timeout, M2 re-pair) then succeeds clean.
const AP2_CONNECT_ATTEMPTS: usize = 2;

/// The AP2 realtime stream rate (matches the ALAC magic cookie). We capture at
/// this rate so PipeWire resamples 48k→44.1k in-graph (on its own RT thread),
/// leaving the sender's decode path an identity copy — critical on a weak Pi,
/// where resampling inside the producer starved the RT sender thread and caused
/// the receivers' render buffer to underrun (silence).
const AP2_RATE: u32 = 44_100;

/// Depth of the capture→sender feed channel (`LiveAudioDecoder` input), in captured
/// chunks. The old value (16 ≈ a few hundred ms) let a late `run_streamer` (the
/// vendored producer, still a tokio task) overflow it → dropping `try_send` lost PCM
/// upstream of the streamer's buffer = the live-path silence. A deep channel lets the
/// producer, when it does run, pull a big batch and get ahead — the same "decode
/// ahead, never starve" behaviour that makes the file-path spike audible.
const AP2_FEED_FRAMES: usize = 128;

/// Render delay (ms): shifts the PT=87 anchor into the future so the receiver buffers
/// this much audio before playing. **Default 0** — the anchor says "play now", which
/// the target receivers handle; every ms here is latency the whole system pays for.
///
/// Per-output and live-tunable (routing/sync_settings.rs). Raise it for a receiver that goes
/// *silent* rather than merely early: that is the signature of packets arriving past
/// their play deadline, which a receiver drops rather than plays late.
pub const AP2_RENDER_DELAY_MS: u32 = 0;

/// Upper bound for the per-output render delay: stays inside the negotiated
/// `latency_max` (88200 frames ≈ 2 s at 44.1 kHz) so the delay fits the
/// receiver's buffer. There is deliberately **no** lower bound — a small delay
/// is a legitimate thing to ask for (it's the whole point of tuning this knob
/// down), it just risks dropouts on a jittery sender, which the UI marks red
/// rather than forbidding.
pub const AP2_RENDER_DELAY_MAX_MS: u16 = 2000;

/// ALAC magic cookie for the realtime default format (44100 Hz / 16-bit / stereo,
/// 352 frames/packet), sent as the SETUP phase-2 ASC so the receiver can decode.
/// Deterministic for that format — this is exactly the blob the standalone spike
/// sent and both receivers accepted.
pub(crate) const ALAC_MAGIC_COOKIE: [u8; 24] = [0, 0, 1, 96, 0, 16, 40, 10, 14, 2, 0, 255, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 172, 68];

/// ALAC magic cookie for a given sample `rate`: the base cookie with its trailing
/// 4 bytes (the sample rate, big-endian) rewritten. `ALAC_MAGIC_COOKIE` is the
/// 44100 case (`…,172,68` = 0x0000AC44); 48000 = 0x0000BB80 = `…,187,128`.
pub(crate) fn alac_cookie(rate: u32) -> [u8; 24] {
    let mut c = ALAC_MAGIC_COOKIE;
    c[20..24].copy_from_slice(&rate.to_be_bytes());
    c
}

/// AP2 realtime `StreamConfig` at `rate` — ALAC/16-bit/stereo, PTP master, ~500 ms
/// .. 2 s receiver render buffer, with the matching ALAC cookie. `sample_rate`
/// drives the SETUP `audioFormat` bit (`airplay_format_value`), which must agree
/// with the cookie's rate.
pub(crate) fn ap2_stream_config(rate: u32) -> StreamConfig {
    // ALAC/16/2, 352 frames/packet from Default; only the rate is ours to pick.
    let audio_format =
        AudioFormat { sample_rate: if rate >= 48_000 { SampleRate::Hz48000 } else { SampleRate::Hz44100 }, ..Default::default() };
    StreamConfig {
        timing_protocol: TimingProtocol::Ptp,
        ptp_mode: PtpMode::Master, // sender is the (external) grandmaster reference
        audio_format,
        asc: Some(alac_cookie(rate).to_vec()),
        latency_min: 22050,
        latency_max: 88200,
        ..Default::default()
    }
}

/// Why a receiver connection failed — `at_setup` distinguishes a SETUP rejection
/// (rate-relevant: a receiver refusing 48 kHz) from a pairing/connect failure
/// (transient, e.g. M2). Used to decide whether to cache a 48 kHz→44.1 kHz downgrade.
struct ConnectFail {
    at_setup: bool,
    msg: String,
}

/// How long [`Ap2ServerHandle::shutdown`] waits for the task to TEARDOWN **all** its
/// receivers before giving up, so a powered-off receiver cannot stall process exit past
/// the container's stop-grace (which would SIGKILL us and lose the TEARDOWNs that *did*
/// have somewhere to go).
///
/// This is the group-wide budget; [`AP2_RELEASE_TIMEOUT`] bounds each receiver within it.
const AP2_TEARDOWN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

/// Per-connection budget for releasing one receiver in [`teardown`]: FLUSH + TEARDOWN +
/// close. Deliberately **smaller than [`AP2_TEARDOWN_TIMEOUT`]**, which is the budget for
/// the *whole* group: teardown is sequential, so a single unresponsive receiver must not
/// consume the entire allowance and leave its groupmates un-torn-down on process exit —
/// that would strand exactly the stale sessions this is meant to release. At 1 s, three
/// receivers still fit inside the group budget, and a responsive one answers in ms.
const AP2_RELEASE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(1);

/// How long a freshly-connected receiver gets to answer the post-connect
/// `GET_PARAMETER` volume read. Deliberately short: the answer is only a UI nicety
/// (an unknown level is reported honestly), while the wait happens at the one point in
/// the group task's life where the device is already registered for commands but its
/// command channel is not yet being drained.
const AP2_VOLUME_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// A running group's AP2 senders. Dropping it signals the task to stop + TEARDOWN
/// every receiver session (mirrors `SendspinServerHandle`: drop = tear down), and
/// drops the capture handle — which closes the capture channel so the RT relay
/// thread exits.
///
/// Drop alone is fire-and-forget: the TEARDOWNs happen in the spawned task, which
/// is fine mid-run (the runtime keeps turning) but NOT on process exit, where
/// nothing polls it again. Use [`Self::shutdown`] there to actually wait for them.
pub struct Ap2ServerHandle {
    shutdown: Option<oneshot::Sender<()>>,
    task: Option<tokio::task::JoinHandle<()>>,
    /// Owned here (not by the relay thread) so dropping this handle closes the
    /// capture channel → the relay's `blocking_recv` returns `None` → the relay
    /// thread exits. `Option` only so [`Self::shutdown`] can close it *before*
    /// awaiting the TEARDOWNs rather than after.
    capture: Option<crate::pw::capture::CaptureHandle>,
    /// The RT relay thread; exits on its own once `capture` closes the channel.
    _relay: std::thread::JoinHandle<()>,
}

impl Ap2ServerHandle {
    /// Graceful stop: signal the task and **wait** (bounded by
    /// [`AP2_TEARDOWN_TIMEOUT`]) for it to TEARDOWN every receiver session, so a
    /// receiver releases its single AirPlay session now instead of holding a stale
    /// one until it times out — which is what makes the next start's first connect
    /// fail ("Pairing error M2" / cold RECORD) and keeps the receiver's AirPlay
    /// input busy for phones. Only worth calling on process exit; mid-run a plain
    /// drop is enough (the runtime still polls the task).
    pub async fn shutdown(mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        // Close the capture NOW so the RT relay stops feeding senders while their
        // connections tear down (the task owns those and does the RTSP work).
        drop(self.capture.take());
        if let Some(task) = self.task.take() {
            if tokio::time::timeout(AP2_TEARDOWN_TIMEOUT, task).await.is_err() {
                tracing::warn!("AP2 group: TEARDOWN did not finish within {:?}; exiting anyway", AP2_TEARDOWN_TIMEOUT);
            }
        }
    }
}

impl Drop for Ap2ServerHandle {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(()); // task's select! wakes → disconnects receivers
        }
        // `capture` then drops (field order) → capture channel closes → relay exits.
    }
}

/// Construct a minimal receiver `Device` for a discovered AP2 endpoint. The
/// features string is a fixed sender-side value proven to select the
/// transient-pairing + realtime + PTP path on real receivers (the discovered
/// per-device features aren't needed for this path).
pub(crate) fn build_device(ip: IpAddr) -> Device {
    Device {
        id: DeviceId([0; 6]),
        name: "AirPlay2".to_string(),
        model: String::new(),
        manufacturer: None,
        serial_number: None,
        addresses: vec![ip],
        port: AP2_PORT,
        features: Features::from_txt_value("0x4A7FCA00,0x3C354BD0").unwrap_or(Features(0)),
        required_sender_features: None,
        public_key: None,
        source_version: Default::default(),
        firmware_version: None,
        os_version: None,
        protocol_version: None,
        requires_password: false,
        status_flags: 0,
        access_control: None,
        pairing_identity: None,
        system_pairing_identity: None,
        bluetooth_address: None,
        homekit_home_id: None,
        group_id: None,
        is_group_leader: false,
        group_public_name: None,
        group_contains_discoverable_leader: false,
        home_group_id: None,
        household_id: None,
        parent_group_id: None,
        parent_group_contains_discoverable_leader: false,
        tight_sync_id: None,
        raop_port: None,
        raop_encryption_types: None,
        raop_codecs: None,
        raop_transport: None,
        raop_metadata_types: None,
        raop_digest_auth: false,
        vodka_version: None,
    }
}

/// Connect + pair + SETUP + RECORD + start a live realtime-ALAC stream to one
/// receiver at `rate` Hz, **retrying a fresh connect** on a transient failure.
///
/// The killer case is the "initial session": a cold receiver (or one whose prior
/// RTSP session hasn't been released) times out pairing/events/RECORD on the first
/// try and, on the second — clean — attempt, connects fine. So we try up to
/// [`AP2_CONNECT_ATTEMPTS`] fresh whole-connects with a short backoff. Only if
/// EVERY attempt fails at SETUP/RECORD (`at_setup`) does the caller treat it as a
/// real 48 kHz rejection and cache a 44.1 kHz downgrade — a single transient RECORD
/// timeout no longer masquerades as a rate rejection.
async fn connect_one(
    node_name: &str,
    ip: IpAddr,
    clock_id: u64,
    render_delay_ms: u32,
    rate: u32,
    senders: &Arc<Mutex<Vec<(String, LiveFrameSender)>>>,
) -> Result<(Connection, mpsc::Receiver<f32>), ConnectFail> {
    let mut last: Option<ConnectFail> = None;
    for attempt in 0..AP2_CONNECT_ATTEMPTS {
        if attempt > 0 {
            // Fresh retry: a short backoff lets the receiver release its prior
            // session (the "Pairing error M2" / cold-RECORD races) before we build a
            // brand-new session.
            tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
            tracing::info!("AP2: retrying connect to '{node_name}' ({ip}) — attempt {}/{}", attempt + 1, AP2_CONNECT_ATTEMPTS);
        }
        match try_connect_once(node_name, ip, clock_id, render_delay_ms, rate, senders).await {
            Ok(ok) => return Ok(ok),
            Err(fail) => {
                tracing::warn!(
                    "AP2: connect attempt {}/{} to '{node_name}' ({ip}) failed: {}",
                    attempt + 1,
                    AP2_CONNECT_ATTEMPTS,
                    fail.msg
                );
                last = Some(fail);
            }
        }
    }
    Err(last.expect("at least one attempt ran"))
}

/// One connect attempt (a fresh RTSP session). Every network step is time-bounded
/// so a hung receiver can't stall the sequential connect loop (which would block
/// group teardown and leak the sender's SCHED_FIFO threads). On a start-streaming
/// failure it removes the just-registered feed so a retry doesn't double-register.
async fn try_connect_once(
    node_name: &str,
    ip: IpAddr,
    clock_id: u64,
    render_delay_ms: u32,
    rate: u32,
    senders: &Arc<Mutex<Vec<(String, LiveFrameSender)>>>,
) -> Result<(Connection, mpsc::Receiver<f32>), ConnectFail> {
    let device = build_device(ip);
    let config = ap2_stream_config(rate);
    // connect_auto: pair-verify if a stored identity exists, else transient (3939).
    let mut conn = match tokio::time::timeout(AP2_CONNECT_TIMEOUT, Connection::connect_auto(device, config, "3939")).await {
        Ok(Ok(c)) => c,
        Ok(Err(e)) => return Err(ConnectFail { at_setup: false, msg: format!("connect/pair failed: {e}") }),
        Err(_) => return Err(ConnectFail { at_setup: false, msg: "connect/pair timed out".into() }),
    };
    // Inject the daemon's grandmaster clock id BEFORE setup so PT=87 carries it.
    conn.set_ptp_clock_id(clock_id);
    // The receiver's OWN volume is authoritative (outputs/ap2/volume.rs): never impose one on
    // connect. Suppress the vendored client's default connect-time volume push, which
    // otherwise forces 0 dB = MAX — blasting a powerful AVR and clobbering the level
    // `get_volume()` reads back below. User intent is re-applied post-connect via the
    // control's command channel (`register`), not here.
    conn.set_send_volume_on_start(false);
    // Render buffer (anchor shifted into the future) — applied by start_streaming_live.
    conn.set_render_delay_ms(render_delay_ms);
    // SETUP now also fails on a bad RECORD (the receiver never acknowledged the
    // session → it wouldn't render); `at_setup` so a PERSISTENT failure (all attempts)
    // is read as a 48 kHz rejection, while a transient RECORD timeout just retries.
    match tokio::time::timeout(AP2_SETUP_TIMEOUT, conn.setup()).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            abandon(&mut conn, node_name).await;
            return Err(ConnectFail { at_setup: true, msg: format!("SETUP/RECORD failed: {e}") });
        }
        Err(_) => {
            abandon(&mut conn, node_name).await;
            return Err(ConnectFail { at_setup: false, msg: "SETUP timed out".into() });
        }
    }
    // Live PCM feed at the group's capture rate (PipeWire resampled the anchor to
    // `rate` in-graph). Register it BEFORE starting so the capture-forward loop fills
    // the decoder during start_live's prefill (else it starts starved).
    let (sender, decoder) = LiveAudioDecoder::create_pair(rate, crate::pw::capture::CHANNELS as u8, AP2_FEED_FRAMES);
    senders.lock().unwrap().push((node_name.to_string(), sender));
    // start_streaming_live spawns the RT sender + producer threads. On failure: stop
    // the connection (join those threads, bounded) AND drop the feed we just
    // registered, so a retry starts clean (no orphaned/duplicate sender).
    let start = tokio::time::timeout(AP2_SETUP_TIMEOUT, conn.start_streaming_live(decoder)).await;
    if !matches!(start, Ok(Ok(()))) {
        senders.lock().unwrap().retain(|(n, _)| n != node_name);
        // `stop()` is a FLUSH — it stops the audio but leaves the RTSP session open, so
        // the full release below is what lets the next attempt start clean.
        let _ = tokio::time::timeout(AP2_SETUP_TIMEOUT, conn.stop()).await;
        abandon(&mut conn, node_name).await;
        return Err(match start {
            Ok(Err(e)) => ConnectFail { at_setup: true, msg: format!("start_streaming_live failed: {e}") },
            _ => ConnectFail { at_setup: false, msg: "start_streaming_live timed out".into() },
        });
    }
    // Device→UI volume feedback (best-effort; reverse event channel).
    let volume_rx = conn.volume_events();
    Ok((conn, volume_rx))
}

/// Release a connection we are giving up on: TEARDOWN the receiver's session and abort
/// the tasks `setup()` started, bounded so an unresponsive receiver can't stall the
/// connect loop.
///
/// **Why a failed attempt must still clean up.** The vendored `Connection` has **no
/// `Drop` impl**, and the two obvious calls are not interchangeable: `stop()` sends a
/// FLUSH (audio stops, session stays), and only `disconnect()` sends TEARDOWN. Dropping
/// the value therefore tells the receiver *nothing* and detaches — rather than aborts —
/// the `events_task`, `timing_task`, `ptp_master_sync_task` and timing server that
/// `setup()` spawned. All of those exist before the RECORD step, so they outlive exactly
/// the failures we see most.
///
/// The receiver-side half of that is the interesting one, and this project already knew
/// the mechanism from the other direction: [`Ap2ServerHandle::shutdown`] exists so a
/// receiver "releases its single AirPlay session now instead of holding a stale one until
/// it times out — which is what makes the next start's first connect fail". An abandoned
/// *failed* attempt did precisely what that comment warns about, so retry N+1 met a
/// receiver still holding the session from retry N: its event port doesn't accept and
/// RECORD times out, which fails, which abandons another session. That is a
/// self-sustaining loop that ends only when the receiver's own session timeout expires —
/// and it is the most likely explanation for "this receiver always needs a few attempts
/// before it works". Observed against the Pioneer VSX-934 on 2026-08-12 (`Events port …
/// connect timed out after 3s` → `RECORD timed out`) on a unit that answered `GET /info`
/// in ~4 ms throughout, i.e. a perfectly healthy receiver.
async fn abandon(conn: &mut Connection, node_name: &str) {
    if tokio::time::timeout(AP2_TEARDOWN_TIMEOUT, conn.disconnect()).await.is_err() {
        tracing::warn!(
            "AP2: '{node_name}' did not acknowledge TEARDOWN within {:?} after a failed connect; \
             its session may stay busy until the receiver times it out",
            AP2_TEARDOWN_TIMEOUT
        );
    }
}

/// Apply one control command to whichever of this group's connections it names.
///
/// Shared by the two places that drain the command channel — the connect loop and the
/// steady-state loop — because the channel MUST be drained in both. A command for a
/// member that isn't connected (yet, or at all) is silently ignored: `Ap2Control`
/// keeps the desired state and `register` re-applies it on the next connect.
async fn apply_command(conns: &mut [(String, Connection)], cmd: Ap2Command) {
    match cmd {
        Ap2Command::SetVolume { node_name, volume } => {
            if let Some((_, conn)) = conns.iter_mut().find(|(n, _)| *n == node_name) {
                if let Err(e) = conn.set_volume(volume).await {
                    tracing::warn!("AP2: set_volume for '{node_name}' failed: {e}");
                }
            }
        }
        Ap2Command::SetRenderDelay { node_name, ms } => {
            // Live anchor-offset change — no reconnect (that churn could leave the
            // receiver silent). The streamer picks it up on the next packet.
            if let Some((_, conn)) = conns.iter_mut().find(|(n, _)| *n == node_name) {
                conn.set_render_delay_live(u32::from(ms)).await;
                tracing::info!("AP2: '{node_name}' render delay set live to {ms}ms");
            }
        }
    }
}

/// TEARDOWN every connection this group established, releasing each receiver's single
/// AirPlay session and **joining the SCHED_FIFO sender + producer threads that
/// `start_streaming_live` spawned** (`Connection::stop`).
///
/// Factored out because it has to run on *every* exit from the group task, not just the
/// steady-state one. It previously ran only after the connect loop had finished every
/// member, so a task abandoned mid-connect — the handle dropped while an unresponsive
/// receiver burned its timeouts — left the already-connected members' threads running
/// for the life of the process. That is the AP2 thread leak: on 2026-08-12 a daemon 40
/// minutes old had three `ap2-producer` threads and five `rt-sender` threads for one
/// receiver actually streaming, two of the producers still burning ~1.4 % CPU each with
/// nothing to feed.
async fn teardown(conns: Vec<(String, Connection)>, fwd_tasks: Vec<tokio::task::JoinHandle<()>>, control: &SharedAp2Control) {
    for t in fwd_tasks {
        t.abort();
    }
    for (name, mut c) in conns {
        control.lock().await.unregister(&name);
        crate::outputs::overlay_mixer::OverlayMixer::global().clear_output_rate(&name);
        // **Bounded per connection.** `stop()` (FLUSH) and `disconnect()` (TEARDOWN +
        // close) are both RTSP round trips, and unbounded they let one unresponsive
        // receiver hang the group task *inside* teardown — still owning `conns`, so no
        // `Connection` is dropped, no socket is closed and no sender thread is joined.
        // One slow receiver would then strand the whole group's resources, which is the
        // opposite of what teardown is for.
        //
        // The evidence this was really happening: four sockets to the two receivers sat
        // in CLOSE_WAIT indefinitely (2026-08-12), in two bursts of one-per-receiver that
        // matched the two abandoned group generations. CLOSE_WAIT means the *receiver*
        // closed and we never did — i.e. a live owner still held the fd. Dropping a
        // `Connection` does close its socket, so bounding these awaits is what guarantees
        // the drop is reached: on timeout we lose the graceful TEARDOWN, but `c` still
        // falls out of scope here and the fd goes with it.
        let released = tokio::time::timeout(AP2_RELEASE_TIMEOUT, async {
            let _ = c.stop().await;
            let _ = c.disconnect().await; // also aborts the event-channel reader
        })
        .await;
        if released.is_err() {
            tracing::warn!(
                "AP2: '{name}' did not complete TEARDOWN within {:?}; closing its socket anyway \
                 (its session may stay busy until the receiver times it out)",
                AP2_RELEASE_TIMEOUT
            );
        }
        // `c` drops here whether or not the round trips finished — closing the RTSP
        // socket and releasing the sender/producer threads.
    }
    tracing::info!("AP2 group: senders stopped");
}

/// Best-effort real-time scheduling for the capture→feed relay thread. Mirrors
/// `sendspin_server`'s relay (prio 40) and the AP2 sender's own SCHED_FIFO thread:
/// the relay must never queue behind the daemon's general-purpose async work, or a
/// scheduling gap drops captured PCM before it reaches the sender. Needs
/// `CAP_SYS_NICE` (the add-on runs as root, so it's granted); degrades gracefully.
fn set_relay_realtime_priority() {
    #[cfg(target_os = "linux")]
    unsafe {
        let mut param: libc::sched_param = std::mem::zeroed();
        param.sched_priority = 40;
        if libc::sched_setscheduler(0, libc::SCHED_FIFO, &param) == 0 {
            tracing::info!("ap2 relay: real-time priority set (SCHED_FIFO, priority 40)");
        } else {
            tracing::debug!("ap2 relay: could not set RT priority (need CAP_SYS_NICE); normal priority");
        }
    }
}

/// Start AP2 senders for a group's `members` (output node_name + receiver IP +
/// optional per-output render delay in ms; `None` = [`AP2_RENDER_DELAY_MS`]),
/// capturing from the group's anchor sink (`sink_node_id`) and streaming to each
/// receiver with PT=87 anchored to the grandmaster `clock_id`. Non-blocking:
/// connects the receivers inside the spawned task so a slow/absent receiver never
/// stalls the reconciler.
pub fn start(
    members: Vec<(String, IpAddr, Option<u16>)>,
    sink_node_id: u32,
    clock_id: u64,
    control: SharedAp2Control,
    rate: u32,
    sync_settings: SharedSyncSettings,
) -> anyhow::Result<Ap2ServerHandle> {
    // Capture at the group's negotiated rate: PipeWire resamples the 48 kHz anchor
    // to `rate` in-graph (its own RT thread), so no Rust-side SRC — a 48 kHz group
    // is passthrough, a 44.1 kHz group is PipeWire-downsampled.
    let (capture, mut pcm_rx) = crate::pw::capture::spawn_with_rate("ap2", sink_node_id, rate)
        .map_err(|e| anyhow::anyhow!("failed to start capture for AP2 group: {e}"))?;
    let (shutdown_tx, mut shutdown_rx) = oneshot::channel::<()>();
    let senders: Arc<Mutex<Vec<(String, LiveFrameSender)>>> = Arc::new(Mutex::new(Vec::new()));

    // Capture→feed relay on a dedicated SCHED_FIFO OS thread (NOT a tokio task) — so
    // the daemon's general-purpose async work can never preempt the hop from capture
    // to the sender feed (that preemption, on the old plain-tokio relay, starved the
    // live path → silence; mirrors sendspin_server's RT relay). It fans each captured
    // chunk (S16 LE → i16) to every registered receiver feed the instant it's
    // registered, so all feeds get identical PCM and stay coincident under the shared
    // grandmaster clock. `blocking_recv` drains the capture channel; the thread exits
    // when the channel closes — i.e. when the `Ap2ServerHandle`'s `_capture` is
    // dropped on shutdown (do NOT move the capture handle into this thread).
    let senders_relay = Arc::clone(&senders);
    let relay = std::thread::Builder::new()
        .name("ap2-relay".into())
        .spawn(move || {
            set_relay_realtime_priority();
            // Per-device announcement overlay (AG). `mix_into` returns false on the
            // plain-music path (no allocation/work); when a device has an active
            // announcement it returns duck(music)+overlay in `mix_buf`. The overlay
            // PCM was resampled to this group's `rate` when the announcement started
            // (announce/mod.rs), so music and overlay are the same rate here — the mix is
            // pure sample addition. `mix_buf` is reused across chunks AND devices.
            let mixer = crate::outputs::overlay_mixer::OverlayMixer::global();
            // Provisional per-device alignment delay (align/relay_delay.rs), applied AFTER the
            // overlay so it shifts everything this receiver renders — as the AP2 render-
            // delay knob it stands in for does. It emits exactly what it is fed, one
            // block per block, so the feed cadence and the receiver's buffering are
            // unchanged; only the content is older. `delay_buf` is reused like `mix_buf`,
            // and with no alignment running the call is one relaxed atomic load.
            let delayer = crate::align::relay_delay::RelayDelay::global();
            let delay_fmt = crate::align::relay_delay::PcmFormat::new(rate, crate::pw::capture::CHANNELS);
            let mut mix_buf: Vec<u8> = Vec::new();
            let mut delay_buf: Vec<u8> = Vec::new();
            while let Some(pcm) = pcm_rx.blocking_recv() {
                let list = senders_relay.lock().unwrap();
                // Fan out with ZERO steady-state allocation on this RT relay thread:
                // each sender hands back a recycled buffer via `take_buffer()`, and we
                // convert the S16LE bytes → i16 straight into it (reusing its
                // capacity). The decoder returns the buffer to the free-list once it's
                // drained the samples. (`pcm` is a pooled `PooledBuf` on the capture
                // side, so the whole capture→feed path is allocation-free once warm.)
                for (name, s) in list.iter() {
                    let mut buf = s.take_buffer();
                    let mixed: &[u8] = if mixer.mix_into(name, &pcm, &mut mix_buf) { &mix_buf } else { &pcm };
                    let src: &[u8] = if delayer.delay_into(name, delay_fmt, mixed, &mut delay_buf) { &delay_buf } else { mixed };
                    buf.extend(src.chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]])));
                    let _ = s.try_send(LivePcmFrame { samples: buf, channels: crate::pw::capture::CHANNELS as u8, sample_rate: rate });
                }
            }
            tracing::debug!("ap2 relay thread exiting");
        })
        .map_err(|e| anyhow::anyhow!("failed to spawn ap2 relay thread: {e}"))?;

    // Volume/mute commands from outputs/ap2/volume.rs (via the API) land here; the task
    // owns the `Connection`s (which need `&mut` to send SET_PARAMETER volume), so
    // it applies them by node name. Each connected device registers this sender.
    let (cmd_tx, mut cmd_rx) = mpsc::channel::<Ap2Command>(AP2_CMD_DEPTH);
    let control_task = control.clone();

    let task = tokio::spawn(async move {
        // Connect every receiver (sequential; each is ~1-3s of pairing/SETUP).
        // connect_one registers its feed (driven by the relay) before starting.
        let mut conns: Vec<(String, Connection)> = Vec::new();
        // Per-device forwarders: receiver-reported volume (event channel) →
        // Ap2Control (device→UI). Aborted on teardown.
        let mut fwd_tasks: Vec<tokio::task::JoinHandle<()>> = Vec::new();
        // Set when a 48 kHz SETUP was rejected: cache the receiver as 44.1k-only and
        // nudge the reconciler so the group restarts at 44.1 kHz (auto-negotiation).
        let mut downgraded = false;
        // Set once the handle asks us to stop. Checked between members so a group
        // abandoned mid-connect stops connecting the *rest* of them and goes straight to
        // teardown — which is what stops the sender threads of the members already up.
        let mut shutting_down = false;
        for (name, ip, delay) in &members {
            if shutting_down {
                tracing::info!("AP2 group: shutting down; skipping the remaining member(s) from '{name}'");
                break;
            }
            let render_delay_ms = u32::from(delay.unwrap_or(AP2_RENDER_DELAY_MS as u16));
            // Connect this member while STILL SERVICING the command channel. Not
            // draining `cmd_rx` for the length of a connect is what let the channel fill
            // up: a caller then blocked on a full channel while holding the `Ap2Control`
            // guard, which deadlocked `/api/outputs`, `/api/routing` and the reconciler
            // (outputs/ap2/volume.rs `try_queue` documents the incident). `try_queue`
            // alone makes that lossy rather than fatal; draining here is what keeps the
            // queue from filling in the first place, since a connect against an
            // unresponsive receiver is precisely the long window.
            //
            // A stop request is *recorded*, not acted on, until this connect returns:
            // cancelling `connect_one` at its `start_streaming_live` await could orphan
            // the SCHED_FIFO threads it had just spawned — the leak this is meant to
            // prevent. Letting the in-flight member finish is bounded by
            // AP2_CONNECT_TIMEOUT/AP2_SETUP_TIMEOUT and leaves a connection we can tear
            // down properly.
            let connect = connect_one(name, *ip, clock_id, render_delay_ms, rate, &senders);
            tokio::pin!(connect);
            let outcome = loop {
                tokio::select! {
                    result = &mut connect => break result,
                    // Guarded so the oneshot is never polled after it completes.
                    _ = &mut shutdown_rx, if !shutting_down => shutting_down = true,
                    Some(cmd) = cmd_rx.recv() => apply_command(&mut conns, cmd).await,
                }
            };
            match outcome {
                Ok((mut c, mut volume_rx)) => {
                    tracing::info!("AP2: streaming to '{}' ({}) @ {}Hz render_delay={}ms", name, ip, rate, render_delay_ms);
                    // It works — drop any stale "why isn't this playing" note the UI
                    // is still showing from an earlier failure.
                    crate::outputs::ap2::health::Ap2Health::global().clear(name);
                    // Publish this output's capture rate so announcement overlays are
                    // rate-matched to it (overlay_mixer resamples the 48 kHz clip).
                    crate::outputs::overlay_mixer::OverlayMixer::global().set_output_rate(name, rate);
                    // Register the command channel (does NOT push a volume unless the
                    // user set one — the device's own volume is authoritative).
                    control_task.lock().await.register(name.clone(), cmd_tx.clone());
                    // READ the receiver's current volume so the UI reflects the real
                    // level (what the user set on the device) rather than a made-up
                    // one. If the receiver doesn't answer GET_PARAMETER, leave it
                    // unknown (the UI shows no/zero level — honest).
                    //
                    // **Bounded**, because this sits between `register` (the device is
                    // now in `Ap2Control::senders`) and the loop that drains its command
                    // channel. An unbounded wait here is the worst possible place for
                    // one: the device looks live to every writer while nothing consumes
                    // what they write. A wedged receiver — TCP established, RTSP silent —
                    // parked this indefinitely and took the daemon's whole UI down with
                    // it (see docs/live-instance-debugging.md). A receiver that cannot
                    // answer a volume read in this long simply has an unknown level.
                    match tokio::time::timeout(AP2_VOLUME_READ_TIMEOUT, c.get_volume()).await.unwrap_or(None) {
                        Some(v) => {
                            tracing::info!("AP2: '{name}' reported volume {:.0}%", v * 100.0);
                            control_task.lock().await.note_reported_volume(name, v);
                        }
                        None => {
                            tracing::debug!("AP2: '{name}' did not report a volume (GET_PARAMETER unsupported); UI shows it as unknown")
                        }
                    }
                    // Forward this receiver's reported volume into the control so
                    // the UI reflects a change made on the device itself.
                    let control_fwd = control_task.clone();
                    let name_fwd = name.clone();
                    fwd_tasks.push(tokio::spawn(async move {
                        while let Some(vol) = volume_rx.recv().await {
                            control_fwd.lock().await.note_reported_volume(&name_fwd, vol);
                        }
                    }));
                    conns.push((name.clone(), c));
                }
                Err(fail) => {
                    tracing::warn!("AP2: could not start '{}' ({}) @ {}Hz: {}", name, ip, rate, fail.msg);
                    // Surface it: this used to be a log line and nothing else — the
                    // output stayed green in the matrix and simply never played. The
                    // liveness probe overwrites/clears this on its next tick, which is
                    // what turns a one-off failure into a standing diagnosis.
                    crate::outputs::ap2::health::Ap2Health::global()
                        .set(name, format!("Could not start the stream at {rate} Hz: {}", fail.msg));
                    // Auto-negotiation fallback: a 48 kHz SETUP rejection ⇒ this
                    // receiver is 44.1k-only. Cache it (persisted) so we don't re-probe,
                    // and flag a reconcile — the group's rate recomputes to 44.1 kHz and
                    // restarts. (A 44.1 kHz failure is a genuine/transient error, not a
                    // rate issue, so we never downgrade below 44.1k.)
                    if rate >= 48_000 && fail.at_setup {
                        if let Err(e) = sync_settings.lock_recover().set_ap2_rate_cap(name, 44_100) {
                            tracing::warn!("AP2: failed to persist 44.1k cap for '{name}': {e}");
                        }
                        tracing::info!("AP2: '{name}' rejected 48 kHz; caching 44.1 kHz and re-negotiating the group");
                        downgraded = true;
                    }
                }
            }
        }
        if downgraded {
            // Nudge the reconciler (via the control's change notifier) → group
            // restarts at 44.1 kHz. The 400 ms reconcile debounce coalesces this.
            control_task.lock().await.notify_reconcile();
        }
        if conns.is_empty() {
            tracing::warn!("AP2 group: no receivers connected");
        }

        // Run until the handle is dropped (shutdown), applying control commands
        // meanwhile. Skipped entirely if the stop already arrived during the connect
        // loop — `shutdown_rx` has completed by then and must not be polled again.
        // Either way we fall through to teardown, which is the only path that stops the
        // sender threads. The relay thread is stopped separately by the handle dropping
        // `_capture` (closes the capture channel → relay's blocking_recv returns None).
        if !shutting_down {
            loop {
                tokio::select! {
                    _ = &mut shutdown_rx => break,
                    Some(cmd) = cmd_rx.recv() => apply_command(&mut conns, cmd).await,
                }
            }
        }
        teardown(conns, fwd_tasks, &control_task).await;
    });

    Ok(Ap2ServerHandle { shutdown: Some(shutdown_tx), task: Some(task), capture: Some(capture), _relay: relay })
}
