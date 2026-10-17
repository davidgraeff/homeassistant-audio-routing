//! Spotify Connect receive source: an embedded librespot device whose decoded
//! PCM feeds a PipeWire producer node (sources/pcm_producer.rs), exactly like
//! the AirPlay receiver does.
//!
//! **No account lives here.** The device advertises itself over mDNS
//! (`_spotify-connect._tcp`); when someone on the LAN picks it in their Spotify
//! app, the app hands it *their* login (the zeroconf `addUser` flow) and the
//! device plays from that account. A second person picking it takes it over.
//! Spotify requires a Premium account for this; a free account's login is
//! refused by Spotify at session start, which surfaces as `last_error`.
//!
//! Shape: one tokio task per source runs the librespot loop (discovery → a
//! `Session` + `Spirc` per login), modelled on librespot's own binary. The
//! player decodes on its own thread into [`RingSink`], which pushes into the
//! producer's ring **with backpressure** — librespot paces playback by the sink
//! blocking, the way an ALSA write would, so the graph clock draining the ring
//! is what keeps Spotify in real time (see [`RingSink::write`] for the case
//! where nothing is draining it).
//!
//! The app's volume slider is librespot's soft mixer (applied to the samples
//! before they reach the ring), so it scales this source only, ahead of every
//! output it is routed to. Track metadata, cover art URL and position go to the
//! source's now-playing record.

use crate::sources::now_playing::{MetadataUpdate, NowPlayingReporter, NowPlayingStore, PlaybackState};
use crate::sources::pcm_producer::{samples_for_ms, spawn_producer, ProducerCmd, RingProducer, CHANNELS, RATE};
use crate::sources::{SourceConfig, SourceEntry, SourceId, SpotifySourceConfig};
use crate::util::locks::LockRecover;
use futures_util::{FutureExt, StreamExt};
use librespot_connect::{ConnectConfig, Spirc};
use librespot_core::authentication::Credentials;
use librespot_core::config::{DeviceType, SessionConfig};
use librespot_core::session::Session;
use librespot_discovery::Discovery;
use librespot_metadata::audio::{AudioItem, UniqueFields};
use librespot_playback::audio_backend::{Sink, SinkResult};
use librespot_playback::config::{Bitrate, PlayerConfig};
use librespot_playback::convert::Converter;
use librespot_playback::decoder::AudioPacket;
use librespot_playback::mixer::softmixer::SoftMixer;
use librespot_playback::mixer::{Mixer, MixerConfig};
use librespot_playback::player::{Player, PlayerEvent};
use pipewire as pw;
use serde::Serialize;
use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Default ingest ring target, ms. Larger than AirPlay's 150 because nothing
/// here is latency-critical (a pause lags by this, before the outputs' own
/// delay, which for AirPlay-2 outputs is far larger) and librespot's decoder
/// delivers in bursts.
pub const DEFAULT_SPOTIFY_LATENCY_MSEC: u32 = 250;
/// Default stream quality, kbit/s — the best Spotify Connect offers.
pub const DEFAULT_SPOTIFY_BITRATE: u16 = 320;
/// Default starting volume of a fresh session, percent.
pub const DEFAULT_SPOTIFY_INITIAL_VOLUME: u8 = 50;

/// How long [`RingSink`] waits for room before concluding that nothing is
/// draining the node. Several times the largest graph quantum, so an ordinary
/// cycle never trips it.
const SINK_STALL: Duration = Duration::from_millis(500);
/// How often a blocked sink re-checks the ring.
const SINK_POLL: Duration = Duration::from_millis(5);
/// Retry delay for a device loop that ended without being asked to (discovery
/// failed to bind at boot, or its responder died).
const RESTART_DELAY: Duration = Duration::from_secs(10);
/// A session that keeps dying is not retried forever: at most this many
/// automatic reconnects per [`RECONNECT_WINDOW`], then the device waits for
/// someone to pick it again — the same limit librespot's binary applies.
const RECONNECT_LIMIT: usize = 5;
const RECONNECT_WINDOW: Duration = Duration::from_secs(60);
/// Bound on a graceful stop, so a hung Spotify connection cannot hold up a
/// reconcile or the daemon's shutdown.
const STOP_TIMEOUT: Duration = Duration::from_secs(3);

