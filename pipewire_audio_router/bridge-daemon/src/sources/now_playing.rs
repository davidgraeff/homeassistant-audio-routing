//! Per-source **now-playing metadata** — what each input is currently playing.
//!
//! See `docs/source-metadata-plan.md`. Every input the router carries already
//! knows what it is playing and none of it reached Home Assistant: an AirPlay
//! sender pushes DMAP metadata, cover art and progress over RTSP
//! (`SET_PARAMETER`), a phone on the Bluetooth bridge exposes AVRCP metadata on
//! the Pi's D-Bus, and the YouTube Music receiver resolves a track it can
//! describe. This module is the one place all of that lands.
//!
//! ## Deliberately source-agnostic
//! Producers are *local* (the AirPlay receiver's handler calls straight in) or
//! *remote* (a Pi reporter posts to `/api/now_playing/*`, api/now_playing.rs). Nothing
//! here knows which — a source is a node name and a bag of optional fields, so no
//! producer's protocol leaks into the model, the API, or Home Assistant.
//!
//! ## Two rules that keep it honest
//! * **Never on an audio thread.** Every write happens on a tokio/API task or a
//!   shairplay RTSP callback thread — never on the RT producer/relay path.
//! * **Entries expire and are cleared.** A [`TTL`] plus an explicit
//!   [`NowPlayingReporter::clear`] on session end. Without both, HA shows last
//!   night's song forever (the ESP32 bridge blanked its text sensors on
//!   disconnect for exactly this reason).

use crate::pw::thread::ChangeNotifier;
use crate::util::locks::LockRecover;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

/// How long an un-refreshed entry stays visible before it is treated as gone.
///
/// Only a *backstop* for a producer that dies without saying goodbye — the
/// normal path is an explicit `clear()`. Long enough that a paused sender which
/// stops sending updates does not blink out mid-pause.
pub const TTL: Duration = Duration::from_secs(90);

/// Playback state, as coarse as every producer can actually report.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlaybackState {
    Playing,
    Paused,
    Stopped,
}

/// Where the cover art for an entry comes from.
///
/// Two cases because the producers genuinely differ: YouTube Music yields a
/// public URL derived from the video id (no fetch, no API call), while AirPlay
/// hands us JPEG *bytes* over RTSP that only this daemon can serve. Bluetooth
/// yields neither — AVRCP cover art is the 1.6 BIP/OBEX feature and BlueZ does
/// not expose it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Artwork {
    /// An absolute URL the *consumer* fetches (Home Assistant proxies it).
    Url { url: String },
    /// Bytes held here and served over REST. `path` is the ready-made,
    /// rev-stamped path a consumer appends to the daemon's base URL — so neither
    /// Home Assistant nor the web UI has to know how the artwork route is spelled.
    /// `rev` is what busts a stale cache on a track change; the bytes themselves
    /// never go on the wire in a listing.
    Embedded { rev: u32, mime: String, len: usize, path: String },
}

/// One source's now-playing state, as published.
///
/// Every descriptive field is optional and independently settable, because
/// producers arrive in pieces: shairplay delivers DMAP metadata, cover art and
/// progress in three separate RTSP requests, in no guaranteed order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct NowPlaying {
    pub state: PlaybackState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub artist: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub album: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u32>,
    /// Playback position, with the wall-clock instant it was true.
    ///
    /// Sent sparingly on purpose (see `publish_position`): Home Assistant
    /// extrapolates from `position_updated_at`, so a faster cadence buys
    /// nothing. The figure leads the sound by the ingest jitter buffer plus the
    /// output's playout latency; that drift is accepted, not corrected.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub position_ms: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub position_updated_at: Option<UnixMillis>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub artwork: Option<Artwork>,
}

/// Wall-clock milliseconds since the Unix epoch — the only timestamp shape a
/// JSON consumer can act on (`Instant` is process-local, and HA needs a real
/// datetime for `media_position_updated_at`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct UnixMillis(pub u64);

impl UnixMillis {
    pub fn now() -> Self {
        Self(SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0))
    }
}

/// A stored entry: the published state plus bookkeeping that stays here.
#[derive(Debug, Clone)]
struct Entry {
    published: NowPlaying,
    /// Cover-art bytes for an [`Artwork::Embedded`], served over REST. One image
    /// per source, replaced on change — never accumulated, never written to disk.
    art_bytes: Option<Arc<Vec<u8>>>,
    /// For the [`TTL`] sweep. Not serialized: a consumer that cares about
    /// staleness gets the absence of an entry, not a timestamp to reason about.
    touched: Instant,
}

