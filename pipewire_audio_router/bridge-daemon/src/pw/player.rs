//! Native announce/clip playback: streams a WAV clip to a target sink node via
//! `pw::stream`, replacing a `pw-cat --playback --target` subprocess.
//!
//! Runs a short-lived PipeWire main loop on the calling thread for the length
//! of the clip, so it's invoked from the async announce handler (api.rs) via
//! `tokio::task::spawn_blocking`. PipeWire's client types are `!Send` and must
//! stay on one thread — a dedicated blocking thread satisfies that naturally,
//! and keeps announce playback fully isolated from the long-lived registry/
//! command thread (pw/thread.rs).
//!
//! Only 16-bit PCM is handled, which is all this daemon ever produces —
//! everything converges on `audio::wav::build_wav` at 16-bit (audio/decode.rs via
//! symphonia, align/calibrate.rs for the click track).

use pipewire as pw;
use pw::spa;
use spa::pod::Pod;
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

struct Wav {
    rate: u32,
    channels: u16,
    /// Interleaved 16-bit little-endian PCM.
    pcm: Vec<u8>,
}

/// Parses a PCM WAV. Reads the `fmt ` and `data` chunks and tolerates any
/// extra chunks in between; only 16-bit PCM is accepted (what we produce).
fn parse_wav(bytes: &[u8]) -> Result<Wav, String> {
    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err("not a RIFF/WAVE file".to_string());
    }
    let mut pos = 12;
    let mut fmt: Option<(u16, u16, u32, u16)> = None; // (format, channels, rate, bits)
    let mut data: Option<Vec<u8>> = None;
    while pos + 8 <= bytes.len() {
        let id = &bytes[pos..pos + 4];
        let size = u32::from_le_bytes(bytes[pos + 4..pos + 8].try_into().unwrap()) as usize;
        let body_start = pos + 8;
        let body_end = (body_start + size).min(bytes.len());
        match id {
            b"fmt " if body_end - body_start >= 16 => {
                let b = &bytes[body_start..body_end];
                fmt = Some((
                    u16::from_le_bytes(b[0..2].try_into().unwrap()),
                    u16::from_le_bytes(b[2..4].try_into().unwrap()),
                    u32::from_le_bytes(b[4..8].try_into().unwrap()),
                    u16::from_le_bytes(b[14..16].try_into().unwrap()),
                ));
            }
            b"data" => data = Some(bytes[body_start..body_end].to_vec()),
            _ => {}
        }
        // RIFF chunks are word-aligned: an odd size has a trailing pad byte.
        pos = body_start + size + (size & 1);
    }
    let (format, channels, rate, bits) = fmt.ok_or("no fmt chunk")?;
    let pcm = data.ok_or("no data chunk")?;
    if format != 1 {
        return Err(format!("unsupported WAV format tag {format} (only PCM)"));
    }
    if bits != 16 {
        return Err(format!("unsupported WAV bit depth {bits} (only 16-bit)"));
    }
    if channels == 0 {
        return Err("WAV has zero channels".to_string());
    }
    Ok(Wav { rate, channels, pcm })
}