/// What the Sources tab shows about a running Spotify device: who is playing
/// on it, from where. Runtime-only, never persisted.
#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct SpotifyStatus {
    /// The Spotify user logged in on this device right now.
    pub user: Option<String>,
    /// The app instance controlling it ("David's iPhone"), when Spotify says.
    pub client: Option<String>,
    /// The Spotify-side volume, percent.
    pub volume: Option<u8>,
    /// Why the most recent login failed, until the next one succeeds — the place
    /// a free account's refusal shows up.
    pub last_error: Option<String>,
}

type SharedStatus = Arc<Mutex<SpotifyStatus>>;

/// Everything one running Spotify source owns. Call [`SpotifyHandle::stop`] to
/// tear it down cleanly; dropping it still stops the device task and the
/// producer, just without waiting for Spotify to hear the goodbye.
pub struct SpotifyHandle {
    /// The device name and config this source was started with — the
    /// [`reconcile`] restart diff.
    name: String,
    cfg: SpotifySourceConfig,
    status: SharedStatus,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    task: Option<tokio::task::JoinHandle<()>>,
    producer_stop: Option<pw::channel::Sender<ProducerCmd>>,
    /// Tells a sink blocked on a full ring to give up, so the player thread can
    /// exit while nothing drains the node any more.
    closing: Arc<AtomicBool>,
}

impl SpotifyHandle {
    pub fn status(&self) -> SpotifyStatus {
        self.status.lock_recover().clone()
    }

    /// Every knob is restart-significant: bitrate and normalisation are player
    /// config, the latency sizes the ring, the name is the mDNS advertisement,
    /// and the starting volume only applies to a new session anyway.
    fn needs_restart(&self, entry: &SourceEntry) -> bool {
        let SourceConfig::Spotify(new) = &entry.config else { return true };
        self.name != entry.label || &self.cfg != new
    }

    /// Log out of Spotify, unregister the mDNS advertisement and remove the
    /// producer node. Bounded by [`STOP_TIMEOUT`].
    pub async fn stop(mut self) {
        self.closing.store(true, Ordering::Relaxed);
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        if let Some(task) = self.task.take() {
            if tokio::time::timeout(STOP_TIMEOUT * 2, task).await.is_err() {
                tracing::warn!("Spotify '{}': device task did not stop in time", self.name);
            }
        }
        if let Some(tx) = self.producer_stop.take() {
            let _ = tx.send(ProducerCmd::Stop);
        }
    }
}

impl Drop for SpotifyHandle {
    fn drop(&mut self) {
        self.closing.store(true, Ordering::Relaxed);
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        if let Some(tx) = self.producer_stop.take() {
            let _ = tx.send(ProducerCmd::Stop);
        }
    }
}

/// Runtime collection of running Spotify devices, keyed by source id —
/// reconciled against the stored source list, like `SharedAirplayMap`.
pub type SharedSpotifyMap = Arc<tokio::sync::Mutex<BTreeMap<SourceId, SpotifyHandle>>>;

/// The status of every running Spotify device, by source id.
pub async fn statuses(map: &SharedSpotifyMap) -> HashMap<SourceId, SpotifyStatus> {
    map.lock().await.iter().map(|(id, h)| (id.clone(), h.status())).collect()
}

