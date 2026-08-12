//! The persisted transcript of one alignment run (docs/mic-alignment-plan.md §11).
//!
//! ## Why this exists at all
//!
//! A run's detail used to live in exactly two places, both of which are gone by the
//! time anyone wants it: `tracing` output (rotated out of the journal within hours on
//! the Pi) and the in-memory [`crate::align::measure::MeasureStatus`], which the
//! *next* run overwrites. The first live multi-position run made that untenable —
//! speakers had to be brought online by hand, the run refused, and there was nothing
//! left to look at afterwards. So every run now writes an **append-only** transcript
//! that can be fetched as one document days later, without the UI and without the
//! journal.
//!
//! **Failures are the point, not the side case.** A run that never got a speaker
//! rendering must produce a transcript that says so: the gate acquisitions, their
//! reasons, the restarts, and the refusal that ended it. A run that succeeded is the
//! cheap case.
//!
//! ## What the shape is, and why it is not a typed enum
//!
//! One event shape ([`Event`]): a monotonic `seq`, a millisecond offset from the start
//! of the run, a stable machine-readable `kind`, an optional `member`, one sentence of
//! prose, and a `detail` object that is **the run's own type, serialised verbatim**
//! (a `Refusal`, a `Proposal`, a `TransitivityCheck`, a gate progress…). That is a
//! deliberate choice over a typed event enum:
//!
//! * the transcript is forensic, not a contract a client computes with — a reader
//!   wants the numbers the run actually held, not a projection of them;
//! * a typed enum would need `Deserialize` on every measurement type it embeds
//!   (they are `Serialize`-only, by design — nothing reads a proposal back), and the
//!   read path here deliberately parses back to `serde_json::Value` so that adding a
//!   field to a measurement type can never break reading *old* transcripts.
//!
//! ## Bounds, because `/data` is a USB stick with no TRIM
//!
//! Three independent bounds, all of which fail *closed* (stop writing, say so in the
//! file) rather than filling the disk:
//!
//! * one **line per event, appended** with a single `write` syscall — never a
//!   read-modify-write of the whole file, which is what would wear the stick;
//! * per-run caps ([`MAX_EVENTS`], [`MAX_BYTES`]); crossing either writes one final
//!   `truncated` line and closes the file, so a wedged run cannot grow without end;
//! * per-directory retention ([`MAX_RUNS`]), applied when a run *begins*, so the
//!   oldest transcript is dropped before a new one is created rather than after.

use serde::Serialize;
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

/// How many run transcripts are kept. The oldest is dropped when a new run begins.
///
/// Six rather than one because the interesting question is usually comparative —
/// "it worked yesterday and refuses today" — and because a user debugging a bad
/// speaker retries several times in a row, which would otherwise evict the very run
/// they wanted to look at.
pub const MAX_RUNS: usize = 6;

/// Most events one run may record before the transcript closes itself.
///
/// A real run records tens to low hundreds (a gate acquisition per member per pass, a
/// measurement each, the proposal, the writes, the verification). Four thousand is
/// therefore "something is looping" rather than "a long run", and the useful response
/// to a loop is to keep the beginning — where the cause is — and stop.
pub const MAX_EVENTS: u64 = 4_000;

/// Most bytes one run may occupy. Belt to [`MAX_EVENTS`]'s braces: one event
/// embedding a large `detail` cannot be bounded by counting events alone.
pub const MAX_BYTES: u64 = 512 * 1024;

