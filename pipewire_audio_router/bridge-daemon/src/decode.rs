//! Decodes announce audio fetched via URL (api.rs's `AnnounceRequest::url`)
//! into WAV, using `symphonia`. See docs/decisions.md "Decoding announce
//! audio: `symphonia`, not an `ffmpeg` subprocess".

use crate::wav::build_wav;
use std::fs::File;
use std::path::Path;
use symphonia::core::audio::{AudioBufferRef, SampleBuffer};
use symphonia::core::codecs::{DecoderOptions, CODEC_TYPE_NULL};
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

/// Decodes the file at `path` — symphonia probes the actual format from
/// content, so it doesn't matter whether it's mp3, wav, aac, ogg, or flac
/// (whatever a caller's HTTP-fetched TTS clip happens to be) — into a
/// complete WAV file's bytes, always standardized to 16-bit PCM regardless
/// of the source sample format. Runs on a blocking thread pool: symphonia's
/// API is synchronous, and decoding would otherwise block the async
/// runtime's worker thread for the duration.
pub async fn decode_file_to_wav(path: &Path) -> anyhow::Result<Vec<u8>> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || decode_file_to_wav_blocking(&path)).await?
}

fn decode_file_to_wav_blocking(path: &Path) -> anyhow::Result<Vec<u8>> {
    let file = File::open(path)?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());

    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }

    let probed = symphonia::default::get_probe().format(&hint, mss, &FormatOptions::default(), &MetadataOptions::default())?;
    let mut format = probed.format;

    let track = format
        .tracks()
        .iter()
        .find(|t| t.codec_params.codec != CODEC_TYPE_NULL)
        .ok_or_else(|| anyhow::anyhow!("no decodable audio track found"))?;
    let track_id = track.id;
    let sample_rate = track.codec_params.sample_rate.ok_or_else(|| anyhow::anyhow!("announce audio has no known sample rate"))?;
    let channels = track.codec_params.channels.ok_or_else(|| anyhow::anyhow!("announce audio has no known channel layout"))?.count() as u16;

    let mut decoder = symphonia::default::get_codecs().make(&track.codec_params, &DecoderOptions::default())?;

    let mut pcm: Vec<u8> = Vec::new();
    loop {
        let packet = match format.next_packet() {
            Ok(packet) => packet,
            // Normal end of stream, not an error — every format eventually
            // exhausts its packets this way.
            Err(SymphoniaError::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(SymphoniaError::ResetRequired) => break,
            Err(e) => return Err(e.into()),
        };
        if packet.track_id() != track_id {
            continue;
        }
        match decoder.decode(&packet) {
            Ok(decoded) => append_as_i16_le(decoded, &mut pcm),
            // A single bad packet shouldn't sink the whole clip — skip it
            // and keep decoding the rest.
            Err(SymphoniaError::DecodeError(_)) => continue,
            Err(e) => return Err(e.into()),
        }
    }

    if pcm.is_empty() {
        anyhow::bail!("decoded zero audio samples from {}", path.display());
    }

    Ok(build_wav(&pcm, sample_rate, 16, channels))
}

/// Converts a decoded packet (whatever sample format the source codec
/// used — u8/s24/f32/...) into interleaved signed 16-bit little-endian
/// PCM, appending to `out`. Standardizing on s16 here means the WAV we
/// build is always in the one format pw-cat/libsndfile are guaranteed to
/// handle, regardless of the source clip's original bit depth.
fn append_as_i16_le(decoded: AudioBufferRef, out: &mut Vec<u8>) {
    let spec = *decoded.spec();
    let duration = decoded.capacity() as u64;
    let mut sample_buf = SampleBuffer::<i16>::new(duration, spec);
    sample_buf.copy_interleaved_ref(decoded);
    out.reserve(sample_buf.samples().len() * 2);
    for sample in sample_buf.samples() {
        out.extend_from_slice(&sample.to_le_bytes());
    }
}
