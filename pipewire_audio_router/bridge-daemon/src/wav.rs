//! Minimal in-memory WAV construction for announce audio: `decode.rs` hands
//! over PCM decoded from a URL-fetched clip (via symphonia), and `calibrate.rs`
//! builds its click track directly. Neither needs a real encoder — both already
//! have raw PCM samples in hand, just missing a header.

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

/// Read a PCM WAV produced by [`build_wav`] (canonical 44-byte header, data
/// chunk last): returns `(sample_rate, channels, pcm_bytes)`. Returns `None` if
/// it isn't the expected layout. Used to turn a synthesized/decoded WAV back
/// into raw PCM for resampling (announce.rs).
pub fn read_pcm16(wav: &[u8]) -> Option<(u32, u16, &[u8])> {
    if wav.len() < 44 || &wav[0..4] != b"RIFF" || &wav[8..12] != b"WAVE" || &wav[12..16] != b"fmt " {
        return None;
    }
    let channels = u16::from_le_bytes([wav[22], wav[23]]);
    let sample_rate = u32::from_le_bytes([wav[24], wav[25], wav[26], wav[27]]);
    // build_wav writes the data chunk immediately after the 16-byte fmt chunk.
    if &wav[36..40] != b"data" {
        return None;
    }
    let data_len = u32::from_le_bytes([wav[40], wav[41], wav[42], wav[43]]) as usize;
    let end = (44 + data_len).min(wav.len());
    Some((sample_rate, channels, &wav[44..end]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_pcm16_round_trips_build_wav() {
        let pcm = vec![1u8, 2, 3, 4, 5, 6, 7, 8];
        let wav = build_wav(&pcm, 48000, 16, 2);
        let (rate, ch, data) = read_pcm16(&wav).expect("parse");
        assert_eq!(rate, 48000);
        assert_eq!(ch, 2);
        assert_eq!(data, pcm.as_slice());
    }

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
