//! mDNS discovery of **Bluetooth→RTP bridges** — Raspberry Pis set up by
//! `firmware/pi-bridge/setup_pi_bridge.py`, which advertise themselves as
//! `_pwrouter-btbridge._tcp.local.` with their stream parameters in TXT.
//!
//! ## Why the bridge has to announce itself
//! The daemon cannot work out *where* an RTP source's audio comes from.
//! `module-rtp-source` exposes only the address it **listens on** (`rtp.source.ip`
//! — `0.0.0.0` for unicast, or the multicast group), never the sender's, and
//! opening a second socket on the RTP port to sniff it would take datagrams away
//! from the module (`SO_REUSEPORT` load-balances unicast) and cause audible
//! dropouts — the same reason `rtp_membership.rs` was deleted. So the bridge
//! publishes an Avahi service file and we browse for it.
//!
//! ## Why not `_pipewire-audio._udp`
//! That type means the opposite: a host running `module-rtp-session` willing to
//! **receive**, which `pw_target_discovery.rs` already surfaces as a routing
//! *output*. A bridge advertising it would appear as a speaker. It also could not
//! carry this direction's audio: stock `module-rtp-session` refuses plain RTP
//! (it wants the AppleMIDI handshake) and in discover mode attaches to *every*
//! session of the media type — including the `pwrouter-*` sessions
//! `pwsink_server.rs` advertises for outputs, which would loop our own output
//! audio back in as an input. So the audio path stays
//! `module-rtp-sink` → `module-rtp-source`; only *discovery* is shared in spirit.
//!
//! ## What discovery is for
//! Two things, and deliberately nothing else — no audio path is built from an
//! advert:
//! 1. **Adoption.** An unconfigured bridge is offered on the Sources tab with its
//!    port/rate/destination prefilled from TXT, so adding it is one click instead
//!    of four retyped numbers.
//! 2. **A diagnostics link.** The advert names the HTTP port of the
//!    `bluetooth-testing-app`; [`probe`] confirms that app is really answering
//!    there before the UI offers a link, so an advert left behind by a bridge
//!    whose app is not running never produces a dead link.

use crate::pw::thread::ChangeNotifier;
use crate::util::locks::LockRecover;
use mdns_sd::{ResolvedService, ScopedIp, ServiceDaemon, ServiceEvent};
use std::collections::BTreeMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The mDNS service type `setup_pi_bridge.py` advertises.
pub const BT_BRIDGE_SERVICE_TYPE: &str = "_pwrouter-btbridge._tcp.local.";

/// TXT `ver` this daemon understands. An advert from a *newer* bridge whose TXT
/// means something else is skipped rather than misread.
const SUPPORTED_TXT_VERSION: u32 = 1;

/// How long a successful/failed diagnostics probe is trusted before re-probing.
/// The page is a convenience, not live state, so this is generous: it keeps a
/// browser-driven `/api/sources` poll from firing an HTTP request per source per
/// second at a Pi Zero.
pub const PROBE_TTL: Duration = Duration::from_secs(30);

/// Timeout for one diagnostics probe. A Pi Zero 2 W serving the page while
/// running the bridge answers in well under this; anything slower is treated as
/// absent rather than made the user wait.
const PROBE_TIMEOUT: Duration = Duration::from_millis(1500);

/// Stream parameters a bridge advertises, all straight from TXT. Used to match an
/// advert to a configured RTP source and to prefill a new one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BridgeStream {
    /// UDP port the bridge **sends to** — the port an RTP source must listen on.
    pub rtp_port: u16,
    /// Where it sends: the add-on host's address for unicast, or a multicast group.
    pub rtp_dest: String,
    pub rate: u32,
    pub channels: u16,
    /// Wire format (`S16LE`); informational, the receiver's is fixed.
    pub format: String,
}

/// One discovered bridge.
#[derive(Debug, Clone)]
pub struct BtBridge {
    /// mDNS instance fullname — stable identity, and the map key.
    pub fullname: String,
    /// Human label (the advert's instance name, e.g. `Bathroom Music`).
    pub display_name: String,
    /// mDNS hostname (`turnerstr-bluetooth.local.`), kept for display.
    pub hostname: String,
    /// Address to reach it at — IPv4 preferred, a routable IPv6 as fallback (see
    /// [`addr_from_service`]). `None` until mDNS resolves one, and the diagnostics
    /// URL needs it.
    pub addr: Option<IpAddr>,
    /// HTTP port of the diagnostics app (the advert's SRV port).
    pub diag_port: u16,
    /// Path the diagnostics page is served at (TXT `diag_path`, default `/`).
    pub diag_path: String,
    /// Advertised stream parameters.
    pub stream: BridgeStream,
    /// Last diagnostics probe: `(when, reachable)`. `None` = never probed.
    pub probe: Option<(Instant, bool)>,
}

