//! What the transcript has to guarantee on its own: that it appends, that it is
//! readable back as one document, that it cannot fill `/data`, and that the oldest
//! run is the one that goes.

use super::*;

fn temp_dir(tag: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!("align-transcript-{tag}-{}-{}", std::process::id(), unix_millis()));
    let _ = std::fs::remove_dir_all(&p);
    p
}

#[test]
fn appends_and_reads_back_as_one_document() {
    let dir = temp_dir("roundtrip");
    let t = Transcripts::new(&dir);
    let log = t.begin();
    log.record(Event::new("run_started", "measuring 2 speakers").detail(&serde_json::json!({ "mode": "sweet_spot" })));
    log.record(Event::for_member("gate_locked", "spk-a", "locked after 4 periods"));
    log.record(Event::new("run_finished", "aligned and verified"));

    let doc = t.document(log.id()).expect("the run just written must be readable");
    assert_eq!(doc.events.len(), 3);
    assert_eq!(doc.events[0]["kind"], "run_started");
    assert_eq!(doc.events[1]["member"], "spk-a");
    // Sequence numbers are what tell a reader a line is missing rather than reordered.
    assert_eq!(doc.events.iter().map(|e| e["seq"].as_u64().unwrap()).collect::<Vec<_>>(), vec![0, 1, 2]);
    assert!(!doc.truncated);

    let listed = t.list();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, log.id());
    assert_eq!(listed[0].events, 3);
    assert_eq!(listed[0].last_kind, "run_finished");
    assert_eq!(listed[0].last_message, "aligned and verified");
    assert!(listed[0].size_bytes > 0);
    let _ = std::fs::remove_dir_all(&dir);
}

/// The user's case: the run never got a speaker rendering. What must survive is the
/// *reason*, not a verdict.
#[test]
fn a_run_whose_speakers_never_appeared_still_has_a_transcript() {
    let dir = temp_dir("silent");
    let t = Transcripts::new(&dir);
    let log = t.begin();
    log.record(Event::new("run_started", "measuring 2 speakers"));
    log.record(Event::for_member("gate_failed", "spk-b", "no tone from 'spk-b' within 30 s"));
    log.record(Event::for_member("refusal", "spk-b", "the click track never arrived from 'spk-b'"));
    log.record(Event::new("run_finished", "refused"));

    let doc = t.document(log.id()).unwrap();
    let kinds: Vec<&str> = doc.events.iter().map(|e| e["kind"].as_str().unwrap()).collect();
    assert_eq!(kinds, vec!["run_started", "gate_failed", "refusal", "run_finished"]);
    assert!(doc.events[1]["message"].as_str().unwrap().contains("no tone"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_event_bound_closes_the_file_and_says_so() {
    let dir = temp_dir("bound");
    let t = Transcripts::new(&dir);
    let log = t.begin();
    for i in 0..(MAX_EVENTS + 50) {
        log.record(Event::new("phase", format!("event {i}")));
    }
    assert!(!log.is_recording(), "the log must close itself rather than growing without end");
    let doc = t.document(log.id()).unwrap();
    assert!(doc.truncated, "and the file must say that is what happened");
    // MAX_EVENTS events plus the marker; nothing after it.
    assert_eq!(doc.events.len() as u64, MAX_EVENTS + 1);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_byte_bound_closes_the_file_too() {
    let dir = temp_dir("bytes");
    let t = Transcripts::new(&dir);
    let log = t.begin();
    let fat = "x".repeat(4096);
    for _ in 0..500 {
        log.record(Event::new("phase", fat.clone()));
    }
    assert!(!log.is_recording());
    let size = std::fs::metadata(dir.join(format!("{}.jsonl", log.id()))).unwrap().len();
    // One event may cross the line (the bound is checked after the append, so the
    // *reason* for the truncation is itself recorded), plus the marker.
    assert!(size < MAX_BYTES + 8 * 1024, "the file must stay bounded, got {size} bytes");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn retention_keeps_the_newest_and_drops_the_oldest() {
    let dir = temp_dir("retain");
    let t = Transcripts::new(&dir);
    let mut ids = Vec::new();
    for i in 0..(MAX_RUNS + 3) {
        let log = t.begin();
        log.record(Event::new("run_started", format!("run {i}")));
        ids.push(log.id().to_string());
        // The id carries a millisecond stamp; a same-millisecond run gets a `-n`
        // suffix, and both orderings have to remain age-ordered by name.
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    let listed = t.list();
    assert_eq!(listed.len(), MAX_RUNS, "at most MAX_RUNS transcripts may exist");
    // Newest first, and the newest is the last one begun.
    assert_eq!(listed[0].id, *ids.last().unwrap());
    for dropped in &ids[..3] {
        assert!(t.document(dropped).is_none(), "{dropped} should have been dropped");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_disabled_store_records_nothing_and_never_fails() {
    let t = Transcripts::disabled();
    let log = t.begin();
    log.record(Event::new("run_started", "no /data here"));
    assert!(!log.is_recording());
    assert_eq!(log.id(), "");
    assert!(t.list().is_empty());
    assert!(t.document("run-1").is_none());
}

#[test]
fn a_fetch_cannot_walk_out_of_the_directory() {
    let dir = temp_dir("traversal");
    let t = Transcripts::new(&dir);
    let log = t.begin();
    log.record(Event::new("run_started", "x"));
    for bad in ["../../etc/passwd", "run-../secret", "run-1/../../x", "settings", ""] {
        assert!(t.document(bad).is_none(), "{bad} must not resolve");
    }
    assert!(t.document(log.id()).is_some());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_half_written_line_does_not_make_the_document_unreadable() {
    let dir = temp_dir("torn");
    let t = Transcripts::new(&dir);
    let log = t.begin();
    log.record(Event::new("run_started", "x"));
    let path = dir.join(format!("{}.jsonl", log.id()));
    let mut f = File::options().append(true).open(&path).unwrap();
    f.write_all(b"{\"kind\":\"phase\",\"mess").unwrap();
    let doc = t.document(log.id()).unwrap();
    assert_eq!(doc.events.len(), 2);
    assert_eq!(doc.events[1]["kind"], "unparseable");
    let _ = std::fs::remove_dir_all(&dir);
}
