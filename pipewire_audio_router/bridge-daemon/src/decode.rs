//! Decodes announce audio fetched via URL (api.rs's `AnnounceRequest::url`)
//! into WAV, using `symphonia`. See docs/decisions.md "Decoding announce
//! audio: `symphonia`, not an `ffmpeg` subprocess".

use crate::wav::build_wav;
use std::fs::File;
use std::io::Cursor;
use std::path::Path;
use symphonia::core::audio::GenericAudioBufferRef;
use symphonia::core::codecs::audio::AudioDecoderOptions;
use symphonia::core::codecs::CodecParameters;
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, TrackType};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;

/// Decodes an in-memory clip (e.g. a `include_bytes!`-embedded diagnostic
/// asset) into a 16-bit PCM WAV. Always standardized to 16-bit PCM,
/// whatever the source sample format. `ext` is a format hint
/// (`"mp3"`, `"wav"`, …); symphonia still probes the real format from content.
pub async fn decode_bytes_to_wav(bytes: &'static [u8], ext: &'static str) -> anyhow::Result<Vec<u8>> {
    tokio::task::spawn_blocking(move || {
        let mss = MediaSourceStream::new(Box::new(Cursor::new(bytes)), Default::default());
        let mut hint = Hint::new();
        hint.with_extension(ext);
        decode_stream_to_wav(mss, hint)
    })
    .await?
}

/// Decode the file at `path` straight to overlay-ready PCM: 48 kHz, stereo,
/// S16LE (the capture/overlay format). Used by the per-device announce path
/// (announce.rs) which mixes the clip into one device's stream.
pub async fn decode_file_to_pcm_48k_stereo(path: &Path) -> anyhow::Result<Vec<u8>> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let file = File::open(&path)?;
        let mss = MediaSourceStream::new(Box::new(file), Default::default());
        let mut hint = Hint::new();
        if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
            hint.with_extension(ext);
        }
        let (pcm, rate, channels) = decode_stream_to_pcm(mss, hint)?;
        Ok(crate::resample::to_48k_stereo_s16le(&pcm, rate, channels))
    })
    .await?
}

/// Shared decode core: probe the stream's format, decode its first audio track,
/// and standardize to a 16-bit PCM WAV.
fn decode_stream_to_wav(mss: MediaSourceStream, hint: Hint) -> anyhow::Result<Vec<u8>> {
    let (pcm, sample_rate, channels) = decode_stream_to_pcm(mss, hint)?;
    Ok(build_wav(&pcm, sample_rate, 16, channels))
}

/// Decode the first audio track to interleaved S16LE PCM, returning it with its
/// native sample rate and channel count.
fn decode_stream_to_pcm(mss: MediaSourceStream, hint: Hint) -> anyhow::Result<(Vec<u8>, u32, u16)> {
    let mut format = symphonia::default::get_probe().probe(&hint, mss, FormatOptions::default(), MetadataOptions::default())?;

    let track = format.first_track_known_codec(TrackType::Audio).ok_or_else(|| anyhow::anyhow!("no decodable audio track found"))?;
    let track_id = track.id;
    let audio_params = match &track.codec_params {
        Some(CodecParameters::Audio(params)) => params,
        _ => anyhow::bail!("no decodable audio track found"),
    };
    let sample_rate = audio_params.sample_rate.ok_or_else(|| anyhow::anyhow!("announce audio has no known sample rate"))?;
    let channels =
        audio_params.channels.as_ref().ok_or_else(|| anyhow::anyhow!("announce audio has no known channel layout"))?.count() as u16;

    let mut decoder = symphonia::default::get_codecs().make_audio_decoder(audio_params, &AudioDecoderOptions::default())?;

    // Reused across packets so the per-packet interleave doesn't reallocate.
    let mut scratch: Vec<i16> = Vec::new();
    let mut pcm: Vec<u8> = Vec::new();
    loop {
        let packet = match format.next_packet() {
            Ok(Some(packet)) => packet,
            // Normal end of stream, not an error — every format eventually
            // exhausts its packets this way (now signalled as `Ok(None)`).
            Ok(None) => break,
            Err(SymphoniaError::ResetRequired) => break,
            Err(e) => return Err(e.into()),
        };
        if packet.track_id != track_id {
            continue;
        }
        match decoder.decode(&packet) {
            Ok(decoded) => append_as_i16_le(decoded, &mut scratch, &mut pcm),
            // A single bad packet shouldn't sink the whole clip — skip it
            // and keep decoding the rest.
            Err(SymphoniaError::DecodeError(_)) => continue,
            Err(e) => return Err(e.into()),
        }
    }

    if pcm.is_empty() {
        anyhow::bail!("decoded zero audio samples from the source clip");
    }

    Ok((pcm, sample_rate, channels))
}

/// Converts a decoded packet (whatever sample format the source codec
/// used — u8/s24/f32/...) into interleaved signed 16-bit little-endian
/// PCM, appending to `out`. Standardizing on s16 here means the WAV we
/// build is always in the one format pw-cat/libsndfile are guaranteed to
/// handle, regardless of the source clip's original bit depth.
fn append_as_i16_le(decoded: GenericAudioBufferRef, scratch: &mut Vec<i16>, out: &mut Vec<u8>) {
    // `copy_to_vec_interleaved` resizes `scratch` to this packet's interleaved
    // sample count and converts whatever the source sample format was into i16.
    decoded.copy_to_vec_interleaved(scratch);
    out.reserve(scratch.len() * 2);
    for sample in scratch.iter() {
        out.extend_from_slice(&sample.to_le_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The committed diagnostic clip the `test-announcement` endpoint embeds
    /// must decode to a non-empty 16-bit PCM WAV — this guards against the
    /// asset being replaced with something symphonia can't read.
    #[tokio::test]
    async fn embedded_test_announcement_decodes() {
        let mp3 = include_bytes!("../assets/test-announcement.mp3");
        let wav = decode_bytes_to_wav(mp3, "mp3").await.expect("decode embedded test announcement");
        assert_eq!(&wav[0..4], b"RIFF");
        assert_eq!(&wav[8..12], b"WAVE");
        // Non-trivial audio: well past the 44-byte header.
        assert!(wav.len() > 10_000, "decoded WAV suspiciously small: {} bytes", wav.len());
    }
}
