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

use crate::api::{SharedSources, SharedStore};
use crate::config::{slugify, RaopEncryption, RaopOutputConfig};
use crate::locks::LockRecover;
use crate::pw_thread::{PwCommand, PwCommandSender};
use crate::raop::{raop_module_args, raop_node_name, RAOP_MODULE_NAME};
use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use std::collections::HashMap;
use std::time::{Duration, Instant};
use tokio::sync::oneshot;

const RAOP_SERVICE_TYPE: &str = "_raop._tcp.local.";

/// How long a discovered RAOP receiver may be mDNS-absent before we unload its
/// sink. An mDNS "removed" is usually a TTL-expiry flap (the receiver never
/// left), so we wait out the grace and cancel on any re-resolve — mirroring the
/// sendspin liveness policy, without a per-device connection to lean on.
const RAOP_ABSENT_GRACE: Duration = Duration::from_secs(90);
/// How often the discovery loop wakes (when idle) to expire pending unloads.
const RAOP_ABSENT_TICK: Duration = Duration::from_secs(15);

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

/// The MAC portion of a RAOP mDNS instance fullname
/// (`"<MAC>@<friendly>._raop._tcp.local."`), or `""` if there's no `@`. Used to
/// recognize our own AirPlay receiver regardless of any ` (N)` suffix mDNS
/// appends to the friendly part on a name conflict.
fn instance_mac(fullname: &str) -> &str {
    let instance = fullname.split("._raop._tcp").next().unwrap_or(fullname);
    instance.split_once('@').map(|(mac, _)| mac).unwrap_or("")
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
pub fn spawn(pw_cmd: PwCommandSender, store: SharedStore, sources: SharedSources, mode: Mode) -> anyhow::Result<ServiceDaemon> {
    let daemon = ServiceDaemon::new()?;
    let receiver = daemon.browse(RAOP_SERVICE_TYPE)?;
    std::thread::Builder::new().name("mdns-discovery".into()).spawn(move || {
        // mDNS fullname -> RAOP node name, for receivers WE loaded, so a
        // later removal unloads exactly those (never store-managed ones).
        let mut loaded: HashMap<String, String> = HashMap::new();
        // Fullnames we've already warned lack IPv4. mdns-sd re-emits
        // ServiceResolved on every announcement, so a peer that never
        // exposes an IPv4 A record would otherwise spam the log many times
        // a second. Warn once; clear the flag if/when it finally resolves.
        let mut warned_no_ipv4: std::collections::HashSet<String> = std::collections::HashSet::new();
        // Fullname -> when it went mDNS-absent. Grace-delayed unload; a
        // re-resolve cancels it. recv_timeout wakes us to expire these even
        // when no mDNS events are arriving.
        let mut pending_unload: HashMap<String, Instant> = HashMap::new();
        'discovery: loop {
            match receiver.recv_timeout(RAOP_ABSENT_TICK) {
                Err(_) if receiver.is_disconnected() => break, // daemon dropped
                Err(_) => {}                                   // timeout — fall through to expire pending unloads
                Ok(ServiceEvent::ServiceResolved(info)) => {
                    let fullname = info.get_fullname().to_string();
                    // A resolve (even a re-announce mid-flap) cancels any
                    // scheduled unload for this receiver.
                    pending_unload.remove(&fullname);

                    // Skip our OWN AirPlay-receive source as early as
                    // possible — by instance MAC (our derived hwaddr), which
                    // is present even before an IPv4 address resolves and is
                    // stable across the " (N)" suffix mDNS appends on a name
                    // conflict. The embedded receiver (airplay_source.rs)
                    // advertises on every interface (incl. docker/IPv6), so
                    // some of its own resolutions carry no IPv4; skipping it
                    // here (before the IPv4 check) avoids both a feedback
                    // loop AND a resolve/log storm from our own registration.
                    let airplay_name = sources.lock_recover().airplay_source_name().map(str::to_string);
                    if let Some(ap) = &airplay_name {
                        let own_mac = crate::airplay_source::mdns_mac(ap);
                        if instance_mac(&fullname).eq_ignore_ascii_case(&own_mac) {
                            continue;
                        }
                    }

                    let Some(output) = output_from_service(&info) else {
                        if warned_no_ipv4.insert(fullname.clone()) {
                            tracing::warn!("discovered RAOP service {fullname} with no IPv4 address yet; skipping");
                        }
                        continue;
                    };
                    warned_no_ipv4.remove(&fullname); // it resolved now

                    let node_name = raop_node_name(&output.name);

                    // Friendly-name-slug fallback for our own receiver (the
                    // instance-MAC check above is the primary guard).
                    if let Some(ap) = &airplay_name {
                        if slugify(ap) == slugify(&output.name) {
                            tracing::debug!("discovered '{}' is our own AirPlay receiver; not loading it as an output", output.name);
                            continue;
                        }
                    }

                    if store.lock_recover().contains(&node_name) {
                        tracing::debug!("discovered '{}' is store-managed ({node_name}); leaving it to the store", output.name);
                        continue;
                    }
                    if loaded.contains_key(&fullname) {
                        continue; // already auto-loaded
                    }

                    if mode == Mode::DryRun {
                        tracing::info!(
                            "[discovery dry-run] would load '{}' at {}:{} ({}) as {node_name}",
                            output.name,
                            output.ip,
                            output.port,
                            output.encryption.as_pipewire_arg()
                        );
                        continue;
                    }

                    tracing::info!(
                        "auto-discovered RAOP receiver '{}' at {}:{} ({}); loading {node_name}",
                        output.name,
                        output.ip,
                        output.port,
                        output.encryption.as_pipewire_arg()
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
                Ok(ServiceEvent::ServiceRemoved(_ty, fullname)) => {
                    warned_no_ipv4.remove(&fullname); // let a re-appear warn again
                                                      // Don't unload now — schedule a grace-delayed unload so a
                                                      // TTL-flap re-resolve can cancel it. Only receivers WE
                                                      // loaded are candidates.
                    if loaded.contains_key(&fullname) {
                        pending_unload.entry(fullname).or_insert_with(Instant::now);
                    }
                }
                Ok(_) => {}
            }

            // Expire pending unloads: receivers absent past the grace window
            // are genuinely gone (a flap would have re-resolved and cancelled).
            if !pending_unload.is_empty() {
                let expired: Vec<String> = pending_unload
                    .iter()
                    .filter(|(_, since)| since.elapsed() >= RAOP_ABSENT_GRACE)
                    .map(|(fullname, _)| fullname.clone())
                    .collect();
                for fullname in expired {
                    pending_unload.remove(&fullname);
                    let Some(node_name) = loaded.remove(&fullname) else { continue };
                    tracing::info!("RAOP receiver '{fullname}' absent > {}s; unloading {node_name}", RAOP_ABSENT_GRACE.as_secs());
                    let (tx, rx) = oneshot::channel();
                    if pw_cmd.send(PwCommand::Unload { node_name, reply: tx }).is_err() {
                        tracing::error!("PipeWire thread unavailable; stopping discovery");
                        break 'discovery;
                    }
                    let _ = rx.blocking_recv();
                }
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
        assert_eq!(output_name_from_fullname("AABBCCDDEEFF@Living Room._raop._tcp.local."), "Living Room");
    }

    #[test]
    fn falls_back_to_whole_instance_without_at_sign() {
        assert_eq!(output_name_from_fullname("Dusche._raop._tcp.local."), "Dusche");
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
