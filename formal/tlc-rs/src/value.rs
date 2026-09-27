//! Values. Every compound value is canonical, so structural equality is
//! TLA+ equality and one hash is one fingerprint. The canonical order is
//! TLC's own `compareTo` order — strings and model values are interned in
//! TLC's token order for exactly this — so a set iterates, and a SYMMETRY
//! representative is chosen, the way TLC does it:
//!  - sets are sorted and deduplicated;
//!  - a function whose domain is 1..n (including the empty function) is a
//!    `Seq`, so `<<a, b>>` = `[i \in 1..2 |-> ...]` holds structurally;
//!  - records are functions from interned strings.
//! Infinite or large sets (`Nat`, `[S -> T]`, `SUBSET S`) stay `Lazy` until
//! something enumerates them; membership never needs to.

use std::cmp::Ordering;
use std::fmt;
use std::sync::{Arc, OnceLock};

pub type R<T> = Result<T, String>;

#[derive(Clone)]
pub enum Value {
    Bool(bool),
    Int(i64),
    Str(u32),
    Model(u32),
    Set(Arc<[Value]>),
    Seq(Arc<[Value]>),
    Func(Arc<Func>),
    Lazy(Arc<Lazy>),
}

pub struct Func {
    /// Shared: EXCEPT changes values, never the domain.
    pub keys: Arc<[Value]>,
    pub vals: Box<[Value]>,
}

pub enum Lazy {
    Nat,
    Int,
    Strings,
    Subset(Value),
    FuncSet(Value, Value),
    RecSet(Box<[(u32, Value)]>),
    Product(Box<[Value]>),
    SeqOf(Value),
}

// ---- interned names -----------------------------------------------------

/// Strings and model-value names, frozen before checking starts.
pub static NAMES: OnceLock<Vec<String>> = OnceLock::new();

pub fn name(id: u32) -> &'static str {
    NAMES.get().map(|n| n[id as usize].as_str()).unwrap_or("?")
}

// ---- construction -------------------------------------------------------

pub fn empty_set() -> Value {
    Value::Set(Arc::from([]))
}

impl Value {
    fn tag(&self) -> u8 {
        match self {
            // A model value is below every other kind, as in TLC.
            Value::Model(_) => 0,
            Value::Bool(_) => 1,
            Value::Int(_) => 2,
            Value::Str(_) => 3,
            Value::Set(_) => 4,
            Value::Seq(_) => 5,
            Value::Func(_) => 6,
            Value::Lazy(_) => 7,
        }
    }

    /// A set from arbitrary elements: normalizes lazies, sorts, dedups.
    pub fn set(mut v: Vec<Value>) -> R<Value> {
        for x in v.iter_mut() {
            if let Value::Lazy(_) = x {
                *x = x.materialize()?;
            }
        }
        v.sort_unstable();
        v.dedup();
        Ok(Value::Set(Arc::from(v)))
    }

    /// A set from elements already sorted and unique.
    pub fn set_sorted(v: Vec<Value>) -> Value {
        debug_assert!(v.windows(2).all(|w| w[0] < w[1]));
        Value::Set(Arc::from(v))
    }

    /// A function from (key, value) pairs; the first binding of a key wins.
    pub fn func(mut pairs: Vec<(Value, Value)>) -> R<Value> {
        for (k, v) in pairs.iter_mut() {
            if let Value::Lazy(_) = k {
                *k = k.materialize()?;
            }
            if let Value::Lazy(_) = v {
                *v = v.materialize()?;
            }
        }
        pairs.sort_by(|a, b| a.0.cmp(&b.0));
        pairs.dedup_by(|b, a| a.0 == b.0);
        Ok(Self::func_sorted(pairs))
    }

    pub fn func_sorted(pairs: Vec<(Value, Value)>) -> Value {
        let is_seq = pairs.iter().enumerate().all(|(i, (k, _))| matches!(k, Value::Int(n) if *n == i as i64 + 1));
        if is_seq {
            return Value::Seq(pairs.into_iter().map(|(_, v)| v).collect());
        }
        let (keys, vals): (Vec<_>, Vec<_>) = pairs.into_iter().unzip();
        Value::Func(Arc::new(Func { keys: keys.into(), vals: vals.into() }))
    }

