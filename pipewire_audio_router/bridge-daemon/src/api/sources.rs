use super::*;

// ---- RTP source (Bluetooth bridge firmware target) ------------------------
//
// An RTP source is a native PipeWire module, not a subprocess, so it is
// loaded/unloaded via the PipeWire thread (PwCommand::Load/Unload, keyed by the
// entry's own node name) rather than through the process supervisor. Re-point it
// live, no restart. Once loaded its node shows up in the routing matrix
// automatically (routing/mod.rs classifies it as a source).

// ---- Multi-source collection CRUD (Phase 3) ------------------------------
//
// The generalized, keyed replacement for the two singular `/api/source/*` routes
// this daemon used to expose: a collection of AirPlay + RTP input sources, each with its own
// stable id / node name (sources/mod.rs). These handlers only mutate the
// STORE — actually loading/unloading the PipeWire module (RTP) or starting/
// stopping the embedded receiver (AirPlay) is done by the per-kind reconcilers
// wired from main.rs (Phases 2 & 4). After each mutation we nudge the change
// notifier so watchers refresh.

/// Nested response shape for a single source. Distinct from the *flat* stored
/// [`SourceEntry`] (`{id,label,kind,<config...>}`): here the kind-specific
/// config is nested under `airplay`/`rtp` (exactly one non-null), plus the
/// derived `node_name` and the live `present` flag. Matches the frontend's
/// `SourceView` (Phase 5) exactly.
#[derive(Serialize, Debug, PartialEq, Eq)]
pub(crate) struct SourceView {
    pub(crate) id: String,
    pub(crate) label: String,
    pub(crate) kind: SourceKind,
    /// A node named `node_name` exists in the live PipeWire registry right now
    /// (the source is actually loaded/running).
    pub(crate) present: bool,
    pub(crate) node_name: String,
    /// The AirPlay config when `kind == airplay`, else `null`.
    pub(crate) airplay: Option<AirplaySourceConfig>,
    /// The RTP config when `kind == rtp`, else `null`.
    pub(crate) rtp: Option<RtpSourceConfig>,
    /// The discovered Bluetooth bridge that feeds this RTP source, when exactly
    /// one advertises this port (and group). `null` for AirPlay sources, when no
    /// bridge advertises, or when two do — see `sources::bt_bridge::match_bridge`.
    pub(crate) bridge: Option<BridgeView>,
}

/// A discovered Bluetooth→RTP bridge, as the UI sees it. Used both nested on an
/// RTP source (`bridge`) and standalone in `discovered_bridges`.
#[derive(Serialize, Debug, PartialEq, Eq)]
pub(crate) struct BridgeView {
    /// mDNS instance fullname — the stable identity the UI keys on.
    pub(crate) fullname: String,
    /// Human label from the advert (the bridge's Bluetooth/host name).
    pub(crate) display_name: String,
    /// mDNS hostname, for display when the address is uninformative.
    pub(crate) hostname: String,
    /// Resolved address, `null` until mDNS resolves one.
    pub(crate) addr: Option<String>,
    /// UDP port this bridge sends RTP to (what a source must listen on).
    pub(crate) rtp_port: u16,
    /// Its RTP destination: the HA host's address, or a multicast group.
    pub(crate) rtp_dest: String,
    pub(crate) rate: u32,
    pub(crate) channels: u16,
    /// Diagnostics page URL, `null` while the address is unresolved.
    pub(crate) diag_url: Option<String>,
    /// The diagnostics app answered on the last probe. **The UI must only offer
    /// the link when this is true** — the advert is installed by
    /// `setup_pi_bridge.py` and outlives any particular app run.
    pub(crate) diag_ok: bool,
}

impl BridgeView {
    fn of(b: &crate::sources::bt_bridge::BtBridge) -> Self {
        Self {
            fullname: b.fullname.clone(),
            display_name: b.display_name.clone(),
            hostname: b.hostname.trim_end_matches('.').to_string(),
            addr: b.addr.map(|a| a.to_string()),
            rtp_port: b.stream.rtp_port,
            rtp_dest: b.stream.rtp_dest.clone(),
            rate: b.stream.rate,
            channels: b.stream.channels,
            diag_url: b.diag_url(),
            diag_ok: b.diag_ok(),
        }
    }
}

