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
use crate::sync_settings::SharedSyncSettings;
use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::oneshot;

/// The resolved connection details (ip/port/encryption) of RAOP receivers
/// discovery currently has loaded, keyed by RAOP node name. Populated by the
/// discovery thread and read by the API (list_outputs) so auto-discovered
/// receivers show the same IP/Port/Encryption columns as manually-added ones —
/// the info exists at resolve time, it just needs surfacing. A device is added
/// when its module is loaded and removed when it's unloaded.
pub type SharedDiscovered = Arc<Mutex<BTreeMap<String, RaopOutputConfig>>>;

/// A receiver discovery loaded, tracked by mDNS fullname. Carries the latency it
/// was loaded with (so an override change triggers a reload) and its resolved
/// config (so the reload can rebuild the module args without a re-announce).
#[derive(Clone)]
struct LoadedReceiver {
    node_name: String,
    latency: Option<u16>,
    output: RaopOutputConfig,
}

/// Load a discovered receiver's `raop-sink` module. `Ok(true)` on success,
/// `Ok(false)` on a module-level error (logged), `Err(())` if the PipeWire
/// thread is gone (caller should stop discovery).
fn load_module(pw_cmd: &PwCommandSender, node_name: &str, args: String) -> Result<bool, ()> {
    let (tx, rx) = oneshot::channel();
    if pw_cmd
        .send(PwCommand::Load { node_name: node_name.to_string(), module_name: RAOP_MODULE_NAME.to_string(), args, reply: tx })
        .is_err()
    {
        tracing::error!("PipeWire thread unavailable; stopping discovery");
        return Err(());
    }
    match rx.blocking_recv() {
        Ok(Ok(())) => Ok(true),
        Ok(Err(e)) => {
            tracing::warn!("failed to load discovered '{node_name}': {e}");
            Ok(false)
        }
        Err(_) => {
            tracing::warn!("no reply loading discovered '{node_name}'");
            Ok(false)
        }
    }
}

/// Unload a discovered receiver's module (idempotent). `Err(())` if the PipeWire
/// thread is gone.
fn unload_module(pw_cmd: &PwCommandSender, node_name: &str) -> Result<(), ()> {
    let (tx, rx) = oneshot::channel();
    if pw_cmd.send(PwCommand::Unload { node_name: node_name.to_string(), reply: tx }).is_err() {
        tracing::error!("PipeWire thread unavailable; stopping discovery");
        return Err(());
    }
    let _ = rx.blocking_recv();
    Ok(())
}

const RAOP_SERVICE_TYPE: &str = "_raop._tcp.local.";

/// Debounce before acting on an mDNS "removed". An mDNS removal is usually a
/// TTL-expiry flap (the receiver never left), so we wait this out and cancel it
/// on any re-resolve. After it elapses we don't blindly unload — we TCP-probe
/// the receiver's last-known address (`probe_reachable`) and only unload when it
/// is *also* unreachable, then re-probe at this same cadence while it stays
/// mDNS-absent-but-reachable. This mirrors the sendspin liveness policy
/// (connection/probe driven): a device that stops announcing over mDNS but
/// still answers on the wire stays loaded.
const RAOP_ABSENT_GRACE: Duration = Duration::from_secs(90);
/// How often the discovery loop wakes (when idle) to expire pending unloads.
const RAOP_ABSENT_TICK: Duration = Duration::from_secs(15);
/// Per-probe TCP connect timeout for the mDNS-absent liveness fallback.
const RAOP_PROBE_TIMEOUT: Duration = Duration::from_secs(2);

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

/// A short blocking TCP connect to `addr` — success means something is
/// listening, i.e. the receiver is still on the network even if it has stopped
/// announcing itself over mDNS. This runs on the discovery OS thread, which has
/// no tokio runtime, so it uses `std::net` with an explicit connect timeout
/// rather than the async connect `sendspin_liveness` uses. A refused connection
/// returns immediately; only an unreachable host waits out the full timeout.
fn probe_reachable(addr: std::net::SocketAddr) -> bool {
    std::net::TcpStream::connect_timeout(&addr, RAOP_PROBE_TIMEOUT).is_ok()
}

