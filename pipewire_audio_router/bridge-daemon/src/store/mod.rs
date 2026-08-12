//! The daemon's persisted state: four JSON files under `/data`, and the one-shot
//! migration that rewrites them.
//!
//! [`routing`] is the routing intent (links by stable node name, reconciled onto
//! the live graph), [`outputs`] is which discovered devices the user actually
//! adopted — the gate that keeps a newly appeared speaker out of the matrix and
//! out of Home Assistant until it is added — [`groups`] the named music and
//! announcement groups, [`settings`] the daemon-wide knobs behind the Settings
//! page. [`migration`] is the drop-raop rewrite that runs before any of them is
//! read.
//!
//! The entry condition, and what makes this directory more than a filing choice:
//! **a store may depend on nothing but [`crate::util`] and its siblings here.** No
//! registry, no senders, no discovery. A module that needs those is not a store,
//! it is a supervisor that persists — which is why `sources/mod.rs` is *not*
//! here (it reconciles live AirPlay and RTP handles) and neither is
//! `routing/sync_settings.rs` (it pushes settings into the senders on write). Keeping the
//! rule means these files can be read, tested and reasoned about without the rest
//! of the daemon.

pub(crate) mod groups;
pub(crate) mod migration;
pub(crate) mod outputs;
pub(crate) mod routing;
pub(crate) mod settings;