/// Pure conversion: flat stored [`SourceEntry`] + a live `present` flag → the
/// nested [`SourceView`] wire shape. Kept side-effect-free so it is unit-tested
/// directly (see tests below).
///
/// `bridges` is the discovered-bridge set to match an RTP source against; pass an
/// empty slice to skip (AirPlay sources never match).
pub(crate) fn source_view(entry: &SourceEntry, present: bool, bridges: &[crate::sources::bt_bridge::BtBridge]) -> SourceView {
    let (airplay, rtp) = match &entry.config {
        SourceConfig::Airplay(a) => (Some(a.clone()), None),
        SourceConfig::Rtp(r) => (None, Some(r.clone())),
    };
    let bridge =
        rtp.as_ref().and_then(|r| crate::sources::bt_bridge::match_bridge(bridges.iter(), r.port, &r.source_addr)).map(BridgeView::of);
    SourceView {
        id: entry.id.clone(),
        label: entry.label.clone(),
        kind: entry.kind(),
        present,
        node_name: entry.node_name(),
        airplay,
        rtp,
        bridge,
    }
}

/// Whether a node with `node_name` is present in the live registry right now.
pub(crate) fn node_present(pw: &SharedState, node_name: &str) -> bool {
    pw.lock_recover().nodes.values().any(|n| n.node_name == node_name)
}

/// The set of node names currently present in the live registry — snapshotted
/// once so a list of sources can be resolved without re-locking per entry.
pub(crate) fn present_node_names(pw: &SharedState) -> std::collections::HashSet<String> {
    pw.lock_recover().nodes.values().map(|n| n.node_name.clone()).collect()
}

/// A default RTP config (all knobs at their `DEFAULT_RTP_*`), used when a
/// `POST` omits the `rtp` object. `RtpSourceConfig` has no `Default` impl, so
/// this spells it out from the shared constants.
pub(crate) fn default_rtp_config() -> RtpSourceConfig {
    RtpSourceConfig {
        port: DEFAULT_RTP_PORT,
        latency_msec: DEFAULT_RTP_LATENCY_MSEC,
        source_addr: DEFAULT_RTP_SOURCE_ADDR.to_string(),
        ignore_ssrc: DEFAULT_RTP_IGNORE_SSRC,
        rate: DEFAULT_RTP_RATE,
    }
}

#[derive(Serialize)]
pub(crate) struct SourcesListResponse {
    pub(crate) sources: Vec<SourceView>,
    /// Discovered Bluetooth bridges that no configured RTP source is listening
    /// for — offered on the Sources tab for one-click adoption, with their
    /// advertised port/rate prefilled so the two ends cannot disagree by typo.
    /// A bridge already matched by a source appears there (`sources[].bridge`)
    /// and is omitted here, so the list is exactly "what is missing".
    pub(crate) discovered_bridges: Vec<BridgeView>,
}

/// `POST /api/sources` body. `kind` selects which config object is honored; the
/// matching `airplay`/`rtp` object carries partial fields (every field has a
/// serde default), and may be omitted entirely to accept all defaults.
#[derive(Deserialize)]
pub(crate) struct CreateSourceRequest {
    pub(crate) label: String,
    pub(crate) kind: SourceKind,
    #[serde(default)]
    pub(crate) airplay: Option<AirplaySourceConfig>,
    #[serde(default)]
    pub(crate) rtp: Option<RtpSourceConfig>,
}

/// `PUT /api/sources/{id}` body. All fields optional: `label` renames, and an
/// `airplay`/`rtp` object replaces the config (must match the source's
/// immutable kind — the store rejects a mismatch). Omitting both config objects
/// is a label-only update.
#[derive(Deserialize)]
pub(crate) struct UpdateSourceRequest {
    #[serde(default)]
    pub(crate) label: Option<String>,
    #[serde(default)]
    pub(crate) airplay: Option<AirplaySourceConfig>,
    #[serde(default)]
    pub(crate) rtp: Option<RtpSourceConfig>,
}

/// A source-CRUD failure. The one convention: the status comes from the kind and the
/// body is `{kind, message}` — see `api::error`.
pub(crate) type SourceError = ApiError;

/// Kept as a helper because the source handlers refuse in a dozen places; the `code` is
/// mapped to the kind rather than sent raw, so a new call site cannot invent a status the
/// vocabulary does not cover.
pub(crate) fn source_err(code: StatusCode, message: String) -> SourceError {
    match code {
        StatusCode::NOT_FOUND => ApiError::not_found(message),
        StatusCode::CONFLICT => ApiError::conflict(message),
        StatusCode::SERVICE_UNAVAILABLE => ApiError::unavailable(message),
        StatusCode::INTERNAL_SERVER_ERROR => ApiError::internal(message),
        _ => ApiError::bad_request(message),
    }
}

pub(crate) async fn list_sources(State(state): State<AppState>) -> Json<SourcesListResponse> {
    // Probe stale diagnostics pages first, so `bridge.diag_ok` in this very
    // response reflects a fresh check. Only when someone is looking (this
    // handler), rate-limited by `PROBE_TTL` — no background polling of Pi Zeros.
    crate::sources::bt_bridge::refresh_probes(&state.bt_bridges).await;

    let entries = state.sources.lock_recover().list();
    let present = present_node_names(&state.pw);
    let bridges: Vec<_> = state.bt_bridges.lock_recover().values().cloned().collect();
    let sources: Vec<SourceView> = entries.iter().map(|e| source_view(e, present.contains(&e.node_name()), &bridges)).collect();
    let discovered_bridges = unmatched_bridges(&bridges, &sources);
    Json(SourcesListResponse { sources, discovered_bridges })
}