    /// Stored values (state variables, set elements) are never lazy.
    pub fn normalized(self) -> R<Value> {
        match self {
            Value::Lazy(_) => self.materialize(),
            v => Ok(v),
        }
    }

    // ---- accessors ------------------------------------------------------

    pub fn as_bool(&self) -> R<bool> {
        match self {
            Value::Bool(b) => Ok(*b),
            v => Err(format!("expected a boolean, got {v}")),
        }
    }

    pub fn as_int(&self) -> R<i64> {
        match self {
            Value::Int(n) => Ok(*n),
            v => Err(format!("expected an integer, got {v}")),
        }
    }

    /// The elements of a finite set, enumerating a lazy one.
    pub fn elems(&self) -> R<Arc<[Value]>> {
        match self {
            Value::Set(s) => Ok(s.clone()),
            Value::Lazy(_) => match self.materialize()? {
                Value::Set(s) => Ok(s),
                _ => unreachable!(),
            },
            v => Err(format!("expected a set, got {v}")),
        }
    }

    pub fn materialize(&self) -> R<Value> {
        let Value::Lazy(l) = self else { return Ok(self.clone()) };
        Ok(match &**l {
            Lazy::Nat | Lazy::Int | Lazy::Strings | Lazy::SeqOf(_) => {
                return Err(format!("cannot enumerate the infinite set {self}"));
            }
            Lazy::Subset(s) => {
                let base = s.elems()?;
                if base.len() > 24 {
                    return Err("SUBSET of a set larger than 24 elements".into());
                }
                let mut out = Vec::with_capacity(1 << base.len());
                for mask in 0u32..(1 << base.len()) {
                    let sub: Vec<Value> =
                        (0..base.len()).filter(|i| mask & (1 << i) != 0).map(|i| base[i].clone()).collect();
                    out.push(Value::set_sorted(sub));
                }
                return Value::set(out);
            }
            Lazy::FuncSet(d, r) => {
                let dom = d.elems()?;
                let rng = r.elems()?;
                let mut out = Vec::new();
                let mut idx = vec![0usize; dom.len()];
                if rng.is_empty() && !dom.is_empty() {
                    return Ok(empty_set());
                }
                loop {
                    let pairs = dom.iter().zip(&idx).map(|(k, &i)| (k.clone(), rng[i].clone())).collect();
                    out.push(Value::func_sorted(pairs));
                    // odometer
                    let mut p = dom.len();
                    loop {
                        if p == 0 {
                            return Value::set(out);
                        }
                        p -= 1;
                        idx[p] += 1;
                        if idx[p] < rng.len() {
                            break;
                        }
                        idx[p] = 0;
                    }
                }
            }
            Lazy::RecSet(fields) => {
                let sets: Vec<Arc<[Value]>> = fields.iter().map(|(_, s)| s.elems()).collect::<R<_>>()?;
                let mut out = Vec::new();
                product(&sets, &mut Vec::new(), &mut |vals| {
                    let pairs = fields.iter().zip(vals).map(|((f, _), v)| (Value::Str(*f), v.clone())).collect();
                    out.push(Value::func_sorted(pairs));
                });
                Value::set(out)?
            }
            Lazy::Product(sets) => {
                let sets: Vec<Arc<[Value]>> = sets.iter().map(|s| s.elems()).collect::<R<_>>()?;
                let mut out = Vec::new();
                product(&sets, &mut Vec::new(), &mut |vals| out.push(Value::Seq(vals.iter().cloned().collect())));
                Value::set(out)?
            }
        })
    }

