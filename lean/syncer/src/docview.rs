//! A manifest document seen chunk by chunk (latency plan 2026-10-23,
//! lever G, §12).
//!
//! A commit changes a handful of paths, and it used to load, clone, diff
//! and rewrite every entry of the document to do it: 4.1 s of a 6.9 s
//! one-file publish at a million entries. A `DocView` holds the pointer,
//! the tombstones and the chunk list, proves every chunk the pointer names
//! is in the bucket (one LIST, as a full load does), and parses a chunk
//! only when a path in it is asked for. Each parse passes the checks a full
//! load makes (`chunk::parse_chunk`).
//!
//! It answers "who cites this handle" without the whole document (§12,
//! claim 2): a handle key is minted `<prefix>/files/<path>@<flush>`, so an
//! entry whose key does not parse to its own path is an ALIAS (a rename's
//! destination, or a key of any other shape), and every entry citing a key
//! is at the path the key names or among the aliases. Each chunk's aliases
//! are kept by address, chunks being immutable, so a process parses every
//! chunk once, on its first commit.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Mutex};

use flint_store::{ObjectStore, StoreError};

use super::chunk::{self, ChunkRef};
use super::manifest::{
    self, Entries, Hole, LeanEntry, LeanManifest, LoadedManifest, LoadedPointer, ManifestHandle, Pointer, Tombstone,
};
use super::{LeanConfig, LeanError, LeanResult};

type EntryMap = BTreeMap<String, LeanEntry>;

/// A chunk's aliases: `(path, key)` for each entry whose key does not name
/// its path.
type Aliases = Arc<Vec<(String, String)>>;

/// One workspace's chunks' aliases, by address (`LeanConfig::alias_cache`):
/// a chunk is immutable, so its aliases never change.
pub type AliasCache = Mutex<HashMap<String, Aliases>>;

/// The path a handle key names, if it has the shape `handle_key` mints.
pub fn key_path<'a>(prefix: &str, key: &'a str) -> Option<&'a str> {
    let rest = key.strip_prefix(prefix)?.strip_prefix("/files/")?;
    Some(rest.rsplit_once('@')?.0)
}

fn aliases_in(prefix: &str, entries: &EntryMap) -> Vec<(String, String)> {
    aliases_of(prefix, entries.iter())
}

fn aliases_of<'a>(prefix: &str, entries: impl Iterator<Item = (&'a String, &'a LeanEntry)>) -> Vec<(String, String)> {
    entries
        .filter(|(p, e)| key_path(prefix, &e.key) != Some(p.as_str()))
        .map(|(p, e)| (p.clone(), e.key.clone()))
        .collect()
}

/// Chunk `i`'s entries in a document held whole: from its first key to the
/// next chunk's.
pub fn chunk_entries<'a>(
    doc: &'a EntryMap,
    refs: &'a [ChunkRef],
    i: usize,
) -> impl Iterator<Item = (&'a String, &'a LeanEntry)> {
    use std::ops::Bound;
    let to = refs.get(i + 1).map_or(Bound::Unbounded, |n| Bound::Excluded(n.first.as_str()));
    doc.range::<str, _>((Bound::Included(refs[i].first.as_str()), to))
}

/// The retire set of an overlay on a document held whole (the gateway's
/// per-path commit, latency plan §16): `DocView::retired`'s rule, with
/// every lookup in the map. A chunk whose aliases the cache lacks is
/// scanned in the map, and remembered.
pub fn retired_over(
    cfg: &LeanConfig,
    refs: &[ChunkRef],
    doc: &EntryMap,
    overlay: &BTreeMap<String, Option<LeanEntry>>,
) -> Vec<String> {
    let mut candidates: Vec<String> = vec![];
    for (p, now) in overlay {
        if let Some(was) = doc.get(p) {
            if now.as_ref().is_none_or(|n| n.key != was.key) {
                candidates.push(was.key.clone());
            }
        }
    }
    if candidates.is_empty() {
        return vec![];
    }
    let aliases: Vec<Aliases> = {
        let mut cache = cfg.alias_cache.lock().unwrap();
        (0..refs.len())
            .map(|i| {
                cache
                    .entry(refs[i].addr.clone())
                    .or_insert_with(|| Arc::new(aliases_of(&cfg.prefix, chunk_entries(doc, refs, i))))
                    .clone()
            })
            .collect()
    };
    let mut by_key: HashMap<&str, Vec<&str>> = HashMap::new();
    for (path, key) in aliases.iter().flat_map(|a| a.iter()) {
        by_key.entry(key.as_str()).or_default().push(path.as_str());
    }
    let overlay_cites: HashSet<&str> = overlay.values().flatten().map(|e| e.key.as_str()).collect();
    candidates
        .into_iter()
        .filter(|k| {
            let cited = overlay_cites.contains(k.as_str())
                || by_key.get(k.as_str()).is_some_and(|ps| ps.iter().any(|p| !overlay.contains_key(*p)))
                || key_path(&cfg.prefix, k)
                    .filter(|q| !overlay.contains_key(*q))
                    .is_some_and(|q| doc.get(q).is_some_and(|e| &e.key == k));
            !cited
        })
        .collect()
}

