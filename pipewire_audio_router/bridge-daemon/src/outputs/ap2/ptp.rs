//! Host-global AirPlay-2 PTP grandmaster, backed by OwnTone's MIT `libairptp`
//! (vendored in `vendor/libairptp/`, compiled by `build.rs`).
//!
//! libairptp binds UDP **319/320** and speaks conformant gPTP — Sync/Follow_Up/
//! Announce, the Apple proprietary `00:0D:93` Signaling/Follow_Up TLVs, and
//! Delay_Resp — which is what makes third-party receivers (Yamaha/Pioneer/…)
//! actually lock. Because 319/320 can be bound only once, there is exactly **one**
//! [`Ap2PtpService`] per daemon: every AP2 receiver, across every group, is a PTP
//! *peer* of this one grandmaster and slaves to its clock. *Which* audio a receiver
//! plays is a per-group RTP concern handled elsewhere (the per-device AP2 senders).
//!
//! The master clock is `CLOCK_MONOTONIC`; audio PT=87 anchors must be stamped from
//! the same clock — see [`monotonic_ns`].
//!
//! Phase 0/1 of the RAOP→AP2 migration: the service is built and self-tested but
//! not yet wired to discovery/streaming (that's Phase 2+), hence `allow(dead_code)`.
#![allow(dead_code)]

use std::collections::{HashMap, HashSet};
use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int};
use std::ptr;
use std::sync::{Arc, Mutex};

/* ------------------------------- raw FFI --------------------------------- */

#[repr(C)]
struct AirptpHandle {
    _private: [u8; 0],
}

extern "C" {
    /// Installs our C shim callbacks (thread name + log routing) on the CURRENT
    /// thread's libairptp TLS. Must run just before `airptp_daemon_start` on the
    /// same thread, so the worker inherits them. See `vendor/.../airptp_shim.c`.
    fn bridge_airptp_install_callbacks();
    fn airptp_daemon_bind(node: *const c_char) -> *mut AirptpHandle;
    fn airptp_daemon_start(hdl: *mut AirptpHandle, clock_id_seed: u64, is_shared: bool) -> c_int;
    fn airptp_peer_add(peer_id: *mut u32, addr: *const c_char, hdl: *mut AirptpHandle) -> c_int;
    fn airptp_peer_remove(peer_id: u32, hdl: *mut AirptpHandle);
    fn airptp_end(hdl: *mut AirptpHandle);
    fn airptp_clock_id_get(clock_id: *mut u64, hdl: *mut AirptpHandle) -> c_int;
    fn airptp_peer_last_seen(peer_id: u32, hdl: *mut AirptpHandle) -> u64;
    fn airptp_errmsg_get() -> *const c_char;
    fn airptp_ports_override(event_port: u16, general_port: u16);
}

/// Sink for libairptp's log lines, called by the C shim (`airptp_shim.c`) from
/// libairptp's worker thread. Routes its diagnostics (bind errors, peer add/
/// remove, sync activity) into `tracing` under the `libairptp` target — without
/// this they are silently discarded (libairptp has no built-in logging).
#[no_mangle]
pub extern "C" fn bridge_airptp_log(msg: *const c_char) {
    if msg.is_null() {
        return;
    }
    // SAFETY: the shim passes a NUL-terminated buffer valid for this call.
    let s = unsafe { CStr::from_ptr(msg) }.to_string_lossy();
    tracing::info!(target: "libairptp", "{}", s.trim_end());
}

fn errmsg() -> String {
    // SAFETY: airptp_errmsg_get returns a thread-local C string or null.
    unsafe {
        let p = airptp_errmsg_get();
        if p.is_null() {
            "unknown airptp error".to_string()
        } else {
            CStr::from_ptr(p).to_string_lossy().into_owned()
        }
    }
}

#[repr(C)]
#[derive(Default)]
struct Timespec {
    tv_sec: i64,
    tv_nsec: i64,
}
extern "C" {
    fn clock_gettime(clk_id: c_int, tp: *mut Timespec) -> c_int;
}

