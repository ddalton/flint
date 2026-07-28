# SPDK error-classification audit — probe, never parse (P3)

**Written 2026-07-28**, the P3 item from
`state-of-the-driver-post-v1.21.0.md`. F48 and F54 were the same defect one
RPC apart, one week apart — a *converged* SPDK target misread as a failure
because the code classified the error *message* and SPDK's message carried
no classifiable text. Each cost a live campaign day to find. This audit
enumerates every site that classifies an SPDK RPC error textually, gives
each a verdict, and records the conversions made.

Every shape claim below is **verified against SPDK source v26.05.1-pre**
(`~/github/spdk` @ `bb2b757ac`) — the generation flint ships. File:line
references are to that tree.

## 1. The two shape families

**Family A — errno-mapped (matchable).** The bdev/lvol/raid RPC handlers
return `spdk_strerror(-rc)` with the errno as the code
(`vbdev_lvol_rpc.c` — 50 `spdk_strerror` sites; `bdev_raid_rpc.c`
likewise). A duplicate is honestly `-17 File exists`
(`lvol.c:1157` returns `-EEXIST` for a duplicate lvol/snapshot name), a
missing object honestly `-19 No such device`. Textual classifiers
(`is_already_exists` / `is_missing`) are **reliable** for this family.

**Family B — collapsed (unmatchable).** The nvmf subsystem RPCs destroy
the classification before it reaches the wire:

| RPC | duplicate / absent shape | source |
|---|---|---|
| `nvmf_create_subsystem` (dup) | `-32603 "Unable to create subsystem X"` | `nvmf_rpc.c:465` (F48, live) |
| `nvmf_subsystem_add_host` (dup) | `-EINVAL` internally → **bare** `-32603 "Internal error"` | `subsystem.c` `add_host_ext` + `nvmf_rpc.c` handler (F54, live) |
| `nvmf_subsystem_add_listener` (dup) | `-32602 "Invalid parameters"` (the ERRLOG says "Listener already exists"; the *response* doesn't) | `nvmf_rpc.c` `nvmf_rpc_listen_paused` |
| `nvmf_delete_subsystem` (absent) | `goto invalid` → `-32602 "Invalid parameters"` | `nvmf_rpc.c` `rpc_nvmf_delete_subsystem` |

The add_host case is the strongest possible argument for probing: the
duplicate is collapsed to `-EINVAL` **inside the library**, so the
information "this was a benign duplicate" is destroyed before the RPC
layer ever sees it. No client-side parser can recover it, in any language.

**Initiator attach is both families at once.**
`bdev_nvme_attach_controller` refuses a duplicate *name* with
`-EALREADY "A controller named X already exists …"`
(`bdev_nvme_rpc.c:397–444` — matchable), but one layer down
`spdk_bdev_nvme_create` reports a registered *trid* as plain
`-EALREADY` → `spdk_strerror` = `"Operation already in progress"`
(`bdev_nvme_rpc.c:258` — no matchable text). And independent of shape, a
timeout whose attach actually landed reads as failure with the controller
live. Text is therefore a fast-path at best, never the truth.

## 2. The doctrine

For **Family B** and for **any convergence-critical retry path**: keep the
textual match as a cheap fast-path if it exists, but on an unrecognized
error **probe the target's state** (`nvmf_get_subsystems`,
`bdev_nvme_get_controllers`, `bdev_get_bdevs`) and derive the verdict from
state, not from what SPDK said about the request. An unreachable target
probes as "not converged" and fails honestly — never converge blind.

For **Family A**: textual classification may stand, but new code should
prefer probes anyway; the family boundary is an SPDK implementation detail
that has already moved once (the nvmf layer used to be errno-mapped too).

