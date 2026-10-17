//! Native AirPlay-receive source: an embedded `shairplay` RAOP server whose
//! decoded PCM is fed into a PipeWire **source** node the daemon owns.
//!
//! Replaces the shairport-sync subprocess. The Ubuntu shairport-sync has no
//! PipeWire backend (alsa/pipe/stdout only), so its audio never reached the
//! graph — the AirPlay source could never appear in the routing matrix or be
//! routed. `shairplay` (pure Rust, spike-validated) hands us decoded f32 PCM
//! via a callback; we push it through a bounded ring buffer into a PipeWire
//! producer stream (mirrors pw/player.rs's playback stream, but long-lived and
//! fed live instead of from a WAV).
//!
//! The producer node is created as soon as the AirPlay source is *configured*
//! (not only while a device is casting), so it's always present in the matrix
//! as a routable source — outputting silence when idle. A `mem`-cheap peak
//! level is computed inline from the received PCM for the UI meter.

use crate::sources::airplay_clients::{self, AirplayClientStore, SharedAirplayClients};
use crate::sources::now_playing::{MetadataUpdate, NowPlayingReporter, PlaybackState};
// The producer node is fixed at 44.1 kHz stereo (pcm_producer.rs); `audio_init`
// adapts a session that negotiates anything else.
use crate::sources::pcm_producer::{samples_for_ms, spawn_producer, ProducerCmd, RingProducer, CHANNELS, RATE};
use crate::sources::{AirplaySourceConfig, SourceConfig, SourceEntry, SourceId};
use crate::util::locks::LockRecover;
use pipewire as pw;
use shairplay::{Ap1Encryption, AudioFormat, AudioHandler, AudioSession, RaopServer, SessionDecision, SessionInfo};
use std::collections::BTreeMap;
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// Shared anti-takeover flag (mirrors sources_store's `airplay_prevent_takeover`),
/// read live by the RAOP `authorize_session` gate so the API can toggle the
/// policy without restarting the receiver.
pub type SharedPreventTakeover = Arc<AtomicBool>;

/// Default RTSP port for an AirPlay receiver (the RAOP default). No longer the
/// source of truth — each source's port is allocated + persisted in
/// `AirplaySourceConfig.port` and passed to [`start`]; this const is only a
/// fallback for a config that somehow still carries port 0.
const DEFAULT_AIRPLAY_PORT: u16 = 5000;

/// Default receiver-side jitter buffer target, in milliseconds. shairplay's
/// RaopBuffer paces packet playout, but its clock and PipeWire's graph clock
/// drift; without a cushion the producer ring underruns and you hear stutter.
/// The producer prebuffers this much before draining and re-buffers on
/// underrun. 150 ms rides out LAN jitter + drift while staying imperceptible
/// for one-way audio. Stored per install (sources/mod.rs) and settable via
/// `/api/sources`, so a noisy install can trade latency for fewer
/// dropouts.
/// Node name of the old single AirPlay source. Production names sources
/// dynamically now; this survives purely as a fixture for the routing tests.
#[cfg(test)]
pub const AIRPLAY_NODE_NAME: &str = "airplay-in";

pub const DEFAULT_AIRPLAY_LATENCY_MSEC: u32 = 150;

/// Cumulative count of ingest-ring samples dropped on overflow (consumer/graph
/// stalled). Overflow should be ~never in steady state; logged, throttled.
static RING_OVERFLOW: AtomicU64 = AtomicU64::new(0);

/// Everything one running AirPlay source owns. Call [`AirplayHandle::stop`] to
/// tear it down cleanly (async, because the RAOP server's shutdown is async);
/// dropping without stopping still stops the PipeWire producer thread.
pub struct AirplayHandle {
    server: Option<RaopServer>,
    producer_stop: Option<pw::channel::Sender<ProducerCmd>>,
    peak: Arc<AtomicU32>,
    /// The service (mDNS/display) name this receiver was started with — part of
    /// the restart-diff (a rename = restart, so the mDNS advertisement updates).
    service_name: String,
    /// The config this receiver was started with — the [`reconcile`] restart
    /// diff compares the live config against the stored one.
    cfg: AirplaySourceConfig,
    /// Live anti-takeover flag shared with this receiver's session gate, so the
    /// API can toggle it (per source) without a restart.
    prevent_takeover: SharedPreventTakeover,
}

