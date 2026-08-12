//! Active-probe presence + demotion for AirPlay-2 receivers.
//!
//! `ap2_discovery` (mDNS) only ever *adds* receivers and sets `present = true`; it
//! deliberately ignores mDNS-removed events (a TTL flap on a power-saving receiver
//! is not proof it left). This task owns the offline decision and eventual removal,
//! mirroring `sendspin_liveness`: each tick it probes every device's advertised
//! RTSP endpoint (`:7000`) with a real `GET /info` round-trip
//! ([`crate::ap2_probe`]) — a receiver with an active session serves `/info` on a
//! second connection by design, so probing never disturbs playback. A device
//! failing a few consecutive ticks is demoted to offline (grayed in the matrix; the
//! reconciler then drops it from its group and tears its sender down).
//!
//! **Removal is only for receivers that are actually gone.** A device that
//! completes a TCP handshake but never answers ([`Ap2Reach::RtspSilent`] — a wedged
//! AirTunes, see [`crate::ap2_probe`]) is demoted but *kept* in the registry: it is
//! still on the network and still advertising, so removing it would only flap
//! against the next mDNS resolve, and the user needs the entry to stay put to see
//! why it isn't playing. Only [`Ap2Reach::Unreachable`] — nothing listening at all
//! — leads to removal and releasing the PTP peer.
//!
//! Demotion/promotion nudges the reconciler via `ChangeNotifier` so a powered-off
//! receiver's sender is torn down and a returning one is re-established, and every
//! verdict is published to [`crate::ap2_health`] so `/api/outputs` can say *why*
//! an output isn't usable instead of leaving it silently green.

use crate::outputs::ap2::discovery::SharedAp2Devices;
use crate::outputs::ap2::health::Ap2Health;
use crate::outputs::ap2::probe::{probe, Ap2Reach};
use crate::outputs::ap2::ptp::SharedAp2Ptp;
use crate::pw::thread::ChangeNotifier;
use crate::util::locks::LockRecover;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

/// How often to re-evaluate every receiver's liveness.
const PROBE_INTERVAL: Duration = Duration::from_secs(12);
/// Per-probe TCP connect timeout.
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);
/// How long a receiver gets to answer `GET /info` once connected. Generous next to
/// the connect bound: a busy receiver mid-session is still expected to answer in
/// milliseconds, but a loaded one on wifi shouldn't be demoted for a slow reply.
const REPLY_TIMEOUT: Duration = Duration::from_secs(4);
/// Consecutive failed ticks before a receiver is demoted to offline.
const FAIL_THRESHOLD: u32 = 3;
/// How long an *unreachable* receiver may stay offline before it's removed from the
/// registry (and its PTP peer released). Never applies to an answering-but-wedged
/// one — see the module docs.
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
                let reach = match addr {
                    Some(addr) => probe(addr, PROBE_TIMEOUT, REPLY_TIMEOUT).await,
                    // Nothing to probe until an IPv4 address resolves.
                    None => Ap2Reach::Unreachable("no resolved IPv4 address".into()),
                };

                if reach == Ap2Reach::Alive {
                    fails.remove(&node_name);
                    offline_since.remove(&node_name);
                    if Ap2Health::global().clear(&node_name) {
                        notify = true;
                    }
                    if !was_present {
                        set_present(&devices, &node_name, true);
                        tracing::info!("AirPlay-2 receiver back online: {node_name}");
                        notify = true;
                    }
                } else {
                    let f = fails.entry(node_name.clone()).or_insert(0);
                    *f += 1;
                    if *f >= FAIL_THRESHOLD {
                        // Publish the reason before demoting, so the UI has something
                        // to show the instant it sees `present: false`. `set` returns
                        // false while the fault is unchanged — that's what keeps this
                        // from logging every 12 s for a receiver that stays broken.
                        if let Some(msg) = reach.fault_message() {
                            if Ap2Health::global().set(&node_name, msg.clone()) {
                                tracing::warn!("AirPlay-2 receiver '{node_name}' unusable: {msg}");
                                notify = true;
                            }
                        }
                        let since = *offline_since.entry(node_name.clone()).or_insert_with(Instant::now);
                        if was_present {
                            set_present(&devices, &node_name, false);
                            tracing::info!("AirPlay-2 receiver offline (probe failing): {node_name}");
                            notify = true;
                        }
                        // Only forget a receiver that is genuinely gone. One that
                        // answers TCP but not RTSP is still here — keep the entry (and
                        // its fault message) rather than flapping against mDNS.
                        let gone = matches!(reach, Ap2Reach::Unreachable(_));
                        if gone && since.elapsed() >= REMOVE_AFTER {
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
                        // The entry is gone, so its fault message has nothing left to
                        // annotate.
                        Ap2Health::global().clear(name);
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
