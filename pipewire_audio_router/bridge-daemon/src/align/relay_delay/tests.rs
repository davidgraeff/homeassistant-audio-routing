//! Tests for the provisional delay line and the calibration mute.

use super::*;

const F48: PcmFormat = PcmFormat::new(48_000, 2);
const FRAME: usize = 4;

/// A stream whose every frame is identifiable: frame `i` is `(i, i)` as two i16s.
fn frames(from: usize, count: usize) -> Vec<u8> {
    let mut v = Vec::with_capacity(count * FRAME);
    for i in from..from + count {
        let s = (i % 30_000) as i16;
        v.extend_from_slice(&s.to_le_bytes());
        v.extend_from_slice(&s.to_le_bytes());
    }
    v
}

fn silence(count: usize) -> Vec<u8> {
    vec![0u8; count * FRAME]
}

/// Push `blocks` blocks of `block_frames` of the identifiable stream through `rd`,
/// returning the concatenated output (undelayed blocks pass through, as the relays
/// would do with the `false` return).
fn run(rd: &RelayDelay, output: &str, block_frames: usize, blocks: usize, start_frame: usize) -> Vec<u8> {
    let mut buf = Vec::new();
    let mut got = Vec::new();
    for b in 0..blocks {
        let src = frames(start_frame + b * block_frames, block_frames);
        let delayed = rd.delay_into(output, F48, &src, &mut buf);
        got.extend_from_slice(if delayed { &buf } else { &src });
    }
    got
}

#[test]
fn no_delay_is_a_true_passthrough_and_costs_no_lookup() {
    let rd = RelayDelay::new();
    assert!(!rd.any_active());
    let mut out = vec![0xAAu8; 8]; // must be left untouched
    assert!(!rd.delay_into("out-a", F48, &frames(0, 64), &mut out));
    assert_eq!(out, vec![0xAAu8; 8]);
    // A line on ANOTHER output must not delay this one.
    rd.set_delay_us("out-b", 5_000).unwrap();
    assert!(rd.any_active());
    assert!(!rd.delay_into("out-a", F48, &frames(0, 64), &mut out));
    assert_eq!(out, vec![0xAAu8; 8]);
    assert!(rd.clear("out-b"));
    assert!(!rd.any_active());
}

#[test]
fn delay_is_sample_accurate_and_not_quantised_to_the_block_size() {
    // 1001 frames is deliberately NOT a multiple of the 960-frame Opus block the
    // sendspin relay re-cuts to, nor of the block size used here. This is the
    // property plan §1.1.1 turns on.
    const BLOCK: usize = 960;
    const D: usize = 1001;
    let rd = RelayDelay::new();
    let us = us_for_frames(D as u64, 48_000);
    rd.set_delay_us("spk", us).unwrap();
    assert_eq!(rd.status("spk").unwrap().delay_frames, D as u64, "µs↔frames round trip must be exact");

    let got = run(&rd, "spk", BLOCK, 6, 0);
    let mut want = silence(D);
    want.extend_from_slice(&frames(0, BLOCK * 6 - D));
    assert_eq!(got, want, "output must be the input shifted by exactly {D} frames");
}

#[test]
fn delay_survives_ring_wraparound() {
    // Enough audio to wrap the (MAX-sized) ring several times.
    const BLOCK: usize = 1024;
    const D: usize = 7_777;
    let rd = RelayDelay::new();
    rd.set_delay_us("spk", us_for_frames(D as u64, 48_000)).unwrap();
    let ring_frames = rd.status("spk").unwrap().ring_bytes / FRAME;
    let blocks = (ring_frames * 3) / BLOCK;
    let got = run(&rd, "spk", BLOCK, blocks, 0);
    let mut want = silence(D);
    want.extend_from_slice(&frames(0, BLOCK * blocks - D));
    assert_eq!(got.len(), want.len());
    assert_eq!(got, want, "content must stay exact across {} ring wraps", 3);
}

