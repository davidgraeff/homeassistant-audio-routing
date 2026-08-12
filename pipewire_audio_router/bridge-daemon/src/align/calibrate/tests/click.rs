//! The generated click pattern.

use super::super::*;

#[test]
fn click_wav_is_a_valid_two_second_stereo_pattern() {
    let wav = click_wav();
    // RIFF/WAVE header present.
    assert_eq!(&wav[0..4], b"RIFF");
    assert_eq!(&wav[8..12], b"WAVE");
    // Stereo, 44100, 16-bit.
    assert_eq!(u16::from_le_bytes(wav[22..24].try_into().unwrap()), CHANNELS);
    assert_eq!(u32::from_le_bytes(wav[24..28].try_into().unwrap()), RATE);
    // data length = 2 s * rate * channels * 2 bytes.
    let expect = (PATTERN_SECS * RATE as f64) as usize * CHANNELS as usize * 2;
    assert_eq!(u32::from_le_bytes(wav[40..44].try_into().unwrap()) as usize, expect);
}
