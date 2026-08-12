//! Routing-driven **sync grouping** reconciler — sendspin multi-room grouping
//! and per-device AirPlay-2 senders in one model, so speakers routed from the
//! same sources play the same audio off one clock.
//!
//! ## The model
//!
//! Grouping is derived from routing intent, not declared: **every output routed
//! from the same set of sources belongs to one group**. Each group is backed by
//! one real `support.null-audio-sink` — the *sync anchor* (`SYNC_GRP_PREFIX`) —
//! which is the group's shared clock/timeline:
//!
//! - the group's sources are linked **into** the anchor;
//! - a filtered sendspin server (sendspin_server) captures **from** the anchor
//!   and dials exactly the group's sendspin devices, pushing one timestamped
//!   stream so they sync (see sendspin's `Group`);
//! - the group's AP2 receivers are driven by in-process senders (outputs/ap2/server.rs)
//!   that capture from the same anchor and stream realtime ALAC with libairptp
//!   PTP timing, so they share the same timeline.
//!
//! Because the anchor is one stable node per source-set, devices can come and
//! go — and the sendspin server / AP2 senders can be restarted when their
//! dialed set changes — without disturbing the anchor or the other members fed
//! from it.
//!
//! ## Reconcile
//!
//! Stateful (owns the running anchors/servers/AP2 senders) and serialized in the
//! single reconciler task (main.rs). On each change it diffs desired groups (from
//! intent + live devices) against running ones: tears down groups that are gone
//! (dropping the server + AP2 senders, destroying the anchor — its links go with
//! it), creates new anchors, and restarts a group's sendspin server / AP2 senders
//! when their dialed set (or the AP2 wire rate) changes.

use crate::outputs::overlay_mixer::OverlayMixer;
use crate::outputs::sendspin;
use crate::outputs::sendspin::discovery::{SendspinDevice, SharedSendspinDevices};
use crate::outputs::sendspin::server::SendspinServerHandle;
use crate::pw::thread::{PwCommand, PwCommandSender, SharedState};
use crate::routing::{self, node_id_for};
use crate::store;
use crate::store::routing::{RoutingLink, SharedRouting};
use crate::util::locks::LockRecover;
use crate::util::node_names::{OutputKind, AP2_DEV_PREFIX, SYNC_GRP_PREFIX};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::time::{Duration, Instant};
use tokio::sync::oneshot;

/// "AirPlay 2 receiver" / "PipeWire host" / … for a message, or a neutral word when the
/// name is not a virtual output at all. Only for the arms that should be unreachable: they
/// have to say *something*, and it must not be the wrong kind's story.
fn kind_or_output(kind: Option<OutputKind>) -> &'static str {
    kind.map_or("output", OutputKind::human)
}

#[derive(Default)]
pub struct GroupReconciler {
    /// Keyed by the group's source-set (sorted sources joined by `KEY_SEP`).
    running: HashMap<String, RunningGroup>,
    /// Standalone senders for ungrouped devices (per-device mode only), keyed by
    /// device node name.
    idle_senders: HashMap<String, IdleSender>,
    /// On-demand announce sessions for unrouted AP2 receivers / pw-sink targets,
    /// keyed by output node name (`ap2-dev-*` / `pwsink-dev-*`).
    announce_sessions: HashMap<String, AnnounceSession>,
    /// An **alignment hold** (align/group.rs): outputs taken exclusively for a
    /// calibration session. While set, [`Self::effective_intent`] rewrites routing
    /// intent so these outputs belong to one synthetic group and nothing else, which
    /// is how a temporary group is formed around an arbitrary selection.
    ///
    /// In memory only, and deliberately: the user's routing store is never touched,
    /// so teardown is "stop overriding" rather than a replay that could half-fail —
    /// and a daemon restart mid-session reads the unmodified store.
    align_hold: Option<(u64, BTreeSet<String>)>,
    /// Something this pass wanted to do didn't take (a sender failed to start, an
    /// anchor didn't appear, a device had no resolved URL yet) and only a *retry*
    /// will fix it. The reconcile task is change-driven with no periodic tick, so
    /// without this a transient failure left a group silent until an unrelated
    /// event happened along — see [`Self::retry_wanted`].
    retry_wanted: bool,
}

impl GroupReconciler {
    /// Make the next reconcile recycle **just this device's** connection, so it
    /// re-applies its static delay on connect — the only way current ESPHome firmware
    /// picks up a delay change (it reads `SetStaticDelay` at stream start, not live).
    /// Returns true if a running group has this device; the caller must nudge a
    /// reconcile (ChangeNotifier) afterwards.
    ///
    /// **Scoped to the one device on purpose.** This used to force a whole-*server*
    /// restart, which dropped every member of the group for a change that belongs to
    /// one member's sender. Daemon-side that was only 219 ms, but each reconnected
    /// speaker then goes silent for tens of seconds (a firmware-side cost, §4.9), so
    /// calibrating one speaker blacked out the room (§4.10). Nothing about the other
    /// members' streams changes: same timeline, same codec, same send-ahead, same
    /// timestamps.
    ///
    /// The exception is deliberately **not** handled here: if the new delay raises the
    /// group's send-ahead requirement above what the running server was started with,
    /// that is a stream-config change and the ordinary restart path re-arms everyone
    /// (see [`sendspin_config_changed`]) — correct, because the send-ahead is a
    /// high-water mark over all members (§4.6) and the timeline fixes it at
    /// construction. This flag then costs nothing extra: the reconcile clears it when
    /// the whole server restarts.
    ///
    /// Records intent instead of tearing down on the spot: the teardown belongs on the
    /// reconcile path, which can `await` the graceful per-device `stream/end`
    /// (`SendspinServerHandle::stop_device`).
    pub fn force_device_reconnect(&mut self, sendspin_node_name: &str) -> bool {
        for g in self.running.values_mut() {
            if g.server_devices.iter().any(|d| d == sendspin_node_name) {
                g.force_device_reconnect.insert(sendspin_node_name.to_string());
                return true;
            }
        }
        false
    }

    /// Gracefully tear down every sendspin server — group senders and idle senders
    /// alike — so each device gets a `stream/end` before the process goes away.
    ///
    /// Without this the add-on's SIGTERM just kills the sockets under devices that
    /// have an active stream, and the 2026-07-28 hardware test says that is expensive:
    /// after an abrupt teardown a Voice PE stayed silent for **tens of seconds** once
    /// the new daemon reconnected it, while AP2 receivers on the same anchor were back
    /// in ~5 s. The old comment here — "sendspin group servers tear down with the
    /// process" — was true and was the problem.
    ///
    /// Bounded: each `shutdown()` carries its own `GRACEFUL_END` timeout, and they run
    /// concurrently, so the total wait is the slowest single one rather than their sum.
    pub async fn shutdown_sendspin(&mut self) {
        let mut handles: Vec<SendspinServerHandle> = Vec::new();
        for g in self.running.values_mut() {
            if let Some(h) = g.server.take() {
                handles.push(h);
                g.server_devices.clear();
                g.server_members.clear();
                g.server_send_ahead_us = 0;
                // Every member is about to be told `stream/end` anyway, so a pending
                // per-device reconnect has nothing left to ask for.
                g.force_device_reconnect.clear();
            }
        }
        // Idle senders are connected too (that is their whole point), and an idle
        // sender mid-announcement has an active stream like any other.
        for (_dev, s) in std::mem::take(&mut self.idle_senders) {
            handles.push(s.server);
        }
        if handles.is_empty() {
            return;
        }
        tracing::info!("graceful shutdown: stream/end + close for {} sendspin server(s)", handles.len());
        let tasks: Vec<_> = handles.into_iter().map(|h| tokio::spawn(async move { h.shutdown().await })).collect();
        for t in tasks {
            let _ = t.await;
        }
    }

