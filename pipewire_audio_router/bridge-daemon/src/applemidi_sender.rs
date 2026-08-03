//! AppleMIDI-compatible RTP **audio sender** — the pw-sink transport that lets a
//! stock PipeWire `module-rtp-session` receiver auto-discover the daemon and play
//! its audio, with **no Avahi** on the daemon (mDNS via the `mdns-sd` crate).
//!
//! # Why this module exists
//! `module-rtp-session` (the only mDNS-discoverable PipeWire audio receiver) will
//! not accept plain unidirectional RTP: it runs an AppleMIDI / RTP-MIDI control
//! handshake first. The daemon can't run `module-rtp-session` itself (it needs a
//! system `avahi-daemon`, deliberately absent — the daemon does mDNS in-process),
//! so it speaks the AppleMIDI **sender** side directly. This was proven end-to-end
//! by a Python spike against a stock receiver (clean tone, `E@440 = 0.238`); see
//! `docs/pipewire-sink-spike-results.md` ("PROVEN" section) and the blueprint
//! `scratchpad/spikes/applemidi_sender.py`.
//!
//! # This file is a FROZEN INTERFACE (contract)
//! The public API below (types + `AppleMidiSender::{start,status}`) is the
//! boundary between Task 1 (this module's implementation) and Task 2 (the Phase-B
//! backend that consumes it: discovery, sync_group follower, media_player). **Keep
//! the public signatures stable** — implement the bodies, don't reshape the API.
//!
//! # The proven protocol recipe (implement exactly this)
//! 1. **Advertise** `_pipewire-audio._udp` via `mdns-sd` (use the shared advertise
//!    daemon when given). Instance = `session_name`; SRV port = `control_port`;
//!    TXT: `subtype=audio format=S16BE rate=<rate> channels=<ch>
//!    position=[ FL FR ] layout=Stereo ts-refclk=private ts-offset=0`.
//! 2. **Bind** UDP `control_port` (control) and `control_port + 1` (data).
//! 3. **The receiver initiates.** It sends `APPLE_MIDI_CMD_IN` (`0xFFFF 0x494E`)
//!    to our control port, then to our data port, each from its own ephemeral
//!    port. For every `IN` reply `OK` (`0x4F4B`) echoing the peer's 4-byte
//!    initiator token + **our** 32-bit SSRC; packet = `FFFF, cmd, protocol=2,
//!    initiator, ssrc, <NUL-terminated session_name>`, all **big-endian**.
//! 4. **Answer `CK`** (`0x434B`) clock-sync on the data channel: on `count=0`
//!    echo ts1, fill ts2 (= our monotonic time in 100 µs units), reply `count=1`;
//!    peer closes with `count=2`. (Optional for audio, but the module sends it.)
//! 5. **Stream RTP** to the source address of each established data `IN`: 12-byte
//!    RTP header (`0x80`, `PT=127`, seq++, ts += samples/packet, our SSRC) +
//!    **L16 big-endian** payload (byte-swap the incoming native-endian S16),
//!    ~2 ms/packet (`rate/500` frames), real-time paced. The module matches the
//!    session by our SSRC and plays it. Send to **every** established data peer.
//! 6. On `BY` (`0x4259`) or drop, retire that peer. On `Drop` send `BY` to all,
//!    close sockets, withdraw the advert, join threads.
//!
//! # Testability (no PipeWire needed)
//! The sender takes PCM over a channel, so it runs without PipeWire. Validate
//! standalone against a stock receiver on the dev box: feed a synthetic tone into
//! the channel, run `pw-cli load-module libpipewire-module-rtp-session {
//! sess.discover-local=true sess.media=audio audio.format=S16BE audio.rate=48000
//! audio.channels=2 }`, and record the created `rtp_session.<name>...ipv4`
//! Audio/Source — mirror `scratchpad/spikes/test_am.sh`. Suggested harness: a
//! hidden dev subcommand `bridge-daemon applemidi-spike --port <p> --freq <hz>`
//! that starts a sender fed a generated tone. Same-host tests need
//! `sess.discover-local=true`; the real deployment is cross-host (no such flag).