/// Plays a 16-bit PCM WAV clip to `target_node_id` on a continuous loop until
/// `stop` is set, blocking the calling thread until then. The clip's PCM is
/// wrapped seamlessly (end → start), so a periodic pattern (the calibration
/// click track) plays gaplessly. Intended for `tokio::task::spawn_blocking`.
pub fn play_loop_to_target(target_node_id: u32, wav_bytes: &[u8], stop: Arc<AtomicBool>) -> Result<(), String> {
    let wav = parse_wav(wav_bytes)?;
    let stride = wav.channels as usize * 2; // 16-bit => 2 bytes/sample
    if wav.pcm.len() < stride {
        return Ok(()); // nothing to play
    }

    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None).map_err(|e| format!("mainloop: {e}"))?;
    let context = pw::context::ContextRc::new(&mainloop, None).map_err(|e| format!("context: {e}"))?;
    let core = context.connect_rc(None).map_err(|e| format!("connect to PipeWire: {e}"))?;

    let stream = pw::stream::StreamBox::new(
        &core,
        "bridge-align",
        pw::properties::properties! {
            *pw::keys::MEDIA_TYPE => "Audio",
            *pw::keys::MEDIA_CATEGORY => "Playback",
            *pw::keys::MEDIA_ROLE => "Notification",
            *pw::keys::NODE_NAME => "bridge-align",
        },
    )
    .map_err(|e| format!("create stream: {e}"))?;

    let pcm = Rc::new(wav.pcm);
    let cursor = Rc::new(Cell::new(0usize));
    let error: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));

    let _listener = stream
        .add_local_listener_with_user_data(())
        .process({
            let pcm = pcm.clone();
            let cursor = cursor.clone();
            move |stream, _| {
                let Some(mut buffer) = stream.dequeue_buffer() else {
                    return;
                };
                let datas = buffer.datas_mut();
                let data = &mut datas[0];
                let filled = if let Some(slice) = data.data() {
                    let cap = (slice.len() / stride) * stride; // whole frames only
                    let mut cur = cursor.get();
                    let mut written = 0usize;
                    while written < cap {
                        if cur >= pcm.len() {
                            cur = 0; // seamless wrap to the pattern start
                        }
                        let n = (cap - written).min(pcm.len() - cur);
                        slice[written..written + n].copy_from_slice(&pcm[cur..cur + n]);
                        written += n;
                        cur += n;
                    }
                    cursor.set(cur);
                    written
                } else {
                    0
                };
                let chunk = data.chunk_mut();
                *chunk.offset_mut() = 0;
                *chunk.stride_mut() = stride as _;
                *chunk.size_mut() = filled as _;
            }
        })
        .state_changed({
            let error = error.clone();
            let mainloop = mainloop.clone();
            move |_stream, _, _old, new| {
                if let pw::stream::StreamState::Error(e) = new {
                    *error.borrow_mut() = Some(e);
                    mainloop.quit();
                }
            }
        })
        .register()
        .map_err(|e| format!("register stream listener: {e}"))?;

    let mut audio_info = spa::param::audio::AudioInfoRaw::new();
    audio_info.set_format(spa::param::audio::AudioFormat::S16LE);
    audio_info.set_rate(wav.rate);
    audio_info.set_channels(wav.channels as u32);
    if wav.channels == 2 {
        let mut position = [0; spa::param::audio::MAX_CHANNELS];
        position[0] = pw::spa::sys::SPA_AUDIO_CHANNEL_FL;
        position[1] = pw::spa::sys::SPA_AUDIO_CHANNEL_FR;
        audio_info.set_position(position);
    }

    let values: Vec<u8> = pw::spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &pw::spa::pod::Value::Object(pw::spa::pod::Object {
            type_: pw::spa::sys::SPA_TYPE_OBJECT_Format,
            id: pw::spa::sys::SPA_PARAM_EnumFormat,
            properties: audio_info.into(),
        }),
    )
    .map_err(|e| format!("serialize format pod: {e}"))?
    .0
    .into_inner();
    let mut params = [Pod::from_bytes(&values).ok_or("invalid format pod")?];

    stream
        .connect(
            spa::utils::Direction::Output,
            Some(target_node_id),
            pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS | pw::stream::StreamFlags::RT_PROCESS,
            &mut params,
        )
        .map_err(|e| format!("connect stream to node {target_node_id}: {e}"))?;

    // The loop is `!Send`, so `stop` can't call `quit()` from outside — poll it
    // on an in-loop timer (250 ms) and quit when the session ends.
    let timer = {
        let ml = mainloop.clone();
        let stop = stop.clone();
        mainloop.loop_().add_timer(move |_| {
            if stop.load(Ordering::Relaxed) {
                ml.quit();
            }
        })
    };
    let _ = timer.update_timer(Some(Duration::from_millis(250)), Some(Duration::from_millis(250)));

    mainloop.run();

    if let Some(e) = error.borrow_mut().take() {
        return Err(e);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::wav::build_wav;

    #[test]
    fn parses_our_own_wav() {
        let pcm = vec![1u8, 2, 3, 4, 5, 6, 7, 8]; // 2 stereo 16-bit frames
        let bytes = build_wav(&pcm, 44100, 16, 2);
        let w = parse_wav(&bytes).unwrap();
        assert_eq!(w.rate, 44100);
        assert_eq!(w.channels, 2);
        assert_eq!(w.pcm, pcm);
    }

    #[test]
    fn rejects_non_wav() {
        assert!(parse_wav(b"not a wav at all").is_err());
    }

    #[test]
    fn rejects_non_pcm_bit_depth() {
        let bytes = build_wav(&[0u8; 16], 22050, 24, 1);
        assert!(parse_wav(&bytes).is_err());
    }
}