    /// Gracefully tear down every running group's pw-sink senders so each remote
    /// `module-rtp-session` receiver gets a clean AppleMIDI `BY` (+ advert
    /// withdraw) and drops its session immediately, rather than holding a stale
    /// session until a timeout after the daemon restarts. Called from the process
    /// shutdown path (main.rs): the async reconcile task's own `Drop` is not
    /// guaranteed to run on exit, so the teardown is triggered explicitly here.
    /// Best-effort and synchronous — `PwSinkServerHandle`/`AppleMidiSender` `Drop`
    /// sends the `BY` inline before returning.
    pub fn shutdown_pwsink(&mut self) {
        let groups = self.running.values().filter(|g| g.pwsink_server.is_some()).count();
        // On-demand announce sessions advertise the same way, so they need the same
        // clean BY (their sinks go with the process — nothing else references them).
        let announce: Vec<String> = self
            .announce_sessions
            .iter()
            .filter(|(_, s)| matches!(s.transport, AnnounceSessionTransport::PwSink { .. }))
            .map(|(o, _)| o.clone())
            .collect();
        if groups == 0 && announce.is_empty() {
            return;
        }
        // Log BEFORE dropping: the container's stop-grace can SIGKILL us partway
        // through the drop, so the tail of a post-drop log may never flush. The
        // BY itself still escapes — `AppleMidiSender::Drop` withdraws the advert
        // and sends BY *first*, before the worker-thread joins that a SIGKILL cuts.
        tracing::info!(
            "graceful shutdown: withdrawing advert + sending BY for {groups} pw-sink group session(s) and {} on-demand announce session(s)",
            announce.len()
        );
        for g in self.running.values_mut() {
            g.pwsink_server = None; // drop → advert withdraw + BY to peers
            g.pwsink_members.clear();
            g.pwsink_ports.clear();
        }
        for output in announce {
            self.announce_sessions.remove(&output); // drop → advert withdraw + BY
        }
    }

    /// Is a per-device sender currently streaming `output` (so an overlay dropped on
    /// it is consumed right away)? Covers every backend: a group's sendspin server /
    /// AP2 senders / pw-sink senders, plus a sendspin device's idle sender.
    ///
    /// For the two **dialed** backends, group membership is not the answer — it lists
    /// what the group *dialed*, including a receiver still connecting or one whose
    /// session failed, neither of which consumes an overlay. That judgement is
    /// [`dialed_session_established`] (shared with the routing matrix); the sendspin
    /// cases below are the ones only this reconciler can see.
    fn has_live_sender(&self, output: &str, ap2_connected: &HashSet<String>) -> bool {
        if let Some(established) = dialed_session_established(output, ap2_connected) {
            return established;
        }
        if self.idle_senders.contains_key(output) {
            return true;
        }
        self.running.values().any(|g| g.server_devices.iter().any(|d| d == output))
    }

    /// Make sure *something* will carry an announcement to `output`, and say what.
    ///
    /// An output only hears an announcement while a per-device sender is reading
    /// its overlay slot (sendspin_server / ap2_server / pwsink_server all call
    /// `OverlayMixer::mix_into`). Sendspin devices always have one — grouped or via
    /// their idle sender — but the **dialed** backends only have a sender while they
    /// are routed, so an unrouted AP2 receiver / pw-sink target had no transport at
    /// all and announcements to it were silently dropped. For those this opens an
    /// on-demand session ([`AnnounceSession`]); otherwise it reports honestly so the
    /// caller can tell the user instead of claiming "playing".
    ///
    /// Call this **before** starting the clip: it publishes the session's wire rate
    /// to the mixer, which `OverlayMixer::start` needs to rate-match the clip.
    pub async fn ensure_announce_transport(&mut self, output: &str, deps: &AnnounceDeps<'_>) -> AnnounceTransport {
        let ap2_connected = deps.ap2_control.lock().await.connected();
        let live = self.has_live_sender(output, &ap2_connected);
        // An on-demand session already up: extend its lease and reuse it (whether or
        // not it has finished connecting).
        if let Some(s) = self.announce_sessions.get_mut(output) {
            s.expires_at = Instant::now() + s.linger;
            return if live { AnnounceTransport::Live } else { AnnounceTransport::Warm };
        }
        if live {
            return AnnounceTransport::Live;
        }
        if !supports_on_demand_announce(output) {
            return AnnounceTransport::Unavailable(no_transport_reason(output));
        }

        // Only for an endpoint with NO wired input. A routed one belongs to the group
        // reconciler, which owns (and retries) its session; a second AP2 session would
        // collide (a receiver accepts one), and a second pw-sink advert would give the
        // receiver two sessions to attach to. Read through the adoption gate exactly
        // as `reconcile` does — an unadopted device's intent is dormant, so nothing
        // owns its session and the on-demand path is the only way to reach it (which
        // is precisely the "which speaker is this?" test-tone case).
        let adopted = crate::store::outputs::adopted_snapshot(deps.outputs);
        let intent: Vec<RoutingLink> = store::routing::snapshot(deps.routing).into_iter().filter(|l| adopted.contains(&l.output)).collect();
        // Read through the alignment hold too: an output taken for a calibration IS
        // routed (into the temporary group), so opening a *second* on-demand session
        // to it would collide with the group's own sender — an AP2 receiver accepts
        // one session at a time. Without this, a barge-in aimed at a held receiver
        // that happened to be unrouted before the session would do exactly that.
        let intent = self.effective_intent(intent);
        if !routing::source_set_of(&intent, output).is_empty() {
            return AnnounceTransport::Unavailable(match OutputKind::of(output) {
                Some(OutputKind::Airplay2) => {
                    "routed, but its AirPlay-2 sender isn't streaming (receiver unreachable, or still connecting)".into()
                }
                Some(OutputKind::PwSink) => {
                    "routed, but no receiver has connected to its session yet (its module-rtp-session must initiate the handshake)".into()
                }
                // Unreachable while `supports_on_demand_announce` gates the caller, and
                // spelled out anyway: the `else` this replaced would have told a sendspin
                // owner about a module-rtp-session handshake their speaker does not have.
                other => format!("routed, but nothing is carrying audio to this {}", kind_or_output(other)),
            });
        }

        match OutputKind::of(output) {
            Some(OutputKind::Airplay2) => self.open_ap2_announce_session(output, deps).await,
            Some(OutputKind::PwSink) => self.open_pwsink_announce_session(output, deps).await,
            // Same gate as above; a kind that reaches here has no on-demand path, and
            // saying so beats dialing it as if it were a pw-sink host.
            other => AnnounceTransport::Unavailable(format!(
                "no on-demand announcement session can be opened for this {}",
                kind_or_output(other)
            )),
        }
    }