use std::collections::HashSet;
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use mdns_sd::{ServiceDaemon, ServiceInfo};

// ---- AppleMIDI / RTP-MIDI wire constants (all big-endian on the wire) -------

/// Every AppleMIDI control/session command starts with this 16-bit signature.
const SIG: u16 = 0xFFFF;
const CMD_IN: u16 = 0x494E; // "IN" — session invitation
const CMD_OK: u16 = 0x4F4B; // "OK" — invitation accepted
const CMD_BY: u16 = 0x4259; // "BY" — end session
const CMD_CK: u16 = 0x434B; // "CK" — clock synchronization
/// AppleMIDI protocol version carried in IN/OK/BY.
const APPLEMIDI_PROTOCOL: u32 = 2;
/// RTP payload type used by `module-rtp-session` for L16 audio (dynamic).
const RTP_PAYLOAD_TYPE: u8 = 127;
/// mDNS service type `module-rtp-session` browses for audio sessions.
const MDNS_SERVICE_TYPE: &str = "_pipewire-audio._udp.local.";

/// Stream format. pw-sink fixes the *wire* encoding to L16 (S16 big-endian) to
/// match `module-rtp-session`'s audio session; only `rate`/`channels` vary.
#[derive(Clone, Copy, Debug)]
pub struct SessionFormat {
    pub rate: u32,
    pub channels: u16,
}

impl Default for SessionFormat {
    fn default() -> Self {
        Self { rate: 48_000, channels: 2 }
    }
}

/// Configuration for one advertised pw-sink session (one per routed target).
pub struct SessionConfig {
    /// mDNS instance name **and** AppleMIDI session name, e.g. `pwsink-<slug>`.
    /// Also the identity a receiver can filter on for per-target routing.
    pub session_name: String,
    /// UDP control port to bind + advertise (data port = `control_port + 1`).
    /// Must be a concrete port the caller allocated (not 0) — the receiver
    /// initiates to this advertised port.
    pub control_port: u16,
    /// LAN interface to pin the mDNS advert + RTP egress to (`None` = default).
    pub ifname: Option<String>,
    /// Audio rate/channels (wire encoding is always L16/big-endian).
    pub format: SessionFormat,
    /// Shared mDNS advertise daemon
    /// ([`crate::discovery_supervisor::shared_advertise_daemon`]). Reuse it to
    /// stay storm-safe; `None` makes the sender create its own daemon (only for
    /// standalone testing — never in the daemon).
    pub advertise_daemon: Option<mdns_sd::ServiceDaemon>,
}

/// Live status of a session — consumed by liveness + `media_player` state.
#[derive(Clone, Debug, Default)]
pub struct SessionStatus {
    /// At least one receiver has completed the IN/OK handshake (control+data).
    pub established: bool,
    /// Number of receivers currently in an established session (streaming).
    pub peer_count: usize,
}

/// One interleaved **native-endian** S16 PCM chunk (`L, R, L, R, …`) at the
/// session rate/channels, as captured from PipeWire. The sender byte-swaps to
/// L16 (big-endian) on the wire and repacketizes into ~2 ms RTP packets. Task 2
/// bridges the group-anchor capture into the channel that yields these.
pub type PcmChunk = Vec<i16>;

/// A running AppleMIDI sender session (one per target). Advertises over mDNS,
/// answers the handshake from discovering `module-rtp-session` receivers, and
/// streams the PCM it is fed as L16 RTP to every established peer. **Dropping it
/// tears everything down** (BY to peers, close sockets, withdraw advert, join
/// threads) — teardown is via `Drop`, so just drop the handle.
pub struct AppleMidiSender {
    /// Shared status, updated by the worker(s); read by [`Self::status`].
    status: Arc<Mutex<SessionStatus>>,
    /// Signals every worker thread to wind down (set on `Drop`).
    shutdown: Arc<AtomicBool>,
    /// Session identity + live peer bookkeeping, shared with the workers.
    peers: Arc<PeerState>,
    /// Clones of the bound sockets, kept so `Drop` can send `BY` to peers.
    ctrl_sock: UdpSocket,
    data_sock: UdpSocket,
    /// Worker thread handles (control reader, data reader, RTP sender).
    threads: Vec<JoinHandle<()>>,
    /// The registered advert: `(daemon, fullname, owns_daemon)` for unregister.
    mdns: Option<(ServiceDaemon, String, bool)>,
}

