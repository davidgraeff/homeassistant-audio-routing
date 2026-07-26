//! AirPlay server builder and lifecycle.

use super::connection::RaopShared;
use super::types::*;
use crate::crypto::pairing::Pairing;
use crate::crypto::rsa::RsaKey;
use crate::error::{ServerError, ShairplayError};
use crate::net::mdns::{AirPlayServiceInfo, MdnsService, RAOP_CN_DEFAULT, RAOP_ET_DEFAULT};
use crate::net::server::{BindConfig, HttpServer};
use std::sync::Arc;

const AIRPORT_KEY: &str = include_str!("../../airport.key");

fn airport_rsakey() -> Arc<RsaKey> {
    use std::sync::OnceLock;
    static KEY: OnceLock<Arc<RsaKey>> = OnceLock::new();
    KEY.get_or_init(|| Arc::new(RsaKey::from_pem(AIRPORT_KEY).expect("built-in airport.key is invalid")))
        .clone()
}

fn random_hwaddr() -> Vec<u8> {
    use rand::RngCore;

    let mut hwaddr = [0u8; super::MAX_HWADDR_LEN];
    rand::thread_rng().fill_bytes(&mut hwaddr);
    // Locally administered, unicast MAC address.
    hwaddr[0] = (hwaddr[0] | 0x02) & !0x01;
    hwaddr.to_vec()
}

#[cfg(feature = "ap2")]
fn derive_pi_from_hwaddr(hwaddr: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(hwaddr);
    let hash = hasher.finalize();
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        hash[0],
        hash[1],
        hash[2],
        hash[3],
        hash[4],
        hash[5],
        (hash[6] & 0x0f) | 0x40, // version 4
        hash[7],
        (hash[8] & 0x3f) | 0x80, // variant 1
        hash[9],
        hash[10],
        hash[11],
        hash[12],
        hash[13],
        hash[14],
        hash[15]
    )
}

/// Builder for [`RaopServer`].
pub struct RaopServerBuilder {
    max_clients: usize,
    hwaddr: Option<Vec<u8>>,
    password: Option<String>,
    name: String,
    bind: BindConfig,
    #[cfg(feature = "ap2")]
    pairing_store: Option<Arc<dyn PairingStore>>,
    #[cfg(feature = "ap2")]
    mode: AirPlayMode,
    output_sample_rate: Option<u32>,
    output_max_channels: Option<u8>,
    /// AP1 codecs to advertise in the `cn` TXT record (empty → default ALAC).
    adv_codecs: Vec<Ap1Codec>,
    /// AP1 encryption modes to advertise in the `et` TXT record (empty → default none).
    adv_encryption: Vec<Ap1Encryption>,
    #[cfg(feature = "ap2")]
    pin: Option<String>,
    #[cfg(feature = "video")]
    video_handler: Option<Arc<dyn crate::raop::video::VideoHandler>>,
    #[cfg(feature = "hls")]
    hls_handler: Option<Arc<dyn crate::raop::hls::HlsHandler>>,
    /// Optional caller-provided mDNS daemon (Linux only). When set, the server
    /// advertises its `_raop._tcp`/`_airplay._tcp` records on this shared daemon
    /// instead of spawning its own — see [`crate::net::mdns`]'s `with_daemon`.
    /// The caller owns the daemon's lifetime.
    #[cfg(not(target_os = "macos"))]
    mdns_daemon: Option<mdns_sd::ServiceDaemon>,
}

impl Default for RaopServerBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl RaopServerBuilder {
    /// Create a new builder with default settings.
    pub fn new() -> Self {
        Self {
            max_clients: 10,
            hwaddr: None,
            password: None,
            name: "Shairplay".to_string(),
            bind: BindConfig::default(),
            #[cfg(feature = "ap2")]
            pairing_store: None,
            #[cfg(feature = "ap2")]
            mode: AirPlayMode::default(),
            output_sample_rate: None,
            output_max_channels: None,
            adv_codecs: Vec::new(),
            adv_encryption: Vec::new(),
            #[cfg(feature = "ap2")]
            pin: None,
            #[cfg(feature = "video")]
            video_handler: None,
            #[cfg(feature = "hls")]
            hls_handler: None,
            #[cfg(not(target_os = "macos"))]
            mdns_daemon: None,
        }
    }

    /// Advertise on a **caller-provided**, shared mDNS daemon instead of a
    /// private one (Linux only). Lets an embedder pin advertising to a single
    /// interface and share one `mDNS_daemon` thread across multiple advertisers.
    /// The daemon is cheaply cloneable; pass a clone and keep the daemon alive.
    #[cfg(not(target_os = "macos"))]
    pub fn mdns_daemon(mut self, daemon: mdns_sd::ServiceDaemon) -> Self {
        self.mdns_daemon = Some(daemon);
        self
    }

