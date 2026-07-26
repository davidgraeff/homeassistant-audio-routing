//! Active-probe presence + demotion for AirPlay-2 receivers.
//!
//! `ap2_discovery` (mDNS) only ever *adds* receivers and sets `present = true`; it
//! deliberately ignores mDNS-removed events (a TTL flap on a power-saving receiver
//! is not proof it left). This task owns the offline decision and eventual removal,
//! mirroring `sendspin_liveness`: each tick it TCP-probes every device's advertised
//! RTSP endpoint (`:7000`) — a receiver with an active session still has that port
//! listening, so probing never disturbs playback. A device failing a few
//! consecutive ticks is demoted to offline (grayed in the matrix; the reconciler
//! then drops it from its group and tears its sender down); one that stays offline
//! past a grace window is removed from the registry and its PTP peer released.
//!
//! Demotion/promotion nudges the reconciler via `ChangeNotifier` so a powered-off
//! receiver's sender is torn down and a returning one is re-established.

use crate::ap2_discovery::SharedAp2Devices;
use crate::ap2_ptp::SharedAp2Ptp;
use crate::locks::LockRecover;
use crate::pw_thread::ChangeNotifier;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

/// How often to re-evaluate every receiver's liveness.
const PROBE_INTERVAL: Duration = Duration::from_secs(12);
/// Per-probe TCP connect timeout.
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);
/// Consecutive failed ticks before a receiver is demoted to offline.
const FAIL_THRESHOLD: u32 = 3;
/// How long a receiver may stay offline before it's removed from the registry
/// (and its PTP peer released).
const REMOVE_AFTER: Duration = Duration::from_secs(300);

pub fn spawn(devices: SharedAp2Devices, ptp: SharedAp2Ptp, changes: ChangeNotifier) {
    tokio::spawn(async move {
        let mut fails: HashMap<String, u32> = HashMap::new();
        let mut offline_since: HashMap<String, Instant> = HashMap::new();
        loop {
            tokio::time::sleep(PROBE_INTERVAL).await;

            // Snapshot (node_name, addr, present) without holding the std lock
            // across the awaits below.
            let candidates: Vec<(String, Option<SocketAddr>, bool)> = {
                let devs = devices.lock_recover();
                devs.iter().map(|(name, d)| (name.clone(), d.addr, d.present)).collect()
            };

            let mut notify = false;
            // (node_name, peer_ip) for devices past the removal grace window.
            let mut to_remove: Vec<(String, Option<String>)> = Vec::new();

            for (node_name, addr, was_present) in candidates {
                let alive = match addr {
                    Some(addr) => tcp_alive(addr).await,
                    None => false, // nothing to probe until an IPv4 address resolves
                };

                if alive {
                    fails.remove(&node_name);
                    offline_since.remove(&node_name);
                    if !was_present {
                        set_present(&devices, &node_name, true);
                        tracing::info!("AirPlay-2 receiver back online: {node_name}");
                        notify = true;
                    }
                } else {
                    let f = fails.entry(node_name.clone()).or_insert(0);
                    *f += 1;
                    if *f >= FAIL_THRESHOLD {
                        let since = *offline_since.entry(node_name.clone()).or_insert_with(Instant::now);
                        if was_present {
                            set_present(&devices, &node_name, false);
                            tracing::info!("AirPlay-2 receiver offline (probe failing): {node_name}");
                            notify = true;
                        }
                        if since.elapsed() >= REMOVE_AFTER {
                            to_remove.push((node_name.clone(), addr.map(|a| a.ip().to_string())));
                        }
                    }
                }
            }

            if !to_remove.is_empty() {
                {
                    let mut devs = devices.lock_recover();
                    for (name, _) in &to_remove {
                        devs.remove(name);
                        fails.remove(name);
                        offline_since.remove(name);
                        tracing::info!("AirPlay-2 receiver removed after staying offline: {name}");
                    }
                }
                // Release the PTP peer for each removed receiver (outside the
                // devices lock; add_peer/remove_peer take the PTP service's own lock).
                for (_, peer_ip) in &to_remove {
                    if let Some(ip) = peer_ip {
                        ptp.remove_peer(ip);
                    }
                }
                notify = true;
            }

            if notify {
                let _ = changes.send(());
            }
        }
    });
}

fn set_present(devices: &SharedAp2Devices, node_name: &str, present: bool) {
    if let Some(d) = devices.lock_recover().get_mut(node_name) {
        d.present = present;
    }
}

/// A short TCP connect to `addr` — reachable = alive. Avoids ICMP (needs
/// raw-socket privileges the add-on container lacks); the AirPlay RTSP port stays
/// open during a session, so this never disturbs an in-flight stream.
async fn tcp_alive(addr: SocketAddr) -> bool {
    matches!(tokio::time::timeout(PROBE_TIMEOUT, tokio::net::TcpStream::connect(addr)).await, Ok(Ok(_)))
}