/// The bridges not already claimed by a source in `sources`, as views.
///
/// Pure so the "already configured disappears from the offer list" rule is
/// unit-tested: a bridge visible in both lists would invite the user to add a
/// duplicate source on a port that is already taken.
pub(crate) fn unmatched_bridges(bridges: &[crate::sources::bt_bridge::BtBridge], sources: &[SourceView]) -> Vec<BridgeView> {
    let claimed: std::collections::HashSet<&str> = sources.iter().filter_map(|s| s.bridge.as_ref()).map(|b| b.fullname.as_str()).collect();
    bridges.iter().filter(|b| !claimed.contains(b.fullname.as_str())).map(BridgeView::of).collect()
}

pub(crate) async fn create_source(
    State(state): State<AppState>,
    Json(req): Json<CreateSourceRequest>,
) -> Result<(StatusCode, Json<SourceView>), SourceError> {
    let config = match req.kind {
        SourceKind::Airplay => SourceConfig::Airplay(req.airplay.unwrap_or_default()),
        SourceKind::Rtp => SourceConfig::Rtp(req.rtp.unwrap_or_else(default_rtp_config)),
    };
    let entry = {
        let mut store = state.sources.lock_recover();
        // add() validates (e.g. RTP port collisions) — surface that as a 400.
        store.add(req.label, config).map_err(|e| source_err(StatusCode::BAD_REQUEST, e.to_string()))?
    };
    // Load/start the new source now, then nudge downstream (routing/groups).
    reconcile_sources(&state).await;
    let _ = state.changes.send(());
    let present = node_present(&state.pw, &entry.node_name());
    let bridges: Vec<_> = state.bt_bridges.lock_recover().values().cloned().collect();
    Ok((StatusCode::CREATED, Json(source_view(&entry, present, &bridges))))
}

pub(crate) async fn get_source(State(state): State<AppState>, Path(id): Path<String>) -> Result<Json<SourceView>, SourceError> {
    let entry =
        state.sources.lock_recover().get(&id).ok_or_else(|| source_err(StatusCode::NOT_FOUND, format!("no source with id '{id}'")))?;
    let present = node_present(&state.pw, &entry.node_name());
    let bridges: Vec<_> = state.bt_bridges.lock_recover().values().cloned().collect();
    Ok(Json(source_view(&entry, present, &bridges)))
}

pub(crate) async fn update_source(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<UpdateSourceRequest>,
) -> Result<Json<SourceView>, SourceError> {
    // Kind is immutable, so it's derived from whichever config object is present
    // (the store rejects a config whose kind differs from the stored entry's).
    let config = match (req.airplay, req.rtp) {
        (Some(a), _) => Some(SourceConfig::Airplay(a)),
        (None, Some(r)) => Some(SourceConfig::Rtp(r)),
        (None, None) => None,
    };
    let entry = {
        let mut store = state.sources.lock_recover();
        store.update(&id, req.label, config).map_err(|e| {
            let msg = e.to_string();
            // "no source with id" → 404; validation (kind change, port clash) → 400.
            let code = if msg.contains("no source with id") { StatusCode::NOT_FOUND } else { StatusCode::BAD_REQUEST };
            source_err(code, msg)
        })?
    };
    // Apply the config change to the running source, then nudge downstream.
    reconcile_sources(&state).await;
    let _ = state.changes.send(());
    let present = node_present(&state.pw, &entry.node_name());
    let bridges: Vec<_> = state.bt_bridges.lock_recover().values().cloned().collect();
    Ok(Json(source_view(&entry, present, &bridges)))
}

pub(crate) async fn delete_source(State(state): State<AppState>, Path(id): Path<String>) -> OpResult {
    // Bind the result so the std MutexGuard drops HERE — a match scrutinee holds
    // its temporaries for the whole match, which would keep the guard alive
    // across the `.await` below and make the handler future `!Send`.
    let removed = state.sources.lock_recover().remove(&id);
    match removed {
        Ok(true) => {
            // Unload/stop the removed source now, then nudge downstream.
            reconcile_sources(&state).await;
            let _ = state.changes.send(());
            ok(format!("removed source '{id}'"))
        }
        Ok(false) => Err(ApiError::not_found(format!("no source with id '{id}'"))),
        Err(e) => Err(ApiError::internal(format!("failed to persist: {e}"))),
    }
}