    pub fn contains(&self, x: &Value) -> R<bool> {
        match self {
            Value::Set(s) => Ok(s.binary_search(x).is_ok()),
            Value::Lazy(l) => match &**l {
                Lazy::Nat => Ok(matches!(x, Value::Int(n) if *n >= 0)),
                Lazy::Int => Ok(matches!(x, Value::Int(_))),
                Lazy::Strings => Ok(matches!(x, Value::Str(_))),
                Lazy::Subset(s) => match x {
                    Value::Set(xs) => {
                        for e in xs.iter() {
                            if !s.contains(e)? {
                                return Ok(false);
                            }
                        }
                        Ok(true)
                    }
                    _ => Ok(false),
                },
                Lazy::SeqOf(s) => match x {
                    Value::Seq(xs) => {
                        for e in xs.iter() {
                            if !s.contains(e)? {
                                return Ok(false);
                            }
                        }
                        Ok(true)
                    }
                    _ => Ok(false),
                },
                Lazy::FuncSet(d, r) => {
                    let dom = d.elems()?;
                    let ok_dom = match x {
                        Value::Seq(xs) => {
                            dom.len() == xs.len()
                                && dom.iter().enumerate().all(|(i, k)| matches!(k, Value::Int(n) if *n == i as i64 + 1))
                        }
                        Value::Func(f) => *f.keys == *dom,
                        _ => false,
                    };
                    if !ok_dom {
                        return Ok(false);
                    }
                    let vals: &[Value] = match x {
                        Value::Seq(xs) => xs,
                        Value::Func(f) => &f.vals,
                        _ => unreachable!(),
                    };
                    for v in vals {
                        if !r.contains(v)? {
                            return Ok(false);
                        }
                    }
                    Ok(true)
                }
                Lazy::RecSet(fields) => match x {
                    Value::Func(f) if f.keys.len() == fields.len() => {
                        // fields are sorted by string id, as record keys are
                        for ((fk, fs), (k, v)) in fields.iter().zip(f.keys.iter().zip(f.vals.iter())) {
                            if !matches!(k, Value::Str(s) if s == fk) || !fs.contains(v)? {
                                return Ok(false);
                            }
                        }
                        Ok(true)
                    }
                    _ => Ok(false),
                },
                Lazy::Product(sets) => match x {
                    Value::Seq(xs) if xs.len() == sets.len() => {
                        for (s, e) in sets.iter().zip(xs.iter()) {
                            if !s.contains(e)? {
                                return Ok(false);
                            }
                        }
                        Ok(true)
                    }
                    _ => Ok(false),
                },
            },
            v => Err(format!("\\in applied to a non-set {v}")),
        }
    }

    /// `f[x]` without cloning f's other values.
    #[inline]
    pub fn apply_ref(&self, x: &Value) -> R<&Value> {
        match self {
            Value::Seq(s) => {
                if let Value::Int(i) = x {
                    if *i >= 1 && (*i as usize) <= s.len() {
                        return Ok(&s[*i as usize - 1]);
                    }
                }
                Err(format!("{x} is not in the domain of {self}"))
            }
            Value::Func(f) => match f.keys.binary_search(x) {
                Ok(i) => Ok(&f.vals[i]),
                Err(_) => Err(format!("{x} is not in the domain of {self}")),
            },
            _ => Err(format!("applying a non-function {self}")),
        }
    }

    #[inline]
    pub fn field_ref(&self, id: u32) -> R<&Value> {
        if let Value::Func(f) = self {
            for (k, v) in f.keys.iter().zip(f.vals.iter()) {
                if matches!(k, Value::Str(s) if *s == id) {
                    return Ok(v);
                }
            }
        }
        Err(format!("{self} has no field {}", name(id)))
    }

    /// Function application `f[x]`.
    pub fn apply(&self, x: &Value) -> R<Value> {
        match self {
            Value::Seq(s) => {
                if let Value::Int(i) = x {
                    if *i >= 1 && (*i as usize) <= s.len() {
                        return Ok(s[*i as usize - 1].clone());
                    }
                }
                Err(format!("{x} is not in the domain of {self}"))
            }
            Value::Func(f) => match f.keys.binary_search(x) {
                Ok(i) => Ok(f.vals[i].clone()),
                Err(_) => Err(format!("{x} is not in the domain of {self}")),
            },
            _ => Err(format!("applying a non-function {self}")),
        }
    }

    pub fn field(&self, id: u32) -> R<Value> {
        match self {
            Value::Func(f) => {
                // Record keys are few: a linear scan beats binary search.
                for (k, v) in f.keys.iter().zip(f.vals.iter()) {
                    if let Value::Str(s) = k {
                        if *s == id {
                            return Ok(v.clone());
                        }
                    }
                }
                Err(format!("record {self} has no field {}", name(id)))
            }
            _ => Err(format!("field .{} of a non-record {self}", name(id))),
        }
    }

