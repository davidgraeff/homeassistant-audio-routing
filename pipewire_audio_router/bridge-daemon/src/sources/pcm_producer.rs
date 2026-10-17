//! The PipeWire half of an in-process receiver: a long-lived **producer** node
//! (a source in the graph) fed from a lock-free SPSC jitter-buffer ring.
//!
//! The receiver side ([`super::airplay`], [`super::spotify`]) pushes decoded,
//! interleaved f32 at [`RATE`]/[`CHANNELS`] into the ring; the node's
//! `RT_PROCESS` callback drains it on the graph data-loop, prebuffering to the
//! configured target first and re-arming to a small guard after an underrun.
//! The node exists for as long as the source is configured, so it is always
//! routable — outputting silence while nothing is being received.
//!
//! `label` names the receiver in thread names and log lines only.

use pipewire as pw;
use pw::spa;
use spa::pod::Pod;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

/// The producer node's fixed format: 44.1 kHz stereo, which is what both
/// AirPlay (AP1 ALAC and, so far, AP2 AAC) and Spotify deliver.
pub(crate) const RATE: u32 = 44_100;
pub(crate) const CHANNELS: usize = 2;

/// After a mid-stream ring underrun, re-arm the jitter buffer to this much audio
/// before draining again — a small hysteresis guard, **not** the full cold-start
/// prebuffer (the source's configured latency). A transient late/lost packet then
/// costs ~one quantum plus this guard instead of a full ~150 ms of silence
/// re-injected at the source and fanned to every output of the group (RC2 in
/// docs/audio-jitter-analysis.md), while still avoiding a per-quantum underrun
/// train. (Chronic underfeed from producer↔graph clock drift — RC3 — needs an
/// adaptive resampler; this only bounds *transient* gaps.)
pub(crate) const REARM_MSEC: u32 = 40;

/// Producer half of the lock-free SPSC jitter-buffer ring (interleaved f32),
/// shared behind a `Mutex` only to serialize the *producer* side across sessions
/// (a session teardown + a new session can briefly overlap). The `Mutex` is
/// **never** touched by the consumer: the PipeWire RT process callback owns the
/// `rtrb::Consumer` and pops lock-free, so a graph cycle can never block on the
/// decode thread (the old `Mutex<VecDeque>` could → `airplay-in` xruns → stutter).
pub(crate) type RingProducer = Arc<Mutex<rtrb::Producer<f32>>>;

/// Interleaved-f32 samples for `msec` of audio at the producer's rate/channels.
pub(crate) fn samples_for_ms(msec: u32) -> usize {
    (RATE as usize) * CHANNELS * (msec as usize) / 1000
}

pub(crate) enum ProducerCmd {
    Stop,
}

/// Spawn the PipeWire producer on a dedicated thread (mirrors
/// sendspin_capture's thread+channel+mainloop shape). Returns a stop sender.
/// `target` is the jitter-buffer prebuffer, in interleaved-f32 samples.
pub(crate) fn spawn_producer(
    label: &'static str,
    node_name: String,
    consumer: rtrb::Consumer<f32>,
    peak: Arc<AtomicU32>,
    target: usize,
    flush: Arc<AtomicBool>,
) -> Result<pw::channel::Sender<ProducerCmd>, String> {
    let (cmd_tx, cmd_rx) = pw::channel::channel::<ProducerCmd>();
    std::thread::Builder::new()
        .name(format!("{}-producer", label.to_ascii_lowercase()))
        .spawn(move || {
            if let Err(e) = run_producer(label, node_name, consumer, peak, target, cmd_rx, flush) {
                tracing::error!("{label} PipeWire producer exited with error: {e}");
            }
        })
        .map_err(|e| format!("spawn producer thread: {e}"))?;
    Ok(cmd_tx)
}

/// Elevate the AirPlay producer thread to `SCHED_FIFO` so its control path
/// preempts the normal-priority worker pool (notably the busy mDNS
/// `ServiceDaemon` threads) under host CPU contention. As with the sendspin
/// capture thread, the steady-state PCM fill runs in the `RT_PROCESS` callback
/// on PipeWire's data-loop (already FIFO 83); this thread mostly sleeps in the
/// mainloop poll, so elevating it protects prompt stream (re)connect / flush /
/// stop handling (flaky-connection resilience) and can never spin-lock a core.
/// Priority 45 mirrors the sendspin capture thread — above the relays (40),
/// below the AP2 sender (50) and PipeWire's data-loop (83). Best-effort:
/// without `CAP_SYS_NICE` it logs and continues at normal priority.
#[cfg(target_os = "linux")]
fn set_producer_realtime_priority(label: &str) {
    // SAFETY: sched_setscheduler on the current thread (pid 0) with a valid,
    // zero-initialised sched_param; no aliasing, no ownership transfer.
    unsafe {
        let mut param: libc::sched_param = std::mem::zeroed();
        param.sched_priority = 45;
        if libc::sched_setscheduler(0, libc::SCHED_FIFO, &param) == 0 {
            tracing::info!("{label} producer: real-time priority set (SCHED_FIFO, priority 45)");
        } else {
            tracing::debug!("{label} producer: could not set RT priority (need CAP_SYS_NICE); running at normal priority");
        }
    }
}
#[cfg(not(target_os = "linux"))]
fn set_producer_realtime_priority(_label: &str) {}

