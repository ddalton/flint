//! Where each flint product keeps the cell that says "I am the writer
//! of this prefix" — and the probe that finds a NEIGHBOUR's.
//!
//! WHY THIS TABLE IS SHARED. One prefix has exactly one writer. Within
//! a product that is a mechanism: every writer contends for one epoch
//! cell, and the loser waits. ACROSS products it is a convention,
//! because each product derives its own cell key and the keys are
//! disjoint — forge's `git/epoch`, lean's `.flint/lean/epoch`. Point a
//! forge repository and a lean workspace at one prefix and both
//! acquire, both are correct that they hold THEIR cell, and neither can
//! see the other. There is no 412, no fence and no log line
//! (composition drill C1).
//!
//! Nothing here changes that. Prevention is left to whatever assigns
//! prefixes — an admission policy, a GitOps path, a platform module —
//! and this module exists so that when that prevention fails, the
//! failure is NOISY instead of silent. That is the whole contract: a
//! control you cannot audit is one you find out about from a support
//! ticket.
//!
//! WHY A HEAD AND NOT A LISTING. The first sketch listed the prefix and
//! looked for a foreign control DIRECTORY. An exact probe of the
//! neighbour's epoch cell is strictly better: one request instead of a
//! paginated listing, it detects a WRITER rather than the litter a
//! writer leaves, and it cannot be confused by nesting — forge's own
//! export at `<prefix>/inner` puts lean's cell at
//! `<prefix>/inner/.flint/lean/epoch`, which is not the key forge
//! probes.
//!
//! ADDING A PRODUCT means adding a row here and calling `neighbours`
//! with your own kind. Do not re-derive a neighbour's key at a call
//! site: a key that drifts turns this check into a permanent all-clear,
//! which is worse than not having it.

use super::{ObjectStore, StoreResult};

/// A flint writer kind, identified by the cell it arbitrates on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Writer {
    /// `flint-forge-syncer` over a bare git repository.
    ForgeRepository,
    /// A `flint-sync` sidecar over a lean workspace. Forge's legible
    /// export is one of these, published by forge rather than by an
    /// agent's sidecar.
    LeanWorkspace,
}

impl Writer {
    /// This kind's epoch cell under `prefix` (no trailing slash).
    pub fn epoch_key(&self, prefix: &str) -> String {
        match self {
            Writer::ForgeRepository => format!("{prefix}/git/epoch"),
            Writer::LeanWorkspace => format!("{prefix}/.flint/lean/epoch"),
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Writer::ForgeRepository => "a forge repository",
            Writer::LeanWorkspace => "a lean workspace",
        }
    }

    /// The knob an operator would reach for to correct it.
    pub fn prefix_field(&self) -> &'static str {
        match self {
            Writer::ForgeRepository => "FlintRepo.spec.keyPrefix",
            Writer::LeanWorkspace => "the lean workspace prefix",
        }
    }

    pub const ALL: [Writer; 2] = [Writer::ForgeRepository, Writer::LeanWorkspace];
}

/// What a probe found: somebody else's lease cell under our prefix.
#[derive(Debug, Clone)]
pub struct Foreign {
    pub kind: Writer,
    pub key: String,
    pub holder_id: String,
    pub epoch: u64,
    /// The holder shut down cleanly. The prefix is still shared — the
    /// workspace's objects are all still there — but nothing is writing
    /// it at this instant.
    pub released: bool,
    /// The STORE's clock for the last renewal, never ours (A8).
    pub last_renew_unix: Option<u64>,
}

impl Foreign {
    /// One line, written for whoever is reading logs at 3am: what is
    /// there, whether it is live, and which field to change.
    pub fn report(&self) -> String {
        let liveness = if self.released {
            "it released cleanly, so nothing is writing it right now — but its objects are \
             still there and it will take the prefix back when it restarts"
                .to_string()
        } else {
            match self.last_renew_unix {
                Some(t) => format!("it is HOLDING that lease (last renewed at {t} by the store's clock)"),
                None => "it is holding that lease".to_string(),
            }
        };
        format!(
            "PREFIX SHARED WITH ANOTHER PRODUCT: {} also writes this prefix. Its lease cell is \
             {} (holder {}, epoch {}), and {}. One prefix has exactly one writer, and this is \
             NOT enforced across products — the two arbitrate on different cells and cannot see \
             each other, so neither will ever fence the other and the loser's writes are lost \
             quietly. Give one of them its own prefix ({} names it).",
            self.kind.label(),
            self.key,
            self.holder_id,
            self.epoch,
            liveness,
            self.kind.prefix_field(),
        )
    }
}