    /// Open an on-demand AP2 session for an unrouted receiver (see
    /// [`AnnounceSession`]). The lease outlasts the receiver's render buffer so a
    /// TEARDOWN can't cut off audio it hasn't rendered yet.
    async fn open_ap2_announce_session(&mut self, output: &str, deps: &AnnounceDeps<'_>) -> AnnounceTransport {
        let device = deps.ap2_devices.lock_recover().get(output).cloned();
        let Some(device) = device else {
            return AnnounceTransport::Unavailable("unknown AirPlay-2 receiver".into());
        };
        if !device.present {
            return AnnounceTransport::Unavailable("receiver is offline".into());
        }
        let Some(addr) = device.addr else {
            return AnnounceTransport::Unavailable("receiver has no resolved address yet".into());
        };
        // Receivers are registered as PTP peers at discovery; make sure the
        // host-global grandmaster is up so PT=87 anchors carry its clock id.
        let clock_id = match deps.ap2_ptp.ensure_started() {
            Ok(id) => id,
            Err(e) => return AnnounceTransport::Unavailable(format!("AP2 PTP grandmaster unavailable: {e}")),
        };
        let (rate, delay) = {
            let ss = deps.sync_settings.lock_recover();
            (ss.ap2_group_rate([output]), ss.ap2_latency(output))
        };

        let (sink_node_name, sink_node_id) = match self.ensure_announce_sink(output, deps).await {
            Ok(v) => v,
            Err(e) => return AnnounceTransport::Unavailable(e),
        };

        // Publish the wire rate BEFORE the clip starts: `OverlayMixer::start`
        // resamples the 48 kHz clip to the output's rate, and ap2_server only
        // publishes it once the receiver has connected — too late for a clip queued
        // now, which would then play back at the wrong pitch on a 44.1 kHz receiver.
        OverlayMixer::global().set_output_rate(output, rate);

        let server = match crate::outputs::ap2::server::start(
            vec![(output.to_string(), addr.ip(), delay)],
            sink_node_id,
            clock_id,
            deps.ap2_control.clone(),
            rate,
            deps.sync_settings.clone(),
        ) {
            Ok(handle) => handle,
            Err(e) => {
                self.abandon_announce_sink(output, sink_node_id, deps.pw_cmd).await;
                return AnnounceTransport::Unavailable(format!("failed to start an on-demand AirPlay-2 session: {e}"));
            }
        };

        let render_delay_ms = u64::from(delay.unwrap_or(crate::outputs::ap2::server::AP2_RENDER_DELAY_MS as u16));
        let linger = ANNOUNCE_LINGER.max(Duration::from_millis(render_delay_ms) + ANNOUNCE_TAIL);
        tracing::info!(
            "on-demand AP2 announce session for '{output}' ({}) opening @ {rate} Hz (sink '{sink_node_name}', lease {}s)",
            addr.ip(),
            linger.as_secs()
        );
        self.announce_sessions.insert(
            output.to_string(),
            AnnounceSession {
                sink_node_name,
                sink_node_id,
                transport: AnnounceSessionTransport::Ap2(server),
                expires_at: Instant::now() + linger,
                linger,
            },
        );
        AnnounceTransport::Starting
    }

    /// Open an on-demand pw-sink session for an unrouted remote PipeWire host: bind a
    /// control/data port pair and advertise `pwrouter-<slug>` for the target's
    /// `module-rtp-session` to attach to, fed from a private silent sink.
    ///
    /// The handshake is **receiver-initiated**, so the clip waits until the target
    /// notices the advert and connects — the overlay isn't consumed before then, so it
    /// still plays whole, a second or two late. Same caveat as a routed pw-sink
    /// session: stock `module-rtp-session` in discover mode attaches to *every*
    /// advertised session, so with 2+ pw-sink targets on one LAN an announcement aimed
    /// at one can be heard by the others (the deferred session-scoping decision, see
    /// docs/pipewire-sink-output.md §4).
    async fn open_pwsink_announce_session(&mut self, output: &str, deps: &AnnounceDeps<'_>) -> AnnounceTransport {
        // The receiver dials *us*, so what has to be true is that its agent is on the
        // socket to be told about the session — not that anything answered an mDNS
        // browse.
        if !deps.agents.lock().await.is_connected(output) {
            return AnnounceTransport::Unavailable("no receiver agent is connected for this host".into());
        }
        let Some(control_port) = self.alloc_pwsink_ports(1).first().copied() else {
            return AnnounceTransport::Unavailable("no free control port for a pw-sink session".into());
        };

        let (sink_node_name, sink_node_id) = match self.ensure_announce_sink(output, deps).await {
            Ok(v) => v,
            Err(e) => return AnnounceTransport::Unavailable(e),
        };

        // The sender sizes its catch-up burst against the buffer the receiver was
        // told to keep, so it is told the same figure the agent got.
        let playout_ms = deps.sync_settings.lock_recover().pwsink_jitter_effective(output);
        let member = crate::outputs::pwsink::server::PwSinkMember { node_name: output.to_string(), control_port, playout_ms };
        let server = match crate::outputs::pwsink::server::start(vec![member], sink_node_id) {
            Ok(handle) => handle,
            Err(e) => {
                self.abandon_announce_sink(output, sink_node_id, deps.pw_cmd).await;
                return AnnounceTransport::Unavailable(format!("failed to start an on-demand pw-sink session: {e}"));
            }
        };

        tracing::info!(
            "on-demand pw-sink announce session for '{output}' advertising on control port {control_port} (sink '{sink_node_name}', lease {}s)",
            ANNOUNCE_LINGER.as_secs()
        );
        self.announce_sessions.insert(
            output.to_string(),
            AnnounceSession {
                sink_node_name,
                sink_node_id,
                transport: AnnounceSessionTransport::PwSink { _server: server, control_port },
                expires_at: Instant::now() + ANNOUNCE_LINGER,
                linger: ANNOUNCE_LINGER,
            },
        );
        AnnounceTransport::Starting
    }

    /// The private silent sink an on-demand session captures from: nothing is routed
    /// in, so its monitor is silence and the overlay mixer supplies the whole
    /// announcement. Reuses the node if one with this name already exists (a previous
    /// session's sink can outlive the daemon — they're created `object.linger`).
    /// Returns `(node_name, node_id)` or the reason to report.
    async fn ensure_announce_sink(&self, output: &str, deps: &AnnounceDeps<'_>) -> Result<(String, u32), String> {
        let sink_node_name = announce_sink_name(output);
        if let Some(id) = node_id_for(&deps.pw.lock_recover(), &sink_node_name) {
            return Ok((sink_node_name, id));
        }
        let (tx, rx) = oneshot::channel();
        if deps.pw_cmd.send(PwCommand::CreateSinkNode { node_name: sink_node_name.clone(), reply: tx }).is_err() {
            return Err("PipeWire thread unavailable".into());
        }
        match rx.await {
            Ok(Ok(())) => {}
            _ => return Err(format!("failed to create announce sink '{sink_node_name}'")),
        }
        match wait_for_node(deps.pw, &sink_node_name).await {
            Some(id) => Ok((sink_node_name, id)),
            None => Err(format!("announce sink '{sink_node_name}' did not appear in the graph")),
        }
    }

    /// Undo [`Self::ensure_announce_sink`] when the sender failed to start, so a
    /// retry doesn't inherit a stray sink (and the overlay rate doesn't stick).
    async fn abandon_announce_sink(&self, output: &str, sink_node_id: u32, pw_cmd: &PwCommandSender) {
        OverlayMixer::global().clear_output_rate(output);
        let (tx, rx) = oneshot::channel();
        if pw_cmd.send(PwCommand::DestroySinkNode { node_id: sink_node_id, reply: tx }).is_ok() {
            let _ = rx.await;
        }
    }

    /// Expire on-demand announce sessions whose lease has run out. Driven from a
    /// slow ticker in main.rs; a session with a clip still queued or playing keeps
    /// having its lease extended (the mixer's stall watchdog bounds that, so a
    /// receiver that never connects can't hold its session open forever).
    pub async fn poll_announce_sessions(&mut self, pw_cmd: &PwCommandSender) {
        if self.announce_sessions.is_empty() {
            return;
        }
        let mixer = OverlayMixer::global();
        // A clip that's playing on the output, or still queued for it, keeps the
        // lease rolling — the queued case has no overlay slot yet, so the mixer
        // alone would let the session go before the clip's turn came.
        let in_flight = crate::announce::AnnounceCoordinator::global().outputs_in_flight();
        let now = Instant::now();
        let mut expired: Vec<String> = Vec::new();
        for (output, s) in self.announce_sessions.iter_mut() {
            if mixer.is_active(output) || in_flight.contains(output) {
                s.expires_at = now + s.linger;
            } else if now >= s.expires_at {
                expired.push(output.clone());
            }
        }
        for output in expired {
            self.drop_announce_session(&output, pw_cmd, "lease expired").await;
        }
    }