impl AirplayHandle {
    /// Recent peak sample magnitude (0.0–1.0) of received AirPlay audio — for
    /// the UI level meter. Decays toward 0 when the graph pulls silence.
    pub fn peak(&self) -> f32 {
        f32::from_bits(self.peak.load(Ordering::Relaxed))
    }

    /// Force-disconnect the sender at `addr` (a peer IP), if connected. Used by
    /// the API's manual force-disconnect.
    pub fn disconnect_client(&self, addr: &str) {
        if let Some(server) = &self.server {
            server.disconnect_client(addr);
        }
    }

    /// Toggle this receiver's anti-takeover policy live (no restart), used by
    /// `PUT /api/sources/{id}/policy`.
    pub fn set_prevent_takeover(&self, prevent: bool) {
        self.prevent_takeover.store(prevent, Ordering::Relaxed);
    }

    /// Whether the given entry differs from what this handle was started with in
    /// a way that requires a full restart (service name / port / latency /
    /// auth-setup). A `prevent_takeover`-only change does **not** need a restart
    /// (it's toggled live via [`set_prevent_takeover`]); a kind mismatch or a
    /// non-AirPlay entry is treated as "restart" (the reconciler will re-derive).
    fn needs_restart(&self, entry: &SourceEntry) -> bool {
        let SourceConfig::Airplay(new) = &entry.config else { return true };
        self.service_name != entry.label
            || self.cfg.port != new.port
            || self.cfg.latency_msec != new.latency_msec
            || self.cfg.auth_setup != new.auth_setup
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

/// Start one AirPlay receiver: bring up its PipeWire producer node (named
/// `node_name`), then the embedded RAOP server that feeds it, advertised over
/// mDNS as `service_name` on RTSP `port`. `latency_msec` is the jitter-buffer
/// target the producer prebuffers before draining. `clients` /
/// `prevent_takeover` are this receiver's **own** per-source registry + policy
/// flag (Phase 4 — each receiver is independent).
///
/// `auth_setup` additionally advertises the MFi auth-setup encryption mode
/// (`et=0,4`) so encryption-*requiring* senders can negotiate. Off by default:
/// PipeWire's raop-discover selects the highest `et`, so enabling it switches
/// PipeWire from unencrypted to auth_setup — a different (still-supported) path
/// that broadens compatibility to non-Apple senders that demand encryption.
#[allow(clippy::too_many_arguments)]
pub async fn start(
    node_name: String,
    service_name: String,
    port: u16,
    latency_msec: u32,
    auth_setup: bool,
    clients: SharedAirplayClients,
    prevent_takeover: SharedPreventTakeover,
    now_playing: NowPlayingReporter,
) -> anyhow::Result<AirplayHandle> {
    // A config left at port 0 shouldn't reach here (the store allocates on add/
    // load), but fall back to the RAOP default rather than binding port 0.
    let port = if port == 0 { DEFAULT_AIRPLAY_PORT } else { port };
    // STACK lifecycle marker: a full AirPlay-receiver (re)start. If this appears in
    // the logs WITHOUT a preceding `USER ACTION: set AirPlay source`, the receiver
    // is being restarted by the stack (a bug) rather than by a human — that's the
    // signal distinguishing the two for the `airplay-in` cycling investigation.
    tracing::info!(
        "STACK: sources::airplay::start — (re)starting AirPlay receiver node={node_name:?} (name={service_name:?}, port={port})"
    );
    // Fresh receiver: nothing is streaming yet, so clear any stale live flags a
    // prior handle might have left (a restart that skipped a disconnect).
    clients.reset_connected();

    let target = samples_for_ms(latency_msec);
    // Bound the ring well past the prebuffer target so a stalled consumer can't
    // grow it without limit; the ring is fixed-capacity (rtrb) — pushes past it
    // are dropped.
    let cap = (target * 4).max(samples_for_ms(1000));
    let (producer, consumer) = rtrb::RingBuffer::<f32>::new(cap);
    let ring_prod: RingProducer = Arc::new(Mutex::new(producer));
    // Set by the decode side on session-end / FLUSH; the RT consumer drains the
    // ring and re-arms a cold prebuffer on its next cycle (see run_producer).
    let flush = Arc::new(AtomicBool::new(false));
    let peak = Arc::new(AtomicU32::new(0));

    let producer_stop = spawn_producer("AirPlay", node_name.clone(), consumer, peak.clone(), target, flush.clone())
        .map_err(|e| anyhow::anyhow!("failed to start AirPlay PipeWire producer: {e}"))?;

    let handler =
        Arc::new(Handler { ring_prod, flush, peak: peak.clone(), clients, prevent_takeover: prevent_takeover.clone(), now_playing });
    let mut builder = RaopServer::builder().name(service_name.clone()).hwaddr(derive_hwaddr(&service_name)).port(port);
    // Advertise on the process-wide shared, LAN-restricted mDNS daemon so the
    // receiver's `_raop._tcp`/`_airplay._tcp` records share one interface-pinned
    // daemon thread with everything else — avoids the host-network multicast
    // amplification across Docker veths. Falls back to shairplay's own daemon if
    // the shared one is unavailable.
    if let Some(daemon) = crate::supervisor::shared_advertise_daemon() {
        builder = builder.mdns_daemon(daemon);
    }
    if auth_setup {
        // Offer both — the sender picks. Leaving RSA out (et=1) is deliberate:
        // PipeWire's RSA path is broken (see decisions.md). Codecs stay at the
        // crate default (cn=0,1 = PCM+ALAC).
        builder = builder.advertise_encryption(vec![Ap1Encryption::None, Ap1Encryption::AuthSetup]);
    }
    let mut server = builder.build(handler).map_err(|e| anyhow::anyhow!("failed to build AirPlay server: {e}"))?;
    server.start().await.map_err(|e| anyhow::anyhow!("failed to start AirPlay server on port {port}: {e}"))?;

    let cfg = AirplaySourceConfig { latency_msec, auth_setup, prevent_takeover: prevent_takeover.load(Ordering::Relaxed), port };
    Ok(AirplayHandle { server: Some(server), producer_stop: Some(producer_stop), peak, service_name, cfg, prevent_takeover })
}

/// Runtime collection of running AirPlay receivers, keyed by source id. Held by
/// `main.rs`/`AppState` and reconciled against the [`SourcesStore`] by
/// [`reconcile`]. `tokio` mutex because start/stop `.await`.
///
/// [`SourcesStore`]: crate::sources::SourcesStore
pub type SharedAirplayMap = Arc<tokio::sync::Mutex<BTreeMap<SourceId, AirplayHandle>>>;

/// Reconcile the running AirPlay receivers against the stored source list:
/// start receivers that are configured but not running, stop ones that were
/// removed (or disabled — an AirPlay entry whose label is blank), and restart
/// one whose restart-significant config changed (name/port/latency/auth-setup).
/// A `prevent_takeover`-only change is applied live without a restart.
///
/// Each receiver gets its OWN per-source client registry (from `clients`) and
/// its own `prevent_takeover` flag seeded from the stored config. Best-effort:
/// a receiver that fails to start is logged and skipped; the others still run.
pub async fn reconcile(
    map: &SharedAirplayMap,
    entries: &[SourceEntry],
    clients: &AirplayClientStore,
    now_playing: &crate::sources::now_playing::NowPlayingStore,
) {
    // Desired = AirPlay entries with a non-blank label. A blank label is the
    // "knobs configured but disabled" state (matches the singular era, where an
    // empty name meant "off"); such an entry must not run a receiver.
    let desired: BTreeMap<SourceId, (SourceEntry, AirplaySourceConfig)> = entries
        .iter()
        .filter_map(|e| match &e.config {
            SourceConfig::Airplay(a) if !e.label.trim().is_empty() => Some((e.id.clone(), (e.clone(), a.clone()))),
            _ => None,
        })
        .collect();

    let mut guard = map.lock().await;

    // Stop receivers that are gone or need a restart; live-sync the policy flag
    // on the ones that stay.
    let mut to_stop: Vec<SourceId> = Vec::new();
    for (id, handle) in guard.iter() {
        match desired.get(id) {
            None => to_stop.push(id.clone()),
            Some((entry, cfg)) => {
                if handle.needs_restart(entry) {
                    to_stop.push(id.clone());
                } else {
                    handle.set_prevent_takeover(cfg.prevent_takeover);
                }
            }
        }
    }
    for id in to_stop {
        if let Some(handle) = guard.remove(&id) {
            tracing::info!("AirPlay reconcile: stopping receiver '{id}'");
            handle.stop().await;
        }
    }

    // Start receivers that are configured but not (or no longer) running.
    for (id, (entry, cfg)) in &desired {
        if guard.contains_key(id) {
            continue;
        }
        let node_name = entry.node_name();
        let registry = clients.registry(id);
        let prevent_takeover: SharedPreventTakeover = Arc::new(AtomicBool::new(cfg.prevent_takeover));
        let reporter = now_playing.reporter(node_name.clone());
        match start(node_name, entry.label.clone(), cfg.port, cfg.latency_msec, cfg.auth_setup, registry, prevent_takeover, reporter).await
        {
            Ok(handle) => {
                guard.insert(id.clone(), handle);
                tracing::info!("AirPlay reconcile: started receiver '{id}' ('{}')", entry.label);
            }
            Err(e) => tracing::warn!("AirPlay reconcile: failed to start receiver '{id}' ('{}'): {e}", entry.label),
        }
    }
}

/// The uppercase-hex MAC (no separators) shairplay puts before `@` in its mDNS
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
    ring_prod: RingProducer,
    /// Session-end / FLUSH signal to the RT consumer (drain + re-arm).
    flush: Arc<AtomicBool>,
    peak: Arc<AtomicU32>,
    /// Persistent registry of senders — updated on connect/name/disconnect for
    /// the Sources-tab connection list (sources/airplay_clients.rs).
    clients: SharedAirplayClients,
    /// Live anti-takeover policy, consulted in `authorize_session`.
    prevent_takeover: SharedPreventTakeover,
    /// Now-playing sink for this source (sources/now_playing.rs). A sender pushes DMAP
    /// metadata, cover art and progress as three separate RTSP `SET_PARAMETER`
    /// requests, in no guaranteed order; each lands in one callback below.
    now_playing: NowPlayingReporter,
}

impl AudioHandler for Handler {
    fn audio_init(&self, format: AudioFormat) -> Box<dyn AudioSession> {
        // The producer node is fixed at RATE/CHANNELS (a stable graph identity
        // for routing). A session may negotiate a different rate/channel count
        // (raw L16/PCM senders can) — adapt it here rather than push mismatched
        // samples: channel up/down-mix to stereo, and a streaming linear
        // resampler for the rate. 44100/2 stays a zero-cost passthrough.
        let in_rate = format.sample_rate.max(1);
        let in_channels = (format.channels as usize).max(1);
        let resampler = (in_rate != RATE).then(|| Resampler::new(in_rate));
        if in_rate != RATE || in_channels != CHANNELS {
            tracing::info!("AirPlay stream {in_rate}Hz/{in_channels}ch → adapting to {RATE}Hz/{CHANNELS}ch");
        } else {
            tracing::info!("AirPlay stream started ({in_rate}Hz/{in_channels}ch)");
        }
        Box::new(Session {
            ring_prod: self.ring_prod.clone(),
            flush: self.flush.clone(),
            peak: self.peak.clone(),
            in_channels,
            resampler,
            frames: Vec::new(),
            scratch: Vec::new(),
        })
    }

