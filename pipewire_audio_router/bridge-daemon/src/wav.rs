//! Minimal in-memory WAV construction, shared by both announce audio
//! sources (api.rs's `AnnounceRequest`): `decode.rs` (v1, URL-fetched
//! clips decoded via symphonia) and `wyoming.rs` (v2, synthesized PCM
//! from a Wyoming TTS server). Neither path needs a real encoder — both
//! already have raw PCM samples in hand, just missing a header.

/// Builds a complete PCM WAV file from raw interleaved samples already in
/// the given format. No external encoder involved — this is just the
/// 44-byte RIFF/WAVE header prepended to the sample data.
pub fn build_wav(pcm: &[u8], sample_rate: u32, bits_per_sample: u16, channels: u16) -> Vec<u8> {
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
