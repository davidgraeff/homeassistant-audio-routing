//! Spike: synchronized AirPlay-2 **test tone** via the PROVEN file path.
//!
//! Reproduces the standalone x86 spike (`AudioDecoder` + `Connection::start_streaming`)
//! *inside* the RPi daemon, to isolate whether AP2 + PTP multi-room fundamentally
//! works on the Pi — independent of the realtime-capture producer that jitters on
//! the live output path (`start_streaming_live`).
//!
//! For each target receiver it: connects (transient pairing, PIN 3939), injects the
//! daemon's single libairptp grandmaster `clock_id` (so every receiver shares one
//! PTP timeline → coincident playback), and streams the SAME in-memory sine-tone WAV
//! via `start_streaming`. The file path decodes ahead from memory, so — unlike the
//! live path — it is NOT gated by realtime capture. If the tone is audible and in
//! sync on both speakers here, the stack (pairing, PTP, ALAC, encryption, render
//! buffer) is sound on the Pi and the live path's jitter is the remaining problem;
//! if it is silent here too, the fault is deeper than jitter.
//!
//! Two modes (bisection knob): `mode:"file"` (default) uses `AudioDecoder` +
//! `start_streaming` — the known-good path proven audible on hardware; `mode:"live"`
//! uses the SAME `LiveAudioDecoder` + `start_streaming_live` the live output path
//! (`ap2_server`) uses, but fed a clean in-process sine (blocking `send`, no capture,
//! no daemon-load jitter). If `file` plays and `live` is silent, the fault is in the
//! live decoder / `start_streaming_live` path — not the capture or the feed.
//!
//! `POST /api/spike/ap2` to start, `DELETE /api/spike/ap2` to stop. Single-slot:
//! starting again replaces any running tone.
//!
//! NOTE: the spike and the live output target the same physical receivers and each
//! receiver accepts only one session — unroute any `ap2-dev-*` output from a source
//! before running the spike, or the connect will race the live session.

use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use airplay_client::{AudioDecoder, Connection, LiveAudioDecoder, LivePcmFrame};
use airplay_core::codec::{AudioFormat, SampleRate};
use airplay_core::stream::{PtpMode, StreamConfig, TimingProtocol};
use tokio::sync::oneshot;

use crate::ap2_ptp::SharedAp2Ptp;
use crate::ap2_server::{build_device, ALAC_MAGIC_COOKIE};

/// ALAC magic cookie (ASC) for a given sample rate: the base cookie with its last
/// 4 bytes (the sample rate, big-endian) rewritten. `ALAC_MAGIC_COOKIE` encodes
/// 44100 (`…, 172, 68` = 0x0000AC44); 48000 = 0x0000BB80 = `…, 187, 128`. Used to
/// test whether the receivers accept 48 kHz realtime ALAC.
fn alac_cookie(rate: u32) -> [u8; 24] {
    let mut c = ALAC_MAGIC_COOKIE;
    c[20..24].copy_from_slice(&rate.to_be_bytes());
    c
}

/// AP2 realtime StreamConfig at `rate` — ALAC/16/stereo, PTP master, with the
/// matching cookie. `audio_format.sample_rate` drives the SETUP `audioFormat` bit
/// (`airplay_format_value`), which MUST agree with the cookie's rate.
fn spike_config(rate: u32) -> StreamConfig {
    let mut audio_format = AudioFormat::default(); // ALAC/16/2, 352 frames/packet
    audio_format.sample_rate = if rate >= 48_000 { SampleRate::Hz48000 } else { SampleRate::Hz44100 };
    StreamConfig {
        timing_protocol: TimingProtocol::Ptp,
        ptp_mode: PtpMode::Master,
        audio_format,
        asc: Some(alac_cookie(rate).to_vec()),
        latency_min: 22050, // ~500ms .. 2s receiver render buffer
        latency_max: 88200,
        ..Default::default()
    }
}

