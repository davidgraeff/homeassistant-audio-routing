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

use crate::airplay_clients::{self, AirplayClientStore, SharedAirplayClients};
use crate::now_playing::{MetadataUpdate, NowPlayingReporter, PlaybackState};
use crate::sources_store::{AirplaySourceConfig, SourceConfig, SourceEntry, SourceId};
use crate::util::locks::LockRecover;
use pipewire as pw;
use pw::spa;
use shairplay::{Ap1Encryption, AudioFormat, AudioHandler, AudioSession, RaopServer, SessionDecision, SessionInfo};
use spa::pod::Pod;
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

/// The producer node's fixed format. AirPlay is 44.1 kHz stereo in practice
/// (AP1 ALAC and, so far, AP2 AAC); `audio_init` warns if a session ever
/// reports otherwise so we'd notice and add resampling.
const RATE: u32 = 44_100;
const CHANNELS: usize = 2;

/// Default receiver-side jitter buffer target, in milliseconds. shairplay's
/// RaopBuffer paces packet playout, but its clock and PipeWire's graph clock
/// drift; without a cushion the producer ring underruns and you hear stutter.
/// The producer prebuffers this much before draining and re-buffers on
/// underrun. 150 ms rides out LAN jitter + drift while staying imperceptible
/// for one-way audio. Stored per install (sources_store.rs) and settable via
/// `/api/sources`, so a noisy install can trade latency for fewer
/// dropouts.
/// Node name of the old single AirPlay source. Production names sources
/// dynamically now; this survives purely as a fixture for the routing tests.
#[cfg(test)]
pub const AIRPLAY_NODE_NAME: &str = "airplay-in";

pub const DEFAULT_AIRPLAY_LATENCY_MSEC: u32 = 150;

/// After a mid-stream ring underrun, re-arm the jitter buffer to this much audio
/// before draining again — a small hysteresis guard, **not** the full cold-start
/// prebuffer (`DEFAULT_AIRPLAY_LATENCY_MSEC`). A transient late/lost packet then
/// costs ~one quantum plus this guard instead of a full ~150 ms of silence
/// re-injected at the source and fanned to every output of the group (RC2 in
/// docs/audio-jitter-analysis.md), while still avoiding a per-quantum underrun
/// train. (Chronic underfeed from producer↔graph clock drift — RC3 — needs an
/// adaptive resampler; this only bounds *transient* gaps.)
const AIRPLAY_REARM_MSEC: u32 = 40;

/// Producer half of the lock-free SPSC jitter-buffer ring (interleaved f32),
/// shared behind a `Mutex` only to serialize the *producer* side across sessions
/// (a session teardown + a new session can briefly overlap). The `Mutex` is
/// **never** touched by the consumer: the PipeWire RT process callback owns the
/// `rtrb::Consumer` and pops lock-free, so a graph cycle can never block on the
/// decode thread (the old `Mutex<VecDeque>` could → `airplay-in` xruns → stutter).
type RingProducer = Arc<Mutex<rtrb::Producer<f32>>>;

/// Cumulative count of ingest-ring samples dropped on overflow (consumer/graph
/// stalled). Overflow should be ~never in steady state; logged, throttled.
static RING_OVERFLOW: AtomicU64 = AtomicU64::new(0);

/// Interleaved-f32 samples for `msec` of audio at the producer's rate/channels.
fn samples_for_ms(msec: u32) -> usize {
    (RATE as usize) * CHANNELS * (msec as usize) / 1000
}

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
    tracing::info!("STACK: airplay_source::start — (re)starting AirPlay receiver node={node_name:?} (name={service_name:?}, port={port})");
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

    let producer_stop = spawn_producer(node_name.clone(), consumer, peak.clone(), target, flush.clone())
        .map_err(|e| anyhow::anyhow!("failed to start AirPlay PipeWire producer: {e}"))?;

    let handler =
        Arc::new(Handler { ring_prod, flush, peak: peak.clone(), clients, prevent_takeover: prevent_takeover.clone(), now_playing });
    let mut builder = RaopServer::builder().name(service_name.clone()).hwaddr(derive_hwaddr(&service_name)).port(port);
    // Advertise on the process-wide shared, LAN-restricted mDNS daemon so the
    // receiver's `_raop._tcp`/`_airplay._tcp` records share one interface-pinned
    // daemon thread with everything else — avoids the host-network multicast
    // amplification across Docker veths. Falls back to shairplay's own daemon if
    // the shared one is unavailable.
    if let Some(daemon) = crate::discovery_supervisor::shared_advertise_daemon() {
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
/// [`SourcesStore`]: crate::sources_store::SourcesStore
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
    now_playing: &crate::now_playing::NowPlayingStore,
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
    /// the Sources-tab connection list (airplay_clients.rs).
    clients: SharedAirplayClients,
    /// Live anti-takeover policy, consulted in `authorize_session`.
    prevent_takeover: SharedPreventTakeover,
    /// Now-playing sink for this source (now_playing.rs). A sender pushes DMAP
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
        // last night's song (now_playing.rs).
        self.now_playing.clear();
    }

    // --- Now-playing (now_playing.rs) ---
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

/// Spawn the PipeWire producer on a dedicated thread (mirrors
/// sendspin_capture's thread+channel+mainloop shape). Returns a stop sender.
/// `target` is the jitter-buffer prebuffer, in interleaved-f32 samples.
fn spawn_producer(
    node_name: String,
    consumer: rtrb::Consumer<f32>,
    peak: Arc<AtomicU32>,
    target: usize,
    flush: Arc<AtomicBool>,
) -> Result<pw::channel::Sender<ProducerCmd>, String> {
    let (cmd_tx, cmd_rx) = pw::channel::channel::<ProducerCmd>();
    std::thread::Builder::new()
        .name("airplay-producer".into())
        .spawn(move || {
            if let Err(e) = run_producer(node_name, consumer, peak, target, cmd_rx, flush) {
                tracing::error!("AirPlay PipeWire producer exited with error: {e}");
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
fn set_producer_realtime_priority() {
    // SAFETY: sched_setscheduler on the current thread (pid 0) with a valid,
    // zero-initialised sched_param; no aliasing, no ownership transfer.
    unsafe {
        let mut param: libc::sched_param = std::mem::zeroed();
        param.sched_priority = 45;
        if libc::sched_setscheduler(0, libc::SCHED_FIFO, &param) == 0 {
            tracing::info!("AirPlay producer: real-time priority set (SCHED_FIFO, priority 45)");
        } else {
            tracing::debug!("AirPlay producer: could not set RT priority (need CAP_SYS_NICE); running at normal priority");
        }
    }
}
#[cfg(not(target_os = "linux"))]
fn set_producer_realtime_priority() {}

fn run_producer(
    node_name: String,
    consumer: rtrb::Consumer<f32>,
    peak: Arc<AtomicU32>,
    target: usize,
    cmd_rx: pw::channel::Receiver<ProducerCmd>,
    flush: Arc<AtomicBool>,
) -> Result<(), String> {
    set_producer_realtime_priority();
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
    let rearm_target = samples_for_ms(AIRPLAY_REARM_MSEC).min(target).max(1);

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

    tracing::info!("AirPlay producer node '{node_name}' ready");
    mainloop.run();

    if let Some(e) = error.borrow_mut().take() {
        return Err(e);
    }
    Ok(())
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
