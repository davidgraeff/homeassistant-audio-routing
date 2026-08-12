//! Advert-driven presence + demotion for **pw-sink targets**.
//!
//! mDNS (pw_target_discovery.rs) only ever *adds* targets; this task owns the
//! online/offline flag and eventual removal, so a discovered host stops being
//! listed as a live output once it's gone. Without it a target stayed
//! `present: true` for the daemon's lifetime — the routing graph then showed a
//! powered-off host as an online speaker and animated its wire.
//!
//! **Why no active probe** (unlike outputs/sendspin/liveness.rs / outputs/ap2/liveness.rs): a
//! pw-sink target exposes nothing of ours to connect to. The audio path is
//! *receiver*-initiated — the remote `module-rtp-session` dials the session we
//! advertise — so there is no port on the target to TCP-probe, and probing the host
//! generally (some closed port) would only tell us the machine answers, not that a
//! receiver is available. So presence is judged from the two signals that do mean
//! something:
//!
//! - the **advert**: `ServiceRemoved` (a goodbye on clean shutdown / module unload,
//!   or SRV expiry ~2 min after the host stops answering) timestamps a withdrawal
//!   in `PwTarget::withdrawn_since`; a fresh resolve clears it.
//! - the **session**: an established AppleMIDI handshake (pw_sink_liveness.rs) is
//!   proof of life that outranks the advert — a receiver streaming from us is
//!   online no matter what mDNS currently says (and this is what keeps a host whose
//!   zeroconf publishing is off, but whose rtp-session runs, from being demoted).
//!
//! Note the split this preserves: `present` here is *reachability*, while
//! "a receiver has actually attached" stays pw_sink_liveness.rs's `established`
//! (surfaced as `streaming` in the routing matrix and `pwsink_streaming` on
//! `/api/outputs`). A host that is up but has no session is honestly reported as
//! present-but-not-streaming rather than either "offline" or "playing".

use crate::pw::thread::ChangeNotifier;
use crate::pw_sink_liveness::PwSinkLiveness;
use crate::pw_target_discovery::SharedPwTargets;
use crate::util::locks::LockRecover;
use std::time::{Duration, Instant};

/// How often to re-evaluate every target.
const CHECK_INTERVAL: Duration = Duration::from_secs(12);
/// How long an advert may stay withdrawn before the target is demoted to offline
/// (grayed in the matrix). Long enough to ride out an mDNS flap, short enough that
/// a host that really went away stops being offered as a live output.
const WITHDRAWN_GRACE: Duration = Duration::from_secs(45);
/// How long a target may stay offline before it's dropped from the registry.
/// Matches sendspin/AP2 liveness; an adopted target still shows (grayed) from the
/// outputs store, so its routing survives.
const REMOVE_AFTER: Duration = Duration::from_secs(300);

/// What to do with one target this tick.
#[derive(Debug, PartialEq, Eq)]
enum Verdict {
    /// Leave it as it is.
    Keep,
    /// Flip `present` to this value (only ever returned when it differs).
    SetPresent(bool),
    /// Drop it from the registry.
    Remove,
}

/// The decision for one target, from its advert state + whether a receiver is
/// attached. `withdrawn_for` is how long its advert has been gone (`None` = it is
/// advertised right now).
fn verdict(present: bool, established: bool, withdrawn_for: Option<Duration>) -> Verdict {
    // An attached receiver outranks the advert: it is streaming, so it is here.
    if established {
        return if present { Verdict::Keep } else { Verdict::SetPresent(true) };
    }
    let Some(gone) = withdrawn_for else {
        // Advertised and not attached: present (reachable), not streaming.
        return if present { Verdict::Keep } else { Verdict::SetPresent(true) };
    };
    if gone >= REMOVE_AFTER {
        return Verdict::Remove;
    }
    if gone >= WITHDRAWN_GRACE && present {
        return Verdict::SetPresent(false);
    }
    Verdict::Keep
}

pub fn spawn(targets: SharedPwTargets, changes: ChangeNotifier) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(CHECK_INTERVAL).await;
            let now = Instant::now();
            let mut notify = false;

            // One lock for the whole pass: every input is either in the registry or
            // a cheap process-global read, so there is nothing to await here.
            let mut tgts = targets.lock_recover();
            let mut to_remove: Vec<String> = Vec::new();
            for (node_name, t) in tgts.iter_mut() {
                let established = PwSinkLiveness::global().get(node_name).is_some_and(|s| s.established);
                let withdrawn_for = t.withdrawn_since.map(|since| now.saturating_duration_since(since));
                match verdict(t.present, established, withdrawn_for) {
                    Verdict::Keep => {}
                    Verdict::SetPresent(present) => {
                        t.present = present;
                        if present {
                            tracing::info!("pw-sink target back online: {node_name}");
                        } else {
                            tracing::info!("pw-sink target offline (advert withdrawn, no session): {node_name}");
                        }
                        notify = true;
                    }
                    Verdict::Remove => to_remove.push(node_name.clone()),
                }
            }
            for name in &to_remove {
                tgts.remove(name);
                tracing::info!("pw-sink target removed after staying offline: {name}");
                notify = true;
            }
            drop(tgts);

            if notify {
                let _ = changes.send(());
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn advertised_target_stays_present() {
        assert_eq!(verdict(true, false, None), Verdict::Keep);
        assert_eq!(verdict(false, false, None), Verdict::SetPresent(true));
    }

    /// The whole point of the grace window: a TTL flap on a live target must not
    /// gray it, and must not gray it at all while a receiver is attached.
    #[test]
    fn a_short_withdrawal_is_ignored() {
        assert_eq!(verdict(true, false, Some(Duration::from_secs(10))), Verdict::Keep);
    }

    #[test]
    fn a_streaming_target_survives_any_withdrawal() {
        assert_eq!(verdict(true, true, Some(REMOVE_AFTER * 2)), Verdict::Keep);
        // ...and comes back online if it was demoted before the session came up.
        assert_eq!(verdict(false, true, Some(REMOVE_AFTER * 2)), Verdict::SetPresent(true));
    }

    #[test]
    fn a_lasting_withdrawal_demotes_then_removes() {
        assert_eq!(verdict(true, false, Some(WITHDRAWN_GRACE)), Verdict::SetPresent(false));
        assert_eq!(verdict(false, false, Some(WITHDRAWN_GRACE)), Verdict::Keep);
        assert_eq!(verdict(false, false, Some(REMOVE_AFTER)), Verdict::Remove);
    }
}