/// Starts mDNS discovery and returns the daemon handle. The handle must be
/// kept alive for discovery to keep running — dropping it stops the browse and
/// ends the worker thread.
pub fn spawn(
    pw_cmd: PwCommandSender,
    store: SharedStore,
    sources: SharedSources,
    mode: Mode,
    sync_settings: SharedSyncSettings,
    discovered: SharedDiscovered,
) -> anyhow::Result<ServiceDaemon> {
    let daemon = ServiceDaemon::new()?;
    let receiver = daemon.browse(RAOP_SERVICE_TYPE)?;
    std::thread::Builder::new().name("mdns-discovery".into()).spawn(move || {
        // mDNS fullname -> what WE loaded (node name + the latency + resolved
        // config), so a later removal unloads exactly those (never store-managed
        // ones) and an override change can reload with the new latency.
        let mut loaded: HashMap<String, LoadedReceiver> = HashMap::new();
        // Fullnames we've already warned lack IPv4. mdns-sd re-emits
        // ServiceResolved on every announcement, so a peer that never
        // exposes an IPv4 A record would otherwise spam the log many times
        // a second. Warn once; clear the flag if/when it finally resolves.
        let mut warned_no_ipv4: std::collections::HashSet<String> = std::collections::HashSet::new();
        // Fullname -> when it went mDNS-absent (or when it was last probed
        // while absent). Grace-delayed, probe-gated unload; a re-resolve
        // cancels it. recv_timeout wakes us to expire these even when no mDNS
        // events are arriving.
        let mut pending_unload: HashMap<String, Instant> = HashMap::new();
        // Fullname -> last-resolved socket address, for the mDNS-absent TCP
        // probe. Refreshed on every resolve so the probe always targets the
        // most recently advertised address.
        let mut last_addr: HashMap<String, std::net::SocketAddr> = HashMap::new();
        // Receivers currently held loaded by the probe fallback (mDNS-absent
        // but still reachable). Tracked only to log the hold once per episode
        // instead of on every re-probe.
        let mut held_by_probe: std::collections::HashSet<String> = std::collections::HashSet::new();
        'discovery: loop {
            match receiver.recv_timeout(RAOP_ABSENT_TICK) {
                Err(_) if receiver.is_disconnected() => break, // daemon dropped
                Err(_) => {}                                   // timeout — fall through to expire pending unloads
                Ok(ServiceEvent::ServiceResolved(info)) => {
                    let fullname = info.get_fullname().to_string();
                    // A resolve (even a re-announce mid-flap) cancels any
                    // scheduled unload for this receiver and ends any
                    // probe-hold episode.
                    pending_unload.remove(&fullname);
                    held_by_probe.remove(&fullname);

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

                    // Remember where to probe if this receiver later goes
                    // mDNS-absent. Kept fresh on every resolve so a DHCP change
                    // is reflected before the device drops off announcements.
                    if let Ok(addr) = format!("{}:{}", output.ip, output.port).parse::<std::net::SocketAddr>() {
                        last_addr.insert(fullname.clone(), addr);
                    }

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
                    // Desired per-output latency override (sync_settings.rs).
                    let desired_latency = sync_settings.lock_recover().raop_latency(&node_name);
                    match loaded.get(&fullname) {
                        // Already loaded with the current latency — nothing to do.
                        Some(l) if l.latency == desired_latency => continue,
                        // Loaded, but the override changed → unload so we reload below.
                        Some(_) => {
                            tracing::info!("discovered '{node_name}': latency override changed to {desired_latency:?}; reloading");
                            if unload_module(&pw_cmd, &node_name).is_err() {
                                break;
                            }
                            loaded.remove(&fullname);
                        }
                        None => {}
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
                        "auto-discovered RAOP receiver '{}' at {}:{} ({}, latency {desired_latency:?}); loading {node_name}",
                        output.name,
                        output.ip,
                        output.port,
                        output.encryption.as_pipewire_arg()
                    );
                    let args = raop_module_args(&output, desired_latency);
                    match load_module(&pw_cmd, &node_name, args) {
                        Ok(true) => {
                            discovered.lock_recover().insert(node_name.clone(), output.clone());
                            loaded.insert(fullname, LoadedReceiver { node_name, latency: desired_latency, output });
                        }
                        Ok(false) => {}
                        Err(()) => break,
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

            // Past the grace debounce, a still-absent receiver is either a
            // genuine departure or an mDNS flap while the device is still up.
            // TCP-probe its last-known address to tell them apart: a reachable
            // device is kept loaded and re-probed one grace window later; only
            // one that is *also* unreachable is unloaded. This is the sendspin
            // liveness policy applied to RAOP.
            if !pending_unload.is_empty() {
                let due: Vec<String> = pending_unload
                    .iter()
                    .filter(|(_, since)| since.elapsed() >= RAOP_ABSENT_GRACE)
                    .map(|(fullname, _)| fullname.clone())
                    .collect();
                for fullname in due {
                    let Some(node_name) = loaded.get(&fullname).map(|l| l.node_name.clone()) else {
                        pending_unload.remove(&fullname); // not ours (shouldn't happen)
                        continue;
                    };
                    // Reachable = mDNS-absent but still answering on the wire:
                    // hold it loaded and reset the timer so we re-probe later.
                    // (No address on record — never fully resolved — counts as
                    // unreachable, so it gets unloaded.)
                    if last_addr.get(&fullname).is_some_and(|addr| probe_reachable(*addr)) {
                        if held_by_probe.insert(fullname.clone()) {
                            tracing::info!(
                                "RAOP receiver '{fullname}' mDNS-absent but still reachable; keeping {node_name} loaded (probe fallback)"
                            );
                        }
                        pending_unload.insert(fullname, Instant::now());
                        continue;
                    }
                    // Genuinely gone: unreachable by probe after the grace window.
                    pending_unload.remove(&fullname);
                    held_by_probe.remove(&fullname);
                    last_addr.remove(&fullname);
                    loaded.remove(&fullname);
                    discovered.lock_recover().remove(&node_name);
                    tracing::info!(
                        "RAOP receiver '{fullname}' absent > {}s and unreachable; unloading {node_name}",
                        RAOP_ABSENT_GRACE.as_secs()
                    );
                    let (tx, rx) = oneshot::channel();
                    if pw_cmd.send(PwCommand::Unload { node_name, reply: tx }).is_err() {
                        tracing::error!("PipeWire thread unavailable; stopping discovery");
                        break 'discovery;
                    }
                    let _ = rx.blocking_recv();
                }
            }

            // Apply latency-override changes (e.g. from the API) to already-loaded
            // discovered receivers within one tick, without waiting for the device
            // to re-announce. Store-managed outputs are reloaded by the API itself.
            if mode == Mode::Load {
                let stale: Vec<String> = loaded
                    .iter()
                    .filter(|(_, l)| sync_settings.lock_recover().raop_latency(&l.node_name) != l.latency)
                    .map(|(fullname, _)| fullname.clone())
                    .collect();
                for fullname in stale {
                    let Some(l) = loaded.get(&fullname).cloned() else { continue };
                    let desired = sync_settings.lock_recover().raop_latency(&l.node_name);
                    tracing::info!("applying latency {desired:?} to discovered '{}' (reload)", l.node_name);
                    if unload_module(&pw_cmd, &l.node_name).is_err() {
                        break 'discovery;
                    }
                    let args = raop_module_args(&l.output, desired);
                    match load_module(&pw_cmd, &l.node_name, args) {
                        Ok(true) => {
                            loaded.insert(fullname, LoadedReceiver { latency: desired, ..l });
                        }
                        Ok(false) => {
                            loaded.remove(&fullname);
                        }
                        Err(()) => break 'discovery,
                    }
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

    #[test]
    fn probe_reachable_true_for_a_listening_socket_false_for_a_closed_one() {
        use std::net::TcpListener;
        // A bound listener is reachable; the same address is refused (fast,
        // not a timeout) once nothing is listening on it.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        assert!(probe_reachable(addr));
        drop(listener);
        assert!(!probe_reachable(addr));
    }
}