/// Shared session identity + the live set of handshaken peers, updated by the
/// reader threads and read by the RTP sender + [`AppleMidiSender::status`].
struct PeerState {
    /// Our 32-bit synchronization source — the module matches the RTP stream to
    /// the session by this value (must stay constant for the session's life).
    ssrc: u32,
    /// AppleMIDI session name echoed in every `OK` (NUL-terminated on the wire).
    session_name: String,
    /// At least one `IN`/`OK` completed on the control channel.
    ctrl_ready: AtomicBool,
    /// The receiver's control-channel address (for a courtesy `BY` on teardown).
    ctrl_peer: Mutex<Option<SocketAddr>>,
    /// Data-channel peers that completed their `IN`/`OK`; RTP goes to each.
    data_peers: Mutex<HashSet<SocketAddr>>,
    /// Mirror of the public status, kept in sync as peers come and go.
    status: Arc<Mutex<SessionStatus>>,
}

impl PeerState {
    /// Recompute `established`/`peer_count` from the current peer set.
    fn refresh_status(&self) {
        let count = self.data_peers.lock().map(|p| p.len()).unwrap_or(0);
        let established = self.ctrl_ready.load(Ordering::Relaxed) && count > 0;
        if let Ok(mut s) = self.status.lock() {
            s.established = established;
            s.peer_count = count;
        }
    }
}

impl AppleMidiSender {
    /// Start the session: register the mDNS advert, bind the control+data ports,
    /// and spawn the worker(s) that run the AppleMIDI handshake and drain `pcm`
    /// into paced L16 RTP. **Non-blocking** — returns once the advert is up and
    /// the sockets are bound. Closing the `pcm` sender stops audio; the session
    /// stays advertised until this handle is dropped.
    ///
    /// `pcm` delivers native-endian S16 interleaved chunks at `config.format`
    /// (fed at real-time rate from the anchor capture in Task 2). The worker
    /// buffers and repacketizes into ~2 ms RTP packets.
    pub fn start(config: SessionConfig, pcm: Receiver<PcmChunk>) -> anyhow::Result<Self> {
        let SessionConfig { session_name, control_port, ifname: _ifname, format, advertise_daemon } = config;
        let data_port = control_port
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("control_port {control_port} leaves no room for the data port"))?;

        // 2. Bind the control + data UDP ports on IPv4 (the receiver initiates
        //    to these advertised ports; bind v4 so single-host ipv4 sessions
        //    work — see the spike gotchas).
        let ctrl_sock = UdpSocket::bind(("0.0.0.0", control_port))
            .map_err(|e| anyhow::anyhow!("bind control port {control_port}: {e}"))?;
        let data_sock = UdpSocket::bind(("0.0.0.0", data_port))
            .map_err(|e| anyhow::anyhow!("bind data port {data_port}: {e}"))?;
        // Short read timeouts so the reader threads can observe `shutdown`.
        ctrl_sock.set_read_timeout(Some(Duration::from_millis(200)))?;
        data_sock.set_read_timeout(Some(Duration::from_millis(200)))?;

        let status = Arc::new(Mutex::new(SessionStatus::default()));
        let shutdown = Arc::new(AtomicBool::new(false));
        let peers = Arc::new(PeerState {
            ssrc: gen_ssrc(),
            session_name: session_name.clone(),
            ctrl_ready: AtomicBool::new(false),
            ctrl_peer: Mutex::new(None),
            data_peers: Mutex::new(HashSet::new()),
            status: status.clone(),
        });

        // 1. Advertise `_pipewire-audio._udp` (SRV port = control port).
        let mdns = advertise(&session_name, control_port, format, advertise_daemon)?;

        // Clones the handle keeps for teardown `BY`s.
        let ctrl_keep = ctrl_sock.try_clone()?;
        let data_keep = data_sock.try_clone()?;

        let mut threads = Vec::with_capacity(3);

