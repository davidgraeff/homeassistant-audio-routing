//! Keeps every **multicast** RTP source ingesting when the stock
//! `libpipewire-module-rtp-source` silently drops its IGMP group membership.
//! One keepalive socket + watcher runs per multicast RTP `SourceEntry`, keyed by
//! the source's PipeWire node name; the watcher set is reconciled against the
//! stored sources each tick.
//!
//! Observed failure (2026-07-27, live): with the source configured for a
//! multicast group (`rtp.source.ip = 239.255.42.42`), after the sender pauses
//! long enough for the module's session to time out, the module **leaves the
//! multicast group and never rejoins** — even once the sender resumes. The
//! packets still reach the host NIC, but with no group membership the kernel
//! drops them before the module's socket, so `bt-bridge-rtp` sits idle
//! (QUANT 0, meter dead) and audio is silently gone until the module is
//! reloaded. We don't own the module's socket, so we can't re-`IP_ADD_MEMBERSHIP`
//! into it; the only lever is a targeted module reload.
//!
//! This watchdog:
//! 1. Holds its **own** UDP socket joined to the group. That keeps the *host*
//!    subscribed (so packets keep arriving on the NIC regardless of the
//!    module's state) and lets us see, cheaply, whether audio is arriving right
//!    now — the "was there sound coming in seconds ago" signal.
//! 2. Reads `/proc/net/igmp`: when the module node is loaded and audio is
//!    actively arriving **but the group's joined-socket count is only our
//!    keepalive** (the module has dropped its join), it reloads the module so
//!    it rejoins. By construction this fires **only** in the broken state — a
//!    healthy streaming module keeps its join (count ≥ 2), and an idle group
//!    (no recent packets) is left alone — so it never interrupts good audio and
//!    doesn't churn while nothing is playing.
//!
//! Unicast sources need none of this (no group membership to lose), so the
//! watchdog no-ops unless `source_addr` is a multicast group. The durable
//! alternative is to run this one-to-one path as unicast; this watchdog makes
//! the multicast mode self-healing for installs that want to keep it.

use crate::api::SharedSources;
use crate::locks::LockRecover;
use crate::pw_thread::{PwCommandSender, SharedState};
use crate::rtp_source;
use crate::sources_store::{RtpSourceConfig, SourceConfig};
use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::time::{Duration, Instant};

/// How often to poll membership + drain the keepalive socket.
const TICK: Duration = Duration::from_secs(5);
/// A packet seen within this window counts as "audio is arriving now" — the
/// gate that keeps us from reloading an idle (legitimately unjoined) source.
const RECENT_ACTIVITY: Duration = Duration::from_secs(8);
/// Don't reload more often than this — a reload takes a moment to re-establish
/// the join, so back off rather than hammer if the first attempt hasn't taken
/// effect by the next tick.
const MIN_RELOAD_INTERVAL: Duration = Duration::from_secs(20);

/// Sum of the "Users" (joined-socket) counts for `group` across all interfaces
/// in a `/proc/net/igmp` dump, or `None` if the group appears on no interface.
///
/// `/proc/net/igmp` prints each group's address as a native-endian hex `u32`;
/// on the little-endian targets this runs on (aarch64 HAOS, x86 dev box) that
/// is the octets reversed — e.g. `239.255.42.42` → `2A2AFFEF`. Group rows are
/// indented and start with that hex address followed by the Users count;
/// interface header rows are not indented and start with a device index, so
/// keying on the address column ignores them.
pub fn group_users(igmp: &str, group: Ipv4Addr) -> Option<u32> {
    let want = le_hex(group);
    let mut total = 0u32;
    let mut found = false;
    for line in igmp.lines() {
        let mut cols = line.split_whitespace();
        let (Some(addr), Some(users)) = (cols.next(), cols.next()) else { continue };
        if addr.eq_ignore_ascii_case(&want) {
            if let Ok(u) = users.parse::<u32>() {
                total += u;
                found = true;
            }
        }
    }
    found.then_some(total)
}

