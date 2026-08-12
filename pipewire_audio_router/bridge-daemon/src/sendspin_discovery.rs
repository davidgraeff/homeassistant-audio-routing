//! mDNS/DNS-SD auto-discovery of sendspin devices on the LAN.
//!
//! Sendspin speakers that run their own embedded server (e.g. ESPHome's
//! `sendspin:` component on Home Assistant Voice PE) advertise themselves on
//! `_sendspin._tcp.local.` and must be *dialed* by a server — see
//! sendspin-rs's `server/discovery.rs`. This module browses that service type
//! (directly via mdns-sd, so we get add **and** remove events — sendspin-rs's
//! own `ClientBrowser` only surfaces resolved services) and maintains a shared
//! registry of live devices.
//!
//! Unlike RAOP discovery (discovery.rs), a discovered sendspin device does
//! **not** get its own PipeWire sink loaded here. It's surfaced as a *virtual*
//! routing output (`sendspin-dev-<slug>`); the actual audio path — one sink +
//! one synchronized sendspin `Group` per set of co-routed devices — is built
//! by the grouping reconciler (sync_group.rs) from the routing intent.
//! This mirrors RAOP's "devices appear in the matrix automatically" UX while
//! honoring sendspin's group-based multi-room model.

use crate::pw::thread::ChangeNotifier;
use crate::util::locks::LockRecover;
use crate::util::node_names::{slugify, SENDSPIN_DEV_PREFIX};
use mdns_sd::{ResolvedService, ServiceDaemon, ServiceEvent};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

/// Matches sendspin-rs's `CLIENT_SERVICE_TYPE` — what a device that runs its
/// own embedded server advertises for itself.
const SENDSPIN_SERVICE_TYPE: &str = "_sendspin._tcp.local.";

/// One discovered device. `fullname` is the stable mDNS instance identity used
/// to filter the group `ClientManager` to exactly the devices in a group.
#[derive(Debug, Clone)]
pub struct SendspinDevice {
    pub fullname: String,
    pub display_name: String,
    /// Resolved server address, used by the liveness probe when the device has
    /// no live connection (sendspin_liveness.rs). `None` until mDNS resolves an
    /// IPv4 address.
    pub addr: Option<std::net::SocketAddr>,
    /// Liveness. mDNS only ever sets this `true` (on resolve) and never removes
    /// the device — an mDNS "removed" is a TTL-expiry flap, not proof the
    /// device left. The liveness task owns demotion (`present = false`) and
    /// eventual removal, from the live connection state + an active TCP probe.
    pub present: bool,
    /// The device's WebSocket URL (`ws://<ip>:<port><path>`), built from its resolved
    /// address and the `path` TXT property it advertises. `None` until an IPv4
    /// resolves or if it advertises no path.
    ///
    /// Stored here because **this registry is the daemon's only browser** of
    /// `_sendspin._tcp`: mdns-sd keeps one listener per service type, so a second
    /// browse (one per sendspin server, as `ClientManager::start` would do) silently
    /// steals the subscription and leaves every earlier browser — including this
    /// registry — deaf. So the servers are handed URLs from here instead of
    /// discovering for themselves (`ClientManager::start_without_discovery`).
    pub url: Option<String>,
    /// Codecs this device advertised that it can decode **at our wire format**
    /// (48 kHz / 16-bit / stereo), lowercase, e.g. `["pcm", "flac", "opus"]`.
    ///
    /// Not from mDNS: it comes from the device's `client/hello`
    /// (`player@v1_support.supported_formats`), so it's only known once a server has
    /// connected to it at least once — `sendspin_server` writes it here on connect.
    /// Empty means "not seen yet" (assume PCM), NOT "supports nothing". Filtered to
    /// our format because a codec the device only decodes at another rate/depth is
    /// not usable for us.
    pub supported_codecs: Vec<String>,
    /// The ongoing buffer this device asks us to keep it stocked with, in ms
    /// (`min_buffer_ms` from its `client/state` player object). `None` until it
    /// reports one.
    ///
    /// The spec makes this a **requirement, not a hint**: "servers must schedule
    /// timestamps so each player's queued audio duration stays at or above its
    /// `min_buffer_ms`", and for a group the send-ahead is "the maximum per-player
    /// send-ahead across grouped players". A player may raise it for "codec init,
    /// decode warmup" — so it can change when the wire codec changes, which is why
    /// the group's lead is recomputed (and its server restarted) when this moves.
    /// Excludes `static_delay_ms`, which the server adds per player.
    pub min_buffer_ms: Option<u32>,
    /// Startup lead this device would like (`required_lead_time_ms`), to keep the
    /// beginning of a stream from being cut off. A *hint* the spec says to honour
    /// only "when doing so adds no latency, i.e. for buffered sources but not live
    /// streams" — ours is live, so it's surfaced for diagnostics and not enforced.
    pub required_lead_time_ms: Option<u32>,
}

/// Live discovered devices, keyed by their virtual output node name
/// (`sendspin-dev-<slug>`). Shared with the API (matrix/outputs listing) and
/// the grouping reconciler.
pub type SharedSendspinDevices = Arc<Mutex<BTreeMap<String, SendspinDevice>>>;

