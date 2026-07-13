//! mDNS/DNS-SD auto-discovery of RAOP (AirPlay) receivers on the LAN.
//!
//! Browses `_raop._tcp` and, for every receiver it resolves, loads a
//! `raop-sink` module for it live (the same `PwCommand::Load` path the manual
//! `/api/outputs` endpoint uses — pw_thread.rs / pw_module.rs); when a receiver
//! goes away, it unloads that module. This makes configured outputs
//! **overrides-only**: a device the user hasn't configured is picked up
//! automatically, while anything in the persistent store (outputs_store.rs) is
//! left exactly as configured.
//!
//! Reconciliation with the store: a store-managed output (seeded from
//! `options.json` or added via the API) is authoritative and already loaded at
//! startup, so discovery skips any receiver whose RAOP node name matches a
//! store entry — no double-load, and the user's per-device settings
//! (encryption/port) win. Discovery only ever loads/unloads receivers it
//! itself brought in, tracked by their mDNS fullname.
//!
//! Encryption can't be reliably derived from the mDNS TXT record, so
//! discovered outputs default to `auth_setup` (the mode proven against real
//! hardware — see config.rs / spike 2). A device needing a different mode is
//! handled by adding it to the store as an override.
//!
//! Runs on its own OS thread with a blocking `recv()` over mdns-sd's event
//! channel; `PwCommandSender::send` is thread-safe, so it drives the PipeWire
//! thread directly without any tokio runtime of its own.

use crate::api::SharedStore;
use crate::config::{RaopEncryption, RaopOutputConfig};
use crate::locks::LockRecover;
use crate::pw_thread::{PwCommand, PwCommandSender};
use crate::raop::{raop_module_args, raop_node_name, RAOP_MODULE_NAME};
use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use std::collections::HashMap;
use tokio::sync::oneshot;

const RAOP_SERVICE_TYPE: &str = "_raop._tcp.local.";

/// Whether discovery loads modules, or only logs what it would load.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Load/unload a `raop-sink` module per discovered/removed receiver.
    Load,
    /// Log discovered/removed receivers but don't touch the graph — a safe way
    /// to see what's on the network without connecting to real devices.
    DryRun,
}

/// Derives a human output name from a RAOP service instance fullname. RAOP
/// instance names are conventionally `"<MAC>@<Friendly Name>"`; we want the
/// friendly part. Falls back to the whole instance label when there's no `@`.
fn output_name_from_fullname(fullname: &str) -> String {
    // e.g. "AABBCCDDEEFF@Living Room._raop._tcp.local." -> "Living Room"
    let instance = fullname.split("._raop._tcp").next().unwrap_or(fullname);
    match instance.split_once('@') {
        Some((_mac, friendly)) if !friendly.trim().is_empty() => friendly.trim().to_string(),
        _ => instance.trim().to_string(),
    }
}

/// Picks the RAOP encryption mode from the mDNS `et` (encryption types) TXT
/// value — a comma-separated list of the modes the receiver supports:
/// `0`=none, `1`=RSA, `2`=FairPlay, `3`=MFiSAP, `4`=FairPlay SAPv2.5.
///
/// We choose the strongest mode PipeWire can speak that the device offers,
/// preferring encrypted ones: receivers routinely advertise `0` yet still
/// reject an unencrypted ANNOUNCE (spike 2 — Pioneer/Dusche list `et=0,4` but
/// `403` on `none`), so `none` is used only when it's the *sole* option. `2`
/// (FairPlay DRM) is something PipeWire can't do, so it's ignored. Anything
/// unrecognized (or a missing `et`) falls back to the proven `auth_setup`.
fn encryption_from_et(et: &str) -> RaopEncryption {
    let offered: Vec<&str> = et.split(',').map(str::trim).collect();
    let has = |code: &str| offered.contains(&code);
    if has("3") || has("4") {
        RaopEncryption::AuthSetup
    } else if has("1") {
        RaopEncryption::Rsa
    } else if has("0") {
        RaopEncryption::None
    } else {
        RaopEncryption::AuthSetup
    }
}

/// Builds a `RaopOutputConfig` from a resolved service, or `None` if it exposes
/// no usable IPv4 address yet. Encryption is derived from the `et` TXT field
/// (falling back to `auth_setup` when absent).
fn output_from_service(info: &ServiceInfo) -> Option<RaopOutputConfig> {
    let ip = info.get_addresses_v4().into_iter().next()?;
    Some(RaopOutputConfig {
        name: output_name_from_fullname(info.get_fullname()),
        ip: ip.to_string(),
        port: info.get_port(),
        encryption: info.get_property_val_str("et").map(encryption_from_et).unwrap_or_default(),
    })
}