    /// Set the maximum number of concurrent connections. Default: 10.
    pub fn max_clients(mut self, n: usize) -> Self {
        self.max_clients = n;
        self
    }
    /// Set the 6-byte hardware address for mDNS registration.
    pub fn hwaddr(mut self, addr: impl Into<Vec<u8>>) -> Self {
        self.hwaddr = Some(addr.into());
        self
    }
    /// Set an optional HTTP Digest authentication password.
    pub fn password(mut self, pw: impl Into<String>) -> Self {
        self.password = Some(pw.into());
        self
    }
    /// Set the RTSP listening port. Default: 5000.
    pub fn port(mut self, port: u16) -> Self {
        self.bind.port = port;
        self
    }
    /// Set full bind configuration (address, port, auto-sensing, IPv6).
    pub fn bind(mut self, config: BindConfig) -> Self {
        self.bind = config;
        self
    }
    /// Set the AirPlay display name. Default: "Shairplay".
    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.name = name.into();
        self
    }

    /// Set a pairing store for persisting device keys across restarts.
    /// Without this, iPhones must re-pair on every server restart.
    #[cfg(feature = "ap2")]
    pub fn pairing_store(mut self, store: Arc<dyn PairingStore>) -> Self {
        self.pairing_store = Some(store);
        self
    }

    /// Set the AirPlay protocol mode. Default: [`AirPlayMode::AirPlay2`].
    ///
    /// Use [`AirPlayMode::AirPlay1`] to advertise as a classic receiver even
    /// when the `ap2` feature is compiled in.
    #[cfg(feature = "ap2")]
    pub fn mode(mut self, mode: AirPlayMode) -> Self {
        self.mode = mode;
        self
    }

    /// Set the desired output sample rate. The library resamples to this rate.
    /// Default: source native rate (no resampling).
    pub fn output_sample_rate(mut self, rate: u32) -> Self {
        self.output_sample_rate = Some(rate);
        self
    }

    /// Set the maximum output channels. Sources with more channels are mixed down.
    /// Sources with fewer channels are passed through (no upmixing).
    /// Default: pass through native channel count.
    pub fn output_max_channels(mut self, channels: u8) -> Self {
        self.output_max_channels = Some(channels);
        self
    }

    /// Set the AP1 codecs advertised in the `_raop._tcp` `cn` TXT record.
    ///
    /// The receiver auto-detects the actual codec per connection from the SDP,
    /// so this only affects discovery/negotiation. Order matters for some
    /// senders (PipeWire's `raop-discover` picks the first listed `cn`). Empty
    /// (the default) advertises ALAC only (`cn=1`).
    pub fn advertise_codecs(mut self, codecs: impl Into<Vec<Ap1Codec>>) -> Self {
        self.adv_codecs = codecs.into();
        self
    }

    /// Set the AP1 encryption/auth modes advertised in the `et` TXT record.
    ///
    /// The receiver auto-dispatches on what a sender actually negotiates, so
    /// this only affects discovery. Beware that some senders (PipeWire's
    /// `raop-discover`) select the **highest** advertised `et`, and PipeWire's
    /// RSA path is broken in 1.6.x. Empty (the default) advertises none (`et=0`).
    pub fn advertise_encryption(mut self, encryption: impl Into<Vec<Ap1Encryption>>) -> Self {
        self.adv_encryption = encryption.into();
        self
    }

    #[cfg(feature = "ap2")]
    /// Require normal HomeKit pair-setup with this one-time PIN.
    ///
    /// Without a PIN, AP2 uses the shairport-sync-style transient pairing
    /// profile. Setting a PIN changes mDNS/GET /info status flags so clients
    /// perform persistent M1-M6 pair-setup and later use pair-verify.
    pub fn pin(mut self, pin: impl Into<String>) -> Self {
        self.pin = Some(pin.into());
        self
    }

    #[cfg(feature = "video")]
    /// Set a video handler for screen mirroring (experimental).
    pub fn video_handler(mut self, handler: Arc<dyn crate::raop::video::VideoHandler>) -> Self {
        self.video_handler = Some(handler);
        self
    }

    #[cfg(feature = "hls")]
    /// Set an HLS handler for YouTube/video URL playback.
    pub fn hls_handler(mut self, handler: Arc<dyn crate::raop::hls::HlsHandler>) -> Self {
        self.hls_handler = Some(handler);
        self
    }

    /// Build the server with the given audio handler.
    pub fn build(self, handler: Arc<dyn AudioHandler>) -> Result<RaopServer, ShairplayError> {
        if self.max_clients == 0 {
            return Err(ServerError::MaxClients(0).into());
        }
        if let Some(password) = self.password.as_ref()
            && password.len() > super::MAX_PASSWORD_LEN
        {
            return Err(ServerError::InvalidPassword(password.len()).into());
        }
        let rsakey = airport_rsakey();
        let pairing = Arc::new(Pairing::generate()?);
        let hwaddr = match self.hwaddr {
            Some(addr) if addr.len() == super::MAX_HWADDR_LEN => addr,
            Some(addr) => return Err(ServerError::InvalidHwAddr(addr.len()).into()),
            None => random_hwaddr(),
        };

        #[cfg(feature = "ap2")]
        let pairing_id = derive_pi_from_hwaddr(&hwaddr);
        #[cfg(feature = "ap2")]
        let device_id = crate::util::hwaddr_airplay(&hwaddr);
        #[cfg(feature = "ap2")]
        let airplay_name = self.name.clone();

        #[cfg(feature = "ap2")]
        let pairing_store: Arc<dyn PairingStore> = self
            .pairing_store
            .unwrap_or_else(|| Arc::new(MemoryPairingStore::default()));
        // Resolve the accessory's long-term identity once: reuse a persisted seed
        // if the store has one, otherwise generate a random seed and hand it back
        // for persistence. (A store with no identity persistence — e.g. the default
        // in-memory one — yields a fresh identity each start; persist it via
        // `PairingStore::load_identity`/`save_identity` to avoid re-pairing.)
        #[cfg(feature = "ap2")]
        let identity_seed = pairing_store.load_identity().unwrap_or_else(|| {
            let seed = crate::crypto::pairing_homekit::generate_identity_seed();
            pairing_store.save_identity(seed);
            seed
        });

        let shared = Arc::new(RaopShared {
            rsakey,
            pairing,
            hwaddr: hwaddr.clone(),
            password: self.password.unwrap_or_default(),
            handler,
            #[cfg(feature = "ap2")]
            pairing_store,
            #[cfg(feature = "ap2")]
            identity_seed,
            output_sample_rate: self.output_sample_rate,
            output_max_channels: self.output_max_channels,
            #[cfg(feature = "ap2")]
            pin: self.pin,
            #[cfg(feature = "video")]
            video_handler: self.video_handler,
            #[cfg(feature = "video")]
            video_ekey: Arc::new(std::sync::RwLock::new(None)),
            #[cfg(feature = "video")]
            video_eiv: Arc::new(std::sync::RwLock::new(None)),
            #[cfg(feature = "ap2")]
            pairing_id,
            #[cfg(feature = "ap2")]
            device_id,
            #[cfg(feature = "ap2")]
            airplay_name,
            #[cfg(feature = "ap2")]
            active_audio: std::sync::Mutex::new(None),
            session_owner: std::sync::Mutex::new(None),
            connections: std::sync::Mutex::new(std::collections::HashMap::new()),
            #[cfg(feature = "hls")]
            hls_handler: self.hls_handler,
        });

        let raop_cn = join_txt(&self.adv_codecs, Ap1Codec::cn_value, RAOP_CN_DEFAULT);
        let raop_et = join_txt(&self.adv_encryption, Ap1Encryption::et_value, RAOP_ET_DEFAULT);

        let mut httpd = HttpServer::new(shared.clone(), self.max_clients);
        httpd.set_bind_config(self.bind.clone());

        Ok(RaopServer {
            shared,
            httpd,
            mdns: None,
            bind: self.bind,
            name: self.name,
            hwaddr,
            raop_cn,
            raop_et,
            #[cfg(feature = "ap2")]
            mode: self.mode,
            #[cfg(not(target_os = "macos"))]
            mdns_daemon: self.mdns_daemon,
        })
    }
}

