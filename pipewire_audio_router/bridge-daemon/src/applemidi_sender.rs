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
//!    [`PACKET_MS`]/packet (`rate / (1000/PACKET_MS)` frames), real-time paced and
//!    catching up after a late wakeup. The module matches the
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

/// Audio carried by one RTP packet, in ms — the sender's `ptime`. **5 ms**: 200
/// wakeups and 200 `sendto` calls per second per target.
///
/// Three constraints pin it:
/// * **MTU.** L16 stereo at 48 kHz is 192 B/ms, so 5 ms is 960 B of payload plus 12 B
///   RTP and 28 B IP/UDP = 1000 B, inside PipeWire's 1280 B default. (6 ms fits; 8 ms
///   does not.)
/// * **The rate divides exactly** — 240 frames at 48 kHz, no rounding.
/// * **The jitter buffer divides by it.** `module-rtp-session` warns unless
///   `sess.latency.msec` is an integer multiple of `rtp.ptime`, and its default buffer
///   is 100 ms: divisible by 5, not by 6.
///
/// For scale, the other backends here: AirPlay 2 sends 352 frames (7.98 ms at
/// 44.1 kHz), sendspin encodes 20 ms Opus blocks, and PipeWire's own `rtp-sink` packs
/// up to `net.mtu`, landing near 6 ms for this format.
pub const PACKET_MS: u32 = 5;

/// Backlog (ms of undelivered audio) the sender aims to hold. A few packets: just
/// enough that ordinary scheduling jitter doesn't starve the next send, and small
/// enough to be a rounding error in the end-to-end delay.
const TARGET_BACKLOG_MS: i64 = 6;

/// Absolute ceiling on a catch-up burst, whatever the receiver's buffer allows: one
/// session must not monopolise the CPU it just got back. 32 packets = 160 ms.
const MAX_BURST_PACKETS: usize = 32;

/// Floor on a catch-up burst: below two packets there is no catching up at all, so
/// even the smallest configured buffer gets this much.
const MIN_BURST_PACKETS: usize = 2;

/// The two backlog limits, derived from the receiver's playout buffer.
///
/// Both scale with that buffer because it is the physical constraint: a burst we emit
/// has to *fit* the far end's jitter buffer, and a backlog we choose to keep has to be
/// one it can absorb. The ceiling is therefore always above one full burst — a lower
/// one would discard audio the loop is about to deliver.
#[derive(Clone, Copy, Debug)]
pub(crate) struct BacklogLimits {
    /// Most packets one wakeup may emit back-to-back while catching up.
    burst_packets: usize,
    /// Past this much queued audio, the oldest is dropped back to
    /// [`TARGET_BACKLOG_MS`]. Deliberately several bursts' worth: multi-pass
    /// catch-up must get its chance before anything is thrown away.
    max_backlog_ms: i64,
}

impl BacklogLimits {
    /// `playout_ms` is the receiver's jitter buffer — what the daemon told this
    /// target's agent to configure as `sess.latency.msec`.
    ///
    /// A burst gets two thirds of that buffer: enough to absorb the scheduling gaps
    /// a loaded host actually produces, while leaving the far end headroom rather
    /// than filling it exactly. The ceiling is three bursts, so a gap too big for
    /// one wakeup is still caught up over the next few instead of being dropped.
    fn for_playout(playout_ms: u16) -> Self {
        let burst_ms = i64::from(playout_ms) * 2 / 3;
        let burst_packets = ((burst_ms / i64::from(PACKET_MS)) as usize).clamp(MIN_BURST_PACKETS, MAX_BURST_PACKETS);
        let burst_ms = (burst_packets as i64) * i64::from(PACKET_MS);
        Self { burst_packets, max_backlog_ms: (burst_ms * 3).max(TARGET_BACKLOG_MS * 2) }
    }
}

