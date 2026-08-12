//! Connection-driven presence + active-probe fallback for sendspin devices.
//!
//! mDNS (sendspin_discovery.rs) only ever *adds* devices; this task owns the
//! online/offline flag and eventual removal, so a flapping mDNS record (TTL
//! expiry on a WiFi-power-saving speaker) never tears down a live group.
//!
//! Each tick, per device: it's present if it has a live server connection
//! (the sendspin volume control tracks connected devices) OR a short TCP
//! connect to its advertised address succeeds. A device with neither, for a
//! few consecutive ticks, is demoted to offline (grayed in the matrix); one
//! that stays offline past a grace window is removed from the registry — so a
//! genuinely-gone device still disappears, just not on a transient blip.

use crate::pw::thread::ChangeNotifier;
use crate::sendspin_discovery::SharedSendspinDevices;
use crate::sendspin_volume::SharedSendspinControl;
use crate::util::locks::LockRecover;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

/// How often to re-evaluate every device's liveness.
const PROBE_INTERVAL: Duration = Duration::from_secs(12);
/// Per-probe TCP connect timeout.
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);
/// Consecutive failed ticks (no connection, probe failing) before a device is
/// demoted to offline in the matrix.
const FAIL_THRESHOLD: u32 = 3;
/// How long a device may stay offline before it's removed from the registry.
const REMOVE_AFTER: Duration = Duration::from_secs(300);

pub fn spawn(devices: SharedSendspinDevices, control: SharedSendspinControl, changes: ChangeNotifier) {
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
            let mut to_remove: Vec<String> = Vec::new();

            for (node_name, addr, was_present) in candidates {
                // Connection-driven: a live server connection is proof of life,
                // no probe needed (and avoids probing a device mid-stream).
                let connected = control.lock().await.is_connected(&node_name);
                let alive = if connected {
                    true
                } else if let Some(addr) = addr {
                    tcp_alive(addr).await
                } else {
                    false // nothing to probe until an IPv4 address resolves
                };

                if alive {
                    fails.remove(&node_name);
                    offline_since.remove(&node_name);
                    if !was_present {
                        set_present(&devices, &node_name, true);
                        tracing::info!("sendspin device back online: {node_name}");
                        notify = true;
                    }
                } else {
                    let f = fails.entry(node_name.clone()).or_insert(0);
                    *f += 1;
                    if *f >= FAIL_THRESHOLD {
                        let since = *offline_since.entry(node_name.clone()).or_insert_with(Instant::now);
                        if was_present {
                            set_present(&devices, &node_name, false);
                            tracing::info!("sendspin device offline (no connection, probe failing): {node_name}");
                            notify = true;
                        }
                        if since.elapsed() >= REMOVE_AFTER {
                            to_remove.push(node_name.clone());
                        }
                    }
                }
            }

            if !to_remove.is_empty() {
                let mut devs = devices.lock_recover();
                for name in &to_remove {
                    devs.remove(name);
                    fails.remove(name);
                    offline_since.remove(name);
                    tracing::info!("sendspin device removed after staying offline: {name}");
                }
                drop(devs);
                notify = true;
            }

            if notify {
                let _ = changes.send(());
            }
        }
    });
}

fn set_present(devices: &SharedSendspinDevices, node_name: &str, present: bool) {
    if let Some(d) = devices.lock_recover().get_mut(node_name) {
        d.present = present;
    }
}

/// A short TCP connect to `addr` — reachable = alive. Avoids ICMP (which needs
/// raw-socket privileges the add-on container lacks) and needs nothing beyond
/// the mDNS-advertised address.
async fn tcp_alive(addr: SocketAddr) -> bool {
    matches!(tokio::time::timeout(PROBE_TIMEOUT, tokio::net::TcpStream::connect(addr)).await, Ok(Ok(_)))
}
