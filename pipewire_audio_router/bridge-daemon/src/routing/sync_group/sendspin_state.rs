//! Whether a sendspin device's server needs starting, restarting or leaving alone.
//!
//! A pure decision table over [`SendspinServerState`], separated from the reconciler
//! because the cost of the wrong answer is asymmetric and worth reading in isolation:
//! a needless restart costs a device reconnect — tens of seconds of silence on that
//! speaker — while a missed one leaves it playing stale settings.
//!
//! [`sendspin_config_changed`] is what "stale" means here: the codec and the group
//! lead, the two things a running stream cannot be told about after the fact.

/// What a reconcile pass should do with one group's sendspin server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ServerAction {
    /// Nothing routed here and nothing running.
    Idle,
    /// Keep the running server exactly as it is. Membership and address changes
    /// land here — they're applied to the live server instead.
    KeepRunning,
    /// (Re)start it, tearing down any existing one first.
    Start,
    /// Tear it down: no sendspin device is routed to this group any more.
    Stop,
}

/// The inputs that decide it.
pub(crate) struct SendspinServerState {
    /// Is any sendspin device routed to this group at all? (Not "dialable" — a
    /// device whose URL hasn't resolved yet still wants the server up, and gets
    /// supervised the moment it resolves.)
    pub(crate) routed: bool,
    pub(crate) have_server: bool,
    /// Did the **stream config** change — the codec or the send-ahead? That is the
    /// server's whole restart identity, because it's what `stream/start` carries and
    /// what the shared timeline fixes at construction. See
    /// [`sendspin_config_changed`].
    pub(crate) config_changed: bool,
}

/// Decide it. Extracted from `reconcile` so the rule is testable without a live
/// PipeWire graph — and because "which changes restart a group" is exactly the
/// thing that regressed: membership used to be part of the identity, so routing one
/// more speaker into a live group made every other member reconnect
/// (docs/old/sendspin-group-churn-plan.md §2b).
///
/// There is deliberately no "a caller asked for a reconnect" input any more. The one
/// caller that did — a per-device static-delay edit — now recycles just that device's
/// connection ([`GroupReconciler::force_device_reconnect`], §4.10); the only part of a
/// delay change that is genuinely group-wide reaches this function as
/// `config_changed`, via the send-ahead.
pub(crate) fn sendspin_server_action(s: SendspinServerState) -> ServerAction {
    match (s.routed, s.have_server) {
        (false, false) => ServerAction::Idle,
        (false, true) => ServerAction::Stop,
        (true, false) => ServerAction::Start,
        (true, true) if s.config_changed => ServerAction::Start,
        (true, true) => ServerAction::KeepRunning,
    }
}

/// Does the group's wanted stream config differ from what the running server was
/// started with — i.e. must **every** member re-arm?
///
/// Two asymmetric rules, and the asymmetry is the whole point:
///
/// - **Codec**: any change. A different `stream/start` payload, and the timeline's
///   encoder is fixed at construction.
/// - **Send-ahead**: only a **raise**. `server_send_ahead_us` is a high-water mark
///   (§4.6) — it is a floor the spec asks us to clear, derived from membership *and*
///   from each member's static delay, so it moves both ways as devices and delays
///   change. Honouring a *drop* would reconnect every member to buy tens of
///   milliseconds of latency back, and a reconnect costs tens of seconds of silence
///   per speaker (§4.9). Keeping a stale-but-larger lead costs latency, never
///   correctness.
///
/// This is also what decides the blast radius of a static-delay edit (§4.10): a
/// member's delay feeds `required_send_ahead_us`, so a delay big enough to push the
/// group's requirement past the running lead lands here and re-arms everyone —
/// correct, because they share one timeline. Anything smaller leaves this false, and
/// only the edited device reconnects.
///
/// **Honest caveat.** "Does the group lead move" is judged against the *running*
/// server's high-water mark, not against the freshly-computed
/// `group_lead_effective_ms` that `GET /api/sync/settings` reports. Those differ
/// after a member with a large requirement leaves: the API's number drops while the
/// mark stays. So a delay edit can change the reported effective lead and still
/// (deliberately) not re-arm the group.
pub(crate) fn sendspin_config_changed(prev_codec: &str, prev_lead_us: i64, want_codec: &str, want_lead_us: i64) -> bool {
    want_codec != prev_codec || want_lead_us > prev_lead_us
}
