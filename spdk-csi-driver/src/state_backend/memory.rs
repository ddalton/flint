//! In-process [`StateBackend`] implementation backed by `DashMap`s and
//! an `AtomicU64`. Behavioural parity with today's
//! `Client`/`Session`/`StateId`/`Layout` managers — choose this when
//! you don't care about restart survival (tests, Lima dev work, smoke
//! runs). Production deployments should pick the SQLite backend
//! shipping in B.2.
//!
//! All operations are constant-time on the underlying DashMap shard,
//! so this stays on the hot path with no measurable overhead vs. the
//! current direct-DashMap accesses. The boundary code (B.3) caches
//! reads in the existing per-manager DashMaps anyway, so the trait
//! cost is paid only on writes.

use super::{
    ClientRecord, LayoutRecord, SessionRecord, StateBackend, StateBackendResult, StateIdRecord,
};
use async_trait::async_trait;
use dashmap::DashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;

/// In-memory [`StateBackend`]. All maps shard internally so concurrent
/// readers and writers don't contend on a global lock.
#[derive(Default)]
pub struct MemoryBackend {
    clients: DashMap<u64, ClientRecord>,
    sessions: DashMap<[u8; 16], SessionRecord>,
    stateids: DashMap<[u8; 12], StateIdRecord>,
    layouts: DashMap<[u8; 16], LayoutRecord>,
    instance_counter: AtomicU64,
    /// Lazily-initialised per-deployment server id. `OnceLock` makes
    /// the first call atomic (no two threads observe different values)
    /// without paying for a mutex on every read.
    server_id: OnceLock<u64>,
}

impl MemoryBackend {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl StateBackend for MemoryBackend {
    async fn put_client(&self, c: &ClientRecord) -> StateBackendResult<()> {
        self.clients.insert(c.client_id, c.clone());
        Ok(())
    }

    async fn get_client(&self, client_id: u64) -> StateBackendResult<Option<ClientRecord>> {
        Ok(self.clients.get(&client_id).map(|r| r.clone()))
    }

    async fn list_clients(&self) -> StateBackendResult<Vec<ClientRecord>> {
        Ok(self.clients.iter().map(|r| r.clone()).collect())
    }

    async fn delete_client(&self, client_id: u64) -> StateBackendResult<()> {
        self.clients.remove(&client_id);
        Ok(())
    }

    async fn put_session(&self, s: &SessionRecord) -> StateBackendResult<()> {
        self.sessions.insert(s.session_id, s.clone());
        Ok(())
    }

    async fn get_session(&self, session_id: &[u8; 16]) -> StateBackendResult<Option<SessionRecord>> {
        Ok(self.sessions.get(session_id).map(|r| r.clone()))
    }

    async fn list_sessions(&self) -> StateBackendResult<Vec<SessionRecord>> {
        Ok(self.sessions.iter().map(|r| r.clone()).collect())
    }

    async fn delete_session(&self, session_id: &[u8; 16]) -> StateBackendResult<()> {
        self.sessions.remove(session_id);
        Ok(())
    }

    async fn put_stateid(&self, s: &StateIdRecord) -> StateBackendResult<()> {
        self.stateids.insert(s.other, s.clone());
        Ok(())
    }

    async fn get_stateid(&self, other: &[u8; 12]) -> StateBackendResult<Option<StateIdRecord>> {
        Ok(self.stateids.get(other).map(|r| r.clone()))
    }

    async fn list_stateids(&self) -> StateBackendResult<Vec<StateIdRecord>> {
        Ok(self.stateids.iter().map(|r| r.clone()).collect())
    }

    async fn delete_stateid(&self, other: &[u8; 12]) -> StateBackendResult<()> {
        self.stateids.remove(other);
        Ok(())
    }

    async fn put_layout(&self, l: &LayoutRecord) -> StateBackendResult<()> {
        self.layouts.insert(l.stateid, l.clone());
        Ok(())
    }

    async fn get_layout(&self, stateid: &[u8; 16]) -> StateBackendResult<Option<LayoutRecord>> {
        Ok(self.layouts.get(stateid).map(|r| r.clone()))
    }

    async fn list_layouts(&self) -> StateBackendResult<Vec<LayoutRecord>> {
        Ok(self.layouts.iter().map(|r| r.clone()).collect())
    }