/// How hard the pace is trimmed per ms of backlog error, in parts per million of
/// the packet interval, and the ceiling on that trim. 250 ppm/ms saturates at
/// ±20 ms of error, and ±5000 ppm (±0.5 %) is ~50× the clock mismatch between two
/// ordinary hosts while being far too small for the receiver's resampler to care.
const PACE_TRIM_PPM_PER_MS: i64 = 250;
const PACE_TRIM_PPM_MAX: i64 = 5_000;

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
    /// The receiver's jitter buffer for this target, in ms — the `sess.latency.msec`
    /// the daemon told its agent to use (`sync_settings::pwsink_jitter_effective`).
    ///
    /// The sender never sets this on the far end; it is told what it is, because both
    /// of its own backlog limits have to respect it: a catch-up burst must fit that
    /// buffer, and a backlog worth keeping must be one that buffer can absorb. See
    /// [`BacklogLimits::for_playout`].
    pub playout_ms: u16,
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
///
/// **Feed it over a bounded channel** (`sync_channel`, as `pwsink_server` does). A
/// queue of undelivered PCM is a queue of late audio, so the producer drops instead of
/// growing it; `rtp_sender_loop` bounds its own end the same way ([`BacklogLimits`]).
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
        let SessionConfig { session_name, control_port, ifname: _ifname, format, playout_ms, advertise_daemon } = config;
        // Both backlog limits follow the far end's buffer, so they are settled once
        // here rather than guessed per packet.
        let limits = BacklogLimits::for_playout(playout_ms);
        let data_port =
            control_port.checked_add(1).ok_or_else(|| anyhow::anyhow!("control_port {control_port} leaves no room for the data port"))?;

        // 2. Bind the control + data UDP ports on IPv4 (the receiver initiates
        //    to these advertised ports; bind v4 so single-host ipv4 sessions
        //    work — see the spike gotchas).
        let ctrl_sock = UdpSocket::bind(("0.0.0.0", control_port)).map_err(|e| anyhow::anyhow!("bind control port {control_port}: {e}"))?;
        let data_sock = UdpSocket::bind(("0.0.0.0", data_port)).map_err(|e| anyhow::anyhow!("bind data port {data_port}: {e}"))?;
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
        // Named + RT-scheduled inside the loop (see `set_sender_realtime_priority`);
        // the two readers above stay ordinary threads, being control-only.
        threads.push(
            std::thread::Builder::new()
                .name("pwsink-rtp".into())
                .spawn({
                    let peers = peers.clone();
                    let shutdown = shutdown.clone();
                    move || rtp_sender_loop(pcm, peers, shutdown, format, limits)
                })
                .map_err(|e| anyhow::anyhow!("failed to spawn the pw-sink RTP sender thread: {e}"))?,
        );

        Ok(Self { status, shutdown, peers, ctrl_sock: ctrl_keep, data_sock: data_keep, threads, mdns })
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

/// Best-effort real-time scheduling for the RTP send thread.
///
/// It wakes every [`PACKET_MS`] to put packets on the wire, so it belongs on the same
/// ladder as the rest of this path: the capture runs at FIFO 45, the capture→feed relay
/// at 40, PipeWire's data loop at 83. Priority 50 matches the AP2 path's `rt-sender`,
/// which does the same job.
///
/// Without `CAP_SYS_NICE` it logs and continues at normal priority, as the capture and
/// relay do; the catch-up logic in [`rtp_sender_loop`] is what keeps that survivable
/// rather than lossy.
fn set_sender_realtime_priority() {
    #[cfg(target_os = "linux")]
    // SAFETY: sched_setscheduler on the current thread (pid 0) with a valid,
    // zero-initialised sched_param; no aliasing, no ownership transfer.
    unsafe {
        let mut param: libc::sched_param = std::mem::zeroed();
        param.sched_priority = 50;
        if libc::sched_setscheduler(0, libc::SCHED_FIFO, &param) == 0 {
            tracing::info!("pwsink RTP sender: real-time priority set (SCHED_FIFO, priority 50)");
        } else {
            tracing::warn!(
                "pwsink RTP sender: could not set RT priority (need CAP_SYS_NICE); running at normal priority — \
                 expect catch-up bursts, and dropouts if the host is loaded"
            );
        }
    }
}

/// How many of the **oldest** queued samples to discard, or 0 while the backlog is
/// within bounds. Fires only past `max_backlog_ms`
/// ([`BacklogLimits::max_backlog_ms`]), and then cuts all the way back to
/// [`TARGET_BACKLOG_MS`] rather than just under the ceiling — a stall that reached
/// the ceiling will reach it again in a few packets otherwise, turning one click
/// into a stutter.
fn overflow_drop(queued_samples: usize, samples_per_ms: usize, max_backlog_ms: i64) -> usize {
    let samples_per_ms = samples_per_ms.max(1);
    if (queued_samples / samples_per_ms) as i64 <= max_backlog_ms {
        return 0;
    }
    queued_samples.saturating_sub(TARGET_BACKLOG_MS.max(0) as usize * samples_per_ms)
}

