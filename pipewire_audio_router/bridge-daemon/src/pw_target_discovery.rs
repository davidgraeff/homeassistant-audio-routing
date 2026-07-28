//! mDNS discovery of **pw-sink targets** — remote PipeWire hosts running
//! `libpipewire-module-rtp-session`, which advertise an RTP **audio session**
//! over mDNS as `_pipewire-audio._udp.local.` (see
//! docs/pipewire-sink-spike-results.md). Unlike the earlier `_workstation._tcp`
//! sketch (a generic "any Linux host" signal that needed a manual approval step
//! to weed out false positives), `_pipewire-audio._udp` is advertised *only* by
//! a host actually set up as an RTP-session receiver — a strong enough signal
//! that targets are directly routable, exactly like discovered sendspin devices
//! (sendspin_discovery.rs) and AirPlay-2 receivers (ap2_discovery.rs). So there
//! is no approval flow and no store: browse, surface, route.
//!
//! Like sendspin/ap2 discovery, a discovered target does **not** get a PipeWire
//! node here — it is a *virtual* routing output `pwsink-dev-<slug>`; the audio
//! path (a per-target AppleMIDI/RTP sender fed from the group anchor's monitor —
//! pwsink_server.rs) is built by the grouping reconciler (sync_group.rs) from the
//! routing intent.
//!
//! ## Filtering our own adverts
//! The daemon itself advertises `_pipewire-audio._udp` sessions (one per routed
//! target, `PWSINK_SESSION_PREFIX` = `pwrouter-`; plus the dev spike
//! `pw-audio-router-spike`) so receivers can discover *us*. Those must not come
//! back as targets, so any instance whose label starts with the session prefix
//! (or is the spike session) is skipped.

use crate::config::{slugify, PWSINK_DEV_PREFIX, PWSINK_SESSION_PREFIX};
use crate::locks::LockRecover;
use crate::pw_thread::ChangeNotifier;
use mdns_sd::{ResolvedService, ServiceDaemon, ServiceEvent};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

/// The mDNS service type `module-rtp-session` uses for audio sessions.
const PIPEWIRE_AUDIO_SERVICE_TYPE: &str = "_pipewire-audio._udp.local.";

/// The dev-spike session name (pw_sink_spike.rs) — one of our own adverts to skip.
const SPIKE_SESSION_NAME: &str = "pw-audio-router-spike";

/// One discovered pw-sink target host.
#[derive(Debug, Clone)]
pub struct PwTarget {
    /// mDNS instance fullname (stable identity).
    pub fullname: String,
    /// Human display name (the session/host label).
    pub display_name: String,
    /// Resolved IPv4 address (informational — the UI shows it; the audio path
    /// does not dial it, since the *receiver* initiates the AppleMIDI handshake
    /// to our advertised session). `None` until resolved.
    pub addr: Option<std::net::IpAddr>,
    /// mDNS presence (a resolve sets it `true`; TTL-flap removes are ignored —
    /// liveness is really carried by the sender handshake, pwsink_liveness.rs).
    pub present: bool,
}

/// Live discovered targets, keyed by virtual output node name
/// (`pwsink-dev-<slug>`). Shared with the API and the grouping reconciler.
pub type SharedPwTargets = Arc<Mutex<BTreeMap<String, PwTarget>>>;

/// Virtual output node name for a discovered host.
pub fn target_node_name(display_name: &str) -> String {
    format!("{PWSINK_DEV_PREFIX}{}", slugify(display_name))
}

fn addr_from_service(info: &ResolvedService) -> Option<std::net::IpAddr> {
    info.get_addresses_v4().into_iter().next().map(std::net::IpAddr::V4)
}

/// Session/host label from the mDNS instance fullname
/// (`fedora._pipewire-audio._udp.local.` -> `fedora`).
fn display_name_from_service(info: &ResolvedService) -> String {
    let fullname = info.get_fullname();
    fullname.split("._pipewire-audio._udp").next().unwrap_or(fullname).trim().to_string()
}

/// True if `label` is one of the daemon's own advertised sessions (so discovery
/// skips it rather than treating us as a target).
fn is_own_advert(label: &str) -> bool {
    label.starts_with(PWSINK_SESSION_PREFIX) || label == SPIKE_SESSION_NAME
}

/// Start browsing `_pipewire-audio._udp.local.` on the shared mDNS `daemon`,
/// keeping `targets` in sync. Our own advertised sessions are filtered out; every
/// other resolved session is surfaced as a directly-routable target. Mirrors
/// sendspin_discovery::spawn / ap2_discovery::spawn.
pub fn spawn(daemon: &ServiceDaemon, targets: SharedPwTargets, changes: ChangeNotifier) -> anyhow::Result<()> {
    let receiver = daemon.browse(PIPEWIRE_AUDIO_SERVICE_TYPE)?;
    std::thread::Builder::new().name("pwsink-discovery".into()).spawn(move || {
        while let Ok(event) = receiver.recv() {
            if let ServiceEvent::ServiceResolved(info) = event {
                let display_name = display_name_from_service(&info);
                if display_name.is_empty() || is_own_advert(&display_name) {
                    continue;
                }
                let node_name = target_node_name(&display_name);
                let fullname = info.get_fullname().to_string();
                let addr = addr_from_service(&info);

                let mut tgts = targets.lock_recover();
                let notify = match tgts.get_mut(&node_name) {
                    Some(t) => {
                        let came_online = !t.present;
                        t.present = true;
                        t.fullname = fullname;
                        if addr.is_some() {
                            t.addr = addr;
                        }
                        came_online
                    }
                    None => {
                        tgts.insert(
                            node_name.clone(),
                            PwTarget { fullname, display_name: display_name.clone(), addr, present: true },
                        );
                        tracing::info!("discovered pw-sink target '{display_name}' ({node_name})");
                        true
                    }
                };
                drop(tgts);
                if notify {
                    let _ = changes.send(());
                }
            }
            // ServiceRemoved ignored (TTL flap ≠ gone), same as sendspin/ap2.
        }
        tracing::info!("pwsink discovery loop ended");
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_node_name_uses_prefix_and_slug() {
        assert_eq!(target_node_name("david-local"), "pwsink-dev-david_local");
    }

    #[test]
    fn own_adverts_are_filtered() {
        assert!(is_own_advert("pwrouter-living_room"));
        assert!(is_own_advert(SPIKE_SESSION_NAME));
        assert!(!is_own_advert("fedora"));
        assert!(!is_own_advert("some-other-host"));
    }
}
