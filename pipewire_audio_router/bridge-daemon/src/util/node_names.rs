//! Shared node-name conventions used across the daemon.
//!
//! There is no add-on `options.json` seeding — every source is created at
//! runtime via the REST API and persisted in the daemon's own stores
//! (sources/mod.rs). These constants are the node-name prefixes those stores
//! and the API handlers share.

/// Node-name prefix for sendspin output sink nodes (shared with
/// sources/mod.rs and the output classification in api/outputs.rs/routing.rs).
pub const SENDSPIN_NODE_PREFIX: &str = "sendspin-out-";

/// Node-name prefix for a discovered sendspin *device* — a virtual routing
/// output (no PipeWire node of its own; audio reaches it via whatever group
/// sink it's dialed into). This is what appears as a matrix column and on the
/// Outputs tab. See outputs/sendspin/discovery.rs.
pub const SENDSPIN_DEV_PREFIX: &str = "sendspin-dev-";

/// Node-name prefix for a discovered AirPlay-2 *receiver* — a virtual routing
/// output (no PipeWire node of its own), mirroring `SENDSPIN_DEV_PREFIX`. The
/// in-process AP2 sender (outputs/ap2/server.rs) streams RTP to it with libairptp PTP
/// timing; discovery (outputs/ap2/discovery.rs) registers it as a PTP peer. This is the
/// only AirPlay output path (the AirPlay-1/RAOP output path was removed).
pub const AP2_DEV_PREFIX: &str = "ap2-dev-";

/// Node-name prefix for a discovered **pw-sink** target — a remote PipeWire host
/// reached over RTP (`module-rtp-session`, mDNS-discovered). A virtual routing
/// output (no local PipeWire node of its own), mirroring `SENDSPIN_DEV_PREFIX` /
/// `AP2_DEV_PREFIX`. See outputs/pwsink/discovery.rs / outputs/pwsink/module_args.rs.
pub const PWSINK_DEV_PREFIX: &str = "pwsink-dev-";

/// mDNS/AppleMIDI **session-name** prefix for the sessions the daemon itself
/// advertises for pw-sink targets (`pwrouter-<slug>`, one per routed target —
/// see outputs/pwsink/server.rs). Distinct from `PWSINK_DEV_PREFIX` (the discovered
/// target's virtual-output node name). Discovery (outputs/pwsink/discovery.rs) filters
/// these out when it browses `_pipewire-audio._udp`, so the daemon never treats
/// its own advertised sessions as discoverable targets.
pub const PWSINK_SESSION_PREFIX: &str = "pwrouter-";

/// Node-name prefix for a **sync anchor** sink — one real `support.null-audio-sink`
/// per set of co-routed outputs, created by routing/sync_group/mod.rs. It is the shared
/// clock/timeline for a group: the sendspin server captures from it, so every
/// device in the group plays the same audio off one clock. Like the sendspin
/// group sink, it's internal plumbing — NOT matched by the matrix's output
/// classification, so it never appears as its own column.
pub const SYNC_GRP_PREFIX: &str = "sync-grp-";

/// Which backend carries a **virtual output**, resolved from its node name.
///
/// The node name is the only identity every layer shares — the matrix, the API, the
/// stores, the frontend and the HA integration all key on it — so "what kind of output is
/// this?" has to be answerable from it. It was, by a chain of
/// `if name.starts_with(SENDSPIN_DEV_PREFIX) … else if … else`, written out at a dozen
/// call sites. **Every one of those chains ends in an `else` that is a guess**, and that
/// is not a theoretical objection: three bugs came out of it.
///
/// * A `pwsink-dev-*` name fell through to the sendspin arm of the frontend's volume
///   dispatch, which *stored* the level as an intent for a device that will never connect
///   and answered `ok: true` — the "mute flips back on its own" report.
/// * The routing matrix's `volume`/`muted` chains had no pw-sink arm at all, so a host the
///   daemon could already drive showed no control.
/// * The alignment wizard answered "is there a level knob?" from a kind table of its own
///   and got both AP2 and pw-sink wrong, in opposite directions.
///
/// Matching on this enum makes the same mistake a **compile error**: adding a kind breaks
/// every `match` that has to decide something per kind, which is exactly the set of places
/// that need looking at. So prefer
///
/// ```ignore
/// match OutputKind::of(node_name) {
///     Some(OutputKind::Sendspin) => …,
///     Some(OutputKind::Airplay2) => …,
///     Some(OutputKind::PwSink)   => …,
///     None => …, // not a virtual output — a real PipeWire node, a source, a group sink
/// }
/// ```
///
/// over any chain of `starts_with`, and **never write a `_ =>` arm**: a wildcard puts the
/// silent `else` back.
///
/// It classifies *virtual outputs* only. `SENDSPIN_NODE_PREFIX` (a real sink node) and the
/// internal plumbing prefixes are deliberately `None`, and code that builds or strips a
/// name still uses the constants directly — that is a name-construction question, not a
/// which-kind one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum OutputKind {
    /// An ESPHome/sendspin speaker ([`SENDSPIN_DEV_PREFIX`]).
    Sendspin,
    /// An AirPlay-2 receiver ([`AP2_DEV_PREFIX`]).
    Airplay2,
    /// A remote PipeWire host reached over RTP ([`PWSINK_DEV_PREFIX`]).
    PwSink,
}

