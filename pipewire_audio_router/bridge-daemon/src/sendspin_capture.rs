//! Native, long-lived PCM capture from a sendspin output's sink node —
//! replaces adapter.py's continuous `pw-record --target <node> ... -` subprocess.
//!
//! Mirrors player.rs's stream setup (same crate APIs, same format-negotiation
//! pod-building) with the differences long-lived capture needs: `Direction::Input`
//! with the `process` callback *reading* captured bytes instead of writing them,
//! no drain-and-quit (this runs until told to stop), and a `pw::channel`-based
//! stop command — the same cross-thread-into-the-mainloop mechanism pw_thread.rs
//! already uses for `PwCommand`, since PipeWire's `!Send` types mean an external
//! thread can't just call `mainloop.quit()` directly.
//!
//! Runs on its own dedicated OS thread (matching pw_thread.rs's own
//! `std::thread::Builder::spawn`, not `tokio::task::spawn_blocking` — this is
//! long-lived, not one bounded task) and forwards each captured buffer's bytes
//! through a `tokio::sync::mpsc::UnboundedSender` (safe to call from any thread,
//! no runtime needed) to whatever async code wants the PCM (sendspin_server.rs).

use pipewire as pw;
use pw::spa;
use spa::pod::Pod;
use std::cell::RefCell;
use std::rc::Rc;
use tokio::sync::mpsc::UnboundedSender;

/// Fixed to match what this daemon has always produced for sendspin (adapter.py's
/// SAMPLE_RATE/CHANNELS/BIT_DEPTH constants) — not derived from the sink node,
/// since `support.null-audio-sink` doesn't itself constrain callers to one rate.
pub const SAMPLE_RATE: u32 = 48_000;
pub const CHANNELS: u16 = 2;

enum CaptureCmd {
    Stop,
}

/// Handle to a running capture thread. Dropping this stops it (best-effort;
/// the thread is given a chance to exit cleanly but isn't waited on, since
/// `Drop` can't be async and the thread can't outlive the sink node it reads
/// from for more than a fraction of a second in practice).
pub struct CaptureHandle {
    cmd_tx: Option<pw::channel::Sender<CaptureCmd>>,
}

impl CaptureHandle {
    /// Stop the capture thread. Idempotent — a second call is a no-op.
    pub fn stop(&mut self) {
        if let Some(tx) = self.cmd_tx.take() {
            let _ = tx.send(CaptureCmd::Stop);
        }
    }
}

impl Drop for CaptureHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Starts capturing PCM from `target_node_id` (a sink node's monitor ports —
/// see `STREAM_CAPTURE_SINK` below) on a dedicated thread. Returns immediately;
/// captured chunks arrive on the returned receiver as they're delivered by
/// PipeWire's own graph clock — nothing here paces or buffers them further.
pub fn spawn(target_node_id: u32) -> Result<(CaptureHandle, tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>), String> {
    let (pcm_tx, pcm_rx) = tokio::sync::mpsc::unbounded_channel();
    let (cmd_tx, cmd_rx) = pw::channel::channel::<CaptureCmd>();

    std::thread::Builder::new()
        .name("sendspin-capture".into())
        .spawn(move || {
            if let Err(e) = run(target_node_id, pcm_tx, cmd_rx) {
                tracing::error!("sendspin capture thread for node {target_node_id} exited with error: {e}");
            }
        })
        .map_err(|e| format!("failed to spawn capture thread: {e}"))?;

    Ok((
        CaptureHandle {
            cmd_tx: Some(cmd_tx),
        },
        pcm_rx,
    ))
}

fn run(
    target_node_id: u32,
    pcm_tx: UnboundedSender<Vec<u8>>,
    cmd_rx: pw::channel::Receiver<CaptureCmd>,
) -> Result<(), String> {
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None).map_err(|e| format!("mainloop: {e}"))?;
    let context = pw::context::ContextRc::new(&mainloop, None).map_err(|e| format!("context: {e}"))?;
    let core = context.connect_rc(None).map_err(|e| format!("connect to PipeWire: {e}"))?;

    let stream = pw::stream::StreamBox::new(
        &core,
        "bridge-sendspin-capture",
        pw::properties::properties! {
            *pw::keys::MEDIA_TYPE => "Audio",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::NODE_NAME => "bridge-sendspin-capture",
            // Tells the session manager to connect this capture stream to the
            // target's *monitor* ports rather than its (nonexistent, for a
            // plain sink) regular output ports — exactly what `pw-record
            // --target <sink>` relies on internally.
            *pw::keys::STREAM_CAPTURE_SINK => "true",
        },
    )
    .map_err(|e| format!("create stream: {e}"))?;

    let error: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));

    let _listener = stream
        .add_local_listener_with_user_data(())
        .process({
            let pcm_tx = pcm_tx.clone();
            move |stream, _| {
                let Some(mut buffer) = stream.dequeue_buffer() else {
                    return;
                };
                let datas = buffer.datas_mut();
                let Some(data) = datas.first_mut() else {
                    return;
                };
                let offset = data.chunk().offset() as usize;
                let size = data.chunk().size() as usize;
                if let Some(slice) = data.data() {
                    let end = (offset + size).min(slice.len());
                    if end > offset {
                        // Send failing just means the receiving end (the
                        // sendspin server task for this output) is gone —
                        // nothing to do here but drop the chunk; the app
                        // side is responsible for calling CaptureHandle::stop
                        // once it no longer wants audio.
                        let _ = pcm_tx.send(slice[offset..end].to_vec());
                    }
                }
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

    // Format: S16LE at the fixed rate/channels every sendspin output uses.
    let mut audio_info = spa::param::audio::AudioInfoRaw::new();
    audio_info.set_format(spa::param::audio::AudioFormat::S16LE);
    audio_info.set_rate(SAMPLE_RATE);
    audio_info.set_channels(CHANNELS as u32);
    let mut position = [0; spa::param::audio::MAX_CHANNELS];
    position[0] = pw::spa::sys::SPA_AUDIO_CHANNEL_FL;
    position[1] = pw::spa::sys::SPA_AUDIO_CHANNEL_FR;
    audio_info.set_position(position);

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
            spa::utils::Direction::Input,
            Some(target_node_id),
            pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS | pw::stream::StreamFlags::RT_PROCESS,
            &mut params,
        )
        .map_err(|e| format!("connect stream to node {target_node_id}: {e}"))?;

    let mainloop_for_cmd = mainloop.clone();
    let _cmd_receiver = cmd_rx.attach(mainloop.loop_(), move |cmd| match cmd {
        CaptureCmd::Stop => mainloop_for_cmd.quit(),
    });

    tracing::info!("sendspin capture connected to node {target_node_id}");
    // Steady-state for as long as this output is configured, not a one-shot
    // roundtrip — stopped externally via CaptureHandle::stop, or on error.
    mainloop.run();

    if let Some(e) = error.borrow_mut().take() {
        return Err(e);
    }
    Ok(())
}