/// Reconcile the running Spotify devices against the stored source list: start
/// configured ones that are not running, stop removed ones, restart changed
/// ones. A blank label is "disabled", as for AirPlay. Best-effort per device.
pub async fn reconcile(map: &SharedSpotifyMap, entries: &[SourceEntry], now_playing: &NowPlayingStore) {
    let desired: BTreeMap<SourceId, (SourceEntry, SpotifySourceConfig)> = entries
        .iter()
        .filter_map(|e| match &e.config {
            SourceConfig::Spotify(c) if !e.label.trim().is_empty() => Some((e.id.clone(), (e.clone(), c.clone()))),
            _ => None,
        })
        .collect();

    let mut guard = map.lock().await;
    let to_stop: Vec<SourceId> = guard
        .iter()
        .filter(|(id, handle)| desired.get(*id).is_none_or(|(entry, _)| handle.needs_restart(entry)))
        .map(|(id, _)| id.clone())
        .collect();
    for id in to_stop {
        if let Some(handle) = guard.remove(&id) {
            tracing::info!("Spotify reconcile: stopping device '{id}'");
            handle.stop().await;
        }
    }

    for (id, (entry, cfg)) in &desired {
        if guard.contains_key(id) {
            continue;
        }
        let node_name = entry.node_name();
        let reporter = now_playing.reporter(node_name.clone());
        match start(id, node_name, entry.label.clone(), cfg.clone(), reporter) {
            Ok(handle) => {
                guard.insert(id.clone(), handle);
                tracing::info!("Spotify reconcile: started device '{id}' ('{}')", entry.label);
            }
            Err(e) => tracing::warn!("Spotify reconcile: failed to start device '{id}' ('{}'): {e}", entry.label),
        }
    }
}

/// Start one Spotify device: its producer node, then the task that advertises
/// it and plays whatever account logs in.
fn start(
    id: &str,
    node_name: String,
    name: String,
    cfg: SpotifySourceConfig,
    now_playing: NowPlayingReporter,
) -> anyhow::Result<SpotifyHandle> {
    tracing::info!("STACK: sources::spotify::start — starting Spotify device node={node_name:?} (name={name:?})");
    // The sink keeps the ring full (backpressure), so its capacity — not the
    // prebuffer target — is the standing latency. A little headroom over the
    // target lets the consumer arm without the two racing.
    let target = samples_for_ms(cfg.latency_msec);
    let cap = target + samples_for_ms(50);
    let (producer, consumer) = rtrb::RingBuffer::<f32>::new(cap);
    let ring: RingProducer = Arc::new(Mutex::new(producer));
    let flush = Arc::new(AtomicBool::new(false));
    let peak = Arc::new(AtomicU32::new(0));
    let producer_stop = spawn_producer("Spotify", node_name, consumer, peak, target, flush.clone())
        .map_err(|e| anyhow::anyhow!("failed to start Spotify PipeWire producer: {e}"))?;

    let closing = Arc::new(AtomicBool::new(false));
    let status: SharedStatus = Arc::default();
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
    let device = Device {
        name: name.clone(),
        device_id: device_id(id),
        cfg: cfg.clone(),
        ring,
        flush,
        closing: closing.clone(),
        now_playing,
        status: status.clone(),
    };
    let task = tokio::spawn(device.run(shutdown_rx));
    Ok(SpotifyHandle { name, cfg, status, shutdown: Some(shutdown_tx), task: Some(task), producer_stop: Some(producer_stop), closing })
}

