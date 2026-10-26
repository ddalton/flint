//! Lever D: the baseline parsed once per process and persisted as a
//! snapshot plus a journal of what each save changed. Whatever the
//! journal holds, a fresh process must read back exactly the baseline the
//! last successful save held, and nothing a failed or torn save wrote.

use super::state::{Baseline, BaselineEntry, SyncerState};
use super::tests::{syncer, write};
use flint_store::memory::MemoryStore;
use std::sync::Arc;

fn entry(tag: u64) -> BaselineEntry {
    BaselineEntry {
        etag: format!("\"e{tag}\""),
        key: Some(format!("k@{tag}")),
        generation: tag,
        size: tag,
        mtime_unix: tag as i64,
        mtime_nanos: Some(tag as u32),
        mode: (tag % 3 != 0).then_some(0o644 | (tag as u32 & 0o111)),
        crc64_b64: Some(format!("c{tag}")),
        racy: tag % 2 == 0,
        change: (tag % 4 != 1).then_some(super::scan::ChangeStamp {
            ctime: tag as i64 + 7,
            ctime_nanos: tag as u32 * 3,
            ino: tag * 11,
        }),
    }
}

/// Every field a save must carry, compared as plain collections.
fn same(a: &Baseline, b: &Baseline) -> bool {
    a.seq == b.seq
        && a.manifest_etag == b.manifest_etag
        && a.derived_etag == b.derived_etag
        && *a.entries == *b.entries
        && *a.skipped == *b.skipped
        && *a.prev_scan == *b.prev_scan
}

/// What a fresh process reads from `dir`.
fn reread(dir: &std::path::Path) -> Baseline {
    SyncerState::open(dir.to_path_buf()).unwrap().load_baseline().unwrap()
}

fn sizes(dir: &std::path::Path) -> (u64, u64) {
    let len = |n: &str| std::fs::metadata(dir.join(n)).map(|m| m.len()).unwrap_or(0);
    (len("baseline.json"), len("baseline.log"))
}

#[test]
fn a_small_save_appends_to_the_journal_and_a_fresh_process_reads_it_back() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    let expect = {
        let st = SyncerState::open(state.clone()).unwrap();
        let mut b = st.take_baseline().unwrap();
        for i in 0..100 {
            b.entries.insert(format!("p{i:03}"), entry(i));
            b.prev_scan.insert(format!("p{i:03}"));
        }
        b.seq = 1;
        st.commit_baseline(b).unwrap();
        let (snap, log) = sizes(&state);
        assert!(snap > 0 && log > 0, "the first save writes a snapshot and its journal");

        let mut b = st.take_baseline().unwrap();
        b.entries.remove("p007");
        b.entries.get_mut("p008").unwrap().size = 9999;
        b.entries.insert("q".into(), entry(500));
        b.skipped.insert("p009".into());
        b.prev_scan.remove("p007");
        b.seq = 2;
        b.manifest_etag = Some("\"m2\"".into());
        b.derived_etag = Some("\"m2\"".into());
        st.commit_baseline(b).unwrap();
        let (snap2, log2) = sizes(&state);
        assert_eq!(snap2, snap, "a small save leaves the snapshot alone");
        assert!(log2 > log, "and appends to the journal");
        assert_eq!(st.baseline_parses(), 0, "nothing was parsed: the store held it");
        st.load_baseline().unwrap()
    };
    assert!(same(&reread(&state), &expect));
}

#[test]
fn a_torn_journal_line_is_a_save_that_never_happened() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    let before = {
        let st = SyncerState::open(state.clone()).unwrap();
        let mut b = st.take_baseline().unwrap();
        b.entries.insert("a".into(), entry(1));
        st.commit_baseline(b).unwrap();
        let mut b = st.take_baseline().unwrap();
        b.entries.insert("b".into(), entry(2));
        st.commit_baseline(b).unwrap();
        st.load_baseline().unwrap()
    };
    // A crash mid-write: half of a further line, then nothing.
    let log = state.join("baseline.log");
    let good = std::fs::metadata(&log).unwrap().len();
    let mut torn = std::fs::read(&log).unwrap();
    torn.extend_from_slice(b"0123456789abcdef {\"seq\":9,\"put\":[[\"c\",");
    std::fs::write(&log, &torn).unwrap();
    {
        let st = SyncerState::open(state.clone()).unwrap();
        assert!(same(&st.load_baseline().unwrap(), &before), "the torn line is not applied");
        assert_eq!(std::fs::metadata(&log).unwrap().len(), good, "and is cut off");
        // The next save follows the last good line, so it is read back.
        let mut b = st.take_baseline().unwrap();
        b.entries.insert("d".into(), entry(4));
        st.commit_baseline(b).unwrap();
    }
    let after = reread(&state);
    assert!(after.entries.contains_key("d") && after.entries.contains_key("b") && !after.entries.contains_key("c"));
}

