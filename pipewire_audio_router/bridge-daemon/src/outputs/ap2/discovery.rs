//! mDNS/DNS-SD auto-discovery of AirPlay-2 receivers on the LAN.
//!
//! AirPlay-2 speakers (Yamaha MusicCast, Pioneer/Onkyo, Denon/HEOS, HomePod, …)
//! advertise `_airplay._tcp.local.`. This module browses that service type and
//! maintains a shared registry of live receivers, mirroring
//! `outputs/sendspin/discovery.rs`: a discovered receiver becomes a *virtual* routing
//! output (`ap2-dev-<slug>`, no PipeWire node of its own). The audio path — one
//! anchor + a per-device AP2 RTP sender per co-routed receiver — is built later
//! by the grouping reconciler (Phase 3); here we only populate the registry and
//! register each receiver as a PTP peer of the host-global grandmaster
//! (`outputs::ap2::ptp::Ap2PtpService`) so libairptp starts driving its clock.
//!
//! This is the replacement for RAOP output discovery (discovery.rs): all target
//! receivers are AirPlay-2-capable.

use crate::outputs::ap2::ptp::SharedAp2Ptp;
use crate::pw::thread::ChangeNotifier;
use crate::util::locks::LockRecover;
use crate::util::node_names::{slugify, AP2_DEV_PREFIX};
use mdns_sd::{ResolvedService, ServiceDaemon, ServiceEvent};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

const AIRPLAY_SERVICE_TYPE: &str = "_airplay._tcp.local.";

/// One discovered AirPlay-2 receiver.
#[derive(Debug, Clone)]
pub struct Ap2Device {
    /// Stable mDNS instance identity (`Dusche._airplay._tcp.local.`).
    pub fullname: String,
    pub display_name: String,
    /// Model from the `model` TXT (e.g. "WX-021"), for display/quirk handling.
    pub model: Option<String>,
    /// Raw 64-bit AirPlay feature bitmask from the `features` TXT (e.g.
    /// `0x445F8A00,0x1C340`), or `None` if absent/unparseable. Decode with
    /// `airplay_core::features::Features::from_raw` — surfaced in `/api/outputs`
    /// for the PTP badge (bit 41 = PTP support) and the Diagnostics capability
    /// decode. NB: on the tested Yamaha + Pioneer this value is *identical*, so
    /// it can't distinguish which one PTP-locks — see the PTP badge logic.
    pub features: Option<u64>,
    /// Resolved server address (first IPv4 + advertised port, usually :7000).
    /// `None` until an IPv4 resolves. Used as the RTSP endpoint (Phase 3) and as
    /// the PTP peer address.
    pub addr: Option<std::net::SocketAddr>,
    /// Liveness. mDNS only ever sets this `true` (on resolve); a future liveness
    /// task owns demotion, same contract as sendspin.
    pub present: bool,
}

/// Live discovered AP2 receivers, keyed by virtual output node name
/// (`ap2-dev-<slug>`). Shared with the API (outputs listing) and, later, the
/// grouping reconciler.
pub type SharedAp2Devices = Arc<Mutex<BTreeMap<String, Ap2Device>>>;

/// Virtual output node name for a discovered receiver.
pub fn device_node_name(display_name: &str) -> String {
    format!("{AP2_DEV_PREFIX}{}", slugify(display_name))
}

fn addr_from_service(info: &ResolvedService) -> Option<std::net::SocketAddr> {
    let ip = info.get_addresses_v4().into_iter().next()?;
    Some(std::net::SocketAddr::new(std::net::IpAddr::V4(ip), info.get_port()))
}

/// Human name: the mDNS instance label (`Dusche._airplay._tcp.local.` -> `Dusche`).
fn display_name_from_service(info: &ResolvedService) -> String {
    let fullname = info.get_fullname();
    fullname.split("._airplay._tcp").next().unwrap_or(fullname).trim().to_string()
}