/// One line of a transcript.
///
/// `kind` is the stable part — a reader (or a future UI) matches on it — and `detail`
/// is whatever the run held at that moment, serialised as-is. `message` is written
/// for a human reading the file with no other context.
#[derive(Debug, Clone, Serialize)]
pub struct Event {
    /// Monotonic within the run, so a reader can tell a lost line from a reordered one.
    pub seq: u64,
    /// Milliseconds since the run began.
    pub at_ms: u64,
    /// Machine-readable tag. Every kind a run can write, in the order a successful one
    /// writes them:
    ///
    /// | kind | `detail` | when |
    /// |---|---|---|
    /// | `run_started` | mode, chained, sources, sample rate, members with their current delay **and** band-split calibration | once, first |
    /// | `phase` | `{phase}` | every state transition — the spine of the file |
    /// | `gate_restart` | the gate's progress, including *why* it restarted | the accumulated window was dropped and re-acquired |
    /// | `gate_failed` | the refusal plus the gate's progress | the gate gave up (timeout, capture gone, member silent) |
    /// | `measurement` | the observation, the gate, the level, the cross-band split and the calibration applied to it | one per accepted reading |
    /// | `set_restart` | attempt, limit, new grid epoch, the refusal that caused it | the capture reconnected and the whole set is retaken |
    /// | `interference` | the `Interference` report | per occurrence, unlike the warning, which de-duplicates |
    /// | `warning` | the `Warning` | once per warning kind |
    /// | `proposal` | the whole `Proposal`: knobs, checks, per-member splits, and the refusal blocking it if any | once per solve |
    /// | `apply` | the proposal being applied | the user pressed apply |
    /// | `write` | kind, from, to, **and the endpoint's verbatim reply** | one per member actually written |
    /// | `write_failed` | kind, from, to, the error | a write was refused by its endpoint |
    /// | `verification` | the whole `Verification` | once, after settling |
    /// | `split_calibrated` | the reading a band-split calibration stored | a calibration was measured while this run was parked |
    /// | `refusal` | the `Refusal` | the run refused |
    /// | `run_finished` | `{phase}` | once, when the run reaches a verdict |
    /// | `abandoned` | the provisional lines dropped and the delays already written | the user abandoned; can follow a `run_finished` |
    /// | `silence_failed` | – | the parked run could not silence the group |
    /// | `truncated` | – | a bound was hit; nothing after it was recorded |
    ///
    /// A reader may also see `unparseable` (a line torn by a kill) or `unserialisable`
    /// (a `detail` that failed to serialise) — both are the file being honest about
    /// itself rather than events a run emits.
    pub kind: &'static str,
    /// The member this concerns, when it concerns one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub member: Option<String>,
    /// One sentence, readable on its own.
    pub message: String,
    /// The run's own numbers, verbatim.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<serde_json::Value>,
}

impl Event {
    pub fn new(kind: &'static str, message: impl Into<String>) -> Self {
        Self { seq: 0, at_ms: 0, kind, member: None, message: message.into(), detail: None }
    }

    pub fn for_member(kind: &'static str, member: impl Into<String>, message: impl Into<String>) -> Self {
        Self { seq: 0, at_ms: 0, kind, member: Some(member.into()), message: message.into(), detail: None }
    }

    /// Attach the run's own value. A value that cannot be serialised is recorded as
    /// the serde error rather than dropping the event: the event's *existence* is
    /// usually the load-bearing part.
    pub fn detail<T: Serialize>(mut self, detail: &T) -> Self {
        self.detail = Some(serde_json::to_value(detail).unwrap_or_else(|e| serde_json::json!({ "unserialisable": e.to_string() })));
        self
    }
}

/// The append-only file one run writes to.
///
/// Cloneable by `Arc` and safe to record into from anywhere, including while the
/// measurement state lock is held: one small `write` on an already-open append-mode
/// handle, no buffering (so a killed daemon still leaves everything up to the last
/// event), and no seek.
pub struct RunLog {
    id: String,
    started: Instant,
    /// `None` once the log is closed — either because it was never enabled (no
    /// `/data`, or a unit test) or because a bound was hit.
    file: Mutex<Option<File>>,
    seq: AtomicU64,
    bytes: AtomicU64,
}

impl RunLog {
    /// A log that records nothing: no transcript directory configured, or a test.
    pub fn disabled() -> Arc<Self> {
        Arc::new(Self {
            id: String::new(),
            started: Instant::now(),
            file: Mutex::new(None),
            seq: AtomicU64::new(0),
            bytes: AtomicU64::new(0),
        })
    }