    pub fn domain(&self) -> R<Value> {
        match self {
            Value::Seq(s) => Ok(Value::set_sorted((1..=s.len() as i64).map(Value::Int).collect())),
            Value::Func(f) => Ok(Value::set_sorted(f.keys.to_vec())),
            _ => Err(format!("DOMAIN of a non-function {self}")),
        }
    }

    /// `[f EXCEPT ![k] = v]` for one key; a key outside the domain leaves
    /// f unchanged, as in TLA+.
    pub fn except(&self, k: &Value, v: Value) -> R<Value> {
        match self {
            Value::Seq(s) => {
                if let Value::Int(i) = k {
                    if *i >= 1 && (*i as usize) <= s.len() {
                        let mut n: Vec<Value> = s.to_vec();
                        n[*i as usize - 1] = v;
                        return Ok(Value::Seq(n.into()));
                    }
                }
                Ok(self.clone())
            }
            Value::Func(f) => match f.keys.binary_search(k) {
                Ok(i) => {
                    let mut vals = f.vals.clone();
                    vals[i] = v;
                    Ok(Value::Func(Arc::new(Func { keys: f.keys.clone(), vals })))
                }
                Err(_) => Ok(self.clone()),
            },
            _ => Err(format!("EXCEPT on a non-function {self}")),
        }
    }

    pub fn pairs(&self) -> R<Vec<(Value, Value)>> {
        match self {
            Value::Seq(s) => Ok(s.iter().enumerate().map(|(i, v)| (Value::Int(i as i64 + 1), v.clone())).collect()),
            Value::Func(f) => Ok(f.keys.iter().cloned().zip(f.vals.iter().cloned()).collect()),
            _ => Err(format!("expected a function, got {self}")),
        }
    }

    /// The image of this value under a permutation of model values
    /// (`p[id]` is where model value `id` goes). Sets and function domains
    /// are re-sorted, since renaming can change their order.
    pub fn permute(&self, p: &[u32]) -> Value {
        self.permute_opt(p).unwrap_or_else(|| self.clone())
    }

    /// Like `permute`, but None when the value contains no model value the
    /// permutation moves — the common case, and then nothing is copied.
    pub fn permute_opt(&self, p: &[u32]) -> Option<Value> {
        self.permute_node(p)
    }

    /// Does the permutation move any model value inside this value?
    pub fn moves(&self, p: &[u32]) -> bool {
        match self {
            Value::Model(m) => p[*m as usize] != *m,
            Value::Bool(_) | Value::Int(_) | Value::Str(_) => false,
            Value::Set(xs) | Value::Seq(xs) => xs.iter().any(|x| x.moves(p)),
            Value::Func(f) => f.keys.iter().chain(f.vals.iter()).any(|x| x.moves(p)),
            Value::Lazy(_) => true,
        }
    }

    /// For a function whose key set the permutation maps onto itself (as
    /// `[Paths -> X]` under Permutations(Paths)), the image's i-th entry is
    /// (keys[i], permute(vals[j])) where keys[j] = inverse(keys[i]). None
    /// when that preimage is not a key: the key set is not closed.
    #[inline]
    fn preimage(f: &Func, i: usize, p: &[u32], inv: &[u32]) -> Option<usize> {
        match &f.keys[i] {
            Value::Model(m) => {
                let pre = inv[*m as usize];
                if pre == *m {
                    return Some(i);
                }
                f.keys.binary_search(&Value::Model(pre)).ok()
            }
            k if !k.moves(p) => Some(i),
            _ => None,
        }
    }