/// The Spotify device id: 40 hex chars, derived from the source id so it stays
/// the same across restarts *and* renames (Spotify apps remember devices by it).
fn device_id(source_id: &str) -> String {
    use sha1::{Digest, Sha1};
    let digest = Sha1::digest(format!("pipewire-audio-router/spotify/{source_id}").as_bytes());
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// Spotify's volume scale (0..=u16::MAX) as a percentage, and back.
fn volume_pct(v: u16) -> u8 {
    ((u32::from(v) * 100 + u32::from(u16::MAX) / 2) / u32::from(u16::MAX)) as u8
}
fn volume_raw(pct: u8) -> u16 {
    (u32::from(pct.min(100)) * u32::from(u16::MAX) / 100) as u16
}

fn bitrate_of(kbps: u16) -> Bitrate {
    match kbps {
        96 => Bitrate::Bitrate96,
        160 => Bitrate::Bitrate160,
        _ => Bitrate::Bitrate320,
    }
}

/// Why a device loop returned.
enum Exit {
    /// The handle asked it to stop.
    Shutdown,
    /// Something it depends on died; the caller restarts it after a delay.
    Failed,
    /// It panicked. Restarted like `Failed`, but the reason stays visible as the
    /// source's `last_error`: a crash that only reached stderr once left a device
    /// that still looked healthy while every app hung on "Connecting…".
    Crashed(String),
}

/// The state one device task owns.
struct Device {
    name: String,
    device_id: String,
    cfg: SpotifySourceConfig,
    ring: RingProducer,
    flush: Arc<AtomicBool>,
    closing: Arc<AtomicBool>,
    now_playing: NowPlayingReporter,
    status: SharedStatus,
}

type SpircTask = Pin<Box<dyn Future<Output = ()> + Send>>;

impl Device {
    /// Run the device until shut down, restarting the whole librespot stack if it
    /// ends on its own (discovery could not bind yet at boot, a responder died).
    async fn run(self, mut shutdown: tokio::sync::oneshot::Receiver<()>) {
        loop {
            // A panic anywhere in the librespot stack must not end the task: that
            // leaves the node and the handle in place, so the device looks fine and
            // is gone.
            let exit = match AssertUnwindSafe(self.run_once(&mut shutdown)).catch_unwind().await {
                Ok(exit) => exit,
                Err(panic) => Exit::Crashed(panic_message(panic.as_ref())),
            };
            match exit {
                Exit::Shutdown => break,
                Exit::Failed | Exit::Crashed(_) => {
                    self.now_playing.clear();
                    let last_error = match exit {
                        Exit::Crashed(msg) => {
                            tracing::error!("Spotify '{}': the device crashed: {msg}", self.name);
                            Some(format!("internal error: {msg}"))
                        }
                        _ => None,
                    };
                    *self.status.lock_recover() = SpotifyStatus { last_error, ..SpotifyStatus::default() };
                    tokio::select! {
                        _ = &mut shutdown => break,
                        _ = tokio::time::sleep(RESTART_DELAY) => {}
                    }
                    tracing::info!("Spotify '{}': restarting the device", self.name);
                }
            }
        }
        self.now_playing.clear();
    }

    async fn run_once(&self, shutdown: &mut tokio::sync::oneshot::Receiver<()>) -> Exit {
        let session_config = SessionConfig { device_id: self.device_id.clone(), ..SessionConfig::default() };
        // Pinned to the LAN interface for the same reason the daemon's own mDNS
        // daemons are (supervisor.rs): with host networking, advertising on every
        // Docker veth amplifies multicast traffic.
        let zeroconf_ip: Vec<std::net::IpAddr> = crate::supervisor::primary_lan_ipv4().map(std::net::IpAddr::V4).into_iter().collect();
        let mut discovery = match Discovery::builder(self.device_id.clone(), session_config.client_id.clone())
            .name(self.name.clone())
            .device_type(DeviceType::Speaker)
            .zeroconf_ip(zeroconf_ip)
            .launch()
        {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!("Spotify '{}': could not start discovery: {e}", self.name);
                return Exit::Failed;
            }
        };
        tracing::info!("Spotify '{}': advertised on the LAN, waiting for an app to pick it", self.name);

        let mixer: Arc<dyn Mixer> = match SoftMixer::open(MixerConfig::default()) {
            Ok(m) => Arc::new(m),
            Err(e) => {
                tracing::warn!("Spotify '{}': could not create the mixer: {e}", self.name);
                discovery.shutdown().await;
                return Exit::Failed;
            }
        };
        let connect_config = ConnectConfig {
            name: self.name.clone(),
            device_type: DeviceType::Speaker,
            initial_volume: volume_raw(self.cfg.initial_volume),
            ..ConnectConfig::default()
        };
        let player_config =
            PlayerConfig { bitrate: bitrate_of(self.cfg.bitrate), normalisation: self.cfg.normalisation, ..PlayerConfig::default() };

        let mut session = Session::new(session_config.clone(), None);
        let sink = RingSink { ring: self.ring.clone(), closing: self.closing.clone(), stalled: false };
        let player = Player::new(player_config, session.clone(), mixer.get_soft_volume(), move || Box::new(sink) as Box<dyn Sink>);
        let mut events = player.get_player_event_channel();

        let mut spirc: Option<Spirc> = None;
        let mut spirc_task: Option<SpircTask> = None;
        // A login waiting to be connected, and the one in use (for reconnects).
        let mut pending: Option<Credentials> = None;
        let mut current: Option<Credentials> = None;
        let mut reconnects: Vec<Instant> = Vec::new();

        let exit = loop {
            tokio::select! {
                _ = &mut *shutdown => break Exit::Shutdown,
                creds = discovery.next() => match creds {
                    Some(creds) => {
                        tracing::info!("Spotify '{}': login from the LAN ({})", self.name, creds.username.as_deref().unwrap_or("unknown user"));
                        // A new login replaces whatever was playing: end that session
                        // and drop its buffered audio, so the next account does not
                        // start with the tail of the previous one's track.
                        self.end_session(&mut spirc, &mut spirc_task, &session);
                        self.flush.store(true, Ordering::Relaxed);
                        reconnects.clear();
                        pending = Some(creds);
                    }
                    None => {
                        tracing::warn!("Spotify '{}': discovery stopped", self.name);
                        break Exit::Failed;
                    }
                },
                _ = async {}, if pending.is_some() => {
                    let Some(creds) = pending.take() else { continue };
                    if session.is_invalid() {
                        session = Session::new(session_config.clone(), None);
                        player.set_session(session.clone());
                    }
                    match Spirc::new(connect_config.clone(), session.clone(), creds.clone(), player.clone(), mixer.clone()).await {
                        Ok((s, task)) => {
                            spirc = Some(s);
                            spirc_task = Some(Box::pin(task));
                            current = Some(creds);
                            self.status.lock_recover().last_error = None;
                        }
                        Err(e) => {
                            tracing::warn!("Spotify '{}': could not start a Connect session: {e}", self.name);
                            self.status.lock_recover().last_error = Some(e.to_string());
                            current = None;
                        }
                    }
                },
                _ = async { if let Some(task) = spirc_task.as_mut() { task.await } }, if spirc_task.is_some() => {
                    spirc_task = None;
                    spirc = None;
                    reconnects.retain(|t| t.elapsed() < RECONNECT_WINDOW);
                    if current.is_some() && reconnects.len() < RECONNECT_LIMIT {
                        tracing::warn!("Spotify '{}': Connect session ended unexpectedly, reconnecting", self.name);
                        reconnects.push(Instant::now());
                        if !session.is_invalid() {
                            session.shutdown();
                        }
                        pending = current.clone();
                    } else {
                        tracing::warn!("Spotify '{}': Connect session keeps ending; waiting for a new login", self.name);
                        current = None;
                        self.now_playing.clear();
                        *self.status.lock_recover() = SpotifyStatus { last_error: Some("the session kept dropping".into()), ..SpotifyStatus::default() };
                    }
                },
                ev = events.recv() => match ev {
                    Some(ev) => self.on_event(ev),
                    None => {
                        tracing::warn!("Spotify '{}': player stopped", self.name);
                        break Exit::Failed;
                    }
                },
            }
        };

        // Teardown: log out, stop advertising, then let the player thread go. The
        // sink is told to stop waiting first, so the thread is not stuck in a write.
        if let Some(s) = spirc.take() {
            let _ = s.shutdown();
        }
        if let Some(task) = spirc_task.take() {
            let _ = tokio::time::timeout(STOP_TIMEOUT, task).await;
        }
        if !session.is_invalid() {
            session.shutdown();
        }
        let _ = tokio::time::timeout(STOP_TIMEOUT, discovery.shutdown()).await;
        if matches!(exit, Exit::Shutdown) {
            self.closing.store(true, Ordering::Relaxed);
        }
        drop(mixer);
        // Dropping the last `Player` joins its thread; keep that off the runtime.
        let _ = tokio::task::spawn_blocking(move || drop(player)).await;
        exit
    }

    /// End the current Connect session, if any, without waiting for it — its task
    /// finishes the goodbye on its own while the next login proceeds.
    fn end_session(&self, spirc: &mut Option<Spirc>, spirc_task: &mut Option<SpircTask>, session: &Session) {
        if let Some(s) = spirc.take() {
            if let Err(e) = s.shutdown() {
                tracing::debug!("Spotify '{}': shutting the old session down: {e}", self.name);
            }
        }
        if let Some(task) = spirc_task.take() {
            tokio::spawn(task);
        }
        if !session.is_invalid() {
            session.shutdown();
        }
        self.now_playing.clear();
        *self.status.lock_recover() = SpotifyStatus::default();
    }

    fn on_event(&self, ev: PlayerEvent) {
        match ev {
            PlayerEvent::TrackChanged { audio_item } => self.now_playing.update(track_metadata(&audio_item)),
            PlayerEvent::Playing { position_ms, .. } => self.now_playing.update(MetadataUpdate {
                state: Some(PlaybackState::Playing),
                position_ms: Some(position_ms),
                ..Default::default()
            }),
            PlayerEvent::Paused { position_ms, .. } => self.now_playing.update(MetadataUpdate {
                state: Some(PlaybackState::Paused),
                position_ms: Some(position_ms),
                ..Default::default()
            }),
            PlayerEvent::Seeked { position_ms, .. } | PlayerEvent::PositionCorrection { position_ms, .. } => {
                self.now_playing.update(MetadataUpdate { position_ms: Some(position_ms), ..Default::default() })
            }
            PlayerEvent::Stopped { .. } => self.now_playing.set_state(PlaybackState::Stopped),
            PlayerEvent::VolumeChanged { volume } => self.status.lock_recover().volume = Some(volume_pct(volume)),
            PlayerEvent::SessionConnected { user_name, .. } => {
                tracing::info!("Spotify '{}': session for '{user_name}'", self.name);
                self.status.lock_recover().user = Some(user_name);
            }
            PlayerEvent::SessionClientChanged { client_name, .. } => {
                self.status.lock_recover().client = Some(client_name).filter(|n| !n.trim().is_empty());
            }
            PlayerEvent::SessionDisconnected { user_name, .. } => {
                tracing::info!("Spotify '{}': '{user_name}' disconnected", self.name);
                let mut st = self.status.lock_recover();
                st.user = None;
                st.client = None;
                drop(st);
                self.now_playing.clear();
            }
            _ => {}
        }
    }
}

