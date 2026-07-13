//! Native AirPlay-receive source: an embedded `shairplay` RAOP server whose
//! decoded PCM is fed into a PipeWire **source** node the daemon owns.
//!
//! Replaces the shairport-sync subprocess. The Ubuntu shairport-sync has no
//! PipeWire backend (alsa/pipe/stdout only), so its audio never reached the
//! graph — the AirPlay source could never appear in the routing matrix or be
//! routed. `shairplay` (pure Rust, spike-validated) hands us decoded f32 PCM
//! via a callback; we push it through a bounded ring buffer into a PipeWire
//! producer stream (mirrors player.rs's playback stream, but long-lived and
//! fed live instead of from a WAV).
//!
//! The producer node is created as soon as the AirPlay source is *configured*
//! (not only while a device is casting), so it's always present in the matrix
//! as a routable source — outputting silence when idle. A `mem`-cheap peak
//! level is computed inline from the received PCM for the UI meter.

use crate::locks::LockRecover;
use pipewire as pw;
use pw::spa;
use shairplay::{AudioFormat, AudioHandler, AudioSession, RaopServer};
use spa::pod::Pod;
use std::collections::VecDeque;
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

/// Stable PipeWire node name for the AirPlay source — what the matrix/routing
/// key on. (shairport-sync, by contrast, made an unpredictably-named node only
/// while a session was live.)
pub const AIRPLAY_NODE_NAME: &str = "airplay-in";

/// RTSP port the AirPlay receiver listens on (the RAOP default; shairport used
/// it too, and it's free once shairport is gone).
const AIRPLAY_PORT: u16 = 5000;

/// The producer node's fixed format. AirPlay is 44.1 kHz stereo in practice
/// (AP1 ALAC and, so far, AP2 AAC); `audio_init` warns if a session ever
/// reports otherwise so we'd notice and add resampling.
const RATE: u32 = 44_100;
const CHANNELS: usize = 2;

/// Shared interleaved-f32 ring buffer between shairplay's audio thread (push)
/// and the PipeWire producer callback (pop).
type Ring = Arc<Mutex<VecDeque<f32>>>;

/// Cap the buffer at ~0.5 s so a paused/absent consumer can't grow it without
/// bound; oldest samples are dropped past this.
const RING_CAP: usize = (RATE as usize) * CHANNELS / 2;

enum ProducerCmd {
    Stop,
}

/// Everything one running AirPlay source owns. Call [`AirplayHandle::stop`] to
/// tear it down cleanly (async, because the RAOP server's shutdown is async);
/// dropping without stopping still stops the PipeWire producer thread.
pub struct AirplayHandle {
    server: Option<RaopServer>,
    producer_stop: Option<pw::channel::Sender<ProducerCmd>>,
    peak: Arc<AtomicU32>,
}

impl AirplayHandle {
    /// Recent peak sample magnitude (0.0–1.0) of received AirPlay audio — for
    /// the UI level meter. Decays toward 0 when the graph pulls silence.
    pub fn peak(&self) -> f32 {
        f32::from_bits(self.peak.load(Ordering::Relaxed))
    }

    /// Stop the RAOP server (unregister mDNS, close listeners) and the PipeWire
    /// producer. Consumes the handle.
    pub async fn stop(mut self) {
        if let Some(mut server) = self.server.take() {
            server.stop().await;
        }
        if let Some(tx) = self.producer_stop.take() {
            let _ = tx.send(ProducerCmd::Stop);
        }
    }
}

impl Drop for AirplayHandle {
    fn drop(&mut self) {
        // Best-effort: if stop() wasn't called, at least stop the producer
        // thread (its mainloop quit drops the stream → removes the node).
        if let Some(tx) = self.producer_stop.take() {
            let _ = tx.send(ProducerCmd::Stop);
        }
    }
}