        // Control-channel reader: answers IN -> OK, CK, BY.
        threads.push(std::thread::spawn({
            let peers = peers.clone();
            let shutdown = shutdown.clone();
            move || reader_loop(ctrl_sock, peers, shutdown, Channel::Control)
        }));
        // Data-channel reader: answers IN -> OK (registers a peer), CK, BY.
        threads.push(std::thread::spawn({
            let peers = peers.clone();
            let shutdown = shutdown.clone();
            move || reader_loop(data_sock, peers, shutdown, Channel::Data)
        }));
        // RTP sender: drains `pcm`, repacketizes into ~2 ms L16 packets, paces.
        threads.push(std::thread::spawn({
            let peers = peers.clone();
            let shutdown = shutdown.clone();
            move || rtp_sender_loop(pcm, peers, shutdown, format)
        }));

        Ok(Self {
            status,
            shutdown,
            peers,
            ctrl_sock: ctrl_keep,
            data_sock: data_keep,
            threads,
            mdns,
        })
    }

    /// Snapshot of the current liveness/status.
    pub fn status(&self) -> SessionStatus {
        self.status.lock().map(|s| s.clone()).unwrap_or_default()
    }
}

impl Drop for AppleMidiSender {
    fn drop(&mut self) {
        // Withdraw the advert first so no new receiver starts a handshake.
        if let Some((daemon, fullname, owns)) = self.mdns.take() {
            let _ = daemon.unregister(&fullname);
            if owns {
                let _ = daemon.shutdown();
            }
        }
        // Courtesy `BY` to every established peer (best-effort).
        let by = build_session_pkt(CMD_BY, 0, self.peers.ssrc, None);
        if let Ok(peer) = self.peers.ctrl_peer.lock() {
            if let Some(addr) = *peer {
                let _ = self.ctrl_sock.send_to(&by, addr);
            }
        }
        if let Ok(peers) = self.peers.data_peers.lock() {
            for addr in peers.iter() {
                let _ = self.data_sock.send_to(&by, *addr);
            }
        }
        // Signal + join the workers (their read timeouts bound the wait).
        self.shutdown.store(true, Ordering::Relaxed);
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}

/// Which AppleMIDI channel a reader thread is servicing.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Channel {
    Control,
    Data,
}

/// Pick a nonzero, session-stable 32-bit SSRC without pulling in an RNG crate.
fn gen_ssrc() -> u32 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or(0x1A2B3C4D);
    let mixed = nanos ^ std::process::id().wrapping_mul(2_654_435_761);
    mixed | 1 // never zero
}

/// Register the mDNS advert for this session. Returns `(daemon, fullname,
/// owns_daemon)`; `owns_daemon` is true only when we created the daemon (the
/// standalone-test path) so `Drop` knows whether to shut it down.
fn advertise(
    session_name: &str,
    control_port: u16,
    format: SessionFormat,
    provided: Option<ServiceDaemon>,
) -> anyhow::Result<Option<(ServiceDaemon, String, bool)>> {
    let (daemon, owns) = match provided {
        Some(d) => (d, false),
        None => (ServiceDaemon::new().map_err(|e| anyhow::anyhow!("mDNS daemon: {e}"))?, true),
    };
    let rate = format.rate.to_string();
    let channels = format.channels.to_string();
    let props = [
        ("subtype", "audio"),
        ("format", "S16BE"),
        ("rate", rate.as_str()),
        ("channels", channels.as_str()),
        ("position", "[ FL FR ]"),
        ("layout", "Stereo"),
        ("ts-refclk", "private"),
        ("ts-offset", "0"),
    ];
    let host = format!("{session_name}.local.");
    let si = ServiceInfo::new(MDNS_SERVICE_TYPE, session_name, &host, "", control_port, &props[..])
        .map_err(|e| anyhow::anyhow!("mDNS ServiceInfo: {e}"))?
        .enable_addr_auto();
    let fullname = si.get_fullname().to_string();
    daemon.register(si).map_err(|e| anyhow::anyhow!("mDNS register: {e}"))?;
    Ok(Some((daemon, fullname, owns)))
}