/// A running tone spike.
///
/// Teardown is deterministic: `stop()` sets `stop_flag` (live-mode feeder threads
/// observe it each iteration and exit promptly — independent of the audio channel),
/// fires `shutdown` (the task disconnects every receiver, dropping the decoders),
/// awaits the task, then **joins** every feeder thread so none outlives the spike.
/// `Drop` is a best-effort safety net that only signals (no join).
struct Ap2ToneSpike {
    shutdown: Option<oneshot::Sender<()>>,
    /// Set true to tell every live-mode feeder thread to exit. Shared with the feeders.
    stop_flag: Arc<AtomicBool>,
    /// Join handles for the live-mode "ap2-spike-tone" feeder threads, populated by
    /// the task as it connects each receiver. Joined in `stop()`.
    feeders: Arc<Mutex<Vec<std::thread::JoinHandle<()>>>>,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl Drop for Ap2ToneSpike {
    fn drop(&mut self) {
        // Safety net for a drop that bypasses stop(): signal so threads can exit.
        // (The authoritative teardown — including the join — happens in stop().)
        self.stop_flag.store(true, Ordering::Release);
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(()); // task wakes → disconnects receivers
        }
    }
}

/// Single global slot — one tone spike at a time (mirrors `per_device_spike`).
fn slot() -> &'static Arc<Mutex<Option<Ap2ToneSpike>>> {
    static SLOT: OnceLock<Arc<Mutex<Option<Ap2ToneSpike>>>> = OnceLock::new();
    SLOT.get_or_init(|| Arc::new(Mutex::new(None)))
}

/// `secs` of a stereo sine tone at `freq_hz`, as a `rate`/16-bit WAV byte buffer.
fn tone_wav(freq_hz: f32, secs: f32, rate: u32) -> Vec<u8> {
    let n = (secs * rate as f32).max(0.0) as usize;
    let mut pcm = Vec::with_capacity(n * 4); // 2 ch * 2 bytes
    let amp = 8000.0f32; // ~ -12 dBFS — clearly audible, not harsh
    let two_pi_f = 2.0 * std::f32::consts::PI * freq_hz;
    for i in 0..n {
        let t = i as f32 / rate as f32;
        let v = ((two_pi_f * t).sin() * amp) as i16;
        let b = v.to_le_bytes();
        pcm.extend_from_slice(&b); // L
        pcm.extend_from_slice(&b); // R
    }
    crate::wav::build_wav(&pcm, rate, 16, 2)
}

/// What the start endpoint reports back.
pub struct Ap2SpikeInfo {
    pub message: String,
    pub targets: Vec<String>,
}

/// Connect one receiver via the file path and start streaming the tone WAV.
async fn connect_tone(
    ip: IpAddr,
    clock_id: u64,
    render_delay_ms: u32,
    wav: &[u8],
    rate: u32,
) -> Result<Connection, String> {
    let device = build_device(ip);
    let config = spike_config(rate);
    let mut conn = Connection::connect_auto(device, config, "3939")
        .await
        .map_err(|e| format!("connect/pair: {e}"))?;
    conn.set_ptp_clock_id(clock_id);
    conn.set_render_delay_ms(render_delay_ms);
    conn.setup().await.map_err(|e| format!("SETUP: {e}"))?;
    // File path: decode the in-memory tone WAV (symphonia) and stream it. Buffers
    // ahead from memory — NOT gated by realtime capture like start_streaming_live.
    let decoder = AudioDecoder::from_bytes(wav, Some("wav")).map_err(|e| format!("decode tone: {e}"))?;
    conn.start_streaming(decoder).await.map_err(|e| format!("start_streaming: {e}"))?;
    Ok(conn)
}

