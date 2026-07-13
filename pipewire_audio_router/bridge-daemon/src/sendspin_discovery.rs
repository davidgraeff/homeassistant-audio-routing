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
//! by the grouping reconciler (sendspin_group.rs) from the routing intent.
//! This mirrors RAOP's "devices appear in the matrix automatically" UX while
//! honoring sendspin's group-based multi-room model.

use crate::config::{slugify, SENDSPIN_DEV_PREFIX};
use crate::locks::LockRecover;
use crate::pw_thread::ChangeNotifier;
use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
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
}

/// Live discovered devices, keyed by their virtual output node name
/// (`sendspin-dev-<slug>`). Shared with the API (matrix/outputs listing) and
/// the grouping reconciler.
pub type SharedSendspinDevices = Arc<Mutex<BTreeMap<String, SendspinDevice>>>;

/// Virtual output node name for a discovered device.
pub fn device_node_name(display_name: &str) -> String {
    format!("{SENDSPIN_DEV_PREFIX}{}", slugify(display_name))
}

/// Human name for a device from its resolved service: the `name` TXT if the
/// device set one (sendspin-rs advertisements do), else the mDNS instance
/// label (`my-device._sendspin._tcp.local.` -> `my-device`).
fn display_name_from_service(info: &ServiceInfo) -> String {
    if let Some(name) = info.get_property_val_str("name") {
        if !name.trim().is_empty() {
            return name.trim().to_string();
        }
    }
    let fullname = info.get_fullname();
    fullname.split("._sendspin._tcp").next().unwrap_or(fullname).trim().to_string()
}

/// Start browsing `_sendspin._tcp.local.` and keep `devices` in sync. Returns
/// the daemon handle — hold it alive for discovery to keep running (dropping
/// it stops the browse). Mirrors discovery.rs's own-thread, blocking-recv
/// shape.
pub fn spawn(devices: SharedSendspinDevices, changes: ChangeNotifier) -> anyhow::Result<ServiceDaemon> {
    let daemon = ServiceDaemon::new()?;
    let receiver = daemon.browse(SENDSPIN_SERVICE_TYPE)?;
    std::thread::Builder::new()
        .name("sendspin-discovery".into())
        .spawn(move || {
            // mDNS fullname -> node name, so a removal drops the right entry.
            let mut by_fullname: BTreeMap<String, String> = BTreeMap::new();
            while let Ok(event) = receiver.recv() {
                match event {
                    ServiceEvent::ServiceResolved(info) => {
                        let display_name = display_name_from_service(&info);
                        let node_name = device_node_name(&display_name);
                        let fullname = info.get_fullname().to_string();
                        by_fullname.insert(fullname.clone(), node_name.clone());
                        let is_new = devices
                            .lock_recover()
                            .insert(node_name.clone(), SendspinDevice { fullname, display_name: display_name.clone() })
                            .is_none();
                        if is_new {
                            tracing::info!("discovered sendspin device '{display_name}' ({node_name})");
                            // Wake the matrix WS + grouping reconciler so the
                            // new device shows up and gets grouped/dialed.
                            let _ = changes.send(());
                        }
                    }
                    ServiceEvent::ServiceRemoved(_ty, fullname) => {
                        if let Some(node_name) = by_fullname.remove(&fullname) {
                            devices.lock_recover().remove(&node_name);
                            tracing::info!("sendspin device '{fullname}' went away; removing {node_name}");
                            let _ = changes.send(());
                        }
                    }
                    _ => {}
                }
            }
            tracing::info!("sendspin discovery loop ended");
        })?;
    Ok(daemon)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_node_name_uses_prefix_and_slug() {
        assert_eq!(device_node_name("Voice PE Kitchen"), "sendspin-dev-voice_pe_kitchen");
    }
}
