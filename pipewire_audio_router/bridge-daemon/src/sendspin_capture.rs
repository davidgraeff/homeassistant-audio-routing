//! Native, long-lived PCM capture from a sendspin output's sink node —
//! replaces adapter.py's continuous `pw-record --target <node> ... -` subprocess.
//!
//! Mirrors player.rs's stream setup (same crate APIs, same format-negotiation
//! pod-building) with the differences long-lived capture needs: `Direction::Input`
//! with the `process` callback *reading* captured bytes instead of writing them,
//! no drain-and-quit (this runs until told to stop), and a `pw::channel`-based
//! stop command — the same cross-thread-into-the-mainloop mechanism pw_thread.rs
//! already uses for `PwCommand`, since PipeWire's `!Send` types mean an external
//! thread can't just call `mainloop.quit()` directly.
//!
//! Runs on its own dedicated OS thread (matching pw_thread.rs's own
//! `std::thread::Builder::spawn`, not `tokio::task::spawn_blocking` — this is
//! long-lived, not one bounded task) and forwards each captured buffer's bytes
//! through a **bounded** `tokio::sync::mpsc` channel (non-blocking `try_send`,
//! drop-on-full, safe to call from any thread, no runtime needed) to whatever
//! consumer wants the PCM (sendspin_server.rs / ap2_server.rs). Buffers are
//! drawn from a small pool ([`PooledBuf`]) and recycled, so the RT capture
//! callback does no heap allocation in steady state, and the bounded channel
//! caps memory/latency if a consumer ever falls behind.

use pipewire as pw;
use pw::spa;
use spa::pod::Pod;
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::mpsc::{self, Receiver, Sender};

/// Fixed to match what this daemon has always produced for sendspin (adapter.py's
/// SAMPLE_RATE/CHANNELS/BIT_DEPTH constants) — not derived from the sink node,
/// since `support.null-audio-sink` doesn't itself constrain callers to one rate.
pub const SAMPLE_RATE: u32 = 48_000;
pub const CHANNELS: u16 = 2;

/// Bounded depth of the capture → consumer PCM channel. Bounded (not unbounded)
/// so a stalled/too-slow consumer can never grow memory or latency without
/// limit: past this depth the RT callback drops the chunk (realtime-correct —
/// prefer a glitch over unbounded backlog) rather than blocking or queueing
/// forever. 32 = exactly one tokio mpsc block (no linked-list growth), ~0.68 s
/// worst-case backlog that only ever fills during an abnormal consumer stall;
/// steady-state occupancy is ~0-1.
const CAPTURE_CHANNEL_CAP: usize = 32;

/// Bounded depth of the buffer-pool free-list. A handful of spare buffers is
/// plenty in steady state; returns past this are freed (the pool re-allocates if
/// ever needed), so total pooled memory is bounded.
const CAPTURE_POOL_CAP: usize = 8;

/// Cumulative count of captured chunks dropped because the PCM channel was full
/// (the consumer couldn't keep up). Logged, throttled.
static CAPTURE_DROPPED: AtomicU64 = AtomicU64::new(0);

/// A captured PCM chunk backed by a **pooled** `Vec<u8>`. The capture callback
/// runs on PipeWire's RT graph data-loop (`RT_PROCESS`); allocating a fresh
/// `Vec` there every quantum risks stalling on the allocator arena. Instead the
/// callback fills a buffer taken from a free-list and hands out this wrapper;
/// on drop the buffer is returned to the free-list for reuse, so steady-state
/// capture does no heap allocation on the RT thread.
///
/// Derefs to `[u8]`, so consumers use it exactly like the old `Vec<u8>` did
/// (`&chunk`, `chunk.len()`, `chunk.chunks_exact(2)`, …) via deref coercion.
pub struct PooledBuf {
    buf: Vec<u8>,
    ret: Sender<Vec<u8>>,
}

impl std::ops::Deref for PooledBuf {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        &self.buf
    }
}

impl Drop for PooledBuf {
    fn drop(&mut self) {
        // Return the buffer to the pool (keeps its capacity for reuse). Bounded
        // + non-blocking: if the pool is already full (or the capture is gone),
        // the buffer is simply freed.
        let _ = self.ret.try_send(std::mem::take(&mut self.buf));
    }
}

enum CaptureCmd {
    Stop,
}

/// Handle to a running capture thread. Dropping this stops it (best-effort;
/// the thread is given a chance to exit cleanly but isn't waited on, since
/// `Drop` can't be async and the thread can't outlive the sink node it reads
/// from for more than a fraction of a second in practice).
pub struct CaptureHandle {
    cmd_tx: Option<pw::channel::Sender<CaptureCmd>>,
}

impl CaptureHandle {
    /// Stop the capture thread. Idempotent — a second call is a no-op.
    pub fn stop(&mut self) {
        if let Some(tx) = self.cmd_tx.take() {
            let _ = tx.send(CaptureCmd::Stop);
        }
    }
}