/// The interval to wait before the next packet: the nominal packet time, trimmed
/// proportionally to the backlog's distance from [`TARGET_BACKLOG_MS`].
///
/// Above target → shorter interval (drain faster); below → longer (let it refill).
/// The trim is capped at [`PACE_TRIM_PPM_MAX`], and the result is never allowed to
/// reach zero, which would spin this thread instead of pacing it.
fn paced_dt_ns(nominal_dt_ns: i64, queued_samples: usize, samples_per_ms: usize) -> i64 {
    let error_ms = (queued_samples / samples_per_ms.max(1)) as i64 - TARGET_BACKLOG_MS;
    let ppm = (error_ms * PACE_TRIM_PPM_PER_MS).clamp(-PACE_TRIM_PPM_MAX, PACE_TRIM_PPM_MAX);
    (nominal_dt_ns - (nominal_dt_ns * ppm) / 1_000_000).max(1)
}

/// RTP sender loop: drain native-endian S16 PCM, repacketize into ~2 ms L16
/// (big-endian) RTP packets, and stream to every established data peer, paced
/// to real time. Returns when `pcm` disconnects (audio stops) or on shutdown.
///
/// # Rate-matching, not just pacing
/// The PCM arrives on PipeWire's graph clock while the pace is kept against this
/// thread's monotonic clock. Any difference between the two accumulates inside `buf` as
/// delay — 100 ppm is ~360 ms per hour — so the pace is trimmed by up to ±0.5 % to hold
/// the backlog at [`TARGET_BACKLOG_MS`], converging the send rate on the producer's.
/// Sending marginally fast or slow is safe: the receiving `module-rtp-session` has an
/// adaptive resampler and a jitter buffer for it, and that buffer is what the
/// playout-delay knob sets.
///
/// [`BacklogLimits`] handles what the trim cannot: a scheduling gap is caught up within
/// one wakeup, and a gap too large for the receiver's buffer is dropped rather than
/// carried as permanent delay.
fn rtp_sender_loop(
    pcm: Receiver<PcmChunk>,
    peers: Arc<PeerState>,
    shutdown: Arc<AtomicBool>,
    format: SessionFormat,
    limits: BacklogLimits,
) {
    set_sender_realtime_priority();
    let send_sock = match UdpSocket::bind(("0.0.0.0", 0)) {
        Ok(s) => s,
        Err(_) => return,
    };
    let channels = format.channels.max(1) as usize;
    let frames_per_pkt = (format.rate / (1000 / PACKET_MS)).max(1) as usize; // PACKET_MS of audio
    let samples_per_pkt = frames_per_pkt * channels;
    let nominal_dt_ns = ((frames_per_pkt as u64 * 1_000_000_000) / format.rate.max(1) as u64) as i64;
    // Samples per ms of audio, for reading `buf` as a duration.
    let samples_per_ms = ((format.rate.max(1) as usize * channels) / 1000).max(1);

    let mut buf: Vec<i16> = Vec::with_capacity(samples_per_pkt * 4);
    let mut seq: u16 = 0;
    let mut ts: u32 = 0;
    let mut next = Instant::now();
    // Trim bookkeeping: how much audio the hard backstop has thrown away, and when
    // it was last reported (a stall produces a burst of trims, and one line per
    // burst is the useful signal).
    let mut trimmed_ms: u64 = 0;
    let mut last_trim_log = Instant::now();
    // Catch-up bookkeeping: extra packets emitted because a wakeup came late. Not a
    // fault — this is the mechanism working — so it is reported at debug, and only
    // as a rate, to tell "the host is late but we cope" apart from the drops above.
    let mut caught_up: usize = 0;
    let mut last_burst_log = Instant::now();

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

        // Accumulate at least one packet's worth of samples. Anything the feed has
        // ready is taken in the same pass (`try_recv`), so the backlog measured
        // below is the true one rather than one chunk of it — the trim can only act
        // on what it can see.
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
        while let Ok(chunk) = pcm.try_recv() {
            buf.extend_from_slice(&chunk);
        }

        // Hard backstop: a stall left more audio queued than any playout buffer
        // should carry, so drop the oldest down to target. `ts` is deliberately NOT
        // advanced past the discarded samples — the receiver must see one unbroken
        // 2 ms-per-packet timeline (that is how it keeps playing), and advancing it
        // would announce a gap it would then re-buffer, which is the delay we are
        // dropping audio to get rid of.
        let drop_samples = overflow_drop(buf.len(), samples_per_ms, limits.max_backlog_ms);
        if drop_samples > 0 {
            buf.drain(..drop_samples);
            trimmed_ms += (drop_samples / samples_per_ms) as u64;
        }
        if trimmed_ms > 0 && last_trim_log.elapsed() >= Duration::from_secs(5) {
            tracing::warn!(
                "pw-sink session '{}': dropped {} ms of audio to keep the send backlog bounded \
                 (it had grown past {} ms). Something starved this thread — check host CPU; the \
                 audio is otherwise intact and no latency was carried forward.",
                peers.session_name,
                trimmed_ms,
                limits.max_backlog_ms,
            );
            trimmed_ms = 0;
            last_trim_log = Instant::now();
        }

        // Emit every packet whose deadline has already passed, not just one.
        //
        // One packet per wakeup would couple throughput to the *wakeup rate*: a thread
        // scheduled 200×/s could deliver only 200 × PACKET_MS of audio per second
        // whatever it was fed, and the shortfall would surface as either growing delay
        // or continuous dropping depending on the queue policy. Sending all due packets
        // decouples the two — one wakeup puts 30 ms on the wire as easily as 5 — so a
        // late wakeup costs a burst the receiver's jitter buffer absorbs.
        // `limits.burst_packets` keeps that burst inside the buffer; past
        // `limits.max_backlog_ms`, `overflow_drop` above is the answer.
        let mut burst = 0usize;
        while buf.len() >= samples_per_pkt {
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
            burst += 1;

            // Rate-matched pace: nominal packet interval, trimmed proportionally to
            // how far the backlog sits from target. Above target we send slightly
            // sooner, below it slightly later, so the send rate settles on whatever
            // rate the capture actually produces and the backlog stops drifting.
            next += Duration::from_nanos(paced_dt_ns(nominal_dt_ns, buf.len(), samples_per_ms) as u64);
            let now = Instant::now();
            if next > now {
                std::thread::sleep(next - now);
                break; // caught up: back to the top for more PCM
            }
            if burst >= limits.burst_packets {
                // Still behind. Break to re-read the feed rather than hog the CPU we
                // just got back — but leave `next` in the past on purpose, so the
                // next pass carries straight on catching up. Resetting it here would
                // abandon the rest of the debt to sit in the queue as delay until
                // `overflow_drop` eventually threw it away.
                break;
            }
            // else: the next deadline is already in the past — send it now.
        }
        if burst > 1 {
            caught_up += burst - 1;
        }
        if caught_up > 0 && last_burst_log.elapsed() >= Duration::from_secs(30) {
            tracing::debug!(
                "pw-sink session '{}': sent {} extra packet(s) in catch-up bursts over the last 30 s \
                 (late wakeups, absorbed without dropping audio)",
                peers.session_name,
                caught_up,
            );
            caught_up = 0;
            last_burst_log = Instant::now();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::sync_channel;

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

    /// 48 kHz stereo: 96 samples per ms of audio.
    const SPMS: usize = 96;

    /// The default 100 ms receiver buffer, for the limits under test.
    const DEFAULT_LIMITS: BacklogLimits = BacklogLimits { burst_packets: 13, max_backlog_ms: 195 };

    #[test]
    fn the_backlog_is_left_alone_until_it_passes_the_ceiling() {
        let ceiling = DEFAULT_LIMITS.max_backlog_ms;
        assert_eq!(overflow_drop(0, SPMS, ceiling), 0);
        assert_eq!(overflow_drop(TARGET_BACKLOG_MS as usize * SPMS, SPMS, ceiling), 0);
        // Exactly at the ceiling is still fine — only *past* it is a problem.
        assert_eq!(overflow_drop(ceiling as usize * SPMS, SPMS, ceiling), 0);
    }

    #[test]
    fn overflow_cuts_all_the_way_back_to_target() {
        // A 500 ms stall: everything above target goes, so what is left is target.
        let queued = 500 * SPMS;
        let dropped = overflow_drop(queued, SPMS, DEFAULT_LIMITS.max_backlog_ms);
        assert_eq!((queued - dropped) / SPMS, TARGET_BACKLOG_MS as usize);
        // …and that is nearly the whole stall, not a token slice of it.
        assert_eq!(dropped / SPMS, 500 - TARGET_BACKLOG_MS as usize);
    }

    /// The invariant that the stutter came from: a burst the loop is *about* to send
    /// must never be larger than the backlog it is allowed to hold, or the drop throws
    /// away audio that was already on its way out.
    #[test]
    fn the_ceiling_always_leaves_room_for_a_full_burst() {
        for playout_ms in [4u16, 10, 20, 50, 100, 250, 500, 2000] {
            let l = BacklogLimits::for_playout(playout_ms);
            let burst_ms = (l.burst_packets as i64) * i64::from(PACKET_MS);
            assert!(
                l.max_backlog_ms > burst_ms,
                "playout {playout_ms}: ceiling {} must exceed one burst of {burst_ms} ms",
                l.max_backlog_ms
            );
        }
    }

    /// A burst has to fit the far end's jitter buffer, so it scales with it — and
    /// stays inside it, since a burst that exactly filled the buffer would leave the
    /// receiver no headroom for the network.
    #[test]
    fn the_burst_scales_with_the_receivers_buffer_and_stays_inside_it() {
        let small = BacklogLimits::for_playout(20);
        let default = BacklogLimits::for_playout(100);
        let large = BacklogLimits::for_playout(600);
        assert!(small.burst_packets < default.burst_packets);
        assert!(default.burst_packets < large.burst_packets);

        // Only across what the API permits (>= PWSINK_JITTER_MIN_MS): below that the
        // two-packet floor *is* the whole buffer, which is exactly why that minimum
        // is three packet times.
        for playout_ms in [crate::sync_settings::PWSINK_JITTER_MIN_MS, 20, 50, 100, 250] {
            let l = BacklogLimits::for_playout(playout_ms);
            let burst_ms = (l.burst_packets as i64) * i64::from(PACKET_MS);
            assert!(burst_ms < i64::from(playout_ms), "playout {playout_ms}: burst {burst_ms} ms must fit inside it");
        }
        // Clamps hold at both extremes: never useless, never unbounded.
        assert_eq!(BacklogLimits::for_playout(0).burst_packets, MIN_BURST_PACKETS);
        assert_eq!(BacklogLimits::for_playout(u16::MAX).burst_packets, MAX_BURST_PACKETS);
    }

    #[test]
    fn the_pace_speeds_up_when_the_backlog_grows_and_slows_when_it_shrinks() {
        let nominal = 2_000_000; // 2 ms in ns
        let at_target = paced_dt_ns(nominal, TARGET_BACKLOG_MS as usize * SPMS, SPMS);
        assert_eq!(at_target, nominal, "no error, no trim");

        let behind = paced_dt_ns(nominal, 40 * SPMS, SPMS);
        assert!(behind < nominal, "a large backlog must send sooner, got {behind}");
        let ahead = paced_dt_ns(nominal, 0, SPMS);
        assert!(ahead > nominal, "an empty buffer must wait longer, got {ahead}");
    }

    /// The trim exists to cancel clock drift, not to resample: it must stay small
    /// enough that the receiver's own resampler absorbs it without artefacts.
    #[test]
    fn the_pace_trim_is_capped_at_half_a_percent() {
        let nominal = 2_000_000;
        let saturated = paced_dt_ns(nominal, 10_000 * SPMS, SPMS); // absurd backlog
        assert_eq!(saturated, nominal - (nominal * PACE_TRIM_PPM_MAX) / 1_000_000);
        assert!(saturated >= nominal * 995 / 1000);
        // Never zero, whatever the arithmetic does — a zero interval spins the CPU.
        assert!(paced_dt_ns(1, 10_000 * SPMS, SPMS) >= 1);
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
            playout_ms: 100,        // the module's own default, as a real target would run
            advertise_daemon: None, // standalone: create our own mDNS daemon.
        };

        // Bounded like the real feed (pwsink_server::PCM_FEED_DEPTH), so the spike
        // exercises the same drop-instead-of-grow shape the daemon runs.
        let (tx, rx) = sync_channel::<PcmChunk>(8);
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
