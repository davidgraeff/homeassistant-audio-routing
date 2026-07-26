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

use std::collections::HashMap;
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
    unsafe { clock_gettime(1 /* CLOCK_MONOTONIC */, &mut ts) };
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
    clock_id: Option<u64>,
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
        Arc::new(Self {
            inner: Mutex::new(Inner {
                master: None,
                peers: HashMap::new(),
                clock_id: None,
            }),
            port_override: None,
        })
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
        let cid = master
            .clock_id()
            .ok_or_else(|| "airptp started but reported no clock id".to_string())?;
        g.master = Some(master);
        g.clock_id = Some(cid);
        tracing::info!("AP2 PTP grandmaster started, clock_id={:#018x}", cid);
        Ok(cid)
    }

    /// The grandmaster clock id, if started.
    pub fn clock_id(&self) -> Option<u64> {
        self.inner.lock().unwrap().clock_id
    }

    /// How long since libairptp last heard a gPTP message from the receiver at `addr`
    /// — its PTP liveness/lock age. A locked receiver sends `Delay_Req` continuously,
    /// so a small age = healthy; a large age or `None` (never seen / not a peer / not
    /// started) means the receiver is NOT PTP-locked, so the AP2 stream's PT=87 anchors
    /// are meaningless to it and it renders silence. Used for degraded-connection UI.
    pub fn peer_lock_age(&self, addr: &str) -> Option<std::time::Duration> {
        let g = self.inner.lock().unwrap();
        let master = g.master.as_ref()?;
        let peer_id = *g.peers.get(addr)?;
        let last_seen = master.peer_last_seen(peer_id);
        if last_seen == 0 {
            return None;
        }
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?
            .as_secs();
        Some(std::time::Duration::from_secs(now.saturating_sub(last_seen)))
    }

    /// Whether the receiver at `addr` looks PTP-locked (heard from within `max_age`).
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
        tracing::info!("AP2 PTP: added peer {} (id={})", addr, id);
        Ok(())
    }

    /// Remove a previously added receiver (no-op if unknown / not started).
    pub fn remove_peer(&self, addr: &str) {
        let mut g = self.inner.lock().unwrap();
        if let Some(id) = g.peers.remove(addr) {
            if let Some(m) = g.master.as_ref() {
                m.remove_peer(id);
                tracing::info!("AP2 PTP: removed peer {} (id={})", addr, id);
            }
        }
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
        let svc = Arc::new(Ap2PtpService {
            inner: Mutex::new(Inner {
                master: None,
                peers: HashMap::new(),
                clock_id: None,
            }),
            port_override: Some((19319, 19320)),
        });
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
}
