use super::*;

// ---- Latency alignment (align/calibrate/mod.rs) -----------------------------

/// Alignable groups (a source-set with its present members), for the picker.
pub(crate) async fn align_groups(State(state): State<AppState>) -> Json<Vec<crate::align::calibrate::AlignGroup>> {
    Json(state.align.groups().await)
}

/// Current calibration state (active session or not).
pub(crate) async fn align_status(State(state): State<AppState>) -> Json<crate::align::calibrate::AlignState> {
    Json(state.align.status().await)
}

/// `GET /api/align/ws` — the **session** state, pushed: one full `AlignState` on connect,
/// then one on every change, and one on **teardown**.
///
/// The same shape and the same push loop as the run's socket
/// (`align::status_ws::status_socket`), and worth having for a different reason. A
/// measurement is pushed because it spends minutes inside gates where only the *message*
/// moves; the session is pushed because the one state change a client cannot predict is
/// the session **ending** — by the idle timeout, by a superseding `start`, or by a stop
/// issued from another tab — and until it hears about that it is showing a wizard for a
/// session that no longer exists while the speakers it named have already gone back to
/// normal. `GET /api/align` stays, and the client falls back to it: a UI that shows
/// nothing because this route 404'd would be worse than one that polls.
///
/// It takes no commands, and a frame sent on it is deliberately **not** activity — see
/// `AlignManager::still_here` for why a held socket must never postpone the timeout.
pub(crate) async fn align_ws(State(state): State<AppState>, ws: axum::extract::ws::WebSocketUpgrade) -> impl axum::response::IntoResponse {
    ws.on_upgrade(move |socket| async move {
        let align = state.align.clone();
        // Subscribed *before* the first status is read, so a change that lands between
        // the two is a redundant push rather than a missed one.
        let changes = align.subscribe();
        crate::align::status_ws::status_socket(socket, changes, move || {
            // A clone per frame rather than a borrow: `AlignManager` is a bundle of
            // `Arc`s, and owning one is what lets the snapshot future be `'static`.
            let align = align.clone();
            Box::pin(async move { serde_json::to_string(&align.status().await).ok() })
        })
        .await;
    })
}

/// `POST /api/align/still-here` — postpone the idle teardown, changing nothing else.
///
/// **Must be driven by a click, never by a timer.** The timeout exists so that a tab
/// nobody is watching cannot leave a room muted, and renewing the session automatically
/// would put that hazard straight back — invisibly. See `AlignManager::still_here`.
pub(crate) async fn align_still_here(
    State(state): State<AppState>,
) -> Result<Json<crate::align::calibrate::AlignState>, (StatusCode, Json<OutputOpResponse>)> {
    tracing::info!("USER ACTION: keep the alignment session open");
    state.align.still_here().await.map(Json).map_err(|e| (StatusCode::CONFLICT, Json(OutputOpResponse { ok: false, message: e })))
}

/// The shared handles an alignment session needs to form and hold its temporary
/// exclusive group (align/group.rs). Assembled here so `AlignManager` — which
/// main.rs builds with three handles — needs no new constructor arguments.
pub(crate) fn hold_deps(state: &AppState) -> crate::align::group::HoldDeps<'_> {
    crate::align::group::HoldDeps { groups: &state.groups, changes: &state.changes, routing: &state.routing, outputs: &state.outputs }
}

#[derive(Deserialize)]
pub(crate) struct AlignStartRequest {
    /// **Either**: source node names identifying an existing group to align (the
    /// by-ear entry point from a source card — its present members are what gets
    /// held).
    #[serde(default)]
    pub(crate) sources: Vec<String>,
    /// **Or**: the speakers to align, picked on the Outputs page (plan §12.1). A
    /// temporary exclusive group is formed around exactly these, independent of how
    /// they are routed now.
    ///
    /// **This is the run's whole scope, not one position's** (plan §12.3.1). It reads
    /// counter-intuitively next to a wizard that then works on a subset, so:
    ///
    /// - send every speaker the run will touch — the floor, the apartment — because
    ///   forming the group reconnects each of them (tens of seconds each, plan §2.3)
    ///   and releasing it does so again. Paying that once is the point;
    /// - then scope each position with `POST /api/align/audible`, which only moves
    ///   mutes and is free;
    /// - a `start` naming the same speakers, or **any subset** of them, does **not**
    ///   re-form anything: the hold keeps its id, its anchor and its senders, nothing
    ///   reconnects, and only audibility changes. `hold_id` in the response is
    ///   unchanged, `hold_reused` is `true`;
    /// - only a selection needing a speaker the current hold does not cover tears the
    ///   session down and forms a new group. `hold_cost` says which of the two
    ///   happened, in words.
    #[serde(default)]
    pub(crate) outputs: Vec<String>,
    /// Which acoustic promise the run makes (plan §1). Only meaningful with
    /// `outputs`; a `sources` start is by-ear by construction. Changing it on a
    /// reusing `start` is free — the mode describes the run, not the group.
    #[serde(default)]
    pub(crate) mode: crate::align::group::AlignMode,
}