    fn authorize_session(&self, addr: &str, name: Option<&str>, current: Option<&SessionInfo>) -> SessionDecision {
        let clients = &self.clients;
        if clients.is_banned(addr, name) {
            tracing::info!("AirPlay: refusing banned client {addr} (name={name:?})");
            return SessionDecision::Reject;
        }
        // Free receiver → admit.
        let Some(cur) = current else {
            return SessionDecision::Allow;
        };
        // Busy. A strictly higher priority wins outright (an explicit override of
        // the anti-takeover policy); otherwise the toggle decides: protect the
        // incumbent (Reject) or fall back to legacy last-wins (Takeover).
        let mine = clients.priority_of(addr, name);
        let theirs = clients.priority_of(&cur.addr, cur.name.as_deref());
        if mine > theirs {
            tracing::info!("AirPlay: {addr} (priority {mine}) takes over from {} (priority {theirs})", cur.addr);
            SessionDecision::Takeover
        } else if self.prevent_takeover.load(Ordering::Relaxed) {
            tracing::info!("AirPlay: refusing {addr} — {} is streaming (prevent-takeover; priority {mine} <= {theirs})", cur.addr);
            SessionDecision::Reject
        } else {
            SessionDecision::Takeover
        }
    }
    fn on_client_connected(&self, addr: &str) {
        tracing::info!("AirPlay client connected: {addr}");
        airplay_clients::on_connected(&self.clients, addr);
    }
    fn on_client_named(&self, addr: &str, name: &str) {
        tracing::info!("AirPlay client {addr} identified as '{name}'");
        airplay_clients::on_named(&self.clients, addr, name);
    }
    fn on_client_disconnected(&self, addr: &str) {
        tracing::info!("AirPlay client disconnected: {addr}");
        airplay_clients::on_disconnected(&self.clients, addr);
        // Signal the RT consumer to drop any residual buffered audio so a new
        // session starts clean (it drains + re-arms a cold prebuffer).
        self.flush.store(true, Ordering::Relaxed);
        // The session is over, so the track is not playing any more. Clearing
        // rather than leaving it to the TTL is what stops Home Assistant showing
        // last night's song (sources/now_playing.rs).
        self.now_playing.clear();
    }

