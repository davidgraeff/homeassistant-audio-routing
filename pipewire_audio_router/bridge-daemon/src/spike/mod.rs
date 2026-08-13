//! **Where an experiment goes, and how to get one out again.**
//!
//! A *spike* is a throwaway program that answers one question about real hardware —
//! "does this receiver accept a 48 kHz SETUP?", "does the per-device sender path play at
//! all without a routed source?" — with the device in the loop and nothing else in the
//! way. This module is deliberately **empty**: the three that lived here (a sendspin
//! per-device sender, an AP2 tone with its PTP grandmaster, a native pw-sink module load)
//! had all answered their question, and the answers live in
//! `docs/` and in the production code they justified. What is left is the recipe, because
//! the *next* one should be as cheap to add — and as cheap to delete.
//!
//! ## Why they are worth having at all
//!
//! Every one of them existed because the question could not be answered from a unit test
//! or from the running daemon:
//!
//! * **the hardware disagrees with the spec** — an AirPlay receiver that refuses a 48 kHz
//!   SETUP, an ESP32 that needs 180 ms of lead where the protocol implies none;
//! * **the production path has too much in it** — asking "can we even reach this device"
//!   through routing, grouping, a sync anchor and a relay means five things can be wrong
//!   at once;
//! * **the experiment must not be reachable by accident.** A spike moves real audio to
//!   real speakers in someone's home, which is exactly why it is not wired into the UI.
//!
//! ## Adding one
//!
//! 1. **Write it as a module here** — `src/spike/<question>.rs`, and `pub(crate) mod
//!    <question>;` below. One `start(...)`/`stop()` pair, its own state in a `OnceLock`,
//!    and no reliance on routing intent or the reconciler: the point is to bypass them.
//!    Log generously at `INFO`; a spike's output *is* its result.
//! 2. **Reach it however is cheapest.** The three predecessors used
//!    `POST/DELETE /api/spike/<name>` because a `curl` from the dev machine was the
//!    shortest path to the device. That is fine while the question is open — and see the
//!    warning below about what it costs. A `#[test]`-driven spike, an `--example`, or a
//!    `main.rs` branch behind an env var are all legitimate too, and the first two do not
//!    touch the shipped surface at all.
//! 3. **Say what it is for, in the module doc**: the question, the hardware it needs, and
//!    what an answer would look like. A spike nobody can interpret six weeks later has to
//!    be run again.
//! 4. **Write the answer down where the decision lives** — the plan or design doc under
//!    `pipewire_audio_router/docs/`, next to the code it justifies. The measurements from
//!    the retired spikes are why `DEFAULT_GROUP_LEAD_MS` is 180 and why the AP2 sender
//!    probes 48 kHz before falling back; the spikes themselves are gone.
//! 5. **Delete it when it has answered.** That is the whole lifecycle, and skipping this
//!    step is how a dev harness becomes a supported interface nobody chose to support.
//!
//! ## If you expose it over HTTP, know what you are paying
//!
//! `/api/spike/*` was five routes in the shipped route table, listed in
//! `docs/api-reference.md`, carrying request types that were never validated the way
//! real endpoints are and no consumer ever called. It cost a reader of that table five
//! rows of "dev-only, not a supported interface" — and it survived two API harmonisation
//! passes before being removed for exactly that reason
//! (`docs/api-harmonisation.md` §7).
//!
//! So if the next spike wants HTTP:
//!
//! * put every route behind `#[cfg(feature = "spike")]`, and leave the feature off by
//!   default, so the shipped binary does not carry them;
//! * do **not** add them to `docs/api-reference.md`, which documents the API a consumer
//!   may rely on. This module doc is the right place;
//! * keep them under one prefix, so `grep -rn '"/api/spike' src/api/mod.rs` is the whole
//!   inventory when it is time to delete.
//!
//! ## The one thing not to do
//!
//! Do not grow a spike into a feature in place. The three here each *led* to production
//! code — per-device senders, the AP2 sender's rate negotiation, the pw-sink transport —
//! and in every case that code was written separately, with the reconciler, the restore
//! obligations and the failure handling a real path needs. A spike is allowed to leak
//! threads, ignore teardown and hard-code an IP; that is what makes it cheap, and it is
//! why it must not become the thing that ships.