/// Record the aliases of a whole document's chunks, which a full load and a
/// whole-document write hold every entry of, and keep this prefix's cache to
/// those chunks. A process that has loaded or written a document then
/// answers "who cites this handle" without parsing a chunk.
pub fn remember(cfg: &LeanConfig, refs: &[ChunkRef], entries: &EntryMap) {
    let prefix = cfg.prefix.as_str();
    let mut cache = cfg.alias_cache.lock().unwrap();
    let mut it = entries.iter().peekable();
    for (i, r) in refs.iter().enumerate() {
        let next = refs.get(i + 1).map(|n| n.first.as_str());
        let mut aliases = vec![];
        while let Some((p, e)) = it.next_if(|(p, _)| next.is_none_or(|n| p.as_str() < n)) {
            if key_path(prefix, &e.key) != Some(p.as_str()) {
                aliases.push((p.clone(), e.key.clone()));
            }
        }
        cache.entry(r.addr.clone()).or_insert_with(|| Arc::new(aliases));
    }
    let live: HashSet<&str> = refs.iter().map(|r| r.addr.as_str()).collect();
    cache.retain(|a, _| live.contains(a.as_str()));
}

/// What opening a view found.
pub enum Opened {
    /// A chunked document, seen chunk by chunk.
    View(DocView),
    /// Anything else, through the full load: no pointer (the legacy
    /// single object, or nothing published), a single-object pointer, or
    /// a LIST the credential may not make (existence then needs every
    /// chunk fetched, which is the full load).
    Full(Option<LoadedManifest>),
}

pub struct DocView {
    store: Arc<dyn ObjectStore>,
    cfg: LeanConfig,
    pub pointer: Pointer,
    pub etag: String,
    pub last_modified_unix: Option<u64>,
    pub refs: Vec<ChunkRef>,
    pub tombstones: BTreeMap<String, Tombstone>,
    parsed: HashMap<usize, Arc<EntryMap>>,
    /// Chunks parsed by this view, for the bench and the tests.
    pub chunks_read: usize,
}

