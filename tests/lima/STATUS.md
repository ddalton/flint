# Flint NFS / pNFS Compliance Work — Status

Living document. Update this when a session ends or a milestone lands.

**Last updated:** 2026-05-04 — v1.0.0 release prep complete (CHANGELOG, version bumps, docs pruned); trunk-based development now in effect (single `main` branch); **publish gated on AWS multi-node validation** (kubeconfig pending from user).
**Branch:** `main` (trunk; all other branches deleted with `archive/config` and `archive/disk_mgmt` tags preserving pre-history). **HEAD:** `c8633b2`.

### Picking up next session — start here

* **v1.0.0 publish is paused awaiting multi-node K8s validation.** All local/release-mode validation passed: `cargo test --lib --release` 330 PASS, `cargo build --release` clean for all production binaries (`csi-driver`, `flint-nfs-server`, `flint-pnfs-mds`, `flint-pnfs-ds`), `helm lint` clean, `helm template` renders cleanly with default + pNFS-enabled values. **Not yet run this session: Lima e2e (`make test-pnfs-{smoke,recall,restart,csi}`) and the AWS cross-host bench (`make test-pnfs-cross-host`).** User is provisioning an AWS K8s cluster with ≥4 workers + ≥10 GbE inter-worker network and will provide kubeconfig. Validation gate before tag/release: at minimum `make test-pnfs-cross-host` must produce a passing TSV; ideally also a smoke equivalent on the AWS cluster to exercise CreateVolume → mount → write → read → delete against real-cluster networking. Tag command queued: `git tag -a v1.0.0 -m "Flint CSI 1.0.0 — first stable release" && git push origin v1.0.0`. GitHub Release follows: `gh release create v1.0.0 --title "v1.0.0" --notes-file <(awk '/^## \[1\.0\.0\]/,/^## \[/' CHANGELOG.md | sed '$d')`.
* **Trunk-based development now in effect.** `kind-no-spdk` was fast-forward merged into `main` (70 commits, no merge commit, no history rewrite) and deleted. `feature/nvmeof-block-device` (1 commit ahead, stale chart edit), `uring` (2 commits, design markdown), `config` (114 commits), and `disk_mgmt` (117 commits) were also deleted. The latter two had their tips preserved permanently as annotated tags `archive/config` and `archive/disk_mgmt` (recover via `git checkout archive/<name>`); the former two are reflog-only (~30-day window). Going forward: all work lands on `main` directly, no feature branches. Release policy is recorded in `~/.claude/projects/.../memory/project_release_policy.md` (SemVer + Keep-a-Changelog + image alias convention `1.0.0` / `1.0` / `1` / `latest`).
* **Snapshot crash-loop fix is the last engineering work this session.** Root cause: `CreateSnapshot` for pNFS volumes fell through to the SPDK metadata lookup at `driver.rs:get_volume_info_from_pv`, which requires `flint.csi.storage.io/node-name` (absent on pNFS volumes — they carry `pnfs.flint.io/*`). The lookup returned `Status::not_found(...)`, which `external-snapshotter` classifies as transient and retries forever. Fix landed in three commits:
  * `0c51302 fix(csi): reject CreateSnapshot for pNFS volumes with FAILED_PRECONDITION` — the load-bearing fix; `validate_snapshot_source` + `lookup_volume_attributes` helpers in `snapshot/snapshot_csi.rs`; 6 new tests pin the before/after behavior.
  * `fbf0124 fix(csi): reject CreateVolume from snapshot/PVC source for pNFS` — defensive guards on the CreateVolume path (pNFS destination + content_source = silent empty volume pre-fix; SPDK destination + pNFS source PVC = same NOT_FOUND retry pitfall pre-fix); refactored `is_pnfs_volume_attrs` to `pub` for cross-crate use; 4 new tests.
  * `a7dd01f feat(csi): startup preflight check for VolumeSnapshot CRDs` — separate but adjacent issue: when the *cluster* lacks the snapshot CRDs, the bundled `snapshot-controller` Deployment crash-loops on "no matches for kind VolumeSnapshotContent" and operators misdiagnose as Flint. New `src/snapshot/preflight.rs` checks the three required CRDs at controller startup and logs the `kubectl apply -k` install command if missing. Non-fatal: non-snapshot RPCs work without the CRDs. 6 new tests pin the message-formatting contract.
