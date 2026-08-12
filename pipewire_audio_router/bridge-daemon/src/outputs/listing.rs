//! The output *listing* — what an output is, as the rest of the daemon sees it.
//!
//! [`OutputInfo`] is the assembled per-output view: the store's adoption verdict,
//! the live discovery registry's presence and address, the effective latency, and
//! for AirPlay-2 its advertised features and PTP-lock health. [`collect_outputs`]
//! builds it from those sources; [`outputs_listings`] partitions it into adopted
//! and merely discovered.
//!
//! It lives here, not with the HTTP handlers that serve it, because it has two
//! consumers: the Outputs page (`api/outputs.rs`) and the routing matrix frame
//! (`routing/mod.rs`). While it sat inside the API module, the routing layer had
//! to depend on `api::` to name its own matrix column type — the last thing
//! keeping `api/` from being a leaf (plan §6).

use crate::state::AppState;
use crate::store::outputs::OutputState;
use crate::util::locks::LockRecover;
use crate::util::node_names::{AP2_DEV_PREFIX, PWSINK_DEV_PREFIX, SENDSPIN_DEV_PREFIX};
use airplay_core::features::Features;
use serde::Serialize;

// ---- Outputs listing ------------------------------------------------------
//
// Every output is now a *virtual*, auto-discovered device (sendspin `sendspin-dev-*`
// or AirPlay-2 `ap2-dev-*`) — there is no manual output store and no runtime
// module load/unload for outputs (the AirPlay-1/RAOP output path was removed).

/// An output for the Outputs tab, in both origins the UI shows:
/// - **discovered**: present via mDNS — `present: true`, `configured: false`.
/// - **offline**: adopted (or referenced by saved routing intent) but not
///   currently discovered — `present: false` (shown grayed; re-linked when it
///   returns).
///
/// The same shape serves both listings: `GET /api/outputs` (adopted only — the
/// system's actual outputs, which is what the routing matrix, the group editors
/// and the HA integration mean by "output") and `GET /api/outputs/discovered`
/// (everything found but not adopted). `state` says which bucket an entry is in.
/// Decoded AirPlay-2 capability flags (from the `features` TXT bitmask), surfaced
/// in `/api/outputs` for the Diagnostics capability card. `raw` is the canonical
/// `0xLOWER,0xUPPER` string for copy/paste + cross-referencing.
#[derive(Serialize)]
pub(crate) struct Ap2FeaturesInfo {
    pub(crate) raw: String,
    /// bit 41 — PTP timing supported.
    pub(crate) ptp: bool,
    /// bit 40 — buffered-audio mode supported (implies PTP is mandatory in that mode).
    pub(crate) buffered_audio: bool,
    /// bit 48 — HomeKit transient pairing (how we connect, PIN 3939).
    pub(crate) transient_pairing: bool,
}