impl DocView {
    /// Open the document at the pointer, `first` being a pointer read the
    /// caller already made (as `manifest::load_from`). A chunk the pointer
    /// names that the bucket lacks restarts onto a moved pointer and is
    /// refused under an unchanged one, as a full load does.
    pub async fn open(
        store: Arc<dyn ObjectStore>,
        cfg: &LeanConfig,
        mut first: Option<Option<LoadedPointer>>,
    ) -> LeanResult<Opened> {
        for attempt in 0..manifest::LOAD_ATTEMPTS {
            let read = match first.take() {
                Some(already) => already,
                None => manifest::load_pointer(store.as_ref(), cfg).await?,
            };
            let Some(lp) = read else {
                return Ok(Opened::Full(manifest::load_from(store.as_ref(), cfg, Some(None)).await?));
            };
            let refs = match lp.pointer.entries()? {
                Entries::Chunked(r) => r.to_vec(),
                Entries::Single { .. } => {
                    return Ok(Opened::Full(manifest::load_from(store.as_ref(), cfg, Some(Some(lp))).await?));
                }
            };
            chunk::check_list_order(&refs)?;
            let listed = match store.list(&format!("{}/{}/chunks/", cfg.prefix, super::LEAN_DIR)).await {
                Ok(l) => l,
                Err(_) => return Ok(Opened::Full(manifest::load_from(store.as_ref(), cfg, Some(Some(lp))).await?)),
            };
            let present: HashSet<&str> = listed.iter().filter_map(|o| o.key.rsplit('/').next()).collect();
            let hole = refs
                .iter()
                .find(|r| !present.contains(r.addr.as_str()))
                .map(|r| Hole::Chunk { addr: r.addr.clone(), first: r.first.clone() })
                .or_else(|| {
                    lp.pointer.tombstones.as_ref().filter(|a| !present.contains(a.as_str())).map(|a| Hole::Tombstones(a.clone()))
                });
            if let Some(hole) = hole {
                let moved = match manifest::load_pointer(store.as_ref(), cfg).await? {
                    Some(now) => now.etag != lp.etag,
                    None => true,
                };
                if moved && attempt + 1 < manifest::LOAD_ATTEMPTS {
                    continue;
                }
                return Err(LeanError::State(hole.refusal(lp.pointer.seq)));
            }
            let tombstones = match lp.pointer.tombstones.clone() {
                None => BTreeMap::new(),
                Some(addr) => {
                    let body = fetch_body(store.as_ref(), cfg, &addr, lp.pointer.seq, || Hole::Tombstones(addr.clone()))
                        .await?;
                    chunk::decode_tombstones(&body, &addr)?
                }
            };
            // What the cache keeps from here is this document's chunks.
            if let Some(c) = cfg.chunk_cache.as_deref() {
                let mut live: HashSet<String> = refs.iter().map(|r| r.addr.clone()).collect();
                live.extend(lp.pointer.tombstones.clone());
                c.retain(&live);
            }
            return Ok(Opened::View(DocView {
                store,
                cfg: cfg.clone(),
                pointer: lp.pointer,
                etag: lp.etag,
                last_modified_unix: lp.last_modified_unix,
                refs,
                tombstones,
                parsed: HashMap::new(),
                chunks_read: 0,
            }));
        }
        unreachable!("the last attempt returns")
    }

    pub fn seq(&self) -> u64 {
        self.pointer.seq
    }

    /// The handle a writer presents to CAS over this read.
    pub fn handle(&self) -> ManifestHandle {
        ManifestHandle {
            etag: self.etag.clone(),
            legacy: false,
            prev_chunks: self.refs.clone(),
            prev_tombstones: self.pointer.tombstones.clone(),
        }
    }

    /// The chunk a path falls in: the last whose first key is not after it.
    pub fn home(&self, p: &str) -> usize {
        self.refs.partition_point(|r| r.first.as_str() <= p).saturating_sub(1)
    }

    /// Chunk `i`'s entries, parsed once per view.
    pub async fn chunk(&mut self, i: usize) -> LeanResult<Arc<EntryMap>> {
        if let Some(c) = self.parsed.get(&i) {
            return Ok(c.clone());
        }
        let r = self.refs[i].clone();
        let seq = self.pointer.seq;
        let body = fetch_body(self.store.as_ref(), &self.cfg, &r.addr, seq, || Hole::Chunk {
            addr: r.addr.clone(),
            first: r.first.clone(),
        })
        .await?;
        let entries = Arc::new(chunk::parse_chunk(&self.refs, i, &body)?);
        self.cfg
            .alias_cache
            .lock()
            .unwrap()
            .entry(r.addr.clone())
            .or_insert_with(|| Arc::new(aliases_in(&self.cfg.prefix, &entries)));
        self.chunks_read += 1;
        self.parsed.insert(i, entries.clone());
        Ok(entries)
    }