impl OutputKind {
    /// Every kind, for tests and for iterating capabilities. Ordered as the UI lists them.
    pub const ALL: [Self; 3] = [Self::Sendspin, Self::Airplay2, Self::PwSink];

    /// This kind's node-name prefix — the inverse of [`Self::of`].
    pub const fn prefix(self) -> &'static str {
        match self {
            Self::Sendspin => SENDSPIN_DEV_PREFIX,
            Self::Airplay2 => AP2_DEV_PREFIX,
            Self::PwSink => PWSINK_DEV_PREFIX,
        }
    }

    /// The kind of a virtual output, or `None` for anything else (a real PipeWire node, a
    /// source, a group sink, an internal plumbing node).
    pub fn of(node_name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| node_name.starts_with(k.prefix()))
    }

    /// The wire string the API and the frontend use (`OutputInfo::kind`). Kept here so a
    /// rename cannot drift between the listing that writes it and the pages that match it.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Sendspin => "sendspin",
            Self::Airplay2 => "airplay2",
            Self::PwSink => "pwsink",
        }
    }

    /// The kind in a sentence, for a message a user reads.
    pub const fn human(self) -> &'static str {
        match self {
            Self::Sendspin => "sendspin speaker",
            Self::Airplay2 => "AirPlay 2 receiver",
            Self::PwSink => "PipeWire host",
        }
    }
}

/// Turns an output's display name into something safe to use as a PipeWire
/// object name and (later) an HA entity-id fragment: lowercase,
/// spaces/punctuation collapsed to underscores.
pub fn slugify(name: &str) -> String {
    name.trim().to_lowercase().chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }).collect::<String>()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugify_collapses_punctuation_and_spaces() {
        assert_eq!(slugify("Pioneer VSX-934"), "pioneer_vsx_934");
        assert_eq!(slugify("Dusche"), "dusche");
    }

    #[test]
    fn classifies_every_virtual_output_kind_by_its_prefix() {
        assert_eq!(OutputKind::of("sendspin-dev-kitchen"), Some(OutputKind::Sendspin));
        assert_eq!(OutputKind::of("ap2-dev-dusche"), Some(OutputKind::Airplay2));
        assert_eq!(OutputKind::of("pwsink-dev-desk"), Some(OutputKind::PwSink));
    }

    /// The `None` cases are the point of the type: a real node, a source or internal
    /// plumbing must not be classified as *some* output kind, which is what the old
    /// `else` arms did.
    #[test]
    fn everything_that_is_not_a_virtual_output_is_none() {
        for name in ["sendspin-out-kitchen", "sync-grp-abc", "airplay-in", "alsa_output.pci-0000_00_1f.3", "", "ap2-dev"] {
            assert_eq!(OutputKind::of(name), None, "{name} should not classify as an output kind");
        }
    }

    /// `prefix` and `of` are inverses, so a kind added to one and forgotten in the other
    /// fails here rather than in production.
    #[test]
    fn prefix_and_of_are_inverses() {
        for kind in OutputKind::ALL {
            assert_eq!(OutputKind::of(&format!("{}whatever", kind.prefix())), Some(kind));
        }
    }
}