#[derive(Serialize)]
pub(crate) struct OutputInfo {
    pub(crate) node_name: String,
    pub(crate) name: String,
    /// Is `name` the user's own (a rename), rather than the one discovery
    /// reported? Only then is there anything for a UI's "use the discovered
    /// name again" control to clear, so it can offer that honestly instead of
    /// showing a button that does nothing.
    pub(crate) renamed: bool,
    /// `"sendspin"` or `"airplay2"` — for the Type column / badge.
    pub(crate) kind: &'static str,
    /// Node/device is live right now.
    pub(crate) present: bool,
    /// Always `false` now that every output is mDNS auto-discovered (kept for
    /// the API shape / a possible future manually-added output kind).
    pub(crate) configured: bool,
    /// The user's verdict on this discovered device (store/outputs.rs):
    /// `"adopted"` (a real output), `"discovered"` (found, awaiting a decision)
    /// or `"ignored"` (dismissed). Only `"adopted"` outputs are routable and
    /// exposed to Home Assistant.
    pub(crate) state: &'static str,
    /// Connection details (from the mDNS-resolved address).
    pub(crate) ip: Option<String>,
    pub(crate) port: Option<u16>,
    pub(crate) encryption: Option<String>,
    /// Per-output latency override in ms; `None` = the type's built-in default.
    /// For AirPlay-2 it's the render delay (outputs/ap2/server.rs, default 0); for
    /// pw-sink it's the receiver's playout delay / jitter buffer
    /// (`routing::sync_settings::DEFAULT_PWSINK_JITTER_MS`). Not meaningful for sendspin
    /// (uses a separate static-delay knob).
    pub(crate) latency_ms: Option<u16>,
    /// The latency this output is **actually running** in ms — the override
    /// above, or the type's default when there is none. Sent so the UI can put a
    /// slider at the running value without hardcoding the daemon's defaults (it
    /// used to carry its own copy of the AP2 1500, which then went stale).
    /// `None` for kinds with no such knob (sendspin).
    pub(crate) latency_effective_ms: Option<u16>,
    /// AirPlay-2 only: PTP-lock health. `Some(true)` = the receiver is currently
    /// returning gPTP to our grandmaster (heard recently); `Some(false)` = registered
    /// but not exchanging gPTP; `None` = not an AP2 output (or PTP not started). NOTE:
    /// since we stream realtime ALAC (type 96), a single receiver renders fine WITHOUT
    /// an active lock (it free-runs off the PT=87 anchors) — a lock only matters for
    /// multi-room drift. So `false` is only alarming when `ptp_relevant` is true; the
    /// UI badge keys off both.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) ptp_locked: Option<bool>,
    /// AirPlay-2 only: seconds since the last gPTP packet from the receiver (lock age);
    /// `None` if never seen / not AP2. Small = healthy.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) ptp_lock_age_s: Option<u64>,
    /// AirPlay-2 only: does the receiver *advertise* PTP support (features bit 41)?
    /// `None` if not AP2 or features weren't seen. A device that doesn't advertise PTP
    /// will never lock, so the UI shouldn't alarm about it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) ptp_supported: Option<bool>,
    /// AirPlay-2 only: is a live PTP lock actually *relevant* for this output right now?
    /// True only when the receiver is present AND shares its source-set with ≥1 other
    /// present AP2 receiver (a multi-room group, where drift is audible). A lone AP2
    /// output plays realtime fine unlocked, so the UI shows an unlocked-but-single-room
    /// device as neutral, not alarming.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) ptp_relevant: Option<bool>,
    /// AirPlay-2 only: decoded capability flags from the `features` TXT, for the
    /// Diagnostics card. `None` if not AP2 or features weren't seen.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) ap2_features: Option<Ap2FeaturesInfo>,
    /// AirPlay-2 only: wire sample-rate mode — `"auto"` (negotiate 48 kHz, fall back
    /// to 44.1 kHz) or `"fixed_44100"`. `None` for non-AP2 outputs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) ap2_rate_mode: Option<&'static str>,
    /// AirPlay-2 only: the effective wire rate in Hz the output will use (48000 or
    /// 44100), reflecting the mode + learned capability. `None` for non-AP2.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) ap2_rate: Option<u32>,
    /// AirPlay-2 only: device-authoritative volume 0.0–1.0 — READ from the receiver
    /// (or last set by the user), or `None` when unknown (receiver didn't report and
    /// the user hasn't set it). Show unknown honestly (no level / 0), never a
    /// fabricated 100 %. We never impose a volume on connect.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) ap2_volume: Option<f32>,
    /// AirPlay-2 only: mute state (`true` = muted). `None` for non-AP2.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) ap2_muted: Option<bool>,
    /// sendspin only: the stored wire-codec choice — `"auto"` (Opus when usable,
    /// else PCM), or a pinned `"pcm"`/`"opus"`/`"flac"`. `None` for other kinds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) sendspin_codec: Option<&'static str>,
    /// sendspin only: the codec the stream will actually use — the choice narrowed by
    /// what the daemon can encode and what the device advertised. Differs from
    /// `sendspin_codec` whenever the choice isn't currently usable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) sendspin_codec_active: Option<&'static str>,
    /// sendspin only: every codec the picker offers, with whether it can be selected
    /// and — when it can't — why not. Drives the greyed-out entries in the UI.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) sendspin_codec_options: Option<Vec<CodecOption>>,
    /// sendspin only: the buffer this device asks us to keep queued (`min_buffer_ms`
    /// from its `client/state`), in ms. `None` until it has connected and reported one.
    /// **It can change with the wire codec** — a player may raise it for "codec init,
    /// decode warmup" — which is why a codec change makes the UI re-read this.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) sendspin_min_buffer_ms: Option<u32>,
    /// sendspin only: the startup lead it would like (`required_lead_time_ms`).
    /// Surfaced for diagnostics; the spec says to extend toward it only for buffered
    /// sources, and this is a live stream.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) sendspin_required_lead_ms: Option<u32>,
    /// sendspin only: the send-ahead its stream actually uses (ms) — the configured
    /// group lead raised to the largest member requirement.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) sendspin_send_ahead_ms: Option<u32>,
    /// pw-sink only: has this host's agent been paired? `Some(false)` = it is asking
    /// to be, which is what a *discovered* pw-sink output is; pairing it is the "Add"
    /// for this kind. `None` = not a pw-sink output.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) pwsink_paired: Option<bool>,
    /// pw-sink only: the code to check against the one the host's own agent logged,
    /// before pairing it. `Some` only while the host is waiting to be paired —
    /// approving a request you cannot identify is how you would hand your audio to a
    /// stranger on the network, so the card shows it next to the button.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) pwsink_pair_code: Option<String>,
    /// pw-sink only: is a remote `module-rtp-session` receiver actually connected
    /// and being streamed to right now (the AppleMIDI handshake completed)?
    /// `Some(false)` = discovered + routed but the receiver hasn't connected yet;
    /// `None` = not a pw-sink output. Distinct from `present` (mDNS visibility).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) pwsink_streaming: Option<bool>,
    /// pw-sink only: the host's master volume as *reported by its agent*, cubic
    /// 0.0-1.0. `None` when no agent is connected — deliberately not a
    /// fabricated 100 % (same rule as `ap2_volume`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) pwsink_volume: Option<f32>,
    /// pw-sink only: the host's mute state, as reported.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) pwsink_muted: Option<bool>,
    /// pw-sink only: which sink on the host plays our stream (display only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) pwsink_sink_name: Option<String>,
    /// pw-sink only: whether foreign streams on that host are currently ducked.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) pwsink_ducked: Option<bool>,
    /// Why this output currently can't play — a human-readable sentence, or `None`
    /// when nothing is known to be wrong. AirPlay-2 only so far
    /// ([`crate::ap2_health`], written by the liveness probe and by a failed
    /// connect). Exists because "routed, present, and silent" used to be
    /// indistinguishable from "playing" in the UI: the reason lived only in the
    /// daemon log. Kind-agnostic on purpose — sendspin/pw-sink can fill it later.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) last_error: Option<String>,
}

