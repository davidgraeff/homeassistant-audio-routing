//! Runtime on/off for mDNS auto-discovery (RAOP + sendspin).
//!
//! Both `ServiceDaemon` handles live here behind a shared mutex so discovery is
//! toggleable from the Settings page: `start()` spawns the two discovery threads
//! (discovery.rs + sendspin_discovery.rs), `stop()` drops the handles — which
//! disconnects each thread's mDNS receiver, so both loops exit cleanly (RAOP
//! breaks on `receiver.is_disconnected()`, sendspin ends its `while let Ok(..)`).
//!
//! Disabling only stops discovering *new* devices; anything already present is
//! left to age out through its normal path (RAOP via the absent-grace probe,
//! sendspin via liveness) so a toggle never tears down live groups.

use crate::api::{SharedSources, SharedStore};
use crate::discovery;
use crate::locks::LockRecover;
use crate::pw_thread::{ChangeNotifier, PwCommandSender};
use crate::sendspin_discovery::{self, SharedSendspinDevices};
use crate::sync_settings::SharedSyncSettings;
use mdns_sd::ServiceDaemon;
use std::sync::{Arc, Mutex};

struct Inner {
    /// The live RAOP + sendspin discovery daemons while running; `None` when off.
    running: Option<(ServiceDaemon, ServiceDaemon)>,
    // Inputs kept so discovery can be (re)spawned on demand.
    pw_cmd: PwCommandSender,
    store: SharedStore,
    sources: SharedSources,
    mode: discovery::Mode,
    devices: SharedSendspinDevices,
    changes: ChangeNotifier,
    /// Per-output RAOP latency overrides (sync_settings.rs) applied by RAOP
    /// discovery when it loads/reloads a receiver.
    sync_settings: SharedSyncSettings,
    /// Resolved connection details of loaded discovered receivers, surfaced to
    /// the API (discovery.rs::SharedDiscovered).
    discovered: discovery::SharedDiscovered,
}

#[derive(Clone)]
pub struct DiscoverySupervisor(Arc<Mutex<Inner>>);

impl DiscoverySupervisor {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        pw_cmd: PwCommandSender,
        store: SharedStore,
        sources: SharedSources,
        mode: discovery::Mode,
        devices: SharedSendspinDevices,
        changes: ChangeNotifier,
        sync_settings: SharedSyncSettings,
        discovered: discovery::SharedDiscovered,
    ) -> Self {
        Self(Arc::new(Mutex::new(Inner { running: None, pw_cmd, store, sources, mode, devices, changes, sync_settings, discovered })))
    }

    pub fn is_running(&self) -> bool {
        self.0.lock_recover().running.is_some()
    }

    /// Spawn both discovery threads if not already running. Idempotent.
    pub fn start(&self) -> anyhow::Result<()> {
        let mut inner = self.0.lock_recover();
        if inner.running.is_some() {
            return Ok(());
        }
        let raop = discovery::spawn(
            inner.pw_cmd.clone(),
            inner.store.clone(),
            inner.sources.clone(),
            inner.mode,
            inner.sync_settings.clone(),
            inner.discovered.clone(),
        )?;
        let sendspin = sendspin_discovery::spawn(inner.devices.clone(), inner.changes.clone())?;
        inner.running = Some((raop, sendspin));
        tracing::info!("mDNS discovery started ({:?})", inner.mode);
        Ok(())
    }

    /// Drop both discovery daemons, stopping the mDNS threads. Idempotent.
    pub fn stop(&self) {
        let mut inner = self.0.lock_recover();
        if inner.running.take().is_some() {
            tracing::info!("mDNS discovery stopped");
        }
    }

    /// Apply a desired on/off state; returns any spawn error from `start()`.
    pub fn set_enabled(&self, enabled: bool) -> anyhow::Result<()> {
        if enabled {
            self.start()
        } else {
            self.stop();
            Ok(())
        }
    }
}