/// Absolute `CLOCK_MONOTONIC` in nanoseconds — the exact clock libairptp
/// advertises as grandmaster. AP2 audio PT=87 anchors must use this so the
/// receiver's slaved clock and the anchor share one timeline.
pub fn monotonic_ns() -> u64 {
    let mut ts = Timespec::default();
    // SAFETY: valid timespec pointer.
    unsafe {
        clock_gettime(1 /* CLOCK_MONOTONIC */, &mut ts)
    };
    ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
}

/* ---------------------------- RAII master -------------------------------- */

/// Owns a private libairptp grandmaster daemon. Not `Clone`; the handle is a
/// raw pointer whose worker thread manages the sockets internally.
struct AirptpMaster {
    hdl: *mut AirptpHandle,
}
// Only touched behind Ap2PtpService's Mutex; libairptp owns its own thread.
unsafe impl Send for AirptpMaster {}

impl AirptpMaster {
    fn start(clock_id_seed: u64, port_override: Option<(u16, u16)>) -> Result<Self, String> {
        // SAFETY: standard libairptp start sequence; all pointers valid for the call.
        unsafe {
            // Install thread-name + log-routing callbacks on THIS thread so the
            // worker (spawned by airptp_daemon_start below) inherits them.
            bridge_airptp_install_callbacks();

            if let Some((ev, gen)) = port_override {
                airptp_ports_override(ev, gen);
            }

            let hdl = airptp_daemon_bind(ptr::null());
            if hdl.is_null() {
                return Err(format!("airptp bind failed: {}", errmsg()));
            }
            if airptp_daemon_start(hdl, clock_id_seed, false) < 0 {
                let e = errmsg();
                airptp_end(hdl);
                return Err(format!("airptp start failed: {}", e));
            }
            Ok(Self { hdl })
        }
    }

    fn clock_id(&self) -> Option<u64> {
        let mut c = 0u64;
        // SAFETY: valid handle + out-pointer.
        if unsafe { airptp_clock_id_get(&mut c, self.hdl) } == 0 {
            Some(c)
        } else {
            None
        }
    }

    /// Unix seconds of the last gPTP packet received from `peer_id` (0 = never/unknown).
    fn peer_last_seen(&self, peer_id: u32) -> u64 {
        // SAFETY: valid handle; best-effort read of the worker's peer table.
        unsafe { airptp_peer_last_seen(peer_id, self.hdl) }
    }

    fn add_peer(&self, addr: &str) -> Result<u32, String> {
        let caddr = CString::new(addr).map_err(|_| "peer addr has NUL".to_string())?;
        let mut id = 0u32;
        // SAFETY: valid handle + C string.
        let ret = unsafe { airptp_peer_add(&mut id, caddr.as_ptr(), self.hdl) };
        if ret < 0 {
            return Err(format!("airptp peer_add({addr}) failed: {}", errmsg()));
        }
        Ok(id)
    }

    fn remove_peer(&self, id: u32) {
        // SAFETY: valid handle.
        unsafe { airptp_peer_remove(id, self.hdl) };
    }
}

impl Drop for AirptpMaster {
    fn drop(&mut self) {
        // SAFETY: handle created by start(); airptp_end stops the daemon + frees it.
        unsafe { airptp_end(self.hdl) };
    }
}

/* -------------------------- Ap2PtpService -------------------------------- */

struct Inner {
    master: Option<AirptpMaster>,
    /// receiver addr → libairptp peer id, for dedup + targeted removal.
    peers: HashMap<String, u32>,
    /// receiver addr → unix seconds when we (re)registered it as a peer. Load-bearing:
    /// libairptp *seeds* `peer->last_seen` with `time(NULL)` inside `daemon_peer_add`
    /// (`vendor/libairptp/src/daemon.c`), so a raw `last_seen` read cannot tell "the
    /// receiver is answering us" from "we just added it". Everything downstream —
    /// the UI badge, and the PTP watchdog that rebuilds sessions — needs the
    /// difference, so we keep the add time and discount anything not newer than it.
    /// See [`heard_age`].
    added_at: HashMap<String, u64>,
    /// Receivers we have genuinely heard gPTP *from* since the daemon started: the
    /// two-way class (see [`Ap2PtpService::peer_ptp`]). Latched, and deliberately
    /// **not** cleared by [`Ap2PtpService::remove_peer`] — a reconnect must not erase
    /// what we know about the device, or the watchdog would re-learn its class from
    /// its own recovery attempt. Cleared only by [`Ap2PtpService::forget_peer`].
    two_way: HashSet<String>,
    clock_id: Option<u64>,
}