/// Start the AirPlay source advertised as `name`: bring up the PipeWire
/// producer node, then the embedded RAOP server feeding it.
pub async fn start(name: String) -> anyhow::Result<AirplayHandle> {
    let ring: Ring = Arc::new(Mutex::new(VecDeque::with_capacity(RING_CAP)));
    let peak = Arc::new(AtomicU32::new(0));

    let producer_stop = spawn_producer(ring.clone(), peak.clone())
        .map_err(|e| anyhow::anyhow!("failed to start AirPlay PipeWire producer: {e}"))?;

    let handler = Arc::new(Handler { ring, peak: peak.clone() });
    let mut server = RaopServer::builder()
        .name(name.clone())
        .hwaddr(derive_hwaddr(&name))
        .port(AIRPLAY_PORT)
        .build(handler)
        .map_err(|e| anyhow::anyhow!("failed to build AirPlay server: {e}"))?;
    server
        .start()
        .await
        .map_err(|e| anyhow::anyhow!("failed to start AirPlay server on port {AIRPLAY_PORT}: {e}"))?;

    Ok(AirplayHandle { server: Some(server), producer_stop: Some(producer_stop), peak })
}

/// The uppercase-hex MAC (no separators) shairplay puts before `@` in its mDNS
/// `_raop._tcp` instance name (e.g. `485D607CEE22@Music Via Airplay`). RAOP
/// discovery uses this to recognize and skip our OWN receiver — stable even
/// when mDNS appends ` (2)` to our name on a transient conflict, so it's more
/// robust than matching the friendly name.
pub fn mdns_mac(name: &str) -> String {
    derive_hwaddr(name).iter().map(|b| format!("{b:02X}")).collect()
}

/// A locally-administered, deterministic MAC derived from the source name, so
/// the AirPlay device identity is stable across restarts and unlikely to
/// collide on a LAN with other installs.
fn derive_hwaddr(name: &str) -> [u8; 6] {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    name.hash(&mut h);
    let bytes = h.finish().to_le_bytes();
    let mut mac = [0u8; 6];
    mac.copy_from_slice(&bytes[..6]);
    mac[0] = (mac[0] & 0xfe) | 0x02; // unicast + locally-administered
    mac
}

struct Handler {
    ring: Ring,
    peak: Arc<AtomicU32>,
}

impl AudioHandler for Handler {
    fn audio_init(&self, format: AudioFormat) -> Box<dyn AudioSession> {
        if format.sample_rate != RATE || format.channels as usize != CHANNELS {
            tracing::warn!(
                "AirPlay session format {}Hz/{}ch differs from the producer's {RATE}Hz/{CHANNELS}ch — audio may be wrong-pitched; resampling not yet implemented",
                format.sample_rate,
                format.channels
            );
        } else {
            tracing::info!("AirPlay stream started ({}Hz/{}ch)", format.sample_rate, format.channels);
        }
        Box::new(Session { ring: self.ring.clone(), peak: self.peak.clone() })
    }

    fn on_client_connected(&self, addr: &str) {
        tracing::info!("AirPlay client connected: {addr}");
    }
    fn on_client_disconnected(&self, addr: &str) {
        tracing::info!("AirPlay client disconnected: {addr}");
        // Clear any residual buffered audio so a new session starts clean.
        self.ring.lock_recover().clear();
    }
}

struct Session {
    ring: Ring,
    peak: Arc<AtomicU32>,
}

impl AudioSession for Session {
    fn audio_process(&mut self, samples: &[f32]) {
        // Inline peak for the meter (cheap; this is the received signal).
        let chunk_peak = samples.iter().fold(0.0f32, |m, &s| m.max(s.abs()));
        // Rise instantly, so transients show; the producer side decays it.
        let cur = f32::from_bits(self.peak.load(Ordering::Relaxed));
        self.peak.store(chunk_peak.max(cur).to_bits(), Ordering::Relaxed);

        let mut ring = self.ring.lock_recover();
        ring.extend(samples.iter().copied());
        // Bound latency: drop oldest beyond the cap.
        while ring.len() > RING_CAP {
            ring.pop_front();
        }
    }