    /// Tear down one on-demand announce session — AP2: TEARDOWN the receiver's RTSP
    /// session; pw-sink: `BY` + withdraw the advert (both on handle drop) — then
    /// destroy its private sink.
    async fn drop_announce_session(&mut self, output: &str, pw_cmd: &PwCommandSender, why: &str) {
        let Some(s) = self.announce_sessions.remove(output) else { return };
        tracing::info!("on-demand announce session for '{output}' torn down ({why}); removing sink '{}'", s.sink_node_name);
        drop(s.transport); // AP2: TEARDOWN + capture close; pw-sink: BY + advert withdraw
        OverlayMixer::global().clear_output_rate(output);
        let (tx, rx) = oneshot::channel();
        if pw_cmd.send(PwCommand::DestroySinkNode { node_id: s.sink_node_id, reply: tx }).is_ok() {
            let _ = rx.await;
        }
    }

    /// Graceful process-exit teardown for every AP2 session — group senders **and**
    /// on-demand announce sessions.
    ///
    /// A receiver accepts one AirPlay session at a time and holds a session we never
    /// closed until it times out: that is what makes the next start's first connect
    /// fail (the "cold/stale session" retry in `connect_one`) and what leaves the
    /// receiver's AirPlay input busy for phones in between. Dropping the handles only
    /// *signals* their tasks, and on process exit nothing polls those again — so this
    /// awaits them (each bounded inside `Ap2ServerHandle::shutdown`, all concurrently
    /// so an unreachable receiver doesn't serialize the rest).
    ///
    /// Called explicitly from main.rs's shutdown path, like `shutdown_pwsink`,
    /// because the reconcile task's own `Drop` isn't guaranteed to run on exit.
    pub async fn shutdown_ap2(&mut self) {
        let mut handles: Vec<crate::outputs::ap2::server::Ap2ServerHandle> = Vec::new();
        for g in self.running.values_mut() {
            if let Some(h) = g.ap2_sender.take() {
                handles.push(h);
                g.ap2_members.clear();
            }
        }
        // On-demand AP2 announce sessions too (pw-sink ones are handled by
        // `shutdown_pwsink`, whose BY is synchronous). Their private sinks go with the
        // process — nothing else references them — so only the RTSP session matters.
        let ap2_announce: Vec<String> = self
            .announce_sessions
            .iter()
            .filter(|(_, s)| matches!(s.transport, AnnounceSessionTransport::Ap2(_)))
            .map(|(o, _)| o.clone())
            .collect();
        for output in ap2_announce {
            tracing::debug!("graceful shutdown: tearing down on-demand announce session for '{output}'");
            if let Some(s) = self.announce_sessions.remove(&output) {
                if let AnnounceSessionTransport::Ap2(h) = s.transport {
                    handles.push(h);
                }
            }
        }
        if handles.is_empty() {
            return;
        }
        tracing::info!("graceful shutdown: TEARDOWN for {} AirPlay-2 session group(s)", handles.len());
        // Concurrently, via tasks (no `futures` dependency): each `shutdown()` carries
        // its own timeout, so the whole wait is bounded by the slowest single one
        // rather than their sum.
        let joins: Vec<tokio::task::JoinHandle<()>> = handles.into_iter().map(|h| tokio::spawn(h.shutdown())).collect();
        for j in joins {
            let _ = j.await;
        }
    }

    /// Take `outputs` exclusively for alignment hold `id` (align/group.rs). The
    /// caller must nudge a reconcile afterwards (ChangeNotifier).
    ///
    /// Replaces any previous hold: one alignment session exists at a time, and a
    /// stale hold would keep displacing routing nothing is aligning.
    pub fn set_align_hold(&mut self, id: u64, outputs: BTreeSet<String>) {
        if outputs.is_empty() {
            self.align_hold = None;
        } else {
            self.align_hold = Some((id, outputs));
        }
    }

    /// Drop hold `id`, restoring the displaced routing on the next reconcile.
    /// Id-guarded so a late teardown can't clear a newer session's hold, and
    /// idempotent so every teardown path may call it.
    pub fn clear_align_hold(&mut self, id: u64) {
        if self.align_hold.as_ref().is_some_and(|(held, _)| *held == id) {
            self.align_hold = None;
        }
    }

    /// The outputs currently held for alignment (empty when none).
    pub fn align_hold_outputs(&self) -> BTreeSet<String> {
        self.align_hold.as_ref().map(|(_, o)| o.clone()).unwrap_or_default()
    }

    /// Routing intent as this pass must see it: the stored intent, with any
    /// alignment hold applied.
    ///
    /// The hold does two things at once, which is what makes a temporary group both
    /// *exclusive* and *independent of current routing*:
    ///
    /// - every link into a held output is **dropped**, so no music source reaches it;
    /// - one synthetic link [`crate::align::group::ALIGN_HOLD_SOURCE`] → each held
    ///   output is **injected**, so all held outputs share a source set nothing else
    ///   has and the reconciler materialises exactly one group for them, with an
    ///   anchor the calibration player can write into. That source is not a real
    ///   node, so nothing is linked into the anchor and it stays silent otherwise.
    ///
    /// Identity when no hold is set, so the ordinary path is byte-for-byte unchanged.
    ///
    /// Note the hold is applied *after* the adoption filter and is not itself
    /// filtered by it: adoption is checked once, when the hold is formed
    /// (`align_group::validate_selection`), and from then on the session owns those
    /// outputs until it ends — removing an output on the Outputs page mid-session
    /// does not silently drop it out of a running measurement.
    fn effective_intent(&self, intent: Vec<RoutingLink>) -> Vec<RoutingLink> {
        let Some((_, held)) = &self.align_hold else { return intent };
        let mut out: Vec<RoutingLink> = intent.into_iter().filter(|l| !held.contains(&l.output)).collect();
        for output in held {
            out.push(RoutingLink { source: crate::align::group::ALIGN_HOLD_SOURCE.to_string(), output: output.clone() });
        }
        out
    }

    /// Snapshot every running group (anchor + members) for the alignment API.
    pub fn snapshot(&self) -> Vec<GroupSnapshot> {
        self.running
            .iter()
            .map(|(key, g)| GroupSnapshot {
                sources: key.split(KEY_SEP).map(str::to_string).collect(),
                anchor_node_id: g.anchor_node_id,
                sendspin_members: g.server_devices.clone(),
                ap2_members: g.ap2_members.clone(),
            })
            .collect()
    }
}

impl GroupReconciler {
    pub fn new() -> Self {
        Self::default()
    }

    /// Did the last [`Self::reconcile`] leave work undone that only a retry can
    /// finish? The reconcile task uses this to wake itself again after a short
    /// delay instead of waiting for the next unrelated change (main.rs).
    pub fn retry_wanted(&self) -> bool {
        self.retry_wanted
    }

    /// Lowest free port at/above the base not used by a running group or idle
    /// sender. `extra` lets a caller reserve ports it's about to assign in the
    /// same reconcile pass (before they land in `running`/`idle_senders`).
    fn alloc_port(&self, extra: &HashSet<u16>) -> u16 {
        let mut used: HashSet<u16> = self.running.values().map(|g| g.port).collect();
        used.extend(self.idle_senders.values().map(|s| s.port));
        used.extend(extra.iter().copied());
        let mut port = GROUP_BASE_PORT;
        while used.contains(&port) {
            port += 1;
        }
        port
    }

