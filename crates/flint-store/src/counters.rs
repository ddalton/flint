//! Requests a store has sent, by kind.
//!
//! What a backend bills by, and what an idle workspace must keep flat: a
//! syncer that trades empty generations with a peer, or retries in a
//! loop, shows up here before it shows up on an invoice. Counted once per
//! request the store issues — a HEAD that precedes a COPY is two — and
//! never per SDK retry of the same request.

use std::sync::atomic::{AtomicU64, Ordering};

/// A point-in-time copy of a store's request counts.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RequestCounts {
    pub get: u64,
    pub head: u64,
    pub put: u64,
    pub copy: u64,
    pub delete: u64,
    pub list: u64,
    /// CreateMultipartUpload, CompleteMultipartUpload, AbortMultipartUpload
    /// (each part is a `put` or a `copy`).
    pub multipart: u64,
}

impl RequestCounts {
    pub fn total(&self) -> u64 {
        self.get + self.head + self.put + self.copy + self.delete + self.list + self.multipart
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestKind {
    Get,
    Head,
    Put,
    Copy,
    Delete,
    List,
    Multipart,
}

/// Lock-free counters a store bumps as it sends.
#[derive(Debug, Default)]
pub struct RequestCounter {
    get: AtomicU64,
    head: AtomicU64,
    put: AtomicU64,
    copy: AtomicU64,
    delete: AtomicU64,
    list: AtomicU64,
    multipart: AtomicU64,
}

impl RequestCounter {
    pub fn bump(&self, kind: RequestKind) {
        let c = match kind {
            RequestKind::Get => &self.get,
            RequestKind::Head => &self.head,
            RequestKind::Put => &self.put,
            RequestKind::Copy => &self.copy,
            RequestKind::Delete => &self.delete,
            RequestKind::List => &self.list,
            RequestKind::Multipart => &self.multipart,
        };
        c.fetch_add(1, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> RequestCounts {
        RequestCounts {
            get: self.get.load(Ordering::Relaxed),
            head: self.head.load(Ordering::Relaxed),
            put: self.put.load(Ordering::Relaxed),
            copy: self.copy.load(Ordering::Relaxed),
            delete: self.delete.load(Ordering::Relaxed),
            list: self.list.load(Ordering::Relaxed),
            multipart: self.multipart.load(Ordering::Relaxed),
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::memory::MemoryStore;
    use crate::{crc64_nvme, GenerationStamps, ObjectStore, PutCondition};
    use bytes::Bytes;

    /// Each request lands in its own kind, and a store that counts says
    /// so. The memory double folds its per-method calls into the kinds a
    /// real store would have sent.
    #[tokio::test]
    async fn requests_are_counted_by_kind() {
        let s = MemoryStore::new();
        let st = GenerationStamps { generation: 1, epoch: 0, flush_uuid: "t".into(), boundary_source: None, posix: None };
        let m = s.put_whole("k", Bytes::from_static(b"x"), &PutCondition::IfNoneMatchAny, &st, crc64_nvme(b"x")).await.unwrap();
        s.head("k").await.unwrap();
        s.get_whole("k", None).await.unwrap();
        s.get_whole("k", None).await.unwrap();
        s.list("").await.unwrap();
        s.delete_if_match("k", &m.etag).await.unwrap();
        let c = s.request_counts().expect("the double counts");
        assert_eq!((c.put, c.head, c.get, c.list, c.delete, c.copy, c.multipart), (1, 1, 2, 1, 1, 0, 0), "{c:?}");
        assert_eq!(c.total(), 6);
    }
}
