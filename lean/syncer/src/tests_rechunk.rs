//! Lever G's re-chunk (`chunk::rechunk_with`; latency plan §12, claim 1):
//! the chunk list it writes is `split_with` of the changed entries, ref for
//! ref and byte for byte, and it reads only the chunks near the changes.

use std::collections::BTreeMap;
use std::sync::Arc;

use super::chunk::{rechunk_with, split_with, ChunkBody, ChunkRef};
use super::manifest::LeanEntry;
use super::tests::entry_at;

/// xorshift64*: deterministic, so a failing case is a seed to replay.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
}

type Doc = BTreeMap<String, LeanEntry>;

fn key(r: &mut Rng, space: u64) -> String {
    format!("d{:02}/f{:04}", r.below(8), r.below(space))
}

/// The old document's chunks, and a loader over them that counts its calls.
fn chunked(doc: &Doc, t: usize, mn: usize, mx: usize) -> (Vec<ChunkRef>, Vec<Arc<Doc>>) {
    let split = split_with(doc, t, mn, mx).unwrap();
    let refs = split.iter().map(|(r, _)| r.clone()).collect();
    let bodies = split
        .iter()
        .map(|(_, b)| Arc::new(serde_json::from_slice::<ChunkBody>(b).unwrap().entries))
        .collect();
    (refs, bodies)
}

fn apply(doc: &Doc, changes: &BTreeMap<String, Option<LeanEntry>>) -> Doc {
    let mut d = doc.clone();
    for (k, v) in changes {
        match v {
            Some(e) => {
                d.insert(k.clone(), e.clone());
            }
            None => {
                d.remove(k);
            }
        }
    }
    d
}

/// Re-chunk `doc` after `changes` and hold it to `split_with` of the result.
/// Returns how many chunks the re-chunk read.
fn check(doc: &Doc, changes: &BTreeMap<String, Option<LeanEntry>>, t: usize, mn: usize, mx: usize, why: &str) -> usize {
    let (refs, bodies) = chunked(doc, t, mn, mx);
    check_over(&refs, &bodies, doc, changes, t, mn, mx, why)
}

/// `check` over a document already chunked.
#[allow(clippy::too_many_arguments)]
fn check_over(
    refs: &[ChunkRef],
    bodies: &[Arc<Doc>],
    doc: &Doc,
    changes: &BTreeMap<String, Option<LeanEntry>>,
    t: usize,
    mn: usize,
    mx: usize,
    why: &str,
) -> usize {
    let mut reads = 0usize;
    let got = rechunk_with(
        refs,
        |i| {
            reads += 1;
            Ok(bodies[i].clone())
        },
        changes,
        t,
        mn,
        mx,
    )
    .unwrap();
    let want = split_with(&apply(doc, changes), t, mn, mx).unwrap();
    let got_refs: Vec<&ChunkRef> = got.iter().map(|(r, _)| r).collect();
    let want_refs: Vec<&ChunkRef> = want.iter().map(|(r, _)| r).collect();
    assert_eq!(got_refs, want_refs, "{why}: the chunk list is not split_with's");
    for ((r, body), (_, want_body)) in got.iter().zip(&want) {
        match body {
            Some(b) => assert_eq!(b, want_body, "{why}: chunk {} encodes differently", r.addr),
            None => assert!(refs.contains(r), "{why}: chunk {} reused but not in the old list", r.addr),
        }
    }
    reads
}

fn edits(r: &mut Rng, doc: &Doc, space: u64, n: u64, seq: u64) -> BTreeMap<String, Option<LeanEntry>> {
    let keys: Vec<&String> = doc.keys().collect();
    let mut ch = BTreeMap::new();
    for _ in 0..n {
        match r.below(4) {
            // A delete of a present key, an update of one, an insert, and a
            // delete of a key the document may lack.
            0 if !keys.is_empty() => {
                ch.insert(keys[r.below(keys.len() as u64) as usize].clone(), None);
            }
            1 if !keys.is_empty() => {
                let k = keys[r.below(keys.len() as u64) as usize].clone();
                ch.insert(k.clone(), Some(entry_at(&k, seq)));
            }
            2 => {
                let k = key(r, space);
                ch.insert(k.clone(), Some(entry_at(&k, seq)));
            }
            _ => {
                ch.insert(key(r, space), None);
            }
        }
    }
    ch
}