/// The `/proc/net/igmp` hex spelling of an IPv4 address on a little-endian host
/// (octets reversed).
fn le_hex(ip: Ipv4Addr) -> String {
    let o = ip.octets();
    format!("{:02X}{:02X}{:02X}{:02X}", o[3], o[2], o[1], o[0])
}

/// A UDP socket bound to the RTP port and joined to the multicast group, kept
/// open for the group's lifetime. Owns a raw fd; closes it on drop.
struct Keepalive {
    fd: i32,
}

impl Keepalive {
    /// Bind `0.0.0.0:port` (with `SO_REUSEADDR`/`SO_REUSEPORT` so it coexists
    /// with the module's own receiver socket) and `IP_ADD_MEMBERSHIP` the group
    /// on the default multicast interface (`INADDR_ANY`, same as the module).
    /// Non-blocking so `drain` never stalls the watchdog.
    fn join(group: Ipv4Addr, port: u16) -> std::io::Result<Self> {
        // SAFETY: standard POSIX socket setup; every fallible libc call is
        // checked and the fd is closed on any error before returning.
        unsafe {
            let fd = libc::socket(libc::AF_INET, libc::SOCK_DGRAM, 0);
            if fd < 0 {
                return Err(std::io::Error::last_os_error());
            }
            let this = Keepalive { fd }; // takes ownership → fd closed on early return

            let one: libc::c_int = 1;
            for opt in [libc::SO_REUSEADDR, libc::SO_REUSEPORT] {
                if libc::setsockopt(
                    fd,
                    libc::SOL_SOCKET,
                    opt,
                    &one as *const _ as *const libc::c_void,
                    std::mem::size_of::<libc::c_int>() as libc::socklen_t,
                ) < 0
                {
                    return Err(std::io::Error::last_os_error());
                }
            }

            let addr = libc::sockaddr_in {
                sin_family: libc::AF_INET as libc::sa_family_t,
                sin_port: port.to_be(),
                sin_addr: libc::in_addr { s_addr: libc::INADDR_ANY },
                sin_zero: [0; 8],
            };
            if libc::bind(fd, &addr as *const _ as *const libc::sockaddr, std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t) < 0 {
                return Err(std::io::Error::last_os_error());
            }

            // s_addr wants the address in network byte order; the octets already
            // are network order, so from_ne_bytes preserves their memory layout.
            let mreq = libc::ip_mreq {
                imr_multiaddr: libc::in_addr { s_addr: u32::from_ne_bytes(group.octets()) },
                imr_interface: libc::in_addr { s_addr: libc::INADDR_ANY },
            };
            if libc::setsockopt(
                fd,
                libc::IPPROTO_IP,
                libc::IP_ADD_MEMBERSHIP,
                &mreq as *const _ as *const libc::c_void,
                std::mem::size_of::<libc::ip_mreq>() as libc::socklen_t,
            ) < 0
            {
                return Err(std::io::Error::last_os_error());
            }

            let flags = libc::fcntl(fd, libc::F_GETFL);
            if flags < 0 || libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) < 0 {
                return Err(std::io::Error::last_os_error());
            }

            Ok(this)
        }
    }

    /// Drain every queued datagram (non-blocking); return whether any arrived.
    /// We only care that packets are flowing, not their contents.
    fn drain(&self) -> bool {
        let mut buf = [0u8; 2048];
        let mut got = false;
        loop {
            // SAFETY: valid fd and buffer; a negative return is EWOULDBLOCK/EAGAIN
            // (nothing left) or a transient error — either way we stop draining.
            let n = unsafe { libc::recv(self.fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len(), 0) };
            if n > 0 {
                got = true;
            } else {
                break;
            }
        }
        got
    }
}

impl Drop for Keepalive {
    fn drop(&mut self) {
        // SAFETY: fd is owned by this struct and closed exactly once.
        unsafe {
            libc::close(self.fd);
        }
    }
}