/// Build an AppleMIDI session-command packet (`IN`/`OK`/`BY`):
/// `SIG, cmd, protocol=2, initiator, ssrc[, name\0]`, all big-endian.
fn build_session_pkt(cmd: u16, initiator: u32, ssrc: u32, name: Option<&str>) -> Vec<u8> {
    let mut p = Vec::with_capacity(16 + name.map_or(0, |n| n.len() + 1));
    p.extend_from_slice(&SIG.to_be_bytes());
    p.extend_from_slice(&cmd.to_be_bytes());
    p.extend_from_slice(&APPLEMIDI_PROTOCOL.to_be_bytes());
    p.extend_from_slice(&initiator.to_be_bytes());
    p.extend_from_slice(&ssrc.to_be_bytes());
    if let Some(n) = name {
        p.extend_from_slice(n.as_bytes());
        p.push(0);
    }
    p
}

/// Read the 16-bit command word of an AppleMIDI packet, or `None` if this is
/// not one (e.g. an inbound RTP packet on the data socket).
fn parse_cmd(data: &[u8]) -> Option<u16> {
    if data.len() < 4 || u16::from_be_bytes([data[0], data[1]]) != SIG {
        return None;
    }
    Some(u16::from_be_bytes([data[2], data[3]]))
}

/// Handle an inbound clock-sync (`CK`) packet, replying `count=1` when the peer
/// opens with `count=0`. Layout: `SIG, CK, ssrc(4), count(1), pad(3),
/// ts1(8), ts2(8), ts3(8)` — three big-endian i64 timestamps.
fn handle_ck(sock: &UdpSocket, data: &[u8], addr: SocketAddr, our_ssrc: u32, start: Instant) {
    if data.len() < 36 {
        return;
    }
    let count = data[8];
    if count != 0 {
        return; // count=2 completes the exchange; nothing to send.
    }
    let ts1 = i64::from_be_bytes(data[12..20].try_into().unwrap());
    let ts3 = i64::from_be_bytes(data[28..36].try_into().unwrap());
    // Our monotonic clock in 100 µs units (matches the Python blueprint).
    let now = (start.elapsed().as_nanos() / 10_000) as i64;
    let mut reply = Vec::with_capacity(36);
    reply.extend_from_slice(&SIG.to_be_bytes());
    reply.extend_from_slice(&CMD_CK.to_be_bytes());
    reply.extend_from_slice(&our_ssrc.to_be_bytes());
    reply.push(1); // count = 1 (responder)
    reply.extend_from_slice(&[0, 0, 0]); // padding
    reply.extend_from_slice(&ts1.to_be_bytes()); // echo ts1
    reply.extend_from_slice(&now.to_be_bytes()); // fill ts2
    reply.extend_from_slice(&ts3.to_be_bytes()); // ts3 untouched
    let _ = sock.send_to(&reply, addr);
}

/// Reader loop for one channel: parse AppleMIDI commands and drive the
/// handshake state machine. Exits when `shutdown` is set.
fn reader_loop(sock: UdpSocket, peers: Arc<PeerState>, shutdown: Arc<AtomicBool>, ch: Channel) {
    let start = Instant::now();
    let mut buf = [0u8; 2048];
    while !shutdown.load(Ordering::Relaxed) {
        let (n, addr) = match sock.recv_from(&mut buf) {
            Ok(v) => v,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock || e.kind() == std::io::ErrorKind::TimedOut => continue,
            Err(_) => continue,
        };
        let data = &buf[..n];
        let Some(cmd) = parse_cmd(data) else { continue };
        match cmd {
            CMD_IN => {
                if data.len() < 16 {
                    continue;
                }
                let initiator = u32::from_be_bytes(data[8..12].try_into().unwrap());
                let ok = build_session_pkt(CMD_OK, initiator, peers.ssrc, Some(&peers.session_name));
                let _ = sock.send_to(&ok, addr);
                match ch {
                    Channel::Control => {
                        peers.ctrl_ready.store(true, Ordering::Relaxed);
                        if let Ok(mut p) = peers.ctrl_peer.lock() {
                            *p = Some(addr);
                        }
                    }
                    Channel::Data => {
                        if let Ok(mut p) = peers.data_peers.lock() {
                            p.insert(addr);
                        }
                    }
                }
                peers.refresh_status();
            }
            CMD_CK => handle_ck(&sock, data, addr, peers.ssrc, start),
            CMD_BY => {
                match ch {
                    Channel::Control => {
                        peers.ctrl_ready.store(false, Ordering::Relaxed);
                        if let Ok(mut p) = peers.ctrl_peer.lock() {
                            if *p == Some(addr) {
                                *p = None;
                            }
                        }
                    }
                    Channel::Data => {
                        if let Ok(mut p) = peers.data_peers.lock() {
                            p.remove(&addr);
                        }
                    }
                }
                peers.refresh_status();
            }
            _ => {}
        }
    }
}

