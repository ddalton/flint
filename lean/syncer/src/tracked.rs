//! Collections that remember which keys changed since they were loaded,
//! so the baseline persists what a save changed rather than the whole
//! document (lever D, `docs/plans/flint-lean-latency-plan-2026-10-23.md`).
//!
//! Reads go through `Deref` to the plain `BTreeMap` / `BTreeSet`. There is
//! deliberately no `DerefMut`: every mutation goes through a method here
//! that records the key, so a mutation the journal would miss does not
//! compile. A key is recorded whether or not the mutation changed its
//! value (`get_mut`, a no-op `insert`): the journal may say more than
//! changed, never less.
//!
//! What it cannot record is a collection REPLACED whole: an assignment
//! drops the old one, and with it the members the new one lacks. So each
//! collection carries the `lineage` it was loaded under. The store writes
//! only the recorded keys when every collection is still of the lineage
//! the disk holds, and the whole document otherwise. Anything built here
//! rather than loaded (`Default`, `FromIterator`) is of lineage 0, which
//! the disk never holds.

use std::borrow::Borrow;
use std::collections::{BTreeMap, BTreeSet};
use std::ops::Deref;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// A `BTreeMap<String, V>` that records the keys it was changed at.
#[derive(Debug, Clone)]
pub struct TrackedMap<V> {
    inner: BTreeMap<String, V>,
    touched: BTreeSet<String>,
    lineage: u64,
}

impl<V> Default for TrackedMap<V> {
    fn default() -> Self {
        Self { inner: BTreeMap::new(), touched: BTreeSet::new(), lineage: 0 }
    }
}

impl<V> Deref for TrackedMap<V> {
    type Target = BTreeMap<String, V>;
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl<V: PartialEq> PartialEq for TrackedMap<V> {
    fn eq(&self, other: &Self) -> bool {
        self.inner == other.inner
    }
}

impl<'a, V> IntoIterator for &'a TrackedMap<V> {
    type Item = (&'a String, &'a V);
    type IntoIter = std::collections::btree_map::Iter<'a, String, V>;
    fn into_iter(self) -> Self::IntoIter {
        self.inner.iter()
    }
}

impl<V> TrackedMap<V> {
    pub fn insert(&mut self, k: String, v: V) -> Option<V> {
        self.touched.insert(k.clone());
        self.inner.insert(k, v)
    }

    pub fn remove<Q>(&mut self, k: &Q) -> Option<V>
    where
        String: Borrow<Q>,
        Q: Ord + ?Sized + ToOwned<Owned = String>,
    {
        let v = self.inner.remove(k);
        if v.is_some() {
            self.touched.insert(k.to_owned());
        }
        v
    }

    pub fn get_mut<Q>(&mut self, k: &Q) -> Option<&mut V>
    where
        String: Borrow<Q>,
        Q: Ord + ?Sized + ToOwned<Owned = String>,
    {
        if self.inner.contains_key(k) {
            self.touched.insert(k.to_owned());
        }
        self.inner.get_mut(k)
    }

    pub fn clear(&mut self) {
        let old = std::mem::take(&mut self.inner);
        self.touched.extend(old.into_keys());
    }

    pub fn retain(&mut self, mut keep: impl FnMut(&String, &mut V) -> bool) {
        let touched = &mut self.touched;
        self.inner.retain(|k, v| {
            let kept = keep(k, v);
            if !kept {
                touched.insert(k.clone());
            }
            kept
        });
    }

    /// The keys changed since the last call, and forget them.
    pub(crate) fn take_touched(&mut self) -> BTreeSet<String> {
        std::mem::take(&mut self.touched)
    }

    pub(crate) fn touched_len(&self) -> usize {
        self.touched.len()
    }

    /// Replay a journal record: no key is recorded as changed.
    pub(crate) fn lineage(&self) -> u64 {
        self.lineage
    }

    /// The store's mark: this map is the disk's state at `lineage`, plus
    /// the changes it records.
    pub(crate) fn stamp(&mut self, lineage: u64) {
        self.lineage = lineage;
    }

    pub(crate) fn apply_put(&mut self, k: String, v: V) {
        self.inner.insert(k, v);
    }

