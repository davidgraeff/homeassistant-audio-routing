//! Tests for the measurement run, one module per subject.
//!
//! The subjects mirror the pieces of the run itself — the lock gate, the interval
//! model, the solver, the pre-flight check, orchestration, the near-field walk,
//! multi-position chaining, what a parked run owes the room and the record, and the
//! relay-vs-device experiment — so a change to one of them has one obvious place to
//! be tested.

mod harness;

mod chain;
mod equivalence;
mod gate;
mod knobs;
mod parked;
mod run;
mod signal;
mod solve;
mod split;
mod walk;
