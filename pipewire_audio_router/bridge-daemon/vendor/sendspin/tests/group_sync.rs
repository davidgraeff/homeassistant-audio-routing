// ABOUTME: Integration tests for Group — proves the actual multi-room
// ABOUTME: correctness property: every member receives the identical audio
// ABOUTME: bytes tagged with the identical timestamp for a given push.

mod common;

use common::{bind_test_listener, connect_peer, pcm_config};
use futures_util::StreamExt;
use sendspin::protocol::client::AudioChunk;
use sendspin::protocol::messages::{Message, PlayerCommand, PlayerCommandType};
use sendspin::server::Group;
use std::time::Duration;
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::Message as WsMessage;

#[tokio::test]
async fn two_members_receive_identical_timestamped_audio() {
    let (listener, url) = bind_test_listener().await;

    let peer_a = tokio::spawn({
        let url = url.clone();
        async move { connect_peer(&url, "member-a").await }
    });
    let peer_b = tokio::spawn({
        let url = url.clone();
        async move { connect_peer(&url, "member-b").await }
    });

    let (conn_a, _) = timeout(Duration::from_secs(5), listener.accept())
        .await
        .unwrap()
        .unwrap();
    let (conn_b, _) = timeout(Duration::from_secs(5), listener.accept())
        .await
        .unwrap()
        .unwrap();

    let group = Group::new(std::sync::Arc::new(sendspin::DefaultClock::default()));
    group
        .add_member(conn_a.client_id().to_string(), conn_a.sender())
        .await
        .unwrap();
    group
        .add_member(conn_b.client_id().to_string(), conn_b.sender())
        .await
        .unwrap();
    assert_eq!(group.len(), 2);

    group.start_stream(pcm_config()).await;
    let sent_timestamp = group.push_audio(&[1, 2, 3, 4, 5, 6, 7, 8]);

    let mut read_a = peer_a.await.unwrap();
    let mut read_b = peer_b.await.unwrap();

    // stream/start reaches both.
    for read in [&mut read_a, &mut read_b] {
        let msg = read.next().await.expect("no stream/start").unwrap();
        assert!(matches!(msg, WsMessage::Text(_)));
    }

    // Both members get the *same* timestamp and the *same* bytes for this push —
    // the actual multi-room correctness property.
    let frame_a = match read_a.next().await.expect("no audio frame A").unwrap() {
        WsMessage::Binary(b) => b,
        other => panic!("expected binary, got {other:?}"),
    };
    let frame_b = match read_b.next().await.expect("no audio frame B").unwrap() {
        WsMessage::Binary(b) => b,
        other => panic!("expected binary, got {other:?}"),
    };
    let chunk_a = AudioChunk::from_bytes(&frame_a).unwrap();
    let chunk_b = AudioChunk::from_bytes(&frame_b).unwrap();
    assert_eq!(chunk_a.timestamp, sent_timestamp);
    assert_eq!(chunk_b.timestamp, sent_timestamp);
    assert_eq!(&*chunk_a.data, &*chunk_b.data);
    assert_eq!(&*chunk_a.data, &[1, 2, 3, 4, 5, 6, 7, 8][..]);
}

