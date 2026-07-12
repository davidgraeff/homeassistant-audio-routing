//! REST API: health check, live PipeWire registry state, and manual link
//! creation (PLAN.md Section 8's eventual routing UI talks to this same
//! API — Section 9 decision #2). Grows into the custom HA integration's
//! backing API too.

use crate::pw_config_gen::RAOP_NODE_PREFIX;
use crate::pw_thread::{ChangeNotifier, SharedState};
use crate::config::SENDSPIN_NODE_PREFIX;
use crate::routing;
use axum::{
    extract::{FromRef, Path, State},
    http::StatusCode,
    response::Html,
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};

/// Shared axum state: both the live PipeWire registry snapshot and the
/// routing UI's change-notification channel (Section 8, routing.rs).
/// Existing handlers below extract just `SharedState` via `FromRef` —
/// they don't need to know this type grew a second field.
#[derive(Clone)]
pub struct AppState {
    pub pw: SharedState,
    pub changes: ChangeNotifier,
}

impl FromRef<AppState> for SharedState {
    fn from_ref(state: &AppState) -> SharedState {
        state.pw.clone()
    }
}

impl FromRef<AppState> for ChangeNotifier {
    fn from_ref(state: &AppState) -> ChangeNotifier {
        state.changes.clone()
    }
}

const ROUTING_UI_HTML: &str = include_str!("../static/routing_ui.html");

pub fn router(pw_state: SharedState, changes: ChangeNotifier) -> Router {
    let state = AppState { pw: pw_state, changes };
    Router::new()
        .route("/", get(routing_ui))
        .route("/health", get(health))
        .route("/api/nodes", get(list_nodes))
        .route("/api/links", post(create_link))
        .route("/api/media_players", get(list_media_players))
        .route("/api/media_players/:node_id/volume", get(get_volume).post(set_volume))
        .route("/api/media_players/:node_id/announce", post(announce))
        .route("/api/routing", get(routing::get_routing))
        .route("/api/routing/link", post(routing::link))
        .route("/api/routing/unlink", post(routing::unlink))
        .route("/api/routing/ws", get(routing::routing_ws))
        .with_state(state)
}

async fn routing_ui() -> Html<&'static str> {
    Html(ROUTING_UI_HTML)
}

async fn health() -> &'static str {
    "ok"
}

#[derive(Serialize)]
struct NodesResponse {
    nodes: Vec<crate::pw_thread::NodeInfo>,
    ports: Vec<crate::pw_thread::PortInfo>,
}

async fn list_nodes(State(pw_state): State<SharedState>) -> Json<NodesResponse> {
    let state = pw_state.lock().unwrap();
    Json(NodesResponse {
        nodes: state.nodes.values().cloned().collect(),
        ports: state.ports.values().cloned().collect(),
    })
}

/// Links two ports by their exact PipeWire port names (e.g.
/// `"alsa_playback.shairport-sync:output_FL"`), one call per channel — the
/// caller (eventually the routing UI / HA integration, for now this
/// project's own test scripts) is responsible for pairing FL/FR etc.
///
/// Implemented via a `pw-link` subprocess rather than pipewire-rs's native
/// `Core::create_object` — a deliberate scope decision, not an oversight;
/// see pw_thread.rs's module doc for why.
#[derive(Deserialize)]
struct CreateLinkRequest {
    from_port: String,
    to_port: String,
}

#[derive(Serialize)]
struct CreateLinkResponse {
    ok: bool,
    message: String,
}

async fn create_link(Json(req): Json<CreateLinkRequest>) -> (StatusCode, Json<CreateLinkResponse>) {
    let output = tokio::process::Command::new("pw-link")
        .arg(&req.from_port)
        .arg(&req.to_port)
        .output()
        .await;

    match output {
        Ok(output) if output.status.success() => (
            StatusCode::OK,
            Json(CreateLinkResponse {
                ok: true,
                message: format!("linked {} -> {}", req.from_port, req.to_port),
            }),
        ),
        // pw-link fails with "File exists" if the link is already present.
        // Treat that as success, not an error — the caller asked for a
        // link to exist between these two ports, and it does. Without
        // this, a caller retrying a create-link call (e.g. while racing a
        // short-lived source node, as real AirPlay sources are — see
        // spikes/shairport-sync-source.md) sees a spurious failure on the
        // second attempt even though the first one already succeeded.
        Ok(output) if String::from_utf8_lossy(&output.stderr).contains("File exists") => (
            StatusCode::OK,
            Json(CreateLinkResponse {
                ok: true,
                message: format!("{} -> {} already linked", req.from_port, req.to_port),
            }),
        ),
        Ok(output) => (
            StatusCode::BAD_REQUEST,
            Json(CreateLinkResponse {
                ok: false,
                message: String::from_utf8_lossy(&output.stderr).trim().to_string(),
            }),
        ),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(CreateLinkResponse {
                ok: false,
                message: format!("failed to run pw-link: {e}"),
            }),
        ),
    }
}