impl Entry {
    fn stopped() -> Self {
        Self {
            published: NowPlaying {
                state: PlaybackState::Stopped,
                title: None,
                artist: None,
                album: None,
                duration_ms: None,
                position_ms: None,
                position_updated_at: None,
                artwork: None,
            },
            art_bytes: None,
            touched: Instant::now(),
        }
    }
}

/// The descriptive fields a producer can set in one go, all optional.
///
/// `None` means "I have nothing new to say about this field", *not* "clear it" —
/// which is what lets shairplay's three separate callbacks each update their own
/// part without clobbering the others. Use [`NowPlayingReporter::clear`] to drop
/// a source's metadata entirely.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct MetadataUpdate {
    pub state: Option<PlaybackState>,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub duration_ms: Option<u32>,
    pub position_ms: Option<u32>,
    /// An absolute artwork URL. Ignored when this source already has embedded
    /// bytes for the *same* track — a producer that can do both should not
    /// downgrade itself.
    pub artwork_url: Option<String>,
}

impl MetadataUpdate {
    /// True when nothing at all was supplied — a request worth rejecting rather
    /// than silently touching the TTL.
    pub fn is_empty(&self) -> bool {
        self.state.is_none()
            && self.title.is_none()
            && self.artist.is_none()
            && self.album.is_none()
            && self.duration_ms.is_none()
            && self.position_ms.is_none()
            && self.artwork_url.is_none()
    }

    /// Trim strings and treat blank as absent, so `""` from a producer clears
    /// nothing and displays nothing.
    fn normalized(mut self) -> Self {
        fn tidy(v: &mut Option<String>) {
            if let Some(s) = v {
                let t = s.trim();
                if t.is_empty() {
                    *v = None;
                } else if t.len() != s.len() {
                    *v = Some(t.to_string());
                }
            }
        }
        tidy(&mut self.title);
        tidy(&mut self.artist);
        tidy(&mut self.album);
        tidy(&mut self.artwork_url);
        self
    }
}

/// All sources' now-playing state, behind one mutex. Held by `AppState`; cheap to
/// clone (an `Arc`).
#[derive(Clone)]
pub struct NowPlayingStore {
    inner: Arc<Mutex<BTreeMap<String, Entry>>>,
    changes: ChangeNotifier,
}

impl NowPlayingStore {
    pub fn new(changes: ChangeNotifier) -> Self {
        Self { inner: Arc::new(Mutex::new(BTreeMap::new())), changes }
    }

    /// A handle scoped to one source's node name, for a producer to write
    /// through. Cheap to clone and safe to hold for a receiver's lifetime.
    pub fn reporter(&self, node_name: impl Into<String>) -> NowPlayingReporter {
        NowPlayingReporter { store: self.clone(), node_name: node_name.into(), art_rev: Arc::new(Mutex::new(0)) }
    }

    /// Everything currently known, TTL-expired entries dropped, keyed by source
    /// node name. This is what the WebSocket frame and the REST reads serialize.
    pub fn snapshot(&self) -> BTreeMap<String, NowPlaying> {
        let now = Instant::now();
        let mut map = self.inner.lock_recover();
        map.retain(|_, e| now.duration_since(e.touched) < TTL);
        map.iter().map(|(k, v)| (k.clone(), v.published.clone())).collect()
    }

    /// One source's state, or `None` when nothing is known (or it expired).
    pub fn get(&self, node_name: &str) -> Option<NowPlaying> {
        let map = self.inner.lock_recover();
        let e = map.get(node_name)?;
        (Instant::now().duration_since(e.touched) < TTL).then(|| e.published.clone())
    }

    /// Cover-art bytes for a source, with their mime type and revision — for the
    /// artwork endpoint. `None` when this source has no embedded art.
    pub fn artwork(&self, node_name: &str) -> Option<(Arc<Vec<u8>>, String, u32)> {
        let map = self.inner.lock_recover();
        let e = map.get(node_name)?;
        let bytes = e.art_bytes.clone()?;
        match &e.published.artwork {
            Some(Artwork::Embedded { rev, mime, .. }) => Some((bytes, mime.clone(), *rev)),
            _ => None,
        }
    }