impl BtBridge {
    /// URL of the diagnostics page, or `None` while the address is unresolved.
    pub fn diag_url(&self) -> Option<String> {
        let addr = self.addr?;
        Some(format!("http://{}{}", host_port(addr, self.diag_port), self.diag_path))
    }

    /// Whether the diagnostics app answered on the last probe (`false` if never).
    pub fn diag_ok(&self) -> bool {
        matches!(self.probe, Some((_, true)))
    }

    /// True when the last probe is older than [`PROBE_TTL`] (or never happened).
    fn probe_stale(&self, now: Instant) -> bool {
        match self.probe {
            None => true,
            Some((at, _)) => now.duration_since(at) >= PROBE_TTL,
        }
    }
}

/// Live discovered bridges, keyed by mDNS fullname. Shared with the API.
pub type SharedBtBridges = Arc<Mutex<BTreeMap<String, BtBridge>>>;

/// `host:port`, bracketing IPv6 so the result is a valid URL authority.
fn host_port(addr: IpAddr, port: u16) -> String {
    match addr {
        IpAddr::V4(v4) => format!("{v4}:{port}"),
        IpAddr::V6(v6) => format!("[{v6}]:{port}"),
    }
}

/// Instance label from the fullname (`Bathroom Music._pwrouter-btbridge._tcp.local.`
/// -> `Bathroom Music`).
fn display_name_from_fullname(fullname: &str) -> String {
    fullname.split("._pwrouter-btbridge._tcp").next().unwrap_or(fullname).trim().to_string()
}

/// Parse the TXT records into [`BridgeStream`] + the diagnostics path.
///
/// Returns `None` when the advert is not one we understand — a `ver` we don't
/// support, a missing/unparseable `rtp_port`, or a missing `rtp_dest`. Being
/// strict here is deliberate: a half-understood advert would prefill an RTP
/// source with wrong numbers, and a wrong port is indistinguishable from a
/// bridge that simply isn't sending.
fn parse_txt(get: &dyn Fn(&str) -> Option<String>) -> Option<(BridgeStream, String)> {
    let ver: u32 = get("ver").and_then(|s| s.parse().ok())?;
    if ver > SUPPORTED_TXT_VERSION {
        return None;
    }
    let rtp_port: u16 = get("rtp_port").and_then(|s| s.parse().ok())?;
    if rtp_port == 0 {
        return None;
    }
    let rtp_dest = get("rtp_dest").filter(|s| !s.trim().is_empty())?;
    let stream = BridgeStream {
        rtp_port,
        rtp_dest: rtp_dest.trim().to_string(),
        rate: get("rate").and_then(|s| s.parse().ok()).unwrap_or(48_000),
        channels: get("channels").and_then(|s| s.parse().ok()).unwrap_or(2),
        format: get("fmt").unwrap_or_else(|| "S16LE".to_string()),
    };
    let diag_path = match get("diag_path") {
        Some(p) if p.starts_with('/') => p,
        _ => "/".to_string(),
    };
    Some((stream, diag_path))
}

/// Best address to reach a bridge at: IPv4 if known, else a routable IPv6.
///
/// A `ServiceResolved` can arrive before the A record is known — observed on the
/// real LAN, where an 8 s browse produced an AAAA-only resolve for a bridge that
/// has both. Taking only IPv4 left `addr: None`, and with it no diagnostics link
/// at all, so a v6 fallback is the difference between a working link and none.
/// Link-local (`fe80::/10`) is skipped: it needs a `%scope` suffix that means
/// nothing in a URL handed to a browser on another host.
fn addr_from_service(info: &ResolvedService) -> Option<IpAddr> {
    if let Some(v4) = info.get_addresses_v4().into_iter().next() {
        return Some(IpAddr::V4(v4));
    }
    info.get_addresses().iter().find_map(|scoped| match scoped {
        ScopedIp::V6(v6) => {
            let a = *v6.addr();
            // `Ipv6Addr::is_unicast_link_local` is still unstable; check fe80::/10.
            (a.segments()[0] & 0xffc0 != 0xfe80).then_some(IpAddr::V6(a))
        }
        _ => None,
    })
}

