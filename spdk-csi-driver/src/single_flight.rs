//! Single flight for long controller operations (2026-10-22).
//!
//! The external-provisioner cancels CreateVolume at its `--timeout` and
//! retries the same name with backoff. A thick volume is zeroed as it is
//! created (blob-zero-new-clusters.patch), so its create can outlast many
//! such deadlines. [`SingleFlight::run`] runs the work in a detached task, so
//! cancelling one RPC does not cancel the creation, and a retry under the same
//! key joins that task instead of starting a second create -- which would
//! fail "File exists" on the same lvstore, or place a duplicate volume on
//! another node for the orphan sweep to find.
//!
//! The first caller to see a flight finish removes it, so a failed create is
//! retried fresh. A flight that finished while nobody waited (every caller
//! was cut off) keeps its result for the next caller.

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex};
use tokio::sync::watch;

pub struct SingleFlight<P, T> {
    flights: Arc<Mutex<HashMap<String, Flight<P, T>>>>,
}

impl<P, T> Clone for SingleFlight<P, T> {
    fn clone(&self) -> Self {
        Self { flights: self.flights.clone() }
    }
}

impl<P, T> Default for SingleFlight<P, T> {
    fn default() -> Self {
        Self { flights: Arc::new(Mutex::new(HashMap::new())) }
    }
}

struct Flight<P, T> {
    params: P,
    done: watch::Receiver<Option<T>>,
}

#[derive(Debug, PartialEq)]
pub enum Joined<T> {
    /// The work's result -- this caller's own flight or one it joined.
    Done(T),
    /// A flight under this key is running with different parameters.
    ParamsDiffer,
    /// The task ended without a result (it panicked).
    Lost,
}

impl<P, T> SingleFlight<P, T>
where
    P: PartialEq + Send + 'static,
    T: Clone + Send + Sync + 'static,
{
    /// Run `work` under `key`, or join the flight already running under it.
    /// `work` is only called when no flight exists. Dropping the returned
    /// future does not cancel the work.
    pub async fn run<F, Fut>(&self, key: &str, params: P, work: F) -> Joined<T>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = T> + Send + 'static,
    {
        let mut done = {
            let mut flights = self.flights.lock().unwrap();
            match flights.get(key) {
                Some(f) if f.params != params => return Joined::ParamsDiffer,
                Some(f) => f.done.clone(),
                None => {
                    let (tx, rx) = watch::channel(None);
                    flights.insert(key.to_string(), Flight { params, done: rx.clone() });
                    let fut = work();
                    tokio::spawn(async move {
                        let _ = tx.send(Some(fut.await));
                    });
                    rx
                }
            }
        };
        let out = match done.wait_for(Option::is_some).await {
            Ok(v) => Joined::Done(v.clone().expect("wait_for(is_some)")),
            Err(_) => Joined::Lost,
        };
        let mut flights = self.flights.lock().unwrap();
        if flights.get(key).is_some_and(|f| f.done.same_channel(&done)) {
            flights.remove(key);
        }
        out
    }

    /// Flights currently held (running, or finished and not yet seen).
    pub fn len(&self) -> usize {
        self.flights.lock().unwrap().len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;
    use tokio::sync::oneshot;

    fn counted(
        calls: &Arc<AtomicUsize>,
        release: oneshot::Receiver<u32>,
    ) -> impl FnOnce() -> std::pin::Pin<Box<dyn Future<Output = u32> + Send>> {
        let calls = calls.clone();
        move || {
            calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move { release.await.unwrap_or(0) })
        }
    }

    // The provisioner's deadline cuts the first CreateVolume off mid-create;
    // its retry joins the same work instead of starting a second create.
    #[tokio::test]
    async fn a_retry_joins_the_flight_its_cancelled_first_attempt_started() {
        let sf: SingleFlight<u64, u32> = SingleFlight::default();
        let calls = Arc::new(AtomicUsize::new(0));
        let (tx, rx) = oneshot::channel();

        let first = {
            let (sf, w) = (sf.clone(), counted(&calls, rx));
            tokio::spawn(async move { sf.run("pvc-1", 100, w).await })
        };
        tokio::time::sleep(Duration::from_millis(20)).await;
        first.abort(); // the CO deadline
        let _ = first.await;
        assert_eq!(sf.len(), 1, "the work outlives its caller");

        let retry = {
            let sf = sf.clone();
            let never: oneshot::Receiver<u32> = oneshot::channel().1;
            let w = counted(&calls, never);
            tokio::spawn(async move { sf.run("pvc-1", 100, w).await })
        };
        tokio::time::sleep(Duration::from_millis(20)).await;
        tx.send(7).unwrap();
        assert_eq!(retry.await.unwrap(), Joined::Done(7));
        assert_eq!(calls.load(Ordering::SeqCst), 1, "one create, not two");
        assert_eq!(sf.len(), 0, "seen finished: removed");
    }

    // Every caller was cut off before the work finished: the result waits
    // for the next one, which gets it without running the work again.
    #[tokio::test]
    async fn a_result_nobody_saw_is_kept_for_the_next_caller() {
        let sf: SingleFlight<u64, u32> = SingleFlight::default();
        let calls = Arc::new(AtomicUsize::new(0));
        let (tx, rx) = oneshot::channel();
        let first = {
            let (sf, w) = (sf.clone(), counted(&calls, rx));
            tokio::spawn(async move { sf.run("pvc-2", 1, w).await })
        };
        tokio::time::sleep(Duration::from_millis(20)).await;
        first.abort();
        let _ = first.await;
        tx.send(9).unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(sf.len(), 1);

        let never: oneshot::Receiver<u32> = oneshot::channel().1;
        assert_eq!(sf.run("pvc-2", 1, counted(&calls, never)).await, Joined::Done(9));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(sf.len(), 0);
    }

    // Once seen, a flight is gone: a create that failed is retried fresh.
    #[tokio::test]
    async fn a_finished_flight_once_seen_runs_again() {
        let sf: SingleFlight<u64, u32> = SingleFlight::default();
        let calls = Arc::new(AtomicUsize::new(0));
        for want in [1u32, 2] {
            let (tx, rx) = oneshot::channel();
            tx.send(want).unwrap();
            assert_eq!(sf.run("pvc-3", 5, counted(&calls, rx)).await, Joined::Done(want));
        }
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn a_different_request_under_a_running_name_is_refused() {
        let sf: SingleFlight<u64, u32> = SingleFlight::default();
        let calls = Arc::new(AtomicUsize::new(0));
        let (tx, rx) = oneshot::channel();
        let first = {
            let (sf, w) = (sf.clone(), counted(&calls, rx));
            tokio::spawn(async move { sf.run("pvc-4", 10, w).await })
        };
        tokio::time::sleep(Duration::from_millis(20)).await;
        let never: oneshot::Receiver<u32> = oneshot::channel().1;
        assert_eq!(sf.run("pvc-4", 20, counted(&calls, never)).await, Joined::ParamsDiffer);
        tx.send(3).unwrap();
        assert_eq!(first.await.unwrap(), Joined::Done(3));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}