/// One output the custom HA integration (Section 6/9) turns into a
/// `media_player` entity: derived from the live registry, not from the
/// add-on's static config, so it only lists outputs PipeWire actually
/// created (a misconfigured RAOP device that never loaded still won't
/// appear — matches this project's "trust the observed state" approach
/// throughout).
#[derive(Serialize)]
struct MediaPlayerInfo {
    node_id: u32,
    node_name: String,
    /// "playing" if any link currently feeds this node, "idle" otherwise
    /// (pw_thread.rs's `node_has_incoming_link`) — there is no richer
    /// PipeWire-native concept of "paused" for a passive routing sink;
    /// see PLAN.md Section 6 for why this entity's state model is
    /// necessarily simpler than a real playback device's.
    state: &'static str,
    /// Included inline (one `wpctl` call per output, run concurrently)
    /// rather than requiring the HA integration to make a second request
    /// per output on every poll — `None` if `wpctl` failed for that node.
    volume: Option<f32>,
}

async fn list_media_players(State(pw_state): State<SharedState>) -> Json<Vec<MediaPlayerInfo>> {
    // Snapshot and release the lock before the async wpctl calls below —
    // std::sync::MutexGuard isn't safe to hold across an .await point.
    let candidates: Vec<(u32, String, bool)> = {
        let state = pw_state.lock().unwrap();
        state
            .nodes
            .values()
            .filter(|n| n.node_name.starts_with(RAOP_NODE_PREFIX) || n.node_name.starts_with(SENDSPIN_NODE_PREFIX))
            .map(|n| (n.node_id, n.node_name.clone(), state.node_has_incoming_link(n.node_id)))
            .collect()
    };

    // Sequential, not concurrent: candidate counts are small (a handful of
    // rooms), and this avoids pulling in a join_all dependency for it.
    let mut players = Vec::with_capacity(candidates.len());
    for (node_id, node_name, playing) in candidates {
        let volume = fetch_wpctl_volume(node_id).await;
        players.push(MediaPlayerInfo {
            node_id,
            node_name,
            state: if playing { "playing" } else { "idle" },
            volume,
        });
    }

    Json(players)
}

async fn fetch_wpctl_volume(node_id: u32) -> Option<f32> {
    let output = tokio::process::Command::new("wpctl")
        .arg("get-volume")
        .arg(node_id.to_string())
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }
    parse_wpctl_volume(&String::from_utf8_lossy(&output.stdout))
}

#[derive(Serialize)]
struct VolumeResponse {
    volume: Option<f32>,
    message: Option<String>,
}

/// `wpctl get-volume`'s only output format, confirmed empirically:
/// `Volume: 0.50` (optionally with a trailing `[MUTED]`).
fn parse_wpctl_volume(stdout: &str) -> Option<f32> {
    stdout.trim().strip_prefix("Volume:")?.split_whitespace().next()?.parse().ok()
}

async fn get_volume(Path(node_id): Path<u32>) -> (StatusCode, Json<VolumeResponse>) {
    let output = tokio::process::Command::new("wpctl")
        .arg("get-volume")
        .arg(node_id.to_string())
        .output()
        .await;
    match output {
        Ok(output) if output.status.success() => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            match parse_wpctl_volume(&stdout) {
                Some(volume) => (StatusCode::OK, Json(VolumeResponse { volume: Some(volume), message: None })),
                None => (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(VolumeResponse {
                        volume: None,
                        message: Some(format!("could not parse wpctl output: {stdout}")),
                    }),
                ),
            }
        }
        Ok(output) => (
            StatusCode::BAD_REQUEST,
            Json(VolumeResponse {
                volume: None,
                message: Some(String::from_utf8_lossy(&output.stderr).trim().to_string()),
            }),
        ),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(VolumeResponse {
                volume: None,
                message: Some(format!("failed to run wpctl: {e}")),
            }),
        ),
    }
}

#[derive(Deserialize)]
struct SetVolumeRequest {
    /// 0.0-1.0, matching `wpctl`'s own scale (1.0 = 100%) and HA's
    /// `MediaPlayerEntity.volume_level`.
    volume: f32,
}