Mock fidelity rule (how F54's regression test exists at all): when a test
mock models an SPDK duplicate/absent response, it must return the **real
v26.05 shape from the table above** — a mock that says "already exists"
where SPDK says "Internal error" verifies the bug back in.

## 3. Converted this pass (2026-07-28)

1. **`hot_rejoin.rs` — all five `bdev_nvme_attach_controller` sites** now
   go through `attach_converged()` (text fast-path + controller-presence
   probe): E_f pre-connect, prestage consumer pre-connect,
   `prestage_inline` consumer pre-connect, copy-source attach,
   localization pad attach. Three of the five previously **hard-failed on
   any error** while sitting behind a best-effort `detach_controller` — a
   silently-failed detach made the retry's attach a duplicate and cost a
   full backoff cycle (~7 min, the F54 economics). Regression tests:
   `residual_ef_preconnect_controller_is_adopted`,
   `attach_converged_probes_state_when_the_error_carries_no_text`,
   `attach_converged_fails_honestly_when_the_controller_is_absent`; the
   FakeRpc mock now refuses duplicate attaches with the real v26.05 shape.
2. **`node_agent.rs` `handle_drop_local_export`** (the F49 route, called
   from NodeStage's local-claim path): delete-of-absent is `-32602
   Invalid parameters`, which the old `is_missing` arm could never match —
   a race with the loss-detector or a concurrent teardown 500'd the whole
   NodeStage attempt. Now probes `nvmf_get_subsystems` on any delete
   error; gone is converged, probe-unreachable stays a failure.
3. **`driver.rs` `create_nvmeof_target` DELETED** — dead code (zero
   callers; its `/api/nvmeof/*` routes never existed, every call would
   have 404ed) carrying three Family-B textual guards. Same fossil family
   as `cleanup_nvmeof_target` (deleted 2026-07-22).

## 4. Sites audited and KEPT (with verdicts)

**Family-A textual classifiers — reliable, kept:**
- `epoch_scheduler.rs` `is_already_exists`/`is_missing` on
  `bdev_lvol_snapshot` / `bdev_lvol_delete` (the epoch cut/rollback):
  lvol duplicates are honest `-EEXIST "File exists"`.
- `hot_rejoin.rs` lvol sites: stranded-head delete (`is_missing`),
  `bdev_lvol_set_parent` "already parent" (`-EEXIST`), quiesce-lease
  releases ("no quiesce lease" is a flint-side message, not SPDK's).
- `guarded_destroy.rs` `probe_error_is_missing`: deliberately narrow
  (documented at its definition), applied to Family-A probes; its
  companion test asserts refusal messages do NOT read as benign.
- `driver.rs:3206` `bdev_raid_create` EEXIST retry: `bdev_raid_rpc.c`
  uses `spdk_strerror`, so "File exists"/`-17` is the real shape.
- `minimal_disk_service.rs` disk-init sites (uring/malloc/attach
  duplicates): Family A, boot-time, non-convergence-critical.
- `spdk_native.rs:424,716`: `-19`-tolerant delete + benign-log list —
  logging tier only.

**Family-B sites kept because the caller is already tolerant or probes:**
- `nvmeof_export.rs` `ensure_export`: **the reference implementation** —
  probe-first with probe-on-error re-reads at every builder; its
  `contains("Method not found")` arm is capability detection, not error
  classification.
- `hot_rejoin.rs` prestage nvmf builders: probe-armed since F48/F54 (the
  textual arms above them are vestigial fast-paths, harmless).
- `hot_rejoin.rs` `cut_ef_strict` `is_already_exists`: classifies only
  the *diagnostic label* — both branches fail, by design (an EEXIST
  E_f snapshot from another instant must NOT be adopted, §7).
- `node_agent.rs` `teardown_local_leg_export` (F51 both-domain
  teardown): lists live subsystems and deletes only what exists — probe
  precedes delete; the race window falls to the caller's retry.
- `node_agent.rs` orphan-sweep subsystem delete: defer-and-retry with a
  freshly-listed target set; a raced absence re-lists next cycle.
- `node_agent.rs:1531,2695` `ublk_create_target` "already exists" arms:
  the real duplicate is `-EBUSY` (`ublk.c`), so the arms never match —
  but both fall to tolerant log-and-continue, so the misclassification
  is cosmetic. Left as-is; do not copy the pattern.
- `node_agent.rs` connect-route `create_nvmeof_target_local` outer
  "already exists" arm: wraps `ensure_export` (which never returns
  duplicate errors) — vestigial.
- `driver.rs:3444,3612` disconnect/delete "does not exist" arms: never
  match Family-B shapes, but both fall through to best-effort `Ok`.

**Out of scope (not SPDK):** kube API "NotFound"/"already exists"
(`cutover.rs`, `rwx_nfs.rs`), nvme-cli stderr ("already connected"),
`nvmeof_utils.rs` operator-facing error prose, flint-internal sentinels
(`LINEAGE_NOT_COVERED`, "no quiesce lease").

## 5. Residuals

- `attach_converged`'s probe checks controller *presence by name*, not
  that the controller serves the expected subnqn. A name collision across
  different subsystems would be silently adopted — exactly as the old
  textual arm would have ("A controller named X already exists, but uses
  a different subnqn" also matched `contains("already exists")`). Not a
  regression; a subnqn-verifying probe is the strengthening if a campaign
  ever attributes a failure here.
- The Family-A/Family-B boundary is v26.05 fact, not contract. An SPDK
  upgrade re-runs §1's verification (grep the table's source sites).
- Live canary owed: one 2.9 run next campaign exercises E_f
  pre-connect + prestage builders end-to-end (the F54 gate already covers
  the nvmf half).