/// `POST /api/align/start` — hold a set of speakers exclusively for an alignment run.
///
/// Responds with the whole `AlignState`, including what this call **cost**:
/// `hold_id` (unchanged ⇒ nothing re-formed), `hold_reused`, `hold_cost` (a sentence),
/// and `level_note` when a member has no level knob (a pw-sink host, plan §7).
pub(crate) async fn align_start(
    State(state): State<AppState>,
    Json(req): Json<AlignStartRequest>,
) -> Result<Json<crate::align::calibrate::AlignState>, (StatusCode, Json<OutputOpResponse>)> {
    let deps = hold_deps(&state);
    let result = match (req.outputs.is_empty(), req.sources.is_empty()) {
        (false, _) => {
            tracing::info!("USER ACTION: start alignment ({:?}) on {} selected output(s)", req.mode, req.outputs.len());
            state.align.start_outputs(&deps, req.outputs, req.mode).await
        }
        (true, false) => state.align.start(&deps, req.sources).await,
        (true, true) => Err("give either `outputs` (the speakers to align) or `sources` (an existing group)".to_string()),
    };
    result.map(Json).map_err(|e| (StatusCode::BAD_REQUEST, Json(OutputOpResponse { ok: false, message: e })))
}

#[derive(Deserialize)]
pub(crate) struct AlignAudibleRequest {
    /// The members to make audible — one for level-setting/measurement (plan §12.2),
    /// all of them for §7's all-play headroom round.
    pub(crate) audible: Vec<String>,
    /// Playback level 0–100; omitted keeps the session's current level.
    #[serde(default)]
    pub(crate) level: Option<u8>,
}

/// `POST /api/align/audible` — plan §12.2's solo, generalised: make exactly these
/// members audible and mute the rest.
///
/// **This is how a position is scoped** (plan §12.3.1): mutes are live and cost
/// nothing, whereas re-selecting speakers by starting a *different* union re-forms the
/// group and reconnects every member. A multi-position walk should call `start` once
/// with the whole scope and then only come here.
pub(crate) async fn align_audible(
    State(state): State<AppState>,
    Json(req): Json<AlignAudibleRequest>,
) -> Result<Json<crate::align::calibrate::AlignState>, (StatusCode, Json<OutputOpResponse>)> {
    let level = match req.level {
        Some(l) => l,
        None => state.align.status().await.volume,
    };
    state
        .align
        .set_audible(req.audible, level)
        .await
        .map(Json)
        .map_err(|e| (StatusCode::BAD_REQUEST, Json(OutputOpResponse { ok: false, message: e })))
}

#[derive(Deserialize)]
pub(crate) struct AlignSelectRequest {
    /// Member kept audible as the fixed reference.
    pub(crate) reference: String,
    /// Member being tuned (audible alongside the reference).
    pub(crate) target: String,
}

pub(crate) async fn align_select(
    State(state): State<AppState>,
    Json(req): Json<AlignSelectRequest>,
) -> Result<Json<crate::align::calibrate::AlignState>, (StatusCode, Json<OutputOpResponse>)> {
    state
        .align
        .select(req.reference, req.target)
        .await
        .map(Json)
        .map_err(|e| (StatusCode::BAD_REQUEST, Json(OutputOpResponse { ok: false, message: e })))
}

#[derive(Deserialize)]
pub(crate) struct AlignVolumeRequest {
    /// Audible-member playback level, 0–100.
    pub(crate) volume: u8,
}

pub(crate) async fn align_volume(
    State(state): State<AppState>,
    Json(req): Json<AlignVolumeRequest>,
) -> Result<Json<crate::align::calibrate::AlignState>, (StatusCode, Json<OutputOpResponse>)> {
    state
        .align
        .set_level(req.volume)
        .await
        .map(Json)
        .map_err(|e| (StatusCode::BAD_REQUEST, Json(OutputOpResponse { ok: false, message: e })))
}

#[derive(Deserialize)]
pub(crate) struct AlignChannelsRequest {
    pub(crate) node_name: String,
    /// `both` (the default), `left` or `right` — which wire channels this member emits
    /// for the rest of the session.
    pub(crate) channels: String,
}

/// `POST /api/align/channel` — measure one member through **one channel** of its pair.
///
/// The click is identical on both channels, so a member driving a stereo pair is two
/// acoustic sources and has no single arrival time; the estimator refuses it as an
/// ambiguous peak. Live, per member, nothing persisted, both channels restored on
/// teardown.
pub(crate) async fn align_channels(
    State(state): State<AppState>,
    Json(req): Json<AlignChannelsRequest>,
) -> Result<Json<crate::align::calibrate::AlignState>, (StatusCode, Json<OutputOpResponse>)> {
    let bad = |message: String| (StatusCode::BAD_REQUEST, Json(OutputOpResponse { ok: false, message }));
    let channels = crate::align::relay_delay::MeasureChannels::parse(&req.channels)
        .ok_or_else(|| bad(format!("'{}' is not a channel choice (expected both, left or right)", req.channels)))?;
    tracing::info!("USER ACTION: measure '{}' through {} channel(s)", req.node_name, channels.as_str());
    state.align.set_channels(req.node_name, channels).await.map(Json).map_err(bad)
}

/// Stop the session: click off, every member's level/mute restored, the temporary
/// exclusive group released so the displaced music comes back.
pub(crate) async fn align_stop(State(state): State<AppState>) -> Json<crate::align::calibrate::AlignState> {
    Json(state.align.stop().await)
}