#[test]
fn a_line_whose_checksum_fails_ends_the_replay_there() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    {
        let st = SyncerState::open(state.clone()).unwrap();
        for k in ["a", "b", "c"] {
            let mut b = st.take_baseline().unwrap();
            b.entries.insert(k.into(), entry(1));
            st.commit_baseline(b).unwrap();
        }
    }
    // Flip one byte of the LAST line's payload: its CRC no longer agrees.
    let log = state.join("baseline.log");
    let mut bytes = std::fs::read(&log).unwrap();
    let n = bytes.len();
    let at = bytes[..n - 1].iter().rposition(|c| *c == b'"').unwrap();
    bytes[at - 1] ^= 0x01;
    std::fs::write(&log, &bytes).unwrap();
    let b = reread(&state);
    assert!(b.entries.contains_key("a") && b.entries.contains_key("b"));
    assert!(!b.entries.contains_key("c"), "the corrupt save is not applied");
}

#[test]
fn a_journal_written_for_another_snapshot_is_ignored() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    {
        let st = SyncerState::open(state.clone()).unwrap();
        let mut b = st.take_baseline().unwrap();
        b.entries.insert("a".into(), entry(1));
        st.commit_baseline(b).unwrap();
        let mut b = st.take_baseline().unwrap();
        b.entries.insert("stale".into(), entry(2));
        st.commit_baseline(b).unwrap();
    }
    let old_journal = std::fs::read(state.join("baseline.log")).unwrap();
    // A whole write (a stale copy is one), then the death the order
    // guards against: the new snapshot landed, its fresh journal did not.
    let snapshot = {
        let st = SyncerState::open(state.clone()).unwrap();
        let mut b = Baseline::default();
        b.entries.insert("a".into(), entry(1));
        b.seq = 7;
        st.save_baseline(&b).unwrap();
        st.load_baseline().unwrap()
    };
    std::fs::write(state.join("baseline.log"), &old_journal).unwrap();
    let b = reread(&state);
    assert!(same(&b, &snapshot), "the old journal's `stale` is not replayed onto the new snapshot");
}

#[test]
fn a_save_after_a_half_written_one_replaces_its_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    {
        let st = SyncerState::open(state.clone()).unwrap();
        let mut b = st.take_baseline().unwrap();
        b.entries.insert("a".into(), entry(1));
        st.commit_baseline(b).unwrap();
        let mut b = st.take_baseline().unwrap();
        b.entries.insert("b".into(), entry(2));
        st.commit_baseline(b).unwrap();
        // A write that failed half-way (ENOSPC, EIO) leaves bytes past the
        // verified length, and the store goes on.
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new().append(true).open(state.join("baseline.log")).unwrap();
        f.write_all(b"0123456789abcdef {\"seq\":1,\"put\":[[\"x\",").unwrap();
        let mut b = st.take_baseline().unwrap();
        b.entries.insert("c".into(), entry(3));
        st.commit_baseline(b).unwrap();
    }
    let b = reread(&state);
    assert!(b.entries.contains_key("c"), "the save after the half-written one is read back");
    assert!(!b.entries.contains_key("x"));
}

/// The e2e plant (`run-verbs.sh`) edits the snapshot in place with `sed`,
/// no syncer running: the journal still applies, and the edit stands.
#[test]
fn a_snapshot_edited_in_place_keeps_its_journal() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    {
        let st = SyncerState::open(state.clone()).unwrap();
        let mut b = st.take_baseline().unwrap();
        b.entries.insert("normal.txt".into(), entry(1));
        st.commit_baseline(b).unwrap();
        let mut b = st.take_baseline().unwrap();
        b.entries.insert("later.txt".into(), entry(2));
        st.commit_baseline(b).unwrap();
    }
    let snap = state.join("baseline.json");
    let text = std::fs::read_to_string(&snap).unwrap();
    assert_eq!(text.matches("\"entries\":{").count(), 1, "fixture: the plant's anchor");
    let planted = text.replacen(
        "\"entries\":{",
        "\"entries\":{\".flint/legacy.txt\":{\"etag\":\"legacy\",\"generation\":1,\"size\":3,\"mtime_unix\":1},",
        1,
    );
    std::fs::write(&snap, planted).unwrap();
    let b = reread(&state);
    assert!(b.entries.contains_key(".flint/legacy.txt"), "the edit stands");
    assert!(b.entries.contains_key("later.txt"), "and the journal still applies");
}