/// The main AirPlay/RAOP server.
///
/// Listens for RTSP connections, handles pairing and encryption,
/// decodes audio, and delivers f32 PCM samples via [`AudioSession`].
/// Automatically registers mDNS services for network discovery.
pub struct RaopServer {
    shared: Arc<RaopShared>,
    httpd: HttpServer,
    mdns: Option<MdnsService>,
    bind: BindConfig,
    name: String,
    hwaddr: Vec<u8>,
    /// Pre-joined AP1 `cn` TXT record value (codecs advertised).
    raop_cn: String,
    /// Pre-joined AP1 `et` TXT record value (encryption modes advertised).
    raop_et: String,
    #[cfg(feature = "ap2")]
    mode: AirPlayMode,
    /// Caller-provided shared mDNS daemon to advertise on, if any (Linux only).
    #[cfg(not(target_os = "macos"))]
    mdns_daemon: Option<mdns_sd::ServiceDaemon>,
}

impl RaopServer {
    /// Create a new server builder.
    pub fn builder() -> RaopServerBuilder {
        RaopServerBuilder::new()
    }

    /// Ask the connection from `addr` (a peer IP) to close, if one exists. Used
    /// by an embedder to force-disconnect a specific sender. Returns once the
    /// signal is sent; the connection tears down shortly after.
    pub fn disconnect_client(&self, addr: &str) {
        self.shared.disconnect_client(addr);
    }