#[test]
fn priming_emits_silence_then_content_with_no_discontinuity() {
    const BLOCK: usize = 480;
    const D: usize = 1_200; // 2.5 blocks: priming ends mid-block
    let rd = RelayDelay::new();
    rd.set_delay_us("spk", us_for_frames(D as u64, 48_000)).unwrap();

    let st = rd.status("spk").unwrap();
    assert!(!st.primed);
    assert_eq!(st.prime_remaining_us, us_for_frames(D as u64, 48_000));

    let got = run(&rd, "spk", BLOCK, 2, 0);
    assert_eq!(got, silence(BLOCK * 2), "before {D} frames have flowed the output is silence");
    assert!(!rd.status("spk").unwrap().primed);

    // Third block straddles the boundary: 240 frames of silence, then frame 0.
    let mut buf = Vec::new();
    let src = frames(2 * BLOCK, BLOCK);
    assert!(rd.delay_into("spk", F48, &src, &mut buf));
    let mut want = silence(D - 2 * BLOCK);
    want.extend_from_slice(&frames(0, BLOCK - (D - 2 * BLOCK)));
    assert_eq!(buf, want);
    let st = rd.status("spk").unwrap();
    assert!(st.primed);
    assert_eq!(st.prime_remaining_us, 0);
}

#[test]
fn increasing_the_delay_reprimes_only_the_increment() {
    const BLOCK: usize = 960;
    let rd = RelayDelay::new();
    rd.set_delay_us("spk", us_for_frames(100, 48_000)).unwrap();
    // Prime and run past it.
    let _ = run(&rd, "spk", BLOCK, 4, 0);
    assert!(rd.status("spk").unwrap().primed);

    // 100 → 5000 frames: 4900 frames of history are missing, so that much silence
    // comes out, and then content resumes exactly 5000 frames behind.
    rd.set_delay_us("spk", us_for_frames(5_000, 48_000)).unwrap();
    assert!(!rd.status("spk").unwrap().primed);
    let got = run(&rd, "spk", BLOCK, 8, BLOCK * 4);
    // History available at the change: 4*960 = 3840 frames (frames 0..3839).
    // First output frame wanted is (3840 + 1) - 5000 → pre-history ⇒ silence until
    // the wanted index reaches 0, i.e. for 5000 - 3840 = 1160 frames.
    let mut want = silence(1_160);
    want.extend_from_slice(&frames(0, BLOCK * 8 - 1_160));
    assert_eq!(got, want);
    assert!(rd.status("spk").unwrap().primed);
}

#[test]
fn decreasing_the_delay_skips_forward_and_stays_exact() {
    const BLOCK: usize = 960;
    let rd = RelayDelay::new();
    rd.set_delay_us("spk", us_for_frames(2_000, 48_000)).unwrap();
    let _ = run(&rd, "spk", BLOCK, 4, 0); // 3840 frames in, primed
    rd.set_delay_us("spk", us_for_frames(500, 48_000)).unwrap();
    assert!(rd.status("spk").unwrap().primed, "a shorter delay needs no new history");
    // Next block covers input frames 3840..4799; at delay 500 the output is
    // 3341..4300 — a jump forward over the 1500 frames the old delay was holding.
    let mut buf = Vec::new();
    let src = frames(4 * BLOCK, BLOCK);
    assert!(rd.delay_into("spk", F48, &src, &mut buf));
    assert_eq!(buf, frames(4 * BLOCK - 500, BLOCK));
}

