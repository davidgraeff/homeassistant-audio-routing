//! Poison-safe mutex locking.
//!
//! The shared registry snapshot (pw/thread.rs) and the outputs store (api/outputs.rs)
//! are plain data behind a `std::sync::Mutex`. With `lock().unwrap()`, a panic
//! in *any* holder while the lock is held poisons the mutex, and every later
//! `lock().unwrap()` then panics too — one transient bug cascading into a fully
//! dead daemon (no registry reads, no API, no discovery). Recovering the guard
//! from a poisoned lock keeps the daemon serving instead: the registry state is
//! a rebuildable snapshot and the store is file-backed, so a half-finished
//! update is at worst momentarily stale, not corrupting.

use std::sync::{Mutex, MutexGuard};

pub trait LockRecover<T> {
    /// Locks the mutex, recovering the guard even if it was poisoned by a
    /// panic in a previous holder (instead of propagating the panic).
    fn lock_recover(&self) -> MutexGuard<'_, T>;
}

impl<T> LockRecover<T> for Mutex<T> {
    fn lock_recover(&self) -> MutexGuard<'_, T> {
        self.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}