    fn audio_flush(&mut self) {
        self.ring.lock_recover().clear();
    }
}

/// Spawn the PipeWire producer on a dedicated thread (mirrors
/// sendspin_capture's thread+channel+mainloop shape). Returns a stop sender.
fn spawn_producer(ring: Ring, peak: Arc<AtomicU32>) -> Result<pw::channel::Sender<ProducerCmd>, String> {
    let (cmd_tx, cmd_rx) = pw::channel::channel::<ProducerCmd>();
    std::thread::Builder::new()
        .name("airplay-producer".into())
        .spawn(move || {
            if let Err(e) = run_producer(ring, peak, cmd_rx) {
                tracing::error!("AirPlay PipeWire producer exited with error: {e}");
            }
        })
        .map_err(|e| format!("spawn producer thread: {e}"))?;
    Ok(cmd_tx)
}

fn run_producer(ring: Ring, peak: Arc<AtomicU32>, cmd_rx: pw::channel::Receiver<ProducerCmd>) -> Result<(), String> {
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None).map_err(|e| format!("mainloop: {e}"))?;
    let context = pw::context::ContextRc::new(&mainloop, None).map_err(|e| format!("context: {e}"))?;
    let core = context.connect_rc(None).map_err(|e| format!("connect to PipeWire: {e}"))?;

    let stream = pw::stream::StreamBox::new(
        &core,
        AIRPLAY_NODE_NAME,
        pw::properties::properties! {
            *pw::keys::MEDIA_TYPE => "Audio",
            *pw::keys::MEDIA_CATEGORY => "Playback",
            *pw::keys::NODE_NAME => AIRPLAY_NODE_NAME,
        },
    )
    .map_err(|e| format!("create stream: {e}"))?;

    let stride = CHANNELS * std::mem::size_of::<f32>(); // bytes per frame
    let error: std::rc::Rc<std::cell::RefCell<Option<String>>> = std::rc::Rc::new(std::cell::RefCell::new(None));

    let _listener = stream
        .add_local_listener_with_user_data(())
        .process({
            let ring = ring.clone();
            let peak = peak.clone();
            move |stream, _| {
                let Some(mut buffer) = stream.dequeue_buffer() else {
                    return;
                };
                let datas = buffer.datas_mut();
                let Some(data) = datas.first_mut() else {
                    return;
                };
                let filled = if let Some(slice) = data.data() {
                    let cap_frames = slice.len() / stride;
                    let want = cap_frames * CHANNELS; // f32 samples wanted
                    let mut got = 0usize;
                    {
                        let mut ring = ring.lock_recover();
                        while got < want {
                            match ring.pop_front() {
                                Some(s) => {
                                    let b = s.to_le_bytes();
                                    slice[got * 4..got * 4 + 4].copy_from_slice(&b);
                                    got += 1;
                                }
                                None => break, // underrun
                            }
                        }
                    }
                    // Zero-pad the rest of the quantum on underrun (silence) and
                    // decay the meter so it falls when audio stops.
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
    // where the user routes it. No RT_PROCESS since the process callback takes
    // a mutex (avoid RT priority inversion).
    stream
        .connect(
            spa::utils::Direction::Output,
            None,
            pw::stream::StreamFlags::MAP_BUFFERS,
            &mut params,
        )
        .map_err(|e| format!("connect producer stream: {e}"))?;

    let mainloop_for_cmd = mainloop.clone();
    let _cmd_receiver = cmd_rx.attach(mainloop.loop_(), move |cmd| match cmd {
        ProducerCmd::Stop => mainloop_for_cmd.quit(),
    });

    tracing::info!("AirPlay producer node '{AIRPLAY_NODE_NAME}' ready");
    mainloop.run();

    if let Some(e) = error.borrow_mut().take() {
        return Err(e);
    }
    Ok(())
}