#[test]
fn the_cap_holds_and_a_rejection_leaves_the_previous_delay_alone() {
    let rd = RelayDelay::new();
    rd.set_delay_us("spk", 20_000).unwrap();
    assert!(rd.set_delay_us("spk", MAX_DELAY_US + 1).is_err());
    assert_eq!(rd.delay_us("spk"), Some(20_000), "a rejected value must not disturb the applied one");
    assert!(rd.set_delay_ms("spk", MAX_DELAY_MS as f64 + 0.001).is_err());
    assert!(rd.set_delay_ms("spk", -1.0).is_err());
    assert!(rd.set_delay_ms("spk", f64::NAN).is_err());
    // The bound is what makes the ring a fixed cost: 1000 ms + headroom at 48 kHz
    // stereo.
    let ring = rd.status("spk").unwrap().ring_bytes;
    assert_eq!(ring, (48_000 + HEADROOM_FRAMES as usize) * FRAME);
    assert_eq!(ring, 224_768, "documented per-output memory cost (~220 KiB)");
    // At the cap itself the line still works.
    rd.set_delay_us("spk", MAX_DELAY_US).unwrap();
    assert_eq!(rd.status("spk").unwrap().delay_frames, 48_000);
    let mut buf = Vec::new();
    assert!(rd.delay_into("spk", F48, &frames(0, 1024), &mut buf));
    assert_eq!(buf, silence(1024));
}

#[test]
fn zero_clears_the_line_and_a_sub_frame_delay_is_a_passthrough() {
    let rd = RelayDelay::new();
    rd.set_delay_us("spk", 5_000).unwrap();
    assert_eq!(rd.set_delay_us("spk", 0).unwrap(), 0);
    assert_eq!(rd.delay_us("spk"), None);
    assert!(!rd.any_active());
    // 10 µs at 48 kHz rounds to 0 frames: the line exists but must not corrupt audio.
    rd.set_delay_us("spk", 10).unwrap();
    let mut buf = Vec::new();
    assert!(!rd.delay_into("spk", F48, &frames(0, 64), &mut buf));
}

#[test]
fn composes_with_an_active_announcement_overlay_in_the_relay_order() {
    // Mirrors the call sites: mix_into first, then delay_into on its result — so the
    // delay applies to duck(music)+overlay, exactly as a device-side knob would.
    let output = "relay-delay-compose-test";
    let mixer = crate::outputs::overlay_mixer::OverlayMixer::global();
    const BLOCK: usize = 480;
    const D: usize = 700;
    let rd = RelayDelay::new();
    rd.set_delay_us(output, us_for_frames(D as u64, 48_000)).unwrap();

    // A long enough clip that it stays active for the whole test.
    let clip = frames(9_000, BLOCK * 8);
    mixer.start(output, 1, clip.clone(), 0.5);

    let mut mix_buf = Vec::new();
    let mut delay_buf = Vec::new();
    let mut mixed_stream = Vec::new();
    let mut out_stream = Vec::new();
    for b in 0..6 {
        let src = frames(b * BLOCK, BLOCK);
        let overlaid = mixer.mix_into(output, &src, &mut mix_buf);
        assert!(overlaid, "the overlay must be active for this test to mean anything");
        mixed_stream.extend_from_slice(&mix_buf);
        let delayed = rd.delay_into(output, F48, &mix_buf, &mut delay_buf);
        assert!(delayed);
        out_stream.extend_from_slice(&delay_buf);
    }
    mixer.stop(output);
    let _ = mixer.take_finished();

    // The delayed stream is the *mixed* stream shifted by D — i.e. the announcement
    // is delayed too, and nothing about the mix is disturbed.
    let mut want = silence(D);
    want.extend_from_slice(&mixed_stream[..mixed_stream.len() - D * FRAME]);
    assert_eq!(out_stream, want);
    // Sanity: the mix really did change the audio (so we aren't comparing music).
    assert_ne!(mixed_stream[..BLOCK * FRAME], frames(0, BLOCK)[..]);
}