/// What we currently know about one receiver's gPTP behaviour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PeerPtp {
    /// Time since a gPTP message genuinely arrived *from* this receiver, or `None` if
    /// none ever has (see [`heard_age`]). Small = it is slaving to us and talking back.
    pub age: Option<std::time::Duration>,
    /// Has this receiver ever answered our grandmaster? Two classes exist on real
    /// hardware, and they need opposite treatment:
    ///
    /// * **Two-way** (e.g. Pioneer VSX-934, `PTPInfo: SUE PTP 1.0`): sends `Delay_Req`
    ///   continuously while streaming — measured at ~8/s, matching our Sync rate. For
    ///   these, a silent stretch is a real fault and worth rebuilding the session for.
    /// * **One-way** (e.g. Yamaha WX-021, `PTPInfo: AirPlay;2.0.4`): answers *nothing*,
    ///   ever — measured 0 packets in 90 s while streaming in a group whose other member
    ///   sent 717 — and renders perfectly anyway off our Sync/Follow_Up + PT=87 anchors.
    ///   For these, silence is the normal state and must never trigger anything.
    pub two_way: bool,
}

/// Time since a receiver genuinely spoke to us, given libairptp's `last_seen`, the
/// moment we registered it as a peer, and now — all in unix seconds.
///
/// `last_seen == 0` is libairptp's "never"; `last_seen <= added_at` is the seed written
/// by `daemon_peer_add` and carries no information about the receiver, so both are
/// `None`. The one-second granularity of `time(NULL)` means a receiver that answers
/// within the same second as the add is reported as unheard until the clock ticks over —
/// harmless, since a two-way receiver sends ~8 packets a second.
fn heard_age(last_seen: u64, added_at: u64, now: u64) -> Option<std::time::Duration> {
    if last_seen == 0 || last_seen <= added_at {
        return None;
    }
    Some(std::time::Duration::from_secs(now.saturating_sub(last_seen)))
}

impl Inner {
    fn empty() -> Self {
        Inner { master: None, peers: HashMap::new(), added_at: HashMap::new(), two_way: HashSet::new(), clock_id: None }
    }
}

/// The daemon's single host-global AirPlay-2 PTP grandmaster. Cheaply cloneable
/// (`Arc`); share it wherever AP2 receivers are added/removed.
pub struct Ap2PtpService {
    inner: Mutex<Inner>,
    /// Override 319/320 (tests only, to avoid privilege/conflict).
    port_override: Option<(u16, u16)>,
}

impl Ap2PtpService {
    pub fn new() -> Arc<Self> {
        Arc::new(Self { inner: Mutex::new(Inner::empty()), port_override: None })
    }

    /// Lazily bind 319/320 and start the grandmaster. Idempotent — returns the
    /// grandmaster clock id (a non-EUI-64 `0xFFFF….` value libairptp derives from
    /// the seed) that the per-device senders embed in their PT=87 anchors.
    pub fn ensure_started(&self) -> Result<u64, String> {
        let mut g = self.inner.lock().unwrap();
        if let Some(cid) = g.clock_id {
            return Ok(cid);
        }
        // Arbitrary 48-bit seed; libairptp ORs 0xFFFF into the top octets.
        const SEED: u64 = 0x0000_A1B2_C3D4_E5F6;
        let master = AirptpMaster::start(SEED, self.port_override)?;
        let cid = master.clock_id().ok_or_else(|| "airptp started but reported no clock id".to_string())?;
        g.master = Some(master);
        g.clock_id = Some(cid);
        tracing::info!("AP2 PTP grandmaster started, clock_id={:#018x}", cid);
        Ok(cid)
    }

    /// The grandmaster clock id, if started.
    pub fn clock_id(&self) -> Option<u64> {
        self.inner.lock().unwrap().clock_id
    }

