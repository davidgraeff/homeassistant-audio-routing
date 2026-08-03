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

/// Node name of the receive stream. Fixed rather than daemon-supplied: the agent
/// identifies its own stream by `rtp.session` anyway (plan §7.1), and a stable
/// name keeps a host's graph readable.
pub const RECEIVE_NODE_NAME: &str = "pwsink-in";
pub const RECEIVE_NODE_DESCRIPTION: &str = "pw-router sink";

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
/// * `jitter_ms` — receiver-side jitter buffer (`sess.latency.msec`). Left unset
///   by default: the module warns when it is not an integer multiple of the
///   negotiated `rtp.ptime` (observed: `100` vs `6.458`), and the right value
///   depends on the sender's packet time, so it is opt-in rather than guessed.
///
/// Note `sess.discover-local` is deliberately left at its default (false): it is
/// only for sender and receiver on the *same* box, and setting it would make the
/// agent connect to sessions advertised by its own host.
///
/// The daemon supplies *parameters* to this function, never a finished argument
/// string: a module-args passthrough would let whatever is on the other end of
/// the WebSocket reconfigure the host's audio arbitrarily (plan §5.1).
pub fn rtp_session_module_args(
    ifname: Option<&str>,
    node_name: &str,
    description: &str,
    jitter_ms: Option<u32>,
) -> String {
    let mut a = String::new();
    a.push_str("{ ");
    if let Some(iface) = ifname {
        write!(a, "local.ifname = \"{iface}\" ").unwrap();
    }
    a.push_str("sess.media = \"audio\" ");
    if let Some(ms) = jitter_ms {
        write!(a, "sess.latency.msec = {ms} ").unwrap();
    }
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
        let args = rtp_session_module_args(Some("enp5s0"), "pwsink-in", "pw-router sink", None);
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
        // Not guessed unless asked for.
        assert!(!args.contains("sess.latency.msec"));
    }

    #[test]
    fn ifname_is_omitted_when_unknown() {
        let args = rtp_session_module_args(None, "pwsink-in", "d", None);
        assert!(!args.contains("local.ifname"));
    }

    #[test]
    fn jitter_buffer_is_included_when_given() {
        let args = rtp_session_module_args(None, "pwsink-in", "d", Some(60));
        assert!(args.contains("sess.latency.msec = 60"));
    }
}