    /// Parse the chunks the given paths fall in, the misses fetched
    /// `chunk_fetch_parallelism` at a time.
    pub async fn prefetch<'a>(&mut self, paths: impl IntoIterator<Item = &'a str>) -> LeanResult<()> {
        if self.refs.is_empty() {
            return Ok(());
        }
        let want: std::collections::BTreeSet<usize> =
            paths.into_iter().map(|p| self.home(p)).filter(|i| !self.parsed.contains_key(i)).collect();
        self.read_chunks(want.into_iter().collect()).await
    }

    async fn read_chunks(&mut self, want: Vec<usize>) -> LeanResult<()> {
        use futures::stream::{self, StreamExt};
        let seq = self.pointer.seq;
        let (store, cfg) = (self.store.clone(), self.cfg.clone());
        let got: Vec<(usize, LeanResult<bytes::Bytes>)> = stream::iter(want.into_iter().map(|i| {
            let r = self.refs[i].clone();
            let (store, cfg) = (store.clone(), cfg.clone());
            async move {
                let hole = || Hole::Chunk { addr: r.addr.clone(), first: r.first.clone() };
                (i, fetch_body(store.as_ref(), &cfg, &r.addr, seq, hole).await)
            }
        }))
        .buffer_unordered(self.cfg.chunk_fetch_parallelism.max(1))
        .collect()
        .await;
        for (i, body) in got {
            let entries = Arc::new(chunk::parse_chunk(&self.refs, i, &body?)?);
            self.cfg
                .alias_cache
                .lock()
                .unwrap()
                .entry(self.refs[i].addr.clone())
                .or_insert_with(|| Arc::new(aliases_in(&self.cfg.prefix, &entries)));
            self.chunks_read += 1;
            self.parsed.insert(i, entries);
        }
        Ok(())
    }

    pub async fn get(&mut self, p: &str) -> LeanResult<Option<LeanEntry>> {
        if self.refs.is_empty() {
            return Ok(None);
        }
        let i = self.home(p);
        Ok(self.chunk(i).await?.get(p).cloned())
    }

    /// The first key of the document under `prefix` that `skip` does not
    /// exclude, in key order.
    pub async fn first_under(&mut self, prefix: &str, skip: &dyn Fn(&str) -> bool) -> LeanResult<Option<String>> {
        if self.refs.is_empty() {
            return Ok(None);
        }
        let mut i = self.home(prefix);
        while i < self.refs.len() {
            if i > self.home(prefix) && !self.refs[i].first.starts_with(prefix) {
                return Ok(None);
            }
            let c = self.chunk(i).await?;
            for k in c.range::<str, _>((std::ops::Bound::Included(prefix), std::ops::Bound::Unbounded)).map(|(k, _)| k) {
                if !k.starts_with(prefix) {
                    return Ok(None);
                }
                if !skip(k) {
                    return Ok(Some(k.clone()));
                }
            }
            i += 1;
        }
        Ok(None)
    }

    /// `chunk::rechunk_with` over this view: a chunk the re-chunk needs and
    /// this view has not read is fetched, and the re-chunk run again.
    pub async fn rechunk(
        &mut self,
        changes: &BTreeMap<String, Option<LeanEntry>>,
    ) -> LeanResult<Vec<(ChunkRef, Option<Vec<u8>>)>> {
        self.prefetch(changes.keys().map(|k| k.as_str())).await?;
        loop {
            let mut need: Option<usize> = None;
            let parsed = &self.parsed;
            let out = chunk::rechunk_with(
                &self.refs,
                |i| match parsed.get(&i) {
                    Some(c) => Ok(c.clone()),
                    None => {
                        need = Some(i);
                        Err(LeanError::State(format!("chunk {i} not read yet")))
                    }
                },
                changes,
                self.cfg.chunk_target,
                self.cfg.chunk_min,
                self.cfg.chunk_max,
            );
            match (out, need) {
                (Ok(r), _) => {
                    // A chunk this commit encoded: its aliases are known now,
                    // so a writer never parses its own chunks again for them.
                    let mut cache = self.cfg.alias_cache.lock().unwrap();
                    for (c, body) in &r {
                        if let Some(b) = body {
                            let key = c.addr.clone();
                            if !cache.contains_key(&key) {
                                let body: chunk::ChunkBody = serde_json::from_slice(b)
                                    .map_err(|e| LeanError::State(format!("chunk {} parse: {e}", c.addr)))?;
                                cache.insert(key, Arc::new(aliases_in(&self.cfg.prefix, &body.entries)));
                            }
                        }
                    }
                    return Ok(r);
                }
                (Err(_), Some(i)) => {
                    self.chunk(i).await?;
                }
                (Err(e), None) => return Err(e),
            }
        }
    }

    /// The retire set (§12, claim 2): each key theirs cites at a path of
    /// `overlay` that the overlay drops, and that the merged document (the
    /// overlay over this view) cites nowhere. In path order, a key twice
    /// where two changed paths dropped it, as the whole-document loop gives
    /// it. Every chunk's aliases are read only when something was dropped.
    pub async fn retired(&mut self, overlay: &BTreeMap<String, Option<LeanEntry>>) -> LeanResult<Vec<String>> {
        let mut candidates: Vec<String> = vec![];
        for (p, now) in overlay {
            if let Some(was) = self.get(p).await? {
                if now.as_ref().is_none_or(|n| n.key != was.key) {
                    candidates.push(was.key);
                }
            }
        }
        if candidates.is_empty() {
            return Ok(vec![]);
        }
        let aliases = self.aliases().await?;
        let mut by_key: HashMap<&str, Vec<&str>> = HashMap::new();
        for (path, key) in &aliases {
            by_key.entry(key.as_str()).or_default().push(path.as_str());
        }
        let overlay_cites: HashSet<&str> = overlay.values().flatten().map(|e| e.key.as_str()).collect();
        let mut out = vec![];
        for k in candidates {
            let cited = overlay_cites.contains(k.as_str())
                || by_key.get(k.as_str()).is_some_and(|ps| ps.iter().any(|p| !overlay.contains_key(*p)))
                || match key_path(&self.cfg.prefix, &k).filter(|q| !overlay.contains_key(*q)) {
                    Some(q) => self.get(q).await?.is_some_and(|e| e.key == k),
                    None => false,
                };
            if !cited {
                out.push(k);
            }
        }
        Ok(out)
    }

    /// Every alias of the document: each chunk's, from the cache by
    /// address, the chunks the cache lacks parsed. What the cache lacks is
    /// read from this view's own parse, never from the cache a second time:
    /// a prune between the two reads would drop aliases silently, and a
    /// missing alias reads as a handle nothing cites.
    pub async fn aliases(&mut self) -> LeanResult<Vec<(String, String)>> {
        let prefix = self.cfg.prefix.clone();
        let mut out: Vec<(String, String)> = vec![];
        let mut missing: Vec<usize> = vec![];
        {
            let cache = self.cfg.alias_cache.lock().unwrap();
            for (i, r) in self.refs.iter().enumerate() {
                match cache.get(&r.addr) {
                    Some(a) => out.extend(a.iter().cloned()),
                    None => missing.push(i),
                }
            }
        }
        let unread: Vec<usize> = missing.iter().copied().filter(|i| !self.parsed.contains_key(i)).collect();
        self.read_chunks(unread).await?;
        for i in missing {
            out.extend(aliases_in(&prefix, &self.parsed[&i]));
        }
        Ok(out)
    }

    /// Forget the aliases of the chunks `live` does not name: the cache then
    /// holds what the next commit can ask about.
    pub fn retain_aliases(&self, live: &HashSet<&str>) {
        self.cfg.alias_cache.lock().unwrap().retain(|a, _| live.contains(a.as_str()));
    }

    /// The whole document, as a full load would give it: for the
    /// collection pass, the event trace and the shadow check.
    pub async fn assemble(&mut self) -> LeanResult<LoadedManifest> {
        let missing: Vec<usize> = (0..self.refs.len()).filter(|i| !self.parsed.contains_key(i)).collect();
        self.read_chunks(missing).await?;
        let mut entries = BTreeMap::new();
        for i in 0..self.refs.len() {
            for (k, v) in self.parsed[&i].iter() {
                entries.insert(k.clone(), v.clone());
            }
        }
        Ok(LoadedManifest {
            manifest: LeanManifest {
                seq: self.pointer.seq,
                entries,
                sole_writer: self.pointer.sole_writer,
                boundary_source: self.pointer.boundary_source.clone(),
                tombstones: self.tombstones.clone(),
            },
            etag: self.etag.clone(),
            last_modified_unix: self.last_modified_unix,
            pointer: Some(self.pointer.clone()),
        })
    }
}