    /// Ask every currently-connected sender to close gracefully. Fires each
    /// live connection's close signal so it sends a clean FIN on its RTSP
    /// control socket — the receiver-side "session is gone" signal (a RAOP
    /// receiver cannot originate an RTSP TEARDOWN). Returns once the signals are
    /// sent; [`stop`](Self::stop) calls this and then awaits the connections so
    /// the FINs reach the wire before the port is released.
    pub fn disconnect_all(&self) {
        self.shared.disconnect_all();
    }

    /// Peer IP of the client that currently holds the audio session, if any.
    pub fn current_client(&self) -> Option<String> {
        self.shared.session_owner.lock().ok().and_then(|g| g.as_ref().map(|s| s.addr.clone()))
    }

    /// Start the server: bind ports, register mDNS services, begin accepting connections.
    ///
    /// mDNS registration is skipped when the `CI` environment variable is set
    /// (Bonjour/Avahi is typically unavailable on CI runners).
    pub async fn start(&mut self) -> Result<(), ShairplayError> {
        let _actual_port = self.httpd.start(self.bind.port).await?;

        // AirPlay 2 PTP sink: accept the sender's clock on 319/320 so it doesn't
        // stall the buffered-audio start. No-op if the ports can't be bound.
        #[cfg(feature = "ap2")]
        if self.mode == AirPlayMode::AirPlay2 {
            crate::net::ptp::spawn_ptp_sink().await;
        }

        if std::env::var("CI").is_err() {
            let info = self.service_info();
            // Advertise on the caller's shared daemon when provided (so all mDNS
            // registrations share one interface-restricted daemon thread),
            // otherwise spawn a private one.
            #[cfg(not(target_os = "macos"))]
            let mut mdns = match self.mdns_daemon.clone() {
                Some(daemon) => MdnsService::with_daemon(daemon),
                None => MdnsService::new()?,
            };
            #[cfg(target_os = "macos")]
            let mut mdns = MdnsService::new()?;
            mdns.register_raop(&info)?;
            #[cfg(feature = "ap2")]
            if self.mode == AirPlayMode::AirPlay2 {
                mdns.register_airplay(&info)?;
            }
            self.mdns = Some(mdns);
        }

        Ok(())
    }

    /// Whether the server is currently running.
    pub fn is_running(&self) -> bool {
        self.httpd.is_running()
    }

    /// Stop the server: unregister mDNS services and close all listeners.
    ///
    /// Live connections are closed gracefully first: `disconnect_all` signals
    /// each to send a clean FIN, and `httpd.stop()` then awaits them (bounded)
    /// so those FINs reach the wire before the listener drops. This is what lets
    /// a sender learn the session is stale on a restart, instead of streaming
    /// RTP into a session the new process never set up.
    pub async fn stop(&mut self) {
        if let Some(mut mdns) = self.mdns.take() {
            mdns.unregister_raop();
            mdns.unregister_airplay();
        }
        self.shared.disconnect_all();
        self.httpd.stop().await;
    }

    /// Get the mDNS service info for this server.
    pub fn service_info(&self) -> AirPlayServiceInfo {
        #[cfg(feature = "ap2")]
        {
            if self.mode == AirPlayMode::AirPlay2 {
                let (_, vk) = crate::crypto::pairing_homekit::identity_keypair(&self.shared.identity_seed);
                let pk_hex: String = vk.as_bytes().iter().map(|b| format!("{b:02x}")).collect();
                let pi = self.shared.pairing_id.clone();
                return AirPlayServiceInfo::new_airplay2(
                    &self.name,
                    self.httpd.port(),
                    &self.hwaddr,
                    !self.shared.password.is_empty(),
                    &pk_hex,
                    &pi,
                    self.shared.pin.is_some(),
                    self.shared.pairing_store.has_any_pairing(),
                );
            }
        }
        AirPlayServiceInfo::new(
            &self.name,
            self.httpd.port(),
            &self.hwaddr,
            !self.shared.password.is_empty(),
            &self.raop_cn,
            &self.raop_et,
        )
    }
}