/// The text of a caught panic, for the log and `last_error`.
fn panic_message(panic: &(dyn std::any::Any + Send)) -> String {
    panic
        .downcast_ref::<&str>()
        .map(|s| s.to_string())
        .or_else(|| panic.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "unknown panic".to_string())
}

/// Title, artist(s), album, duration and the largest cover of a track (or the
/// show of a podcast episode).
fn track_metadata(item: &AudioItem) -> MetadataUpdate {
    let (artist, album) = match &item.unique_fields {
        UniqueFields::Track { artists, album, .. } => {
            let names: Vec<&str> = artists.iter().map(|a| a.name.as_str()).collect();
            (Some(names.join(", ")), Some(album.clone()))
        }
        UniqueFields::Episode { show_name, .. } => (Some(show_name.clone()), None),
        UniqueFields::Local { artists, album, .. } => (artists.clone(), album.clone()),
    };
    MetadataUpdate {
        title: Some(item.name.clone()),
        artist,
        album,
        duration_ms: Some(item.duration_ms),
        artwork_url: item.covers.iter().max_by_key(|c| c.width).map(|c| c.url.clone()),
        ..Default::default()
    }
}

/// librespot's audio sink: decoded samples into the producer's ring.
///
/// librespot paces playback by the sink blocking, so `write` waits for room
/// rather than dropping — the graph draining the ring at its clock is what keeps
/// the track in real time. But a producer node that is routed nowhere is not
/// scheduled by PipeWire at all, and then nothing drains it: waiting would
/// freeze the track (and the app's progress bar) until someone routes it. So
/// after [`SINK_STALL`] without progress the sink switches to pacing by the wall
/// clock — sleeping out each packet's duration and discarding it — until the
/// ring has room again.
struct RingSink {
    ring: RingProducer,
    closing: Arc<AtomicBool>,
    stalled: bool,
}