impl Drop for CaptureHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Starts capturing PCM from `target_node_id` (a sink node's monitor ports —
/// see `STREAM_CAPTURE_SINK` below) on a dedicated thread. Returns immediately;
/// captured chunks arrive on the returned receiver as they're delivered by
/// PipeWire's own graph clock — nothing here paces or buffers them further.
pub fn spawn(label: &'static str, target_node_id: u32) -> Result<(CaptureHandle, Receiver<PooledBuf>), String> {
    spawn_with_rate(label, target_node_id, SAMPLE_RATE)
}

/// Like [`spawn`], but requests capture at `rate` Hz. PipeWire resamples in-graph
/// (on its own RT thread) between the monitor's native rate and `rate`, so a
/// consumer that needs a specific rate (e.g. the AP2 sender's 44100) can avoid
/// resampling on its own hot path. Chunks are S16LE / `CHANNELS` at `rate`.
pub fn spawn_with_rate(label: &'static str, target_node_id: u32, rate: u32) -> Result<(CaptureHandle, Receiver<PooledBuf>), String> {
    let (pcm_tx, pcm_rx) = mpsc::channel(CAPTURE_CHANNEL_CAP);
    let (cmd_tx, cmd_rx) = pw::channel::channel::<CaptureCmd>();

    std::thread::Builder::new()
        // Linux truncates thread names at 15 bytes, so "<label>-capture"
        // rather than the full node name.
        .name(format!("{label}-capture"))
        .spawn(move || {
            if let Err(e) = run(label, target_node_id, rate, pcm_tx, cmd_rx) {
                tracing::error!("{label} capture thread for node {target_node_id} exited with error: {e}");
            }
        })
        .map_err(|e| format!("failed to spawn capture thread: {e}"))?;

    Ok((CaptureHandle { cmd_tx: Some(cmd_tx) }, pcm_rx))
}

/// Elevate the capture thread to `SCHED_FIFO` so it preempts the normal-priority
/// worker pool (notably the busy mDNS `ServiceDaemon` threads) under host CPU
/// contention. The steady-state PCM copy runs in the `RT_PROCESS` callback on
/// PipeWire's own data-loop (already FIFO 83), so this thread mostly sleeps in
/// `epoll_wait`; the elevation matters for the *control* path — prompt stream
/// (re)connect, format renegotiation and stop handling — which otherwise stalls
/// for seconds when the box is saturated (a cause of flaky connections). It
/// blocks in the mainloop's poll when idle, so FIFO here can never spin-lock a
/// core. Priority 45 sits above the relays (40) and below the AP2 sender (50)
/// and PipeWire's data-loop (83). Without `CAP_SYS_NICE` it logs and continues
/// at normal priority — exactly like the relay's `set_relay_realtime_priority`.
#[cfg(target_os = "linux")]
fn set_capture_realtime_priority() {
    // SAFETY: sched_setscheduler on the current thread (pid 0) with a valid,
    // zero-initialised sched_param; no aliasing, no ownership transfer.
    unsafe {
        let mut param: libc::sched_param = std::mem::zeroed();
        param.sched_priority = 45;
        if libc::sched_setscheduler(0, libc::SCHED_FIFO, &param) == 0 {
            tracing::info!("sendspin capture: real-time priority set (SCHED_FIFO, priority 45)");
        } else {
            tracing::debug!("sendspin capture: could not set RT priority (need CAP_SYS_NICE); running at normal priority");
        }
    }
}
#[cfg(not(target_os = "linux"))]
fn set_capture_realtime_priority() {}

fn run(
    label: &str,
    target_node_id: u32,
    rate: u32,
    pcm_tx: Sender<PooledBuf>,
    cmd_rx: pw::channel::Receiver<CaptureCmd>,
) -> Result<(), String> {
    set_capture_realtime_priority();
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None).map_err(|e| format!("mainloop: {e}"))?;
    let context = pw::context::ContextRc::new(&mainloop, None).map_err(|e| format!("context: {e}"))?;
    let core = context.connect_rc(None).map_err(|e| format!("connect to PipeWire: {e}"))?;

    // `label` names this stream after the subsystem that owns it. All three
    // consumers of this module (the sendspin relay, the AP2 sender, the pw-sink
    // sender) used to hard-code "bridge-sendspin-capture", so a graph with an
    // AP2 receiver routed showed two identically-named nodes and no way to tell
    // which was which — it read as a leaked duplicate and cost real time chasing
    // one during the 2026-08-03 investigation (docs/sendspin-open-items.md).
    let node_name = format!("bridge-{label}-capture");
    let stream = pw::stream::StreamBox::new(
        &core,
        &node_name,
        pw::properties::properties! {
            *pw::keys::MEDIA_TYPE => "Audio",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::NODE_NAME => node_name.as_str(),
            // Tells the session manager to connect this capture stream to the
            // target's *monitor* ports rather than its (nonexistent, for a
            // plain sink) regular output ports — exactly what `pw-record
            // --target <sink>` relies on internally.
            *pw::keys::STREAM_CAPTURE_SINK => "true",
        },
    )
    .map_err(|e| format!("create stream: {e}"))?;

    let error: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));

    // Buffer pool free-list (bounded): the RT process callback pulls a recycled
    // `Vec` to fill (no allocation once warm), and each `PooledBuf` returns its
    // buffer here on drop. Bounded so pooled memory can't grow without limit;
    // returns past the cap are freed. tokio mpsc try_recv/try_send are
    // non-blocking, so neither the RT callback nor a consumer's drop blocks.
    let (free_tx, free_rx) = mpsc::channel::<Vec<u8>>(CAPTURE_POOL_CAP);

    let _listener = stream
        .add_local_listener_with_user_data(())
        .process({
            let pcm_tx = pcm_tx.clone();
            let free_tx = free_tx;
            let mut free_rx = free_rx;
            move |stream, _| {
                let Some(mut buffer) = stream.dequeue_buffer() else {
                    return;
                };
                let datas = buffer.datas_mut();
                let Some(data) = datas.first_mut() else {
                    return;
                };
                let offset = data.chunk().offset() as usize;
                let size = data.chunk().size() as usize;
                if let Some(slice) = data.data() {
                    let end = (offset + size).min(slice.len());
                    if end > offset {
                        // Reuse a pooled buffer (retains its capacity → no
                        // RT-thread allocation in steady state); allocate only
                        // until the pool warms up.
                        let mut buf = free_rx.try_recv().unwrap_or_default();
                        buf.clear();
                        buf.extend_from_slice(&slice[offset..end]);
                        // Non-blocking send: on `Full` the consumer is behind, so
                        // drop this chunk (a glitch beats unbounded backlog) — the
                        // returned PooledBuf frees its buffer back to the pool via
                        // Drop. On `Closed` the consumer is gone (the app calls
                        // CaptureHandle::stop when done). Either way, never block
                        // the RT thread.
                        if let Err(e) = pcm_tx.try_send(PooledBuf { buf, ret: free_tx.clone() }) {
                            if matches!(e, mpsc::error::TrySendError::Full(_)) {
                                let n = CAPTURE_DROPPED.fetch_add(1, Ordering::Relaxed) + 1;
                                // ~every 0.5 s of dropped audio at a ~21 ms quantum.
                                if n % 24 == 0 {
                                    tracing::debug!("sendspin capture (node {target_node_id}): consumer behind, dropped {n} chunks total");
                                }
                            }
                        }
                    }
                }
            }
        })
        .state_changed({
            let error = error.clone();
            let mainloop = mainloop.clone();
            move |_stream, _, _old, new| {
                if let pw::stream::StreamState::Error(e) = new {
                    *error.borrow_mut() = Some(e);
                    mainloop.quit();
                }
            }
        })
        .register()
        .map_err(|e| format!("register stream listener: {e}"))?;

    // Format: S16LE at the fixed rate/channels every sendspin output uses.
    let mut audio_info = spa::param::audio::AudioInfoRaw::new();
    audio_info.set_format(spa::param::audio::AudioFormat::S16LE);
    audio_info.set_rate(rate);
    audio_info.set_channels(CHANNELS as u32);
    let mut position = [0; spa::param::audio::MAX_CHANNELS];
    position[0] = pw::spa::sys::SPA_AUDIO_CHANNEL_FL;
    position[1] = pw::spa::sys::SPA_AUDIO_CHANNEL_FR;
    audio_info.set_position(position);

    let values: Vec<u8> = pw::spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &pw::spa::pod::Value::Object(pw::spa::pod::Object {
            type_: pw::spa::sys::SPA_TYPE_OBJECT_Format,
            id: pw::spa::sys::SPA_PARAM_EnumFormat,
            properties: audio_info.into(),
        }),
    )
    .map_err(|e| format!("serialize format pod: {e}"))?
    .0
    .into_inner();
    let mut params = [Pod::from_bytes(&values).ok_or("invalid format pod")?];

    stream
        .connect(
            spa::utils::Direction::Input,
            Some(target_node_id),
            pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS | pw::stream::StreamFlags::RT_PROCESS,
            &mut params,
        )
        .map_err(|e| format!("connect stream to node {target_node_id}: {e}"))?;

    let mainloop_for_cmd = mainloop.clone();
    let _cmd_receiver = cmd_rx.attach(mainloop.loop_(), move |cmd| match cmd {
        CaptureCmd::Stop => mainloop_for_cmd.quit(),
    });

    tracing::info!("sendspin capture connected to node {target_node_id}");
    // Steady-state for as long as this output is configured, not a one-shot
    // roundtrip — stopped externally via CaptureHandle::stop, or on error.
    mainloop.run();

    if let Some(e) = error.borrow_mut().take() {
        return Err(e);
    }
    Ok(())
}