#[test]
fn a_rate_change_reprimes_and_keeps_the_delay_in_time_not_samples() {
    // An AP2 group renegotiating 48 → 44.1 kHz: the ring's contents are a different
    // rate, so history is dropped, but the *time* delay is preserved.
    let rd = RelayDelay::new();
    rd.set_delay_us("spk", 10_000).unwrap(); // 10 ms
    let _ = run(&rd, "spk", 960, 4, 0);
    assert_eq!(rd.status("spk").unwrap().delay_frames, 480);
    assert!(rd.status("spk").unwrap().primed);

    let f441 = PcmFormat::new(44_100, 2);
    let mut buf = Vec::new();
    assert!(rd.delay_into("spk", f441, &frames(0, 100), &mut buf));
    let st = rd.status("spk").unwrap();
    assert_eq!(st.rate, 44_100);
    assert_eq!(st.delay_frames, 441, "10 ms at 44.1 kHz");
    assert!(!st.primed, "history from the old rate must not be reused — it re-primes from scratch");
    assert_eq!(st.prime_remaining_us, us_for_frames(441 - 100, 44_100));
    assert_eq!(buf, silence(100));
    // No reallocation on the way down in rate: the ring was sized for 48 kHz.
    assert_eq!(st.ring_bytes, (48_000 + HEADROOM_FRAMES as usize) * FRAME);
}

#[test]
fn a_block_that_is_not_whole_frames_passes_through_undelayed() {
    let rd = RelayDelay::new();
    rd.set_delay_us("spk", 5_000).unwrap();
    let mut buf = Vec::new();
    let mut src = frames(0, 64);
    src.push(0x7f); // 257 bytes: not a whole stereo frame
    assert!(!rd.delay_into("spk", F48, &src, &mut buf));
    assert!(!rd.delay_into("spk", F48, &[], &mut buf));
}

#[test]
fn set_all_replaces_the_whole_set_atomically() {
    let rd = RelayDelay::new();
    rd.set_delay_us("a", 1_000).unwrap();
    rd.set_delay_us("b", 2_000).unwrap();
    rd.set_all_us([("b", 3_000), ("c", 4_000)]).unwrap();
    let snap: Vec<(String, u64)> = rd.snapshot().into_iter().map(|(o, s)| (o, s.delay_us)).collect();
    assert_eq!(snap, vec![("b".to_string(), 3_000), ("c".to_string(), 4_000)], "'a' must be gone, 'b' updated");

    // A rejected member leaves the previous set untouched.
    assert!(rd.set_all_us([("b", 1), ("d", MAX_DELAY_US + 1)]).is_err());
    let snap: Vec<(String, u64)> = rd.snapshot().into_iter().map(|(o, s)| (o, s.delay_us)).collect();
    assert_eq!(snap, vec![("b".to_string(), 3_000), ("c".to_string(), 4_000)]);

    // A zero in a batch means "no delay on that output".
    rd.set_all_us([("b", 0), ("c", 5_000)]).unwrap();
    assert_eq!(rd.delay_us("b"), None);
    assert_eq!(rd.clear_all(), 1);
    assert!(!rd.any_active());
}

#[test]
fn all_primed_is_the_measurement_gate() {
    let rd = RelayDelay::new();
    assert!(rd.all_primed(), "no lines ⇒ nothing to wait for");
    rd.set_delay_us("a", 10_000).unwrap();
    rd.set_delay_us("b", 1_000).unwrap();
    assert!(!rd.all_primed());
    let _ = run(&rd, "b", 960, 2, 0); // 'b' primes (48 frames)
    assert!(rd.status("b").unwrap().primed);
    assert!(!rd.all_primed(), "'a' still hasn't seen audio");
    let _ = run(&rd, "a", 960, 1, 0); // 960 > 480 frames
    assert!(rd.all_primed());
}

// ---- calibration mute (W17) ---------------------------------------------------