/// Start browsing `_airplay._tcp.local.` on the shared mDNS `daemon` and keep
/// `devices` in sync, registering each resolved receiver as a PTP peer of the
/// host-global grandmaster. The browser runs until `daemon` is shut down (which
/// disconnects the receiver and ends the loop). Mirrors
/// `outputs::sendspin::discovery::spawn`.
pub fn spawn(daemon: &ServiceDaemon, devices: SharedAp2Devices, changes: ChangeNotifier, ptp: SharedAp2Ptp) -> anyhow::Result<()> {
    let receiver = daemon.browse(AIRPLAY_SERVICE_TYPE)?;
    std::thread::Builder::new().name("ap2-discovery".into()).spawn(move || {
        while let Ok(event) = receiver.recv() {
            match event {
                ServiceEvent::ServiceResolved(info) => {
                    let display_name = display_name_from_service(&info);
                    let node_name = device_node_name(&display_name);
                    let fullname = info.get_fullname().to_string();
                    let model = info.get_property_val_str("model").map(|s| s.to_string());
                    let features = info
                        .get_property_val_str("features")
                        .and_then(|s| airplay_core::features::Features::from_txt_value(s).ok())
                        .map(|f| f.raw());
                    let addr = addr_from_service(&info);

                    let mut devs = devices.lock_recover();
                    let notify = match devs.get_mut(&node_name) {
                        Some(dev) => {
                            let came_online = !dev.present;
                            dev.present = true;
                            dev.fullname = fullname;
                            if model.is_some() {
                                dev.model = model;
                            }
                            if features.is_some() {
                                dev.features = features;
                            }
                            if addr.is_some() {
                                dev.addr = addr;
                            }
                            if came_online {
                                tracing::info!("AirPlay-2 receiver '{display_name}' back online ({node_name})");
                            }
                            came_online
                        }
                        None => {
                            devs.insert(
                                node_name.clone(),
                                Ap2Device { fullname, display_name: display_name.clone(), model, features, addr, present: true },
                            );
                            tracing::info!("discovered AirPlay-2 receiver '{display_name}' ({node_name})");
                            true
                        }
                    };
                    drop(devs);

                    // Register with the host-global PTP grandmaster so libairptp
                    // drives this receiver's clock. Lazy-starts the grandmaster
                    // (binds 319/320) on the first peer. add_peer is idempotent
                    // per address.
                    if let Some(a) = addr {
                        let ip = a.ip().to_string();
                        if let Err(e) = ptp.add_peer(&ip) {
                            tracing::warn!("AP2 PTP add_peer({ip}) failed: {e}");
                        }
                    }

                    if notify {
                        let _ = changes.send(());
                    }
                }
                ServiceEvent::ServiceRemoved(_ty, fullname) => {
                    // Ignored, same rationale as sendspin: an mDNS "removed" is a
                    // TTL flap, not proof the receiver left. A liveness task will
                    // own real offline/removal (and PTP peer removal).
                    tracing::debug!("mDNS removed {fullname} (ignored; liveness decides offline)");
                }
                _ => {}
            }
        }
        tracing::info!("AirPlay-2 discovery loop ended");
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_node_name_uses_prefix_and_slug() {
        assert_eq!(device_node_name("Pioneer VSX-934"), "ap2-dev-pioneer_vsx_934");
    }

    /// Real-LAN smoke test: browse `_airplay._tcp` for a few seconds and report
    /// discovered receivers + the PTP peers/clock they registered. Ignored by
    /// default (needs a network with AP2 receivers + bindable 319/320).
    ///   cargo test -p bridge-daemon ap2_discovery_smoke_lan -- --ignored --nocapture
    #[test]
    #[ignore = "network smoke test: browses the real LAN for _airplay._tcp receivers"]
    fn ap2_discovery_smoke_lan() {
        use std::time::Duration;

        let devices: SharedAp2Devices = Arc::new(Mutex::new(BTreeMap::new()));
        let (changes, _rx) = tokio::sync::broadcast::channel::<()>(16);
        let ptp = crate::outputs::ap2::ptp::Ap2PtpService::new();

        let daemon = ServiceDaemon::new().expect("mdns daemon");
        spawn(&daemon, devices.clone(), changes, ptp.clone()).expect("spawn ap2 discovery");
        std::thread::sleep(Duration::from_secs(8));

        let devs = devices.lock().unwrap();
        println!("\n=== AP2 discovery: {} receiver(s) on the LAN ===", devs.len());
        for (node, d) in devs.iter() {
            println!("  {node} -> '{}'  addr={:?}  model={:?}  present={}", d.display_name, d.addr, d.model, d.present);
        }
        println!("PTP grandmaster clock_id: {:?}", ptp.clock_id());
        println!("PTP peers registered: {:?}", ptp.peers());

        assert!(!devs.is_empty(), "expected at least one _airplay._tcp receiver on the LAN");
    }
}
