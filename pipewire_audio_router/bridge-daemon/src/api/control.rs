use super::*;

// ---- Source control endpoints (sources/control.rs) ------------------------------
//
// A producer that can be *driven* announces where; the announcement is read back on
// the source listing (`SourceView.control`). The daemon never talks to that URL.

/// `POST /api/control/report` — a producer announces (or withdraws) its control API.
///
/// Self-identifying like `POST /api/now_playing/report`: the producer knows the UDP port
/// it transmits to, not the source id this daemon assigned, so the port is resolved
/// against the source store. `"url": null` withdraws the announcement — what a producer
/// sends on its way out, so Home Assistant marks it unavailable at once instead of after
/// the TTL. `404` when no RTP source listens on that port yet: the producer simply keeps
/// heartbeating, and its first beat after the source is created registers it.
pub(crate) async fn report_control(State(state): State<AppState>, Json(req): Json<ControlReportRequest>) -> OpResult {
    let node_name = {
        let sources = state.sources.lock_recover();
        sources.list().iter().find(|e| matches!(&e.config, SourceConfig::Rtp(r) if r.port == req.rtp_port)).map(|e| e.node_name())
    };
    let Some(node_name) = node_name else {
        return Err(ApiError::not_found(format!("no RTP source is configured on port {}", req.rtp_port)));
    };
    let Some(url) = req.url else {
        state.controls.withdraw(&node_name);
        return ok(format!("control endpoint withdrawn for '{node_name}'"));
    };
    let control = crate::sources::control::SourceControl { url, features: req.features, protocol: req.protocol };
    control.validate().map_err(ApiError::bad_request)?;
    let announced = state.controls.get(&node_name);
    state.controls.announce(&node_name, control);
    // A heartbeat that changes nothing is not news; a new or moved endpoint is, so the
    // listing's watchers (the web UI) refresh rather than wait for their next poll.
    if announced != state.controls.get(&node_name) {
        let _ = state.changes.send(());
    }
    ok(format!("control endpoint recorded for '{node_name}'"))
}

/// Attach each source's announced control surface to its view — runtime state the pure
/// [`source_view`] cannot see, in the same way [`with_spotify_status`] adds the device
/// status.
pub(crate) fn with_controls(state: &AppState, mut views: Vec<SourceView>) -> Vec<SourceView> {
    for v in &mut views {
        v.control = state.controls.get(&v.node_name);
    }
    views
}

#[derive(Deserialize)]
pub(crate) struct ControlReportRequest {
    /// The UDP port this producer's RTP sender transmits to — which source it is.
    pub(crate) rtp_port: u16,
    /// Base URL of the producer's control API; `null` (or absent) withdraws it.
    #[serde(default)]
    pub(crate) url: Option<String>,
    #[serde(default)]
    pub(crate) features: Vec<String>,
    #[serde(default = "default_control_protocol")]
    pub(crate) protocol: u32,
}

fn default_control_protocol() -> u32 {
    1
}
