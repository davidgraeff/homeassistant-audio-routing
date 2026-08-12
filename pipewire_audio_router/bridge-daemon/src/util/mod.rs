//! Cross-cutting helpers that belong to no subsystem.
//!
//! [`locks`] is the poison-safe `Mutex` extension every shared store locks through
//! — the daemon must keep serving after a panic in some unrelated holder, not
//! cascade into a dead process. [`node_names`] holds the node-name prefixes
//! (`sendspin-dev-`, `ap2-dev-`, `pwsink-dev-`, …) that the stores, the discovery
//! modules and the output classification all have to agree on — naming
//! conventions, not configuration: the daemon reads no config file, every source
//! is created at runtime through the API and persisted in `store/`.
//! [`host_assessment`] answers "is this machine strong enough for realtime
//! multi-room audio?" once at boot.
//!
//! Nothing here may depend on another module of this crate. That is the entry
//! condition: a helper that needs the registry or a store is not a util, it is a
//! part of whatever it needs.

pub(crate) mod host_assessment;
pub(crate) mod locks;
pub(crate) mod node_names;