async fn set_volume(Path(node_id): Path<u32>, Json(req): Json<SetVolumeRequest>) -> (StatusCode, Json<VolumeResponse>) {
    let output = tokio::process::Command::new("wpctl")
        .arg("set-volume")
        .arg(node_id.to_string())
        .arg(req.volume.to_string())
        .output()
        .await;
    match output {
        Ok(output) if output.status.success() => (StatusCode::OK, Json(VolumeResponse { volume: Some(req.volume), message: None })),
        Ok(output) => (
            StatusCode::BAD_REQUEST,
            Json(VolumeResponse {
                volume: None,
                message: Some(String::from_utf8_lossy(&output.stderr).trim().to_string()),
            }),
        ),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(VolumeResponse {
                volume: None,
                message: Some(format!("failed to run wpctl: {e}")),
            }),
        ),
    }
}

/// TTS/voice-response ducked announce stream (PLAN.md Section 5.6). Ducks
/// every source currently linked into this sink by setting their **node**
/// volume (not link volume — PipeWire Links have no Props/gain stage at
/// all, only a Format param; disproven empirically in
/// spikes/05-tts-ducking-mechanism.md), plays the announce clip into the
/// sink via `pw-cat --target`, then restores every ducked source to its
/// original volume, always — even if playback fails — so an announce
/// error can never leave music stuck at duck volume.
///
/// The announce audio itself comes from exactly one of two mutually
/// exclusive sources — **additive**, not a v1→v2 migration (Phase 3.5):
/// - `url` (**v1**, unchanged): a rendered TTS clip fetched over HTTP
///   (LAN-local, e.g. HA's own `tts` integration), decoded to WAV via
///   ffmpeg (pw-cat/libsndfile can't decode compressed formats like mp3
///   itself).
/// - `wyoming` (**v2**, new): synthesized directly against a local
///   Wyoming TTS server (e.g. Piper, see wyoming.rs), skipping the
///   render-to-file-then-HTTP-fetch round trip for lower first-audible-
///   word latency. Whichever caller wants this picks it per call — HA's
///   `tts.speak` keeps using `url` exactly as before; nothing is forced
///   to switch.
#[derive(Deserialize)]
struct AnnounceRequest {
    url: Option<String>,
    wyoming: Option<WyomingAnnounceRequest>,
    /// 0.0-1.0, the level surviving sources are ducked to while the
    /// announce plays. Defaults to a level that keeps music audibly
    /// present but subordinate, matching Section 5.6's "ducked, not
    /// silenced" design.
    #[serde(default = "default_duck_volume")]
    duck_volume: f32,
}

#[derive(Deserialize)]
struct WyomingAnnounceRequest {
    host: String,
    #[serde(default = "default_wyoming_port")]
    port: u16,
    text: String,
    /// Optional Piper multi-speaker voice name; omit for the server's
    /// default voice.
    voice: Option<String>,
}

fn default_duck_volume() -> f32 {
    0.25
}

fn default_wyoming_port() -> u16 {
    10200
}

#[derive(Serialize)]
struct AnnounceResponse {
    ok: bool,
    message: String,
}