    /// The run's identifier, and the `{id}` a fetch addresses. Empty when disabled.
    ///
    /// Read by the tests and by anything that wants to point a user at the transcript
    /// of the run they are looking at; the daemon itself never needs to name a run,
    /// because `?run=latest` is what a client actually asks for.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Whether anything is actually being written (false for [`Self::disabled`] and
    /// after a bound closed the file). The bounds' own observable, so they are tested
    /// rather than asserted about.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn is_recording(&self) -> bool {
        self.file.lock().unwrap_or_else(std::sync::PoisonError::into_inner).is_some()
    }

    /// Append one event. Never fails and never panics: a transcript that cannot be
    /// written must not take a measurement down with it, so an IO error closes the
    /// log and is reported once through `tracing`.
    pub fn record(&self, mut ev: Event) {
        let mut guard = self.file.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(file) = guard.as_mut() else { return };
        ev.seq = self.seq.fetch_add(1, Ordering::Relaxed);
        ev.at_ms = self.started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
        let mut line = match serde_json::to_string(&ev) {
            Ok(s) => s,
            Err(e) => format!("{{\"kind\":\"unserialisable\",\"seq\":{},\"message\":{:?}}}", ev.seq, e.to_string()),
        };
        line.push('\n');
        let bytes = self.bytes.fetch_add(line.len() as u64, Ordering::Relaxed) + line.len() as u64;
        let over = bytes > MAX_BYTES || ev.seq + 1 >= MAX_EVENTS;
        if let Err(e) = file.write_all(line.as_bytes()) {
            tracing::warn!("alignment transcript {}: {e}; no more of this run is recorded", self.id);
            *guard = None;
            return;
        }
        if over {
            // The bound is reported *in* the transcript, because a truncated file that
            // does not say it was truncated reads as a run that stopped for no reason.
            let note = format!(
                "{{\"seq\":{},\"at_ms\":{},\"kind\":\"truncated\",\"message\":\"this run hit the transcript bound ({MAX_EVENTS} events / \
                 {MAX_BYTES} bytes) and nothing after this point was recorded\"}}\n",
                ev.seq + 1,
                ev.at_ms
            );
            let _ = file.write_all(note.as_bytes());
            *guard = None;
        }
    }
}

/// A run transcript as the API lists it: enough to choose one without fetching it.
#[derive(Debug, Clone, Serialize)]
pub struct RunSummary {
    pub id: String,
    /// Unix seconds, from the file's own first event.
    pub started_unix: u64,
    pub events: usize,
    pub size_bytes: u64,
    /// The `kind` of the last event — `run_finished` for a complete transcript, or
    /// whatever the run was doing when the daemon died.
    pub last_kind: String,
    /// The last event's sentence: for a finished run, the verdict.
    pub last_message: String,
    /// The first event's `detail` (a `run_started` header: mode, members, delays).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started: Option<serde_json::Value>,
}

/// One whole transcript, as one document.
#[derive(Debug, Clone, Serialize)]
pub struct RunDocument {
    pub id: String,
    pub started_unix: u64,
    pub events: Vec<serde_json::Value>,
    /// True when the file ended with a `truncated` marker.
    pub truncated: bool,
}

/// The transcript directory: retention, creation, listing and reading.
pub struct Transcripts {
    /// `None` disables everything — used by unit tests and by any deployment with no
    /// writable `/data`, so a missing directory degrades to "no transcripts" instead
    /// of failing runs.
    dir: Option<PathBuf>,
}