impl RingSink {
    fn push_paced(&mut self, mut samples: &[f32]) {
        let mut waited = Duration::ZERO;
        while !samples.is_empty() {
            if self.closing.load(Ordering::Relaxed) {
                return;
            }
            let pushed = {
                let mut prod = self.ring.lock_recover();
                let n = prod.slots().min(samples.len());
                for &s in &samples[..n] {
                    // Cannot fail: `n` is bounded by the free slots, and this is
                    // the only producer while the lock is held.
                    let _ = prod.push(s);
                }
                n
            };
            if pushed > 0 {
                samples = &samples[pushed..];
                waited = Duration::ZERO;
                if self.stalled {
                    self.stalled = false;
                    tracing::debug!("Spotify sink: the graph is pulling again");
                }
                continue;
            }
            if self.stalled || waited >= SINK_STALL {
                if !self.stalled {
                    self.stalled = true;
                    tracing::debug!("Spotify sink: nothing is draining the node; pacing by the wall clock");
                }
                std::thread::sleep(duration_of(samples.len()));
                return;
            }
            std::thread::sleep(SINK_POLL);
            waited += SINK_POLL;
        }
    }
}

/// Playback time of `samples` interleaved samples at the producer format.
fn duration_of(samples: usize) -> Duration {
    Duration::from_micros((samples / CHANNELS) as u64 * 1_000_000 / u64::from(RATE))
}