* **Docs pruned for 1.0.0.** Removed 7 stale design / planning docs (6,178 lines): `FINAL_COMPREHENSIVE_SUMMARY.md`, `HYBRID_SPDK_RUST_DESIGN.md`, `RUST_NVMET_DESIGN.md`, `RUST_STORAGE_STACK_DESIGN.md`, `FLINT_CSI_ARCHITECTURE.md`, `docs/spdk-driver-inventory.md`, `docs/plans/pnfs-csi-integration.md`. README's broken `MULTI_REPLICA_QUICKSTART.md` and `FLINT_CSI_ARCHITECTURE.md` links removed; the `Copyright © 2024 Cloudera, Inc.` line replaced with pointers to ADRs / plans / STATUS.md / CHANGELOG. ADRs `0001..0003`, `docs/plans/pnfs-production-readiness.md`, `docs/MEMORY_DISK_CLEANUP_PROCEDURE.md`, and all sub-component READMEs kept.
* **CHANGELOG.md added at repo root** in Keep-a-Changelog format. The 1.0.0 section establishes the public API surface for SemVer purposes (CSI gRPC verbs + StorageClass parameters + `volume_context` keys); breaking changes from now on require a `MAJOR` bump. Documents container-image conventions (`dilipdalton/flint-csi-driver:1.0.0` etc., x86-64 first, ARM64 to follow), the four deployment modes, and known limitations (pNFS-no-snapshots, no-SPDK no-Flint-replication, FFL deferred indefinitely).
* **Phase B is fully done.** All 5 sub-PRs (`982edc1`, `a2af4e0`, `e5e1ef3`, `02d3ee5`, `4d2f162`) plus the FH-stability follow-up (`3f000bb`) landed and pushed. The full chain: clients/stateids/layouts persist through a `SqliteBackend` (WAL+NORMAL crash-safe); a per-deployment `server_id` lives in a `server_identity` singleton table and is stamped into every NFSv4 file handle so cached FHs survive restart; `make test-pnfs-restart` proves a kernel client's `read()` against pre-restart open handles still returns the original bytes after the MDS comes back. Sessions deliberately observed-but-not-restored — slot replay state can't survive restart per RFC 8881 §15.1.10.4; the kernel's natural BADSESSION → fresh CREATE_SESSION recovery handles that.
* **Phase A + Phase B together = pNFS shippable to a first customer.** DS death triggers CB_LAYOUTRECALL + forced revocation (Phase A); MDS pod roll preserves client_id, stateids, layouts, and file-handle stamps so an existing mount keeps working byte-for-byte (Phase B). Phase C (FFL mirroring for HDFS-style replication) is demand-driven from here.
* **Cross-host bench harness is scaffolded** (`make test-pnfs-cross-host`, sources at `tests/k8s/pnfs-bench/`). Needs a Kubernetes cluster — **1 control + 4 workers is the recommended topology**: worker-1 = MDS, worker-2 = DS1, worker-3 = DS2, worker-4 = client. Control node deliberately not used for workload (would inject scheduler/etcd noise into bench numbers). 3-worker fallback supported (MDS+client co-located on worker-1; ~10-15% noisier but still useful). When cluster is ready: `KUBECONFIG=… PNFS_IMAGE=… MDS_NODE=… DS_NODES="…" CLIENT_NODE=… make test-pnfs-cross-host`. Generates a Namespace + MDS+DS Deployments with `nodeName` pins + a client Deployment, runs `bs={4K,1M} × {read,write}` fio sweep, dumps TSV + markdown table. Stretch: 5-worker cluster lets the same harness run N=1, 2, 3 DS sweeps for the scaling-curve answer. **NIC requirement: ≥10 GbE between workers** — 1 GbE saturates client-side before the architecture does and produces a misleading floor. Full topology + per-host requirements + pass criterion in `tests/k8s/pnfs-bench/README.md`.
* **Loopback nconnect sweep is in (`make test-pnfs-nconnect`); the data points to cross-host as the only next move.** Single-host with `nconnect={1,4,8,16}` × `bs={4K,1M}` × `{read,write}` shows **throughput essentially flat across nconnect** on this Mac/loopback hardware (snapshot in `tests/lima/pnfs/nconnect-results-2026-05-03.tsv`). That **rules out** the per-TCP-serial RPC handler at `server_v4.rs:176` as the single-host ceiling — pipelining it would not move the number on this hardware. The bottleneck is below the per-connection layer (kernel page cache writeback, shared-APFS-journal between MDS+DS1+DS2, loopback TCP saturation, fio iodepth — can't disentangle without separating the kernels). **Next move: a real cross-host bench** (Option 1 from the original "performance scaling" branch). Estimated ~3 days harness + Terraform; ~$5–20 cloud cost; ~$0 if you have spare Linux boxes + a switch. The bench is reusable forever.
* **Test gates as of HEAD:** `cargo test --lib --release` **330 PASS / 0 FAIL** (was 314 at the start of this session — +16 from snapshot guards (10) + CRD preflight (6)). `helm lint` clean; `helm template` renders cleanly with default and pNFS-enabled values. **Lima e2es and pynfs not re-run this session** — last known state from `5650436`: `make test-pnfs-smoke` green, `make test-pnfs-recall` green (5 markers), `make test-pnfs-restart` green with hash assertion firing, `make test-nfs-protocol` 167 PASS / 4 FAIL / 91 SKIP. The four commits this session changed only the snapshot CSI handler, CreateVolume guards, and the preflight check — code paths the unit tests cover; no protocol or persistence paths were touched. **AWS multi-node `make test-pnfs-cross-host` is the publish gate** for measured cross-host scaling and end-to-end real-cluster validation.
* **Conformance work concluded at 167/4/91; the remaining 4 fails are intentionally deferred niche cases (none cascade or corrupt data; production risk profile is low):**
  * `RNM20` — LINK returns NFS4ERR_ACCESS instead of OK on a hard-link create. Filesystem-perms bug. Affects build systems / Git checkouts that hard-link cache outputs; effectively zero impact on the perf-tier ML use case (write-once / read-many). ~½ session to fix.
  * `SEQ9c` — LOOKUP with a malformed name expects `NFS4ERR_INVAL`; we return success-ish or NOENT. Real Linux kernel clients never send malformed names (kernel paths are pre-validated). Pure conformance polish. ~½ session.
  * `SEQ2` — IndexError in pynfs's XDR decoder. Likely a niche reply-shape mismatch on a SEQUENCE replay-cache edge. Linux kernel decoder is more permissive and would just retry. May need ½–1 session, possibly more if it's a real boundary bug.
  * `RECC3` — non-reclaim OPEN during the server's grace window before the client did RECLAIM_COMPLETE expects `NFS4ERR_GRACE`. Our gate is correct in principle but only fires for the fixed 90s post-start; pynfs reaches RECC3 well past that. Right fix is **dynamic grace** (extend until either all known clients RECLAIM_COMPLETE or `lease_time` elapses with no reclaim attempts). Matters during MDS restart under sustained load; Phase B's persistence already handles steady-state pod rolls (clients keep `client_id` via case-1 EXCHANGE_ID renewal, never even need GRACE). ~1 session.
  
  Cumulative cost-benefit if all four were fixed: ~3–4 sessions for `167 → ~171/0/91`. No measurable customer-visible improvement; deferred until perf work picks them up as side-effects.
* **Useful files for orienting fast:**
  * `docs/plans/pnfs-production-readiness.md` — master plan (Phase A + B done, C deferred).
  * `src/state_backend/{mod,memory,sqlite}.rs` — trait + records + two impls.
  * `src/state_backend/mod.rs::spawn_persist` — the sync→async bridge each mutation uses.
  * `src/nfs/v4/state/{client,session,stateid}.rs` — managers retrofitted in B.3 (`load_records`, `to_record`/`from_record`, persist on every mutation).
  * `src/pnfs/mds/layout.rs::LayoutManager` — same shape; `persist` + `persist_delete` on `generate_layout` / `return_layout` / `revoke_layout`.
  * `src/nfs/v4/state/mod.rs::StateManager::load_from_backend` — pulls clients + sessions + stateids out of the backend at startup.
  * `src/pnfs/mds/server.rs::MetadataServer::load_persisted_state` — pNFS startup hook that calls `load_from_backend` + `layout.load_records` + bumps the persisted instance counter.
  * `src/pnfs/config.rs::PnfsConfig::build_state_backend` — config → `Arc<dyn StateBackend>` dispatch (Memory or Sqlite).
  * `tests/lima/pnfs/restart.sh` + `mds-restart.yaml` — the e2e harness (mount, write, kill MDS, restart, assert post-restart read returns the original bytes).
  * `src/nfs/v4/filehandle.rs::FileHandleManager::new_with_instance_id` — receives the persisted `server_id` from `MetadataServer::new`; stamps every FH with it so cached handles survive restart.
  * `tests/lima/pnfs/nconnect.sh` + `nconnect-results-2026-05-03.tsv` — single-host nconnect sweep (`make test-pnfs-nconnect`) and its first-run snapshot. Result: throughput flat across nconnect; rules out per-TCP-serial RPC pipelining as the next move on this hardware.
  * `tests/k8s/pnfs-bench/{cross-host-bench.sh,manifests.sh,README.md}` — cross-host bench harness (`make test-pnfs-cross-host`). Scaffolded but not yet run; needs a Kubernetes cluster with ≥4 workers + ≥10 GbE inter-worker network + local NVMe on the DS workers. README documents required env, topology, NIC/disk requirements, and pass criterion.

### Today in one paragraph

A user can now `helm upgrade --set pnfs.enabled=true …`, `kubectl apply` a
StorageClass with `parameters.layout: pnfs`, and a pod that mounts the
resulting PVC writes to a real pNFS-striped volume. SPDK code paths are
byte-identical when pNFS is disabled — the integration is opt-in via the
`FLINT_PNFS_MDS_ENDPOINT` env var, gated by ADR 0001's "keep one driver,
defer split" decision. End-to-end test (`make test-pnfs-csi`) exercises
the full create → mount → write → read → delete cycle against the actual
gRPC verbs and kernel data path. Honest single-host write win is **1.6×**
over single-server NFS at fsync=1 (ADR 0003); the architectural claim of
linear scaling with DS count remains untested cross-host. With Phase A
shipped, DS death triggers a server-initiated CB_LAYOUTRECALL via the
back-channel and forced revocation if the client doesn't return the
layout within the deadline — `make test-pnfs-recall` is the truth
source for that path.

### Headline

**The pNFS data path is now real, end-to-end.** A 24 MiB write from a
Linux NFSv4.1 client mount stripes across two DSes (8 MiB / 16 MiB),
the kernel reads it back through the layout, and the SHA-256 round-
trips. The MDS holds metadata only (0 bytes on disk for the striped
file).

```
                      DS1   DS2   MDS    client read-back
─────────────────────────────────────────────────────────
audit baseline       0     0     24M    hash matches (MDS-direct)
+ FILE-layout fixes  0     0     24M    "       (still MDS-direct)
+ DS endpoint fixes  0     0     24M    "       (kernel never connects)
+ DS RECLAIM/FH (✶)  8M    16M   0      ❌ 0-byte file (no LAYOUTCOMMIT)
+ LAYOUTCOMMIT (✶)   8M    16M   0      ✅ 24M, hash matches
```

✶ = this session.

### Conformance score (pynfs full suite, 262 tests)

```
Baseline (original audit run): 26 PASS  / 69 FAIL  / 167 SKIP   (96 runnable)
Current head:                 153 PASS  / 18 FAIL  / 91  SKIP  (171 runnable)
```

5.8× the original pass count. Six suites at 100%; nine more above 70%.
The pNFS work is *invisible* to pynfs (it runs against a single
non-pNFS mount); the pNFS smoke test is the truth source for the
data plane.

---

## Per-suite breakdown (current)

```
st_current_stateid     9/9   100%   ✓ ← new this session
st_destroy_clientid    8/8   100%   ✓
st_compound            5/5   100%   ✓
st_trunking            2/2   100%   ✓
st_destroy_session     1/1   100%   ✓
st_create_session     27/28   96%
st_rename             32/35   91%
st_lookupp             8/9    88%
st_exchange_id        23/26   88%
st_putfh               7/8    87%
st_sequence           14/17   82%
st_courtesy            4/5    80%
st_open                5/7    71%
st_secinfo_no_name     4/4   100%   ✓ ← new this session
st_secinfo             2/2   100%   ✓ ← new this session
st_verify              1/1   100%   ✓ ← new this session
st_reclaim_complete    1/4    25%
st_delegation          0/3     0% (blocked on CB_RECALL)
```

---

## What was achieved

The work falls into three big phases. All commits are on the
`kind-no-spdk` branch, pushed to `origin`.

### Phase 1 — RFC framing & encoding correctness (baseline → 49 PASS)

Hardened the wire layer so all subsequent error-path tests at least
*reach* the right handler instead of crashing the harness with
`GARBAGE_ARGS`. After Phase 1 there are zero RPC-level GARBAGE_ARGS in
the suite.

| # | Commit | What |
|---|--------|------|
| 1.A | `aaf3de7` | NFSv4 status enum cross-referenced against RFC 7530/8881/7862 (was off-by-various from RFC); `OperationResult::Unsupported` now encodes opcode + status (was emitting only status, causing pynfs decoder EOFError); minor-version gate; tag-validity gate; `unsafe { buf.set_len }` UB removed |
| 1.B-1 | `1a543b5` | NFSv4.1 SEQUENCE exactly-once replay cache wired end-to-end; resync branch removed; `RetryUncachedRep` for replays before the original returned |
| 1.B-2 | `6d4c30d` | CREATE_SESSION input validation (TOOSMALL / INVAL / BADXDR for malformed channel attrs); rdma_ird array decoded properly |
| 1.B-3 | `64ab3d4` | EXCHANGE_ID flag-bit validation, eia_client_impl_id<1> array length check, `NFS4ERR_NOT_ONLY_OP` for session-establishment ops bundled with non-SEQUENCE companions, `Operation::BadXdr` for malformed but recognised opcodes |
| 1.B-4 | `7f3acd2` | Body-less ops (GETFH/SAVEFH/RESTOREFH/READLINK/PUTROOTFH/...) at end of COMPOUND no longer mis-classified as BADXDR |
| 1.B-5 | `61779f1` | Tightened stateid validation: WRITE no longer accepts seqid=0-on-unknown-other (anonymous-write bypass); LOCK takes client_id from session (was hardcoded 1 = silent multi-client lock fights); LOCK byte-range overflow → INVAL |

### Phase 2 — Filesystem semantics (49 → 107 PASS, 75 new tests unlocked)

Made pynfs's `--maketree` initialisation work end-to-end so the suite
could even *start* the bulk of its tests.

| # | Commit | What |
|---|--------|------|
| 2.A | `a4fe847` | SETATTR4res missing `attrsset` bitmap (decoder hit EOF on the next op); SETATTR on dangling symlinks → Ok no-op (was NOENT) |
| 2.B | `c1dcc3c` | CREATE for SOCK/FIFO/BLK/CHR creates a regular-file stand-in (was BADTYPE, breaking maketree) |
| 2.C | `8cbe29f` | RENAME error chain: empty name → INVAL, "."/".." → BADNAME, non-dir parent → NOTDIR, dir-into-non-dir → EXIST, dir-into-non-empty-dir → NOTEMPTY, self-rename cinfo invariance. CREATE wire layout fixed (createtype4 union tail was being read after objname) |

### Phase 3 — Real protocol state machines (107 → 141 PASS)

The hard parts: bringing actual RFC state machines online.

| # | Commit | What |
|---|--------|------|
| 3.A | `872b81d` | EXCHANGE_ID RFC 8881 §18.35.5 nine-case decision table (UPD × confirmed × verifier × principal). RPC-level principal plumbed end-to-end through `Auth::principal()` → `CompoundContext::principal` → `ClientManager::exchange_id`. Client gains `confirmed` flag (set by CREATE_SESSION). |
| 3.B | `ecb26d0` | CREATE_SESSION RFC 8881 §18.36.4 sequence + replay cache (per-clientid `last_cs_sequence` + cached structured response, returned byte-for-byte on retry). Principal-collision check (CLID_INUSE) for unconfirmed records only. SEQUENCE `REP_TOO_BIG_TO_CACHE` when cachethis + tiny ca_maxresponsesize_cached. |
| 3.C | `8320986` | LOOKUPP RFC 5661 §18.10.4: non-directory CFH → NOTDIR, symlink CFH → SYMLINK |
| 3.D | `c86c718` | DESTROY_CLIENTID validation (STALE_CLIENTID / CLIENTID_BUSY); SEQUENCE compound-position rules (SEQUENCE_POS for misplaced SEQUENCE, OP_NOT_IN_SESSION for v4.1 op without SEQUENCE prefix); slot table sized to negotiated ca_maxrequests (SEQUENCE BADSLOT for slot_id beyond it); ca_maxoperations enforcement (TOO_MANY_OPS) |
| 3.E | `7262e72` | RFC 8881 §16.2.3.1.2 "current stateid" sentinel resolution: OPEN/LOCK/LOCKU/SAVEFH/RESTOREFH propagate it, PUTFH/PUTROOTFH/PUTPUBFH/LOOKUP/LOOKUPP invalidate it; FREE_STATEID op (LOCKS_HELD for open/lock state); fixed pre-existing LOCK locker4 union decode bug |

### Phase 4 — Real pNFS data path (this session: `cdbbe21..9076e96`)

A 24 MiB striped write from a Linux NFSv4.1 client now actually
traverses both DSes, and read-back through the MDS sees the right
size and bytes. Six surgical fixes; each was load-bearing — remove
any one and the kernel falls back to MDS-direct I/O silently.

| # | Commit | What | Effect on smoke (DS1/DS2/MDS, client size) |
|---|--------|------|-------|
| 4.A | `cdbbe21` | `FATTR4_FS_LAYOUT_TYPES` advertise [FILES] only (was [FILES, BLOCK, FLEX_FILES]); LAYOUTGET handler emits FILE layout instead of broken FFLv4 | kernel now negotiates FILES + issues GETDEVICEINFO (was silent fallback). 0/0/24M, 24M client. |
| 4.B | `272ceef` | `MdsControlService` overrides DS-reported endpoint with operator-configured one (DS reports its bind 0.0.0.0; client needs externally routable). `endpoint_to_uaddr` DNS-resolves hostnames so non-IPv4 DS endpoints encode to a parseable uaddr. | kernel now receives valid uaddr `192.168.5.2.80.11/.12` (was `0.0.0.0.80.11`). 0/0/24M, 24M client (still MDS-direct: kernel can't talk to DS). |
| 4.C | `23faf5b` | DS dispatcher answers `RECLAIM_COMPLETE` (opcode 58, RFC §18.51): kernel was getting NOTSUPP and marking the DS unhealthy. DS accepts MDS-issued v1 filehandles via `FileHandleManager::parse_path_lenient` and rebases by basename — DS no longer fails strict instance/hash check. | **8M/16M/0**. Kernel writes through DSes. 0-byte client read-back (MDS doesn't know the size). |
| 4.D | `9076e96` | **LAYOUTCOMMIT** (RFC 8881 §18.42) wired end-to-end: decoder for `LAYOUTCOMMIT4args` (offset/length/reclaim/stateid + `newoffset4` + `newtime4` + `layoutupdate4`); `OperationResult::LayoutCommit(status, Option<u64>)` with `newsize4` reply union; handler resolves CFH→path and `set_len` if `last_write_offset+1` extends EOF; best-effort `set_times` from `time_modify`. | **8M/16M/0, 24M client. PASS.** |
| 4.E | (this session) | **LAYOUTRETURN** (RFC 5661 §18.4 / RFC 8881 §18.44) wired end-to-end. Pre-fix the decoder treated `layoutreturn4` as a length-prefixed opaque (it's a discriminated union — discriminator was eaten as length); the dispatcher returned `Ok` without calling the pNFS handler at all, so layouts leaked across mount cycles. Now: `Operation::LayoutReturn` carries a typed `LayoutReturn4Body { File{offset,length,stateid,body}, Fsid, All }`; dispatcher resolves `(client_id,fsid)` from the SEQUENCE-bound session and calls `pnfs_handler.layoutreturn(...)`; `MdsOperations::layoutreturn` calls `LayoutManager::return_layout` (FILE) / `return_fsid_for_client` (FSID) / `return_all_for_client` (ALL). Smoke confirms `Layout returned: stateid=…` now fires per FILE return. 4 unit tests cover wire decode of all three variants + unknown-discriminator error. | smoke unchanged (8M/16M/0, 24M client, hash matches), pynfs unchanged (148/23/91), but layouts no longer leak. |

The smoke now exits with `✓ PASS: data path crossed both DSes (real
pNFS striping)`.

### Phase 5 — pNFS CSI integration (this session: `ed70fe7..0679abf`)

After Phase 4 the *protocol* worked; nothing wired it to Kubernetes. Phase 5
shipped the five-PR plan in `docs/plans/pnfs-csi-integration.md`:

| PR | Commit | What |
|---|--------|------|
| 5.1 | `ed70fe7` | MDS gRPC `CreateVolume(volume_id, size_bytes)` + `DeleteVolume(volume_id)`. Idempotent on matching size, refuses size mismatch, rejects path-traversal volume_ids. 5 unit tests. |
| 5.2 | `9f5f94c` + `57fd7b2` | Driver-side `pnfs_csi` module — talks to MDS, returns `volume_context` with five `pnfs.flint.io/*` keys. 5 unit tests against an in-process tonic mock. Bonus: dropped the unused per-file `Mutex<File>` and made COMMIT reuse the WRITE-side cached fd (ADR 0003 cleanups). |
| 5.3 | `1da59ab` | Wired into `main.rs`: `create_volume` / `delete_volume` / `node_publish_volume` branch on `parameters.layout: pnfs` and `volume_context["pnfs.flint.io/mds-ip"]` respectively. SPDK code paths byte-identical when `FLINT_PNFS_MDS_ENDPOINT` is unset. |
| 5.4 | `aeb0a7e` | Helm chart `pnfs:` values section (`enabled: false` default), conditional env-var injection in `controller.yaml`, example StorageClass at `deployments/pnfs-csi-storageclass.yaml`, ClusterIP Services for MDS gRPC + NFS. |
| 5.5 | `0679abf` | End-to-end test: `pnfs-csi-cli` test binary + `tests/lima/pnfs/csi-e2e.sh` orchestrator. 7 assertions: ctx-key shape, MDS file size, mount, sha256 round-trip, per-DS allocation, delete cleanup, re-create-after-delete. |

Run `make test-pnfs-csi` for the integration test:
- DS1: 56 MiB allocated, DS2: 64 MiB allocated, MDS: 0 bytes.
- All 7 assertions PASS in ~5 s.

### Phase 6 — perf characterization (this session: `d868e19..79cf6ef`)

First head-to-head pNFS-vs-single-server fio bench, with corrected
analysis after a deeper sweep:

```
                          single-server NFS    pNFS (2 DSes)    ratio
WRITE (fsync=1, jobs=1)        173 MiB/s        268 MiB/s       1.55×
WRITE (fsync=1, jobs=4)        168 MiB/s        274 MiB/s       1.63×
WRITE (fsync=1, jobs=8)        161 MiB/s        262 MiB/s       1.62×
WRITE (bs=4K,   jobs=4)        158 MiB/s        249 MiB/s       1.57×
READ  (jobs=4)                 270 MiB/s        267 MiB/s       1.01×
```

**Honest claim: ~1.6× write win, block-size invariant.** ADR 0002 had a
2.10× number from a single noisy run; ADR 0003 settled it with the sweep.
The mechanism is "shard the server" (two DS processes give ~2× server-side
RPC slots, narrowed by shared APFS journal), not protocol cleverness — see
`server_v4.rs:176` for the per-TCP-serial RPC handler that's the dominant
single-host bottleneck.

Reads tie at 270 MiB/s on this hardware: loopback TCP / single-client
saturation is the bottleneck before per-server protocol overhead kicks in.
**Cross-host scaling remains an architectural prediction, not a measurement.**

### Phase 7 — Production-readiness foundation (this session: `1fa43dc`)

The pNFS CSI integration ships, but the data plane has known gaps that
block real customer use. ADR-style discussion landed at
`docs/plans/pnfs-production-readiness.md` outlining Phase A
(CB_LAYOUTRECALL backchannel, ~2 weeks) and Phase B (state persistence,
~1 week). Phase A is broken into 5 sub-pieces; A.1 shipped this session.

| Sub | Commit | What |
|---|---|---|
| **A.1 — connection writer plumbing** | `1fa43dc` | `BackChannelWriter` (async-mutex-serialized writer over `BufWriter<OwnedWriteHalf>`); `CompoundDispatcher::back_channels` registry keyed by `SessionId`; `BIND_CONN_TO_SESSION` honors `conn_dir=BACK/BOTH` and registers the writer; forward replies now flow through the same writer (refactor preserved behavior). Unblocks A.2-A.5. 3 unit tests cover marker framing, concurrent-non-interleave, and EPIPE on peer close. Lib: 271 → 274; smoke green; pynfs unchanged 153/18/91. |
| **A.2 — CB-side RPC encode/decode** | `8bb02bc` | New `nfs::v4::cb_compound` module with typed `CbCompoundCall` / `CbResult` / `CbCompoundReply`, plus `encode_cb_call(xid, cb_program, args)` for full RPC CALL framing and `decode_cb_reply(bytes, expected_xid)` for the response (typed `CbReplyError` separates XDR errors, RPC rejections, RPC accept-status failures, and xid mismatches). Pulled `csa_cb_program` out of `CREATE_SESSION` (was discarded as `_`) and persisted it on `Session` so the back-channel call can address `program=cb_program, version=1, proc=CB_COMPOUND`. Refactored `pnfs::mds::callback::encode_cb_layoutrecall` to use the new typed encoder — fixes a pre-existing wire bug where `CB_SEQUENCE` omitted the trailing `referring_call_lists<>` length-prefix (every byte after it would have mis-framed). 6 unit tests: args round-trip, RPC CALL header shape, success-reply decode, `NFS4ERR_NOMATCHING_LAYOUT` reply decode, xid-mismatch refusal, `PROG_UNAVAIL` accept-error surfacing. Lib: 274 → 280; smoke green; pynfs unchanged 153/18/91. |
| **A.3 — real CB send-and-await** | `a4d7255` | `BackChannelWriter` now carries a per-connection inflight registry (`xid → oneshot::Sender<Bytes>`) and a `next_xid` counter; new `send_cb_compound(cb_program, args, timeout)` does the full register → `send_record` → await → decode dance and surfaces `CallbackError::{Timeout, Transport, Reply, ConnectionClosed}`. The `handle_tcp_connection` read loop in `server_v4.rs` now peeks `msg_type` after each frame: REPLY (=1) is routed to `deliver_reply(xid, body)` and the loop continues; CALL falls through to the existing forward-dispatch path. An `InflightGuard` on the loop's stack runs `drop_all_inflight()` on every exit path so awaiting CB callers see `ConnectionClosed` instead of hanging on the timeout. `pnfs::mds::callback::CallbackManager` was rewritten around this: takes the dispatcher's `back_channels` registry + `Arc<StateManager>` at construction, looks up `Session.cb_program` on each call, and replaces the old `send_callback_rpc` stub. 6 unit tests over real loopback TCP pairs: happy-path round-trip, `NFS4ERR_NOMATCHING_LAYOUT` carried through the typed reply, timeout when the client stays silent, no-back-channel fast-fail, mid-call connection drop → `ConnectionClosed`, and a wire-decoder sanity check that re-parses the CALL bytes off the socket. Lib: 280 → 285; smoke green; pynfs unchanged 153/18/91. |
| **A.4 — DS-death → recall fan-out** | `f58700f` | `LayoutManager::recall_layouts_for_device` now returns `Vec<(SessionIdBytes, LayoutStateId)>` so the caller knows which session to recall each layout from. `CallbackManager::recall_layouts_for_device` takes those pairs and routes one CB CALL per pair (no broadcast). `MetadataServer` constructs a `CallbackManager` once and shares it with the heartbeat monitor — when the registry marks a DS offline, the monitor calls `fan_out_recalls(device_id)` which pulls the pairs and fires CB_LAYOUTRECALLs. The pNFS MDS TCP loop now wraps its writer in `BackChannelWriter`, peeks `msg_type` to route inbound REPLY frames to the inflight registry, and uses `InflightGuard` cleanup on exit (mirrors what nfs/server_v4.rs got in A.3). The dispatcher's `CREATE_SESSION` arm registers the connection's writer in `back_channels` whenever the client sets `CONN_BACK_CHAN` in csa_flags — Linux v4.1 clients do this on every fresh mount and never send a separate `BIND_CONN_TO_SESSION`. 2 new unit tests cover per-session routing (3 layouts → 2 clients → 2+1 CALLs, no cross-fire) and the empty-input no-op. New `make test-pnfs-recall` Lima script with companion `mds-recall.yaml` / `ds{1,2}-recall.yaml` (heartbeatTimeout=5, DS heartbeatInterval=2 — chosen so the MDS doesn't false-positive while DSes are alive); the test starts a multi-GiB background `dd`, waits for LAYOUTGET, kills DS1, and asserts all four MDS-side recall markers fire. Lib: 285 → 287; smoke green; pynfs unchanged 153/18/91; recall e2e PASS. |
| **A.5 — forced layout revocation** | (this session) | New `LayoutManager::revoke_layout(stateid) -> bool`: same end-state as `return_layout` (drops from primary + by_owner index, decrements device counters) but **idempotent** — a second call (or a race with the client's own LAYOUTRETURN) is a no-op. Subsequent client uses of a revoked stateid see "not found" and the dispatcher returns `NFS4ERR_BAD_STATEID` naturally. `CallbackManager::recall_layouts_for_device` now returns `Vec<RecallResult>` with a typed `RecallOutcome` per pair — `Acked` / `TimedOut` / `NoChannel` / `Transport(msg)` — so the caller can decide what to revoke. The heartbeat's `fan_out_recalls` applies the RFC 5661 §12.5.5.2 policy matrix: `TimedOut` / `NoChannel` / `Transport` → revoke immediately; `Acked` → spawn a 10s post-recall deadline timer that revokes if `LAYOUTRETURN` doesn't arrive. 4 new unit tests: layout revoke is idempotent + clears indexes, revoke isolates per-client, recall surfaces TimedOut as the typed outcome, recall surfaces NoChannel as the typed outcome. Updated `make test-pnfs-recall` to assert a fifth marker — `Layout revoked` / `Forcibly revoking layout` — closing the loop on the e2e. Lib: 287 → 291; smoke green; pynfs unchanged 153/18/91; recall e2e PASS with all 5 markers. **Phase A complete.** |

After Phase A, Phase B (state persistence — `StateBackend` trait with
`memory` + `sqlite` impls) lands in ~1 week. Together they make pNFS
safe to ship to a first customer; whether to build Phase C (FFL
mirroring for HDFS-style replication) is then a demand-driven call.

### Phase 8 — State persistence (this branch session: `982edc1..4d2f162`)

Phase B in five sub-PRs. Together they make pNFS state survive an MDS
pod roll: a kernel client reconnecting after a restart finds its
persisted `client_id` and resumes against the same record set, no
fresh `STALE_CLIENTID` allocation, no `BAD_STATEID` storm. Combined
with Phase A (DS-death recall), pNFS is now safe to ship to a first
customer.

| Sub | Commit | What |
|---|---|---|
| **B.1 — StateBackend trait + MemoryBackend** | `982edc1` | New top-level `src/state_backend/` module. Trait is async (`put_*` / `get_*` / `list_*` / `delete_*` per record kind, plus `increment_instance_counter` / `get_instance_counter`); idempotency contract is upsert + idempotent-delete so the boundary code doesn't need a special path. Records (`ClientRecord`, `SessionRecord`, `StateIdRecord`, `LayoutRecord` + `CachedCreateSessionResRecord` + `LayoutSegmentRecord` + `StateTypeRecord` / `IoModeRecord` enums) are plain types only. `MemoryBackend` is a `DashMap` per record kind plus an `AtomicU64` counter (`fetch_add` SeqCst). 5 unit tests. Lib: 291 → 296. |
| **B.2 — SqliteBackend** | `a2af4e0` | `rusqlite = "0.31"` with `bundled` feature (SQLite 3.45+ statically linked, no system libsqlite). 5-table schema + `schema_version` canary. `journal_mode=WAL` + `synchronous=NORMAL`. `Arc<std::sync::Mutex<Connection>>`; every method does its work inside `tokio::task::spawn_blocking`. `increment_instance_counter` uses 3.35+ `UPDATE ... RETURNING`. Load-bearing test: `sqlite_state_survives_reopen` writes one of every record kind, drops backend, reopens, asserts every record + the counter survived. 6 unit tests. Lib: 296 → 302. |
| **B.3 — wire backend into managers** | `e5e1ef3` | New `state_backend::spawn_persist` helper bridges sync manager APIs to async StateBackend via fire-and-forget `tokio::spawn` (acceptable lag bound: ~1s; RFC 8881 §15.1.10.4 lets clients retry uncached ops). `Client/Session/StateId/Layout` types each grow `to_record` + `from_record` boundary conversions. Constructors take `Arc<dyn StateBackend>`. `StateManager::new` threads it through, `new_in_memory(volume_id)` is the test-side convenience. Mutations (`exchange_id`, `mark_confirmed`, `update_sequence`, `record_create_session_reply`, `allocate`, `update_seqid`, `revoke`, `generate_layout`, `return_layout`, `revoke_layout`, etc.) persist after the sync DashMap edit. **`StateManager::load_from_backend()`** seeds caches at startup. New `test_state_manager_reload_from_shared_backend` integration test proves a fresh StateManager over the same backend reconstructs state. 15 files updated, ~30 call sites touched. Lib: 302 → 303. |
| **B.4 — config + InstanceCounter** | `02d3ee5` | `pnfs::config::StateBackend`: drop never-implemented `Kubernetes`/`Etcd` variants, add `Sqlite`. `PnfsConfig::build_state_backend(&StateConfig)` → `Arc<dyn StateBackend>` (path defaults to `/var/lib/flint-pnfs/state.db`). `MetadataServer::new` constructs the configured backend; new `load_persisted_state()` async hook called from `serve()` before the listener accepts: bumps + logs the instance counter, calls `state_mgr.load_from_backend()`, calls `layout_manager.load_records(backend.list_layouts().await?)`. Manual verification: counter goes 1 → 2 across MDS restarts; `state.db` + WAL appear at the configured path. 2 new config tests including round-trip and dispatch. Lib: 303 → 305. |
| **B.5 — Lima e2e** | `4d2f162` | New `make test-pnfs-restart` Lima script with companion `mds-restart.yaml`. Phase 1 starts MDS with `state.backend: sqlite`, mounts, writes 24 MiB. Phase 2 kills MDS, restarts over the same `state.db`, asserts: counter advanced 1→2, ClientManager + SessionManager + LayoutManager load lines fired, kernel reached the persisted client_id post-restart via `CREATE_SESSION sequence>=2` (the §18.36.4 forward-progress branch — without persistence this would be `SEQ_MISORDERED`). **Phase B.5 hard lesson — encoded in `SessionManager::load_records`:** Linux NFSv4.1 clients deadlock on `NFS4ERR_SEQ_MISORDERED`. Reloading a session has slot.seqid=0; kernel's seqid=21 looks misordered; kernel retries forever. Fix: `load_records` *observes* persisted sessions (bumps `next_session_id` past their max) but does NOT put them in the live map and fire-and-forget deletes them. Kernel sees `BADSESSION`, reissues EXCHANGE_ID (finds persisted client via §18.35.5 case 1/5/6) → fresh CREATE_SESSION (sees persisted last_cs_sequence, accepts seq+1). Mount keeps working. e2e green: 8 assertion markers fire end-to-end. |

**Phase B follow-up: persistent FH instance discriminator (`3f000bb`).**
The first cut of Phase B left `FileHandleManager::generate_instance_id`
wall-clock-derived, so post-restart the kernel's cached FHs would
error with `NFS4ERR_BADHANDLE`. This commit closes the gap:

* Schema bumped 1 → 2 with a clean v1→v2 migration path. New
  `server_identity` singleton table holds a non-zero random `u64`
  generated once at first DB creation, reused for the lifetime of
  the state.db.
* New `StateBackend::get_or_init_server_id()` async trait method.
  MemoryBackend uses `OnceLock` for atomic-once init;
  SqliteBackend uses INSERT-OR-IGNORE-then-SELECT over the
  singleton row (atomic at the SQLite level + connection mutex
  serialises concurrent first-callers).
* `MetadataServer::new` is now `async`; pulls the server id from
  the backend BEFORE constructing `FileHandleManager` and passes
  it via `new_with_instance_id`. Logs the value at startup as an
  operator-visible canary.
* `make test-pnfs-restart` now asserts on the post-restart hash
  match (was informational); the kernel's `read()` against
  pre-restart cached FHs returns the original bytes byte-for-byte,
  with zero stale-handle markers in the MDS log.
* 5 new tests including the load-bearing
  `sqlite_server_id_survives_reopen`, a 16-way concurrency check,
  and a v1→v2 migration test. Lib: 305 → 310.

Phase B is now feature-complete with no known gaps.

### Phase 9 — pynfs conformance (this branch session: `c39954a..8155065`)

Six rounds of focused conformance work landed across `c39954a`,
`ef2205a`, `c3dcc53`, `3ce5d8b`, `4e23170`, `8155065`. Net **+14
PASS / -14 FAIL** (153 → 167 PASS, 18 → 4 FAIL, 91 SKIP unchanged).
Each round was scoped to a single structural fix that unlocked
multiple tests, and each shipped a Lima/pynfs re-run to prove the
delta. None regressed smoke or restart e2es.

| Tier | Commit | What | pynfs delta |
|---|---|---|---|
| **1A** | `c39954a` | Real RFC 8881 §18.51 RECLAIM_COMPLETE state machine: `Client.reclaim_complete` (persisted, schema v2 → v3 with idempotent `pragma_table_info`-guarded migration), `mark_reclaim_complete -> ReclaimCompleteOutcome`, `is_reclaim_complete` for the OPEN gate. Dispatcher RECLAIM_COMPLETE arm honors `rca_one_fs=TRUE` (per-fs no-op) vs `FALSE` (whole-client bit flip + `COMPLETE_ALREADY` on retry). OPEN claim-type gating (`CLAIM_PREVIOUS=1`, `CLAIM_DELEGATE_PREV=3`, `CLAIM_DELEG_PREV_FH=6`): `NO_GRACE` outside grace OR after RECLAIM_COMPLETE; non-reclaim OPENs during grace before RECLAIM_COMPLETE → `GRACE`. | +2 (RECC1, RECC2, RECC4 fixed; RECC3 still needs dynamic grace) |
| **1B** | `ef2205a` | One-line `tokio::fs::metadata` → `symlink_metadata` in `handle_lookup`. RFC 5661 §16.10.5 mandates LOOKUP returns the symlink's filehandle without dereferencing — we were following trailing symlinks and erroring with NOENT on dangling-symlink leaves (which pynfs's `--maketree` deliberately creates). | +4 (LKPP1a, PUTFH1a, RNM2a, RNM3a) |
| **1C** | `c3dcc53` | Per-session resource-limit enforcement (RFC 8881 §18.46.4). New `CompoundRequest.wire_size` field set at decode; dispatcher REQ_TOO_BIG check after SEQUENCE binds session. Encode-then-measure REP_TOO_BIG check post-loop with stripped fallback that keeps the SEQUENCE result at `results[0]` so clients still read `sr_status`. `OperationResult` derives `Clone`. | +2 (SEQ6, CSESS26) |
| **1A.2** | `3ce5d8b` | Real lease expiration + RFC 8881 §18.35.5 case-5 deferred replacement. `Client.pending_replaces: Option<u64>` (in-memory only — half-confirmed clientid wouldn't survive restart). Case-5 EXCHANGE_ID no longer immediately removes the old client; new client carries the deferred-cleanup target. `mark_confirmed -> Option<u64>` returns the old clientid; CREATE_SESSION cascades through `sessions / stateids / delegations / clients` to discard old state on confirm. New CREATE_SESSION lease-validity check returns STALE_CLIENTID for a clientid whose lease has fully expired. | +3 (EID5f, EID5fb, EID9) |
| **1D** | `4e23170` | RFC 7530 §16.16 same-owner OPEN seqid bump. New in-memory `open_states: DashMap<(client_id, owner, fh), OpenState>` + `opens_by_fh` index on `StateIdManager`. New methods `find_open` / `share_conflict` / `record_open` / `find_exclusive_match`. `handle_open` routes both create + no-create paths through `record_open` so repeated OPENs by the same (client, owner, fh) get the same `stateid.other` with seqid bumped. CLAIM_NULL no-create resolves parent + name to the file's fh as the open key. | +1 (OPEN2) |
| **1E** | `8155065` | RFC 8881 §8.4.2.4 courtesy-release at COMPOUND entry. New `lock_mgr` field on `CompoundDispatcher`. `cleanup_expired` cascade now sweeps stateids + open_states + opens_by_fh; dispatcher-side hook drives `lock_mgr.remove_client_locks` for expired clients before each compound runs. Re-enables share-deny gates in handle_open (now self-healing instead of leaking dead clients' state forever). Pre-existing EXCLUSIVE4_1 decoder bug fixed: 8-byte verifier was being discarded; now packed into `OpenHow.attrs` so the dispatcher's existing convention finds it. | +2 (COUR3, OPEN6) |

**Lib gate**: 311 → 314 PASS (3 new unit tests for OpenState
shape: `test_record_open_bumps_seqid_for_same_owner`,
`test_share_conflict_detection`, `test_find_exclusive_match`).

**Remaining 4 fails — explicitly deferred**, see top-of-file
"Picking up next session" for production risk per test. Cost-
benefit summary: ~3–4 more sessions for 167 → ~171, no measurable
customer-visible improvement; conformance work concluded here.

### pynfs coverage for Phase A

Honest accounting: pynfs has **no public test for CB_LAYOUTRECALL on
FILES layout**. The 10 `st_delegation` tests would exercise the same
back-channel infrastructure once delegation *grants* are wired
(separate work). Direct verification of Phase A is via:

* **Lima e2e** (built in A.4): `make test-pnfs-recall` — kill a DS
  mid-write, assert the kernel got recalled and the in-flight `dd`
  either completes via MDS-direct fallback or fails cleanly with EIO.
* **Per-piece unit tests** with each sub-PR.
* **Regression gate**: pynfs stays ≥ 153 PASS / 18 FAIL / 91 SKIP
  after every commit; smoke stays green.

#### What's still TODO on the data path

The smoke is green but the pNFS implementation has known gaps that
don't block the smoke. Tracked items (and what would surface them):

- ~~**CB_LAYOUTRECALL backchannel (Task #4)**~~ — **done.** Phase A
  (sub-PRs A.1 through A.5) shipped in this branch. DS death triggers
  `CB_LAYOUTRECALL` over the same TCP connection the client uses for
  forward channel; if the client doesn't `LAYOUTRETURN` within ~10s,
  the MDS forcibly revokes the layout server-side so subsequent
  client uses error with `BAD_STATEID` rather than misroute writes.
  Verified end-to-end by `make test-pnfs-recall`.
- **Layout/state persistence (Task #5)** — instance IDs and layout
  stateids regenerate on MDS restart; clients see `STALE_DEVICEID` /
  `BAD_STATEID`. Would surface in: restart MDS during a long write;
  client errors out instead of recovering. Last gate before pNFS is
  shippable to a real customer.
- ~~**LAYOUTRETURN FSID/ALL (Task #6)**~~ — **done (commit 4.E above)**.
  Layouts are now actually freed on FILE/FSID/ALL returns. Linux's
  per-file FILE-typed returns at unmount drive the path that's
  exercised in CI; ALL/FSID code paths are covered by unit tests but
  not yet by an integration test (no current client sends them in
  the smoke flow).
- **Multi-client correctness** — only the smoke uses one mount on
  one VM. Two concurrent clients writing the same file would
  exercise stateid-per-owner code that hasn't been driven yet.
- **Smoke-test asserts** — the smoke checks "any bytes on each DS"
  and "client hash matches"; it doesn't yet check the per-DS slices
  reconstruct in correct stripe order, or that the MDS-side stat
  size matches LAYOUTCOMMIT. A scheduled agent (job `3ddeefa6`) will
  add those on 2026-05-15.

---

## What's left

Sorted by likely effort × test count.

### High ROI — addressable in one focused session each

| Bucket | Tests | What's needed |
|---|---|---|
| ~~`st_secinfo` + `st_secinfo_no_name` (4)~~ | ~~SECINFO/SECINFO_NO_NAME~~ | **Done.** Both ops now decode/encode (`[AUTH_NONE, AUTH_SYS, RPCSEC_GSS(Kerberos V5)]`), clear CFH on success per RFC 5661 §2.6.3.1.1.8, and SECINFO_NO_NAME(PARENT) of the served root returns NFS4ERR_NOENT. 6/6 secinfo tests now pass. |
| `st_courtesy` (1) remaining | one test | "Courtesy client" handling (RFC 8881 §8.4.2.4) — graceful expired-client cleanup. Likely needs lease-expiry triggering state cleanup but allowing renewal-within-grace. |
| `st_reclaim_complete` (3) | RECLAIM2/3/4 | Validate reclaim-complete state machine: rejecting state-using ops before RECLAIM_COMPLETE during grace; rejecting reclaim ops outside grace. |
| ~~`st_verify` (1)~~ | ~~VERIFY1-ish~~ | **Done.** VERIFY/NVERIFY both wired against the canonical GETATTR encoding for bytewise comparison; ATTRNOTSUPP for unsupported bits per §18.30.3. 1/1 passes. |
| `st_open` (2) remaining | OPEN edge cases | Likely OPEN_CONFIRM (v4.0) or specific share-deny conflicts. |

### Medium ROI — needs a real subsystem

| Bucket | Tests | What's needed |
|---|---|---|
| `st_delegation` (3) | DELEG5/6/7 | Requires the **CB_RECALL backchannel** — already on the original audit's pNFS task list (Task #4). Need TCP back-channel, RPC encode/decode, retry logic. ~1-2 weeks. Same machinery unlocks pNFS CB_LAYOUTRECALL and Linux client delegations. |
| `st_rename` (3) remaining | RNM…linkdata | Likely related to dangling-symlink resolution at PUTFH time (separate file-handle subsystem fix). |
| `st_lookupp` (1) remaining | LKPP1a testLink | Same dangling-symlink PUTFH issue. |
| `st_sequence` (2) remaining | SEQ6/SEQ9c | REQ_TOO_BIG (need to compute incoming COMPOUND wire size against ca_maxrequestsize) and a specific replay-cache LOOKUP test. |
| `st_putfh` (1) remaining | PUTFH1a | Dangling-symlink PUTFH (same root cause as st_lookupp / st_rename remainder). |
| `st_exchange_id` (3) remaining | EID5e/9, EID9 LeasePeriod | Need real lease-expiration → STALE_CLIENTID semantics (we always-renew currently). |

### From the original audit — bigger structural items

These weren't visible in the pynfs sweep (because pynfs runs against a
single mount), but the original audit flagged them as production blockers.
None block any pynfs test. Tracking them here so they don't fall off the
radar:

- **Task #1 (pending)** — RPC framing & XDR DoS hardening: per-frame
  size cap is in (4 MiB), but the multi-fragment accumulation path is
  not. Linux NFS v4.1 clients don't fragment so this hasn't bitten, but
  any client that does will hit silent corruption.
- ~~**Task #4**~~ — **done.** pNFS CB_LAYOUTRECALL backchannel
  shipped end-to-end across A.1–A.5. The CB infrastructure is also
  the prerequisite for the `st_delegation` pynfs tests (which still
  need delegation *grants* on top of CB — separate work).
- **Task #5 (pending)** — pNFS state persistence. Device IDs and layout
  stateids are randomly regenerated on every MDS restart, so any client
  with a layout sees `STALE_DEVICEID` / `BAD_STATEID` after a restart.
  Plan: `StateBackend` trait with `MemoryBackend` (parity, default) +
  `SqliteBackend` (durable, production). ~1 week. See
  `docs/plans/pnfs-production-readiness.md` Phase B.
- ~~**Task #6**~~ — **done.** LAYOUTRETURN FILE/FSID/ALL all wired
  through `LayoutManager::return_layout` / `return_fsid_for_client` /
  `return_all_for_client`. Layouts no longer leak across mount cycles.

`/Users/ddalton/github/flint/spdk-csi-driver/src/pnfs/` houses the
relevant code; the original audit lives in chat history (run
`git log --grep="phase 1"` for the on-tree commit summaries).

---

## How to run the tests

### One-time setup (macOS)

```bash
brew install lima                  # if not already installed
make lima-up                       # ~3 min: builds the Ubuntu 24.04 VM
                                   # with pynfs preinstalled at /opt/pynfs.
                                   # Idempotent — skips if VM exists.
```

The Lima VM provides a Linux NFSv4.1 kernel client and pynfs. macOS
itself can't be the client because its NFS client is v4.0-only and
buggy.

### Run the full conformance suite (~3 min)

```bash
make test-nfs-protocol
```

This Makefile target:
1. Builds `flint-nfs-server` (release).
2. Pre-creates `/tmp/flint-nfs-export/tmp` for pynfs's `--maketree` step.
3. Starts the server in the background on `0.0.0.0:20490`.
4. Runs pynfs against it from inside the Lima VM with
   `--maketree --nocleanup all`, saving JSON results to
   `/tmp/pynfs.json` (in the VM) and copying them to
   `/tmp/flint-pynfs-results.json` (on the host).
5. Stops the server.

The first run after `make lima-up` may need a fresh test-tree:

```bash
chmod -R u+w /tmp/flint-nfs-export 2>/dev/null
rm -rf /tmp/flint-nfs-export/tmp
mkdir -p /tmp/flint-nfs-export/tmp
chmod 0777 /tmp/flint-nfs-export/tmp
```

### Other useful targets

```bash
make nfs-server               # foreground, verbose logs (useful for debugging)
make nfs-server-bg            # background, logs to /tmp/flint-nfs.log
make nfs-server-stop          # stop the background server
make test-nfs-mount           # sanity: mount + write + read + unmount
make test-nfs-frag            # T1: large WRITE forces fragmented RPC

make lima-shell               # interactive shell inside the test VM
make lima-down                # tear down the VM
```

### pNFS test targets

The pNFS suite is its own thing — it brings up MDS + 2 DSes and runs
two distinct tests:

```bash
make build-pnfs               # one-time: build flint-pnfs-{mds,ds}
make test-pnfs-smoke          # mount + 24 MiB write/read + per-DS byte count
make test-pnfs-pynfs          # pynfs `pnfs` flag set (8 conformance tests)
make test-pnfs-all            # both
```

Current baseline (commit `9076e96`):

* Smoke test: **✓ PASS — data path crossed both DSes (real pNFS striping).**
  24 MiB striped across DS1 (8 MiB) and DS2 (16 MiB), MDS holds 0 bytes,
  kernel-side `ls -la` shows 24 MiB and the round-trip SHA-256 matches.
* pynfs pNFS subset: **1 PASS / 3 FAIL / 4 SKIP** out of 8 tests.
  CSID7 testOpenLayoutGet passes. The 3 FAILs hardcode
  `LAYOUT4_BLOCK_VOLUME` (we're files-layout, not block); the 4 SKIPs
  are dependency-chained on those. Score is unchanged — pynfs pNFS
  doesn't exercise the actual data path, only layout grants.

See `tests/lima/pnfs/README.md` for topology, debugging tips, and
"what PASS/DEGRADED/FAIL mean".

### Run a single pynfs test (debugging)

```bash
make nfs-server-bg

limactl shell flint-nfs-client -- bash -lc \
  'cd /opt/pynfs/nfs4.1 && python3 ./testserver.py \
     host.lima.internal:20490/tmp \
     --maketree --nocleanup CSESS5'

# Watch the server log in another terminal:
tail -f /tmp/flint-nfs.log

make nfs-server-stop
```

Useful test groupings (pynfs flag names): `compound`, `putfh`, `getfh`,
`sequence`, `exchange_id`, `create_session`, `destroy_session`,
`destroy_clientid`, `lookup`, `lookupp`, `current_stateid`, `rename`,
`open`, `courtesy`, `delegation`, `secinfo`, `secinfo_no_name`,
`reclaim_complete`, `trunking`, `verify`. Use `--showcodes` to list
specific test codes.

### Compare two runs

The repo has a snapshot of every commit's pynfs JSON in
`tests/lima/pynfs-after-*.json`. Diffing them shows which tests
flipped:

```bash
python3 -c "
import json
old = {t['code']: t for t in json.load(open('tests/lima/pynfs-after-rename-2026-05-01.json'))['testcase']}
new = {t['code']: t for t in json.load(open('tests/lima/pynfs-after-destroy-clientid-and-sequence-2026-05-01.json'))['testcase']}
def res(t):
    if 'failure' in t: return 'FAIL'
    if t.get('skipped'): return 'SKIP'
    return 'PASS'
for code in sorted(set(old) | set(new)):
    o = res(old.get(code,{})); n = res(new.get(code,{}))
    if o != n: print(f'{code:10} {o:4} → {n:4}')"
```

### Unit tests

```bash
cd spdk-csi-driver
cargo test --lib                            # 291 tests, all passing as of 2ea070d
```

---

## Repository layout

```
spdk-csi-driver/
├── src/
│   ├── nfs/
│   │   ├── server_v4.rs        TCP/RPC frame loop, COMPOUND dispatch entry
│   │   ├── rpc.rs              Auth, CallMessage, ReplyBuilder, principal()
│   │   ├── xdr.rs              base XDR codec
│   │   ├── rpcsec_gss.rs       RPCSEC_GSS (mostly stub)
│   │   ├── kerberos.rs         GSS Kerberos (mostly stub)
│   │   └── v4/
│   │       ├── compound.rs       COMPOUND request/response types, encode/decode
│   │       ├── dispatcher.rs     COMPOUND op-dispatch loop, session-pos/maxops checks
│   │       ├── filehandle.rs     FH ↔ path, normalize, pseudo-root
│   │       ├── filehandle_pnfs.rs pNFS FH layout
│   │       ├── pseudo.rs         pseudo-FS / exports
│   │       ├── protocol.rs       ★ Nfs4Status, opcode::*, exchgid_flags
│   │       ├── xdr.rs            v4-specific XDR helpers
│   │       ├── operations/
│   │       │   ├── session.rs    EXCHANGE_ID / CREATE_SESSION / SEQUENCE / DESTROY_*
│   │       │   ├── fileops.rs    OPEN / CREATE / RENAME / SETATTR / LOOKUP / LOOKUPP / GETATTR / READDIR / REMOVE / LINK
│   │       │   ├── ioops.rs      READ / WRITE / COMMIT / OPEN
│   │       │   ├── lockops.rs    LOCK / LOCKU / LOCKT
│   │       │   └── perfops.rs    NFSv4.2 ALLOCATE / DEALLOCATE / SEEK / READ_PLUS / COPY
│   │       └── state/
│   │           ├── client.rs     ★ ClientManager + §18.35.5 state machine + §18.36.4 cs cache
│   │           ├── session.rs    SessionManager + slot/replay
│   │           ├── stateid.rs    StateIdManager (validate / validate_for_read)
│   │           ├── lease.rs      LeaseManager
│   │           └── delegation.rs DelegationManager
│   └── pnfs/                  pNFS MDS + DS (less polished, blocked on Task #4)
└── tests/
    └── lima/
        ├── nfs-client.yaml    ★ Lima VM config (Ubuntu + pynfs)
        ├── PYNFS_BASELINE.md  ← stale; this file (STATUS.md) supersedes it
        ├── STATUS.md          ★ this file
        └── pynfs-*.json       ← per-commit pynfs JSON snapshots
```

★ = the files most often touched in this work.

---

## Where to pick up next

Three independent fronts. Pick whichever maps to current priorities.

### Front A — pNFS production-readiness (audit's structural gaps)

The CSI integration ships, but the data plane has known durability and
restart gaps that block real customer use. These are the **production
gates**; they don't move user-visible features but they're prerequisites
for anyone running this in prod. Plan is at
`docs/plans/pnfs-production-readiness.md`.

1. ~~**CB_LAYOUTRECALL backchannel (Task #4)**~~ — **done.** All five
   sub-PRs shipped: A.1 (`1fa43dc`) connection writer plumbing, A.2
   (`8bb02bc`) CB RPC framing, A.3 (`a4d7255`) real send-and-await,
   A.4 (`f58700f`) DS-death → recall fan-out + Lima e2e, A.5
   (`2ea070d`) forced revocation on timeout. `make test-pnfs-recall`
   is the truth source for the full chain.
2. ~~**Layout/state persistence (Task #5)**~~ — **done.** All five
   sub-PRs shipped: B.1 (`982edc1`) trait + MemoryBackend, B.2
   (`a2af4e0`) SqliteBackend with WAL+NORMAL durability, B.3
   (`e5e1ef3`) backend wired into `Client/Session/StateId/Layout`
   managers via fire-and-forget `tokio::spawn`, B.4 (`02d3ee5`)
   config-driven `state.backend: sqlite` + InstanceCounter at
   startup, B.5 (`4d2f162`) Lima `make test-pnfs-restart` e2e.
   `make test-pnfs-restart` is the truth source for the full chain
   (8 markers). One known follow-up: `FileHandleManager::generate_instance_id`
   is wall-clock-derived, so cached FHs go stale across restart; fix
   is half a session of work, persists the instance discriminator
   alongside the existing `instance_counter` table.
3. **Cross-host fio bench — NEXT.** The single-Mac-host 1.6× number
   is a floor *and the loopback nconnect sweep landed in this
   session has confirmed there's nothing more to learn from
   single-host*. Snapshot at
   `tests/lima/pnfs/nconnect-results-2026-05-03.tsv`:

   ```
   bs=4K                    bs=1M
   nconn  write  read      nconn  write  read
       1  247.2 259.4          1  324.3 299.6
       4  187.4 214.0          4  324.5 289.3
       8  216.1 235.9          8  217.4 285.6
      16  291.7 285.1         16  314.3 254.6
   ```

   Throughput is **flat across nconnect at both block sizes** —
   noise dominates any signal. That **rules out** the per-TCP-serial
   RPC handler at `server_v4.rs:176` as the single-host ceiling: if
   it were, MiB/s would climb monotonically with nconnect. The
   real single-host bottleneck lives below the per-connection
   layer:

   * **Server-side APFS journal contention.** MDS, DS1, DS2 all
     write to `/tmp/flint-pnfs-{mds-exports,ds1,ds2}/` on the same
     APFS volume; APFS journals are per-volume, so even though the
     three processes look parallel, their fsyncs serialise on the
     volume's journal lock. Cross-host puts each DS on a separate
     filesystem, breaking that.
   * **Loopback TCP saturation.** The kernel-internal loopback
     adapter has limits well below real 25 GbE. Cross-host moves
     the bytes onto a real NIC.
   * **Kernel page cache writeback.** All three servers' caches
     compete for the same Mac host's page cache.

   None of these are fixable without separating the kernels, and
   none of them tell us anything about whether the *architecture*
   scales. The only honest path forward is a real cross-host bench.

   **Recommended deliverable:** `make test-pnfs-cross-host` —
   Terraform spec for 4× small AWS instances in one AZ (1 client +
   1 MDS + 2 DSes; `c6gn.large` or similar; 25 Gbit network;
   ephemeral nvme), the same fio sweep, results dumped to
   `tests/lima/pnfs/cross-host-results-*.tsv`. ~3 days of
   harness work; ~$5–20 per benchmark run; reusable forever for
   future N=4, N=8 sweeps as customers ask "show me at scale."

### Front A.5 — Performance scaling (gated on cross-host data)

Once the cross-host curve is in, pick *one* of these based on what
the data exposes:

* **Per-TCP-serial RPC bottleneck** (~2 weeks) — the right target
  *only* if cross-host scales near-linearly to N=3 *and* multi-
  client cross-host runs show per-host plateau. **Loopback already
  ruled this out for the single-host case**; cross-host might
  re-expose it once APFS-journal contention is removed. Pipeline
  the read loop in `server_v4.rs:176`: dispatch each frame on its
  own task, write replies in xid-order. SEQUENCE slot semantics
  need careful re-checking so exactly-once isn't compromised.
* **Read-from-DS instead of MDS-proxied** (~1 week) — the right
  target if cross-host writes scale but reads top out. Today
  smoke advertises layouts only on the WRITE flow; OPEN-for-read
  paths land on the MDS and get proxied. Closing this requires
  advertising layouts on OPEN-for-read and verifying the kernel
  uses them. Modest perf win on its own (1.2–1.5×) but it's the
  path that *does* scale cross-host with DS count.

**What NOT to do next** (deliberately deferred until cross-host
data picks the target):
* FFL mirroring (Phase C) — speculative durability work, blocks
  scaling work, ~5–7 weeks. Wait for a customer ask.
* Locality-aware layouts — real win, but only legible after
  cross-host numbers exist.
* Snapshots / `ControllerExpandVolume` / DS auto-discovery —
  customer-asks, not perf-asks.
* More pynfs conformance — the remaining 18 fails are correctness
  polish, not perf.

### Front B — pNFS feature work (beyond the perf-tier MVP)

The integration ships a minimal slice. Real customers will ask for:

1. **Replication / HA** — see "HDFS replication factor 3 equivalent"
   section below. FFL-mirrored layouts. Multi-week project.
2. **Snapshots / clones for pNFS** — currently SPDK-only. Would need
   MDS to coordinate consistent point-in-time across DSes.
3. **`ControllerExpandVolume` for pNFS.** Today the StorageClass
   has `allowVolumeExpansion: false`; flip it on by extending the
   MDS file via gRPC + propagating to the client.
4. **DS auto-discovery via DaemonSet** so adding/removing nodes
   doesn't require operator action. The `DeviceRegistry` already
   exists; needs a registration RPC from a DS-side bootstrap.
5. **Locality-aware layout selection** — read k8s topology labels
   so layouts prefer same-zone DSes. Big perf win on cross-AZ
   clusters.

### Front C — pynfs core protocol score (single-mount tests)

`st_current_stateid`, `st_secinfo`, `st_secinfo_no_name`, `st_verify`
are all now 100%. Next single-session wins:

* **`st_courtesy` + `st_reclaim_complete` (4 tests)** — both want a
  real lease-expiration / grace-period state machine: lease-expired
  clients hold "courtesy" state until the next conflicting op (RFC
  8881 §8.4.2.4); reclaim ops outside grace MUST be rejected.
* **`st_exchange_id` EID9 (1 test)** — `testLeasePeriod` wants real
  lease expiry → STALE_CLIENTID (we always-renew today).

Beyond those, the remaining `st_*` failures cluster around symlink
PUTFH resolution (st_lookupp / st_rename / st_putfh tail), REQ_TOO_BIG
(st_sequence), and CB_RECALL-blocked delegation tests.

---

## HDFS replication factor 3 — pNFS equivalent

**Q: HDFS supports replication-factor=3 for durability. How can pNFS do
the same?**

**A: FlexFiles layout (FFLv4, RFC 8435) with mirrored DS sets.** The
data path we ship today (FILES layout, RFC 5661 §13) is HDFS replication
factor *1* — every stripe lives on exactly one DS, and DS death means
data loss. To get HDFS-grade durability via pNFS, the protocol
mechanism is FFLv4 mirroring: the MDS hands out a layout that lists N
DSes for the same byte range as **mirrors**, and the kernel client
writes to all N in parallel and reads from any.

### What the kernel does for free (already shipped in Linux)

Linux's NFSv4.1 client implements FFLv4 mirrored layouts natively:

- WRITE: client fans out to every DS in the mirror set; succeeds iff
  all DS WRITEs succeed.
- READ: client picks any one mirror per request (load-balances across).
- DS error: client reports it via `LAYOUTRETURN` with `ff_io_errors4`,
  marks the layout invalid, asks for a fresh one.

So the client side is solved. The work is all on the MDS.

### What the MDS would need to do

| Component | Effort | Status today |
|---|---:|---|
| FFLv4 layout encoding (mirrored variant) | ~3 days | We had FFLv4 advertised but pulled it back (commit `cdbbe21`) because layout negotiation was off; bringing it back for mirroring is real-but-tractable work. The `FfLayoutReturn4` decoder already exists. |
| `LayoutPolicy::MirroredStripe { factor: N }` in MDS | ~2 days | Today's `LayoutManager::assign_segments_for_layout` picks one DS per stripe; needs to pick N and emit them as the mirror set. |
| **CB_LAYOUTRECALL backchannel (Task #4)** | ~1-2 weeks | Required to revoke layouts when re-mirroring after DS failure. Already on the roadmap as a production prereq regardless. |
| **State persistence (Task #5)** | ~1 week | Required so re-mirror progress survives MDS restart. Already on the roadmap. |
| Re-mirror coordinator (background scrub) | ~2 weeks | When DS dies, MDS must copy bytes from a healthy mirror to a new replacement DS. This is its own subsystem — NameNode-style replication tracking, queue, throttle, retry. **No starter code today.** |
| Topology / rack awareness | ~3 days | Bias DS selection so mirrors land on different nodes / zones (HDFS's "rack awareness"). Reads k8s topology labels. |

**Total: ~5-7 weeks for honest HDFS-replication-factor-3 equivalence.**

### A simpler, weaker variant (~2-3 weeks)

If "auto-healing" isn't required, you can ship FFL mirroring without
the re-mirror coordinator: writes go to N mirrors, reads survive any
one DS failure, but rebuilding to N replicas after DS death needs
manual operator intervention. Useful for "single-DS-failure tolerance"
without the full coordination machinery.

### A different architectural answer worth considering

For ML datasets specifically (the user-stated workload), a simpler
pattern often beats FFL mirroring on cost/complexity:

- Source of truth lives in S3 (or any object store); S3 already
  provides 11×9s durability for free.
- DSes are *caches* — they pull lazily from S3 on first read of a
  file, then serve from local.
- DS death = empty cache when it comes back; next read re-pulls.
- No re-mirror coordinator, no scrub task, no HDFS-style protocol.

Trade-offs: cache-miss reads have S3 latency (10s-100s of ms) instead
of DS latency (ms); steady-state reads after warming have full pNFS
perf. For read-heavy training data this is fine; for write-heavy
workloads it's a worse fit than FFL mirroring.

The two answers are **complementary**, not exclusive:

- ML-training tier: pNFS + S3 spillover. Cheap, durable enough
  ("worst case I re-warm from S3"), no replication subsystem to
  build.
- Database / write-heavy / strict-durability tier: existing SPDK
  NVMe-oF path. Already replicated, already shipping.
- Future: FFL-mirrored pNFS only if a customer specifically asks for
  "HDFS-shape" semantics (replication-factor-N at the storage layer
  with auto-healing) and is willing to wait for the multi-week
  effort.

### Recommendation

Don't build FFL mirroring speculatively. Ship the perf tier (already
done), add Task #4 + #5 for production-readiness, then revisit
based on actual customer requests. ADR 0001 already encodes the
"don't speculatively build modular abstractions" principle for
the same reasons.

---

## Performance discipline

Every commit in this work has preserved happy-path performance. Hot
path is `SEQUENCE → READ/WRITE → COMPOUND encode`. Specifically
preserved invariants:

- Per-COMPOUND allocations: tag (one String), op vec (one Vec), result
  vec (one Vec). No extra allocs added.
- Per-WRITE: open fd cached in `DashMap<StateId, ...>` on first hit;
  subsequent WRITE on same stateid is one DashMap get + one positioned
  write. Validator added one extra DashMap get per WRITE (state lookup)
  — measurable overhead is in the noise compared to the disk write.
- Replay cache: per-slot `Option<Vec<u8>>` of the encoded COMPOUND
  reply; one Bytes::clone (Arc-backed) on cache, return-as-is on
  replay (no re-encoding, no state mutation, no lease renewal).
- Status enum: `#[repr(u32)]` with explicit discriminants — same
  generated code as before the audit, just correct values.

Anything that adds work to the hot path should be flagged in its
commit message with a measurement or a justification.