/// RTP sender loop: drain native-endian S16 PCM, repacketize into ~2 ms L16
/// (big-endian) RTP packets, and stream to every established data peer, paced
/// to real time. Returns when `pcm` disconnects (audio stops) or on shutdown.
fn rtp_sender_loop(pcm: Receiver<PcmChunk>, peers: Arc<PeerState>, shutdown: Arc<AtomicBool>, format: SessionFormat) {
    let send_sock = match UdpSocket::bind(("0.0.0.0", 0)) {
        Ok(s) => s,
        Err(_) => return,
    };
    let channels = format.channels.max(1) as usize;
    let frames_per_pkt = (format.rate / 500).max(1) as usize; // ~2 ms
    let samples_per_pkt = frames_per_pkt * channels;
    let dt = Duration::from_nanos((frames_per_pkt as u64 * 1_000_000_000) / format.rate.max(1) as u64);

    let mut buf: Vec<i16> = Vec::with_capacity(samples_per_pkt * 4);
    let mut seq: u16 = 0;
    let mut ts: u32 = 0;
    let mut next = Instant::now();

    loop {
        if shutdown.load(Ordering::Relaxed) {
            return;
        }
        // No handshaken data peer yet: discard PCM so it can't pile up, and keep
        // the pacing clock fresh so streaming starts cleanly once a peer joins.
        let have_peers = peers.data_peers.lock().map(|p| !p.is_empty()).unwrap_or(false);
        if !have_peers {
            match pcm.recv_timeout(Duration::from_millis(100)) {
                Ok(_) => {}
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
            buf.clear();
            next = Instant::now();
            continue;
        }

        // Accumulate at least one packet's worth of samples.
        while buf.len() < samples_per_pkt {
            match pcm.recv_timeout(Duration::from_millis(200)) {
                Ok(chunk) => buf.extend_from_slice(&chunk),
                Err(RecvTimeoutError::Timeout) => {
                    if shutdown.load(Ordering::Relaxed) {
                        return;
                    }
                }
                Err(RecvTimeoutError::Disconnected) => return, // sender closed: stop audio.
            }
        }

        // 12-byte RTP header + L16 big-endian payload (byte-swap S16 samples).
        let mut pkt = Vec::with_capacity(12 + samples_per_pkt * 2);
        pkt.push(0x80); // V=2, no padding/extension/CSRC
        pkt.push(RTP_PAYLOAD_TYPE & 0x7f); // marker=0, PT=127
        pkt.extend_from_slice(&seq.to_be_bytes());
        pkt.extend_from_slice(&ts.to_be_bytes());
        pkt.extend_from_slice(&peers.ssrc.to_be_bytes());
        for &s in &buf[..samples_per_pkt] {
            pkt.extend_from_slice(&s.to_be_bytes());
        }
        buf.drain(..samples_per_pkt);

        if let Ok(list) = peers.data_peers.lock() {
            for addr in list.iter() {
                let _ = send_sock.send_to(&pkt, *addr);
            }
        }

        seq = seq.wrapping_add(1);
        ts = ts.wrapping_add(frames_per_pkt as u32);

        // Real-time pace: advance the deadline, sleep the remainder.
        next += dt;
        let now = Instant::now();
        if next > now {
            std::thread::sleep(next - now);
        } else {
            next = now; // fell behind — don't accumulate a debt.
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::channel;

    #[test]
    fn ok_packet_matches_wire_layout() {
        // build_session_pkt(OK, initiator, ssrc, name):
        // FFFF OK proto=2 initiator ssrc name\0
        let pkt = build_session_pkt(CMD_OK, 0x1122_3344, 0xDEAD_BEEF, Some("hi"));
        assert_eq!(&pkt[0..2], &[0xFF, 0xFF]); // signature
        assert_eq!(&pkt[2..4], &[0x4F, 0x4B]); // "OK"
        assert_eq!(&pkt[4..8], &[0, 0, 0, 2]); // protocol = 2
        assert_eq!(&pkt[8..12], &0x1122_3344u32.to_be_bytes()); // echoed initiator
        assert_eq!(&pkt[12..16], &0xDEAD_BEEFu32.to_be_bytes()); // our ssrc
        assert_eq!(&pkt[16..], b"hi\0"); // NUL-terminated session name
    }

    #[test]
    fn by_packet_has_no_name() {
        let pkt = build_session_pkt(CMD_BY, 0, 0x1, None);
        assert_eq!(pkt.len(), 16);
        assert_eq!(&pkt[2..4], &[0x42, 0x59]); // "BY"
    }

    #[test]
    fn parse_cmd_recognizes_signature_and_rejects_rtp() {
        let ok = build_session_pkt(CMD_IN, 7, 9, None);
        assert_eq!(parse_cmd(&ok), Some(CMD_IN));
        // An RTP packet begins 0x80 0x7f... — not the 0xFFFF signature.
        assert_eq!(parse_cmd(&[0x80, 0x7f, 0, 0, 0, 0]), None);
        assert_eq!(parse_cmd(&[0xFF]), None); // too short
    }

    #[test]
    fn ssrc_is_nonzero() {
        assert_ne!(gen_ssrc(), 0);
    }

    /// Spike: prove a stock `module-rtp-session` receiver establishes the
    /// AppleMIDI handshake against this sender and plays a clean 440 Hz tone.
    ///
    /// Run alongside the shell harness (see the module docs / test_am.sh):
    /// `cargo test --bin bridge-daemon applemidi -- --ignored --nocapture`
    /// then load `libpipewire-module-rtp-session { sess.discover-local=true
    /// sess.media=audio audio.format=S16BE audio.rate=48000 audio.channels=2 }`
    /// and `pw-record` the discovered `rtp_session.*.ipv4` Audio/Source.
    #[test]
    #[ignore]
    fn spike_against_module_rtp_session() {
        let format = SessionFormat { rate: 48_000, channels: 2 };
        let config = SessionConfig {
            session_name: "pw-router-am".to_string(),
            control_port: 5004,
            ifname: None,
            format,
            advertise_daemon: None, // standalone: create our own mDNS daemon.
        };

        let (tx, rx) = channel::<PcmChunk>();
        // Feed a 440 Hz stereo tone (amplitude ~8000) at real time until the
        // sender drops the receiver end (on teardown).
        let feeder = std::thread::spawn(move || {
            let rate = format.rate as f32;
            let freq = 440.0f32;
            let chunk_frames = (format.rate / 100) as usize; // 10 ms
            let mut phase = 0.0f32;
            loop {
                let mut chunk: Vec<i16> = Vec::with_capacity(chunk_frames * 2);
                for _ in 0..chunk_frames {
                    let v = (phase.sin() * 8000.0) as i16;
                    phase += 2.0 * std::f32::consts::PI * freq / rate;
                    if phase > 2.0 * std::f32::consts::PI {
                        phase -= 2.0 * std::f32::consts::PI;
                    }
                    chunk.push(v); // L
                    chunk.push(v); // R
                }
                if tx.send(chunk).is_err() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        });

        let sender = AppleMidiSender::start(config, rx).expect("sender starts");
        eprintln!("[am] sender up on control 5004 / data 5005; waiting 15s for a receiver...");
        std::thread::sleep(Duration::from_secs(15));
        let st = sender.status();
        eprintln!("[am] status: established={} peers={}", st.established, st.peer_count);
        drop(sender);
        let _ = feeder.join();
    }
}