    pub(crate) fn apply_del(&mut self, k: &str) {
        self.inner.remove(k);
    }
}

impl<V> FromIterator<(String, V)> for TrackedMap<V> {
    /// A map built whole: every key counts as changed.
    fn from_iter<I: IntoIterator<Item = (String, V)>>(iter: I) -> Self {
        let inner: BTreeMap<String, V> = iter.into_iter().collect();
        let touched = inner.keys().cloned().collect();
        Self { inner, touched, lineage: 0 }
    }
}

impl<V: Serialize> Serialize for TrackedMap<V> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        self.inner.serialize(s)
    }
}

impl<'de, V: Deserialize<'de>> Deserialize<'de> for TrackedMap<V> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Ok(Self { inner: BTreeMap::deserialize(d)?, touched: BTreeSet::new(), lineage: 0 })
    }
}

/// A `BTreeSet<String>` that records the members it gained or lost.
#[derive(Debug, Clone, Default)]
pub struct TrackedSet {
    inner: BTreeSet<String>,
    touched: BTreeSet<String>,
    lineage: u64,
}

impl Deref for TrackedSet {
    type Target = BTreeSet<String>;
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl PartialEq for TrackedSet {
    fn eq(&self, other: &Self) -> bool {
        self.inner == other.inner
    }
}

impl<'a> IntoIterator for &'a TrackedSet {
    type Item = &'a String;
    type IntoIter = std::collections::btree_set::Iter<'a, String>;
    fn into_iter(self) -> Self::IntoIter {
        self.inner.iter()
    }
}

impl TrackedSet {
    pub fn insert(&mut self, k: String) -> bool {
        let added = self.inner.insert(k.clone());
        if added {
            self.touched.insert(k);
        }
        added
    }

    pub fn remove<Q>(&mut self, k: &Q) -> bool
    where
        String: Borrow<Q>,
        Q: Ord + ?Sized + ToOwned<Owned = String>,
    {
        let removed = self.inner.remove(k);
        if removed {
            self.touched.insert(k.to_owned());
        }
        removed
    }

    pub fn clear(&mut self) {
        let old = std::mem::take(&mut self.inner);
        self.touched.extend(old);
    }

    /// Become exactly `members`, which must be sorted and free of
    /// duplicates (a `BTreeMap`'s or `BTreeSet`'s keys). One merge walk:
    /// only the members gained or lost are copied, so a set of a million
    /// that did not change costs comparisons, not a million allocations.
    pub fn sync_to<'a>(&mut self, members: impl IntoIterator<Item = &'a String>) {
        let mut add: Vec<String> = vec![];
        let mut del: Vec<String> = vec![];
        {
            let mut have = self.inner.iter().peekable();
            for want in members {
                loop {
                    match have.peek() {
                        Some(h) if *h < want => {
                            del.push((*h).clone());
                            have.next();
                        }
                        Some(h) if *h == want => {
                            have.next();
                            break;
                        }
                        _ => {
                            add.push(want.clone());
                            break;
                        }
                    }
                }
            }
            del.extend(have.cloned());
        }
        for k in del {
            self.inner.remove(&k);
            self.touched.insert(k);
        }
        for k in add {
            self.inner.insert(k.clone());
            self.touched.insert(k);
        }
    }

    pub(crate) fn take_touched(&mut self) -> BTreeSet<String> {
        std::mem::take(&mut self.touched)
    }

    pub(crate) fn touched_len(&self) -> usize {
        self.touched.len()
    }

    pub(crate) fn lineage(&self) -> u64 {
        self.lineage
    }

    pub(crate) fn stamp(&mut self, lineage: u64) {
        self.lineage = lineage;
    }

    pub(crate) fn apply_add(&mut self, k: String) {
        self.inner.insert(k);
    }

    pub(crate) fn apply_del(&mut self, k: &str) {
        self.inner.remove(k);
    }
}

impl FromIterator<String> for TrackedSet {
    /// A set built whole: every member counts as changed.
    fn from_iter<I: IntoIterator<Item = String>>(iter: I) -> Self {
        let inner: BTreeSet<String> = iter.into_iter().collect();
        let touched = inner.clone();
        Self { inner, touched, lineage: 0 }
    }
}

impl Serialize for TrackedSet {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        self.inner.serialize(s)
    }
}

impl<'de> Deserialize<'de> for TrackedSet {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Ok(Self { inner: BTreeSet::deserialize(d)?, touched: BTreeSet::new(), lineage: 0 })
    }
}