    async fn delete_layout(&self, stateid: &[u8; 16]) -> StateBackendResult<()> {
        self.layouts.remove(stateid);
        Ok(())
    }

    async fn increment_instance_counter(&self) -> StateBackendResult<u64> {
        // SeqCst rather than Relaxed: callers (B.4 startup logic) treat
        // the post-increment value as a fence — every persisted record
        // written *after* the counter increment must observe the new
        // value. SeqCst makes that intuitive even though Relaxed would
        // suffice for the counter alone, because the counter's value
        // is effectively published through unrelated DashMap writes.
        Ok(self.instance_counter.fetch_add(1, Ordering::SeqCst) + 1)
    }

    async fn get_instance_counter(&self) -> StateBackendResult<u64> {
        Ok(self.instance_counter.load(Ordering::SeqCst))
    }

    async fn get_or_init_server_id(&self) -> StateBackendResult<u64> {
        // `OnceLock::get_or_init` runs the closure exactly once per
        // backend instance, even under concurrent first-call races —
        // no dupe writes, no mutex on the steady-state read path.
        // `rand::random::<u64>() | 1` keeps the value non-zero so a
        // caller treating zero as "uninitialised" still works.
        Ok(*self.server_id.get_or_init(|| rand::random::<u64>() | 1))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state_backend::tests::{
        instance_counter_monotonic, put_overwrites, round_trip_all,
        server_id_stable_and_nonzero,
    };

    #[tokio::test]
    async fn memory_round_trip_all_records() {
        let b = MemoryBackend::new();
        round_trip_all(&b).await;
    }

    #[tokio::test]
    async fn memory_instance_counter_monotonic() {
        let b = MemoryBackend::new();
        instance_counter_monotonic(&b).await;
    }

    #[tokio::test]
    async fn memory_put_is_upsert() {
        let b = MemoryBackend::new();
        put_overwrites(&b).await;
    }

    #[tokio::test]
    async fn memory_server_id_stable_within_lifetime() {
        let b = MemoryBackend::new();
        server_id_stable_and_nonzero(&b).await;
    }

    /// Concurrent writers on different keys don't lose updates and
    /// don't deadlock. The DashMap shard count is the actual mechanism;
    /// this is just a regression sentinel for "did someone wrap it in
    /// a global Mutex".
    #[tokio::test]
    async fn memory_concurrent_writes_no_loss() {
        use std::sync::Arc;
        let b = Arc::new(MemoryBackend::new());
        let mut tasks = Vec::new();
        for i in 0..64u64 {
            let b = Arc::clone(&b);
            tasks.push(tokio::spawn(async move {
                let c = ClientRecord {
                    client_id: i,
                    owner: format!("c{}", i).into_bytes(),
                    verifier: i,
                    server_owner: "s".into(),
                    server_scope: b"sc".to_vec(),
                    sequence_id: 1,
                    flags: 0,
                    principal: b"p".to_vec(),
                    confirmed: false,
                    last_cs_sequence: None,
                    cs_cached_res: None,
                    initial_cs_sequence: 1,
                    reclaim_complete: false,
                };
                b.put_client(&c).await.unwrap();
            }));
        }
        for t in tasks {
            t.await.unwrap();
        }
        assert_eq!(b.list_clients().await.unwrap().len(), 64);
    }

    /// Counter increments are atomic across many concurrent calls — no
    /// duplicate values, no skipped values. Catches the obvious bug of
    /// using `load + 1; store` instead of `fetch_add`.
    #[tokio::test]
    async fn memory_instance_counter_atomic() {
        use std::sync::Arc;
        let b = Arc::new(MemoryBackend::new());
        let mut tasks = Vec::new();
        for _ in 0..32 {
            let b = Arc::clone(&b);
            tasks.push(tokio::spawn(async move {
                b.increment_instance_counter().await.unwrap()
            }));
        }
        let mut seen: Vec<u64> = Vec::new();
        for t in tasks {
            seen.push(t.await.unwrap());
        }
        seen.sort();
        assert_eq!(seen, (1..=32).collect::<Vec<u64>>());
        assert_eq!(b.get_instance_counter().await.unwrap(), 32);
    }
}
