//! pw-sink spike — stream a **self-driving test tone to a remote PipeWire host**
//! over the proven native `rtp-sink` media path, and **advertise the session
//! over mDNS** (`_pipewire-audio._udp`, via the daemon's own storm-safe `mdns-sd`
//! — no Avahi/`module-rtp-session`, which can't run in the addon container; see
//! docs/pipewire-sink-spike-results.md). A/B oracle for the pw-sink transport
//! (mirrors `spike/ap2.rs`).
//!
//! `POST /api/spike/pw-sink {"target_ip":"…","freq":440}` creates a
//! `null-audio-sink` anchor (steady QUANT-1024 driver), loops a sine into it,
//! loads an `rtp-sink` unicasting S16LE/48k/stereo to the target, links the
//! anchor monitor into it (production follower-sink data path), and registers
//! the mDNS advert so a receiver can discover the session (format/port in TXT).
//! The receiver plays it via a static `rtp-source` (proven) or auto-discovery.
//! `DELETE /api/spike/pw-sink` tears everything down. One at a time.

use crate::outputs::pwsink;
use crate::pw::thread::{PwCommand, PwCommandSender, SharedState};
use crate::routing::node_id_for;
use crate::util::locks::LockRecover;
use mdns_sd::{ServiceDaemon, ServiceInfo};
use serde::Serialize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tokio::sync::{oneshot, Mutex};

/// The daemon-side rtp-sink node (a real, routable Audio/Sink; rtp-sink honors
/// this node.name, unlike rtp-session which auto-names).
const SINK_NODE_NAME: &str = "pwsink-spike";
/// The steady QUANT-1024 anchor null-sink that drives the tone into the sink.
const ANCHOR_NODE_NAME: &str = "pwsink-spike-anchor";
/// SAP/mDNS session name (advert instance + the receiver's session identity).
const SESS_NAME: &str = "pw-audio-router-spike";
/// mDNS service type module-rtp-session uses for audio sessions.
const MDNS_SERVICE_TYPE: &str = "_pipewire-audio._udp.local.";

/// One running pw-sink spike. Tear down through [`stop`] (Drop can't await);
/// Drop is a best-effort backstop on daemon exit.
struct PwSinkSpike {
    stop: Arc<AtomicBool>,
    player: Option<tokio::task::JoinHandle<()>>,
    anchor_node_id: u32,
    mdns: Option<(ServiceDaemon, String)>, // (advertise daemon, fullname) for unregister
    pw_cmd: PwCommandSender,
}

impl Drop for PwSinkSpike {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some((daemon, fullname)) = &self.mdns {
            let _ = daemon.unregister(fullname);
        }
        let (tx, _rx) = oneshot::channel();
        let _ = self.pw_cmd.send(PwCommand::Unload { node_name: SINK_NODE_NAME.to_string(), reply: tx });
        let (tx, _rx) = oneshot::channel();
        let _ = self.pw_cmd.send(PwCommand::DestroySinkNode { node_id: self.anchor_node_id, reply: tx });
    }
}

fn slot() -> &'static Arc<Mutex<Option<PwSinkSpike>>> {
    static SLOT: OnceLock<Arc<Mutex<Option<PwSinkSpike>>>> = OnceLock::new();
    SLOT.get_or_init(|| Arc::new(Mutex::new(None)))
}

/// What the start endpoint reports back.
#[derive(Debug, Serialize)]
pub struct PwSinkSpikeInfo {
    pub message: String,
    pub sink_node_id: u32,
}

/// `secs` of a stereo sine at `freq_hz` as a 48k/16-bit WAV (seamless loop at
/// integer `freq_hz`). Mirrors `spike::ap2::tone_wav`.
fn tone_wav(freq_hz: f32, secs: f32, rate: u32) -> Vec<u8> {
    let n = (secs * rate as f32).max(0.0) as usize;
    let mut pcm = Vec::with_capacity(n * 4);
    let amp = 8000.0f32; // ~ -12 dBFS
    let two_pi_f = 2.0 * std::f32::consts::PI * freq_hz;
    for i in 0..n {
        let t = i as f32 / rate as f32;
        let v = ((two_pi_f * t).sin() * amp) as i16;
        let b = v.to_le_bytes();
        pcm.extend_from_slice(&b);
        pcm.extend_from_slice(&b);
    }
    crate::audio::wav::build_wav(&pcm, rate, 16, 2)
}