/// Whether `candidate` should replace `current` as a bridge's address. IPv4 wins
/// over IPv6 whenever it becomes known, so an early AAAA-only resolve does not
/// pin the daemon to v6 for the rest of the session.
fn better_addr(current: Option<IpAddr>, candidate: Option<IpAddr>) -> Option<IpAddr> {
    match (current, candidate) {
        (_, None) => current,
        (None, c) => c,
        (Some(IpAddr::V6(_)), Some(IpAddr::V4(v4))) => Some(IpAddr::V4(v4)),
        (Some(cur), Some(cand)) if cur.is_ipv4() == cand.is_ipv4() => Some(cand),
        _ => current,
    }
}

/// Build a [`BtBridge`] from a resolved service, or `None` if the advert is not a
/// bridge advert we understand (see [`parse_txt`]).
fn bridge_from_service(info: &ResolvedService) -> Option<BtBridge> {
    let fullname = info.get_fullname().to_string();
    let display_name = display_name_from_fullname(&fullname);
    if display_name.is_empty() {
        return None;
    }
    let get = |k: &str| info.get_property_val_str(k).map(|s| s.to_string());
    let (stream, diag_path) = parse_txt(&get)?;
    // TXT `diag_port` wins over the SRV port only if it disagrees *and* parses —
    // the service file sets both, and the SRV port is the authority.
    let diag_port = match info.get_port() {
        0 => get("diag_port").and_then(|s| s.parse().ok())?,
        p => p,
    };
    Some(BtBridge {
        fullname,
        display_name,
        hostname: info.get_hostname().to_string(),
        addr: addr_from_service(info),
        diag_port,
        diag_path,
        stream,
        probe: None,
    })
}

/// Start browsing for bridges on the shared mDNS `daemon`, keeping `bridges` in
/// sync. Mirrors `pw_target_discovery::spawn`: resolves are merged, removes are
/// ignored (an mDNS TTL flap is not a bridge going away — and unlike an output,
/// a stale entry here cannot misroute audio, it can only offer a stale link,
/// which the probe then refuses).
pub fn spawn(daemon: &ServiceDaemon, bridges: SharedBtBridges, changes: ChangeNotifier) -> anyhow::Result<()> {
    let receiver = daemon.browse(BT_BRIDGE_SERVICE_TYPE)?;
    std::thread::Builder::new().name("btbridge-discovery".into()).spawn(move || {
        while let Ok(event) = receiver.recv() {
            if let ServiceEvent::ServiceResolved(info) = event {
                let Some(found) = bridge_from_service(&info) else {
                    continue;
                };
                let mut map = bridges.lock_recover();
                let notify = match map.get_mut(&found.fullname) {
                    Some(b) => {
                        let addr = better_addr(b.addr, found.addr);
                        let changed = b.stream != found.stream || b.diag_port != found.diag_port || b.addr != addr;
                        // A changed address invalidates the probe verdict: it was
                        // about the old endpoint.
                        if b.addr != addr {
                            b.probe = None;
                        }
                        b.addr = addr;
                        b.hostname = found.hostname;
                        b.diag_port = found.diag_port;
                        b.diag_path = found.diag_path;
                        // Stream parameters moving (a re-run of setup_pi_bridge.py
                        // with a new port) does not invalidate the probe — same
                        // host, same page.
                        b.stream = found.stream;
                        changed
                    }
                    None => {
                        tracing::info!(
                            "discovered Bluetooth bridge '{}' at {:?} (RTP -> {}:{}, diag port {})",
                            found.display_name,
                            found.addr,
                            found.stream.rtp_dest,
                            found.stream.rtp_port,
                            found.diag_port
                        );
                        map.insert(found.fullname.clone(), found);
                        true
                    }
                };
                drop(map);
                if notify {
                    let _ = changes.send(());
                }
            }
        }
        tracing::info!("bt-bridge discovery loop ended");
    })?;
    Ok(())
}

