//! `DashMap`, plus a scheduling point in front of every operation when
//! the `shuttle-test` feature is on. A type alias, not a wrapper, in
//! the normal build: `Map<K, V>` IS `DashMap<K, V>` and nothing here
//! costs anything.
//!
//! **Why this has to exist, learned the expensive way.** shuttle only
//! preempts at primitives it owns. The delegation table's hot indices
//! are `DashMap`s, which it does not own, so with only the entry
//! `Mutex` aliased the scheduler had nowhere to interleave *between*
//! two map operations — and that is exactly where the publish-order
//! defect lived:
//!
//! ```text
//!     by_stateid.insert(...)          ← the record becomes findable
//!     by_client.entry(...).push(...)  ← no shuttle primitive anywhere
//!     *live_per_client.entry(...) += 1   in this window
//! ```
//!
//! A returner reaching the record through `by_stateid` in that window
//! decrements a counter that has not been incremented yet, and the
//! increment then lands on a record that no longer exists. Every step
//! is a `DashMap` call, so shuttle explored the whole sequence as one
//! atomic block and the check passed against the KNOWN-BAD ordering —
//! a green result that proved nothing. Yielding before each operation
//! is what gives the scheduler the seam.
//!
//! The yield goes at the START of each method, before the shard lock is
//! taken, and never while a guard is outstanding: parking a coroutine
//! that holds a real `DashMap` shard lock blocks the one OS thread
//! every other coroutine runs on, which hangs the execution instead of
//! failing it. See `gc_map_entry` for the one place that could not be
//! made safe and is disabled under the feature instead.

#[cfg(not(feature = "shuttle-test"))]
pub type Map<K, V> = dashmap::DashMap<K, V>;

#[cfg(feature = "shuttle-test")]
pub use sched::Map;

#[cfg(feature = "shuttle-test")]
mod sched {
    use dashmap::iter::Iter;
    use dashmap::mapref::entry::Entry;
    use dashmap::mapref::one::{Ref, RefMut};
    use dashmap::DashMap;
    use std::borrow::Borrow;
    use std::hash::Hash;

    /// A scheduling point. `yield_now` is a context switch in shuttle
    /// and a no-op cost the normal build never pays, because the normal
    /// build does not use this type at all.
    #[inline]
    fn seam() {
        shuttle::thread::yield_now();
    }

    pub struct Map<K, V>(DashMap<K, V>);

    impl<K: Eq + Hash + Clone, V> Map<K, V> {
        pub fn new() -> Self {
            Self(DashMap::new())
        }
        pub fn get<Q>(&self, k: &Q) -> Option<Ref<'_, K, V>>
        where
            K: Borrow<Q>,
            Q: Hash + Eq + ?Sized,
        {
            seam();
            self.0.get(k)
        }
        pub fn get_mut<Q>(&self, k: &Q) -> Option<RefMut<'_, K, V>>
        where
            K: Borrow<Q>,
            Q: Hash + Eq + ?Sized,
        {
            seam();
            self.0.get_mut(k)
        }
        pub fn insert(&self, k: K, v: V) -> Option<V> {
            seam();
            self.0.insert(k, v)
        }
        pub fn remove<Q>(&self, k: &Q) -> Option<(K, V)>
        where
            K: Borrow<Q>,
            Q: Hash + Eq + ?Sized,
        {
            seam();
            self.0.remove(k)
        }
        pub fn remove_if<Q>(&self, k: &Q, f: impl FnOnce(&K, &V) -> bool) -> Option<(K, V)>
        where
            K: Borrow<Q>,
            Q: Hash + Eq + ?Sized,
        {
            seam();
            self.0.remove_if(k, f)
        }
        pub fn contains_key<Q>(&self, k: &Q) -> bool
        where
            K: Borrow<Q>,
            Q: Hash + Eq + ?Sized,
        {
            seam();
            self.0.contains_key(k)
        }
        pub fn entry(&self, k: K) -> Entry<'_, K, V> {
            seam();
            self.0.entry(k)
        }
        pub fn iter(&self) -> Iter<'_, K, V> {
            seam();
            self.0.iter()
        }
        pub fn len(&self) -> usize {
            self.0.len()
        }
        pub fn is_empty(&self) -> bool {
            self.0.is_empty()
        }
    }
}
