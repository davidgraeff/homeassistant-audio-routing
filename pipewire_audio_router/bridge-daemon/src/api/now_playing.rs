use super::*;

// ---- Now-playing metadata (sources/now_playing.rs) --------------------------------
//
// Reads are the cold-path companion to the `now_playing` WebSocket frame (a
// consumer that just connected, or `curl`). Writes are how a *remote* producer
// reports — the local AirPlay receiver writes straight into the store through its
// own reporter handle and never comes through here.

/// `GET /api/now_playing` — every source with something to say, keyed by node
/// name. Same payload as the socket's `now_playing` frame.
pub(crate) async fn list_now_playing(State(state): State<AppState>) -> Json<NowPlayingListResponse> {
    Json(NowPlayingListResponse { sources: state.now_playing.snapshot() })
}

/// `GET /api/now_playing/{node_name}` — one source, or 404 when nothing is known.
pub(crate) async fn get_now_playing(
    State(state): State<AppState>,
    Path(node_name): Path<String>,
) -> Result<Json<crate::sources::now_playing::NowPlaying>, SourceError> {
    state
        .now_playing
        .get(&node_name)
        .map(Json)
        .ok_or_else(|| source_err(StatusCode::NOT_FOUND, format!("nothing playing on '{node_name}'")))
}

/// `PUT /api/now_playing/{node_name}` — merge metadata into a source by node name.
///
/// Only *configured* sources are accepted. That is the one check worth making:
/// this endpoint is unauthenticated like the rest of the API, and without it a
/// caller could invent listings for nodes that do not exist, which would then
/// appear in the socket frame and in Home Assistant.
pub(crate) async fn put_now_playing(
    State(state): State<AppState>,
    Path(node_name): Path<String>,
    Json(update): Json<crate::sources::now_playing::MetadataUpdate>,
) -> Result<Json<OutputOpResponse>, SourceError> {
    if update.is_empty() {
        return Err(source_err(StatusCode::BAD_REQUEST, "no metadata fields supplied".to_string()));
    }
    let known = state.sources.lock_recover().list().iter().any(|e| e.node_name() == node_name);
    if !known {
        return Err(source_err(StatusCode::NOT_FOUND, format!("no source with node name '{node_name}'")));
    }
    state.now_playing.reporter(node_name.clone()).update(update);
    Ok(Json(OutputOpResponse { ok: true, message: format!("now playing updated for '{node_name}'") }))
}

/// `DELETE /api/now_playing/{node_name}` — the session ended.
///
/// A producer that goes away should say so rather than leave the TTL to collect
/// it, so Home Assistant's media card collapses instead of freezing on the last
/// track. Idempotent: clearing something already gone is a success.
pub(crate) async fn clear_now_playing(State(state): State<AppState>, Path(node_name): Path<String>) -> Json<OutputOpResponse> {
    state.now_playing.reporter(node_name.clone()).clear();
    Json(OutputOpResponse { ok: true, message: format!("now playing cleared for '{node_name}'") })
}

/// `GET /api/now_playing/{node_name}/artwork` — the embedded cover art bytes.
///
/// The `?rev=` in the published path is a cache-buster, not a selector: there is
/// only ever one current image per source, so the query is deliberately ignored
/// here and the *current* art is returned.
pub(crate) async fn get_now_playing_artwork(State(state): State<AppState>, Path(node_name): Path<String>) -> Response {
    match state.now_playing.artwork(&node_name) {
        Some((bytes, mime, rev)) => (
            StatusCode::OK,
            [
                (header::CONTENT_TYPE, mime),
                // Immutable *for this rev*: the path changes when the picture does,
                // so a consumer (and HA's image proxy) can cache hard.
                (header::CACHE_CONTROL, "public, max-age=3600, immutable".to_string()),
                (header::ETAG, format!("\"{node_name}-{rev}\"")),
            ],
            Body::from(bytes.to_vec()),
        )
            .into_response(),
        None => (StatusCode::NOT_FOUND, "no artwork").into_response(),
    }
}

/// `POST /api/now_playing/report` — the self-identifying entry point for a remote
/// producer (the Pi bridge's reporter, plan §3.4/WP3).
///
/// A reporter on the Pi knows its own RTP port; it does not know — and must not
/// have to learn — the source ids this daemon assigned. So it says "I am the
/// sender on port 46000" and the port is resolved against the source store, which
/// is the same key `bt_bridge_discovery` already matches an advert to a source by.
/// A port matching no configured source is a `404`, not an error worth retrying:
/// the bridge may simply be set up before its source was added here.
pub(crate) async fn report_now_playing(
    State(state): State<AppState>,
    Json(req): Json<NowPlayingReportRequest>,
) -> Result<Json<OutputOpResponse>, SourceError> {
    let node_name = {
        let sources = state.sources.lock_recover();
        sources.list().iter().find(|e| matches!(&e.config, SourceConfig::Rtp(r) if r.port == req.rtp_port)).map(|e| e.node_name())
    };
    let Some(node_name) = node_name else {
        return Err(source_err(StatusCode::NOT_FOUND, format!("no RTP source is configured on port {}", req.rtp_port)));
    };
    let reporter = state.now_playing.reporter(node_name.clone());
    // An empty body is how a reporter says "nothing is playing any more" without
    // needing a second endpoint — its BlueZ player object simply went away.
    if req.metadata.is_empty() {
        reporter.clear();
        return Ok(Json(OutputOpResponse { ok: true, message: format!("now playing cleared for '{node_name}'") }));
    }
    reporter.update(req.metadata);
    Ok(Json(OutputOpResponse { ok: true, message: format!("now playing updated for '{node_name}'") }))
}

#[derive(Serialize)]
pub(crate) struct NowPlayingListResponse {
    pub(crate) sources: std::collections::BTreeMap<String, crate::sources::now_playing::NowPlaying>,
}

#[derive(Deserialize)]
pub(crate) struct NowPlayingReportRequest {
    /// The UDP port this reporter's `module-rtp-sink` transmits to — the key that
    /// identifies which of our sources it is feeding.
    pub(crate) rtp_port: u16,
    /// The metadata itself, flattened, so the body reads as one object rather than
    /// a wrapper: `{"rtp_port": 46000, "title": "…", "artist": "…"}`.
    #[serde(flatten)]
    pub(crate) metadata: crate::sources::now_playing::MetadataUpdate,
}