    /// What we know about the receiver at `addr`'s gPTP behaviour right now: how long
    /// since it genuinely spoke to us, and whether it ever has (its class).
    ///
    /// **Not a plain getter** — it latches the two-way class the first time it sees real
    /// traffic, because that is the only moment the evidence exists (libairptp keeps a
    /// timestamp, not a history). Both callers poll on a timer, so the latch is reliable:
    /// the liveness tick (every 12 s) and `/api/outputs`.
    pub fn peer_ptp(&self, addr: &str) -> PeerPtp {
        let mut g = self.inner.lock().unwrap();
        let age = (|| {
            let master = g.master.as_ref()?;
            let peer_id = *g.peers.get(addr)?;
            let added_at = g.added_at.get(addr).copied().unwrap_or(0);
            let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).ok()?.as_secs();
            heard_age(master.peer_last_seen(peer_id), added_at, now)
        })();
        if age.is_some() && g.two_way.insert(addr.to_string()) {
            tracing::info!("AP2 PTP: {addr} answers our grandmaster (two-way gPTP) — its clock lock is observable");
        }
        PeerPtp { age, two_way: g.two_way.contains(addr) }
    }

    /// How long since the receiver at `addr` genuinely spoke to us; `None` if it never
    /// has. See [`PeerPtp`] — for a one-way receiver this is `None` forever *and that is
    /// normal*, so never treat it on its own as "not locked, so it renders silence".
    pub fn peer_lock_age(&self, addr: &str) -> Option<std::time::Duration> {
        self.peer_ptp(addr).age
    }

    /// Whether the receiver at `addr` is answering us right now (heard within `max_age`).
    pub fn peer_locked(&self, addr: &str, max_age: std::time::Duration) -> bool {
        self.peer_lock_age(addr).is_some_and(|age| age <= max_age)
    }

    /// Register a receiver (by IP string) as a PTP peer so libairptp drives it.
    /// Starts the grandmaster on first use. Idempotent per address.
    pub fn add_peer(&self, addr: &str) -> Result<(), String> {
        self.ensure_started()?;
        let mut g = self.inner.lock().unwrap();
        if g.peers.contains_key(addr) {
            return Ok(());
        }
        let id = {
            let m = g.master.as_ref().expect("master started above");
            m.add_peer(addr)?
        };
        g.peers.insert(addr.to_string(), id);
        // Remember when, so libairptp's seeded `last_seen` can be discounted.
        if let Ok(now) = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
            g.added_at.insert(addr.to_string(), now.as_secs());
        }
        tracing::info!("AP2 PTP: added peer {} (id={})", addr, id);
        Ok(())
    }

    /// Remove a previously added receiver (no-op if unknown / not started), keeping what
    /// we learned about its gPTP class — this is the reconnect path, and the device is
    /// the same device afterwards. Use [`Self::forget_peer`] when it actually leaves.
    pub fn remove_peer(&self, addr: &str) {
        let mut g = self.inner.lock().unwrap();
        g.added_at.remove(addr);
        if let Some(id) = g.peers.remove(addr) {
            if let Some(m) = g.master.as_ref() {
                m.remove_peer(id);
                tracing::info!("AP2 PTP: removed peer {} (id={})", addr, id);
            }
        }
    }

    /// Remove a receiver **and** forget its gPTP class — for a device that has left the
    /// registry. If it comes back it has to demonstrate two-way gPTP again, which is
    /// what keeps a replaced-but-same-IP device from inheriting the old one's verdict.
    pub fn forget_peer(&self, addr: &str) {
        self.remove_peer(addr);
        self.inner.lock().unwrap().two_way.remove(addr);
    }

    /// Currently registered peer addresses (for diagnostics/API).
    pub fn peers(&self) -> Vec<String> {
        self.inner.lock().unwrap().peers.keys().cloned().collect()
    }
}