/// One object's body: from the cache where it hashes to its address, else
/// from the bucket, checked and kept. Gone from the bucket after the LIST
/// showed it is a hole: live chunks are never swept, and a retired one
/// outlives its retirement by the retire age.
async fn fetch_body(
    store: &dyn ObjectStore,
    cfg: &LeanConfig,
    addr: &str,
    seq: u64,
    hole: impl Fn() -> Hole,
) -> LeanResult<bytes::Bytes> {
    let cache = cfg.chunk_cache.as_deref();
    if let Some(b) = cache.and_then(|c| c.get(addr)) {
        if chunk::chunk_address(&b) == addr {
            return Ok(b);
        }
        if let Some(c) = cache {
            c.remove(addr);
        }
    }
    let body = match store.get_whole(&cfg.chunk_key(addr), None).await {
        Ok((_, b)) => b,
        Err(StoreError::NotFound(_)) => return Err(LeanError::State(hole().refusal(seq))),
        Err(e) => return Err(e.into()),
    };
    chunk::check_address_of(&body, addr)?;
    if let Some(c) = cache {
        c.put(addr, &body);
    }
    Ok(body)
}

/// What a commit installed, for the steps after its CAS: the whole
/// documents on the full path, a view and the overlay on lever G's.
pub enum CommitDocs {
    Full { theirs: LeanManifest, installed: LeanManifest },
    View {
        theirs: DocView,
        overlay: BTreeMap<String, Option<LeanEntry>>,
        /// The retire set, computed for the retire log before the CAS.
        retired: Vec<String>,
        seq: u64,
        tombstones: BTreeMap<String, Tombstone>,
        sole_writer: bool,
        boundary_source: Option<String>,
    },
}