/// Probe every bridge whose diagnostics verdict is stale, in parallel, and store
/// the results. Called from the API before serving `/api/sources`, so the check
/// happens only when someone is looking — no background polling of Pi Zeros.
///
/// A probe passes only if the response *looks like the testing app* (see
/// [`looks_like_diag_app`]): a bare open port could be anything, and offering a
/// link to a random web server would be worse than offering none.
pub async fn refresh_probes(bridges: &SharedBtBridges) {
    let now = Instant::now();
    let stale: Vec<(String, String)> = {
        let map = bridges.lock_recover();
        map.values().filter(|b| b.probe_stale(now)).filter_map(|b| b.diag_url().map(|url| (b.fullname.clone(), url))).collect()
    };
    if stale.is_empty() {
        return;
    }
    // Concurrently, so one unreachable bridge doesn't add its whole timeout to
    // the request that triggered the refresh.
    let mut tasks = tokio::task::JoinSet::new();
    for (fullname, url) in stale {
        tasks.spawn(async move { (fullname, probe(&url).await) });
    }
    let mut results = Vec::new();
    while let Some(joined) = tasks.join_next().await {
        match joined {
            Ok(r) => results.push(r),
            // A probe task that dies leaves the bridge's verdict *unset*, which
            // the UI shows as "not answering" — so it gets logged rather than
            // dropped (that silence hid a panicking probe client for a while).
            Err(e) => tracing::warn!("bridge diagnostics probe task failed: {e}"),
        }
    }
    let mut map = bridges.lock_recover();
    let at = Instant::now();
    for (fullname, ok) in results {
        if let Some(b) = map.get_mut(&fullname) {
            if b.diag_ok() != ok {
                tracing::info!("bridge '{}' diagnostics page {}", b.display_name, if ok { "reachable" } else { "unreachable" });
            }
            b.probe = Some((at, ok));
        }
    }
}

/// The probe HTTP client, built once. A fresh `Client` per refresh would throw
/// away the connection pool and re-do TLS-less setup for every check.
///
/// `None` when a client cannot be built at all. That is not hypothetical: our
/// reqwest uses the `rustls` feature, i.e. `rustls-platform-verifier`, which
/// loads the **system** CA store — so in a runtime image without
/// `ca-certificates` every `build()` fails with "No CA certificates were loaded
/// from the system", even though this probe only ever speaks plain HTTP to a LAN
/// address. This used to fall back to `Client::new()`, which *panics* on exactly
/// that error; the panic happened inside a probe task, [`refresh_probes`]
/// dropped the `JoinError` silently, no verdict was ever stored, and every
/// bridge reported "diagnostics page not answering" while its page was perfectly
/// reachable. Hence: no panic, and say so in the log.
fn probe_client() -> Option<&'static reqwest::Client> {
    static CLIENT: std::sync::OnceLock<Option<reqwest::Client>> = std::sync::OnceLock::new();
    CLIENT
        .get_or_init(|| match reqwest::Client::builder().timeout(PROBE_TIMEOUT).build() {
            Ok(c) => Some(c),
            Err(e) => {
                tracing::warn!("no HTTP client for bridge diagnostics probes ({e}) — diagnostics links stay hidden");
                None
            }
        })
        .as_ref()
}

/// One diagnostics probe: `GET <url>api/state` must answer 200 with a body that
/// looks like the testing app.
///
/// Every negative verdict is logged: a probe that quietly returns `false` turns
/// into a UI badge claiming the page is down, so the log has to be able to say
/// whether that was a refused connection, an HTTP error, or a body that wasn't
/// the testing app's.
async fn probe(base_url: &str) -> bool {
    let Some(client) = probe_client() else {
        return false;
    };
    let url = format!("{}api/state", if base_url.ends_with('/') { base_url.to_string() } else { format!("{base_url}/") });
    let resp = match client.get(&url).timeout(PROBE_TIMEOUT).send().await {
        Ok(r) => r,
        Err(e) => {
            tracing::debug!("bridge diagnostics probe {url}: {e}");
            return false;
        }
    };
    if !resp.status().is_success() {
        tracing::debug!("bridge diagnostics probe {url}: HTTP {}", resp.status());
        return false;
    }
    match resp.text().await {
        Ok(body) => {
            let ok = looks_like_diag_app(&body);
            if !ok {
                tracing::debug!("bridge diagnostics probe {url}: answered, but not the testing app");
            }
            ok
        }
        Err(e) => {
            tracing::debug!("bridge diagnostics probe {url}: reading the body failed ({e})");
            false
        }
    }
}

