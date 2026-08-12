//! PipeWire **Profiler** subscription — per-node xrun counts for the routing UI.
//!
//! This is the same data `pw-top`'s `ERR` column shows: the `module-profiler`
//! global emits a `profile` POD once per driver cycle, carrying a driver block
//! plus one follower block per node, each with a cumulative `xrun_count`. We
//! bind that global and keep a `node_name -> xrun_count` snapshot the routing
//! matrix reads (routing/mod.rs), so the graph can badge a node that's dropping
//! frames — the "where is the stutter" signal.
//!
//! `pipewire-rs` 0.10 has no typed `Profiler` proxy and the `profile` event
//! isn't in its safe API, so this binds the global by id via the raw registry
//! `bind` method and attaches a raw `pw_profiler_events` object listener. All of
//! it runs on the PipeWire thread (pw/thread.rs) — the proxy/hook are `!Send`
//! and only ever touched there. The profile callback fires on that thread's main
//! loop (not the RT data-loop), so writing the shared map under a std `Mutex` is
//! fine.
//!
//! COST: bound only while a routing-matrix WebSocket is open (pw_thread toggles
//! it on `PwCommand::SetProfiling`, driven by the same watcher count as the peak
//! meters). With no listener attached, `module-profiler` has nothing to emit to,
//! so an idle install with the UI closed pays nothing. While open it's one POD
//! parse per driver cycle on the main loop.

use crate::util::locks::LockRecover;
use pipewire as pw;
use pw::spa::pod::deserialize::PodDeserializer;
use pw::spa::pod::Value;
use std::collections::HashMap;
use std::os::raw::c_void;
use std::pin::Pin;
use std::ptr::NonNull;
use std::sync::{Arc, Mutex};

/// `node_name -> latest cumulative xrun count`, as last reported by the
/// profiler. Replaced wholesale each `profile` tick (the profiler reports every
/// active node every cycle, so a node absent from a tick has simply gone away).
/// Shared with the routing snapshot; read in build_matrix.
pub type SharedXruns = Arc<Mutex<HashMap<String, u32>>>;

pub fn new_xruns() -> SharedXruns {
    Arc::new(Mutex::new(HashMap::new()))
}

/// Context handed to the C callback as its `data` pointer. Heap-boxed and kept
/// alive by [`ProfilerListener`] for exactly as long as the listener is armed.
struct ProfilerCtx {
    xruns: SharedXruns,
}

/// A live profiler subscription: the bound proxy, its object-listener hook, and
/// the boxed events/context the hook points at. Dropping it removes the hook
/// (stopping callbacks and, if we were the only listener, profiler emission)
/// then destroys the proxy. Created and dropped only on the PipeWire thread.
pub struct ProfilerListener {
    proxy: NonNull<pw::sys::pw_proxy>,
    // The hook is an intrusive list node libpipewire keeps a pointer to, so it
    // must not move; pinned. Declared before the boxes it references only for
    // readability — teardown order is handled explicitly in `Drop`.
    hook: Pin<Box<pw::spa::sys::spa_hook>>,
    _events: Box<pw::sys::pw_profiler_events>,
    _ctx: Box<ProfilerCtx>,
}

impl Drop for ProfilerListener {
    fn drop(&mut self) {
        // SAFETY: on the PipeWire thread; hook was registered against this proxy
        // with the boxed events/ctx still alive here. Remove the hook first so
        // no further callback can fire, then destroy the proxy. The boxes drop
        // after this returns — still valid throughout `spa_hook_remove`.
        unsafe {
            pw::spa::sys::spa_hook_remove(self.hook.as_mut().get_unchecked_mut());
            pw::sys::pw_proxy_destroy(self.proxy.as_ptr());
        }
    }
}