/// Starts mDNS discovery and returns the daemon handle. The handle must be
/// kept alive for discovery to keep running — dropping it stops the browse and
/// ends the worker thread.
pub fn spawn(pw_cmd: PwCommandSender, store: SharedStore, mode: Mode) -> anyhow::Result<ServiceDaemon> {
    let daemon = ServiceDaemon::new()?;
    let receiver = daemon.browse(RAOP_SERVICE_TYPE)?;
    std::thread::Builder::new()
        .name("mdns-discovery".into())
        .spawn(move || {
            // mDNS fullname -> RAOP node name, for receivers WE loaded, so a
            // later removal unloads exactly those (never store-managed ones).
            let mut loaded: HashMap<String, String> = HashMap::new();
            while let Ok(event) = receiver.recv() {
                match event {
                    ServiceEvent::ServiceResolved(info) => {
                        let Some(output) = output_from_service(&info) else {
                            tracing::warn!(
                                "discovered RAOP service {} with no IPv4 address yet; skipping",
                                info.get_fullname()
                            );
                            continue;
                        };
                        let node_name = raop_node_name(&output.name);
                        let fullname = info.get_fullname().to_string();

                        if store.lock_recover().contains(&node_name) {
                            tracing::debug!(
                                "discovered '{}' is store-managed ({node_name}); leaving it to the store",
                                output.name
                            );
                            continue;
                        }
                        if loaded.contains_key(&fullname) {
                            continue; // already auto-loaded
                        }

                        if mode == Mode::DryRun {
                            tracing::info!(
                                "[discovery dry-run] would load '{}' at {}:{} ({}) as {node_name}",
                                output.name, output.ip, output.port, output.encryption.as_pipewire_arg()
                            );
                            continue;
                        }

                        tracing::info!(
                            "auto-discovered RAOP receiver '{}' at {}:{} ({}); loading {node_name}",
                            output.name, output.ip, output.port, output.encryption.as_pipewire_arg()
                        );
                        let args = raop_module_args(&output);
                        let (tx, rx) = oneshot::channel();
                        let sent = pw_cmd.send(PwCommand::Load {
                            node_name: node_name.clone(),
                            module_name: RAOP_MODULE_NAME.to_string(),
                            args,
                            reply: tx,
                        });
                        if sent.is_err() {
                            tracing::error!("PipeWire thread unavailable; stopping discovery");
                            break;
                        }
                        match rx.blocking_recv() {
                            Ok(Ok(())) => {
                                loaded.insert(fullname, node_name);
                            }
                            Ok(Err(e)) => tracing::warn!("failed to load discovered '{node_name}': {e}"),
                            Err(_) => tracing::warn!("no reply loading discovered '{node_name}'"),
                        }
                    }
                    ServiceEvent::ServiceRemoved(_ty, fullname) => {
                        let Some(node_name) = loaded.remove(&fullname) else {
                            continue; // not one we loaded (store-managed, or dry-run)
                        };
                        tracing::info!("RAOP receiver '{fullname}' went away; unloading {node_name}");
                        let (tx, rx) = oneshot::channel();
                        if pw_cmd.send(PwCommand::Unload { node_name, reply: tx }).is_err() {
                            tracing::error!("PipeWire thread unavailable; stopping discovery");
                            break;
                        }
                        let _ = rx.blocking_recv();
                    }
                    _ => {}
                }
            }
            tracing::info!("mDNS discovery loop ended");
        })?;
    Ok(daemon)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn friendly_name_taken_from_after_the_at_sign() {
        assert_eq!(
            output_name_from_fullname("AABBCCDDEEFF@Living Room._raop._tcp.local."),
            "Living Room"
        );
    }

    #[test]
    fn falls_back_to_whole_instance_without_at_sign() {
        assert_eq!(
            output_name_from_fullname("Dusche._raop._tcp.local."),
            "Dusche"
        );
    }

    #[test]
    fn tolerates_missing_service_suffix() {
        assert_eq!(output_name_from_fullname("Kitchen"), "Kitchen");
    }

    #[test]
    fn empty_friendly_part_falls_back_to_instance() {
        // "MAC@" with nothing after -> use the whole instance label.
        assert_eq!(output_name_from_fullname("AABBCC@._raop._tcp.local."), "AABBCC@");
    }

    #[test]
    fn et_prefers_auth_setup_when_mfi_fairplay_offered() {
        // Pioneer/Dusche advertise et=0,4 -> auth_setup (proven working).
        assert_eq!(encryption_from_et("0,4"), RaopEncryption::AuthSetup);
        assert_eq!(encryption_from_et("0,3"), RaopEncryption::AuthSetup);
    }

    #[test]
    fn et_picks_rsa_for_airport_express_gen1() {
        assert_eq!(encryption_from_et("0,1"), RaopEncryption::Rsa);
    }

    #[test]
    fn et_none_only_when_sole_option() {
        assert_eq!(encryption_from_et("0"), RaopEncryption::None);
    }

    #[test]
    fn et_unknown_or_fairplay_only_falls_back_to_auth_setup() {
        assert_eq!(encryption_from_et("2"), RaopEncryption::AuthSetup); // FairPlay DRM: unsupported
        assert_eq!(encryption_from_et(""), RaopEncryption::AuthSetup);
    }
}