    /// Drop everything for sources that no longer exist. Called after a source is
    /// deleted or renamed away, so a removed source cannot leave a ghost track
    /// behind that the TTL would otherwise keep alive for a minute and a half.
    pub fn retain_sources(&self, live: &[String]) {
        let mut changed = false;
        {
            let mut map = self.inner.lock_recover();
            let before = map.len();
            map.retain(|k, _| live.iter().any(|n| n == k));
            changed = changed || map.len() != before;
        }
        if changed {
            let _ = self.changes.send(());
        }
    }
}

/// One source's write handle. All mutation goes through here, so the change
/// notification and the TTL touch can't be forgotten at a call site.
#[derive(Clone)]
pub struct NowPlayingReporter {
    store: NowPlayingStore,
    node_name: String,
    /// Monotonic artwork revision for this source, so a consumer's cached image
    /// URL changes on every new picture.
    art_rev: Arc<Mutex<u32>>,
}

impl NowPlayingReporter {
    pub fn node_name(&self) -> &str {
        &self.node_name
    }

    /// Merge descriptive fields. A supplied field wins; an absent one is left
    /// alone. Creates the entry (as `Playing`, since a producer only describes a
    /// track it is actually playing) when there was none.
    pub fn update(&self, update: MetadataUpdate) {
        let update = update.normalized();
        let mut map = self.store.inner.lock_recover();
        let entry = map.entry(self.node_name.clone()).or_insert_with(|| {
            let mut e = Entry::stopped();
            e.published.state = PlaybackState::Playing;
            e
        });
        let p = &mut entry.published;
        // A new *title* means a new track, so anything the previous track left
        // behind must go rather than linger under the new name. Cover art is the
        // one that shows: a producer sending only a title would otherwise keep
        // the old album's picture.
        if let Some(title) = &update.title {
            if p.title.as_deref() != Some(title.as_str()) {
                p.album = None;
                p.artist = None;
                p.duration_ms = None;
                p.position_ms = None;
                p.position_updated_at = None;
                p.artwork = None;
                entry.art_bytes = None;
            }
        }
        if let Some(state) = update.state {
            p.state = state;
        }
        if update.title.is_some() {
            p.title = update.title;
        }
        if update.artist.is_some() {
            p.artist = update.artist;
        }
        if update.album.is_some() {
            p.album = update.album;
        }
        if update.duration_ms.is_some() {
            p.duration_ms = update.duration_ms;
        }
        if let Some(pos) = update.position_ms {
            p.position_ms = Some(pos);
            p.position_updated_at = Some(UnixMillis::now());
        }
        if let Some(url) = update.artwork_url {
            // Embedded bytes for the current track outrank a URL.
            if !matches!(p.artwork, Some(Artwork::Embedded { .. })) {
                p.artwork = Some(Artwork::Url { url });
            }
        }
        entry.touched = Instant::now();
        drop(map);
        let _ = self.store.changes.send(());
    }

    /// Replace this source's cover art with `bytes`, bumping the revision.
    ///
    /// Sniffs the container rather than trusting a declared type: shairplay hands
    /// over whatever the sender attached, and the mime type ends up in a `GET`
    /// response header.
    pub fn set_artwork(&self, bytes: Vec<u8>) {
        if bytes.is_empty() {
            return;
        }
        let mime = sniff_image_mime(&bytes);
        let rev = {
            let mut r = self.art_rev.lock_recover();
            *r = r.wrapping_add(1);
            *r
        };
        let len = bytes.len();
        let path = artwork_path(&self.node_name, rev);
        let mut map = self.store.inner.lock_recover();
        let entry = map.entry(self.node_name.clone()).or_insert_with(Entry::stopped);
        entry.published.artwork = Some(Artwork::Embedded { rev, mime: mime.to_string(), len, path });
        entry.art_bytes = Some(Arc::new(bytes));
        entry.touched = Instant::now();
        drop(map);
        let _ = self.store.changes.send(());
    }

