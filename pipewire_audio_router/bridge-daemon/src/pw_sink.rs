//! pw-sink output backend module concerns: the SPA-JSON `args` strings for the
//! two native PipeWire modules that stream audio to a **remote PipeWire host**.
//!
//! Unlike Sendspin/AP2 (which push a raw PCM channel to a receiver that cannot
//! take a PipeWire stream), a remote PipeWire host speaks PipeWire/RTP natively,
//! so the "sender" here is just two libpipewire modules loaded into the daemon's
//! own context via `PwCommand::Load` (see pw/thread.rs / `pw_control::module`), exactly
//! like the rtp-source in sources/rtp.rs:
//!
//!   * **`libpipewire-module-rtp-sink`** — one per target. A real `Audio/Sink`
//!     node whose input is fed from the graph (the group anchor's monitor, or —
//!     in the spike — a test-tone `pw::stream`). It RTP-unicasts S16LE/48k/stereo
//!     to `destination.ip:port` and flags itself `sess.sap.announce = true`.
//!   * **`libpipewire-module-rtp-sap`** — one instance for the whole daemon. It
//!     SAP-announces every sink flagged `sess.sap.announce = true`, so a remote
//!     host running rtp-sap in discover mode auto-creates the matching source
//!     (zero static config on the receiver — the "no manually added nodes" goal).
//!
//! The jitter buffer (`sess.latency.msec`) is a *receiver-side* concern (it sets
//! the remote rtp-sap's target buffer), so it is not in the sender args here; it
//! is configured on the receiving host. See docs/pipewire-sink-roadmap.md §5 and
//! docs/pipewire-sink-spike-results.md.

use std::fmt::Write as _;

/// The PipeWire module that provides an RTP sink (one loaded per target).
pub const PWSINK_MODULE_NAME: &str = "libpipewire-module-rtp-sink";

/// Default RTP media port. 5004 is the AES67/RTP convention (see the shipped
/// `pipewire-aes67.conf`). One port per target session is fine on unicast.
pub const DEFAULT_PWSINK_PORT: u16 = 5004;

/// SPA-JSON `args` for one `rtp-sink` streaming to `dest_ip:port`.
///
/// * `node_name` — the local `Audio/Sink` node (routing-matrix identity).
/// * `sess_name` — the SAP session name; the remote receiver sees it as
///   `rtp.session`, so it is how a target is identified per-session.
/// * `dest_ip`   — the target host's unicast IPv4. Because the media is unicast,
///   only that host receives the audio even though the SAP announcement is
///   multicast (per-target routing — spike-results §3).
///
/// Fixed to S16LE/48000/2 to match the 48 kHz internal bus (architecture.md §8),
/// so nothing resamples on this path. `node.always-process = true` keeps the
/// sink clocked so it emits RTP steadily; `sess.sap.announce = true` opts it
/// into the rtp-sap announcer below.
///
/// **Wire format note (proven on the real LAN):** although the SDP advertises
/// `L16` (RFC 3551 = big-endian), PipeWire actually puts the bytes on the wire in
/// the configured native format — **S16LE** — and does NOT byte-swap to
/// RFC-canonical big-endian. So the **receiver must use `audio.format=S16LE`**
/// (S16BE gives loud byte-swapped noise). See docs/pipewire-sink-spike-results.md.
pub fn rtp_sink_module_args(node_name: &str, sess_name: &str, dest_ip: &str, port: u16, ifname: Option<&str>) -> String {
    let mut a = String::new();
    a.push_str("{ ");
    if let Some(iface) = ifname {
        write!(a, "local.ifname = \"{iface}\" ").unwrap();
    }
    write!(a, "destination.ip = \"{dest_ip}\" ").unwrap();
    write!(a, "destination.port = {port} ").unwrap();
    a.push_str("net.ttl = 16 ");
    // 1 ms packets: low latency, and every device copes (aes67.conf note).
    a.push_str("sess.min-ptime = 2 ");
    a.push_str("sess.max-ptime = 2 ");
    write!(a, "sess.name = \"{sess_name}\" ").unwrap();
    a.push_str("sess.media = \"audio\" ");
    a.push_str("audio.format = \"S16LE\" ");
    a.push_str("audio.rate = 48000 ");
    a.push_str("audio.channels = 2 ");
    write!(
        a,
        "stream.props = {{ media.class = \"Audio/Sink\" node.name = \"{node_name}\" node.description = \"{sess_name}\" sess.sap.announce = true node.always-process = true }} "
    )
    .unwrap();
    a.push('}');
    a
}

/// SPA-JSON `args` for an `rtp-sap` announcer targeting one host by **unicast**.
///
/// `sap_dest_ip` is the target host's IP — the SAP/SDP is unicast there
/// (`sap.ip = <target>`), not to the multicast group, because multicast SAP does
/// not survive consumer routers (spike-results §). This makes the announcer
/// **per-target** (one alongside each target's rtp-sink), which is fine — it also
/// makes announcements naturally scoped to the intended receiver.
///
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sink_args_are_a_braced_object_with_unicast_dest_and_sap_flag() {
        let args = rtp_sink_module_args("pwsink-spike", "pw-router-spike", "192.168.178.21", DEFAULT_PWSINK_PORT, None);
        assert!(args.starts_with("{ ") && args.ends_with('}'));
        assert!(args.contains("destination.ip = \"192.168.178.21\""));
        assert!(args.contains("destination.port = 5004"));
        assert!(args.contains("audio.format = \"S16LE\""));
        assert!(args.contains("audio.rate = 48000"));
        assert!(args.contains("audio.channels = 2"));
        assert!(args.contains("media.class = \"Audio/Sink\""));
        assert!(args.contains("node.name = \"pwsink-spike\""));
        assert!(args.contains("sess.sap.announce = true"));
        assert!(args.contains("node.always-process = true"));
        assert!(!args.contains("local.ifname"));
    }

    #[test]
    fn sink_args_pin_the_interface_when_given() {
        let args = rtp_sink_module_args("pwsink-spike", "s", "192.168.178.21", 5004, Some("end0"));
        assert!(args.contains("local.ifname = \"end0\""));
    }
}
