# SPDK v26.05 to v26.09: upgrade assessment and replica catch-up review

Date: 2026-09-27. Base today: SPDK v26.05 plus six patches (`docker/Dockerfile.spdk`).
Candidate: SPDK v26.09 (tag `0bbb7fe4d`, 347 commits after v26.05, same DPDK submodule).

Files in this directory:

| File | What it is |
|---|---|
| `raid-skip-rebuild-2609.patch` | The raid skip_rebuild + leased quiesce patch ported to v26.09, with the lease-id fix of 6.2. Replaces `raid-skip-rebuild.patch` when the base moves. |
| `write-cache-advertise.patch` | New, v26.09-motivated. Makes lvol and raid bdevs report a volatile write cache so hosts keep sending FLUSH (section 4.3). |
| `nvmf-hostlog.patch` | The existing hostlog patch plus a one-line stub in `test/unit/lib/nvmf/ctrlr.c/ctrlr_ut.c`. Applies to v26.05 and v26.09. Without the stub the nvmf controller unit test does not link on either version; the shipped Dockerfile never noticed because it passes `--disable-unit-tests`. Supersedes the copy in the crate root. |
| `Dockerfile.spdk-2609` | `docker/Dockerfile.spdk` with the four changes an upgrade needs (section 7). |
| `Dockerfile.spdk-2609-unittests` | Same tree and patch set built with SPDK unit tests enabled; runs the suites that cover the patched code. |

Nothing here is wired into the build. `docker/Dockerfile.spdk` still builds v26.05.

## 1. Verdict

- **The upgrade is mechanically feasible.** Five of the six patches apply to v26.09 unchanged. The sixth, `raid-skip-rebuild.patch`, fails only because v26.09 moved the JSON-RPC schema from `schema/schema.json` to `schema/schema.yaml` and now autogenerates every RPC decoder from it. The ported patch in this directory passes SPDK's generator lints (`scripts/genrpc.py`) and compiles (section 8).
- **Nothing in v26.09 replaces a flint patch.** Upstream still has no assume-clean add, no raid quiesce RPC and no grow RPC. The lvol FLUSH handler, the batched recovery scan and the three logging patches remain flint-only.
- **One v26.09 change fixes a hole under our patch.** In v26.05 a bdev quiesce did not gate FLUSH, COMPARE or SEEK; v26.09 does (commit `73b7888d9`). Our leased raid quiesce therefore gates strictly more on v26.09.
- **One v26.09 change breaks a durability property flint relies on unless we patch for it.** The nvmf target now advertises the NVMe Volatile Write Cache only when a namespace's bdev reports `write_cache`. Raid and lvol bdevs never do. Without `write-cache-advertise.patch`, a kernel initiator on an nvmf export of a raid or lvol stops sending FLUSH, and the lvol FLUSH-to-sync_md handler (the F5 fix) is never invoked on that path. Details in 4.3.
- **One v26.09 change costs read latency for every host that does not opt in.** The TCP target now sets the C2H_SUCCESS flag only for hosts that disabled SQ flow control at connect. Neither the kernel initiator (default) nor SPDK's `bdev_nvme` (default) does. Both can opt in with one flag each. Details in 5.1.
- **The catch-up protocol had a hole independent of the SPDK version, now closed.** The lease "renew" RPC was the same call as "acquire" and answered `true` in both cases, so a lease that lapsed mid-window was silently re-acquired and the `skip_rebuild` add then passed the held-lease check against a base that missed writes. Fixed 2026-09-27 with lease ids in both patch variants and in `hot_rejoin.rs`. Details in 6.2.

## 2. What was checked, and how

- `git apply --check` of each patch against a clean `v26.09` worktree.
- `git diff v26.05..v26.09` and `git log` over every subsystem a patch touches (`module/bdev/raid`, `lib/blob`, `module/bdev/lvol`, `lib/lvol`, `lib/nvmf`, `lib/ublk`, `lib/bdev`, `module/bdev/nvme`), plus the v26.09 CHANGELOG section.
- A census of every SPDK RPC method and parameter name the Rust driver sends (grep over `src/`), compared with the v26.09 `schema.yaml` parameter lists, and of every JSON response key the driver parses, compared with the `spdk_json_write_named_*` diff between the tags.
- The ported schema run through `scripts/genrpc.py --rpcs` and `--doc` (both pass; the generator lints the C registrations, the Python CLI and the docs template).
- A full image build of `Dockerfile.spdk-2609` and a unit-test build on the Linux build box (`ddalton@10.0.0.249`), results in section 8.
- Two read-only audits of the as-built catch-up code (`catchup.rs`, `hot_rejoin.rs`, `replica_sync.rs`, `replica_replace.rs`, `snapshot/multi_replica.rs`, `leg_size_guard.rs`) and of the design docs, cross-checked against each other and against the SPDK source at both tags.