/// The scope decision for late joiners (see src/server/mod.rs): no
/// historical replay — a member that joins mid-stream gets stream/start
/// with the current config and then whatever the *next* push_audio() call
/// sends, in sync with everyone already in the group, but nothing from
/// before it joined. This proves that's actually what happens, not just
/// what the doc comment claims.
#[tokio::test]
async fn a_late_joiner_gets_current_stream_start_and_only_subsequent_audio() {
    let (listener, url) = bind_test_listener().await;

    let peer_a = tokio::spawn({
        let url = url.clone();
        async move { connect_peer(&url, "early-member").await }
    });
    let (conn_a, _) = timeout(Duration::from_secs(5), listener.accept())
        .await
        .unwrap()
        .unwrap();
    let group = Group::new(std::sync::Arc::new(sendspin::DefaultClock::default()));
    group
        .add_member(conn_a.client_id().to_string(), conn_a.sender())
        .await
        .unwrap();

    group.start_stream(pcm_config()).await;
    // Sent before the late joiner exists — it must never see this.
    group.push_audio(&[0xAA; 8]);

    let mut read_a = peer_a.await.unwrap();
    assert!(matches!(
        read_a
            .next()
            .await
            .expect("no stream/start for early member")
            .unwrap(),
        WsMessage::Text(_)
    ));
    assert!(matches!(
        read_a
            .next()
            .await
            .expect("no first chunk for early member")
            .unwrap(),
        WsMessage::Binary(_)
    ));

    // Now the late joiner connects, after a stream is already live.
    let peer_b = tokio::spawn({
        let url = url.clone();
        async move { connect_peer(&url, "late-member").await }
    });
    let (conn_b, _) = timeout(Duration::from_secs(5), listener.accept())
        .await
        .unwrap()
        .unwrap();
    group
        .add_member(conn_b.client_id().to_string(), conn_b.sender())
        .await
        .unwrap();
    let mut read_b = peer_b.await.unwrap();

    // It gets stream/start with the live config, immediately (not deferred
    // until some future push_audio call).
    let start_msg = match timeout(Duration::from_secs(5), read_b.next())
        .await
        .expect("timed out waiting for late joiner's stream/start")
        .unwrap()
        .unwrap()
    {
        WsMessage::Text(t) => t,
        other => panic!("expected text stream/start, got {other:?}"),
    };
    match serde_json::from_str::<Message>(&start_msg).unwrap() {
        Message::StreamStart(s) => {
            let player = s.player.expect("expected player config");
            assert_eq!(player.sample_rate, 48000);
        }
        other => panic!("expected stream/start, got {other:?}"),
    }

    // The next push reaches both members with the identical timestamp/bytes
    // — the late joiner is synchronized with the one that was already there.
    let sent_timestamp = group.push_audio(&[0xBB; 8]);

    let frame_a = match read_a.next().await.expect("no second chunk for A").unwrap() {
        WsMessage::Binary(b) => b,
        other => panic!("expected binary, got {other:?}"),
    };
    let frame_b = match read_b
        .next()
        .await
        .expect("no chunk for late joiner")
        .unwrap()
    {
        WsMessage::Binary(b) => b,
        other => panic!("expected binary, got {other:?}"),
    };
    let chunk_a = AudioChunk::from_bytes(&frame_a).unwrap();
    let chunk_b = AudioChunk::from_bytes(&frame_b).unwrap();
    assert_eq!(chunk_a.timestamp, sent_timestamp);
    assert_eq!(chunk_b.timestamp, sent_timestamp);
    assert_eq!(&*chunk_b.data, &[0xBB; 8][..]);
    assert_eq!(
        &*chunk_a.data, &*chunk_b.data,
        "late joiner must be in sync with the member that was already there"
    );

    // And it never received the chunk sent before it joined.
    assert_ne!(&*chunk_b.data, &[0xAA; 8][..]);
}

#[tokio::test]
async fn a_dead_member_is_pruned_without_blocking_the_survivor() {
    let (listener, url) = bind_test_listener().await;

    let peer_a = tokio::spawn({
        let url = url.clone();
        async move { connect_peer(&url, "member-a").await }
    });
    let peer_b = tokio::spawn({
        let url = url.clone();
        async move { connect_peer(&url, "member-b").await }
    });

    let (conn_a, _) = timeout(Duration::from_secs(5), listener.accept())
        .await
        .unwrap()
        .unwrap();
    let (conn_b, _) = timeout(Duration::from_secs(5), listener.accept())
        .await
        .unwrap()
        .unwrap();

    let group = Group::new(std::sync::Arc::new(sendspin::DefaultClock::default()));
    group
        .add_member(conn_a.client_id().to_string(), conn_a.sender())
        .await
        .unwrap();
    group
        .add_member(conn_b.client_id().to_string(), conn_b.sender())
        .await
        .unwrap();

    // Kill member A's connection outright (not a clean disconnect) — the
    // survivor must still get its command.
    conn_a.disconnect().await.unwrap();
    let mut read_a = peer_a.await.unwrap();
    let mut read_b = peer_b.await.unwrap();
    // Drain member A's own close frame so its stream doesn't matter further.
    let _ = timeout(Duration::from_secs(2), read_a.next()).await;

    group
        .send_player_command(PlayerCommand {
            command: PlayerCommandType::Volume,
            volume: Some(77),
            mute: None,
            static_delay_ms: None,
        })
        .await;

    assert_eq!(
        group.member_ids(),
        vec!["member-b"],
        "member-a's failed send must have pruned it from the group"
    );

    let msg = timeout(Duration::from_secs(5), read_b.next())
        .await
        .expect("timed out waiting for server/command")
        .expect("no message")
        .unwrap();
    let text = match msg {
        WsMessage::Text(t) => t,
        other => panic!("expected text, got {other:?}"),
    };
    match serde_json::from_str::<Message>(&text).unwrap() {
        Message::ServerCommand(cmd) => {
            let player = cmd.player.expect("expected player command");
            assert_eq!(player.volume, Some(77));
        }
        other => panic!("expected server/command, got {other:?}"),
    }
}