/// One entry in a sendspin output's codec picker.
#[derive(Serialize)]
pub(crate) struct CodecOption {
    /// `"auto"`, `"pcm"`, `"opus"`, `"flac"`.
    pub(crate) codec: &'static str,
    /// Selectable? False ⇒ the UI greys it out (and rejects it if posted anyway).
    pub(crate) available: bool,
    /// Why it isn't selectable — shown as the option's tooltip.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) reason: Option<String>,
}

/// everything but PCM) or **the device didn't advertise it** at our wire format.
/// `Auto` is always selectable; it just resolves to the best usable option.
pub(crate) fn sendspin_codec_info(
    node_name: &str,
    device_codecs: &[String],
    settings: &crate::routing::sync_settings::SyncSettings,
) -> (&'static str, &'static str, Vec<CodecOption>) {
    let mode = settings.sendspin_codec(node_name);
    let active = crate::outputs::sendspin::server::resolve_codec(mode, std::iter::once(&device_codecs.to_vec()));
    let mut options = vec![CodecOption { codec: "auto", available: true, reason: None }];
    for codec in crate::outputs::sendspin::server::OFFERED_CODECS {
        let encodable = crate::outputs::sendspin::server::can_encode(codec);
        let supported = crate::outputs::sendspin::server::device_supports(device_codecs, codec);
        let reason = match (encodable, supported) {
            (true, true) => None,
            (false, _) => Some(format!("the add-on can't encode {codec} yet")),
            (true, false) if device_codecs.is_empty() => {
                Some("not known yet — the device hasn't connected, so it hasn't told us what it decodes".to_string())
            }
            (true, false) => Some(format!("this device doesn't advertise {codec} at 48 kHz/16-bit/stereo")),
        };
        options.push(CodecOption { codec, available: reason.is_none(), reason });
    }
    (mode.as_str(), active, options)
}