fn run_producer(
    label: &'static str,
    node_name: String,
    consumer: rtrb::Consumer<f32>,
    peak: Arc<AtomicU32>,
    target: usize,
    cmd_rx: pw::channel::Receiver<ProducerCmd>,
    flush: Arc<AtomicBool>,
) -> Result<(), String> {
    set_producer_realtime_priority(label);
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None).map_err(|e| format!("mainloop: {e}"))?;
    let context = pw::context::ContextRc::new(&mainloop, None).map_err(|e| format!("context: {e}"))?;
    let core = context.connect_rc(None).map_err(|e| format!("connect to PipeWire: {e}"))?;

    let stream = pw::stream::StreamBox::new(
        &core,
        &node_name,
        pw::properties::properties! {
            *pw::keys::MEDIA_TYPE => "Audio",
            *pw::keys::MEDIA_CATEGORY => "Playback",
            *pw::keys::NODE_NAME => node_name.as_str(),
        },
    )
    .map_err(|e| format!("create stream: {e}"))?;

    let stride = CHANNELS * std::mem::size_of::<f32>(); // bytes per frame
    let error: std::rc::Rc<std::cell::RefCell<Option<String>>> = std::rc::Rc::new(std::cell::RefCell::new(None));
    // The prebuffer arm threshold: the full `target` at cold start, dropping to
    // the small `rearm_target` after the first mid-stream underrun so recovery
    // stays near-realtime (M4 / RC2). Clamped so a very low configured latency
    // never makes the re-arm guard *larger* than the cold-start prebuffer.
    let rearm_target = samples_for_ms(REARM_MSEC).min(target).max(1);

    let _listener = stream
        .add_local_listener_with_user_data(())
        // The process callback runs RT (RT_PROCESS, on the graph data-loop). It
        // owns the SPSC consumer and its jitter-buffer state exclusively (no Rc
        // shared with this thread — that would be a data race once RT_PROCESS
        // moves the callback off this mainloop thread); `flush`/`peak` are shared
        // only via atomics. It must never block: `pop` is lock-free.
        .process({
            let peak = peak.clone();
            // Owned, mutated across invocations (FnMut). Start in prebuffer mode.
            let mut consumer = consumer;
            let mut draining = false;
            let mut arm_target = target;
            move |stream, _| {
                let Some(mut buffer) = stream.dequeue_buffer() else {
                    return;
                };
                let datas = buffer.datas_mut();
                let Some(data) = datas.first_mut() else {
                    return;
                };
                // Session ended / FLUSH: drop stale buffered audio and re-arm the
                // full cold-start prebuffer for the next session.
                if flush.swap(false, Ordering::Relaxed) {
                    while consumer.pop().is_ok() {}
                    draining = false;
                    arm_target = target;
                }
                let filled = if let Some(slice) = data.data() {
                    let cap_frames = slice.len() / stride;
                    let want = cap_frames * CHANNELS; // f32 samples wanted
                                                      // Leave prebuffer once we've accumulated the current arm
                                                      // threshold (full `target` cold, small `rearm_target` after
                                                      // a prior underrun).
                    if !draining && consumer.slots() >= arm_target {
                        draining = true;
                    }
                    let mut got = 0usize;
                    if draining {
                        // Bulk-read up to a full quantum in one lock-free chunk
                        // (two slices when the ring wraps).
                        let n = consumer.slots().min(want);
                        if let Ok(chunk) = consumer.read_chunk(n) {
                            let (s0, s1) = chunk.as_slices();
                            for &s in s0.iter().chain(s1.iter()) {
                                slice[got * 4..got * 4 + 4].copy_from_slice(&s.to_le_bytes());
                                got += 1;
                            }
                            chunk.commit_all();
                        }
                        // Underran mid-quantum: re-prebuffer before draining again
                        // so we don't dribble out repeated micro-gaps — but only to
                        // the small `rearm_target` guard, not the full cold-start
                        // prebuffer, so recovery stays near-realtime (M4 / RC2).
                        if got < want {
                            draining = false;
                            arm_target = rearm_target;
                        }
                    }
                    // Zero-pad the rest of the quantum (prebuffering or underrun)
                    // and decay the meter so it falls when audio stops.
                    if got < want {
                        for b in &mut slice[got * 4..cap_frames * stride] {
                            *b = 0;
                        }
                        let p = f32::from_bits(peak.load(Ordering::Relaxed));
                        peak.store((p * 0.8).to_bits(), Ordering::Relaxed);
                    }
                    cap_frames * stride // always emit a full quantum
                } else {
                    0
                };
                let chunk = data.chunk_mut();
                *chunk.offset_mut() = 0;
                *chunk.stride_mut() = stride as _;
                *chunk.size_mut() = filled as _;
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

    let mut audio_info = spa::param::audio::AudioInfoRaw::new();
    audio_info.set_format(spa::param::audio::AudioFormat::F32LE);
    audio_info.set_rate(RATE);
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

    // Direction::Output = a producer (source in the graph). No AUTOCONNECT: we
    // don't want it wired to the default sink — the routing reconciler links it
    // where the user routes it. RT_PROCESS: run the process callback on the
    // graph's RT data-loop so it's never preempted by general-purpose async work
    // (the non-RT producer was the live stutter cause — airplay-in xruns). Safe
    // now that the jitter buffer is a lock-free SPSC ring (no mutex in the
    // callback), so there's no RT priority inversion.
    stream
        .connect(
            spa::utils::Direction::Output,
            None,
            pw::stream::StreamFlags::MAP_BUFFERS | pw::stream::StreamFlags::RT_PROCESS,
            &mut params,
        )
        .map_err(|e| format!("connect producer stream: {e}"))?;

    let mainloop_for_cmd = mainloop.clone();
    let _cmd_receiver = cmd_rx.attach(mainloop.loop_(), move |cmd| match cmd {
        ProducerCmd::Stop => mainloop_for_cmd.quit(),
    });

    tracing::info!("{label} producer node '{node_name}' ready");
    mainloop.run();

    if let Some(e) = error.borrow_mut().take() {
        return Err(e);
    }
    Ok(())
}
