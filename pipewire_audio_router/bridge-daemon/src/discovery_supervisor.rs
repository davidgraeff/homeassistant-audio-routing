//! Runtime on/off for mDNS auto-discovery (sendspin + AirPlay-2 + pw-sink
//! targets + Bluetooth→RTP bridges).
//!
//! A **single** shared `ServiceDaemon` lives here behind a shared mutex and both
//! browsers (outputs/sendspin/discovery.rs + ap2_discovery.rs) run their `browse()` on
//! it — one `mDNS_daemon` OS thread for the whole daemon instead of one per
//! service type. `start()` builds the daemon (restricted to the LAN interface —
//! see `lan_restricted_daemon`) and spawns the two worker threads; `stop()`
//! calls `daemon.shutdown()`, which disconnects every browse receiver so both
//! loops exit cleanly (they end their `while let Ok(..)`). Dropping the handle
//! alone would *not* stop the daemon — its run loop only exits on an explicit
//! shutdown — so `stop()` must call `shutdown()`.
//!
//! Disabling only stops discovering *new* devices; anything already present is
//! left to age out through its normal path (sendspin/AP2 via liveness) so a
//! toggle never tears down live groups.

use crate::ap2_discovery::{self, SharedAp2Devices};
use crate::ap2_ptp::SharedAp2Ptp;
use crate::outputs::sendspin;
use crate::outputs::sendspin::discovery::SharedSendspinDevices;
use crate::pw::thread::ChangeNotifier;
use crate::util::locks::LockRecover;
use mdns_sd::{IfKind, ServiceDaemon};
use std::net::{IpAddr, Ipv4Addr, UdpSocket};
use std::sync::{Arc, Mutex, OnceLock};

/// The primary LAN IPv4 of this host — the source address the kernel picks for
/// its default route. With `host_network: true` the container also sees every
/// Docker bridge (`hassio`, ~11 `veth*`, `docker0`) plus IPv6 link-local
/// interfaces; binding mDNS to *all* of them turns one badly-behaved LAN device
/// (e.g. an old AirPlay projector advertising `_airplay._tcp` with no resolvable
/// address) into a self-amplifying query storm — each multicast query is echoed
/// out ~10 Docker veths and received back, pegging the CPU. Restricting the
/// daemon to this one interface kills both the amplification and the IPv6/AAAA
/// path (the entire observed storm was AAAA queries).
///
/// The UDP-`connect` trick sends no packets: it only makes the kernel choose the
/// outbound interface for that destination, which — because Docker bridge routes
/// are more specific and never the default — is the real LAN interface.
fn primary_lan_ipv4() -> Option<Ipv4Addr> {
    let sock = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    sock.connect((Ipv4Addr::new(8, 8, 8, 8), 53)).ok()?;
    match sock.local_addr().ok()?.ip() {
        IpAddr::V4(v4) if !v4.is_loopback() && !v4.is_unspecified() => Some(v4),
        _ => None,
    }
}

/// Pin `daemon` to the primary LAN interface (see `primary_lan_ipv4`). Disables
/// every interface, then re-enables only the LAN one; mdns-sd applies selections
/// in order, so exactly one IPv4 interface stays bound (Docker veths + all IPv6
/// excluded) and the sockets on the rest are closed — so the daemon neither
/// sends nor receives the multicast flood there. `label` names the daemon in the
/// log line. Best-effort: on the fallback path it at least drops IPv6.
fn restrict_to_lan(daemon: &ServiceDaemon, label: &str) {
    match primary_lan_ipv4() {
        Some(lan) => {
            let _ = daemon.disable_interface(IfKind::All);
            let _ = daemon.enable_interface(IfKind::Addr(IpAddr::V4(lan)));
            tracing::info!("{label} restricted to LAN interface {lan}");
        }
        None => {
            let _ = daemon.disable_interface(IfKind::IPv6);
            tracing::warn!("could not determine primary LAN IPv4; {label} left on all IPv4 interfaces (IPv6 disabled)");
        }
    }
}

/// Build the single shared mDNS **browse** daemon, restricted to the primary LAN
/// interface. All three discovery browsers share it.
fn lan_restricted_daemon() -> anyhow::Result<ServiceDaemon> {
    let daemon = ServiceDaemon::new()?;
    restrict_to_lan(&daemon, "mDNS discovery");
    Ok(daemon)
}

