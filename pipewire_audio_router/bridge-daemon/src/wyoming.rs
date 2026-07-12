//! Minimal Wyoming protocol TTS client (PLAN.md Section 5.6, v2 —
//! Phase 3.5), used as an **additive alternative** to the v1 file+URL
//! announce path (api.rs's `AnnounceRequest::url`), not a replacement:
//! `POST /api/media_players/:node_id/announce` accepts either `url` (HA's
//! existing `tts`-rendered-file contract, unchanged) or `wyoming`
//! (synthesize directly against a local Piper instance, skipping the
//! render-to-file-then-HTTP-fetch round trip for lower first-audible-word
//! latency). Both paths converge on the same WAV file before the
//! duck/play/restore logic in api.rs, which has no idea which path
//! produced it.
//!
//! Wyoming (<https://github.com/rhasspy/wyoming>) frames every message as
//! one JSON object per line, optionally followed by exactly
//! `payload_length` raw bytes (no separating newline) if that field is
//! present — used here for the PCM audio chunks. `AudioFormat.width` in
//! the protocol is **bytes** per sample, not bits (confirmed against the
//! `wyoming` Python package's `AudioChunk`/`AudioFormat` dataclasses,
//! e.g. `width=2` for 16-bit PCM) — converted to bits when building the
//! WAV header below.
//!
//! Scope: talks directly to Piper's `synthesize` event and collects the
//! `audio-chunk` stream into an in-memory buffer, since TTS/announce
//! clips are short (a sentence or two) — no need for the seek-and-patch
//! streaming-WAV-header dance a longer recording would require.

use serde::Deserialize;
use serde_json::json;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

#[derive(Deserialize)]
struct EventHeader {
    #[serde(rename = "type")]
    event_type: String,
    #[serde(default)]
    data: serde_json::Value,
    payload_length: Option<usize>,
}

/// Synthesizes `text` via a Wyoming TTS server (e.g. Piper) at
/// `host:port` and returns a complete WAV file's bytes. `voice` is
/// optional (Piper supports multi-speaker models; omit for the server's
/// default voice).
pub async fn synthesize_to_wav(host: &str, port: u16, text: &str, voice: Option<&str>) -> anyhow::Result<Vec<u8>> {
    let mut stream = TcpStream::connect((host, port)).await?;

    let mut data = json!({ "text": text });
    if let Some(voice_name) = voice {
        data["voice"] = json!({ "name": voice_name });
    }
    let mut request_line = serde_json::to_vec(&json!({ "type": "synthesize", "data": data }))?;
    request_line.push(b'\n');
    stream.write_all(&request_line).await?;
    stream.flush().await?;

    let (read_half, _write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);

    let mut pcm = Vec::new();
    let mut sample_rate: u32 = 22050;
    let mut width_bytes: u16 = 2;
    let mut channels: u16 = 1;
    let mut got_audio_start = false;

    loop {
        let mut line = String::new();
        // read_line's return value is bytes read, not a bool — 0 means EOF
        // (server closed the connection) rather than an empty line.
        let bytes_read = reader.read_line(&mut line).await?;
        if bytes_read == 0 {
            break;
        }
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            continue;
        }
        let header: EventHeader = serde_json::from_str(trimmed)?;

        match header.event_type.as_str() {
            "audio-start" => {
                got_audio_start = true;
                if let Some(v) = header.data.get("rate").and_then(|v| v.as_u64()) {
                    sample_rate = v as u32;
                }
                if let Some(v) = header.data.get("width").and_then(|v| v.as_u64()) {
                    width_bytes = v as u16;
                }
                if let Some(v) = header.data.get("channels").and_then(|v| v.as_u64()) {
                    channels = v as u16;
                }
            }
            "audio-chunk" => {
                if let Some(len) = header.payload_length {
                    let mut chunk = vec![0u8; len];
                    reader.read_exact(&mut chunk).await?;
                    pcm.extend_from_slice(&chunk);
                }
            }
            "audio-stop" => break,
            // Any other event (e.g. "error") — drain its payload if it has
            // one, so the stream stays framed correctly, then keep reading.
            _ => {
                if let Some(len) = header.payload_length {
                    let mut discard = vec![0u8; len];
                    reader.read_exact(&mut discard).await?;
                }
                if header.event_type == "error" {
                    anyhow::bail!("wyoming server reported an error: {}", header.data);
                }
            }
        }
    }

    if !got_audio_start {
        anyhow::bail!("wyoming server at {host}:{port} never sent audio-start");
    }
    if pcm.is_empty() {
        anyhow::bail!("wyoming server at {host}:{port} produced no audio for the given text");
    }

    Ok(build_wav(&pcm, sample_rate, width_bytes * 8, channels))
}

/// Builds a minimal PCM WAV file in memory — no external encoder needed
/// since we already have raw samples in the exact format Wyoming reported.
fn build_wav(pcm: &[u8], sample_rate: u32, bits_per_sample: u16, channels: u16) -> Vec<u8> {
    let byte_rate = sample_rate * channels as u32 * (bits_per_sample as u32 / 8);
    let block_align = channels * (bits_per_sample / 8);
    let data_len = pcm.len() as u32;

    let mut out = Vec::with_capacity(44 + pcm.len());
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVE");
    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&16u32.to_le_bytes()); // fmt chunk size
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&channels.to_le_bytes());
    out.extend_from_slice(&sample_rate.to_le_bytes());
    out.extend_from_slice(&byte_rate.to_le_bytes());
    out.extend_from_slice(&block_align.to_le_bytes());
    out.extend_from_slice(&bits_per_sample.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    out.extend_from_slice(pcm);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wav_header_matches_input_format() {
        let pcm = vec![0u8; 100];
        let wav = build_wav(&pcm, 22050, 16, 1);
        assert_eq!(&wav[0..4], b"RIFF");
        assert_eq!(&wav[8..12], b"WAVE");
        assert_eq!(u32::from_le_bytes(wav[4..8].try_into().unwrap()), 36 + 100);
        assert_eq!(u16::from_le_bytes(wav[22..24].try_into().unwrap()), 1); // channels
        assert_eq!(u32::from_le_bytes(wav[24..28].try_into().unwrap()), 22050); // rate
        assert_eq!(u16::from_le_bytes(wav[34..36].try_into().unwrap()), 16); // bits/sample
        assert_eq!(u32::from_le_bytes(wav[40..44].try_into().unwrap()), 100); // data size
        assert_eq!(&wav[44..], pcm.as_slice());
    }
}
