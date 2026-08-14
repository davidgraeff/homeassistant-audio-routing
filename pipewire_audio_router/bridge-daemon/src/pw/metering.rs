//! On-demand peak-level metering for routing sources.
//!
//! A tiny PipeWire capture per metered node computes a peak (max |sample|) from
//! the audio it produces. Taps exist **only while a client is subscribed to the
//! `meters` topic** on `/api/events` (see `events/mod.rs`, which owns that
//! accounting) — when the last subscriber goes away, all taps are torn down, so
//! idle installs pay nothing. Note the unit is the *subscription*, not the
//! socket: a page that holds the socket open for other topics pays nothing here.
//! The matrix snapshot carries each source's current peak so the UI can draw a
//! level meter (and you can see, e.g., whether AirPlay/RTP audio is actually
//! arriving).
//!
//! Only *sources* are metered: they're real nodes with output ports we can tap.
//! (Sendspin device "outputs" are virtual — audio reaches them via a group
//! sink — so there's no per-device node to tap here.)

use crate::util::locks::LockRecover;
use pipewire as pw;
use pw::spa;
use spa::pod::Pod;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

/// One running tap. Dropping it stops the capture thread (removing its node).
struct Tap {
    peak: Arc<AtomicU32>,
    node_id: u32,
    stop: Option<pw::channel::Sender<()>>,
}

impl Drop for Tap {
    fn drop(&mut self) {
        if let Some(tx) = self.stop.take() {
            let _ = tx.send(());
        }
    }
}

#[derive(Default)]
struct Inner {
    /// Number of web clients currently watching the matrix. Taps live only
    /// while this is > 0.
    watchers: usize,
    /// node_name -> tap.
    taps: HashMap<String, Tap>,
}

/// Shared metering hub (in AppState). Reference-counts matrix watchers and owns
/// the live taps.
#[derive(Default)]
pub struct MeterHub {
    inner: Mutex<Inner>,
}

pub type SharedMeters = Arc<MeterHub>;

impl MeterHub {
    pub fn new() -> SharedMeters {
        Arc::new(Self::default())
    }

    /// A matrix WS client connected.
    pub fn watch(&self) {
        self.inner.lock_recover().watchers += 1;
    }

    /// A matrix WS client disconnected; tear down all taps when none remain.
    pub fn unwatch(&self) {
        let mut inner = self.inner.lock_recover();
        inner.watchers = inner.watchers.saturating_sub(1);
        if inner.watchers == 0 {
            inner.taps.clear(); // Drop stops each capture thread.
        }
    }

    /// Current peak (0.0–1.0) for a node, or 0.0 if it isn't metered.
    pub fn peak(&self, node_name: &str) -> f32 {
        self.inner.lock_recover().taps.get(node_name).map(|t| f32::from_bits(t.peak.load(Ordering::Relaxed))).unwrap_or(0.0)
    }

    /// Ensure exactly the given `(node_name, node_id)` sources are tapped —
    /// spawn new taps, drop taps for nodes that went away or changed id. No-op
    /// (and clears everything) when unwatched. Called by the WS handler on each
    /// matrix snapshot while a client is connected.
    pub fn reconcile_sources(&self, wanted: &[(String, u32)]) {
        let mut inner = self.inner.lock_recover();
        if inner.watchers == 0 {
            inner.taps.clear();
            return;
        }
        // Drop taps no longer wanted (gone, or the node reloaded with a new id).
        inner.taps.retain(|name, tap| wanted.iter().any(|(n, id)| n == name && *id == tap.node_id));
        // Spawn taps for newly-wanted sources.
        for (name, id) in wanted {
            if inner.taps.contains_key(name) {
                continue;
            }
            match spawn_peak_tap(*id) {
                Ok((peak, stop)) => {
                    inner.taps.insert(name.clone(), Tap { peak, node_id: *id, stop: Some(stop) });
                }
                Err(e) => tracing::warn!("peak tap for source '{name}' (id {id}) failed: {e}"),
            }
        }
    }
}

/// Spawn a capture that measures the peak of `source_node_id`'s output on a
/// dedicated thread (mirrors sendspin_capture, but computes a peak instead of
/// forwarding PCM, and taps a *source*'s output ports rather than a sink's
/// monitor). Returns the shared peak cell and a stop sender.
fn spawn_peak_tap(source_node_id: u32) -> Result<(Arc<AtomicU32>, pw::channel::Sender<()>), String> {
    let peak = Arc::new(AtomicU32::new(0));
    let (stop_tx, stop_rx) = pw::channel::channel::<()>();
    let peak_for_thread = peak.clone();
    std::thread::Builder::new()
        .name("peak-tap".into())
        .spawn(move || {
            if let Err(e) = run_tap(source_node_id, peak_for_thread, stop_rx) {
                tracing::debug!("peak tap for node {source_node_id} exited: {e}");
            }
        })
        .map_err(|e| format!("spawn peak-tap thread: {e}"))?;
    Ok((peak, stop_tx))
}

fn run_tap(source_node_id: u32, peak: Arc<AtomicU32>, stop_rx: pw::channel::Receiver<()>) -> Result<(), String> {
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None).map_err(|e| format!("mainloop: {e}"))?;
    let context = pw::context::ContextRc::new(&mainloop, None).map_err(|e| format!("context: {e}"))?;
    let core = context.connect_rc(None).map_err(|e| format!("connect to PipeWire: {e}"))?;

    let stream = pw::stream::StreamBox::new(
        &core,
        "bridge-peak-tap",
        pw::properties::properties! {
            *pw::keys::MEDIA_TYPE => "Audio",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::NODE_NAME => "bridge-peak-tap",
        },
    )
    .map_err(|e| format!("create stream: {e}"))?;

    let error: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));

    let _listener = stream
        .add_local_listener_with_user_data(())
        .process({
            let peak = peak.clone();
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
                    // Interpret as f32 samples (we requested F32LE).
                    let mut p = 0.0f32;
                    let bytes = &slice[offset..end];
                    for f in bytes.as_chunks::<4>().0 {
                        let v = f32::from_le_bytes([f[0], f[1], f[2], f[3]]).abs();
                        if v > p {
                            p = v;
                        }
                    }
                    peak.store(p.to_bits(), Ordering::Relaxed);
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

    // Request F32LE so peak is a trivial read; PipeWire converts as needed. No
    // fixed rate/channels — capture whatever the source produces.
    let mut audio_info = spa::param::audio::AudioInfoRaw::new();
    audio_info.set_format(spa::param::audio::AudioFormat::F32LE);
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

    // Direction::Input capturing the source's output ports (no
    // STREAM_CAPTURE_SINK — that's for tapping a sink's monitor).
    stream
        .connect(
            spa::utils::Direction::Input,
            Some(source_node_id),
            pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
            &mut params,
        )
        .map_err(|e| format!("connect peak tap to node {source_node_id}: {e}"))?;

    let mainloop_for_cmd = mainloop.clone();
    let _cmd = stop_rx.attach(mainloop.loop_(), move |()| mainloop_for_cmd.quit());

    mainloop.run();

    if let Some(e) = error.borrow_mut().take() {
        return Err(e);
    }
    Ok(())
}