/// Connect one receiver via the **LIVE** path — the same `LiveAudioDecoder` +
/// `start_streaming_live` `ap2_server` uses — but fed a clean in-process sine tone
/// (blocking `send`, so the feed never starves and never drops), with NO PipeWire
/// capture and no daemon-load jitter. This is the bisection: if the file mode plays
/// but this is silent, the fault is in the live decoder / `start_streaming_live`
/// path itself, not the capture or the feed.
async fn connect_tone_live(
    ip: IpAddr,
    clock_id: u64,
    render_delay_ms: u32,
    freq_hz: f32,
    rate: u32,
    stop_flag: Arc<AtomicBool>,
) -> Result<(Connection, std::thread::JoinHandle<()>), String> {
    let device = build_device(ip);
    let config = spike_config(rate);
    let mut conn = Connection::connect_auto(device, config, "3939")
        .await
        .map_err(|e| format!("connect/pair: {e}"))?;
    conn.set_ptp_clock_id(clock_id);
    conn.set_render_delay_ms(render_delay_ms);
    conn.setup().await.map_err(|e| format!("SETUP: {e}"))?;

    let (sender, decoder) = LiveAudioDecoder::create_pair(rate, 2, 128);

    // Feeder: phase-continuous sine, stereo i16 at TONE_RATE. Uses `try_send` so it
    // NEVER blocks in the channel past a stop: it checks `stop_flag` every iteration
    // and exits promptly regardless of channel state. When the (bounded) channel is
    // full it sleeps briefly — pacing to the decoder's consumption, the "decode-ahead,
    // never starve" behaviour, without any capture. Phase only advances on a
    // successful send, so a dropped/retried frame stays continuous. Started BEFORE
    // start_streaming_live so start_live's prefill fills immediately.
    let feeder = std::thread::Builder::new()
        .name("ap2-spike-tone".into())
        .spawn(move || {
            let chunk = (rate / 100) as usize; // 10ms of frames at this rate
            let phase_inc = 2.0 * std::f32::consts::PI * freq_hz / rate as f32;
            let amp = 8000.0f32;
            let mut phase = 0.0f32;
            while !stop_flag.load(Ordering::Acquire) {
                // Build the next chunk from the *current* phase without committing the
                // advance — so if the send fails we regenerate the identical frame.
                let mut next_phase = phase;
                let mut samples = Vec::with_capacity(chunk * 2);
                for _ in 0..chunk {
                    let v = (next_phase.sin() * amp) as i16;
                    samples.push(v); // L
                    samples.push(v); // R
                    next_phase += phase_inc;
                    if next_phase > std::f32::consts::TAU {
                        next_phase -= std::f32::consts::TAU;
                    }
                }
                if sender.try_send(LivePcmFrame { samples, channels: 2, sample_rate: rate }) {
                    phase = next_phase; // commit only on success
                } else {
                    // Channel full (or decoder dropped) — pace, then re-check stop_flag.
                    std::thread::sleep(Duration::from_millis(2));
                }
            }
            tracing::debug!("ap2 spike tone feeder exiting");
        })
        .map_err(|e| format!("spawn tone feeder: {e}"))?;

    conn.start_streaming_live(decoder)
        .await
        .map_err(|e| format!("start_streaming_live: {e}"))?;
    Ok((conn, feeder))
}