#[test]
fn a_stale_copy_is_written_whole_as_every_save_was_before() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    let st = SyncerState::open(state.clone()).unwrap();
    let mut b = st.take_baseline().unwrap();
    b.entries.insert("k".into(), entry(1));
    st.commit_baseline(b).unwrap();
    let mut one = st.load_baseline().unwrap();
    let mut two = st.load_baseline().unwrap();
    one.entries.remove("k");
    st.save_baseline(&one).unwrap();
    two.entries.insert("j".into(), entry(2));
    st.save_baseline(&two).unwrap();
    // Last writer wins, whole: `two` still had `k`.
    let disk = { drop(st); reread(&state) };
    assert!(same(&disk, &two));
    assert!(disk.entries.contains_key("k"));
}

#[test]
fn a_collection_replaced_whole_is_written_whole() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    let st = SyncerState::open(state.clone()).unwrap();
    let mut b = st.take_baseline().unwrap();
    for k in ["a", "b", "c"] {
        b.prev_scan.insert(k.into());
    }
    st.commit_baseline(b).unwrap();
    let mut b = st.take_baseline().unwrap();
    // Built rather than loaded: it cannot say `b` and `c` left.
    b.prev_scan = ["a".to_string()].into_iter().collect();
    st.commit_baseline(b).unwrap();
    drop(st);
    assert_eq!(reread(&state).prev_scan.iter().cloned().collect::<Vec<_>>(), vec!["a".to_string()]);
}

#[test]
fn sync_to_gains_and_loses_exactly_the_difference() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    let st = SyncerState::open(state.clone()).unwrap();
    let mut b = st.take_baseline().unwrap();
    let start: std::collections::BTreeSet<String> = ["a", "c", "e", "g"].iter().map(|s| s.to_string()).collect();
    b.prev_scan.sync_to(&start);
    st.commit_baseline(b).unwrap();
    let mut b = st.take_baseline().unwrap();
    let next: std::collections::BTreeSet<String> = ["b", "c", "g", "h"].iter().map(|s| s.to_string()).collect();
    b.prev_scan.sync_to(&next);
    assert_eq!(*b.prev_scan, next);
    st.commit_baseline(b).unwrap();
    drop(st);
    assert_eq!(*reread(&state).prev_scan, next);
}

/// A seeded walk of every mutation the trackers offer, against a plain
/// mirror, re-read from disk at every step, with stale copies and
/// compactions mixed in.
#[test]
fn any_sequence_of_saves_reads_back_as_the_last_one() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    let mut rng: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut next = |n: u64| {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        rng % n
    };
    let mut st = SyncerState::open(state.clone()).unwrap();
    st.set_baseline_compact_min(2048);
    let mut stale: Option<Baseline> = None;
    for step in 0..400u64 {
        let mut b = st.take_baseline().unwrap();
        for _ in 0..(1 + next(4)) {
            let k = format!("p{}", next(40));
            match next(9) {
                0 | 1 => { b.entries.insert(k, entry(step)); }
                2 => { b.entries.remove(&k); }
                3 => { if let Some(e) = b.entries.get_mut(&k) { e.size = step; } }
                4 => { b.skipped.insert(k); }
                5 => { b.skipped.remove(&k); }
                6 => { b.prev_scan.insert(k); }
                7 => { b.prev_scan.remove(&k); }
                _ => { b.entries.retain(|p, _| !p.ends_with('3')); }
            }
        }
        b.seq = step;
        b.derived_etag = (next(3) == 0).then(|| format!("\"d{step}\""));
        if next(10) == 0 {
            stale = Some(b.clone());
        }
        st.commit_baseline(b).unwrap();
        if next(15) == 0 {
            if let Some(s) = stale.take() {
                st.save_baseline(&s).unwrap();
            }
        }
        let held = st.load_baseline().unwrap();
        drop(st);
        assert!(same(&reread(&state), &held), "step {step}: the disk is not what the store held");
        st = SyncerState::open(state.clone()).unwrap();
        st.set_baseline_compact_min(2048);
    }
}

/// The point of D: a steady barrier parses the baseline no more. The
/// control (forget it between barriers) shows the counter counts.
#[tokio::test]
async fn a_steady_barrier_does_not_parse_the_baseline() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut a = syncer(&store, dir.path()).await;
    a.checkout().await.unwrap();
    write(dir.path(), "a.txt", "v1");
    a.run_barrier().await.unwrap();
    let parses = a.state.baseline_parses();
    for k in 0..3 {
        a.run_barrier().await.unwrap();
        write(dir.path(), "a.txt", &format!("v{}", k + 2));
        a.run_barrier().await.unwrap();
    }
    assert_eq!(a.state.baseline_parses(), parses, "an idle tick or a publish parsed the baseline");
    // Control: a store that forgets between barriers parses once each.
    a.state.forget_baseline();
    a.run_barrier().await.unwrap();
    assert_eq!(a.state.baseline_parses(), parses + 1, "the counter does not count parses");
}