/// Probe `prefix` for the lease cells of every writer kind that is not
/// `mine`. One HEAD-shaped read per foreign kind (currently one).
///
/// A store error is NOT a finding and NOT a failure: this is a
/// diagnostic, and a prefix that cannot be probed must never stop a
/// writer that would otherwise have started. Errors are swallowed here
/// so that no caller can accidentally make this fatal.
pub async fn neighbours(
    store: &dyn ObjectStore,
    prefix: &str,
    mine: Writer,
) -> StoreResult<Vec<Foreign>> {
    let mut found = vec![];
    for kind in Writer::ALL.iter().copied().filter(|k| *k != mine) {
        let key = kind.epoch_key(prefix);
        if let Ok(Some(state)) = store.epoch_read(&key).await {
            found.push(Foreign {
                kind,
                key,
                holder_id: state.holder_id,
                epoch: state.epoch,
                released: state.released,
                last_renew_unix: state.last_renew_unix,
            });
        }
    }
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::MemoryStore;

    async fn claim(store: &MemoryStore, key: &str, holder: &str) {
        store.epoch_acquire(key, holder, None).await.expect("acquire");
    }

    /// The condition drill C1 measured: two products, one prefix, both
    /// holding, neither able to see the other. Each must now find the
    /// other's cell.
    #[tokio::test]
    async fn each_product_finds_the_others_lease_under_one_prefix() {
        let store = MemoryStore::new();
        claim(&store, &Writer::ForgeRepository.epoch_key("t/shared"), "forge-1").await;
        claim(&store, &Writer::LeanWorkspace.epoch_key("t/shared"), "lean-1").await;

        let seen = neighbours(&store, "t/shared", Writer::ForgeRepository).await.unwrap();
        assert_eq!(seen.len(), 1, "forge must see the lean workspace");
        assert_eq!(seen[0].kind, Writer::LeanWorkspace);
        assert_eq!(seen[0].holder_id, "lean-1");
        assert!(!seen[0].released);

        let seen = neighbours(&store, "t/shared", Writer::LeanWorkspace).await.unwrap();
        assert_eq!(seen.len(), 1, "lean must see the forge repository");
        assert_eq!(seen[0].holder_id, "forge-1");
    }

    /// A writer must never report ITSELF. This is the leg that would
    /// turn the check into a permanent false alarm on every healthy
    /// deployment, which is how a diagnostic gets switched off.
    #[tokio::test]
    async fn a_writer_alone_on_its_prefix_finds_nothing() {
        let store = MemoryStore::new();
        claim(&store, &Writer::ForgeRepository.epoch_key("t/solo"), "forge-1").await;
        assert!(neighbours(&store, "t/solo", Writer::ForgeRepository).await.unwrap().is_empty());

        let store2 = MemoryStore::new();
        claim(&store2, &Writer::LeanWorkspace.epoch_key("t/solo"), "lean-1").await;
        assert!(neighbours(&store2, "t/solo", Writer::LeanWorkspace).await.unwrap().is_empty());
    }

    /// The probe is an EXACT key, and that is what makes forge's own
    /// legible export invisible to it.
    ///
    /// A repository at `t/a` may export to `t/a/inner` — the syncer's
    /// self-collision guard is string equality, so nesting is admitted
    /// (drill C1b). That export is a real lean workspace and writes a
    /// real lean epoch cell. If this probe listed the subtree instead
    /// of naming a key, every such repository would report itself as
    /// colliding with its own export, forever.
    #[tokio::test]
    async fn a_nested_export_is_not_mistaken_for_a_foreign_writer() {
        let store = MemoryStore::new();
        claim(&store, &Writer::ForgeRepository.epoch_key("t/a"), "forge-1").await;
        claim(&store, &Writer::LeanWorkspace.epoch_key("t/a/inner"), "forge-1-export").await;
        assert!(
            neighbours(&store, "t/a", Writer::ForgeRepository).await.unwrap().is_empty(),
            "a repository must not report its own nested export"
        );
    }

    /// A released cell still means the prefix is shared: the objects
    /// are there and the holder takes it back on restart. It is
    /// reported, and the wording says which it is.
    #[tokio::test]
    async fn a_released_neighbour_is_still_reported_and_says_so() {
        let store = MemoryStore::new();
        let key = Writer::LeanWorkspace.epoch_key("t/quiet");
        let lease = store.epoch_acquire(&key, "lean-1", None).await.unwrap();
        store.epoch_release(&key, &lease).await.unwrap();

        let seen = neighbours(&store, "t/quiet", Writer::ForgeRepository).await.unwrap();
        assert_eq!(seen.len(), 1, "a clean release does not make the prefix unshared");
        assert!(seen[0].released);
        let r = seen[0].report();
        assert!(r.contains("released cleanly"), "{r}");
        assert!(!r.contains("HOLDING"), "a released cell must not read as live: {r}");
    }

    /// The report has to be usable by whoever reads it, so it names
    /// the neighbour, the cell, and the field to change.
    #[tokio::test]
    async fn the_report_names_the_cell_and_the_field_to_change() {
        let store = MemoryStore::new();
        claim(&store, &Writer::LeanWorkspace.epoch_key("t/x"), "lean-9").await;
        let seen = neighbours(&store, "t/x", Writer::ForgeRepository).await.unwrap();
        let r = seen[0].report();
        assert!(r.contains("t/x/.flint/lean/epoch"), "{r}");
        assert!(r.contains("lean-9"), "{r}");
        assert!(r.contains("a lean workspace"), "{r}");
        assert!(r.contains("NOT enforced across products"), "{r}");
    }
}