/// Poll until `node_name` is present in the live registry (or give up).
async fn wait_for_node(pw: &SharedState, node_name: &str) -> Option<u32> {
    for _ in 0..40 {
        if let Some(id) = node_id_for(&pw.lock_recover(), node_name) {
            return Some(id);
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    None
}

/// Unload-then-load a module keyed by `node_name` (idempotent reload).
async fn reload_module(pw_cmd: &PwCommandSender, node_name: &str, module: &str, args: &str) -> Result<(), String> {
    let (tx, rx) = oneshot::channel();
    let _ = pw_cmd.send(PwCommand::Unload { node_name: node_name.to_string(), reply: tx });
    let _ = rx.await;
    let (tx, rx) = oneshot::channel();
    pw_cmd
        .send(PwCommand::Load { node_name: node_name.to_string(), module_name: module.to_string(), args: args.to_string(), reply: tx })
        .map_err(|_| "pipewire thread unavailable".to_string())?;
    match rx.await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => Err(e),
        Err(_) => Err("no reply from pipewire thread".to_string()),
    }
}

/// Register the mDNS advert for this session (format/port in TXT), via the
/// daemon's shared storm-safe advertise daemon. Returns `(daemon, fullname)`
/// for later unregister, or `None` if advertising is unavailable.
fn advertise(port: u16) -> Option<(ServiceDaemon, String)> {
    let daemon = crate::supervisor::shared_advertise_daemon()?;
    let props = [
        ("format", "S16LE"),
        ("rate", "48000"),
        ("channels", "2"),
        ("position", "[ FL FR ]"),
        ("subtype", "audio"),
        ("sess.name", SESS_NAME),
    ];
    let host = format!("{SESS_NAME}.local.");
    match ServiceInfo::new(MDNS_SERVICE_TYPE, SESS_NAME, &host, "", port, &props[..]) {
        Ok(si) => {
            let si = si.enable_addr_auto();
            let fullname = si.get_fullname().to_string();
            match daemon.register(si) {
                Ok(()) => {
                    tracing::info!("pw-sink spike: advertised '{SESS_NAME}' over mDNS ({MDNS_SERVICE_TYPE}) port {port}");
                    Some((daemon, fullname))
                }
                Err(e) => {
                    tracing::warn!("pw-sink spike: mDNS register failed: {e}");
                    None
                }
            }
        }
        Err(e) => {
            tracing::warn!("pw-sink spike: mDNS ServiceInfo build failed: {e}");
            None
        }
    }
}

async fn cleanup(pw_cmd: &PwCommandSender, anchor_id: u32) {
    let (tx, rx) = oneshot::channel();
    let _ = pw_cmd.send(PwCommand::Unload { node_name: SINK_NODE_NAME.to_string(), reply: tx });
    let _ = rx.await;
    let (tx, rx) = oneshot::channel();
    let _ = pw_cmd.send(PwCommand::DestroySinkNode { node_id: anchor_id, reply: tx });
    let _ = rx.await;
}

/// Start the spike: anchor + rtp-sink to `target_ip` + monitor link + mDNS
/// advert + looping tone. `ifname` pins the rtp-sink egress to the LAN iface.
pub async fn start(
    pw: &SharedState,
    pw_cmd: &PwCommandSender,
    target_ip: &str,
    freq_hz: f32,
    ifname: Option<&str>,
) -> Result<PwSinkSpikeInfo, String> {
    stop().await; // one at a time

    // 1. Anchor null-sink (steady QUANT-1024 driver).
    let (tx, rx) = oneshot::channel();
    pw_cmd
        .send(PwCommand::CreateSinkNode { node_name: ANCHOR_NODE_NAME.to_string(), reply: tx })
        .map_err(|_| "pipewire thread unavailable".to_string())?;
    match rx.await {
        Ok(Ok(())) => {}
        other => return Err(format!("failed to create anchor sink: {other:?}")),
    }
    let Some(anchor_id) = wait_for_node(pw, ANCHOR_NODE_NAME).await else {
        return Err("anchor sink did not appear in the graph".to_string());
    };

    // 2. rtp-sink unicasting to the target.
    let port = pwsink::module_args::DEFAULT_PWSINK_PORT;
    let sink_args = pwsink::module_args::rtp_sink_module_args(SINK_NODE_NAME, SESS_NAME, target_ip, port, ifname);
    if let Err(e) = reload_module(pw_cmd, SINK_NODE_NAME, pwsink::module_args::PWSINK_MODULE_NAME, &sink_args).await {
        cleanup(pw_cmd, anchor_id).await;
        return Err(format!("failed to load rtp-sink: {e}"));
    }
    if wait_for_node(pw, SINK_NODE_NAME).await.is_none() {
        cleanup(pw_cmd, anchor_id).await;
        return Err("rtp-sink node did not appear in the graph".to_string());
    }

    // 3. Link the anchor's monitor into the rtp-sink (follower-sink data path).
    crate::routing::ensure_monitor_link_by_name(pw, pw_cmd, ANCHOR_NODE_NAME, SINK_NODE_NAME).await;

    // 4. Advertise the session over mDNS for discovery.
    let mdns = advertise(port);

    // 5. Loop the test tone into the anchor.
    let wav = tone_wav(freq_hz, 1.0, 48_000);
    let stop_flag = Arc::new(AtomicBool::new(false));
    let player = tokio::task::spawn_blocking({
        let stop_flag = stop_flag.clone();
        move || {
            if let Err(e) = crate::pw::player::play_loop_to_target(anchor_id, &wav, stop_flag) {
                tracing::warn!("pw-sink spike tone player ended: {e}");
            }
        }
    });

    *slot().lock().await =
        Some(PwSinkSpike { stop: stop_flag, player: Some(player), anchor_node_id: anchor_id, mdns, pw_cmd: pw_cmd.clone() });
    tracing::info!("pw-sink spike: {freq_hz:.0} Hz -> {target_ip}:{port} (anchor {anchor_id} -> rtp-sink), advertised over mDNS");
    Ok(PwSinkSpikeInfo {
        message: format!(
            "streaming {freq_hz:.0} Hz to {target_ip}:{port} via anchor+rtp-sink; advertised '{SESS_NAME}' over mDNS (_pipewire-audio._udp)"
        ),
        sink_node_id: anchor_id,
    })
}

/// Stop the spike: unregister mDNS, signal + join the player, unload rtp-sink,
/// destroy the anchor.
pub async fn stop() {
    let spike = slot().lock().await.take();
    if let Some(mut s) = spike {
        s.stop.store(true, Ordering::Relaxed);
        if let Some((daemon, fullname)) = s.mdns.take() {
            let _ = daemon.unregister(&fullname);
        }
        if let Some(t) = s.player.take() {
            let _ = t.await;
        }
        cleanup(&s.pw_cmd, s.anchor_node_id).await;
    }
}