/// The one place that decides, from the observable signals, whether the module
/// is stuck (audio arriving on the group but the module not joined) and should
/// be reloaded. Pure so it can be unit-tested without a live graph.
///
/// - `module_loaded`: the `bt-bridge-rtp` node is present in the registry.
/// - `igmp_users`: joined-socket count for the group (includes our keepalive).
/// - `audio_recent`: our keepalive saw a packet within [`RECENT_ACTIVITY`].
/// - `cooling`: a reload happened within [`MIN_RELOAD_INTERVAL`].
///
/// Reload iff the module is loaded, audio is arriving now, we're not cooling
/// down, and the only joined socket is our own keepalive (`users < 2`) — i.e.
/// the module has dropped its join.
fn should_reload(module_loaded: bool, igmp_users: u32, audio_recent: bool, cooling: bool) -> bool {
    module_loaded && audio_recent && !cooling && igmp_users < 2
}

/// Per-source watchdog state: the keepalive socket joined to this source's group
/// plus the two timers `should_reload` gates on. Keyed by the source's PipeWire
/// node name so several multicast RTP sources are watched independently.
struct Watcher {
    /// The multicast group this watcher's keepalive is joined to; a change here
    /// (or in `port`) forces the socket to be rebuilt.
    group: Ipv4Addr,
    /// The UDP port the keepalive is bound to.
    port: u16,
    /// Our own group-joined socket (keeps the host subscribed + samples traffic).
    sock: Keepalive,
    /// When the keepalive last saw a packet — the "audio arriving now" signal.
    last_rx: Option<Instant>,
    /// When we last reloaded this source's module — the cooldown gate.
    last_reload: Option<Instant>,
}