    /// Report a position, rate-limited to one publish per
    /// [`POSITION_PUBLISH_INTERVAL`].
    ///
    /// Progress arrives far faster than it is worth publishing (shairplay reports
    /// on every sender update), and the routing socket is shared with the UI. HA
    /// extrapolates between updates from `position_updated_at`, so throttling
    /// costs nothing visible.
    pub fn publish_position(&self, position_ms: u32, duration_ms: Option<u32>) {
        {
            let map = self.store.inner.lock_recover();
            if let Some(e) = map.get(&self.node_name) {
                if let Some(at) = e.published.position_updated_at {
                    let age = UnixMillis::now().0.saturating_sub(at.0);
                    if age < POSITION_PUBLISH_INTERVAL.as_millis() as u64 {
                        return;
                    }
                }
            }
        }
        self.update(MetadataUpdate { position_ms: Some(position_ms), duration_ms, ..Default::default() });
    }

    /// Note that this source is playing (or paused/stopped) without touching any
    /// descriptive field.
    pub fn set_state(&self, state: PlaybackState) {
        let mut map = self.store.inner.lock_recover();
        let Some(entry) = map.get_mut(&self.node_name) else {
            // Nothing described yet: only a *stop* is worth recording, and
            // recording it as an empty entry would just be a TTL-limited ghost.
            return;
        };
        if entry.published.state == state {
            entry.touched = Instant::now();
            return;
        }
        entry.published.state = state;
        entry.touched = Instant::now();
        drop(map);
        let _ = self.store.changes.send(());
    }

    /// Forget everything for this source — the session ended.
    pub fn clear(&self) {
        let removed = self.store.inner.lock_recover().remove(&self.node_name).is_some();
        if removed {
            let _ = self.store.changes.send(());
        }
    }
}

/// The path a consumer fetches embedded cover art from — kept next to the model
/// so the published `path` and the route in api/now_playing.rs cannot drift apart.
pub fn artwork_path(node_name: &str, rev: u32) -> String {
    format!("/api/now_playing/{node_name}/artwork?rev={rev}")
}

/// Minimum gap between published position updates. HA fills the gaps itself.
pub const POSITION_PUBLISH_INTERVAL: Duration = Duration::from_secs(5);

