//! Load/unload PipeWire modules into *this process's own* `pw_context` at
//! runtime.
//!
//! Used by the daemon for its `rtp-sink`/`rtp-source`/`raop-sink` outputs and by
//! the agent for the `rtp-session` receiver — in both cases loaded into the
//! process's own context, so the nodes are ordinary graph nodes whose lifetime is
//! the process's.
//!
//! `pipewire-rs` 0.10 doesn't wrap `pw_context_load_module` — it lives in
//! `pipewire/impl-module.h`, which the crate's bindgen `wrapper.h`
//! (`pipewire/pipewire.h`) doesn't pull in — so we declare the two C functions
//! ourselves. `libpipewire` is already linked by `pipewire-sys` (its build.rs
//! links it via pkg-config), so these symbols resolve at final link time
//! without any extra `#[link]` on our side.
//!
//! THREADING (important): a `pw_context` is not thread-safe and is owned by the
//! thread running the loop. Everything here must be called on that thread.
//! `LoadedModule` holds a raw pointer and is therefore `!Send`, so the compiler
//! prevents it from escaping that thread — including its `Drop`, which calls
//! back into libpipewire.

use pipewire::sys::pw_context;
use std::ffi::{c_char, c_void, CString};

// Opaque handle; we only ever hold the pointer and later hand it back to
// pw_impl_module_destroy — never dereference it.
#[repr(C)]
struct pw_impl_module {
    _opaque: [u8; 0],
}

extern "C" {
    /// `struct pw_impl_module *pw_context_load_module(struct pw_context *,
    /// const char *name, const char *args, struct pw_properties *)`.
    fn pw_context_load_module(
        context: *mut pw_context,
        name: *const c_char,
        args: *const c_char,
        properties: *mut c_void,
    ) -> *mut pw_impl_module;
    /// `void pw_impl_module_destroy(struct pw_impl_module *)`.
    fn pw_impl_module_destroy(module: *mut pw_impl_module);
}

/// A module loaded into our own context. Dropping it unloads the module and
/// destroys every node it created — which is how removing a RAOP output makes its
/// sink node disappear from the daemon's graph, and how the agent's receive stream
/// disappears from the host's graph the moment the agent exits: live, with no
/// PipeWire restart and no stale config.
pub struct LoadedModule {
    ptr: *mut pw_impl_module,
}

impl LoadedModule {
    /// Load `name` (e.g. `libpipewire-module-rtp-session`) with `args` (an
    /// SPA-JSON object string) into `context`.
    ///
    /// # Safety
    /// `context` must be a live `pw_context` owned by the current thread, and
    /// this must be called on that thread. `NUL` bytes in `name`/`args` are
    /// rejected rather than passed to C.
    pub unsafe fn load(context: *mut pw_context, name: &str, args: &str) -> Result<Self, String> {
        let c_name = CString::new(name).map_err(|_| "module name contains a NUL byte".to_string())?;
        let c_args = CString::new(args).map_err(|_| "module args contain a NUL byte".to_string())?;
        // The module takes ownership of nothing we pass; passing NULL props lets
        // it build its own from `args`, matching config-file loading.
        let ptr = pw_context_load_module(context, c_name.as_ptr(), c_args.as_ptr(), std::ptr::null_mut());
        if ptr.is_null() {
            Err(format!("pw_context_load_module returned NULL for {name}"))
        } else {
            Ok(LoadedModule { ptr })
        }
    }
}

impl Drop for LoadedModule {
    fn drop(&mut self) {
        // Only ever constructed and dropped on the loop thread (LoadedModule is
        // !Send via the raw pointer), so this is safe to call here.
        unsafe { pw_impl_module_destroy(self.ptr) }
    }
}