#[test]
fn a_rechunk_is_split_of_the_changed_entries_over_random_documents_and_edits() {
    // Tiny sizing, so cuts, suppressed cuts, forced cuts and cascades are
    // everywhere; and the shipped ratio (min = target/4, max = 4 x target).
    for (t, mn, mx) in [(4usize, 1usize, 6usize), (8, 2, 32), (5, 3, 7), (16, 4, 64)] {
        for seed in 1..=400u64 {
            let mut r = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ (t as u64));
            let space = 1 + r.below(300);
            let size = r.below(400);
            let mut doc = Doc::new();
            for _ in 0..size {
                let k = key(&mut r, space);
                doc.insert(k.clone(), entry_at(&k, 1));
            }
            let many = r.below(4) == 0;
            let n = 1 + r.below(if many { 40 } else { 4 });
            let ch = edits(&mut r, &doc, space, n, 2);
            check(&doc, &ch, t, mn, mx, &format!("sizing ({t},{mn},{mx}) seed {seed}"));
        }
    }
}

#[test]
fn a_rechunk_handles_every_edge_of_the_key_stream() {
    let (t, mn, mx) = (4usize, 2usize, 8usize);
    let doc: Doc = (0..60).map(|i| (format!("k{i:03}"), entry_at(&format!("k{i:03}"), 1))).collect();
    let (refs, bodies) = chunked(&doc, t, mn, mx);
    assert!(refs.len() > 4, "fixture: too few chunks to have edges");
    let one = |k: &str, v: Option<LeanEntry>| -> BTreeMap<String, Option<LeanEntry>> { [(k.to_string(), v)].into() };
    let e = |k: &str| Some(entry_at(k, 2));
    // Every old chunk's first and last key: deleted, updated, and an insert
    // just after the last (inside the old chunk's range).
    for (i, r) in refs.iter().enumerate() {
        let last = bodies[i].keys().last().unwrap().clone();
        for (k, v) in [
            (r.first.clone(), None),
            (r.first.clone(), e(&r.first)),
            (last.clone(), None),
            (last.clone(), e(&last)),
            (format!("{last}~"), e(&format!("{last}~"))),
        ] {
            check(&doc, &one(&k, v), t, mn, mx, &format!("chunk {i} key {k}"));
        }
    }
    // Before the first key, after the last, the whole document deleted, and
    // the empty document grown.
    check(&doc, &one("a", e("a")), t, mn, mx, "insert before the first key");
    check(&doc, &one("z", e("z")), t, mn, mx, "insert after the last key");
    let all: BTreeMap<String, Option<LeanEntry>> = doc.keys().map(|k| (k.clone(), None)).collect();
    check(&doc, &all, t, mn, mx, "every key deleted");
    let grow: BTreeMap<String, Option<LeanEntry>> = (0..30).map(|i| (format!("n{i:02}"), e(&format!("n{i:02}")))).collect();
    check(&Doc::new(), &grow, t, mn, mx, "the empty document grown");
    check(&doc, &BTreeMap::new(), t, mn, mx, "no change");
}

/// The point of it: a one-path change at a realistic size reads the chunk it
/// falls in and, in expectation, about one more for the cascade, never the
/// document.
#[test]
fn a_one_path_change_reads_a_few_chunks_not_the_document() {
    let (t, mn, mx) = (64usize, 16usize, 256usize);
    let mut r = Rng(7);
    let doc: Doc = (0..10_000).map(|i| (format!("src/f{i:06}"), entry_at(&format!("f{i:06}"), 1))).collect();
    let (refs, bodies) = chunked(&doc, t, mn, mx);
    let chunks = refs.len();
    let (mut total, mut worst) = (0usize, 0usize);
    let trials = 60;
    for i in 0..trials {
        let k = format!("src/f{:06}", r.below(10_000));
        let v = match i % 3 {
            0 => None,
            1 => Some(entry_at(&k, 2)),
            _ => Some(entry_at(&format!("{k}x"), 2)),
        };
        let k = if i % 3 == 2 { format!("{k}x") } else { k };
        let reads = check_over(&refs, &bodies, &doc, &[(k, v)].into(), t, mn, mx, &format!("trial {i}"));
        total += reads;
        worst = worst.max(reads);
    }
    let mean = total as f64 / trials as f64;
    println!("a one-path change read {mean:.2} chunks on average, {worst} at most, of {chunks}");
    assert!(mean <= 3.0, "a one-path change read {mean:.2} chunks on average of {chunks}");
    assert!(worst <= 8, "a one-path change read {worst} chunks of {chunks}");
}