impl Sink for RingSink {
    fn write(&mut self, packet: AudioPacket, converter: &mut Converter) -> SinkResult<()> {
        // `Raw` only exists with the passthrough decoder, which is never enabled.
        if let AudioPacket::Samples(samples) = packet {
            let samples = converter.f64_to_f32(&samples);
            self.push_paced(&samples);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sink(cap: usize) -> (RingSink, rtrb::Consumer<f32>) {
        let (p, c) = rtrb::RingBuffer::<f32>::new(cap);
        (RingSink { ring: Arc::new(Mutex::new(p)), closing: Arc::new(AtomicBool::new(false)), stalled: false }, c)
    }

    #[test]
    fn the_device_id_is_stable_hex_and_ignores_the_label() {
        let id = device_id("wohnzimmer");
        assert_eq!(id.len(), 40);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(id, device_id("wohnzimmer"));
        assert_ne!(id, device_id("kueche"));
    }

    #[test]
    fn volume_round_trips_through_spotifys_scale() {
        assert_eq!(volume_raw(100), u16::MAX);
        assert_eq!(volume_raw(0), 0);
        for pct in [0u8, 1, 33, 50, 99, 100] {
            assert_eq!(volume_pct(volume_raw(pct)), pct);
        }
    }

    #[test]
    fn a_sink_with_room_pushes_everything() {
        let (mut s, mut c) = sink(16);
        s.push_paced(&[0.5; 8]);
        assert_eq!(c.slots(), 8);
        assert!((0..8).all(|_| c.pop() == Ok(0.5)));
        assert!(!s.stalled);
    }

    #[test]
    fn an_undrained_sink_paces_by_the_clock_instead_of_blocking_forever() {
        let (mut s, c) = sink(4);
        let t = Instant::now();
        // Fills the ring, then waits out the stall budget and gives up.
        s.push_paced(&[0.1; 8]);
        assert!(s.stalled);
        assert!(t.elapsed() >= SINK_STALL);
        assert_eq!(c.slots(), 4);
        // Once stalled, a packet costs its own duration, not the stall budget again.
        let packet = vec![0.0; 2 * 441]; // 10 ms at 44.1 kHz stereo
        let t = Instant::now();
        s.push_paced(&packet);
        assert!(t.elapsed() < SINK_STALL / 2, "stalled sink re-waited the stall budget");
    }

    #[test]
    fn a_closing_sink_returns_at_once() {
        let (mut s, _c) = sink(2);
        s.closing.store(true, Ordering::Relaxed);
        let t = Instant::now();
        s.push_paced(&[0.0; 1000]);
        assert!(t.elapsed() < SINK_POLL * 4);
    }

    #[test]
    fn a_panic_message_is_readable_whatever_its_payload() {
        let caught = std::panic::catch_unwind(|| panic!("no provider {}", 1)).unwrap_err();
        assert_eq!(panic_message(caught.as_ref()), "no provider 1");
        let caught = std::panic::catch_unwind(|| panic!("static")).unwrap_err();
        assert_eq!(panic_message(caught.as_ref()), "static");
    }

    #[test]
    fn duration_of_counts_frames_not_samples() {
        assert_eq!(duration_of(2 * 44_100), Duration::from_secs(1));
    }
}