#[test]
fn a_calibration_mute_is_exact_silence_and_the_idle_path_stays_free() {
    let rd = RelayDelay::new();
    let mut out = vec![0xAAu8; 8]; // must be left untouched while nothing applies
    assert!(!rd.delay_into("spk", F48, &frames(0, 64), &mut out));
    assert_eq!(out, vec![0xAAu8; 8], "no effects ⇒ one atomic load and nothing else");

    // A mute on ANOTHER output opens the gate but must not silence this one.
    assert!(!rd.set_muted("other", true), "there was no mute before");
    assert!(rd.any_active() && rd.any_muted());
    assert!(!rd.delay_into("spk", F48, &frames(0, 64), &mut out));
    assert_eq!(out, vec![0xAAu8; 8]);

    // Muted: exact silence, one block out per block in, and no ring at all.
    rd.set_muted("spk", true);
    let src = frames(0, 64);
    assert!(rd.delay_into("spk", F48, &src, &mut out));
    assert_eq!(out, silence(64), "every sample is zero — silence, not attenuation");
    let st = rd.status("spk").unwrap();
    assert!(st.muted);
    assert_eq!((st.delay_us, st.ring_bytes, st.primed), (0, 0, true), "a mute-only output costs no memory and has nothing to prime");
    assert_eq!(rd.snapshot().iter().filter(|(_, s)| s.muted).count(), 2);

    // Unmuting drops the entry, so the RT gate closes again.
    assert!(rd.set_muted("spk", false), "the previous state is reported");
    assert!(!rd.delay_into("spk", F48, &src, &mut out));
    rd.set_muted("other", false);
    assert!(!rd.any_active(), "nothing left ⇒ the hot path is free again");
    assert!(rd.status("spk").is_none());
}

/// The property that makes the mute safe to use mid-run: the delay line keeps
/// consuming and advancing while muted, so unmuting resumes at the sample continuous
/// playback would have reached. Asserted on **samples** against an identical,
/// never-muted line — a stalled ring would replay the muted stretch and a skipped one
/// would jump over it, and the estimator would measure either as a real offset.
#[test]
fn the_ring_keeps_advancing_while_muted_so_unmuting_is_continuous() {
    const BLOCK: usize = 480;
    const D: usize = 1_200;
    let us = us_for_frames(D as u64, 48_000);
    let reference = RelayDelay::new();
    let muted = RelayDelay::new();
    reference.set_delay_us("spk", us).unwrap();
    muted.set_delay_us("spk", us).unwrap();

    let (mut buf_ref, mut buf_mut) = (Vec::new(), Vec::new());
    for blk in 0..12 {
        if blk == 4 {
            muted.set_muted("spk", true);
        }
        if blk == 7 {
            assert!(muted.set_muted("spk", false));
        }
        let src = frames(blk * BLOCK, BLOCK);
        assert!(reference.delay_into("spk", F48, &src, &mut buf_ref));
        assert!(muted.delay_into("spk", F48, &src, &mut buf_mut));
        if (4..7).contains(&blk) {
            assert_eq!(buf_mut, silence(BLOCK), "block {blk} must be silent");
        } else {
            assert_eq!(buf_mut, buf_ref, "block {blk} must be exactly the never-muted stream");
        }
    }
    // The first block after the unmute, spelled out: input frame 7*480 read 1200
    // frames back, i.e. the audio the muted stretch was consuming — not a repeat of
    // it, and not a jump past it.
    let mut buf = Vec::new();
    assert!(muted.delay_into("spk", F48, &frames(12 * BLOCK, BLOCK), &mut buf));
    assert_eq!(buf, frames(12 * BLOCK - D, BLOCK));
    // And the delay is untouched: a mute neither re-primes nor changes the offset.
    let st = muted.status("spk").unwrap();
    assert!(st.primed && !st.muted);
    assert_eq!(st.delay_frames, D as u64);
}