/// Every output node name we know of *besides* what mDNS currently sees: the
/// adopted set (so an adopted device that's off the network stays listed as
/// offline) plus anything still referenced by saved routing intent or by a
/// music/announcement group. The latter two matter because adoption starts
/// empty on an upgrade — a device the user had routed must still be findable on
/// the Outputs page (under "Discovered", offline if need be) so its saved
/// routing can be picked back up or cleaned away, rather than becoming
/// invisible state.
pub(crate) fn remembered_output_names(state: &AppState) -> std::collections::BTreeSet<String> {
    let mut names = state.outputs.lock_recover().adopted().clone();
    names.extend(state.routing.lock_recover().referenced_outputs());
    let groups = state.groups_config.lock_recover();
    for g in groups.music() {
        names.extend(g.members.iter().cloned());
    }
    for g in groups.announcement() {
        names.extend(g.targets.iter().cloned());
    }
    names
}

/// Build the full output listing — every kind, every known device, each tagged
/// with its adoption `state`. The two endpoints below are filtered views of this:
/// `/api/outputs` keeps the adopted ones, `/api/outputs/discovered` the rest.
pub(crate) async fn collect_outputs(state: &AppState) -> Vec<OutputInfo> {
    use std::collections::BTreeSet;

    let mut outputs: Vec<OutputInfo> = Vec::new();
    let remembered = remembered_output_names(state);
    let adoption = |n: &str| state.outputs.lock_recover().state(n).as_str();

    // Discovered sendspin devices (present) + any offline ones we remember — so
    // users see every output the system knows about.
    let devices = state.sendspin_devices.lock_recover().clone();
    let mut sendspin_names: BTreeSet<String> = devices.keys().cloned().collect();
    sendspin_names.extend(remembered.iter().filter(|n| n.starts_with(SENDSPIN_DEV_PREFIX)).cloned());
    for node_name in sendspin_names {
        let dev = devices.get(&node_name);
        let present = dev.is_some();
        let name = dev
            .map(|d| d.display_name.clone())
            .unwrap_or_else(|| node_name.strip_prefix(SENDSPIN_DEV_PREFIX).unwrap_or(&node_name).replace(['_', '-'], " "));
        // IP/Port come from the mDNS-resolved server address (`None` until an
        // IPv4 address resolves). Sendspin has no transport encryption, so the
        // column is a constant "None" rather than an absent value.
        let addr = dev.and_then(|d| d.addr);
        let device_codecs = dev.map(|d| d.supported_codecs.clone()).unwrap_or_default();
        let (codec_mode, codec_active, codec_options) =
            sendspin_codec_info(&node_name, &device_codecs, &state.sync_settings.lock_recover());
        // What this device asked for, and the send-ahead its stream ends up with — the
        // same computation sync_group feeds the server, so the UI can't disagree with
        // the audio path.
        let (min_buffer_ms, required_lead_ms) = dev.map(|d| (d.min_buffer_ms, d.required_lead_time_ms)).unwrap_or((None, None));
        let send_ahead_ms = {
            let ss = state.sync_settings.lock_recover();
            let static_delay = ss.sendspin_delays().get(&node_name).copied().unwrap_or(0);
            let us = crate::outputs::sendspin::server::required_send_ahead_us(
                ss.group_lead_us(),
                codec_active,
                ss.opus_floor_ms(),
                std::iter::once((min_buffer_ms, static_delay)),
            );
            (us / 1000) as u32
        };
        outputs.push(OutputInfo {
            kind: "sendspin",
            present,
            configured: false, // sendspin devices are always auto-discovered
            state: adoption(&node_name),
            name,
            // Set by the rename pass at the end of collect_outputs.
            renamed: false,
            ip: addr.map(|a| a.ip().to_string()),
            port: addr.map(|a| a.port()),
            encryption: Some("None".to_string()),
            latency_ms: None,
            // Sendspin's delay knob is the static delay, reported separately.
            latency_effective_ms: None,
            ptp_locked: None, // sendspin has no PTP
            ptp_lock_age_s: None,
            ptp_supported: None,
            ptp_relevant: None,
            ap2_features: None,
            ap2_rate_mode: None,
            ap2_rate: None,
            ap2_volume: None,
            ap2_muted: None,
            sendspin_codec: Some(codec_mode),
            sendspin_codec_active: Some(codec_active),
            sendspin_codec_options: Some(codec_options),
            sendspin_min_buffer_ms: min_buffer_ms,
            sendspin_required_lead_ms: required_lead_ms,
            sendspin_send_ahead_ms: Some(send_ahead_ms),
            pwsink_paired: None,
            pwsink_pair_code: None,
            pwsink_streaming: None,
            pwsink_volume: None,
            pwsink_muted: None,
            pwsink_sink_name: None,
            pwsink_ducked: None,
            // No health source for sendspin yet (its own liveness task would be the
            // place); reported honestly as "nothing known" rather than "fine".
            last_error: None,
            node_name,
        });
    }

    // Discovered AirPlay-2 receivers (present) + remembered offline ones. These
    // are the RAOP-output replacement; like sendspin devices they're virtual (no
    // PipeWire node) and always auto-discovered.
    let ap2_devices = state.ap2_devices.lock_recover().clone();
    // Per-output AP2 render-delay overrides (routing/sync_settings.rs), keyed by node name
    // — the per-output latency field (`latency_ms`).
    let ap2_latencies = state.sync_settings.lock_recover().ap2_latencies();
    // Routing intent snapshot + the set of present AP2 receivers, so we can tell
    // whether a live PTP lock is *relevant* for each output: it only matters when
    // ≥2 present AP2 receivers share a source-set (a multi-room group that would
    // audibly drift without a shared clock). A lone AP2 output renders realtime
    // fine unlocked.
    let ap2_intent = crate::store::routing::snapshot(&state.routing);
    let ap2_present_nodes: Vec<String> = ap2_devices.keys().cloned().collect();
    // Device-authoritative volume/mute snapshot (read from the receiver on connect,
    // or set by the user); volume is absent when unknown → reported as `None`.
    let (ap2_vols, ap2_mutes) = {
        let c = state.ap2_control.lock().await;
        (c.volumes(), c.mutes())
    };
    let mut ap2_names: BTreeSet<String> = ap2_devices.keys().cloned().collect();
    ap2_names.extend(remembered.iter().filter(|n| n.starts_with(AP2_DEV_PREFIX)).cloned());
    for node_name in ap2_names {
        let dev = ap2_devices.get(&node_name);
        let present = dev.is_some();
        let name = dev
            .map(|d| d.display_name.clone())
            .unwrap_or_else(|| node_name.strip_prefix(AP2_DEV_PREFIX).unwrap_or(&node_name).replace(['_', '-'], " "));
        let addr = dev.and_then(|d| d.addr);
        // PTP-lock health: has libairptp heard gPTP from this receiver recently? A
        // locked receiver sends Delay_Req at a ~130ms cadence, so 5s is a generous
        // "still locked" window. If a present, routed receiver isn't locked, its stream
        // renders silence — surface that as degraded in the UI.
        let ptp_age = addr.and_then(|a| state.ap2_ptp.peer_lock_age(&a.ip().to_string()));
        let ptp_locked = if present { Some(ptp_age.is_some_and(|age| age <= std::time::Duration::from_secs(5))) } else { None };
        // Decoded capabilities (features TXT bit 41 = PTP, 40 = buffered, 48 = transient).
        let features = dev.and_then(|d| d.features).map(Features::from_raw);
        let ptp_supported = features.map(|f| f.supports_ptp());
        let ap2_features = features.map(|f| Ap2FeaturesInfo {
            raw: f.to_txt_value(),
            ptp: f.supports_ptp(),
            buffered_audio: f.supports_buffered_audio(),
            transient_pairing: f.supports_transient_pairing(),
        });
        // Relevant = present AND in a ≥2-member AP2 group (shares a non-empty
        // source-set with another present AP2 receiver). Only then does an unlocked
        // receiver risk audible multi-room drift.
        let ptp_relevant = if present {
            let my = crate::routing::source_set_of(&ap2_intent, &node_name);
            Some(!my.is_empty() && ap2_present_nodes.iter().any(|o| o != &node_name && crate::routing::source_set_of(&ap2_intent, o) == my))
        } else {
            None
        };
        // Rate mode (user choice) + the effective wire rate it resolves to.
        let (rate_mode, rate) = {
            let ss = state.sync_settings.lock_recover();
            let mode = match ss.ap2_rate_mode(&node_name) {
                crate::routing::sync_settings::Ap2RateMode::Auto => "auto",
                crate::routing::sync_settings::Ap2RateMode::Fixed44100 => "fixed_44100",
            };
            (mode, ss.ap2_effective_rate(&node_name))
        };
        outputs.push(OutputInfo {
            kind: "airplay2",
            present,
            configured: false, // AP2 receivers are always auto-discovered
            state: adoption(&node_name),
            name,
            // Set by the rename pass at the end of collect_outputs.
            renamed: false,
            ip: addr.map(|a| a.ip().to_string()),
            port: addr.map(|a| a.port()),
            // AirPlay 2 always uses HomeKit transient pairing + encryption.
            encryption: Some("HomeKit".to_string()),
            latency_ms: ap2_latencies.get(&node_name).copied(),
            latency_effective_ms: Some(
                ap2_latencies.get(&node_name).copied().unwrap_or(crate::outputs::ap2::server::AP2_RENDER_DELAY_MS as u16),
            ),
            ptp_locked,
            ptp_lock_age_s: ptp_age.map(|a| a.as_secs()),
            ptp_supported,
            ptp_relevant,
            ap2_features,
            ap2_rate_mode: Some(rate_mode),
            ap2_rate: Some(rate),
            ap2_volume: ap2_vols.get(&node_name).copied(),
            ap2_muted: Some(ap2_mutes.get(&node_name).copied().unwrap_or(false)),
            sendspin_codec: None,
            sendspin_codec_active: None,
            sendspin_codec_options: None,
            sendspin_min_buffer_ms: None,
            sendspin_required_lead_ms: None,
            sendspin_send_ahead_ms: None,
            pwsink_paired: None,
            pwsink_pair_code: None,
            pwsink_streaming: None,
            pwsink_volume: None,
            pwsink_muted: None,
            pwsink_sink_name: None,
            pwsink_ducked: None,
            last_error: crate::outputs::ap2::health::Ap2Health::global().get(&node_name),
            node_name,
        });
    }

    // pw-sink hosts = **receiver agents** (docs/receiver-agent-plan.md §3). A host is
    // here because a helper on it dialled in, not because something answered an mDNS
    // browse — which is why `present` means "the agent is connected" and
    // `pwsink_streaming` still means "a receiver completed the AppleMIDI handshake"
    // (outputs/pwsink/sender_liveness.rs).
    //
    // A host that has only *asked* to pair is listed too, as a `discovered` output
    // carrying its pairing code: pairing is this kind's "Add" (`adopt_output`), so
    // one section and one button serve every output kind. Remembered names are listed
    // as well, so an unpaired host with routing entries doesn't silently vanish.
    let (agent_hosts, agent_rows) = {
        let agents = state.agents.lock().await;
        (agents.hosts(), agents.snapshot())
    };
    let mut pwsink_names: BTreeSet<String> = agent_hosts.iter().map(|h| h.node_name.clone()).collect();
    pwsink_names.extend(remembered.iter().filter(|n| n.starts_with(PWSINK_DEV_PREFIX)).cloned());
    // Per-output playout delay overrides (the remote receiver's jitter buffer) —
    // the pw-sink entry in the same `latency_ms` field AP2 uses for its render
    // delay, so one slider and one endpoint serve both kinds.
    let pwsink_jitters = state.sync_settings.lock_recover().pwsink_jitters();
    for node_name in pwsink_names {
        let host = agent_hosts.iter().find(|h| h.node_name == node_name);
        let connected = host.map(|h| h.connected).unwrap_or(false);
        let paired = host.map(|h| h.paired).unwrap_or(false);
        let name = host
            .map(|h| h.label.clone())
            .unwrap_or_else(|| node_name.strip_prefix(PWSINK_DEV_PREFIX).unwrap_or(&node_name).replace(['_', '-'], " "));
        let host_state = agent_rows.iter().find(|row| row.node_name == node_name).and_then(|row| row.state.clone());
        let streaming = crate::outputs::pwsink::sender_liveness::PwSinkLiveness::global().get(&node_name).map(|s| s.established);
        outputs.push(OutputInfo {
            kind: "pwsink",
            present: connected,
            configured: false, // a pairing, not a hand-written config
            state: adoption(&node_name),
            name,
            // Set by the rename pass at the end of collect_outputs.
            renamed: false,
            ip: None, // the agent dials us; we never need its address
            port: None,
            encryption: Some("None".to_string()), // L16 RTP is unencrypted
            latency_ms: pwsink_jitters.get(&node_name).copied(),
            latency_effective_ms: Some(
                pwsink_jitters.get(&node_name).copied().unwrap_or(crate::routing::sync_settings::DEFAULT_PWSINK_JITTER_MS),
            ),
            ptp_locked: None,
            ptp_lock_age_s: None,
            ptp_supported: None,
            ptp_relevant: None,
            ap2_features: None,
            ap2_rate_mode: None,
            ap2_rate: None,
            ap2_volume: None,
            ap2_muted: None,
            sendspin_codec: None,
            sendspin_codec_active: None,
            sendspin_codec_options: None,
            sendspin_min_buffer_ms: None,
            sendspin_required_lead_ms: None,
            sendspin_send_ahead_ms: None,
            pwsink_paired: Some(paired),
            pwsink_pair_code: host.and_then(|h| h.pair_code.clone()),
            pwsink_streaming: if connected { Some(streaming.unwrap_or(false)) } else { None },
            // Host-reported, never fabricated: the user's own desktop owns these
            // values and the agent pushes changes back (plan §9.4).
            pwsink_volume: host_state.as_ref().and_then(|s| s.volume),
            pwsink_muted: host_state.as_ref().and_then(|s| s.muted),
            pwsink_sink_name: host_state.as_ref().and_then(|s| s.sink_name.clone()),
            pwsink_ducked: host_state.as_ref().map(|s| s.ducked),
            last_error: None,
            node_name,
        });
    }

    // The user's own name for an output wins over whatever discovery reported —
    // applied once here rather than per kind above, since every kind derives its
    // name differently but overrides the same way.
    let renamed = crate::store::outputs::names_snapshot(&state.outputs);
    for o in &mut outputs {
        if let Some(name) = renamed.get(&o.node_name) {
            o.name = name.clone();
            o.renamed = true;
        }
    }

    outputs
}

/// An output's user-facing name for a message: the user's own name if they gave
/// one, else the name derived from the node name. Discovery's mDNS name isn't
/// consulted — this is for toasts, where the store lookup is cheap and reaching
/// into three discovery maps isn't.
pub(crate) fn output_label(state: &AppState, node_name: &str) -> String {
    state.outputs.lock_recover().name(node_name).map(str::to_string).unwrap_or_else(|| crate::routing::output_display_name(node_name))
}

/// Both listings from one pass: `(adopted, offered)`.
///
/// Split out of the two handlers below so the routing WebSocket can push these
/// listings when they change (routing/mod.rs) instead of the UI polling for them. One
/// `collect_outputs` call serves both halves, which is what the handlers did
/// individually anyway.
pub(crate) async fn outputs_listings(state: &AppState) -> (Vec<OutputInfo>, Vec<OutputInfo>) {
    let outputs = collect_outputs(state).await;
    outputs.into_iter().partition(|o| o.state == OutputState::Adopted.as_str())
}