    // --- Now-playing (sources/now_playing.rs) ---
    //
    // The library parses DMAP for us and offers these as no-ops; this source is
    // the richest metadata producer in the system precisely because a sender
    // volunteers all three.

    fn on_metadata(&self, metadata: &shairplay::TrackMetadata) {
        tracing::debug!("AirPlay metadata: title={:?} artist={:?} album={:?}", metadata.title, metadata.artist, metadata.album);
        self.now_playing.update(MetadataUpdate {
            state: Some(PlaybackState::Playing),
            title: metadata.title.clone(),
            artist: metadata.artist.clone(),
            album: metadata.album.clone(),
            duration_ms: metadata.duration_ms,
            ..Default::default()
        });
    }

    fn on_coverart(&self, coverart: &[u8]) {
        tracing::debug!("AirPlay cover art: {} bytes", coverart.len());
        self.now_playing.set_artwork(coverart.to_vec());
    }

    fn on_progress(&self, start: u32, current: u32, end: u32) {
        // RTP timestamps at 44100 Hz, and `start` is the track's own origin
        // rather than zero — so position is measured from it. Wrapping subtraction
        // keeps a session that has run past u32 from reporting nonsense.
        let elapsed = current.wrapping_sub(start);
        let total = end.wrapping_sub(start);
        let ms = |ts: u32| (u64::from(ts) * 1000 / u64::from(RATE)) as u32;
        // A sender reports progress on every update; the reporter throttles the
        // publish to keep this off the routing socket's hot path.
        self.now_playing.publish_position(ms(elapsed), (total > 0).then(|| ms(total)));
    }
}

struct Session {
    ring_prod: RingProducer,
    /// Session-end / FLUSH signal to the RT consumer (drain + re-arm).
    flush: Arc<AtomicBool>,
    peak: Arc<AtomicU32>,
    /// Channels the sender negotiated (adapted to CHANNELS before the ring).
    in_channels: usize,
    /// `Some` when the session rate differs from RATE (streaming resample);
    /// `None` = same rate, no resampling.
    resampler: Option<Resampler>,
    /// Scratch buffers for the off-rate/mix path, reused across calls (the fast
    /// path pushes `samples` directly and never touches these): `frames` holds
    /// the channel-adapted stereo frames, `scratch` the interleaved output.
    frames: Vec<[f32; 2]>,
    scratch: Vec<f32>,
}

impl Session {
    /// Push interleaved f32 samples into the SPSC ring. Locks only the producer
    /// side (never the RT consumer); on overflow (a stalled consumer) the excess
    /// is dropped rather than growing memory — logged, throttled.
    fn push(&self, samples: &[f32]) {
        let mut prod = self.ring_prod.lock_recover();
        let mut pushed = 0usize;
        for &s in samples {
            if prod.push(s).is_err() {
                break; // ring full — drop the remainder this chunk
            }
            pushed += 1;
        }
        let dropped = samples.len() - pushed;
        if dropped > 0 {
            let total = RING_OVERFLOW.fetch_add(dropped as u64, Ordering::Relaxed) + dropped as u64;
            // ~every 0.5 s of dropped audio, so a persistent stall is visible
            // without per-chunk spam.
            if total % (RATE as u64 * CHANNELS as u64 / 2) < dropped as u64 {
                tracing::debug!("AirPlay ingest ring overflow: dropped {dropped} samples (total {total})");
            }
        }
    }
}

impl AudioSession for Session {
    fn audio_process(&mut self, samples: &[f32]) {
        // Inline peak for the meter (cheap; this is the received signal).
        let chunk_peak = samples.iter().fold(0.0f32, |m, &s| m.max(s.abs()));
        // Rise instantly, so transients show; the consumer side decays it.
        let cur = f32::from_bits(self.peak.load(Ordering::Relaxed));
        self.peak.store(chunk_peak.max(cur).to_bits(), Ordering::Relaxed);

        if self.resampler.is_none() && self.in_channels == CHANNELS {
            // Fast path: already RATE/CHANNELS — push through unchanged.
            self.push(samples);
        } else {
            // Off-rate / non-stereo path: reuse `frames` + `scratch` (disjoint
            // fields, so the resampler can borrow `frames` while filling
            // `scratch`); no per-chunk allocation.
            to_stereo_frames_into(samples, self.in_channels, &mut self.frames);
            self.scratch.clear();
            match &mut self.resampler {
                Some(rs) => rs.process(&self.frames, &mut self.scratch),
                None => {
                    for f in &self.frames {
                        self.scratch.push(f[0]);
                        self.scratch.push(f[1]);
                    }
                }
            }
            self.push(&self.scratch);
        }
    }