/// Spawn the membership watchdog. Cheap: one 5 s timer, one datagram socket per
/// *multicast* RTP source, and a single `/proc/net/igmp` read per tick shared
/// across sources. Each tick it reconciles its per-source watcher map against
/// the stored RTP sources (adding watchers for new multicast sources, dropping
/// them for removed/unicast/disabled ones) and reloads any specific module that
/// is stuck (audio arriving on its group but the module unjoined). No-op while
/// no multicast RTP source is configured.
pub fn spawn(pw: SharedState, pw_cmd: PwCommandSender, sources: SharedSources) {
    tokio::spawn(async move {
        // node name -> watcher. Recreated for a source whose group/port changes.
        let mut watchers: HashMap<String, Watcher> = HashMap::new();
        let mut ticker = tokio::time::interval(TICK);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            ticker.tick().await;

            // Desired watchers: one per *multicast* RTP source, keyed by node
            // name. Unicast/disabled sources contribute nothing (no group to
            // watch). Snapshot under the std lock, then drop it before awaiting.
            let desired: HashMap<String, (Ipv4Addr, RtpSourceConfig)> = {
                let store = sources.lock_recover();
                store
                    .list()
                    .into_iter()
                    .filter_map(|e| {
                        let node_name = e.node_name();
                        match e.config {
                            SourceConfig::Rtp(cfg) => {
                                let group = cfg.source_addr.parse::<Ipv4Addr>().ok().filter(Ipv4Addr::is_multicast)?;
                                Some((node_name, (group, cfg)))
                            }
                            SourceConfig::Airplay(_) => None,
                        }
                    })
                    .collect()
            };

            // Drop watchers whose source was removed or is no longer multicast.
            watchers.retain(|name, _| desired.contains_key(name));

            // One shared read of the membership table per tick.
            let igmp = std::fs::read_to_string("/proc/net/igmp").unwrap_or_default();

            for (node_name, (group, cfg)) in &desired {
                // (Re)establish this source's keepalive if new or group/port changed.
                let stale = !matches!(watchers.get(node_name), Some(w) if w.group == *group && w.port == cfg.port);
                if stale {
                    match Keepalive::join(*group, cfg.port) {
                        Ok(sock) => {
                            tracing::info!("rtp membership watchdog: joined {group}:{} for '{node_name}' (keepalive)", cfg.port);
                            // Preserve the cooldown across a socket rebuild so a
                            // config edit doesn't immediately re-reload.
                            let last_reload = watchers.get(node_name).and_then(|w| w.last_reload);
                            watchers.insert(node_name.clone(), Watcher { group: *group, port: cfg.port, sock, last_rx: None, last_reload });
                        }
                        Err(e) => {
                            tracing::warn!("rtp membership watchdog: could not join {group}:{} for '{node_name}': {e}", cfg.port);
                            watchers.remove(node_name);
                            continue;
                        }
                    }
                }
                let Some(w) = watchers.get_mut(node_name) else { continue };

                if w.sock.drain() {
                    w.last_rx = Some(Instant::now());
                }

                let module_loaded = pw.lock_recover().nodes.values().any(|n| &n.node_name == node_name);
                let users = group_users(&igmp, *group).unwrap_or(0);
                let audio_recent = w.last_rx.is_some_and(|t| t.elapsed() < RECENT_ACTIVITY);
                let cooling = w.last_reload.is_some_and(|t| t.elapsed() < MIN_RELOAD_INTERVAL);

                if should_reload(module_loaded, users, audio_recent, cooling) {
                    tracing::warn!(
                        "RTP multicast {group}: audio arriving but module '{node_name}' dropped its group join (igmp users={users}); reloading to rejoin"
                    );
                    if let Err(e) = rtp_source::reload(&pw_cmd, node_name, cfg).await {
                        tracing::warn!("rtp membership watchdog: reload of '{node_name}' failed: {e}");
                    }
                    w.last_reload = Some(Instant::now());
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    // A representative /proc/net/igmp dump (indented group rows under an
    // interface header), matching what we read live on the HA host.
    const IGMP: &str = "\
Idx\tDevice    : Count Querier\tGroup    Users Timer\tReporter
1\tlo        :     1      V3
\t\t\t\t010000E0     1 0:00000000\t\t0
2\tend0      :     5      V3
\t\t\t\t2A2AFFEF     2 0:00000000\t\t0
\t\t\t\t010000E0     1 0:00000000\t\t0";

    #[test]
    fn le_hex_matches_proc_spelling() {
        assert_eq!(le_hex(Ipv4Addr::new(239, 255, 42, 42)), "2A2AFFEF");
        assert_eq!(le_hex(Ipv4Addr::new(224, 0, 0, 1)), "010000E0");
    }

    #[test]
    fn group_users_sums_and_detects_absence() {
        // 239.255.42.42 joined by 2 sockets on end0.
        assert_eq!(group_users(IGMP, Ipv4Addr::new(239, 255, 42, 42)), Some(2));
        // 224.0.0.1 appears on both lo and end0 → summed.
        assert_eq!(group_users(IGMP, Ipv4Addr::new(224, 0, 0, 1)), Some(2));
        // A group nobody joined.
        assert_eq!(group_users(IGMP, Ipv4Addr::new(239, 1, 2, 3)), None);
    }

    #[test]
    fn reload_only_when_stuck_with_live_audio() {
        // Healthy: module + our keepalive both joined (users=2) → never reload.
        assert!(!should_reload(true, 2, true, false));
        // Stuck: audio arriving, only our keepalive joined (users=1) → reload.
        assert!(should_reload(true, 1, true, false));
        // Group entirely gone (users=0) with audio arriving → reload.
        assert!(should_reload(true, 0, true, false));
        // Idle: nobody streaming (no recent audio) → leave it alone, no churn.
        assert!(!should_reload(true, 1, false, false));
        // Cooling down after a recent reload → wait.
        assert!(!should_reload(true, 1, true, true));
        // Module not even loaded → nothing to reload.
        assert!(!should_reload(false, 0, true, false));
    }
}