/// Shared handle stored in `AppState`.
pub type SharedAp2Ptp = Arc<Ap2PtpService>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ffi_links_and_monotonic_ns_works() {
        // No bind — just proves the C library links and a symbol is callable.
        let _ = errmsg();
        assert!(monotonic_ns() > 0);
    }

    #[test]
    fn grandmaster_starts_and_reports_clock_id() {
        // Use high, unprivileged ports so the test needs no capability and can't
        // collide with a real grandmaster on 319/320.
        let svc = Arc::new(Ap2PtpService { inner: Mutex::new(Inner::empty()), port_override: Some((19319, 19320)) });
        let cid = svc.ensure_started().expect("grandmaster should start");
        // libairptp forces a non-EUI-64 id: top 16 bits = 0xFFFF.
        assert_eq!(cid >> 48, 0xFFFF, "clock id should carry the 0xFFFF prefix");
        assert_eq!(svc.clock_id(), Some(cid));
        // Idempotent.
        assert_eq!(svc.ensure_started().unwrap(), cid);
        // Peer add/remove round-trips (loopback addr is fine — no traffic asserted).
        svc.add_peer("127.0.0.1").expect("add_peer");
        assert_eq!(svc.peers(), vec!["127.0.0.1".to_string()]);
        svc.add_peer("127.0.0.1").expect("idempotent add");
        assert_eq!(svc.peers().len(), 1);
        svc.remove_peer("127.0.0.1");
        assert!(svc.peers().is_empty());
    }

    /// The bug this module was fixed for, and the one worth pinning hardest: adding a
    /// peer must not look like the receiver answered us. libairptp seeds
    /// `peer->last_seen` with `time(NULL)` in `daemon_peer_add`, so before this check a
    /// freshly-added receiver read back as "locked 0 s ago" — which is how a receiver
    /// that has never sent a single gPTP packet ended up in a two-minute
    /// lose-lock → reconnect → "locked!" → lose-lock loop, the reconnect being the only
    /// thing that ever refreshed the timestamp.
    #[test]
    fn a_peer_add_is_not_evidence_that_the_receiver_answered() {
        let now = 1_000_000u64;
        // Exactly the seed `daemon_peer_add` writes.
        assert_eq!(heard_age(now, now, now), None, "the add's own timestamp is not traffic");
        // And still not, later: the seed never becomes evidence just because time passed.
        assert_eq!(heard_age(now, now, now + 300), None);
        // libairptp's explicit "never".
        assert_eq!(heard_age(0, now, now), None);
        // A packet that arrived after the add IS evidence, and its age is real.
        assert_eq!(heard_age(now + 5, now, now + 8), Some(std::time::Duration::from_secs(3)));
        // A re-add (reconnect) invalidates the older reading rather than preserving it:
        // otherwise a recovery attempt would credit itself with the lock it went looking for.
        assert_eq!(heard_age(now + 5, now + 10, now + 12), None);
        // Clock going backwards must not panic or read as a huge age.
        assert_eq!(heard_age(now + 5, now, now), Some(std::time::Duration::ZERO));
    }

    /// A receiver's *class* — does it answer gPTP at all — has to survive a reconnect,
    /// because the reconnect path removes and re-adds the peer. Losing it there would
    /// disarm the watchdog for exactly the two-way receivers it exists to protect.
    #[test]
    fn the_gptp_class_survives_a_reconnect_but_not_a_departure() {
        let svc = Arc::new(Ap2PtpService { inner: Mutex::new(Inner::empty()), port_override: Some((19419, 19420)) });
        svc.ensure_started().expect("grandmaster should start");
        svc.add_peer("127.0.0.2").expect("add_peer");
        // Nothing heard yet: a loopback "peer" never answers, which is exactly the
        // one-way receiver's steady state.
        assert_eq!(svc.peer_ptp("127.0.0.2"), PeerPtp { age: None, two_way: false });

        // Stand in for "we heard from it" by latching the class directly — the FFI's
        // timestamp is not ours to fake.
        svc.inner.lock().unwrap().two_way.insert("127.0.0.2".to_string());
        svc.remove_peer("127.0.0.2");
        svc.add_peer("127.0.0.2").expect("re-add");
        assert!(svc.peer_ptp("127.0.0.2").two_way, "a reconnect must not erase what we learned");

        svc.forget_peer("127.0.0.2");
        svc.add_peer("127.0.0.2").expect("re-add after departure");
        assert!(!svc.peer_ptp("127.0.0.2").two_way, "a device that left has to prove itself again");
    }
}