async fn announce(
    State(pw_state): State<SharedState>,
    Path(node_id): Path<u32>,
    Json(req): Json<AnnounceRequest>,
) -> (StatusCode, Json<AnnounceResponse>) {
    let (target_name, source_node_ids): (String, Vec<u32>) = {
        let state = pw_state.lock().unwrap();
        match state.nodes.get(&node_id) {
            Some(target) => {
                // A stereo source contributes two Link objects (FL + FR)
                // into the sink, both sharing the same output_node — dedupe
                // by node id here, or a node gets ducked/restored twice:
                // the second "duck" fetches the volume the first duck call
                // already set (mistaking it for the original), and the
                // second "restore" then clobbers the correct restore with
                // that wrong cached value, leaving the source stuck ducked.
                let mut sources: Vec<u32> = state
                    .links
                    .values()
                    .filter(|l| l.input_node == node_id)
                    .map(|l| l.output_node)
                    .collect();
                sources.sort_unstable();
                sources.dedup();
                (target.node_name.clone(), sources)
            }
            None => {
                return (
                    StatusCode::NOT_FOUND,
                    Json(AnnounceResponse {
                        ok: false,
                        message: format!("no such node: {node_id}"),
                    }),
                )
            }
        }
    };

    let (url, wyoming_req) = match (&req.url, &req.wyoming) {
        (Some(url), None) => (Some(url), None),
        (None, Some(w)) => (None, Some(w)),
        (Some(_), Some(_)) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(AnnounceResponse {
                    ok: false,
                    message: "exactly one of `url` or `wyoming` must be given, not both".to_string(),
                }),
            )
        }
        (None, None) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(AnnounceResponse {
                    ok: false,
                    message: "one of `url` or `wyoming` is required".to_string(),
                }),
            )
        }
    };

    let fetch_path = std::env::temp_dir().join(format!("announce-{node_id}-fetch"));
    let wav_path = std::env::temp_dir().join(format!("announce-{node_id}.wav"));
    // Always clear stale paths from a prior announce before writing new
    // ones — see tests/test_addon_phase3_multi_output.sh's comments on
    // cliraop's mkfifo gotcha for why a leftover file at a reused path is
    // a real, previously-hit failure mode, not just defensive paranoia.
    let _ = tokio::fs::remove_file(&fetch_path).await;
    let _ = tokio::fs::remove_file(&wav_path).await;

    if let Some(url) = url {
        if let Err(e) = fetch_to_file(url, &fetch_path).await {
            return (
                StatusCode::BAD_GATEWAY,
                Json(AnnounceResponse {
                    ok: false,
                    message: format!("failed to fetch announce audio: {e}"),
                }),
            );
        }

        let ffmpeg_result = tokio::process::Command::new("ffmpeg")
            .arg("-y")
            .arg("-i")
            .arg(&fetch_path)
            .arg(&wav_path)
            .output()
            .await;
        let _ = tokio::fs::remove_file(&fetch_path).await;
        if let Err(e) = check_command_result(ffmpeg_result, "ffmpeg") {
            return (StatusCode::BAD_REQUEST, Json(AnnounceResponse { ok: false, message: e }));
        }
    } else if let Some(w) = wyoming_req {
        // No ffmpeg decode step needed here — we build the WAV ourselves
        // from the exact PCM format Wyoming reports (wyoming.rs).
        match crate::wyoming::synthesize_to_wav(&w.host, w.port, &w.text, w.voice.as_deref()).await {
            Ok(wav_bytes) => {
                if let Err(e) = tokio::fs::write(&wav_path, &wav_bytes).await {
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(AnnounceResponse {
                            ok: false,
                            message: format!("failed to write synthesized audio: {e}"),
                        }),
                    );
                }
            }
            Err(e) => {
                return (
                    StatusCode::BAD_GATEWAY,
                    Json(AnnounceResponse {
                        ok: false,
                        message: format!("wyoming synthesis failed: {e}"),
                    }),
                )
            }
        }
    }

    let mut original_volumes = Vec::with_capacity(source_node_ids.len());
    for &src_id in &source_node_ids {
        if let Some(vol) = fetch_wpctl_volume(src_id).await {
            original_volumes.push((src_id, vol));
            let _ = set_wpctl_volume(src_id, req.duck_volume).await;
        }
    }

    let play_result = tokio::process::Command::new("pw-cat")
        .arg("--target")
        .arg(&target_name)
        .arg("--playback")
        .arg(&wav_path)
        .output()
        .await;

    // Restore unconditionally, before inspecting play_result — a failed
    // announce must never leave music stuck at duck volume.
    for (src_id, vol) in &original_volumes {
        let _ = set_wpctl_volume(*src_id, *vol).await;
    }
    let _ = tokio::fs::remove_file(&wav_path).await;

    match check_command_result(play_result, "pw-cat") {
        Ok(()) => (
            StatusCode::OK,
            Json(AnnounceResponse {
                ok: true,
                message: format!("announced on {target_name}, ducked {} source(s)", original_volumes.len()),
            }),
        ),
        Err(e) => (StatusCode::BAD_REQUEST, Json(AnnounceResponse { ok: false, message: e })),
    }
}

fn check_command_result(result: std::io::Result<std::process::Output>, program: &str) -> Result<(), String> {
    match result {
        Ok(output) if output.status.success() => Ok(()),
        Ok(output) => Err(format!("{program} failed: {}", String::from_utf8_lossy(&output.stderr).trim())),
        Err(e) => Err(format!("failed to run {program}: {e}")),
    }
}

async fn fetch_to_file(url: &str, path: &std::path::Path) -> anyhow::Result<()> {
    let response = reqwest::get(url).await?.error_for_status()?;
    let bytes = response.bytes().await?;
    tokio::fs::write(path, &bytes).await?;
    Ok(())
}

async fn set_wpctl_volume(node_id: u32, volume: f32) -> bool {
    tokio::process::Command::new("wpctl")
        .arg("set-volume")
        .arg(node_id.to_string())
        .arg(volume.to_string())
        .output()
        .await
        .map(|o| o.status.success())
        .unwrap_or(false)
}