impl CommitDocs {
    pub fn installed_seq(&self) -> u64 {
        match self {
            CommitDocs::Full { installed, .. } => installed.seq,
            CommitDocs::View { seq, .. } => *seq,
        }
    }

    /// The handles the installed document no longer cites anywhere, of
    /// those theirs cited at a path the install changed.
    pub fn retired(&self) -> Vec<String> {
        match self {
            CommitDocs::Full { theirs, installed } => {
                let still_cited: HashSet<&str> = installed.entries.values().map(|e| e.key.as_str()).collect();
                let mut retired: Vec<String> = vec![];
                for (path, was) in &theirs.entries {
                    let dropped = installed.entries.get(path).map(|now| now.key != was.key).unwrap_or(true);
                    if dropped && !still_cited.contains(was.key.as_str()) {
                        retired.push(was.key.clone());
                    }
                }
                retired
            }
            CommitDocs::View { retired, .. } => retired.clone(),
        }
    }

    pub async fn installed_get(&mut self, p: &str) -> LeanResult<Option<LeanEntry>> {
        match self {
            CommitDocs::Full { installed, .. } => Ok(installed.entries.get(p).cloned()),
            CommitDocs::View { theirs, overlay, .. } => match overlay.get(p) {
                Some(v) => Ok(v.clone()),
                None => theirs.get(p).await,
            },
        }
    }

    pub async fn theirs_get(&mut self, p: &str) -> LeanResult<Option<LeanEntry>> {
        match self {
            CommitDocs::Full { theirs, .. } => Ok(theirs.entries.get(p).cloned()),
            CommitDocs::View { theirs, .. } => theirs.get(p).await,
        }
    }

    /// The installed document's entries at `paths` alone, for a caller that
    /// reads only those (`note_carrier_uncited`).
    pub async fn installed_at(&mut self, paths: &[String]) -> LeanResult<LeanManifest> {
        let mut entries = BTreeMap::new();
        if let CommitDocs::View { theirs, .. } = self {
            theirs.prefetch(paths.iter().map(|p| p.as_str())).await?;
        }
        for p in paths {
            if let Some(e) = self.installed_get(p).await? {
                entries.insert(p.clone(), e);
            }
        }
        Ok(LeanManifest { seq: self.installed_seq(), entries, ..Default::default() })
    }

    /// The whole installed document: the collection pass's.
    pub async fn installed_full(&mut self) -> LeanResult<std::borrow::Cow<'_, LeanManifest>> {
        match self {
            CommitDocs::Full { installed, .. } => Ok(std::borrow::Cow::Borrowed(installed)),
            CommitDocs::View { theirs, overlay, seq, tombstones, sole_writer, boundary_source, .. } => {
                let mut m = theirs.assemble().await?.manifest;
                for (p, v) in overlay.iter() {
                    match v {
                        Some(e) => {
                            m.entries.insert(p.clone(), e.clone());
                        }
                        None => {
                            m.entries.remove(p);
                        }
                    }
                }
                m.seq = *seq;
                m.tombstones = tombstones.clone();
                m.sole_writer = *sole_writer;
                m.boundary_source = boundary_source.clone();
                Ok(std::borrow::Cow::Owned(m))
            }
        }
    }
}