/// Whether a `/api/state` body is the bluetooth-testing-app's. Checks for the
/// three top-level keys it always returns, so an unrelated service answering 200
/// on port 8080 does not earn a "Diagnostics" link.
fn looks_like_diag_app(body: &str) -> bool {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(body) else {
        return false;
    };
    v.get("audio").is_some() && v.get("graph").is_some() && v.get("counters").is_some()
}

/// Which discovered bridge feeds the RTP source listening on `port` with
/// `source_addr`, if any.
///
/// The match is on the advertised destination, which is exactly what the two ends
/// have to agree on anyway:
/// - **port must be equal** — a bridge sending to another port is not this source.
/// - **multicast source** (`source_addr` is a group): the bridge must advertise
///   that same group, or it is feeding a different group on the same port.
/// - **unicast source** (`0.0.0.0`, i.e. "any sender"): any destination matches,
///   since the source accepts whatever arrives on the port.
///
/// Ambiguity is resolved by returning `None` rather than guessing: two bridges
/// sending to the same port and group are indistinguishable from here, and a
/// wrong diagnostics link is worse than no link.
pub fn match_bridge<'a>(bridges: impl IntoIterator<Item = &'a BtBridge>, port: u16, source_addr: &str) -> Option<&'a BtBridge> {
    let want_group = is_multicast_addr(source_addr);
    let mut hits = bridges
        .into_iter()
        .filter(|b| b.stream.rtp_port == port && (!want_group || b.stream.rtp_dest.eq_ignore_ascii_case(source_addr.trim())));
    let first = hits.next()?;
    match hits.next() {
        Some(_) => None, // ambiguous — don't guess
        None => Some(first),
    }
}