/// Identify an image by its magic bytes. Only the two formats AirPlay senders
/// actually attach are recognized; anything else is served as a generic binary
/// rather than mislabelled.
fn sniff_image_mime(bytes: &[u8]) -> &'static str {
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        "image/jpeg"
    } else if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        "image/png"
    } else {
        "application/octet-stream"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> NowPlayingStore {
        let (tx, _rx) = tokio::sync::broadcast::channel(16);
        NowPlayingStore::new(tx)
    }

    fn meta(title: &str) -> MetadataUpdate {
        MetadataUpdate { title: Some(title.to_string()), ..Default::default() }
    }

    #[test]
    fn an_update_creates_a_playing_entry() {
        let s = store();
        s.reporter("airplay-in").update(meta("Song"));
        let np = s.get("airplay-in").expect("entry");
        assert_eq!(np.state, PlaybackState::Playing);
        assert_eq!(np.title.as_deref(), Some("Song"));
    }

    #[test]
    fn absent_fields_do_not_clobber_present_ones() {
        // shairplay delivers metadata, artwork and progress in separate requests,
        // so a later partial update must not wipe an earlier one.
        let s = store();
        let r = s.reporter("airplay-in");
        r.update(MetadataUpdate {
            title: Some("Song".into()),
            artist: Some("Artist".into()),
            album: Some("Album".into()),
            ..Default::default()
        });
        r.update(MetadataUpdate { position_ms: Some(1000), ..Default::default() });
        let np = s.get("airplay-in").unwrap();
        assert_eq!(np.artist.as_deref(), Some("Artist"));
        assert_eq!(np.album.as_deref(), Some("Album"));
        assert_eq!(np.position_ms, Some(1000));
        assert!(np.position_updated_at.is_some(), "a position must carry when it was true");
    }

    #[test]
    fn a_new_title_drops_the_previous_tracks_leftovers() {
        let s = store();
        let r = s.reporter("airplay-in");
        r.update(MetadataUpdate { title: Some("A".into()), album: Some("Album A".into()), ..Default::default() });
        r.set_artwork(vec![0xFF, 0xD8, 0xFF, 0x00]);
        r.update(meta("B"));
        let np = s.get("airplay-in").unwrap();
        assert_eq!(np.title.as_deref(), Some("B"));
        assert_eq!(np.album, None, "the previous album must not survive a track change");
        assert_eq!(np.artwork, None, "nor its cover art");
        assert!(s.artwork("airplay-in").is_none());
    }

    #[test]
    fn artwork_revision_advances_so_a_cached_image_is_refetched() {
        let s = store();
        let r = s.reporter("airplay-in");
        r.update(meta("Song"));
        r.set_artwork(vec![0xFF, 0xD8, 0xFF, 1]);
        let (_, mime, rev1) = s.artwork("airplay-in").unwrap();
        assert_eq!(mime, "image/jpeg");
        r.set_artwork(vec![0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n', 2]);
        let (bytes, mime, rev2) = s.artwork("airplay-in").unwrap();
        assert_eq!(mime, "image/png");
        assert!(rev2 > rev1, "rev must advance ({rev1} -> {rev2})");
        assert_eq!(bytes.len(), 9);
    }

    #[test]
    fn a_url_never_downgrades_embedded_art() {
        let s = store();
        let r = s.reporter("airplay-in");
        r.update(meta("Song"));
        r.set_artwork(vec![0xFF, 0xD8, 0xFF, 1]);
        r.update(MetadataUpdate { artwork_url: Some("http://example/x.jpg".into()), ..Default::default() });
        assert!(matches!(s.get("airplay-in").unwrap().artwork, Some(Artwork::Embedded { .. })));
    }

    #[test]
    fn blank_strings_are_treated_as_absent() {
        let s = store();
        s.reporter("rtp-in-bt").update(MetadataUpdate { title: Some("  Song  ".into()), artist: Some("   ".into()), ..Default::default() });
        let np = s.get("rtp-in-bt").unwrap();
        assert_eq!(np.title.as_deref(), Some("Song"), "surrounding whitespace is trimmed");
        assert_eq!(np.artist, None, "a blank field is nothing, not an empty display");
    }

    #[test]
    fn clear_removes_the_entry() {
        let s = store();
        let r = s.reporter("airplay-in");
        r.update(meta("Song"));
        r.clear();
        assert!(s.get("airplay-in").is_none());
        assert!(s.snapshot().is_empty());
    }

    #[test]
    fn set_state_on_an_undescribed_source_does_not_invent_an_entry() {
        // A stop for a source nobody ever described would be a TTL-limited ghost
        // in every consumer's listing.
        let s = store();
        s.reporter("airplay-in").set_state(PlaybackState::Stopped);
        assert!(s.get("airplay-in").is_none());
    }

    #[test]
    fn set_state_flips_an_existing_entry() {
        let s = store();
        let r = s.reporter("airplay-in");
        r.update(meta("Song"));
        r.set_state(PlaybackState::Paused);
        assert_eq!(s.get("airplay-in").unwrap().state, PlaybackState::Paused);
    }

    #[test]
    fn position_publishes_are_rate_limited() {
        let s = store();
        let r = s.reporter("airplay-in");
        r.update(meta("Song"));
        r.publish_position(1000, Some(200_000));
        let first = s.get("airplay-in").unwrap();
        assert_eq!(first.position_ms, Some(1000));
        // Immediately after, a second report is dropped: HA extrapolates.
        r.publish_position(2000, Some(200_000));
        assert_eq!(s.get("airplay-in").unwrap().position_ms, Some(1000));
    }

    #[test]
    fn an_expired_entry_is_invisible_and_swept() {
        let s = store();
        s.reporter("airplay-in").update(meta("Song"));
        {
            let mut map = s.inner.lock_recover();
            let e = map.get_mut("airplay-in").unwrap();
            e.touched = Instant::now() - (TTL + Duration::from_secs(1));
        }
        assert!(s.get("airplay-in").is_none(), "past the TTL it must not be readable");
        assert!(s.snapshot().is_empty());
        assert!(s.inner.lock_recover().is_empty(), "and the snapshot sweeps it");
    }

    #[test]
    fn retain_sources_drops_a_deleted_sources_metadata() {
        let s = store();
        s.reporter("airplay-in").update(meta("A"));
        s.reporter("rtp-in-bt").update(meta("B"));
        s.retain_sources(&["airplay-in".to_string()]);
        assert!(s.get("airplay-in").is_some());
        assert!(s.get("rtp-in-bt").is_none());
    }

    #[test]
    fn an_empty_update_is_recognized() {
        assert!(MetadataUpdate::default().is_empty());
        assert!(!meta("x").is_empty());
    }
}