    /// `self.permute(p).cmp(other)`, without building the permuted copy
    /// wherever the image's element order can be known in place.
    pub fn cmp_perm_with(&self, other: &Value, p: &[u32], inv: &[u32]) -> Ordering {
        match (self, other) {
            (Value::Model(m), Value::Model(o)) => p[*m as usize].cmp(o),
            (Value::Bool(_) | Value::Int(_) | Value::Str(_), _) => self.cmp(other),
            (Value::Seq(xs), Value::Seq(ys)) => xs.len().cmp(&ys.len()).then_with(|| {
                xs.iter().zip(ys.iter()).map(|(x, y)| x.cmp_perm_with(y, p, inv)).find(|c| c.is_ne()).unwrap_or(Ordering::Equal)
            }),
            (Value::Func(f), Value::Func(g)) => {
                if f.keys.len() != g.keys.len() {
                    return f.keys.len().cmp(&g.keys.len());
                }
                if (0..f.keys.len()).any(|i| Self::preimage(f, i, p, inv).is_none()) {
                    return self.permute(p).cmp(other);
                }
                for i in 0..f.keys.len() {
                    let j = Self::preimage(f, i, p, inv).unwrap();
                    // the image's i-th key is keys[i] itself: the key set is closed
                    let c = f.keys[i].cmp(&g.keys[i]).then_with(|| f.vals[j].cmp_perm_with(&g.vals[i], p, inv));
                    if c.is_ne() {
                        return c;
                    }
                }
                Ordering::Equal
            }
            (Value::Set(_), _) if !self.moves(p) => self.cmp(other),
            _ => self.permute(p).cmp(other),
        }
    }

    /// `self.permute(p).cmp(self)`.
    pub fn cmp_perm(&self, p: &[u32], inv: &[u32]) -> Ordering {
        self.cmp_perm_with(self, p, inv)
    }

    /// `self.permute(p).hash()`, without building the copy wherever the
    /// image's element order can be known in place (the hash is
    /// compositional).
    pub fn hash_perm(&self, p: &[u32], inv: &[u32]) -> u64 {
        match self {
            Value::Model(m) => scalar(0x40 | ((p[*m as usize] as u64) << 8)),
            Value::Bool(_) | Value::Int(_) | Value::Str(_) => self.hash(),
            Value::Seq(xs) => {
                let mut h = scalar(0x60 | ((xs.len() as u64) << 8));
                xs.iter().for_each(|x| mix(&mut h, x.hash_perm(p, inv)));
                h
            }
            Value::Func(f) => {
                let mut h = scalar(0x70 | ((f.keys.len() as u64) << 8));
                for i in 0..f.keys.len() {
                    let Some(j) = Self::preimage(f, i, p, inv) else { return self.permute(p).hash() };
                    mix(&mut h, f.keys[i].hash());
                    mix(&mut h, f.vals[j].hash_perm(p, inv));
                }
                h
            }
            Value::Set(_) if !self.moves(p) => self.hash(),
            _ => self.permute(p).hash(),
        }
    }

    fn permute_node(&self, p: &[u32]) -> Option<Value> {
        match self {
            Value::Model(m) => {
                let to = p[*m as usize];
                if to == *m { None } else { Some(Value::Model(to)) }
            }
            Value::Bool(_) | Value::Int(_) | Value::Str(_) => None,
            Value::Set(xs) | Value::Seq(xs) => {
                let mut out: Option<Vec<Value>> = None;
                for (i, x) in xs.iter().enumerate() {
                    if let Some(y) = x.permute_opt(p) {
                        out.get_or_insert_with(|| xs[..i].to_vec()).push(y);
                    } else if let Some(o) = out.as_mut() {
                        o.push(x.clone());
                    }
                }
                let v = out?;
                Some(match self {
                    Value::Set(_) => Value::set(v).expect("normalized values permute to normalized values"),
                    _ => Value::Seq(v.into()),
                })
            }
            Value::Func(f) => {
                let mut out: Option<Vec<(Value, Value)>> = None;
                for (i, (k, v)) in f.keys.iter().zip(f.vals.iter()).enumerate() {
                    let (k2, v2) = (k.permute_opt(p), v.permute_opt(p));
                    if out.is_none() && (k2.is_some() || v2.is_some()) {
                        out = Some(f.keys[..i].iter().cloned().zip(f.vals[..i].iter().cloned()).collect());
                    }
                    if let Some(o) = out.as_mut() {
                        o.push((k2.unwrap_or_else(|| k.clone()), v2.unwrap_or_else(|| v.clone())));
                    }
                }
                Some(Value::func(out?).expect("normalized values permute to normalized values"))
            }
            Value::Lazy(_) => Some(self.permute(p)),
        }
    }