/// Whether `addr` is a multicast group (IPv4 224.0.0.0/4 or IPv6 `ff00::/8`), as
/// opposed to the `0.0.0.0` "accept any sender" form.
fn is_multicast_addr(addr: &str) -> bool {
    match addr.trim().parse::<IpAddr>() {
        Ok(IpAddr::V4(v4)) => v4.is_multicast(),
        Ok(IpAddr::V6(v6)) => v6.is_multicast(),
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A TXT lookup closure over owned copies, standing in for `ResolvedService`
    /// (which cannot be constructed outside mdns-sd).
    fn txt(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let owned: Vec<(String, String)> = pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        move |k: &str| owned.iter().find(|(key, _)| key == k).map(|(_, v)| v.clone())
    }

    fn bridge(name: &str, port: u16, dest: &str) -> BtBridge {
        BtBridge {
            fullname: format!("{name}._pwrouter-btbridge._tcp.local."),
            display_name: name.to_string(),
            hostname: "pi.local.".into(),
            addr: Some(IpAddr::V4(std::net::Ipv4Addr::new(192, 168, 178, 78))),
            diag_port: 8080,
            diag_path: "/".into(),
            stream: BridgeStream { rtp_port: port, rtp_dest: dest.into(), rate: 48_000, channels: 2, format: "S16LE".into() },
            probe: None,
        }
    }

    #[test]
    fn parses_a_full_advert() {
        let get = txt(&[
            ("ver", "1"),
            ("role", "rtp-sender"),
            ("rtp_port", "46000"),
            ("rtp_dest", "239.255.42.42"),
            ("rate", "48000"),
            ("fmt", "S16LE"),
            ("channels", "2"),
            ("diag_path", "/"),
        ]);
        let (stream, path) = parse_txt(&get).unwrap();
        assert_eq!(stream.rtp_port, 46000);
        assert_eq!(stream.rtp_dest, "239.255.42.42");
        assert_eq!(stream.rate, 48_000);
        assert_eq!(stream.channels, 2);
        assert_eq!(path, "/");
    }

    #[test]
    fn optional_txt_falls_back_to_the_defaults_both_ends_use() {
        let get = txt(&[("ver", "1"), ("rtp_port", "46000"), ("rtp_dest", "192.168.178.22")]);
        let (stream, path) = parse_txt(&get).unwrap();
        assert_eq!(stream.rate, 48_000, "48 kHz is what setup_pi_bridge.py and rtp_source.rs default to");
        assert_eq!(stream.channels, 2);
        assert_eq!(stream.format, "S16LE");
        assert_eq!(path, "/");
    }

    /// A bridge that also reports AVRCP metadata advertises a *second role* and a
    /// `meta_ver` on the same advert, keeping `ver=1` (docs/source-metadata-plan.md
    /// §3.5). It must still parse here unchanged — the alternative, bumping `ver`,
    /// would make such a Pi vanish from any add-on not updated in lockstep, taking
    /// Bluetooth discovery and adoption with it.
    #[test]
    fn a_metadata_capable_bridge_still_parses() {
        let get = txt(&[
            ("ver", "1"),
            ("role", "rtp-sender,metadata"),
            ("meta_ver", "1"),
            ("rtp_port", "46000"),
            ("rtp_dest", "192.168.178.22"),
            ("rate", "48000"),
            ("fmt", "S16LE"),
            ("channels", "2"),
            ("diag_path", "/"),
        ]);
        let (stream, path) = parse_txt(&get).expect("an extended role must not break discovery");
        assert_eq!(stream.rtp_port, 46000);
        assert_eq!(path, "/");
    }

    #[test]
    fn a_newer_txt_version_is_skipped_not_guessed() {
        let get = txt(&[("ver", "2"), ("rtp_port", "46000"), ("rtp_dest", "x")]);
        assert!(parse_txt(&get).is_none());
    }

    #[test]
    fn unusable_adverts_are_rejected() {
        // No ver → not our advert at all.
        assert!(parse_txt(&txt(&[("rtp_port", "46000"), ("rtp_dest", "x")])).is_none());
        // Missing / zero / unparseable port → would prefill a wrong source.
        assert!(parse_txt(&txt(&[("ver", "1"), ("rtp_dest", "x")])).is_none());
        assert!(parse_txt(&txt(&[("ver", "1"), ("rtp_port", "0"), ("rtp_dest", "x")])).is_none());
        assert!(parse_txt(&txt(&[("ver", "1"), ("rtp_port", "no"), ("rtp_dest", "x")])).is_none());
        // Missing / blank destination → can't tell multicast from unicast.
        assert!(parse_txt(&txt(&[("ver", "1"), ("rtp_port", "46000")])).is_none());
        assert!(parse_txt(&txt(&[("ver", "1"), ("rtp_port", "46000"), ("rtp_dest", "  ")])).is_none());
    }

    #[test]
    fn a_relative_diag_path_is_normalized_to_root() {
        let get = txt(&[("ver", "1"), ("rtp_port", "46000"), ("rtp_dest", "x"), ("diag_path", "state")]);
        assert_eq!(parse_txt(&get).unwrap().1, "/");
    }

    #[test]
    fn display_name_strips_the_service_suffix() {
        assert_eq!(display_name_from_fullname("Bathroom Music._pwrouter-btbridge._tcp.local."), "Bathroom Music");
        assert_eq!(display_name_from_fullname("plain"), "plain");
    }

    #[test]
    fn diag_url_needs_a_resolved_address() {
        let mut b = bridge("Pi", 46000, "0.0.0.0");
        assert_eq!(b.diag_url().unwrap(), "http://192.168.178.78:8080/");
        b.addr = None;
        assert!(b.diag_url().is_none(), "no address → no link, rather than a broken one");
    }

    #[test]
    fn diag_url_brackets_ipv6() {
        let mut b = bridge("Pi", 46000, "0.0.0.0");
        b.addr = Some(IpAddr::V6("fe80::1".parse().unwrap()));
        assert_eq!(b.diag_url().unwrap(), "http://[fe80::1]:8080/");
    }

    #[test]
    fn unicast_source_matches_any_destination() {
        // `0.0.0.0` = accept any sender, so the bridge's destination (the HA
        // host's own address) can't be compared against it.
        let b = vec![bridge("Pi", 46000, "192.168.178.22")];
        assert!(match_bridge(&b, 46000, "0.0.0.0").is_some());
        assert!(match_bridge(&b, 47000, "0.0.0.0").is_none(), "different port is a different source");
    }

    #[test]
    fn multicast_source_must_match_the_group() {
        let b = vec![bridge("Pi", 46000, "239.255.42.42")];
        assert!(match_bridge(&b, 46000, "239.255.42.42").is_some());
        assert!(match_bridge(&b, 46000, "239.255.42.43").is_none(), "same port, other group = other stream");
    }

    #[test]
    fn two_candidates_are_ambiguous_and_yield_nothing() {
        let b = vec![bridge("A", 46000, "239.255.42.42"), bridge("B", 46000, "239.255.42.42")];
        assert!(match_bridge(&b, 46000, "239.255.42.42").is_none(), "a wrong link is worse than no link");
    }

    #[test]
    fn probe_body_must_look_like_the_testing_app() {
        assert!(looks_like_diag_app(r#"{"audio":{},"graph":{},"counters":{}}"#));
        // A random 200 on port 8080 must not earn a diagnostics link.
        assert!(!looks_like_diag_app(r#"{"hello":"world"}"#));
        assert!(!looks_like_diag_app("<html>router admin</html>"));
        // Partial shape (an older/other app) is refused too.
        assert!(!looks_like_diag_app(r#"{"audio":{},"graph":{}}"#));
    }

    #[test]
    fn probe_verdicts_expire() {
        let mut b = bridge("Pi", 46000, "0.0.0.0");
        let now = Instant::now();
        assert!(b.probe_stale(now), "never probed = stale");
        b.probe = Some((now, true));
        assert!(!b.probe_stale(now));
        assert!(b.probe_stale(now + PROBE_TTL));
        assert!(b.diag_ok());
        b.probe = Some((now, false));
        assert!(!b.diag_ok());
    }

    /// Live check of the one link the unit tests cannot cover: that `mdns-sd`
    /// actually sees the Avahi service file `setup_pi_bridge.py` writes, and that
    /// its TXT survives the round trip. Needs a real bridge on the LAN.
    ///
    ///   cargo test -p bridge-daemon btbridge_discovery_smoke_lan -- --ignored --nocapture
    #[test]
    #[ignore = "network smoke test: browses the real LAN for _pwrouter-btbridge._tcp bridges"]
    fn btbridge_discovery_smoke_lan() {
        let bridges: SharedBtBridges = Arc::new(Mutex::new(BTreeMap::new()));
        let (changes, _rx) = tokio::sync::broadcast::channel::<()>(16);
        let daemon = ServiceDaemon::new().expect("mdns daemon");
        spawn(&daemon, bridges.clone(), changes).expect("spawn bt-bridge discovery");
        std::thread::sleep(Duration::from_secs(8));

        {
            let found = bridges.lock().unwrap();
            println!("\n=== bt-bridge discovery: {} bridge(s) on the LAN ===", found.len());
            for b in found.values() {
                println!(
                    "  '{}' host={} addr={:?} RTP->{}:{} {}Hz/{}ch diag={:?}",
                    b.display_name,
                    b.hostname,
                    b.addr,
                    b.stream.rtp_dest,
                    b.stream.rtp_port,
                    b.stream.rate,
                    b.stream.channels,
                    b.diag_url()
                );
            }
            assert!(!found.is_empty(), "expected at least one _pwrouter-btbridge._tcp advert on the LAN");
        }

        // Probe the real diagnostics app: an advert alone must not produce a link.
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
        rt.block_on(refresh_probes(&bridges));
        for b in bridges.lock().unwrap().values() {
            println!("  probe '{}': diag_ok={} ({:?})", b.display_name, b.diag_ok(), b.diag_url());
        }
    }

    #[test]
    fn ipv4_is_preferred_and_never_downgraded() {
        let v4 = Some(IpAddr::V4(std::net::Ipv4Addr::new(192, 168, 178, 78)));
        let v6: Option<IpAddr> = Some("fdb7::1".parse().unwrap());
        // An AAAA-only first resolve must not pin us to v6 once the A arrives.
        assert_eq!(better_addr(v6, v4), v4);
        // ...and a later AAAA must not undo a known v4.
        assert_eq!(better_addr(v4, v6), v4);
        // Same family: take the newer value (the host may have moved).
        let other_v4 = Some(IpAddr::V4(std::net::Ipv4Addr::new(192, 168, 178, 79)));
        assert_eq!(better_addr(v4, other_v4), other_v4);
        // A resolve without any usable address leaves what we had.
        assert_eq!(better_addr(v4, None), v4);
        assert_eq!(better_addr(None, v4), v4);
    }

    #[test]
    fn multicast_detection() {
        assert!(is_multicast_addr("239.255.42.42"));
        assert!(is_multicast_addr("224.0.0.56"));
        assert!(!is_multicast_addr("0.0.0.0"));
        assert!(!is_multicast_addr("192.168.178.22"));
        assert!(!is_multicast_addr("not-an-address"));
    }
}