    fn audio_flush(&mut self) {
        // Tell the RT consumer to drop buffered-but-unplayed audio (RAOP FLUSH).
        self.flush.store(true, Ordering::Relaxed);
        if let Some(rs) = &mut self.resampler {
            rs.reset();
        }
    }
}

/// Interleaved input samples (`in_channels`-wide) → stereo frames, appended to
/// `out` (cleared first). Mono is duplicated to both channels; >2 channels take
/// the first two (front L/R). Writes into a caller-owned buffer so the off-rate
/// ingest path reuses it instead of allocating a `Vec` per chunk.
fn to_stereo_frames_into(samples: &[f32], in_channels: usize, out: &mut Vec<[f32; 2]>) {
    out.clear();
    match in_channels {
        1 => out.extend(samples.iter().map(|&s| [s, s])),
        ch => out.extend(samples.chunks_exact(ch).map(|f| [f[0], f[1]])),
    }
}

/// Allocating convenience wrapper over [`to_stereo_frames_into`], used by tests.
#[cfg(test)]
fn to_stereo_frames(samples: &[f32], in_channels: usize) -> Vec<[f32; 2]> {
    let mut out = Vec::new();
    to_stereo_frames_into(samples, in_channels, &mut out);
    out
}

/// Streaming linear-interpolation resampler, stereo, from a source rate to
/// RATE. Carries the last input frame across calls so interpolation is
/// continuous over `audio_process` chunk boundaries. Linear (not sinc) is a
/// deliberate trade-off: the common case is 44100/2 (this resampler is never
/// constructed then), so it only runs for the rare off-rate sender, where
/// "correct pitch, modest quality" beats the previous wrong-pitch behavior.
struct Resampler {
    /// Source frames consumed per output frame (`in_rate / RATE`).
    ratio: f64,
    /// Fractional read position, measured from `prev` (virtual index 0).
    pos: f64,
    /// Last input frame of the previous chunk (virtual index 0 next call).
    prev: [f32; 2],
    have_prev: bool,
}

impl Resampler {
    fn new(in_rate: u32) -> Self {
        Self { ratio: in_rate as f64 / RATE as f64, pos: 0.0, prev: [0.0; 2], have_prev: false }
    }