    /// Allocate `n` control ports for a group's pw-sink AppleMIDI sessions (each
    /// session also binds `control + 1` as its data port), avoiding every port a
    /// running group's pw-sink senders already hold. Ports step by 2 from
    /// PWSINK_BASE_PORT so the control/data pairs never overlap.
    fn alloc_pwsink_ports(&self, n: usize) -> Vec<u16> {
        // Both a group's senders and an on-demand announce session bind out of this
        // range, so both must be in the taken set.
        let group_ports = self.running.values().flat_map(|g| g.pwsink_ports.iter().copied());
        let announce_ports = self.announce_sessions.values().filter_map(|s| match &s.transport {
            AnnounceSessionTransport::PwSink { control_port, .. } => Some(*control_port),
            AnnounceSessionTransport::Ap2(_) => None,
        });
        next_free_pwsink_ports(group_ports.chain(announce_ports), n)
    }

    /// `send_ahead_us` is the group presentation lead from the sync settings
    /// (routing/sync_settings.rs), applied to every group's sendspin server.
    // Reconciliation reads from every subsystem it has to line up.
    #[allow(clippy::too_many_arguments)]
    pub async fn reconcile(
        &mut self,
        pw: &SharedState,
        pw_cmd: &PwCommandSender,
        routing: &SharedRouting,
        adopted: &crate::store::outputs::SharedOutputs,
        devices: &SharedSendspinDevices,
        control: &crate::outputs::sendspin::volume::SharedSendspinControl,
        send_ahead_us: i64,
        ap2_devices: &crate::outputs::ap2::discovery::SharedAp2Devices,
        ap2_ptp: &crate::outputs::ap2::ptp::SharedAp2Ptp,
        sync_settings: &crate::routing::sync_settings::SharedSyncSettings,
        ap2_control: &crate::outputs::ap2::volume::SharedAp2Control,
        pwsink_hosts: &PwsinkHosts,
    ) {
        // Re-earned every pass: whatever failed last time either succeeds now or
        // sets this again.
        self.retry_wanted = false;
        // Adoption gates the audio path, not just the UI: intent whose output the
        // user hasn't added on the Outputs page is dormant, so it forms no group
        // and no stream/session is ever opened to that device. Filtering the
        // intent (rather than the device maps) keeps the *on-demand* announce
        // path — the test tone that tells you which speaker this is — working for
        // a merely discovered device.
        let adopted_set = crate::store::outputs::adopted_snapshot(adopted);
        let intent: Vec<RoutingLink> = store::routing::snapshot(routing).into_iter().filter(|l| adopted_set.contains(&l.output)).collect();
        // An alignment session's temporary exclusive group (align/group.rs) is an
        // override on this intent, not an edit of the store — see `effective_intent`.
        let intent = self.effective_intent(intent);
        let devices_map = devices.lock_recover().clone();
        let ap2_map = ap2_devices.lock_recover().clone();
        let ap2_latencies = sync_settings.lock_recover().ap2_latencies();
        let mut desired = compute_desired(&intent, &devices_map, &ap2_map, &ap2_latencies, pwsink_hosts);

        // Resolve each group's AP2 capture/wire rate from the per-output rate mode
        // + learned capability cache (48000 iff every member's effective rate is
        // 48000, else 44100). Done here (not in compute_desired) so the rate logic
        // stays with the settings store.
        {
            let ss = sync_settings.lock_recover();
            for d in desired.values_mut() {
                if !d.ap2_members.is_empty() {
                    d.ap2_rate = ss.ap2_group_rate(d.ap2_members.iter().map(|(n, _, _)| n.as_str()));
                }
                // Sendspin wire codec: one stream serves the whole group, so the
                // per-output choices are narrowed to something EVERY member decodes
                // (and the daemon can encode). A conflict resolves to PCM, which
                // every player must handle.
                if !d.sendspin_node_names.is_empty() {
                    let member_codecs: Vec<Vec<String>> = d
                        .sendspin_node_names
                        .iter()
                        .map(|n| devices_map.get(n).map(|dev| dev.supported_codecs.clone()).unwrap_or_default())
                        .collect();
                    // The group's mode is the least-permissive member choice: an
                    // explicit pick anywhere wins over Auto, and PCM wins over the
                    // rest (a member pinned to PCM must not be sent Opus).
                    let mode = d
                        .sendspin_node_names
                        .iter()
                        .map(|n| ss.sendspin_codec(n))
                        .min_by_key(|m| match m {
                            crate::routing::sync_settings::SendspinCodec::Pcm => 0,
                            crate::routing::sync_settings::SendspinCodec::Flac => 1,
                            crate::routing::sync_settings::SendspinCodec::Opus => 2,
                            crate::routing::sync_settings::SendspinCodec::Auto => 3,
                        })
                        .unwrap_or_default();
                    d.sendspin_codec = sendspin::server::resolve_codec(mode, member_codecs.iter());
                    // Send-ahead floor, resolved AFTER the codec because a compressed
                    // stream needs more decode headroom than PCM: whichever is larger of
                    // what each member reported and its codec's own minimum, plus that
                    // member's static delay.
                    let delays = ss.sendspin_delays();
                    d.sendspin_send_ahead_us = sendspin::server::required_send_ahead_us(
                        send_ahead_us,
                        d.sendspin_codec,
                        ss.opus_floor_ms(),
                        d.sendspin_node_names
                            .iter()
                            .map(|n| (devices_map.get(n).and_then(|dev| dev.min_buffer_ms), delays.get(n).copied().unwrap_or(0))),
                    );
                }
            }
        }

        // 1. Tear down groups no longer desired (server first, then the anchor —
        //    destroying the anchor node takes its source/monitor links with it).
        let stale: Vec<String> = self.running.keys().filter(|k| !desired.contains_key(*k)).cloned().collect();
        for key in stale {
            if let Some(rg) = self.running.remove(&key) {
                tracing::info!(
                    "tearing down sync group {} ({} sendspin, {} ap2)",
                    rg.anchor_node_name,
                    rg.server_devices.len(),
                    rg.ap2_members.len()
                );
                if let Some(server) = rg.server {
                    // `stream/end` to each member before its socket goes, same as a
                    // config-change restart — an unrouted speaker should be told the
                    // stream is over, not left to notice a dead connection.
                    server.shutdown().await;
                }
                drop(rg.ap2_sender); // signals AP2 senders to TEARDOWN their receivers
                drop(rg.pwsink_server); // tears down each pw-sink target session (BY + advert)
                let (tx, rx) = oneshot::channel();
                if pw_cmd.send(PwCommand::DestroySinkNode { node_id: rg.anchor_node_id, reply: tx }).is_ok() {
                    let _ = rx.await;
                }
            }
        }

        // 1b. Idle-sender teardown. Every **adopted** device that isn't in a group
        //     keeps a standalone sender (so it's always reachable — e.g.
        //     announcements to an idle speaker). Drop the sender of any device that
        //     is now grouped or gone, BEFORE the group servers below dial, so a
        //     newly-grouped device isn't dialed by both its idle sender and its
        //     group at once.
        //
        //     Adoption gates this as strictly as it gates grouping: an idle sender
        //     is a live WebSocket streaming silence into the device, so without the
        //     gate the daemon would connect to — and continuously feed — every
        //     sendspin speaker on the LAN, including ones the user never added. A
        //     device removed on the Outputs page therefore also gets its idle
        //     sender dropped here, with the usual `stream/end` goodbye.
        let grouped: HashSet<String> = desired.values().flat_map(|d| d.sendspin_node_names.iter().cloned()).collect();
        let want_idle: HashSet<String> =
            devices_map.keys().filter(|d| !grouped.contains(*d) && adopted_set.contains(*d)).cloned().collect();
        let drop_idle: Vec<String> = self.idle_senders.keys().filter(|d| !want_idle.contains(*d)).cloned().collect();
        for dev in drop_idle {
            if let Some(s) = self.idle_senders.remove(&dev) {
                tracing::info!("idle sender '{}' torn down (device grouped or gone)", s.sink_node_name);
                // Awaited, not dropped: this runs immediately before the group
                // server below dials the same device, so the old connection must be
                // gone first — and if an announcement was playing through this
                // sender, the device is told the stream ended.
                s.server.shutdown().await;
                let (tx, rx) = oneshot::channel();
                if pw_cmd.send(PwCommand::DestroySinkNode { node_id: s.sink_node_id, reply: tx }).is_ok() {
                    let _ = rx.await;
                }
            }
        }

        // 1c. On-demand announce-session teardown. An AP2 receiver accepts ONE
        //     session and a pw-sink receiver would see two adverts, so an on-demand
        //     session must go BEFORE the group senders below dial/advertise for the
        //     same endpoint. Also drops sessions whose device/target went away (the
        //     handle would be streaming into the void).
        let grouped: HashSet<&str> = desired
            .values()
            .flat_map(|d| d.ap2_members.iter().map(|(n, _, _)| n.as_str()).chain(d.pwsink_members.iter().map(String::as_str)))
            .collect();
        let drop_announce: Vec<(String, &str)> = self
            .announce_sessions
            .keys()
            .filter_map(|o| {
                let still_there = if o.starts_with(AP2_DEV_PREFIX) {
                    ap2_map.get(o).is_some_and(|d| d.present && d.addr.is_some())
                } else {
                    pwsink_hosts.contains_key(o)
                };
                if grouped.contains(o.as_str()) {
                    Some((o.clone(), "endpoint is now routed — its group sender takes over"))
                } else if !still_there {
                    Some((o.clone(), "endpoint went offline"))
                } else {
                    None
                }
            })
            .collect();
        for (output, why) in drop_announce {
            self.drop_announce_session(&output, pw_cmd, why).await;
        }

        // 2. Create/steer each desired group.
        for (key, d) in &desired {
            // a. Ensure the anchor sink exists (create + wait, within this call,
            //    so the wiring below finds it and we don't re-create next tick).
            if !self.running.contains_key(key) {
                let anchor_node_name = format!("{SYNC_GRP_PREFIX}{}", group_hash(key));
                let (tx, rx) = oneshot::channel();
                if pw_cmd.send(PwCommand::CreateSinkNode { node_name: anchor_node_name.clone(), reply: tx }).is_err() {
                    continue;
                }
                match rx.await {
                    Ok(Ok(())) => {}
                    _ => {
                        tracing::warn!("failed to create sync anchor '{anchor_node_name}' — retrying shortly");
                        self.retry_wanted = true;
                        continue;
                    }
                }
                let Some(anchor_node_id) = wait_for_node(pw, &anchor_node_name).await else {
                    tracing::warn!("sync anchor '{anchor_node_name}' did not appear in the graph in time — retrying shortly");
                    self.retry_wanted = true;
                    continue;
                };
                let port = self.alloc_port(&HashSet::new());
                tracing::info!("created sync anchor '{anchor_node_name}' (id {anchor_node_id}) for source(s) {:?}", d.sources);
                self.running.insert(
                    key.clone(),
                    RunningGroup {
                        anchor_node_name,
                        anchor_node_id,
                        port,
                        server: None,
                        server_devices: Vec::new(),
                        server_members: Vec::new(),
                        server_codec: "pcm",
                        server_send_ahead_us: 0,
                        force_device_reconnect: BTreeSet::new(),
                        ap2_sender: None,
                        ap2_members: Vec::new(),
                        ap2_rate: 48_000,
                        pwsink_server: None,
                        pwsink_members: Vec::new(),
                        pwsink_ports: Vec::new(),
                    },
                );
            }

            // Snapshot what we need so no borrow of `self.running` is held across
            // an await (the async link/server calls below).
            let (anchor_name, anchor_id, port, prev_members, prev_codec, prev_lead, have_server, prev_ap2, prev_ap2_rate, prev_pwsink) = {
                let rg = self.running.get(key).expect("just inserted");
                (
                    rg.anchor_node_name.clone(),
                    rg.anchor_node_id,
                    rg.port,
                    rg.server_members.clone(),
                    rg.server_codec,
                    rg.server_send_ahead_us,
                    rg.server.is_some(),
                    rg.ap2_members.clone(),
                    rg.ap2_rate,
                    rg.pwsink_members.clone(),
                )
            };

            // b. Wire each source into the anchor (idempotent).
            for source in &d.sources {
                routing::ensure_link_by_name(pw, pw_cmd, source, &anchor_name).await;
            }

            // c. The group's sendspin server. Each device is its own single-member
            //    sender sharing one timeline off the anchor capture, so a device can
            //    be ducked/overlaid independently while staying in sync (see
            //    sendspin_server).
            //
            //    Its restart identity is the **stream config** — the codec and the
            //    send-ahead, i.e. what `stream/start` carries and what the shared
            //    timeline fixes at construction. Membership is deliberately NOT part
            //    of it: `ClientManager::supervise` adds a device to a running server
            //    and the membership task gives any newly-connected client its own
            //    `Group` on the live timeline, so a join needs nothing torn down.
            //    Restarting for a join instead cost every *existing* member a full
            //    reconnect — 813 ms of it measured end-to-end, and every device's
            //    stream re-anchored (docs/old/sendspin-group-churn-plan.md §2b, H1).
            //
            //    A restart, when the config really did change, is still only the
            //    server — never the anchor — so AP2/RAOP outputs fed from the same
            //    anchor don't blip.
            let action = sendspin_server_action(SendspinServerState {
                routed: !d.sendspin_node_names.is_empty(),
                have_server,
                config_changed: sendspin_config_changed(prev_codec, prev_lead, d.sendspin_codec, d.sendspin_send_ahead_us),
            });
            let restart = action == ServerAction::Start;
            if matches!(action, ServerAction::Start | ServerAction::Stop) {
                // Graceful and awaited: the members are told their stream ended
                // instead of having the socket pulled from under them, and the
                // listener is really gone before the new server binds the same port
                // below (see `SendspinServerHandle::shutdown`).
                if let Some(server) = self.running.get_mut(key).and_then(|rg| rg.server.take()) {
                    tracing::info!(
                        "sync group '{anchor_name}': stopping sendspin server ({}) — every member reconnects, and real firmware answers a reconnect with seconds of silence",
                        if action == ServerAction::Stop {
                            "no sendspin devices routed here any more".to_string()
                        } else if d.sendspin_codec != prev_codec {
                            format!("wire codec {prev_codec} -> {}", d.sendspin_codec)
                        } else {
                            // Since §4.10 this is the only way a static-delay edit
                            // reaches the whole group: the delay fed into the
                            // send-ahead high-water mark and pushed it up, which every
                            // member's timeline shares.
                            format!(
                                "a member needs a longer send-ahead than the running {} ms ({} ms)",
                                prev_lead / 1000,
                                d.sendspin_send_ahead_us / 1000
                            )
                        }
                    );
                    server.shutdown().await;
                }
                if let Some(rg) = self.running.get_mut(key) {
                    rg.server_devices = Vec::new();
                    rg.server_members = Vec::new();
                    // Every member is reconnecting anyway, so any pending per-device
                    // reconnect (§4.10) has already been granted — and re-granting it
                    // in step (c3) below would drop a device we just re-armed.
                    rg.force_device_reconnect.clear();
                    rg.server_send_ahead_us = 0; // the high-water mark dies with its server
                }
            }
            if restart {
                match sendspin::server::start_server_per_device(
                    &anchor_name,
                    &group_display(d),
                    port,
                    anchor_id,
                    d.sendspin_members.clone(),
                    d.sendspin_send_ahead_us,
                    control.clone(),
                    devices.clone(),
                    sendspin::server::StreamPolicy::Always,
                    d.sendspin_codec,
                )
                .await
                {
                    Ok(handle) => {
                        tracing::info!(
                            "sync group '{anchor_name}': per-device senders on port {port} dialing {} device(s), codec {}, send-ahead {} ms{}",
                            d.sendspin_members.len(),
                            d.sendspin_codec,
                            d.sendspin_send_ahead_us / 1000,
                            if d.sendspin_send_ahead_us > send_ahead_us {
                                // Say which rule raised it: a member's own request, our
                                // codec floor, and/or its static delay (which the device
                                // subtracts from every timestamp, so the server must send
                                // that much further ahead or its chunks land in the past).
                                format!(
                                    " (raised from the configured {} ms to cover a member's buffer requirement + its static delay)",
                                    send_ahead_us / 1000
                                )
                            } else {
                                String::new()
                            }
                        );
                        if let Some(rg) = self.running.get_mut(key) {
                            rg.server = Some(handle);
                            rg.server_codec = d.sendspin_codec;
                            rg.server_send_ahead_us = d.sendspin_send_ahead_us;
                        }
                    }
                    Err(e) => {
                        // Nothing else will nudge us: the reconcile task is
                        // change-driven, so without an explicit retry this group
                        // would have no sendspin output until an unrelated event
                        // happened along.
                        tracing::warn!("sync group '{anchor_name}': failed to start sendspin server: {e} — retrying shortly");
                        self.retry_wanted = true;
                    }
                }
            }

            // c2. Membership on the RUNNING server — the part that used to cost a
            //     restart. Three cases, all applied in place:
            //
            //     * a device joined → `supervise` dials it; the membership task puts
            //       it in its own group on the live timeline when it reports
            //       `client/state`, and its groupmates never notice;
            //     * a device left → `stop_device` ends *its* stream and stops *its*
            //       supervisor, gracefully;
            //     * a device re-resolved at a new address → `supervise` is idempotent
            //       per fullname, so an unchanged URL costs nothing and a changed one
            //       redials. (This is what the per-server mDNS browser used to do,
            //       before it turned out to be stealing every other server's
            //       subscription.)
            if !restart && prev_members != d.sendspin_members {
                let departed: Vec<String> = prev_members
                    .iter()
                    .map(|(fullname, _)| fullname.clone())
                    .filter(|fullname| !d.sendspin_members.iter().any(|(f, _)| f == fullname))
                    .collect();
                let arrived = d.sendspin_members.iter().filter(|(f, _)| !prev_members.iter().any(|(pf, _)| pf == f)).count();
                if let Some(server) = self.running.get(key).and_then(|rg| rg.server.as_ref()) {
                    for fullname in &departed {
                        server.stop_device(fullname).await;
                    }
                    server.supervise(&d.sendspin_members);
                    tracing::info!(
                        "sync group '{anchor_name}': sendspin membership now {} device(s) (+{arrived}/-{}) — no restart, the stream keeps running",
                        d.sendspin_members.len(),
                        departed.len()
                    );
                }
            }

            // Bookkeeping for the API/UI and the alignment wizard. Tracked whether or
            // not anything restarted, so a join that the running server absorbed is
            // still visible to `groups_snapshot`.
            if let Some(rg) = self.running.get_mut(key) {
                if rg.server.is_some() {
                    rg.server_devices = d.sendspin_node_names.clone();
                    rg.server_members = d.sendspin_members.clone();
                }
            }

            // c3. Per-device forced reconnect — the static-delay path
            //     (docs/old/sendspin-group-churn-plan.md §4.10). ESPHome firmware reads
            //     `SetStaticDelay` at stream start, so a live push doesn't shift a
            //     running stream and the device has to reconnect; nothing about its
            //     *groupmates'* streams changes, so only it does.
            //
            //     Done in two passes on purpose. This one ends the device's stream and
            //     drops it from the remembered member set; the *next* pass sees the
            //     member set differ from what's desired and re-supervises it through
            //     the ordinary membership path (c2), which redials.
            //
            //     Why not `stop_device` + `supervise` back-to-back here: `stop_client`
            //     only *signals* the old supervisor, which then emits its
            //     `Disconnected` event, while `supervise` immediately spawns a fresh
            //     one for the same fullname. Both feed one serial event loop, so a
            //     `Disconnected` that lands after the new `Connected` would remove the
            //     new connection's `pending`/`groups`/`client_to_node` entries and
            //     unregister its control sender — the device would sit connected and
            //     never be streamed to (exactly the silent-but-healthy-looking failure
            //     §4.8 had to add instrumentation to see). The dial takes ~130 ms, so
            //     that ordering is *likely* fine and not *guaranteed*; splitting the
            //     passes makes it structural instead. It costs up to `RECONCILE_RETRY`
            //     (3 s) before the redial, which is free against the tens of seconds
            //     the speaker's own resync costs (§4.9) — and only this speaker waits.
            //
            //     Read fresh from `running` rather than from the snapshot above: a
            //     restart earlier in this pass already reconnected everyone and cleared
            //     the set, and acting on a stale copy would drop a device we had just
            //     re-armed.
            let forced_reconnects: Vec<String> =
                self.running.get(key).map(|rg| rg.force_device_reconnect.iter().cloned().collect()).unwrap_or_default();
            if !forced_reconnects.is_empty() {
                // Resolve node names to the mDNS fullnames the server supervises, and
                // only for devices this group actually has a supervised member for (a
                // device whose URL never resolved has no connection to recycle — it
                // will pick the delay up from `SendspinControl::register` when it first
                // connects).
                let stopping: Vec<(String, String)> = forced_reconnects
                    .iter()
                    .filter_map(|node| devices_map.get(node).map(|dev| (node.clone(), dev.fullname.clone())))
                    .filter(|(_, fullname)| d.sendspin_members.iter().any(|(f, _)| f == fullname))
                    .collect();
                if let Some(server) = self.running.get(key).and_then(|rg| rg.server.as_ref()) {
                    for (node, fullname) in &stopping {
                        tracing::info!(
                            "sync group '{anchor_name}': reconnecting only '{node}' to apply its static delay — the other {} member(s) keep streaming",
                            d.sendspin_members.len().saturating_sub(1)
                        );
                        server.stop_device(fullname).await;
                    }
                }
                if let Some(rg) = self.running.get_mut(key) {
                    // Consumed either way: a device we couldn't stop has nothing to
                    // reconnect, and leaving the request set would retry forever.
                    rg.force_device_reconnect.clear();
                    if !stopping.is_empty() {
                        rg.server_members.retain(|(f, _)| !stopping.iter().any(|(_, sf)| sf == f));
                        // The next pass re-supervises it via (c2); nothing else would
                        // wake this change-driven task, so ask for that pass.
                        self.retry_wanted = true;
                    }
                }
            }

            // d. (Re)start AP2 senders when the receiver set changes. Like sendspin,
            //    each per-device Connection is fixed at start, so a change means
            //    drop-and-recreate — only the senders, never the shared anchor.
            // Identity is the receiver SET + the negotiated group rate — a
            // membership change or a rate change (UI mode switch / cached 48→44.1
            // downgrade) restarts the senders. Render delay is intentionally NOT in
            // the identity: it's retuned live (ap2_control → SetRenderDelay), so a
            // delay edit never reconnects (that churn could silence a flaky receiver).
            let ap2_identity: Vec<String> = d.ap2_members.iter().map(|(n, _, _)| n.clone()).collect();
            if ap2_identity != prev_ap2 || d.ap2_rate != prev_ap2_rate {
                if let Some(rg) = self.running.get_mut(key) {
                    rg.ap2_sender = None; // drop → TEARDOWN old receiver sessions
                    rg.ap2_members = Vec::new();
                }
                if !d.ap2_members.is_empty() {
                    // Receivers are already PTP peers of the host-global grandmaster
                    // (registered at discovery); ensure it's up and get its clock id.
                    match ap2_ptp.ensure_started() {
                        Ok(clock_id) => match crate::outputs::ap2::server::start(
                            d.ap2_members.clone(),
                            anchor_id,
                            clock_id,
                            ap2_control.clone(),
                            d.ap2_rate,
                            sync_settings.clone(),
                        ) {
                            Ok(handle) => {
                                tracing::info!(
                                    "sync group '{anchor_name}': AP2 senders streaming to {} receiver(s) @ {} Hz",
                                    d.ap2_members.len(),
                                    d.ap2_rate
                                );
                                if let Some(rg) = self.running.get_mut(key) {
                                    rg.ap2_sender = Some(handle);
                                    rg.ap2_members = ap2_identity;
                                    rg.ap2_rate = d.ap2_rate;
                                }
                            }
                            Err(e) => {
                                tracing::warn!("sync group '{anchor_name}': failed to start AP2 senders: {e} — retrying shortly");
                                self.retry_wanted = true;
                            }
                        },
                        Err(e) => {
                            tracing::warn!("sync group '{anchor_name}': AP2 PTP grandmaster unavailable: {e} — retrying shortly");
                            self.retry_wanted = true;
                        }
                    }
                }
            }

            // e. (Re)start pw-sink senders when the target set changes. Each
            //    target's AppleMIDI session (advert + bound ports) is fixed at
            //    start, so a membership change is a drop-and-recreate — only the
            //    pw-sink senders, never the shared anchor (so co-routed sendspin/AP2
            //    outputs fed from the same anchor never blip). Fresh control ports
            //    are allocated per restart; the receiver reconnects to the new
            //    advertised session.
            if d.pwsink_members != prev_pwsink {
                if let Some(rg) = self.running.get_mut(key) {
                    rg.pwsink_server = None; // drop → tear down old target sessions
                    rg.pwsink_members = Vec::new();
                    rg.pwsink_ports = Vec::new();
                }
                if !d.pwsink_members.is_empty() {
                    let ports = self.alloc_pwsink_ports(d.pwsink_members.len());
                    let members: Vec<crate::outputs::pwsink::server::PwSinkMember> = d
                        .pwsink_members
                        .iter()
                        .zip(ports.iter())
                        .map(|(node_name, port)| crate::outputs::pwsink::server::PwSinkMember {
                            playout_ms: sync_settings.lock_recover().pwsink_jitter_effective(node_name),
                            node_name: node_name.clone(),
                            control_port: *port,
                        })
                        .collect();
                    match crate::outputs::pwsink::server::start(members, anchor_id) {
                        Ok(handle) => {
                            tracing::info!(
                                "sync group '{anchor_name}': pw-sink senders advertising {} target session(s)",
                                d.pwsink_members.len()
                            );
                            if let Some(rg) = self.running.get_mut(key) {
                                rg.pwsink_server = Some(handle);
                                rg.pwsink_members = d.pwsink_members.clone();
                                rg.pwsink_ports = ports;
                            }
                        }
                        Err(e) => {
                            tracing::warn!("sync group '{anchor_name}': failed to start pw-sink senders: {e} — retrying shortly");
                            self.retry_wanted = true;
                        }
                    }
                }
            }
        }

        // 3. Idle-sender creation (per-device mode): stand up a standalone sender
        //    for every ungrouped device that doesn't have one, so it's always
        //    reachable. Its own silent sink → it streams silence until the overlay
        //    mixer injects an announcement, then falls back to silence.
        for dev in &want_idle {
            if self.idle_senders.contains_key(dev) {
                continue;
            }
            let Some(fullname) = devices_map.get(dev).map(|d| d.fullname.clone()) else {
                continue;
            };
            // Single-device stream, so the codec is just this device's choice
            // narrowed by what it advertised (and what we can encode).
            let (idle_codec, idle_lead_us) = {
                let ss = sync_settings.lock_recover();
                let caps = devices_map.get(dev).map(|d| d.supported_codecs.clone()).unwrap_or_default();
                let codec = sendspin::server::resolve_codec(ss.sendspin_codec(dev), std::iter::once(&caps));
                let lead = sendspin::server::required_send_ahead_us(
                    send_ahead_us,
                    codec,
                    ss.opus_floor_ms(),
                    std::iter::once((
                        devices_map.get(dev).and_then(|d| d.min_buffer_ms),
                        ss.sendspin_delays().get(dev).copied().unwrap_or(0),
                    )),
                );
                (codec, lead)
            };
            let suffix = dev.strip_prefix(crate::util::node_names::SENDSPIN_DEV_PREFIX).unwrap_or(dev);
            let sink_node_name = format!("{IDLE_SINK_PREFIX}{suffix}");
            let (tx, rx) = oneshot::channel();
            if pw_cmd.send(PwCommand::CreateSinkNode { node_name: sink_node_name.clone(), reply: tx }).is_err() {
                continue;
            }
            match rx.await {
                Ok(Ok(())) => {}
                _ => {
                    tracing::warn!("idle sender: failed to create sink '{sink_node_name}' — retrying shortly");
                    self.retry_wanted = true;
                    continue;
                }
            }
            let Some(sink_node_id) = wait_for_node(pw, &sink_node_name).await else {
                tracing::warn!("idle sender: sink '{sink_node_name}' did not appear — retrying shortly");
                self.retry_wanted = true;
                continue;
            };
            let port = self.alloc_port(&HashSet::new());
            // One member, from the registry — the URL is what the single daemon-wide
            // browser resolved (an idle sender doesn't browse either).
            let Some(idle_url) = devices_map.get(dev).and_then(|d| d.url.clone()) else {
                tracing::debug!("idle sender for '{dev}': no resolved URL yet; retrying shortly");
                self.retry_wanted = true;
                continue;
            };
            let members = vec![(fullname, idle_url)];
            match sendspin::server::start_server_per_device(
                &sink_node_name,
                &format!("idle: {}", routing::output_display_name(dev)),
                port,
                sink_node_id,
                members,
                idle_lead_us,
                control.clone(),
                devices.clone(),
                // Idle: stay connected (warm + controllable) but stream nothing
                // until an announcement is actually headed for this device.
                sendspin::server::StreamPolicy::WhenAnnounced,
                idle_codec,
            )
            .await
            {
                Ok(server) => {
                    tracing::info!("idle sender for '{dev}' up on port {port} (silence until announced)");
                    self.idle_senders.insert(dev.clone(), IdleSender { sink_node_name, sink_node_id, port, server });
                }
                Err(e) => {
                    tracing::warn!("idle sender for '{dev}': failed to start: {e} — retrying shortly");
                    self.retry_wanted = true;
                    let (t, r) = oneshot::channel();
                    if pw_cmd.send(PwCommand::DestroySinkNode { node_id: sink_node_id, reply: t }).is_ok() {
                        let _ = r.await;
                    }
                }
            }
        }
    }
}

/// Poll until `node_name` is present in the live registry (or give up). Mirrors
/// sendspin_server's old wait-for-node before linking a freshly-created sink.
async fn wait_for_node(pw: &SharedState, node_name: &str) -> Option<u32> {
    for _ in 0..40 {
        if let Some(id) = node_id_for(&pw.lock_recover(), node_name) {
            return Some(id);
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    None
}

// The reconciler's pieces. Re-exported so the code and its tests keep addressing
// them by name: these boundaries organise the file, they are not an interface.
mod announce;
mod desired;
mod model;
mod sendspin_state;

pub(crate) use announce::*;
pub(crate) use desired::*;
pub(crate) use model::*;
pub(crate) use sendspin_state::*;

#[cfg(test)]
mod tests;
