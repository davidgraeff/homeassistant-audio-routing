//! The agent↔daemon wire protocol (docs/receiver-agent-plan.md §5).
//!
//! JSON over one WebSocket, dialled *out* by the agent — nothing listens on the
//! receiver host. Mirrored byte-for-byte by `bridge-daemon/src/pwsink_agent.rs`;
//! the two files must stay in sync (they are small and versioned by
//! [`PROTOCOL_VERSION`], which the daemon checks in `Hello`).
//!
//! Two properties this shape deliberately has:
//!
//! * **Commands are an enum, not a passthrough.** The daemon can set volume, set
//!   mute, duck and unduck — it cannot ask the host to run, load or configure
//!   anything else. Even the receiver module's arguments are built by the agent
//!   from the parameters in `Welcome` (§5.1), never sent as a string.
//! * **Pairing happens on this same socket.** A first connection without a token
//!   becomes a pending pair request, carrying a short code the agent logs, so the
//!   person approving in the UI can check they are approving *this* host and not a
//!   stranger's.

use serde::{Deserialize, Serialize};

/// Bumped on any incompatible change; the daemon refuses mismatched majors.
pub const PROTOCOL_VERSION: u32 = 1;

/// Agent → daemon.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentMsg {
    /// First message on every connection. `token` is absent only when pairing.
    ///
    /// The three identity fields are sent raw rather than pre-combined: the daemon
    /// derives the pairing identity (`machine_id:user` — one agent per logged-in
    /// session, plan §13.2), the display label and the routing node name from them,
    /// so that naming lives in one place.
    Hello {
        protocol: u32,
        agent_version: String,
        machine_id: String,
        hostname: String,
        user: String,
        token: Option<String>,
        /// The pairing code this agent *process* offers, minted once at startup
        /// and logged there. Sent on every hello so a reconnect — a daemon
        /// restart, a ping timeout, a revoked token — keeps showing the code the
        /// host's journal already printed, instead of rotating it under the
        /// approver's feet.
        ///
        /// It is a **verification** string, not a secret: it is compared by a
        /// human across two channels (this host's log ↔ the add-on UI), and
        /// approval is still a human click that mints a daemon-side token. The
        /// daemon therefore validates its shape and mints its own if this is
        /// missing or malformed, so an older agent — or a rogue one offering a
        /// lookalike string — changes nothing.
        #[serde(default)]
        pair_code: Option<String>,
    },
    /// Pushed whenever anything changes, including local changes made by the user.
    State(HostState),
    /// A router session other than ours is also being received here (§7.1).
    ForeignSession {
        session: String,
    },
    Pong,
}

/// Daemon → agent.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DaemonMsg {
    /// Pairing accepted as pending: show/log `code`, then wait for `paired`. It is
    /// the code the agent offered in `Hello`, unless the daemon had to mint one.
    PairPending {
        code: String,
    },
    /// Approval granted — persist this token and reconnect with it.
    Paired {
        token: String,
    },
    /// Refused: unknown token, or a protocol mismatch. Never fatal to the agent —
    /// it drops a token the daemon no longer honours and keeps dialling in, so a
    /// host that was unpaired (or whose daemon lost its store) comes back as
    /// pairable on its own instead of needing a hand on that machine.
    Denied {
        reason: String,
    },
    /// Control granted. The agent becomes the receiver for `session_name` and
    /// enforces `keepalive_secs` (§9.2).
    Welcome {
        session_name: String,
        ifname: Option<String>,
        jitter_ms: Option<u32>,
        keepalive_secs: u64,
    },
    /// Stop being a receiver: the target was unrouted or removed. The agent
    /// unloads the module (its nodes disappear) but stays paired and connected.
    Release,
    SetVolume {
        volume: f32,
    },
    SetMute {
        muted: bool,
    },
    Duck {
        depth: f32,
        ramp_ms: u64,
    },
    Unduck {
        ramp_ms: u64,
    },
    Ping,
}

/// The host's controllable state, as the daemon sees it.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct HostState {
    /// Cubic 0.0-1.0 (the wpctl/HA `volume_level` scale), `None` when unknown.
    pub volume: Option<f32>,
    pub muted: Option<bool>,
    pub sink_name: Option<String>,
    /// Our receive stream exists and is linked to a sink.
    pub receiving: bool,
    /// Foreign streams are currently attenuated.
    pub ducked: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hello_without_a_token_is_a_pair_request() {
        let json = serde_json::to_string(&AgentMsg::Hello {
            protocol: PROTOCOL_VERSION,
            agent_version: "0.1.0".into(),
            machine_id: "abc".into(),
            hostname: "david-local".into(),
            user: "david".into(),
            token: None,
            pair_code: Some("A1B2C3".into()),
        })
        .unwrap();
        assert!(json.contains("\"type\":\"hello\""));
        assert!(json.contains("\"token\":null"));
        assert!(json.contains("\"pair_code\":\"A1B2C3\""));
    }

    #[test]
    fn a_hello_from_an_older_agent_still_parses() {
        // `pair_code` is `#[serde(default)]` precisely so this keeps working: the
        // daemon mints a code itself for an agent that offers none.
        let json = r#"{"type":"hello","protocol":1,"agent_version":"0.1.0","machine_id":"abc",
                       "hostname":"h","user":"u","token":null}"#;
        match serde_json::from_str::<AgentMsg>(json).unwrap() {
            AgentMsg::Hello { pair_code, .. } => assert_eq!(pair_code, None),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn commands_round_trip() {
        for msg in [
            DaemonMsg::SetVolume { volume: 0.42 },
            DaemonMsg::SetMute { muted: true },
            DaemonMsg::Duck {
                depth: 0.2,
                ramp_ms: 200,
            },
            DaemonMsg::Unduck { ramp_ms: 400 },
            DaemonMsg::Release,
            DaemonMsg::Ping,
            DaemonMsg::Welcome {
                session_name: "pwrouter-david_local".into(),
                ifname: Some("enp5s0".into()),
                jitter_ms: None,
                keepalive_secs: 10,
            },
        ] {
            let json = serde_json::to_string(&msg).unwrap();
            assert_eq!(serde_json::from_str::<DaemonMsg>(&json).unwrap(), msg);
        }
    }

    #[test]
    fn unknown_command_is_rejected_not_guessed() {
        // A daemon speaking a newer protocol must not have its messages
        // half-interpreted by an older agent.
        let err = serde_json::from_str::<DaemonMsg>(r#"{"type":"run_shell","cmd":"rm -rf /"}"#);
        assert!(err.is_err());
    }

    #[test]
    fn state_round_trips() {
        let state = HostState {
            volume: Some(0.5),
            muted: Some(false),
            sink_name: Some("alsa_output.x".into()),
            receiving: true,
            ducked: false,
        };
        let json = serde_json::to_string(&AgentMsg::State(state.clone())).unwrap();
        match serde_json::from_str::<AgentMsg>(&json).unwrap() {
            AgentMsg::State(s) => assert_eq!(s, state),
            other => panic!("unexpected {other:?}"),
        }
    }
}