/// One process-wide mDNS daemon shared by everything outside the toggleable
/// browse daemon, restricted to the LAN interface. It backs the sendspin server +
/// AirPlay-receiver (shairplay) **advertisements** ([`crate::sendspin_server`],
/// [`crate::airplay_source`]) *and* the sendspin server's dial-in
/// **`ClientManager` browse** — all via the vendored crates' `with_daemon`
/// constructors. Distinct from the toggleable browse daemon above: these must
/// outlive discovery on/off toggles, so this daemon is never shut down (each
/// user unregisters its own service / stops its own browse on drop; the idle
/// daemon costs one thread). Created lazily on first use.
///
/// Sharing one daemon here — instead of one per sendspin group and one per
/// shairplay start — keeps a single `mDNS_daemon` thread for all advertisements
/// and, because it's pinned to the LAN interface, avoids the multicast
/// amplification a `host_network` container otherwise suffers across its Docker
/// `veth`s. Returns `None` (callers fall back to a private daemon) only if the
/// daemon can't be created. All crates now share mdns-sd 0.20, so this single
/// `ServiceDaemon` type is injectable into both vendored crates' `with_daemon`
/// constructors.
pub(crate) fn shared_advertise_daemon() -> Option<ServiceDaemon> {
    static DAEMON: OnceLock<Option<ServiceDaemon>> = OnceLock::new();
    DAEMON
        .get_or_init(|| match ServiceDaemon::new() {
            Ok(daemon) => {
                restrict_to_lan(&daemon, "mDNS advertisements");
                Some(daemon)
            }
            Err(e) => {
                tracing::warn!("shared advertise daemon start failed ({e}); advertisers will each create their own");
                None
            }
        })
        .clone()
}

struct Inner {
    /// The single shared mDNS daemon (sendspin + AirPlay-2 browsers both run on
    /// it) while discovery is running; `None` when off.
    running: Option<ServiceDaemon>,
    // Inputs kept so discovery can be (re)spawned on demand.
    devices: SharedSendspinDevices,
    /// Discovered AirPlay-2 receivers (ap2_discovery.rs).
    ap2_devices: SharedAp2Devices,
    /// Host-global AP2 PTP grandmaster — discovery registers each receiver as a peer.
    ap2_ptp: SharedAp2Ptp,
    /// Discovered pw-sink targets (pw_target_discovery.rs).
    pw_targets: crate::pw_target_discovery::SharedPwTargets,
    /// Discovered Bluetooth→RTP bridges (sources/bt_bridge.rs). Unlike the
    /// others these are not outputs and build no audio path — they annotate RTP
    /// *sources* with their sender's identity and diagnostics page.
    bt_bridges: crate::sources::bt_bridge::SharedBtBridges,
    changes: ChangeNotifier,
}

#[derive(Clone)]
pub struct DiscoverySupervisor(Arc<Mutex<Inner>>);

impl DiscoverySupervisor {
    pub fn new(
        devices: SharedSendspinDevices,
        ap2_devices: SharedAp2Devices,
        ap2_ptp: SharedAp2Ptp,
        pw_targets: crate::pw_target_discovery::SharedPwTargets,
        bt_bridges: crate::sources::bt_bridge::SharedBtBridges,
        changes: ChangeNotifier,
    ) -> Self {
        Self(Arc::new(Mutex::new(Inner { running: None, devices, ap2_devices, ap2_ptp, pw_targets, bt_bridges, changes })))
    }

    pub fn is_running(&self) -> bool {
        self.0.lock_recover().running.is_some()
    }

    /// Spawn both discovery browsers on one shared LAN-restricted daemon if not
    /// already running. Idempotent.
    pub fn start(&self) -> anyhow::Result<()> {
        let mut inner = self.0.lock_recover();
        if inner.running.is_some() {
            return Ok(());
        }
        let daemon = lan_restricted_daemon()?;
        // If any browser fails to spawn, shut the daemon down so we don't leak
        // its OS thread (the run loop only exits on an explicit shutdown).
        let spawned = (|| -> anyhow::Result<()> {
            sendspin::discovery::spawn(&daemon, inner.devices.clone(), inner.changes.clone())?;
            ap2_discovery::spawn(&daemon, inner.ap2_devices.clone(), inner.changes.clone(), inner.ap2_ptp.clone())?;
            crate::pw_target_discovery::spawn(&daemon, inner.pw_targets.clone(), inner.changes.clone())?;
            crate::sources::bt_bridge::spawn(&daemon, inner.bt_bridges.clone(), inner.changes.clone())?;
            Ok(())
        })();
        if let Err(e) = spawned {
            let _ = daemon.shutdown();
            return Err(e);
        }
        inner.running = Some(daemon);
        tracing::info!("mDNS discovery started");
        Ok(())
    }

    /// Shut the shared mDNS daemon down, ending both discovery threads.
    /// Idempotent.
    pub fn stop(&self) {
        let mut inner = self.0.lock_recover();
        if let Some(daemon) = inner.running.take() {
            // `shutdown()` sends the daemon's run loop a `Command::Exit`; it
            // drops every browse sender, so all four worker loops disconnect
            // and exit. Best-effort — a shutdown error just means the daemon
            // was already gone.
            let _ = daemon.shutdown();
            tracing::info!("mDNS discovery stopped");
        }
    }

    /// Apply a desired on/off state; returns any spawn error from `start()`.
    pub fn set_enabled(&self, enabled: bool) -> anyhow::Result<()> {
        if enabled {
            self.start()
        } else {
            self.stop();
            Ok(())
        }
    }
}