/// Virtual output node name for a discovered device.
pub fn device_node_name(display_name: &str) -> String {
    format!("{SENDSPIN_DEV_PREFIX}{}", slugify(display_name))
}

/// The resolved server address (first IPv4 + advertised port), or `None` if no
/// IPv4 has resolved yet. Used by the liveness probe.
/// The device's dial URL from its resolved service, mirroring what sendspin-rs's own
/// `ClientBrowser` builds: first non-loopback IPv4, its port, and the `path` TXT it
/// advertises (a device without one isn't dialable).
fn url_from_service(info: &ResolvedService) -> Option<String> {
    let ip = info.get_addresses_v4().into_iter().find(|a| !a.is_loopback() && !a.is_link_local())?;
    let path = info.get_property_val_str("path")?;
    if !path.starts_with('/') {
        return None;
    }
    Some(format!("ws://{ip}:{}{path}", info.get_port()))
}

fn addr_from_service(info: &ResolvedService) -> Option<std::net::SocketAddr> {
    let ip = info.get_addresses_v4().into_iter().next()?;
    Some(std::net::SocketAddr::new(std::net::IpAddr::V4(ip), info.get_port()))
}

/// Human name for a device from its resolved service: the `name` TXT if the
/// device set one (sendspin-rs advertisements do), else the mDNS instance
/// label (`my-device._sendspin._tcp.local.` -> `my-device`).
fn display_name_from_service(info: &ResolvedService) -> String {
    if let Some(name) = info.get_property_val_str("name") {
        if !name.trim().is_empty() {
            return name.trim().to_string();
        }
    }
    let fullname = info.get_fullname();
    fullname.split("._sendspin._tcp").next().unwrap_or(fullname).trim().to_string()
}

/// Start browsing `_sendspin._tcp.local.` on the shared mDNS `daemon` and keep
/// `devices` in sync. The browser runs until `daemon` is shut down (which
/// disconnects the receiver and ends the loop). Mirrors discovery.rs's
/// own-thread, blocking-recv shape.
pub fn spawn(daemon: &ServiceDaemon, devices: SharedSendspinDevices, changes: ChangeNotifier) -> anyhow::Result<()> {
    let receiver = daemon.browse(SENDSPIN_SERVICE_TYPE)?;
    std::thread::Builder::new().name("sendspin-discovery".into()).spawn(move || {
        while let Ok(event) = receiver.recv() {
            match event {
                ServiceEvent::ServiceResolved(info) => {
                    let display_name = display_name_from_service(&info);
                    let node_name = device_node_name(&display_name);
                    let fullname = info.get_fullname().to_string();
                    let addr = addr_from_service(&info);
                    let url = url_from_service(&info);

                    let mut devs = devices.lock_recover();
                    let notify = match devs.get_mut(&node_name) {
                        Some(dev) => {
                            // Keep it online (a resolve is a strong "here"
                            // signal), refresh addr/fullname, and notify only
                            // if it had been demoted to offline.
                            let came_online = !dev.present;
                            dev.present = true;
                            dev.fullname = fullname;
                            if addr.is_some() {
                                dev.addr = addr;
                            }
                            // A re-resolve at a new address must reach the servers, so
                            // they can redirect their supervisors (`supervise` is
                            // idempotent and treats a changed URL as a redial).
                            if url.is_some() && dev.url != url {
                                dev.url = url.clone();
                            }
                            if came_online {
                                tracing::info!("sendspin device '{display_name}' back online ({node_name})");
                            }
                            came_online
                        }
                        None => {
                            devs.insert(
                                node_name.clone(),
                                SendspinDevice {
                                    fullname,
                                    display_name: display_name.clone(),
                                    addr,
                                    present: true,
                                    // Filled in by sendspin_server from the device's own
                                    // client/hello + client/state, not from mDNS.
                                    url,
                                    supported_codecs: Vec::new(),
                                    min_buffer_ms: None,
                                    required_lead_time_ms: None,
                                },
                            );
                            tracing::info!("discovered sendspin device '{display_name}' ({node_name})");
                            true
                        }
                    };
                    drop(devs);
                    if notify {
                        // Wake the matrix WS + grouping reconciler.
                        let _ = changes.send(());
                    }
                }
                ServiceEvent::ServiceRemoved(_ty, fullname) => {
                    // Deliberately ignored: an mDNS "removed" is a TTL-expiry
                    // flap (WiFi power-save, a missed re-announce), not proof
                    // the device left — acting on it tore down live groups.
                    // Liveness (sendspin_liveness.rs: connection state + an
                    // active TCP probe) owns real offline/removal.
                    tracing::debug!("mDNS removed {fullname} (ignored; liveness decides offline)");
                }
                _ => {}
            }
        }
        tracing::info!("sendspin discovery loop ended");
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_node_name_uses_prefix_and_slug() {
        assert_eq!(device_node_name("Voice PE Kitchen"), "sendspin-dev-voice_pe_kitchen");
    }
}
