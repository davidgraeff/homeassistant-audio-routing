use super::*;

pub(crate) async fn health() -> &'static str {
    "ok"
}

/// The built web UI, read into memory at startup and served from RAM (see the
/// `fallback` note in `router`). A UI redeploy restarts the daemon, which reloads.
pub(crate) struct StaticAssets {
    /// URL-path (no leading `/`, e.g. `assets/index-abc.js`) → file.
    pub(crate) files: HashMap<String, StaticFile>,
    /// `index.html`, cloned out for the SPA fallback (client-side routes → index).
    pub(crate) index: Option<StaticFile>,
}

#[derive(Clone)]
pub(crate) struct StaticFile {
    pub(crate) body: Bytes, // ref-counted: cheap per-request clone, no re-read
    pub(crate) content_type: &'static str,
}

impl StaticAssets {
    pub(crate) fn load(dir: &FsPath) -> Self {
        let mut files = HashMap::new();
        Self::walk(dir, dir, &mut files);
        let index = files.get("index.html").cloned();
        tracing::info!("web UI: loaded {} file(s) into memory from {}", files.len(), dir.display());
        if index.is_none() {
            tracing::warn!("web UI: no index.html under {} — the UI won't serve", dir.display());
        }
        Self { files, index }
    }

    fn walk(root: &FsPath, cur: &FsPath, files: &mut HashMap<String, StaticFile>) {
        let Ok(rd) = std::fs::read_dir(cur) else { return };
        for entry in rd.flatten() {
            let p = entry.path();
            if p.is_dir() {
                Self::walk(root, &p, files);
            } else if let (Ok(bytes), Ok(rel)) = (std::fs::read(&p), p.strip_prefix(root)) {
                let key = rel.to_string_lossy().replace('\\', "/");
                let content_type = content_type_for(&key);
                files.insert(key, StaticFile { body: Bytes::from(bytes), content_type });
            }
        }
    }
}

/// Minimal extension→MIME map for the assets Vite emits (no `mime_guess` dep).
pub(crate) fn content_type_for(path: &str) -> &'static str {
    match path.rsplit('.').next().unwrap_or("") {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" | "map" => "application/json; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "ico" => "image/x-icon",
        "woff2" => "font/woff2",
        "woff" => "font/woff",
        "ttf" => "font/ttf",
        "webmanifest" => "application/manifest+json",
        "txt" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

pub(crate) fn static_response(f: &StaticFile, key: &str) -> Response {
    // Content-hashed assets are immutable (cache a year); the entrypoint HTML must
    // never be cached, or a stale index pins old asset URLs after a redeploy.
    let cache = if key.starts_with("assets/") { "public, max-age=31536000, immutable" } else { "no-cache" };
    Response::builder()
        .header(header::CONTENT_TYPE, f.content_type)
        .header(header::CACHE_CONTROL, cache)
        .body(Body::from(f.body.clone()))
        .expect("static response builds")
}

/// Serve the SPA from RAM. Exact file if present; a missing `/assets/*` is a real
/// 404; any other unknown path falls back to `index.html` (client-side routing).
pub(crate) async fn static_fallback(Extension(assets): Extension<Arc<StaticAssets>>, uri: Uri) -> Response {
    let raw = uri.path().trim_start_matches('/');
    let key = if raw.is_empty() { "index.html" } else { raw };
    if let Some(f) = assets.files.get(key) {
        return static_response(f, key);
    }
    if key.starts_with("assets/") {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    }
    match &assets.index {
        Some(f) => static_response(f, "index.html"),
        None => (StatusCode::NOT_FOUND, "web UI not built").into_response(),
    }
}

#[derive(Serialize)]
pub(crate) struct NodesResponse {
    pub(crate) nodes: Vec<crate::pw::thread::NodeInfo>,
    pub(crate) ports: Vec<crate::pw::thread::PortInfo>,
}

pub(crate) async fn list_nodes(State(pw_state): State<SharedState>) -> Json<NodesResponse> {
    let state = pw_state.lock_recover();
    Json(NodesResponse { nodes: state.nodes.values().cloned().collect(), ports: state.ports.values().cloned().collect() })
}

/// Links two ports by their exact PipeWire port names (e.g.
/// `"airplay-in:output_FL"`), one call per channel — the
/// caller (eventually the routing UI / HA integration, for now this
/// project's own test scripts) is responsible for pairing FL/FR etc.
///
/// Created natively via `Core::create_object` on the PipeWire thread (see
/// pw/thread.rs) — the port names are resolved to object ids against the live
/// registry here, then handed over as a `CreateLinks` command.
#[derive(Deserialize)]
pub(crate) struct CreateLinkRequest {
    pub(crate) from_port: String,
    pub(crate) to_port: String,
}

#[derive(Serialize)]
pub(crate) struct CreateLinkResponse {
    pub(crate) ok: bool,
    pub(crate) message: String,
}

/// Resolves a full `"node.name:port.name"` string to its `(node_id, port_id)`
/// in the live registry, or `None` if either isn't present. Splits on the last
/// `:` so a node name containing `:` still resolves (port names never do).
pub(crate) fn resolve_port(pw: &SharedState, full_name: &str) -> Option<(u32, u32)> {
    let (node_name, port_name) = full_name.rsplit_once(':')?;
    let state = pw.lock_recover();
    let node_id = state.nodes.values().find(|n| n.node_name == node_name).map(|n| n.node_id)?;
    let port_id = state.ports.values().find(|p| p.node_id == node_id && p.port_name == port_name).map(|p| p.port_id)?;
    Some((node_id, port_id))
}

pub(crate) async fn create_link(State(app): State<AppState>, Json(req): Json<CreateLinkRequest>) -> (StatusCode, Json<CreateLinkResponse>) {
    let Some((out_node, out_port)) = resolve_port(&app.pw, &req.from_port) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(CreateLinkResponse { ok: false, message: format!("unknown output port: {}", req.from_port) }),
        );
    };
    let Some((in_node, in_port)) = resolve_port(&app.pw, &req.to_port) else {
        return (StatusCode::BAD_REQUEST, Json(CreateLinkResponse { ok: false, message: format!("unknown input port: {}", req.to_port) }));
    };

    let (reply_tx, reply_rx) = oneshot::channel();
    let cmd = PwCommand::CreateLinks { specs: vec![LinkSpec { out_node, out_port, in_node, in_port }], reply: reply_tx };
    if app.pw_cmd.send(cmd).is_err() {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(CreateLinkResponse { ok: false, message: "pipewire thread unavailable".to_string() }),
        );
    }
    match reply_rx.await {
        Ok(Ok(message)) => (StatusCode::OK, Json(CreateLinkResponse { ok: true, message })),
        Ok(Err(message)) => (StatusCode::BAD_REQUEST, Json(CreateLinkResponse { ok: false, message })),
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(CreateLinkResponse { ok: false, message: "pipewire thread dropped the request".to_string() }),
        ),
    }
}