/// Bind the profiler global `profiler_id` and start feeding `xruns`. Returns
/// `None` if libpipewire refuses the bind. MUST be called on the PipeWire
/// thread (raw proxy work); `registry` is that thread's registry.
pub fn subscribe(registry: &pw::registry::RegistryRc, profiler_id: u32, xruns: SharedXruns) -> Option<ProfilerListener> {
    // SAFETY: replicates pipewire-rs's own `Registry::bind` (which we can't use:
    // no `ProxyT` for Profiler), calling the registry interface's `bind` method
    // to get a raw `pw_proxy` for the profiler global, then attaching a
    // `pw_profiler_events` object listener. `registry.as_raw_ptr()` is the live
    // registry on this thread; all pointers passed to libpipewire (events, ctx,
    // hook) are boxed and owned by the returned `ProfilerListener` for the
    // listener's whole lifetime.
    unsafe {
        let type_ = std::ffi::CString::new(pw::types::ObjectType::Profiler.to_str()).ok()?;
        let proxy = pw::spa::spa_interface_call_method!(
            registry.as_raw_ptr(),
            pw::sys::pw_registry_methods,
            bind,
            profiler_id,
            type_.as_ptr(),
            pw::sys::PW_VERSION_PROFILER,
            0
        );
        let proxy = NonNull::new(proxy.cast::<pw::sys::pw_proxy>())?;

        let mut ctx = Box::new(ProfilerCtx { xruns });
        let mut events: Box<pw::sys::pw_profiler_events> = Box::new(std::mem::zeroed());
        events.version = pw::sys::PW_VERSION_PROFILER_EVENTS;
        events.profile = Some(on_profile);
        let mut hook: Pin<Box<pw::spa::sys::spa_hook>> = Box::pin(std::mem::zeroed());

        pw::sys::pw_proxy_add_object_listener(
            proxy.as_ptr(),
            hook.as_mut().get_unchecked_mut() as *mut pw::spa::sys::spa_hook,
            (&*events as *const pw::sys::pw_profiler_events).cast::<c_void>(),
            (&mut *ctx as *mut ProfilerCtx).cast::<c_void>(),
        );

        Some(ProfilerListener { proxy, hook, _events: events, _ctx: ctx })
    }
}

/// The `profile` event callback. Parses the Profiler POD into a fresh
/// `node_name -> xrun_count` map and publishes it.
unsafe extern "C" fn on_profile(data: *mut c_void, pod: *const pw::spa::sys::spa_pod) {
    if data.is_null() || pod.is_null() {
        return;
    }
    // SAFETY: `data` is the `ProfilerCtx` we boxed in `subscribe`, alive for the
    // listener's lifetime; `pod` is a valid Profiler POD for this call. Its byte
    // length is the 8-byte header plus the declared body `size`.
    let ctx = &*(data as *const ProfilerCtx);
    let total = std::mem::size_of::<pw::spa::sys::spa_pod>() + (*pod).size as usize;
    let bytes = std::slice::from_raw_parts(pod as *const u8, total);

    let Ok((_, Value::Object(obj))) = PodDeserializer::deserialize_from::<Value>(bytes) else {
        return;
    };

    let mut map: HashMap<String, u32> = HashMap::new();
    for prop in &obj.properties {
        // Driver block + one follower block per node; both carry the same field
        // layout ending in xrun_count.
        if prop.key != pw::spa::sys::SPA_PROFILER_driverBlock && prop.key != pw::spa::sys::SPA_PROFILER_followerBlock {
            continue;
        }
        let Value::Struct(fields) = &prop.value else { continue };
        // Block struct layout (module-profiler / pw-top): [0]=id(Int)
        // [1]=name(String) [2..6]=Long [6]=status(Int) [7]=latency(Fraction)
        // [8]=xrun_count(Int). Index by position but bounds-check, so a future
        // layout change degrades to "no xrun shown" rather than a wrong number.
        let Some(Value::String(name)) = fields.get(1) else { continue };
        let Some(Value::Int(xruns)) = fields.get(8) else { continue };
        map.insert(name.clone(), (*xruns).max(0) as u32);
    }
    *ctx.xruns.lock_recover() = map;
}