/// A mute, an announcement overlay and a delay offset, all live at once — the state a
/// barge-in during a solo actually produces.
#[test]
fn a_mute_composes_with_an_overlay_and_a_delay_at_the_same_time() {
    let output = "relay-delay-mute-compose-test";
    let mixer = crate::outputs::overlay_mixer::OverlayMixer::global();
    const BLOCK: usize = 480;
    const D: usize = 700;
    let rd = RelayDelay::new();
    rd.set_delay_us(output, us_for_frames(D as u64, 48_000)).unwrap();
    mixer.start(output, 1, frames(9_000, BLOCK * 16), 0.5);

    let (mut mix_buf, mut delay_buf) = (Vec::new(), Vec::new());
    let (mut mixed, mut got) = (Vec::new(), Vec::new());
    for blk in 0..10 {
        if blk == 3 {
            rd.set_muted(output, true);
        }
        if blk == 6 {
            rd.set_muted(output, false);
        }
        let src = frames(blk * BLOCK, BLOCK);
        assert!(mixer.mix_into(output, &src, &mut mix_buf), "the overlay must stay active for this to mean anything");
        mixed.extend_from_slice(&mix_buf);
        assert!(rd.delay_into(output, F48, &mix_buf, &mut delay_buf));
        got.extend_from_slice(&delay_buf);
    }
    mixer.stop(output);
    let _ = mixer.take_finished();

    // Expected: duck(music)+overlay, shifted by D, with the muted blocks zeroed. So
    // the mute silences the announcement too — exactly what a device-side mute does —
    // while the delay is unaffected in both directions.
    let mut want = silence(D);
    want.extend_from_slice(&mixed[..mixed.len() - D * FRAME]);
    for blk in 3..6 {
        want[blk * BLOCK * FRAME..(blk + 1) * BLOCK * FRAME].fill(0);
    }
    assert_eq!(got, want);
    // Nothing was lost inside the muted window: the ring kept advancing, so the
    // block after the unmute carries real (mixed) audio.
    assert_ne!(got[6 * BLOCK * FRAME..7 * BLOCK * FRAME], silence(BLOCK)[..]);
}

#[test]
fn a_mute_and_a_delay_are_independent_state_on_the_same_output() {
    let rd = RelayDelay::new();
    rd.set_delay_us("spk", 5_000).unwrap();
    rd.set_muted("spk", true);

    // Clearing the delay must NOT unmute: a silenced member becoming audible again as
    // a side effect of a renormalisation is the hazard this mechanism exists for.
    assert!(rd.clear("spk"));
    assert!(rd.is_muted("spk") && rd.any_active());
    assert_eq!(rd.delay_us("spk"), None);
    assert_eq!(rd.status("spk").unwrap().ring_bytes, 0, "the ring goes with the delay");

    // Nor do the batch delay setters.
    rd.set_delay_us("spk", 5_000).unwrap();
    rd.set_all_us([("other", 1_000)]).unwrap();
    assert!(rd.is_muted("spk"), "set_all_us replaces the delay set, not the mute set");
    assert_eq!(rd.delay_us("spk"), None);
    rd.set_delay_us("spk", 5_000).unwrap();
    assert_eq!(rd.clear_all(), 2, "two delays cleared");
    assert!(rd.is_muted("spk"), "…and the mute survived that too");

    // The other way round: a mute change leaves the line and its history alone.
    rd.set_delay_us("spk", us_for_frames(480, 48_000)).unwrap();
    let _ = run(&rd, "spk", 960, 2, 0);
    assert!(rd.status("spk").unwrap().primed);
    rd.set_muted("spk", true);
    assert!(rd.status("spk").unwrap().primed, "muting must not re-prime");
    assert!(rd.set_muted("spk", false));
    assert!(rd.status("spk").unwrap().primed);
    assert_eq!(rd.delay_us("spk"), Some(us_for_frames(480, 48_000)));
}