    // ---- hashing ----------------------------------------------------------

    /// A structural hash, compositional: a compound value's hash is a
    /// function of its children's hashes alone. (A per-allocation memo of
    /// these, and of permuted images, was measured and lost: 20.2s -> 22.4s
    /// on LeanScopedSyncHolds, 14.4s -> 15.5s on LeanChunkGC.)
    pub fn hash(&self) -> u64 {
        match self {
            Value::Bool(b) => scalar(0x10 | *b as u64),
            Value::Int(n) => {
                let mut h = scalar(0x20);
                mix(&mut h, *n as u64);
                h
            }
            Value::Str(s) => scalar(0x30 | ((*s as u64) << 8)),
            Value::Model(s) => scalar(0x40 | ((*s as u64) << 8)),
            Value::Lazy(_) => self.materialize().expect("hashing an infinite set").hash(),
            _ => self.hash_node(),
        }
    }

    fn hash_node(&self) -> u64 {
        match self {
            Value::Set(xs) | Value::Seq(xs) => {
                let tag = if matches!(self, Value::Set(_)) { 0x50 } else { 0x60 };
                let mut h = scalar(tag | ((xs.len() as u64) << 8));
                xs.iter().for_each(|x| mix(&mut h, x.hash()));
                h
            }
            Value::Func(f) => {
                let mut h = scalar(0x70 | ((f.keys.len() as u64) << 8));
                for (k, v) in f.keys.iter().zip(f.vals.iter()) {
                    mix(&mut h, k.hash());
                    mix(&mut h, v.hash());
                }
                h
            }
            _ => self.hash(),
        }
    }
}

#[inline]
fn scalar(x: u64) -> u64 {
    let mut h = 0x243F_6A88_85A3_08D3_u64;
    mix(&mut h, x);
    h
}

/// One 64x64->128 multiply per word (wyhash's "mum"): cheap, and a far
/// better avalanche than FxHash's rotate-xor-multiply, which matters when
/// the 64-bit result is the only record that a state was seen.
#[inline(always)]
fn mix(h: &mut u64, x: u64) {
    let m = ((*h ^ x) as u128).wrapping_mul(0x9E37_79B9_7F4A_7C15_u128 | (0xA076_1D64_78BD_642F_u128 << 64));
    *h = (m as u64) ^ ((m >> 64) as u64);
}

/// The hash of one variable's value.
#[inline]
pub fn var_hash(v: &Value) -> u64 {
    v.hash()
}

/// A state's fingerprint, from its per-variable hashes. Keeping the two
/// levels separate is what lets a successor reuse its parent's hash for
/// every variable the action left alone.
#[inline]
pub fn combine(hs: &[u64]) -> u64 {
    let mut h = 0x1319_8A2E_0370_7344_u64;
    for &x in hs {
        mix(&mut h, x);
    }
    // final avalanche (murmur3 fmix64)
    h ^= h >> 33;
    h = h.wrapping_mul(0xff51_afd7_ed55_8ccd);
    h ^= h >> 33;
    h = h.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
    h ^ (h >> 33)
}

pub fn fingerprint(vals: &[Value]) -> u64 {
    let hs: Vec<u64> = vals.iter().map(var_hash).collect();
    combine(&hs)
}

/// Cheap sufficient test for equality: the same scalar, or the same
/// allocation. A successor's untouched variables pass it.
#[inline]
pub fn same(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Bool(x), Value::Bool(y)) => x == y,
        (Value::Int(x), Value::Int(y)) => x == y,
        (Value::Str(x), Value::Str(y)) | (Value::Model(x), Value::Model(y)) => x == y,
        (Value::Set(x), Value::Set(y)) | (Value::Seq(x), Value::Seq(y)) => Arc::ptr_eq(x, y),
        (Value::Func(x), Value::Func(y)) => Arc::ptr_eq(x, y),
        _ => false,
    }
}

fn product(sets: &[Arc<[Value]>], cur: &mut Vec<Value>, f: &mut dyn FnMut(&[Value])) {
    if cur.len() == sets.len() {
        f(cur);
        return;
    }
    for x in sets[cur.len()].iter() {
        cur.push(x.clone());
        product(sets, cur, f);
        cur.pop();
    }
}

