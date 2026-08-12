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
//!
//! ## The PTP-lock watchdog
//!
//! The same tick also watches for the failure a `GET /info` probe cannot see: a
//! receiver that is reachable, holds a healthy RTSP session, is being sent audio — and
//! renders **nothing**, because it lost its lock on our PTP grandmaster. Our PT=87
//! anchors are timestamps in the grandmaster's timeline, so a receiver whose slaved
//! clock has drifted away from it has no idea when to play them. Observed repeatedly on
//! a Pioneer VSX-934, where the only recoveries were restarting the add-on or
//! power-cycling the AVR: both work because both build a new session.
//!
//! Two rules keep it from firing on a healthy system:
//!
//! * **Only receivers that have demonstrated a lock.** A Yamaha WX-021 in this house
//!   never sends a `Delay_Req` at all and plays perfectly — its lock age is hours.
//!   "Advertises PTP" is a capability; "is locked" is a runtime fact, and only a
//!   receiver that *was* locked and stopped can have lost it.
//! * **Only while it is supposed to be streaming**, with a cooldown between attempts.
//!   A reconnect costs that one receiver a few seconds of audio, so it must never
//!   become a loop against a receiver that simply will not lock.

use crate::outputs::ap2::discovery::SharedAp2Devices;
use crate::outputs::ap2::health::Ap2Health;
use crate::outputs::ap2::probe::{probe, Ap2Reach};
use crate::outputs::ap2::ptp::SharedAp2Ptp;
use crate::outputs::ap2::volume::SharedAp2Control;
use crate::pw::thread::ChangeNotifier;
use crate::util::locks::LockRecover;
use std::collections::{HashMap, HashSet};
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

/// A gPTP message this recently from a receiver counts as **locked**: it is slaving to
/// our grandmaster right now. Matches the badge `/api/outputs` shows
/// (`outputs/listing.rs`), so the UI and the watchdog cannot disagree about what
/// "locked" means. A locked receiver exchanges `Delay_Req`/`Delay_Resp` continuously,
/// so this is generous rather than tight.
const PTP_LOCK_FRESH: Duration = Duration::from_secs(5);

/// Silence this long from a receiver that **was** locked is treated as lock lost. Six
/// times [`PTP_LOCK_FRESH`] on purpose: the badge may flicker on one slow exchange,
/// while a *recovery* costs that receiver its audio for a few seconds and so must only
/// answer a fault that is real.
const PTP_LOCK_LOST_AFTER: Duration = Duration::from_secs(30);

/// Minimum time between automatic reconnects of the same receiver. A receiver that
/// cannot re-lock must cost one attempt every two minutes, not one per tick — and the
/// fault note stays visible in between, which is what tells the user it is still wrong.
const PTP_RECOVER_COOLDOWN: Duration = Duration::from_secs(120);

/// Should the watchdog rebuild this receiver's session? Pure, because every condition
/// here exists to stop it firing on a working system and each one is worth pinning:
///
/// * `age` — how long since libairptp heard gPTP from it (`None` = never).
/// * `ever_locked` — it has been seen locked since the daemon started. A receiver that
///   never locks (and plays fine, like the Yamaha here) can't have *lost* a lock.
/// * `streaming` — it has a live sender. With no session there is nothing to rebuild.
/// * `since_last_recover` — `None` when we have not tried before.
fn ptp_recovery_due(age: Option<Duration>, ever_locked: bool, streaming: bool, since_last_recover: Option<Duration>) -> bool {
    ever_locked
        && streaming
        && age.is_none_or(|a| a >= PTP_LOCK_LOST_AFTER)
        && since_last_recover.is_none_or(|since| since >= PTP_RECOVER_COOLDOWN)
}