/// Per-device senders on a shared timeline: two *separate*
/// groups — each standing in for one independently-addressable device — that
/// share a single `SharedTimeline` emit an identical timestamp for the same
/// chunk when the caller stamps the timeline once and fans the result to each
/// group via `push_at`. This is the property that lets per-device senders
/// stay sample-accurately coincident while being ducked/overlaid/routed on
/// their own. It also proves the two senders can carry *different* bytes at
/// that one shared timestamp (the per-device overlay/duck primitive).
#[tokio::test]
async fn separate_groups_sharing_a_timeline_stamp_identically() {
    use sendspin::server::SharedTimeline;
    use std::sync::Arc;

    let (listener, url) = bind_test_listener().await;

    let peer_a = tokio::spawn({
        let url = url.clone();
        async move { connect_peer(&url, "device-a").await }
    });
    let peer_b = tokio::spawn({
        let url = url.clone();
        async move { connect_peer(&url, "device-b").await }
    });
    let (conn_a, _) = timeout(Duration::from_secs(5), listener.accept())
        .await
        .unwrap()
        .unwrap();
    let (conn_b, _) = timeout(Duration::from_secs(5), listener.accept())
        .await
        .unwrap()
        .unwrap();

    // One timeline, two independent single-member groups (one per "device").
    let timeline = Arc::new(SharedTimeline::new(Arc::new(
        sendspin::DefaultClock::default(),
    )));
    let group_a = Group::with_timeline(Arc::clone(&timeline));
    let group_b = Group::with_timeline(Arc::clone(&timeline));
    group_a
        .add_member(conn_a.client_id().to_string(), conn_a.sender())
        .await
        .unwrap();
    group_b
        .add_member(conn_b.client_id().to_string(), conn_b.sender())
        .await
        .unwrap();

    let config = pcm_config();
    // The shared-timeline contract: the *coordinator* owns the timeline's
    // config/anchor, and each group only announces the stream to its own members.
    // `start_stream` would re-anchor the shared timeline from inside one group and
    // desync the other, so a `Group<SharesTimeline>` does not have it at all.
    timeline.set_config(config.clone());
    group_a.broadcast_stream_start(config.clone()).await;
    group_b.broadcast_stream_start(config).await;

    // Stamp ONCE for this chunk, then hand that single ts to each group. Device
    // B gets a different payload (as an overlay/duck would produce) at the very
    // same timestamp.
    let bytes_a = [1u8, 2, 3, 4, 5, 6, 7, 8];
    let bytes_b = [9u8, 9, 9, 9, 9, 9, 9, 9];
    let shared_ts = timeline.stamp(bytes_a.len());
    group_a.push_at(shared_ts, &bytes_a);
    group_b.push_at(shared_ts, &bytes_b);

    let mut read_a = peer_a.await.unwrap();
    let mut read_b = peer_b.await.unwrap();

    // Each device gets its stream/start (Text) first.
    for read in [&mut read_a, &mut read_b] {
        let msg = read.next().await.expect("no stream/start").unwrap();
        assert!(matches!(msg, WsMessage::Text(_)));
    }

    let frame_a = match read_a.next().await.expect("no audio A").unwrap() {
        WsMessage::Binary(b) => b,
        other => panic!("expected binary, got {other:?}"),
    };
    let frame_b = match read_b.next().await.expect("no audio B").unwrap() {
        WsMessage::Binary(b) => b,
        other => panic!("expected binary, got {other:?}"),
    };
    let chunk_a = AudioChunk::from_bytes(&frame_a).unwrap();
    let chunk_b = AudioChunk::from_bytes(&frame_b).unwrap();

    // The whole point: identical timestamp across independent senders...
    assert_eq!(chunk_a.timestamp, shared_ts);
    assert_eq!(chunk_b.timestamp, shared_ts);
    assert_eq!(chunk_a.timestamp, chunk_b.timestamp);
    // ...while each carries its own (potentially ducked/overlaid) payload.
    assert_eq!(&*chunk_a.data, &bytes_a[..]);
    assert_eq!(&*chunk_b.data, &bytes_b[..]);
}