// ---- order and equality ----------------------------------------------------

impl Ord for Value {
    fn cmp(&self, o: &Value) -> Ordering {
        use Value::*;
        match (self, o) {
            (Bool(a), Bool(b)) => a.cmp(b),
            (Int(a), Int(b)) => a.cmp(b),
            (Str(a), Str(b)) | (Model(a), Model(b)) => a.cmp(b),
            (Set(a), Set(b)) | (Seq(a), Seq(b)) => {
                if Arc::ptr_eq(a, b) {
                    Ordering::Equal
                } else {
                    a.len().cmp(&b.len()).then_with(|| a[..].cmp(&b[..]))
                }
            }
            (Func(a), Func(b)) => {
                if Arc::ptr_eq(a, b) {
                    Ordering::Equal
                } else {
                    // TLC: domain size, then (key, value) pairs interleaved.
                    a.keys.len().cmp(&b.keys.len()).then_with(|| {
                        for ((k1, v1), (k2, v2)) in a.keys.iter().zip(a.vals.iter()).zip(b.keys.iter().zip(b.vals.iter())) {
                            let c = k1.cmp(k2).then_with(|| v1.cmp(v2));
                            if c != Ordering::Equal {
                                return c;
                            }
                        }
                        Ordering::Equal
                    })
                }
            }
            (Lazy(_), _) | (_, Lazy(_)) => {
                let a = self.materialize().expect("comparing an infinite set");
                let b = o.materialize().expect("comparing an infinite set");
                a.cmp(&b)
            }
            _ => self.tag().cmp(&o.tag()),
        }
    }
}

impl PartialOrd for Value {
    fn partial_cmp(&self, o: &Value) -> Option<Ordering> {
        Some(self.cmp(o))
    }
}

impl PartialEq for Value {
    fn eq(&self, o: &Value) -> bool {
        self.cmp(o) == Ordering::Equal
    }
}

impl Eq for Value {}

// ---- display ----------------------------------------------------------------

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        fn list(f: &mut fmt::Formatter, open: &str, xs: &[Value], close: &str) -> fmt::Result {
            write!(f, "{open}")?;
            for (i, x) in xs.iter().enumerate() {
                if i > 0 {
                    write!(f, ", ")?;
                }
                write!(f, "{x}")?;
            }
            write!(f, "{close}")
        }
        match self {
            Value::Bool(b) => write!(f, "{}", if *b { "TRUE" } else { "FALSE" }),
            Value::Int(n) => write!(f, "{n}"),
            Value::Str(s) => write!(f, "{:?}", name(*s)),
            Value::Model(s) => write!(f, "{}", name(*s)),
            Value::Set(s) => list(f, "{", s, "}"),
            Value::Seq(s) => list(f, "<<", s, ">>"),
            Value::Func(fu) => {
                if fu.keys.iter().all(|k| matches!(k, Value::Str(_))) {
                    write!(f, "[")?;
                    for (i, (k, v)) in fu.keys.iter().zip(fu.vals.iter()).enumerate() {
                        let Value::Str(s) = k else { unreachable!() };
                        write!(f, "{}{} |-> {v}", if i > 0 { ", " } else { "" }, name(*s))?;
                    }
                    write!(f, "]")
                } else {
                    write!(f, "(")?;
                    for (i, (k, v)) in fu.keys.iter().zip(fu.vals.iter()).enumerate() {
                        write!(f, "{}{k} :> {v}", if i > 0 { " @@ " } else { "" })?;
                    }
                    write!(f, ")")
                }
            }
            Value::Lazy(l) => match &**l {
                Lazy::Nat => write!(f, "Nat"),
                Lazy::Int => write!(f, "Int"),
                Lazy::Strings => write!(f, "STRING"),
                Lazy::Subset(s) => write!(f, "SUBSET {s}"),
                Lazy::SeqOf(s) => write!(f, "Seq({s})"),
                Lazy::FuncSet(d, r) => write!(f, "[{d} -> {r}]"),
                Lazy::RecSet(_) => write!(f, "[record set]"),
                Lazy::Product(_) => write!(f, "[product set]"),
            },
        }
    }
}

impl fmt::Debug for Value {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}