    fn reset(&mut self) {
        self.pos = 0.0;
        self.have_prev = false;
    }

    /// Resample `input` stereo frames into `out` as interleaved f32 at RATE.
    fn process(&mut self, input: &[[f32; 2]], out: &mut Vec<f32>) {
        if input.is_empty() {
            return;
        }
        let (prev, have_prev) = (self.prev, self.have_prev);
        // Virtual buffer: [prev?, input...]; index 0 is `prev` when carried.
        let get = |idx: usize| -> [f32; 2] {
            if have_prev {
                if idx == 0 {
                    prev
                } else {
                    input[idx - 1]
                }
            } else {
                input[idx]
            }
        };
        let vlen = input.len() + usize::from(have_prev);
        if vlen >= 2 {
            // Emit output frames while both interpolation neighbors exist.
            let last = (vlen - 1) as f64;
            let mut pos = self.pos;
            while pos < last {
                let i = pos.floor() as usize;
                let frac = (pos - i as f64) as f32;
                let a = get(i);
                let b = get(i + 1);
                out.push(a[0] + (b[0] - a[0]) * frac);
                out.push(a[1] + (b[1] - a[1]) * frac);
                pos += self.ratio;
            }
            // Carry the leftover, now measured from the new `prev`.
            self.pos = pos - last;
        }
        self.prev = *input.last().unwrap();
        self.have_prev = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn to_stereo_frames_adapts_channel_counts() {
        // Mono → duplicated to both channels.
        assert_eq!(to_stereo_frames(&[0.5, -0.5], 1), vec![[0.5, 0.5], [-0.5, -0.5]]);
        // Stereo → unchanged pairs.
        assert_eq!(to_stereo_frames(&[0.1, 0.2, 0.3, 0.4], 2), vec![[0.1, 0.2], [0.3, 0.4]]);
        // >2 channels → first two (front L/R). Two 6-channel frames = 12 samples.
        let two_6ch: Vec<f32> = (1..=12).map(|n| n as f32).collect();
        assert_eq!(to_stereo_frames(&two_6ch, 6), vec![[1.0, 2.0], [7.0, 8.0]]);
    }

    #[test]
    fn resampler_downsamples_48k_to_44100_at_the_right_rate() {
        // 48000 → 44100: expect ~44100/48000 output frames, streamed in chunks,
        // and continuous across chunk boundaries (no per-chunk reset).
        let mut rs = Resampler::new(48_000);
        let mut out = Vec::new();
        // One second of input as 100 chunks of 480 stereo frames = 48000 frames.
        let chunk: Vec<[f32; 2]> = (0..480).map(|i| [i as f32, 0.0]).collect();
        let _ = chunk; // shape only; use a ramp below for continuity
        let mut frame = 0i64;
        for _ in 0..100 {
            let block: Vec<[f32; 2]> = (0..480)
                .map(|_| {
                    frame += 1;
                    [frame as f32, -(frame as f32)]
                })
                .collect();
            rs.process(&block, &mut out);
        }
        let out_frames = out.len() / 2;
        let expected = 48_000f64 * (RATE as f64 / 48_000f64); // = 44100
                                                              // Within a couple of frames of the ideal ratio (boundary rounding).
        assert!((out_frames as f64 - expected).abs() <= 2.0, "got {out_frames} output frames, expected ~{expected}");
        // Right channel is the negation of the left everywhere (interpolation is
        // per-channel and the input satisfied R = -L).
        let v: Vec<f32> = out.into_iter().collect();
        for f in v.as_chunks::<2>().0 {
            assert!((f[0] + f[1]).abs() < 1e-3, "L/R not mirrored after resample");
        }
    }

    #[test]
    fn resampler_upsamples_and_stays_monotonic() {
        // 22050 → 44100 (2×): a monotonically increasing ramp must stay
        // non-decreasing through linear interpolation.
        let mut rs = Resampler::new(22_050);
        let mut out = Vec::new();
        let block: Vec<[f32; 2]> = (0..100).map(|i| [i as f32, i as f32]).collect();
        rs.process(&block, &mut out);
        let left: Vec<f32> = out.iter().step_by(2).copied().collect();
        assert!(left.len() >= 190, "2x upsample should ~double the frame count");
        assert!(left.windows(2).all(|w| w[1] >= w[0] - 1e-6), "ramp not monotonic");
    }
}
