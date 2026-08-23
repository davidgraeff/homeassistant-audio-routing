//! Drive a real sendspin speaker directly, to test device-side behaviour without a deploy.
//!
//! Built to chase docs/sendspin-open-items.md item 4 — "the device receives audio and
//! renders nothing" — and kept because the class of question recurs and the alternative is
//! a ~15-minute `deploy-dev.sh` per hypothesis.
//!
//! ## Why this works without any audio
//!
//! The oracle is device-side, in the speaker's own log (see
//! docs/live-instance-debugging.md §"Device-side logs from the ESPHome speakers"): a device
//! that is really going to render logs
//!
//! ```text
//! sendspin.client: Stream Started
//! sendspin.sync_task: Processed new codec header: opus, 48000 Hz, 2 ch, 16-bit
//! ring_buffer: Created ring buffer with size 19200
//! speaker_mixer: Starting
//! ```
//!
//! and one that will stay silent logs only `Stream Started`, then reports `PLAYING` anyway.
//! All of that is decided at `stream/start`, **before the first audio frame** — so a
//! scenario here is a dial plus one or two control frames. No encoder, no PipeWire, no
//! anchor, and a fresh build in seconds.
//!
//! ## Usage
//!
//! ```text
//! # take the speaker away from the daemon first, or the two compete for it:
//! curl -XDELETE http://<ha>:8099/api/outputs/sendspin-dev-<slug>
//!
//! cargo run --example sendspin_handover -- ws://192.168.178.82:8928/sendspin plain
//! cargo run --example sendspin_handover -- ws://192.168.178.82:8928/sendspin reissue
//!
//! # afterwards, give it back:
//! curl -XPOST http://<ha>:8099/api/outputs/sendspin-dev-<slug>/adopt
//! ```
//!
//! Scenarios (what phase 2 does after its `stream/start`):
//!
//! | scenario | phase 2 | measured |
//! |---|---|---|
//! | `plain` | one `stream/start` | ~12/25 rendered |
//! | `end-first` | `stream/end`, then `stream/start` | 1/4 |
//! | `delay:<ms>` | wait, then one `stream/start` | 8/10, unreliable at any delay |
//! | `reissue` | `stream/start`, then the same frame 1 s later | **15/15** — what shipped |
//!
//! Flags: `--no-idle-hop` skips phase 1 (the control — the fault still occurs without it,
//! which is what disproved the original "idle-sender handover" diagnosis), and
//! `--wait-state` waits for the initial `client/state` before streaming, the way the
//! daemon's own `ready` gate does.
//!
//! ## Three things that will mislead you
//!
//! 1. **Self-prime.** The fault needs a *prior*, successfully-configured stream of the same
//!    format, so phase 0 below always streams and ends one first. Without it the first run
//!    after a quiet period passes and the next fails, which reads as flakiness.
//! 2. **Scope the verdict to phase 2.** Phase 0 always logs `Processed new codec header`;
//!    grepping the whole log therefore reports the opposite of the truth. Read only the
//!    window after the `sync-grp-handover` handshake.
//! 3. **Batch it.** The base failure rate is roughly half, so anything under about four
//!    runs per arm is noise — several n=2/n=3 results reversed here at n=4+.

use sendspin::protocol::messages::{ConnectionReason, Message, StreamPlayerConfig};
use sendspin::server::ServerRole;
use sendspin::{Clock, DefaultClock, ServerConnection};
use std::sync::Arc;
use std::time::Duration;

/// The wire format the daemon uses for these speakers, so the device sees exactly the
/// config it sees in production (the codec header is the thing under test).
fn stream_config() -> StreamPlayerConfig {
    StreamPlayerConfig { codec: "opus".to_string(), sample_rate: 48_000, channels: 2, bit_depth: 16, codec_header: None }
}

/// Service the socket for the rest of the connection's life. The device sends `client/time`
/// continuously and an unread socket would back up and stall the handshake we are testing.
fn drain(mut conn: ServerConnection) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move { while conn.recv_message().await.is_some() {} })
}

/// Read until the device's initial `client/state`, which is what the daemon waits for before
/// it streams (the spec's MUST, and `READY_GRACE` in outputs/sendspin/server.rs).
async fn await_client_state(conn: &mut ServerConnection) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while let Ok(Some(message)) = tokio::time::timeout_at(deadline, conn.recv_message()).await {
        if matches!(message, Message::ClientState(_)) {
            println!("   phase 2: got the initial client/state");
            return;
        }
    }
    println!("   phase 2: no client/state within 3s (the daemon would stream anyway after its grace)");
}