#[test]
fn mute_batches_are_scoped_and_every_clearer_is_a_removal() {
    let rd = RelayDelay::new();
    // One solo = one batch = one lock, so no relay block sees half of it.
    assert_eq!(rd.set_mutes([("a", true), ("b", true), ("c", false)]), 2);
    assert_eq!(rd.muted_outputs(), vec!["a".to_string(), "b".to_string()]);
    assert_eq!(rd.set_mutes([("a", true), ("b", true)]), 0, "idempotent");

    // The next position's solo moves the mutes and touches nothing else.
    rd.set_muted("elsewhere", true);
    assert_eq!(rd.set_mutes([("a", false), ("b", true), ("c", true)]), 2);
    assert_eq!(rd.muted_outputs(), vec!["b".to_string(), "c".to_string(), "elsewhere".to_string()]);

    // Teardown, scoped: a stale hold releasing late can only touch its own outputs.
    assert_eq!(rd.unmute_all(["b", "c"]), 2);
    assert_eq!(rd.muted_outputs(), vec!["elsewhere".to_string()]);
    assert_eq!(rd.unmute_all(["b", "c"]), 0, "idempotent, and never a panic");

    // The abort of last resort.
    assert_eq!(rd.clear_mutes(), 1);
    assert!(!rd.any_muted() && !rd.any_active());
    assert_eq!(rd.clear_mutes(), 0);

    // A delay on an output whose mute is cleared keeps its entry and its ring.
    rd.set_delay_us("d", 4_000).unwrap();
    rd.set_muted("d", true);
    assert_eq!(rd.clear_mutes(), 1);
    assert_eq!(rd.delay_us("d"), Some(4_000));
    assert!(rd.any_active());
}

#[test]
fn frame_conversion_rounds_to_nearest_and_round_trips() {
    assert_eq!(frames_for(0, 48_000), 0);
    assert_eq!(frames_for(20_833, 48_000), 1_000); // 20.833 ms
    assert_eq!(frames_for(10, 48_000), 0); // under half a frame
    assert_eq!(frames_for(11, 48_000), 1); // over half a frame (20.83 µs)
    for frames in [1u64, 7, 959, 960, 961, 1_001, 47_999, 48_000] {
        assert_eq!(frames_for(us_for_frames(frames, 48_000), 48_000), frames, "48k round trip for {frames}");
    }
    for frames in [1u64, 7, 441, 1_001, 44_100] {
        assert_eq!(frames_for(us_for_frames(frames, 44_100), 44_100), frames, "44.1k round trip for {frames}");
    }
    assert_eq!(F48.frame_bytes(), 4);
}

/// Plan §12.2's stereo-pair remedy: a member driving two speakers is two acoustic
/// sources of the identical click, so its arrival is not one time. Emitting one channel
/// makes it one source, and it has to be exactly that — the other channel silent, the
/// chosen one bit-identical, and the block the same length so the sender's cadence,
/// timestamps and backlog are untouched.
#[test]
fn a_channel_mask_keeps_one_channel_bit_exact_and_the_block_the_same_length() {
    let rd = RelayDelay::new();
    let src = frames(0, 8);
    let mut out = Vec::new();

    assert_eq!(rd.channels("spk"), MeasureChannels::Both, "both is the default and costs no entry");
    assert!(!rd.any_active());
    assert!(!rd.delay_into("spk", F48, &src, &mut out), "both channels ⇒ nothing to do");

    rd.set_channels("spk", MeasureChannels::Left);
    assert!(rd.any_active(), "a mask is an effect, so the RT gate has to be open");
    assert!(rd.delay_into("spk", F48, &src, &mut out));
    assert_eq!(out.len(), src.len());
    for (i, frame) in out.as_chunks::<FRAME>().0.iter().enumerate() {
        assert_eq!(&frame[..2], &src[i * FRAME..i * FRAME + 2], "left is untouched, frame {i}");
        assert_eq!(&frame[2..], &[0, 0], "right is silent, frame {i}");
    }

    rd.set_channels("spk", MeasureChannels::Right);
    assert!(rd.delay_into("spk", F48, &src, &mut out));
    for (i, frame) in out.as_chunks::<FRAME>().0.iter().enumerate() {
        assert_eq!(&frame[..2], &[0, 0], "left is silent, frame {i}");
        assert_eq!(&frame[2..], &src[i * FRAME + 2..(i + 1) * FRAME], "right is untouched, frame {i}");
    }

    // Back to both, and the entry goes with it: nothing else was on this output.
    rd.set_channels("spk", MeasureChannels::Both);
    assert!(!rd.any_active());
    assert!(!rd.delay_into("spk", F48, &src, &mut out));
}

