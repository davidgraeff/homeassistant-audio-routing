//! The `libpipewire-module-rtp-session` arguments the agent loads to *become*
//! the pw-sink receiver — the dynamic replacement for the hand-written
//! `pipewire.conf.d/90-pwsink-receiver.conf` drop-in
//! (docs/receiver-agent-plan.md §7).
//!
//! Mirrors `bridge-daemon/src/pw_sink.rs`, which builds the sender-side args the
//! same way: an SPA-JSON object string handed to `pw_context_load_module`.
//!
//! Format is fixed S16BE / 48000 / 2: the daemon's AppleMIDI sender byte-swaps
//! to L16 big-endian on the wire, and the receiver must match (the same
//! wire-format trap documented in pw_sink.rs).

use std::fmt::Write as _;

/// The PipeWire module that implements the AppleMIDI/RTP session receiver.
pub const RTP_SESSION_MODULE_NAME: &str = "libpipewire-module-rtp-session";

/// Wire format the daemon sends (`applemidi_sender::SessionFormat::default()`).
const AUDIO_FORMAT: &str = "S16BE";
const AUDIO_RATE: u32 = 48000;
const AUDIO_CHANNELS: u32 = 2;

/// SPA-JSON `args` for the receive-side session.
///
/// * `ifname` — the LAN interface facing the daemon. `None` leaves it unset so
///   the module picks any, which is what an auto-detect failure should degrade
///   to rather than refusing to receive.
/// * `node_name` — the receive stream's node name. The stream is a
///   `Stream/Output/Audio` with `node.autoconnect`, so WirePlumber routes it to
///   the host's default sink exactly as the drop-in did.
///
/// Note `sess.discover-local` is deliberately left at its default (false): it is
/// only for sender and receiver on the *same* box, and setting it would make the
/// agent connect to sessions advertised by its own host.
pub fn rtp_session_module_args(ifname: Option<&str>, node_name: &str, description: &str) -> String {
    let mut a = String::new();
    a.push_str("{ ");
    if let Some(iface) = ifname {
        write!(a, "local.ifname = \"{iface}\" ").unwrap();
    }
    a.push_str("sess.media = \"audio\" ");
    write!(a, "audio.format = \"{AUDIO_FORMAT}\" ").unwrap();
    write!(a, "audio.rate = {AUDIO_RATE} ").unwrap();
    write!(a, "audio.channels = {AUDIO_CHANNELS} ").unwrap();
    a.push_str("audio.position = [ FL FR ] ");
    write!(
        a,
        "stream.props = {{ media.class = \"Stream/Output/Audio\" node.name = \"{node_name}\" node.description = \"{description}\" node.autoconnect = true }} "
    )
    .unwrap();
    a.push('}');
    a
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn args_match_the_drop_in_they_replace() {
        let args = rtp_session_module_args(Some("enp5s0"), "pwsink-in", "pw-router sink");
        assert!(args.starts_with("{ ") && args.ends_with('}'));
        assert!(args.contains("local.ifname = \"enp5s0\""));
        assert!(args.contains("sess.media = \"audio\""));
        // S16BE, not S16LE: the sender byte-swaps to L16 big-endian.
        assert!(args.contains("audio.format = \"S16BE\""));
        assert!(args.contains("audio.rate = 48000"));
        assert!(args.contains("audio.channels = 2"));
        assert!(args.contains("media.class = \"Stream/Output/Audio\""));
        assert!(args.contains("node.autoconnect = true"));
        // Only for same-box sender+receiver; must stay absent.
        assert!(!args.contains("sess.discover-local"));
    }

    #[test]
    fn ifname_is_omitted_when_unknown() {
        let args = rtp_session_module_args(None, "pwsink-in", "d");
        assert!(!args.contains("local.ifname"));
    }
}