pub fn spawn(devices: SharedAp2Devices, ptp: SharedAp2Ptp, ap2_control: SharedAp2Control, changes: ChangeNotifier) {
    tokio::spawn(async move {
        let mut fails: HashMap<String, u32> = HashMap::new();
        let mut offline_since: HashMap<String, Instant> = HashMap::new();
        // Receivers seen PTP-locked at least once since the daemon started — the set the
        // watchdog is allowed to act on (see the module docs).
        let mut ever_locked: HashSet<String> = HashSet::new();
        // When each receiver was last reconnected by the watchdog, for the cooldown.
        let mut last_recover: HashMap<String, Instant> = HashMap::new();
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
            // Which receivers have a live sender right now: the watchdog below only acts
            // on those, since a receiver with no session has nothing to rebuild (and the
            // reconciler is already responsible for giving it one).
            let streaming = ap2_control.lock().await.connected();

            for (node_name, addr, was_present) in candidates {
                let reach = match addr {
                    Some(addr) => probe(addr, PROBE_TIMEOUT, REPLY_TIMEOUT).await,
                    // Nothing to probe until an IPv4 address resolves.
                    None => Ap2Reach::Unreachable("no resolved IPv4 address".into()),
                };

                if reach == Ap2Reach::Alive {
                    fails.remove(&node_name);
                    offline_since.remove(&node_name);
                    if !was_present {
                        set_present(&devices, &node_name, true);
                        tracing::info!("AirPlay-2 receiver back online: {node_name}");
                        notify = true;
                    }
                    // The fault a probe cannot see: reachable, session up, and not
                    // rendering because it lost the grandmaster. Recover it by rebuilding
                    // just its session — the automatic form of the Resync button.
                    //
                    // Evaluated BEFORE clearing the health note, because the probe
                    // answering is exactly what this fault looks like: clearing first
                    // would publish "lost its clock lock" and then wipe it 12 s later,
                    // leaving the UI green for a receiver that is playing nothing.
                    let mut ptp_fault: Option<String> = None;
                    if let Some(ip) = addr.map(|a| a.ip().to_string()) {
                        let age = ptp.peer_lock_age(&ip);
                        if age.is_some_and(|a| a <= PTP_LOCK_FRESH) {
                            // Locked now: remember it, and forget any past attempt so a
                            // receiver that flaps twice a day isn't rate-limited by
                            // yesterday's recovery.
                            if ever_locked.insert(node_name.clone()) {
                                tracing::debug!("AP2 PTP: '{node_name}' ({ip}) is locked to the grandmaster");
                            }
                            last_recover.remove(&node_name);
                        } else if ever_locked.contains(&node_name) && streaming.contains(&node_name) {
                            // A lock it had, gone, while we are still streaming to it. The
                            // note stands for as long as that is true; the *action* is rate
                            // limited, so the two are decided separately.
                            let age_note = match age {
                                Some(a) => format!("last heard {}s ago", a.as_secs()),
                                None => "never heard from since it was added as a peer".to_string(),
                            };
                            ptp_fault = Some(format!("Lost its PTP clock lock ({age_note}) — rebuilding its session"));
                            if ptp_recovery_due(age, true, true, last_recover.get(&node_name).map(Instant::elapsed)) {
                                tracing::warn!(
                                    "AP2 PTP: '{node_name}' ({ip}) has lost its clock lock ({age_note}) while streaming — \
                                     rebuilding its session, which is what an add-on restart used to do for you"
                                );
                                last_recover.insert(node_name.clone(), Instant::now());
                                if !ap2_control.lock().await.reconnect(&node_name, "its PTP clock lock was lost") {
                                    tracing::warn!("AP2 PTP: could not reach '{node_name}''s sender task to ask for a reconnect");
                                }
                            }
                        }
                    }
                    let health_changed = match &ptp_fault {
                        Some(msg) => Ap2Health::global().set(&node_name, msg.clone()),
                        // Reachable and (as far as we can tell) locked: nothing is wrong,
                        // so drop whatever the last failure left behind.
                        None => Ap2Health::global().clear(&node_name),
                    };
                    if health_changed {
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
                        // A receiver that left takes its lock history with it: if it comes
                        // back it must prove a lock again before the watchdog acts on it.
                        ever_locked.remove(name);
                        last_recover.remove(name);
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The receiver this exists for: a Pioneer VSX-934 that was locked, is still being
    /// streamed to, and has gone quiet on gPTP. That combination renders silence and
    /// nothing else in the daemon notices it.
    #[test]
    fn a_streaming_receiver_that_lost_a_lock_it_had_is_recovered() {
        assert!(ptp_recovery_due(Some(Duration::from_secs(45)), true, true, None));
        // "Never heard from" counts as lost too, once it *had* been locked — that is a
        // peer whose gPTP stopped entirely rather than merely slowed.
        assert!(ptp_recovery_due(None, true, true, None));
        // And again after the cooldown, because a receiver can fail to re-lock.
        assert!(ptp_recovery_due(None, true, true, Some(PTP_RECOVER_COOLDOWN)));
    }

    /// The three guards, each of which would otherwise reconnect a healthy receiver.
    /// The first is the important one: a Yamaha WX-021 in this house never sends a
    /// `Delay_Req` at all and plays perfectly, so "no recent gPTP" on its own is not a
    /// fault — it is that model's normal state (docs/sendspin-open-items.md, and the
    /// reason the badge is a runtime fact rather than a capability).
    #[test]
    fn a_receiver_that_never_locked_is_left_alone() {
        assert!(!ptp_recovery_due(None, false, true, None), "a receiver that never locks has nothing to lose");
        assert!(!ptp_recovery_due(Some(Duration::from_secs(3600)), false, true, None));
        // Not streaming ⇒ no session to rebuild; the reconciler owns that case.
        assert!(!ptp_recovery_due(None, true, false, None));
        // Still locked ⇒ nothing wrong. (The caller only reaches here when the age is
        // past PTP_LOCK_FRESH, but the rule must hold on its own.)
        assert!(!ptp_recovery_due(Some(Duration::from_secs(1)), true, true, None));
    }

    /// A receiver that cannot re-lock must cost one attempt per cooldown, not one per
    /// 12-second tick: each attempt takes its audio away for a few seconds, so a loop
    /// would be worse than the fault.
    #[test]
    fn recovery_is_rate_limited_per_receiver() {
        assert!(!ptp_recovery_due(None, true, true, Some(Duration::from_secs(1))));
        assert!(!ptp_recovery_due(None, true, true, Some(PTP_RECOVER_COOLDOWN - Duration::from_secs(1))));
        assert!(ptp_recovery_due(None, true, true, Some(PTP_RECOVER_COOLDOWN + Duration::from_secs(1))));
    }
}