/// The three effects are independent state on one output, which is what lets a run set a
/// channel while a member is muted and expect both to still hold when it is unmuted.
#[test]
fn a_mask_composes_with_the_mute_and_the_delay_without_disturbing_either() {
    let rd = RelayDelay::new();
    let delay_frames = 480u64;
    rd.set_delay_us("spk", us_for_frames(delay_frames, 48_000)).unwrap();
    rd.set_channels("spk", MeasureChannels::Left);
    let _ = run(&rd, "spk", 960, 2, 0);
    let st = rd.status("spk").expect("an entry with all of it on");
    assert!(st.primed, "the mask must not re-prime the ring");
    assert_eq!(st.delay_frames, delay_frames);
    assert_eq!(st.channels_emitted, MeasureChannels::Left);

    // Delayed *and* masked: the emitted content is the delayed left channel, and the
    // right one is silent — one buffer, not two copies.
    let mut out = Vec::new();
    let src = frames(2_000, 4);
    assert!(rd.delay_into("spk", F48, &src, &mut out));
    assert_eq!(out.len(), src.len());
    assert!(out.as_chunks::<FRAME>().0.iter().all(|f| f[2..] == [0, 0]), "right silent");
    assert!(out.as_chunks::<FRAME>().0.iter().any(|f| f[..2] != [0, 0]), "left is real, delayed content");

    // A mute wins over the mask (silence has no channels), and dropping it brings the
    // mask back rather than losing it.
    rd.set_muted("spk", true);
    assert!(rd.delay_into("spk", F48, &src, &mut out));
    assert!(out.iter().all(|b| *b == 0), "muted ⇒ exact silence");
    assert!(rd.set_muted("spk", false));
    assert_eq!(rd.channels("spk"), MeasureChannels::Left, "the mask survived the mute");

    // Clearing the delay leaves the mask, and clearing the mask leaves the delay.
    assert!(rd.clear("spk"));
    assert_eq!(rd.channels("spk"), MeasureChannels::Left);
    rd.set_delay_us("spk", 4_000).unwrap();
    assert_eq!(rd.unmask_all(["spk"]), 1);
    assert_eq!(rd.delay_us("spk"), Some(4_000));
    assert_eq!(rd.unmask_all(["spk"]), 0, "idempotent, and never a panic");
}

/// Only a stereo block has a left and a right. Anything else is left exactly as it is:
/// masking "the second of six channels" is not what was asked for, and a mono member's
/// wire is stereo anyway (that is why the choice is offered per member and labelled).
#[test]
fn a_mask_is_a_no_op_on_a_layout_that_has_no_left_and_right() {
    let rd = RelayDelay::new();
    rd.set_channels("spk", MeasureChannels::Right);
    let mono = PcmFormat::new(48_000, 1);
    let src = vec![1u8, 2, 3, 4, 5, 6];
    let mut out = vec![0xAAu8; 3];
    assert!(!rd.delay_into("spk", mono, &src, &mut out), "mono is untouched");
    assert_eq!(out, vec![0xAAu8; 3], "and the caller's buffer is not even written");

    // A partial trailing frame is refused for the same reason the delay line refuses
    // one: it cannot be interpreted without inventing the missing samples.
    let mut out = Vec::new();
    let odd = vec![1u8, 2, 3];
    assert!(rd.delay_into("spk", F48, &odd, &mut out));
    assert_eq!(out, vec![1, 2, 3], "the whole frames in it are masked, the tail is copied as-is");

    assert_eq!(MeasureChannels::parse("L"), Some(MeasureChannels::Left));
    assert_eq!(MeasureChannels::parse("stereo"), Some(MeasureChannels::Both));
    assert_eq!(MeasureChannels::parse("centre"), None);
    assert_eq!(MeasureChannels::Right.as_str(), "right");
}
