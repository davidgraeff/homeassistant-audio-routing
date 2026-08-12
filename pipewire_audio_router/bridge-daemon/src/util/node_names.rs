//! Shared node-name conventions used across the daemon.
//!
//! There is no add-on `options.json` seeding — every source is created at
//! runtime via the REST API and persisted in the daemon's own stores
//! (sources.rs). These constants are the node-name prefixes those stores
//! and the API handlers share.

/// Node-name prefix for sendspin output sink nodes (shared with
/// sources.rs and the output classification in api.rs/routing.rs).
pub const SENDSPIN_NODE_PREFIX: &str = "sendspin-out-";

/// Node-name prefix for a discovered sendspin *device* — a virtual routing
/// output (no PipeWire node of its own; audio reaches it via whatever group
/// sink it's dialed into). This is what appears as a matrix column and on the
/// Outputs tab. See sendspin_discovery.rs.
pub const SENDSPIN_DEV_PREFIX: &str = "sendspin-dev-";

/// Node-name prefix for a discovered AirPlay-2 *receiver* — a virtual routing
/// output (no PipeWire node of its own), mirroring `SENDSPIN_DEV_PREFIX`. The
/// in-process AP2 sender (ap2_server.rs) streams RTP to it with libairptp PTP
/// timing; discovery (ap2_discovery.rs) registers it as a PTP peer. This is the
/// only AirPlay output path (the AirPlay-1/RAOP output path was removed).
pub const AP2_DEV_PREFIX: &str = "ap2-dev-";

/// Node-name prefix for a discovered **pw-sink** target — a remote PipeWire host
/// reached over RTP (`module-rtp-session`, mDNS-discovered). A virtual routing
/// output (no local PipeWire node of its own), mirroring `SENDSPIN_DEV_PREFIX` /
/// `AP2_DEV_PREFIX`. See pw_target_discovery.rs / pw_sink.rs.
pub const PWSINK_DEV_PREFIX: &str = "pwsink-dev-";

/// mDNS/AppleMIDI **session-name** prefix for the sessions the daemon itself
/// advertises for pw-sink targets (`pwrouter-<slug>`, one per routed target —
/// see pwsink_server.rs). Distinct from `PWSINK_DEV_PREFIX` (the discovered
/// target's virtual-output node name). Discovery (pw_target_discovery.rs) filters
/// these out when it browses `_pipewire-audio._udp`, so the daemon never treats
/// its own advertised sessions as discoverable targets.
pub const PWSINK_SESSION_PREFIX: &str = "pwrouter-";

/// Node-name prefix for a **sync anchor** sink — one real `support.null-audio-sink`
/// per set of co-routed outputs, created by sync_group.rs. It is the shared
/// clock/timeline for a group: the sendspin server captures from it, so every
/// device in the group plays the same audio off one clock. Like the sendspin
/// group sink, it's internal plumbing — NOT matched by the matrix's output
/// classification, so it never appears as its own column.
pub const SYNC_GRP_PREFIX: &str = "sync-grp-";

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
}