## 3. Patch portability

| Patch | On v26.09 | Notes |
|---|---|---|
| `nvmf-hostlog.patch` | applies | `lib/nvmf/ctrlr.c` changed a lot (735 lines) but not around this log line. Its call to `spdk_nvme_transport_id_trtype_str` breaks the `ctrlr_ut` link on both versions; the corrected copy here adds the stub. |
| `lvol-flush-sync.patch` | applies | No functional lvol or blobstore change upstream between the tags. |
| `ublk-debug.patch` | applies | `lib/ublk/ublk.c` unchanged; only `ublk_rpc.c` moved to autogen decoders. |
| `blob-recovery-batched.patch` | applies | Only upstream blob change is `11ebc9f85` (default `md_page_size` before the grow paths), which does not touch the recovery scan. |
| `blob-shutdown-debug.patch` | applies | |
| `raid-skip-rebuild.patch` | **fails** | Hunk against the deleted hand-written `rpc_bdev_raid_add_base_bdev_decoders[]`, and `schema/schema.json` no longer exists. Ported as `raid-skip-rebuild-2609.patch`. |

What the port changes, and only that:

- `schema/schema.yaml` gains the `skip_rebuild` boolean on `bdev_raid_add_base_bdev` and the three methods `bdev_raid_quiesce` (`name`, `lease_ms`), `bdev_raid_quiesce_list`, `bdev_raid_unquiesce` (`name`). The generator emits `struct rpc_bdev_raid_quiesce_ctx { char *name; uint64_t lease_ms; ... }` and the decoder arrays from this.
- `module/bdev/raid/bdev_raid_rpc.c`: the two hand-written decoder arrays for quiesce and unquiesce are dropped (they would now be duplicate definitions and the generator's lint rejects any hand-written decoder). Everything else in the C code is byte-identical to the v26.05 patch: lease struct, pin/unpin, expiry poller, `-EPERM` without a held lease, `-EBUSY` while pinned.
- `doc/jsonrpc.md.jinja2` gains three sections, because the generator refuses a non-private method without a documented example request.
- `module/bdev/raid/bdev_raid.c`, `bdev_raid.h`, `python/spdk/cli/bdev.py`: unchanged from the v26.05 patch, applied at small offsets.

The Rust driver talks JSON-RPC directly and sends the same request bodies on both versions. The second consumer of this patch, `src/pnfs/mds/block_export.rs` (same quiesce, add, unquiesce triple), needs no change either.

## 4. Upstream changes that touch our patches

### 4.1 Quiesce now gates FLUSH, COMPARE, SEEK (`73b7888d9`)

In v26.05 `bdev_io_range_is_locked()` fell through to `return false` for FLUSH, COMPARE, COMPARE_AND_WRITE, SEEK_HOLE, SEEK_DATA, NVME_IOV_MD and ZONE_APPEND, so those types bypassed both LBA range locks and quiesce. v26.09 blocks them. RESET, ABORT and NVME_ADMIN still bypass by design.

For the hot-rejoin window this is strictly better: with `lvol-flush-sync.patch` a FLUSH is a `spdk_blob_sync_md` on a survivor's lvol, and in v26.05 that could run concurrently with the `bdev_lvol_snapshot` cutting E_f on the same lvol. The blobstore serialises persists, so no defect is asserted, but v26.09 removes the race. The point-in-time argument for E_f was already sound on v26.05 because no ungated type mutates data.

Cost: a FLUSH now waits for the lease like a write does. The kernel initiator's fsync latency during a window becomes the window length (measured 150 to 180 ms; 10 s lease worst case). No design doc analyses which IO types the quiesce gates; the docs treat it as "all IO", which was not true on v26.05.

### 4.2 Reset-before-remove workaround reverted (`ef1a810a9`)

v26.05 ships commit `607f67020`: `bdev_raid_remove_base_bdev` and the failure-driven remove both issue `spdk_bdev_reset()` to the base bdev after the raid quiesce and before dropping channels, to avoid a use-after-free caused by FLUSHes that bypassed the quiesce. For a `bdev_nvme` leg that reset is an NVMe-oF controller reset (TCP reconnect); lvol also accepts RESET. v26.09 removes the workaround because 4.1 fixed the root cause. Expect leg removal (`maint_roll.rs`, the F44 cutover detach paths) to be quieter and faster on v26.09. The driver does not depend on the reset (audit, section 6.1).

### 4.3 Volatile Write Cache is now derived from the bdevs (`7daf05559`)

v26.05: `cdata->vwc.present = 1` and `wce = 1` for every nvmf controller, unconditionally. v26.09: `vwc_present = any namespace bdev has write_cache` (an empty subsystem reports true so a cached namespace can still be added), Set Features VWC=0 is rejected as not changeable, and adding a cached namespace to a subsystem that already has controllers and advertises no VWC is refused.

Which bdevs report `write_cache` (unchanged between the tags): `bdev_nvme` when the attached controller advertises VWC; `bdev_aio` and `bdev_malloc` always; **raid hard-codes 0** (`bdev_raid.c:1634`); **lvol never sets it** (calloc'd 0); `bdev_uring` and `bdev_null` 0.

Consequences on v26.09 without a fix, per flint path:

| Path | Effect |
|---|---|
| Kernel `nvme-tcp` attach (`BLOCK_DEVICE_BACKEND=nvme-tcp`, and the pNFS DS raw NVMe/TCP path) exporting a raid or lvol | Kernel sees VWC absent, sets the queue to write-through, never issues FLUSH or FUA. The lvol FLUSH handler is never invoked. |
| ublk attach over the raid | Unaffected: ublk sets `UBLK_ATTR_VOLATILE_CACHE` from `spdk_bdev_io_type_supported(FLUSH)`, and raid1 supports FLUSH when every base does. |
| Replica leg (peer exports an lvol, local `bdev_nvme` attaches it) | Peer subsystem advertises VWC absent, so the local `bdev_nvme` bdev has `write_cache = 0` and completes FLUSH locally without sending it. Today on v26.05 the same FLUSH is already swallowed, because `bdev_nvme_set_options.enable_flush` defaults to `false` and flint never sets it (section 6.3). |

`write-cache-advertise.patch` sets `write_cache = 1` on lvol bdevs (correct with the flush handler present: an lvol does hold volatile state that FLUSH commits) and derives a raid's `write_cache` from its bases at configure time. With it, v26.09 advertises VWC on every flint export exactly as v26.05 did unconditionally. The upstream unit test `bdev_raid_ut.c:774` still passes because its base bdevs report no cache.

What FLUSH-to-sync_md actually protects is narrower than the F5 write-up implies: the blobstore persists a thin cluster allocation (extent page or md sync) before it completes the write that allocated it (`blob_insert_cluster_msg`), so allocations are durable at IO completion regardless of FLUSH. The chart still calls the F5 fixes a hard dependency for ublk mode, so the property should be preserved, and the fix is ten lines.

### 4.4 RPC decoders are generated (`9c06b926d`, `59392d738`, YAML schema)

Behaviour-preserving for callers: the generated decoders carry the same names, types and required flags as the hand-written ones. The driver's parameter census against the v26.09 schema found every name it sends still present: `nvmf_create_subsystem`, `nvmf_subsystem_add_ns` (`bdev_name`, `uuid`, `nguid`, `ptpl_file`, `nsid`), `nvmf_subsystem_add_listener`, `bdev_nvme_attach_controller`, `bdev_nvme_set_options` (timeouts only), `ublk_*`, `bdev_lvol_*` including `clear_method`, `bdev_raid_create` (`superblock`), `bdev_raid_delete` (`clear_sb`), `bdev_uring_create`. None of the parameters v26.09 removed (`io_unit_size`, `num_shared_buffers`, `buf_cache_size`, `hide_metadata`, `max_discard_size_kib`, `discovery_filter`, `bdev_nvme_set_multipath_policy`) appear anywhere in `src/`, the chart or the Dockerfile. Response keys the driver parses (`bdev_raid_get_bdevs`, `bdev_get_bdevs`, `bdev_lvol_get_lvols`, `nvmf_get_subsystems`, `nvmf_subsystem_get_controllers`, `bdev_nvme_get_controllers`, `ublk_get_disks`) are unchanged; `nvmf_get_subsystems` only gains `admin_label`, `nguid`, `eui64`.

Build-time: `scripts/genrpc.py` now imports `yaml`, so the builder image needs `python3-yaml` (not installed today, would fail at `include/Makefile`'s generate step).

## 5. Performance-relevant changes in v26.09

### 5.1 C2H_SUCCESS is negotiated (`00c2f8cd4`, `d6945b55a`)

The TCP target used to set the SUCCESS flag on the last C2HData PDU whenever the transport's `c2h_success` option (default true) allowed it, letting the host complete a READ without waiting for the response capsule. That was out of spec when SQ flow control was enabled, since the host never received the SQ head pointer. v26.09 sets SUCCESS only when the host requested "disable SQ flow control" in the Fabrics Connect command.

- The kernel initiator does not request it by default. flint's `nvme connect` (`node_agent.rs:2103-2119`) does not pass `--disable-sqflow`. On v26.09 every kernel READ (nvme-tcp attach mode, pNFS DS) gets one extra 24-byte CapsuleResp PDU and one more receive wakeup. Mitigation: add `-d` / `--disable-sqflow` to the connect argv (nvme-cli 2.8 has it). Needs an A/B on the read ladder.
- SPDK's `bdev_nvme` host does not request it either. v26.09 adds `disable_sq_flow_control` to `bdev_nvme_attach_controller`; passing `true` for replica-leg attaches keeps the one-PDU read completion on the replication path.

### 5.2 Other data-path items

| Commit | What | Flint relevance |
|---|---|---|
| `253bdda6e` bdev_io pool returns in batches of 64 | Channel teardown takes 4 mempool locks per thread instead of 256 | Faster, less contended `ublk_stop_disk` / raid delete teardown on busy nodes. |
| `b9c1abf42` nvme/tcp polls the sock group even for a single qpair | Forward progress for socket impls that need group polling | Initiator side (`bdev_nvme` legs); latency of remote-leg IO under `uring` sock impl. |
| `f5fa82e34`, `cf5096c28`, `6e2ad75bc`, `4eb8f8170` nvmf/tcp inlining of the PDU read path | Refactor toward a faster TCP target read path | Small CPU per PDU on the target. |
| `a7a00c6ed` bdev/nvme rescans namespaces after a successful reconnect | A namespace resized while the controller was disconnected is reflected in the bdev without unregistering it | Directly useful to online expansion of a replica leg that was disconnected during the resize (F56 / `leg_size_guard.rs` territory). Today a size change made while a leg is down is not seen until re-attach. |
| `f1ed209ce` atomic `ana_state_updating` | Stops a message flood on the app thread when a burst of IO fails with ANA errors | Fewer stalls during a peer failover. |
| `0e34de4f8` unregister AER callbacks before depopulate; `369865e14` subsystem UAF in ctrlr destruct; `70f7d5ea7` AER `mgmt_io_outstanding` leak on an inactive qpair | Crash and hang fixes in exactly the teardown paths flint exercises (delete subsystem while controllers exist, pause subsystem) | The AER leak "can block subsystem pause"; flint pauses subsystems on `nvmf_subsystem_add_ns`/`remove_ns`. |
| `f79e44c3d` double free between range unlock and unregister | Unquiesce racing an unregister could free the bdev name twice | The hot-rejoin unwind does unquiesce followed by raid delete. |
| `be17ba145` log buffer for long messages | | The hostlog patch prints long lines. |

Not relevant to flint: KV namespaces, DIF moved to the bdev layer (no DIF), Extended Discovery Log Page, `ns_data_alloc_mode` (saves about 3.6 KB per attached namespace), RDMA interrupt mode and UMR (flint uses TCP), `--max-nvmf-sgl-entries` (RDMA), `--with-vmd` option (default unchanged), sock API refactor (flint does not link SPDK).

DPDK is the same submodule commit at both tags, so the `--target-arch=corei7` floor and the EAL behaviour are unchanged. ISA-L moved to v2.32.1 and ISA-L Crypto to v2.26.1 (nasm on Ubuntu 24.04 is fine).

## 6. The replica catch-up logic as built

Full audit with line citations lives in the session transcript; the durable summary:

### 6.1 Shape

- State of record is the PV annotation `disk.chert.us/replica-sync-state` (`in_sync | stale | standby`, per-replica `active_lvol_uuid`, `reverted_to`, `hot_rejoin` marker). The raid superblock is never used (`superblock: false`).
- The driver **never starts SPDK's background rebuild**. The only plain `bdev_raid_add_base_bdev` calls are dead code (`controller_operator.rs:369`, `raid/raid_service.rs:129`). Admission is either:
  - at NodeStage, by listing the equalised head in `bdev_raid_create` (SPDK treats every create-time base as in sync), after a final common epoch cut with no writer alive and a base-inclusive shallow-copy replay to it; or
  - live into an ONLINE raid through the patch: `bdev_raid_quiesce` (10 s lease), strict E_f snapshot on every in-sync survivor, either an esnap clone of E_f on the returning node (`bdev_lvol_clone_bdev`) or an inline shallow-copy of the delta into the live leg (delta up to 64 MiB, bounded to half the lease), one renew, `bdev_raid_add_base_bdev {skip_rebuild: true}`, `bdev_raid_unquiesce`, then background localisation and `bdev_lvol_set_parent`.
- The copy engine everywhere is `bdev_lvol_start_shallow_copy` from a snapshot on the source node into the `nvme_<nqn>n1` bdev of the destination's fenced export, polled with `bdev_lvol_check_shallow_copy` (`state`, `copied_clusters`, `error`), stall-detected at 600 s.
- The exported block device is the raid bdev (ublk, or nvmf for the nvme-tcp backend). Replica legs are exported as lvols.
- The driver depends on: the raid quiesce draining all writes (true on both versions), `bdev_raid_get_bdevs` fields `state`, `base_bdevs_list[].{name,uuid,is_configured}`, the nvmf namespace inheriting the backing bdev's UUID, shallow copy copying only the source's own allocated clusters at identical offsets, and a short leg being refused by the add. None of these changed in v26.09.

### 6.2 Finding, FIXED: a lapsed lease was re-acquired silently (patch protocol, both versions)

`rpc_bdev_raid_quiesce` finds an existing lease and renews it, or creates a new one; both paths answer `true`. The window issues exactly two quiesce calls: acquire (W1) and renew immediately before the add (W6, `hot_rejoin.rs:946-953`). If W2 to W5 take longer than `lease_ms` (three AER waits of up to 3 s each plus RPC latency can approach it), the poller releases the quiesce, guest writes resume on the survivors, W6 creates a fresh lease, and W7's held-lease check passes against a head or leg that missed those writes. That is the silent divergence the patch comment warns about, and the spike doc's claim that "the single window is checked, not a convention" holds only while the lease has not lapsed. Nothing measures elapsed time since W1, and `bdev_raid_quiesce_list` (which exposes `poller_armed`, `pin_count`) is never called. The unit fake returns `true` from both RPCs and ignores lease state, so no test can catch it.

Why testing never saw it: the drills measured windows of 148 to 176 ms against a 10 s lease, and the unit fake answered `true` to every quiesce with no lease state at all. The pNFS block export (`src/pnfs/mds/block_export.rs:1934-1947`) reasons about exactly this case and aborts on its own clock when the copy used more than three quarters of the lease; `hot_rejoin.rs` had no such guard.

**The fix (2026-09-27), enforced by the target rather than by a clock:**

- `bdev_raid_quiesce` answers `{"lease_id": N}` (monotonic per target) instead of `true`, for an acquire and for a renew. With `lease_id` in the request the call is renew-only: `-ENOENT` if that lease lapsed, `-ESTALE` (code -116) if a different lease is held. A bare call still acquires-or-renews, so the Python CLI and the pNFS caller keep working.
- `bdev_raid_add_base_bdev` with `skip_rebuild` accepts `lease_id` and refuses with `-ESTALE` unless the held lease is that one. Without `lease_id` it admits as before and logs a warning.
- `bdev_raid_unquiesce` accepts `lease_id` and refuses with `-ESTALE` to release a lease that is not the caller's. `bdev_raid_quiesce_list` shows `lease_id`; the ARMED and RENEW log lines carry `id N`.
- `hot_rejoin.rs`: both windows record the id from W1 and name it on the renew, the add, the release and the unwind's release; `-ESTALE` on a release is treated like `-ENOENT` (ours is already gone). A target that answers `true` (patch predating lease ids) runs the old protocol and logs a warning once. The unit fake now models lease state; five new tests pin the id threading, the lapse, a successor's lease, the legacy target, and the inline window.

Both patch variants carry the change: `spdk-csi-driver/raid-skip-rebuild.patch` (v26.05, the shipped recipe) and `raid-skip-rebuild-2609.patch` here. Evidence in 8.4. `src/pnfs/mds/block_export.rs` adopted the same contract: the caller acquires and releases, the window names the lease on its renew and its add, its clock guard stays as an early abort, and its fake target models lease ids (two new tests: the lapse, and a target without ids).

### 6.3 Finding: FLUSH never reaches a remote leg today

`bdev_nvme` completes FLUSH locally unless both `bdev->write_cache` and `g_opts.enable_flush` are set (`bdev_nvme.c:3442`). `enable_flush` defaults to `false` and flint's `bdev_nvme_set_options` call sets only timeouts. So on v26.05 the raid1's FLUSH fan-out reaches the local lvol and is dropped for every remote leg; the peer's lvol FLUSH handler runs only for its own local raid. Whether that matters is bounded by 4.3's last paragraph, but it is not the behaviour the F5 narrative describes. To make it real: pass `enable_flush: true` in `bdev_nvme_set_options`, and on v26.09 also ship `write-cache-advertise.patch` so the peer advertises VWC.

### 6.4 Smaller findings

- `catchup.rs:1465-1476` attaches the copy controller without the P4 dead-target transport bounds that every hot-rejoin attach applies (`hot_rejoin.rs:231-233`).
- The `-EPERM` from a `skip_rebuild` add without a held lease is not special-cased by the driver; it is treated like any other add failure (unwind, 300 s backoff), which is safe.
- The `-EBUSY` retry on the add (`hot_rejoin.rs:959-968`) attributes EBUSY to a releasing lease, but the patch's lease logic never returns EBUSY from the add path; only `raid_bdev_add_base_bdev` itself can.

## 7. What an upgrade to v26.09 needs

1. `docker/Dockerfile.spdk`: `git checkout v26.09`; add `python3-yaml` to the builder's apt list; replace `raid-skip-rebuild.patch` with `raid-skip-rebuild-2609.patch`; add `write-cache-advertise.patch`; take the corrected `nvmf-hostlog.patch` from here (test-only change, same runtime binary); update the two "v26.05" strings. `Dockerfile.spdk-2609` here is exactly that.
2. Driver: add `disable_sq_flow_control: true` to replica-leg `bdev_nvme_attach_controller` calls and `-d` to the kernel `nvme connect`, then A/B the read ladder against v26.05 (5.1). Consider `enable_flush: true` (6.3).
3. 6.2 is fixed in both patch variants, in `hot_rejoin.rs` and in `block_export.rs`. The v26.05 image was rebuilt and published as `dilipdalton/spdk-tgt:1.7.0` (2026-09-27, `scripts/release.sh images` on the x86 box via `DOCKER_HOST=ssh://`, digest `sha256:f87c05170ebb…`), and `flint-csi-driver-chart/values.yaml` pins it. Against a `1.6.x` target both callers run the old protocol and warn once.
4. Live checks the unit tests cannot give: on a v26.09 node, `cat /sys/block/nvmeXnY/queue/write_cache` must read `write back` for an nvme-tcp attach of a raid; `ublk_get_disks` / kernel `queue/write_cache` for the ublk path; a hot-rejoin drill (`docs/tier2-operator-runbook.md` section 9 scrub) to confirm E_f and the admitted leg are bit-identical; the F5 dirty-restart drill (`docs/attach-detach-campaign-2026-07.md`) since the recovery scan patch is unchanged but the load path around it moved.
5. Re-run the adversarial set `docs/incremental-replica-rebuild.md` section 9-8 asks for "per SPDK bump, behaviourally".

## 8. Build and unit-test evidence

All on `ddalton@10.0.0.249` (Ubuntu, kernel 6.12, Docker 29.8 as a snap, so the build context must live under `$HOME`, not `/mnt/nvme`). Context and logs: `~/spdk-2609-port/`.

### 8.1 Image build (`Dockerfile.spdk-2609`)

`docker build -f Dockerfile.spdk -t spdk-tgt:2609-port .` exited 0 with all seven patches applied by `patch -p1` inside the build, three times: once per revision of the patch set (the two later revisions only added unit-test stubs, so the runtime binary is unchanged). The final run used exactly the files in this directory.

### 8.2 Smoke test of the built image

`spdk_tgt --no-huge --no-pci -s 1024` inside the image with three 16 MiB malloc bdevs, driven with the image's own `rpc.py`:

| Step | Result |
|---|---|
| `spdk_get_version` | `SPDK v26.09 git sha1 0bbb7fe4d` |
| `bdev_raid_create -n r1 -r 1 -b "m0 m1"`, then `bdev_raid_remove_base_bdev m1` | `online`, slot freed |
| `bdev_raid_add_base_bdev r1 m2 --skip-rebuild` with no lease | refused, code -1: `skip_rebuild add requires a held bdev_raid_quiesce lease on r1` |
| `bdev_raid_unquiesce r1` with no lease | refused, code -2: `no quiesce lease held on raid bdev r1` |
| `bdev_raid_quiesce r1 --lease-ms 3000`, `bdev_raid_quiesce_list` | lease armed: `poller_armed: true, pin_count: 0` |
| renew, then `bdev_raid_add_base_bdev r1 m2 --skip-rebuild` | `online [m0, m2]`, no `process`; log: `Admitted in-sync base bdev m2 to raid bdev r1 (rebuild skipped)` |
| `bdev_raid_unquiesce r1` | lease list empty |
| `bdev_raid_quiesce r1 --lease-ms 1500`, wait 2.5 s | log: `EXPIRED on 'r1' - auto-unquiescing`, `RELEASED 'r1' after expiry`; list empty |
| `bdev_raid_quiesce r1` right after that expiry | answers `true` and arms a **new** lease (the 6.2 ambiguity, observed) |
| remove m2, plain `bdev_raid_add_base_bdev r1 m2` | log: `Started rebuild on raid bdev r1` then `Finished rebuild`; stock path intact |

### 8.3 Unit tests (`Dockerfile.spdk-2609-unittests`)

First run: `bdev_raid_ut` failed to link with `undefined reference to spdk_bdev_has_write_cache`, introduced by `write-cache-advertise.patch` (the raid unit test compiles `bdev_raid.c` against stubs). Fixed by adding the stub to `test/unit/lib/bdev/raid/bdev_raid.c/bdev_raid_ut.c` in that patch; the ported raid patch itself linked.

Second run: every listed suite linked (`bdev_raid_ut`, `raid1_ut`, `vbdev_lvol_ut`, `lvol_ut`, `blob_ut`, `bdev_ut`, `subsystem_ut`) but `ctrlr_ut` failed to link with `undefined reference to spdk_nvme_transport_id_trtype_str`, a symbol our v26.05 `nvmf-hostlog.patch` introduced into `lib/nvmf/ctrlr.c`. Pre-existing on v26.05, invisible because the image build disables unit tests. Fixed with a stub in `ctrlr_ut.c` (the corrected `nvmf-hostlog.patch` here applies to both tags). The tests themselves did not run in this attempt because `make` stopped first.

Third run, with the files exactly as they are in this directory: build exit 0, every suite ran, `ALL LISTED UNIT TESTS PASSED`.

| Suite | Asserts run | Failed |
|---|---|---|
| `bdev_raid_ut` | 6601 | 0 |
| `raid1_ut` | 4374 | 0 |
| `vbdev_lvol_ut` | 770 | 0 |
| `lvol_ut` | 1505 | 0 |
| `blob_ut` (includes every `blob_dirty_shutdown` case the recovery patch is gated on) | 206448 | 0 |
| `bdev_ut` (includes the new quiesce range-lock cases) | 4949 | 0 |
| `subsystem_ut` (includes the VWC refresh cases) | 1294 | 0 |
| `ctrlr_ut` | 2250 | 0 |

### 8.4 Lease-id fix (6.2): builds and contract check

Images rebuilt with the lease-id patches: `spdk-tgt:2605-lease` from the shipped `docker/Dockerfile.spdk` with the updated crate-root `raid-skip-rebuild.patch` (exit 0), and `spdk-tgt:2609-port` from `Dockerfile.spdk-2609` with the updated `raid-skip-rebuild-2609.patch` (exit 0). Both ran the same contract check inside the image (`spdk_tgt --no-huge --no-pci -s 1024`, three 16 MiB malloc bdevs, raid1 with one free slot), with identical results:

| Step | Result (both images) |
|---|---|
| `bdev_raid_quiesce r1 --lease-ms 3000` | `{"lease_id": 1}`; `bdev_raid_quiesce_list` shows `lease_id: 1` |
| renew with `--lease-id 99` | -116 `quiesce lease on raid bdev r1 is 1, not 99: the caller's lease lapsed and was re-acquired` |
| `bdev_raid_add_base_bdev r1 m2 --skip-rebuild --lease-id 99` | -116 `skip_rebuild add refused: held lease on r1 is 1, the caller's lease 99 lapsed (snapshot window breached)` |
| `bdev_raid_unquiesce r1 --lease-id 99` | -116 `quiesce lease on raid bdev r1 is 1, not the caller's 99` |
| renew and add with `--lease-id 1` | `online [m0, m2]`, no process; log `Admitted in-sync base bdev m2 to raid bdev r1 (rebuild skipped)` |
| `bdev_raid_unquiesce r1 --lease-id 1` | lease list empty |
| acquire 1200 ms (id 2), wait 2.2 s, renew `--lease-id 2` | -2 `no quiesce lease held on raid bdev r1: lease 2 lapsed`; list stays empty, no fresh lease armed |
| bare acquire, bare release (compatibility) | `{"lease_id": 3}`, released |

Unit tests: the v26.09 suite rerun with the lease-id patch passed with the same counts as in 8.3 (`ALL LISTED UNIT TESTS PASSED`, exit 0). Driver: `cargo test --lib hot_rejoin` on the box against HEAD `11b508ef` plus the `hot_rejoin.rs` change:

```
test result: ok. 71 passed; 0 failed; 0 ignored; 0 measured; 2491 filtered out
```

Positive control, so the new tests are known to pin the load-bearing line: with the renew's `renew["lease_id"] = json!(id)` replaced by a no-op in both windows, the four lease tests fail (the lapse test panics at "no add on a lapsed lease": the old protocol re-acquired and admitted); restored, all four pass.

pNFS block export (`cargo test --lib -- pnfs::mds::block_export hot_rejoin::tests`, both suites together):

```
test result: ok. 111 passed; 0 failed; 0 ignored; 0 measured; 2453 filtered out
```

Same control there: with the window's renew changed from `self.quiesce(raid, lease_ms, held.id)` to a bare renew, the lapse test fails with "expected a deferral, got Rebuilt" (the old protocol admitted the leg) and the window test fails on the missing `lease_id`; restored, both pass.

### 8.5 Published image

`scripts/release.sh check` reported only `dilipdalton/spdk-tgt:1.7.0` missing (the chart pin was bumped first), and `scripts/release.sh images` built and pushed exactly that one, from `spdk-csi-driver/docker/Dockerfile.spdk` with the crate-root patches, on the box's daemon over `DOCKER_HOST=ssh://ddalton@10.0.0.249` with the Mac's Docker Hub credentials (the box has none). Every layer was a cache hit, so the pushed image id `7358627b9f19` is the `spdk-tgt:2605-lease` image of 8.4 byte for byte. Registry view (`docker buildx imagetools inspect`): `sha256:f87c05170ebb8037c0f514612e8b419a9ecdb0e5c73e537a2f94dfd3b1712131`, amd64. The image pulled back from Docker Hub onto the box passed the 8.4 contract check again, and the plain add still starts the stock rebuild. After the push the gate lists all four chart images as present. `1.6.2` on Docker Hub (2026-08-02, with a `-skylake-avx512` sibling) was an unpinned A/B build and was left alone.

Artifacts left on the box: images `spdk-tgt:2609-port`, `spdk-ut:2609-port`, `spdk-tgt:2605-lease`; contexts and logs in `~/spdk-2609-port/`, `~/spdk-2605-lease/`, `~/flint-lease-fix/` (driver tree; cargo target at `/mnt/nvme/targets/lease-fix`). Remove with `docker rmi` and `rm -rf` when no longer wanted; the unit-test image and the cargo target are large.

## 9. Design docs that no longer match the code or upstream

Flagged for the owner to update or delete; not edited here.

- `docs/tier2-operator-runbook.md` section 2 says all Tier-1/2 machinery is dark by default. `docs/f50-hotrejoin-window-concurrency.md` section 4 records hot-rejoin and cutover compiled default ON since v1.19.0, and `docs/attach-detach-campaign-2026-07.md` L2274 says orchestrators default on post-v1.16.0.
- `docs/tier2-evaluation-2026-06-12.md` L109 speaks of "the existing five patches"; the Dockerfile carries six. `docs/attach-detach-campaign-2026-07.md` L2218-2227 lists four patches re-applied onto pristine v26.05 and omits `blob-shutdown-debug.patch`.
- `docs/incremental-replica-rebuild.md` L280-285 and `docker/Dockerfile.spdk` L52-54 give `bdev_raid_delete clear_sb` as the reason v26.05 is required. Rev 5 of the same doc (L191-199) made raids `superblock: false`, which removes the need; the requirement is now only hardening for pre-rev-5 lvols. The Dockerfile comment should say so.
- `docs/incremental-replica-rebuild.md` L755-759 and `docs/tier2-evaluation-2026-06-12.md` L72-78: "no grow/assume-clean/quiesce RPC through v26.05". Still true through v26.09; the wording can be bumped.
- `docs/tier2-spike-2026-06-12.md` L90-101: "the single window isn't a convention, it's checked". True only while the lease has not lapsed (6.2).
- No doc records that the quiesce did not gate FLUSH on v26.05 (4.1) or that FLUSH delivery depends on the controller advertising VWC (4.3). Both belong in `docs/incremental-replica-rebuild.md` section 7 or a successor.
- `README.adoc` and `flint-csi-driver-chart/values.yaml:33-34` name v26.05; both change with the tag.