impl Transcripts {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: Some(dir.into()) }
    }

    pub fn disabled() -> Self {
        Self { dir: None }
    }

    pub fn dir(&self) -> Option<&Path> {
        self.dir.as_deref()
    }

    /// Start a transcript. Applies retention first, so the disk high-water mark is
    /// `MAX_RUNS` files rather than `MAX_RUNS + 1`.
    ///
    /// Infallible by construction: if the directory cannot be created or the file
    /// cannot be opened, the returned log is disabled and the run proceeds.
    pub fn begin(&self) -> Arc<RunLog> {
        let Some(dir) = self.dir.as_ref() else { return RunLog::disabled() };
        if let Err(e) = std::fs::create_dir_all(dir) {
            tracing::warn!("alignment transcripts: cannot create {}: {e}; this run is not recorded", dir.display());
            return RunLog::disabled();
        }
        self.retain(MAX_RUNS.saturating_sub(1));
        let stamp = unix_millis();
        let (id, path) = (0u32..64)
            .map(|n| match n {
                0 => format!("run-{stamp}"),
                n => format!("run-{stamp}-{n}"),
            })
            .map(|id| (id.clone(), dir.join(format!("{id}.jsonl"))))
            .find(|(_, p)| !p.exists())
            .unwrap_or_else(|| (format!("run-{stamp}-x"), dir.join(format!("run-{stamp}-x.jsonl"))));
        match File::options().create(true).append(true).open(&path) {
            Ok(file) => Arc::new(RunLog {
                id,
                started: Instant::now(),
                file: Mutex::new(Some(file)),
                seq: AtomicU64::new(0),
                bytes: AtomicU64::new(0),
            }),
            Err(e) => {
                tracing::warn!("alignment transcripts: cannot open {}: {e}; this run is not recorded", path.display());
                RunLog::disabled()
            }
        }
    }

    /// Drop the oldest transcripts until at most `keep` remain. Ids sort
    /// chronologically (they are millisecond stamps), so name order is age order.
    fn retain(&self, keep: usize) {
        let Some(dir) = self.dir.as_ref() else { return };
        let mut files = self.files();
        if files.len() <= keep {
            return;
        }
        files.sort();
        let doomed = files.len() - keep;
        for id in files.into_iter().take(doomed) {
            let path = dir.join(format!("{id}.jsonl"));
            if let Err(e) = std::fs::remove_file(&path) {
                tracing::warn!("alignment transcripts: cannot drop {}: {e}", path.display());
            }
        }
    }

    /// Every stored transcript's id.
    fn files(&self) -> Vec<String> {
        let Some(dir) = self.dir.as_ref() else { return Vec::new() };
        let Ok(rd) = std::fs::read_dir(dir) else { return Vec::new() };
        rd.filter_map(Result::ok)
            .filter_map(|e| e.file_name().to_str().map(str::to_string))
            .filter_map(|n| n.strip_suffix(".jsonl").map(str::to_string))
            .filter(|id| valid_id(id))
            .collect()
    }

    /// Newest first, so the UI's default choice is the run that just happened.
    pub fn list(&self) -> Vec<RunSummary> {
        let mut ids = self.files();
        ids.sort();
        ids.reverse();
        ids.iter().filter_map(|id| self.summary(id)).collect()
    }

    fn summary(&self, id: &str) -> Option<RunSummary> {
        let doc = self.document(id)?;
        let size_bytes = self.dir.as_ref().and_then(|d| std::fs::metadata(d.join(format!("{id}.jsonl"))).ok()).map_or(0, |m| m.len());
        let last = doc.events.last();
        Some(RunSummary {
            id: doc.id,
            started_unix: doc.started_unix,
            events: doc.events.len(),
            size_bytes,
            last_kind: last.and_then(|e| e.get("kind")).and_then(|k| k.as_str()).unwrap_or("").to_string(),
            last_message: last.and_then(|e| e.get("message")).and_then(|k| k.as_str()).unwrap_or("").to_string(),
            started: doc.events.first().and_then(|e| e.get("detail")).cloned(),
        })
    }

    /// One transcript as a document. `None` when there is no such run — including
    /// when `id` is not a well-formed run id, which is also what keeps this from
    /// being a path traversal.
    pub fn document(&self, id: &str) -> Option<RunDocument> {
        let dir = self.dir.as_ref()?;
        if !valid_id(id) {
            return None;
        }
        let raw = std::fs::read_to_string(dir.join(format!("{id}.jsonl"))).ok()?;
        let events: Vec<serde_json::Value> = raw
            .lines()
            .filter(|l| !l.trim().is_empty())
            // A half-written last line (the daemon was killed mid-append) is reported
            // as itself rather than making the whole document unreadable.
            .map(|l| serde_json::from_str(l).unwrap_or_else(|_| serde_json::json!({ "kind": "unparseable", "raw": l })))
            .collect();
        let started_unix =
            id.strip_prefix("run-").and_then(|s| s.split('-').next()).and_then(|s| s.parse::<u64>().ok()).unwrap_or(0) / 1000;
        let truncated = events.last().and_then(|e| e.get("kind")).and_then(|k| k.as_str()) == Some("truncated");
        Some(RunDocument { id: id.to_string(), started_unix, events, truncated })
    }
}

/// `run-<digits>` or `run-<digits>-<suffix>`, and nothing that could leave the
/// directory.
fn valid_id(id: &str) -> bool {
    id.starts_with("run-")
        && id.len() <= 40
        && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        && id.strip_prefix("run-").is_some_and(|rest| rest.chars().next().is_some_and(|c| c.is_ascii_digit()))
}

fn unix_millis() -> u128 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis())
}

static SHARED: OnceLock<Arc<Transcripts>> = OnceLock::new();

/// Point the transcript store at a directory (called once, from `main`).
pub fn init(dir: PathBuf) {
    let _ = SHARED.set(Arc::new(Transcripts::new(dir)));
}

/// The process-wide store. Disabled until [`init`] — so a unit test, or a daemon with
/// no writable `/data`, simply has no transcripts rather than failing.
pub fn shared() -> Arc<Transcripts> {
    SHARED.get_or_init(|| Arc::new(Transcripts::disabled())).clone()
}

#[cfg(test)]
mod tests;