async fn dial(url: &str, server_id: &str, reason: ConnectionReason) -> ServerConnection {
    let clock: Arc<dyn Clock> = Arc::new(DefaultClock::default());
    let role = ServerRole::new(server_id, server_id).clock(clock).connection_reason(reason);
    match role.dial(url).await {
        Ok(conn) => conn,
        Err(e) => {
            eprintln!("dial to {url} failed: {e}");
            eprintln!("is the daemon still holding this device? un-adopt it first (see the module docs)");
            std::process::exit(1);
        }
    }
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let positional: Vec<&String> = args.iter().filter(|a| !a.starts_with("--")).collect();
    let Some(url) = positional.first() else {
        eprintln!("usage: sendspin_handover <ws://host:8928/sendspin> [scenario] [--no-idle-hop] [--wait-state]");
        eprintln!("scenarios: plain | reissue | end-first | delay:<ms>");
        std::process::exit(2);
    };
    let scenario = positional.get(1).map(|s| s.as_str()).unwrap_or("plain").to_string();
    let idle_hop = !args.iter().any(|a| a == "--no-idle-hop");
    let wait_state = args.iter().any(|a| a == "--wait-state");

    // Phase 0 — PRIME. A group server that really streamed and then ended cleanly. See
    // "self-prime" in the module docs: without this the fault does not reproduce.
    println!("phase 0 (prime): playback, stream/start, stream/end");
    let c0 = dial(url, "sync-grp-prime", ConnectionReason::Playback).await;
    let s0 = c0.sender();
    let d0 = drain(c0);
    let _ = s0.queue_stream_start(stream_config()).written().await;
    tokio::time::sleep(Duration::from_secs(3)).await;
    let _ = s0.queue_stream_end().written().await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    drop(s0);
    d0.abort();
    let _ = d0.await;
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Phase 1 — the daemon's idle sender: `Discovery`, connected, never streaming.
    if idle_hop {
        println!("phase 1 (idle sender): discovery, no stream");
        let c1 = dial(url, "idle-dev-handover", ConnectionReason::Discovery).await;
        let s1 = c1.sender();
        let d1 = drain(c1);
        tokio::time::sleep(Duration::from_secs(3)).await;
        drop(s1);
        d1.abort();
        let _ = d1.await;
        tokio::time::sleep(Duration::from_millis(150)).await;
    } else {
        println!("phase 1: skipped (--no-idle-hop)");
    }

    // Phase 2 — the group server taking the device back. This is the phase under test, and
    // the only part of the log the verdict may be read from.
    println!("phase 2: playback as 'sync-grp-handover', scenario '{scenario}'");
    let mut c2 = dial(url, "sync-grp-handover", ConnectionReason::Playback).await;
    let s2 = c2.sender();
    if wait_state {
        await_client_state(&mut c2).await;
    }
    let d2 = drain(c2);

    match scenario.as_str() {
        "end-first" => {
            let _ = s2.queue_stream_end().written().await;
            tokio::time::sleep(Duration::from_millis(200)).await;
            let _ = s2.queue_stream_start(stream_config()).written().await;
        }
        "reissue" => {
            let _ = s2.queue_stream_start(stream_config()).written().await;
            tokio::time::sleep(Duration::from_secs(1)).await;
            println!("   phase 2: re-issuing the identical stream/start");
            let _ = s2.queue_stream_start(stream_config()).written().await;
        }
        s if s.starts_with("delay:") => {
            let ms: u64 = s.trim_start_matches("delay:").parse().unwrap_or(250);
            tokio::time::sleep(Duration::from_millis(ms)).await;
            let _ = s2.queue_stream_start(stream_config()).written().await;
        }
        _ => {
            let _ = s2.queue_stream_start(stream_config()).written().await;
        }
    }

    tokio::time::sleep(Duration::from_secs(8)).await;
    let _ = s2.queue_stream_end().written().await;
    d2.abort();
    println!();
    println!("done. In the device log, AFTER the 'server_id=sync-grp-handover' handshake:");
    println!("  'Processed new codec header' + 'ring_buffer: Created'  => it rendered");
    println!("  only 'Stream Started'                                  => silent (the fault)");
}