/// Start the tone spike to `targets` (display-name, IP). Reuses the daemon's single
/// libairptp grandmaster (`ptp`) so it shares 319/320 rather than double-binding.
/// `live=false` uses the file path (`start_streaming`); `live=true` uses the live
/// path (`start_streaming_live` + `LiveAudioDecoder`) — the bisection knob.
pub async fn start(
    targets: Vec<(String, IpAddr)>,
    ptp: &SharedAp2Ptp,
    freq_hz: f32,
    secs: f32,
    render_delay_ms: u32,
    live: bool,
    rate: u32,
    file_wav: Option<Vec<u8>>,
) -> Result<Ap2SpikeInfo, String> {
    if targets.is_empty() {
        return Err("no target receivers".into());
    }
    // Single slot: tear down any prior spike first.
    stop().await;

    let clock_id = ptp.ensure_started()?;
    for (_, ip) in &targets {
        let _ = ptp.add_peer(&ip.to_string());
    }

    // File mode streams a WAV (buffers ahead, never starves): a caller-supplied clip
    // (e.g. the decoded test-announcement voice — a voice makes a wrong playback rate
    // obvious to the ear) if given, else the generated sine tone. Live mode ignores it
    // (it feeds the synthetic sine directly).
    let wav = if live { Vec::new() } else { file_wav.unwrap_or_else(|| tone_wav(freq_hz, secs, rate)) };
    let names: Vec<String> = targets.iter().map(|(n, _)| n.clone()).collect();
    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();

    let stop_flag = Arc::new(AtomicBool::new(false));
    let feeders: Arc<Mutex<Vec<std::thread::JoinHandle<()>>>> = Arc::new(Mutex::new(Vec::new()));

    let targets_task = targets.clone();
    let stop_flag_task = stop_flag.clone();
    let feeders_task = feeders.clone();
    let task = tokio::spawn(async move {
        let mut conns: Vec<Connection> = Vec::new();
        for (name, ip) in &targets_task {
            let r = if live {
                connect_tone_live(*ip, clock_id, render_delay_ms, freq_hz, rate, stop_flag_task.clone())
                    .await
                    .map(|(c, feeder)| {
                        feeders_task.lock().unwrap().push(feeder);
                        c
                    })
            } else {
                connect_tone(*ip, clock_id, render_delay_ms, &wav, rate).await
            };
            match r {
                Ok(c) => {
                    tracing::info!("AP2 spike ({}): tone → '{}' ({})", if live { "live" } else { "file" }, name, ip);
                    conns.push(c);
                }
                Err(e) => tracing::warn!("AP2 spike: '{}' ({}) failed: {}", name, ip, e),
            }
        }
        if conns.is_empty() {
            tracing::warn!("AP2 spike: no receivers connected");
        }
        // Hold the sessions open until stop() drops the handle. (File mode EOFs after
        // `secs`; live mode streams continuously until stopped.)
        let _ = shutdown_rx.await;
        for mut c in conns {
            let _ = c.stop().await;
            let _ = c.disconnect().await;
        }
        tracing::info!("AP2 spike: stopped");
    });

    *slot().lock().unwrap() = Some(Ap2ToneSpike {
        shutdown: Some(shutdown_tx),
        stop_flag,
        feeders,
        task: Some(task),
    });
    let mode = if live { "live (start_streaming_live)".to_string() } else { format!("file ({secs:.0}s)") };
    Ok(Ap2SpikeInfo {
        message: format!(
            "streaming {freq_hz:.0}Hz tone [{mode}] @ {rate}Hz to {} receiver(s) (render_delay={render_delay_ms}ms)",
            names.len()
        ),
        targets: names,
    })
}

/// Tear down the running tone spike (no-op if none). Deterministic: signal the
/// feeders to exit, disconnect every receiver, await the task, then **join** every
/// feeder thread so none outlives the spike (see `Ap2ToneSpike`).
pub async fn stop() -> String {
    let taken = slot().lock().unwrap().take();
    let Some(mut spike) = taken else {
        return "no AP2 tone spike running".into();
    };

    // 1. Signal feeders to stop (they check this every iteration, so they can't
    //    block forever in the channel).
    spike.stop_flag.store(true, Ordering::Release);
    // 2. Wake the task → it disconnects the connections and drops the decoders.
    if let Some(tx) = spike.shutdown.take() {
        let _ = tx.send(());
    }
    // 3. Await the task's teardown. This also guarantees the connect loop finished,
    //    so every feeder handle has been registered in `feeders`.
    if let Some(task) = spike.task.take() {
        let _ = task.await;
    }
    // 4. Join the feeder threads off the async executor. They've all observed the
    //    stop flag by now, so this returns promptly.
    let feeders = std::mem::take(&mut *spike.feeders.lock().unwrap());
    if !feeders.is_empty() {
        let _ = tokio::task::spawn_blocking(move || {
            for h in feeders {
                let _ = h.join();
            }
        })
        .await;
    }

    "AP2 tone spike stopped".into()
}
