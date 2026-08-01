# Changelog

All notable changes to Flint CSI will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

The public API surface for SemVer purposes is the CSI gRPC verbs, the
StorageClass `parameters` schema, and the `volume_context` key
namespace. Internal Rust types and node-agent HTTP routes are not
covered by the stability guarantee.

## [Unreleased]

The pNFS truncate-correctness release. A truncate applied to the MDS stub
and then fanned out to N data servers leaves a window in which the DSes
still hold bytes past the new EOF; the `truncate_dirty` gate exists to
make that window unobservable, and TLC found that it did not. Closing
F65 took nine further defects, every one of which had been invisible
because the server's own logs reported success — the recall was emitted,
refused on the wire, and scored as an ack.

**No live upgrade path is provided and none is needed: flint is not
deployed anywhere.** The MDS state database schema was collapsed to a
single version with the migration machinery removed; a database written
by an earlier build is refused at open with an actionable error rather
than migrated.

### Added

- **A TLA+ model of the pNFS truncate gate** (`formal/FlintTruncate.tla`,
  seven configs). It carries two theorems and is explicit that only one
  holds: `Inv_ClearImpliesFlushed` (the gate's own claim — whenever the
  mark is absent, no DS holds content past the MDS size) is proven, and
  `Inv_NoStaleServe` is deliberately NOT listed in the shipped config,
  because one residual still defeats it. The counterexample that started
  the wave was three steps from `Init`.
- **A synthetic NFSv4.1/pNFS client** (`tests/k8s/pnfs-drills/synth_client.py`)
  that holds layouts across events the harness schedules, answers
  `CB_LAYOUTRECALL`, and has a `--deaf` mode that accepts callbacks and
  never replies. A real kernel returns its layout ~80 ms after each I/O,
  so the states this release is about are not reachable with one. It is
  explicitly **not** a conformance oracle — pynfs remains that.
- **An XDR callback decoder for the drills** (`cb-decode.py`). The drill
  previously ran `grep -c CB_` over binary XDR, which is structurally
  always zero, so it passed by not looking.
- **Regression coverage for the pNFS READ/WRITE stub guard**, which had
  shipped since 2026-07-06 with nothing in the tree that would go red if
  it were deleted. Both dispositions are now asserted by exact status;
  the `FailFast` arm is reachable only because the test double overrides
  `fallback_io_disposition` directly rather than inheriting the trait
  default, which cannot produce it.
- **`docs/plans/v42-copy-sparse-hardening.md`** — the conformance and
  measurement work this release does *not* do, with the deciding question
  stated plainly: nobody has established whether a Linux client falls
  back cleanly when COPY returns `NFS4ERR_NOTSUPP`, and the READ_PLUS
  precedent does not transfer because its fallback target is mandatory.

### Fixed

- **`tar --sparse` backed up striped files as nothing.** A pNFS file's
  MDS stub is `set_len`-only, so `blocks()` is 0 while the size is real —
  the metadata signature of a fully sparse file — and `FATTR4_SPACE_USED`
  reported that verbatim. Measured on a real Linux client: `tar --sparse`
  of a 24 MiB striped file produced a **10,240-byte archive** and restored
  a file containing **zero** non-zero bytes, exit status 0; `du` said 0.
  A backup that silently contains nothing. The MDS now reports
  `space_used = size` for pinned files — per file, so a genuinely sparse
  never-layouted file still reports its real allocation. `cp
  --sparse=auto/always` was verified unaffected (it reads the data).
- **COPY livelocked a real Linux client, and the server did the work every
  time.** `wr_writeverf` was a hardcoded zero, commented "sync copy:
  unused". Linux (verified on the wire, kernel 6.8) issues COPY and COMMIT
  in ONE compound and compares COPY's verifier against COMMIT's; zeros
  never match, so the client read every successful copy as a server reboot
  and reissued the identical COPY forever. Measured: one 1 MiB
  `copy_file_range()` produced **264,601 COPY RPCs**, each of which the
  server actually performed, and the syscall never returned. COPY now
  reports the same per-lifetime verifier as WRITE and COMMIT; the same
  operation now takes **2 RPCs** and returns. Every reply was
  individually well-formed and said NFS4_OK — only the *relation* between
  the two verifiers was wrong, which is why no single-operation assertion
  could have caught it.
- **COPY silently dropped the tail of its own arguments.** `COPY4args`
  ends with `ca_source_server<netloc4>` and the decoder stopped before it,
  so the array's length word was read as the next opcode. For the ordinary
  empty case that word is 0 — reserved — so the COMPOUND was truncated to
  one operation plus an OP_ILLEGAL. Worse, a **non-empty** list is an
  inter-server copy request, and it was ignored while a **local** copy was
  performed and reported OK. The array is now consumed arm by arm (an
  unknown `netloc_type4` is BADXDR, since an unknown discriminant means an
  unknown width) and a non-empty list returns `NFS4ERR_NOTSUPP`.
- **COPY's reply contradicted itself.** `cr_synchronous` echoed the
  client's *request* rather than what the server did, while
  `wr_callback_id` was encoded as an empty array — so an async request got
  a reply that simultaneously said "this is asynchronous" and "there is
  nothing to wait for". flint emits no CB_OFFLOAD and dispatches neither
  OFFLOAD_STATUS nor OFFLOAD_CANCEL; there has never been an asynchronous
  copy to describe. It now reports TRUE, and the fsync is unconditional,
  which makes the hardcoded `wr_committed = FILE_SYNC4` true by
  construction instead of true only when the client happened to ask.
- **COPY and CLONE accepted ranges that run off the end of the source**
  (RFC 7862 §15.2.3 and §15.13.3: "MUST fail with NFS4ERR_INVAL"), and
  **COPY accepted a source and destination that were the same file** —
  which is not merely non-conforming but corrupting, since the chunk loop
  is a memcpy where a same-file copy needs a memmove. CLONE's rule is
  deliberately weaker, matching the RFC: same file is legal there unless
  the ranges overlap. Comparison is by `(dev, ino)`, not path, because the
  filehandle layer follows a rename-alias table.
- **COPY and CLONE never advanced the destination's change attribute**,
  the very ordering key `change_counter` exists to protect — its module
  doc names "two extends of a COPY burst" as the disease. Mostly masked by
  the ctime floor, except when a prior bump landed in the same clock tick.
- **SEEK reported success for offsets past the end of the file, and could
  never report EOF.** Linux returns ENXIO for two different questions —
  "past EOF" and "no more content of that type" — and RFC 7862 §15.11.3
  gives them opposite answers: the first MUST be `NFS4ERR_NXIO`, the
  second is OK with `sr_eof` TRUE. Both were collapsed into a success.
  `sr_eof` was additionally hardcoded false, so the RFC's own worked
  example (`SEEK 0 CONTENT_HOLE` on a dense file → `eof=1, offset=size`)
  was answered wrongly. An unknown `sa_what` was treated as HOLE and now
  returns `NFS4ERR_UNION_NOTSUPP`. `NxIo` and `UnionNotsupp` had both been
  declared and never used.
- **ALLOCATE/DEALLOCATE offsets above `i64::MAX` reached `fallocate` as
  negative values** — a wire `u64` cast straight to `off_t`. Rejected
  before the cast now. ENOSPC also maps to `NFS4ERR_NOSPC` rather than
  a generic I/O error.
- **Two dead attribute encoders deleted** (493 lines, zero callers) that
  disagreed with the live encoder on `space_used`. Two encoders answering
  one attribute differently is how the next reader gets it wrong.
- **CLONE destroyed the destination before it knew it could clone.** The
  whole-file path opened the destination `.truncate(true)` *before*
  issuing the FICLONE ioctl and returned an error on failure with the
  file already emptied. `mkfs.ext4` is a shipped option and ext4 has no
  reflink, so on ext4 **every** whole-file CLONE emptied the destination
  and rebuilt it non-atomically; if the rebuild then failed (ENOSPC,
  EACCES) the client was told CLONE had failed and the data was gone.
  Now FICLONERANGE, which writes nothing on failure. Live on the
  standalone RWX mount (`vers=4.2`) — nothing to do with pNFS.
- **CLONE read one request two ways.** `count == 0` meant "replace the
  whole destination file" in its `(0,0,0)` branch and "to source EOF,
  leave the tail alone" in its range branch. One path now, one reading.
  The range branch also computed `len() - src_offset` on `u64` with no
  `[profile]` overflow checks in the workspace, so a source offset past
  EOF wrapped to ~16 EiB in release; that is now `NFS4ERR_INVAL`.
  `std::fs::copy` is gone from CLONE entirely — it is whole-file, it
  truncates, and it carries the source's permission bits.
- **NFSv4.2 operations were not gated on the negotiated minor version.**
  The COMPOUND decoder and dispatcher routed COPY, CLONE, ALLOCATE,
  DEALLOCATE, SEEK, READ_PLUS and IO_ADVISE purely by opcode number, and
  the only minor-version check rejected `> 2`. The pNFS MDS mount is
  `minorversion=1`, so its safety was a *client convention* — one
  hand-mount against the MDS Service port reached every 4.2 handler.
  They are now `NFS4ERR_OP_ILLEGAL` outside a 4.2 COMPOUND.
- **COPY, CLONE, ALLOCATE, DEALLOCATE and SEEK ignored striped files.**
  Only READ and WRITE consulted the pNFS stub guard. For a placement-
  pinned file the MDS's local file is a sparse size-only stub, so COPY
  read zeros and reported success, DEALLOCATE punched a hole in a file
  that is already all holes, and SEEK answered "the whole file is one
  hole" — the F15 fake-sparse class. All five now return
  `NFS4ERR_NOTSUPP` for pinned files, per file rather than per role, so
  files that were never layouted stay fully usable on an MDS.
  COPY and CLONE are guarded inside the handler rather than the
  dispatcher because their source is SAVED_FH: a `current_fh`-keyed
  guard structurally cannot see it.
- **F65 — a truncate did not recall held layouts.** The gate is a
  LAYOUTGET-time check, so a layout acquired *before* the truncate walked
  straight past it and the read never reached the MDS at all.
  `note_truncate` now recalls and revokes the file's layouts between
  marking the gate and fanning out.
- **C1 — the callback carried the layout stateid verbatim** where RFC 8881
  §12.5.3 wants `seqid+1`, so a conforming client rejected every recall.
- **C2 — `CB_SEQUENCE` hardcoded slot 0 / seqid 1.** Per-session
  back-channel slot sequencing now holds the lock across the reply await
  (§2.10.6.1).
- **C3 — a refused reply was scored as an ack.** This is why C1 and C2
  could hide: `Ok(_reply) => Acked` discarded the status, so the server
  logged success either way. Replies are classified now, including a
  compound in which `CB_LAYOUTRECALL` never ran.
- **C4 — LAYOUTCOMMIT re-extended the truncated stub.**
- **C5 — one back-channel writer per session against an `nconnect=4`
  mount**, so a session's other bound transports were never tried.
- **C6 — a grant could escape both the gate and the recall.** LAYOUTGET
  reads the gate and publishes the layout with no lock between them, so a
  grant could pass the check, have the mark arm under it, and publish
  after the recall's snapshot. The publish now re-reads the gate and
  revokes what it just inserted.
- **C8 — callbacks were sent with AUTH_NONE and refused at the RPC layer**
  (`reply_stat = MSG_DENIED`, in 419 µs — an active refusal, not a
  timeout). `csa_sec_parms<>` was never decoded; the server now answers
  with the credential the client offered.
- **C9 — the back channel was registered but never announced.** RFC 8881
  §18.36.3 makes `csr_flags` the server's answer to `csa_flags`, and
  Linux sends zero `BIND_CONN_TO_SESSION` on a v4.1 mount — so that
  echoed `CONN_BACK_CHAN` bit *is* the entire handshake. Without it the
  client refuses callbacks one layer below the auth check C8 fixed. The
  flag cannot be set alone: `nfs4_verify_back_channel_attrs` only runs
  when it is set, and the old 1 MB `csr_back_chan_attrs` against Linux's
  `PAGE_SIZE` offer would have failed the mount.
- **R2 — a self-recall stalled the connection read loop** for the full
  callback timeout.
- **R3 — a post-recall LAYOUTRETURN was answered SERVERFAULT**, which
  aborts the compound Linux folds it into and leaks the open behind it.
- **R4 — the truncate-dirty gate did not survive an MDS restart.** It is
  persisted and the retry re-armed on load.
- **A truncate's cost was linear in the number of layout holders.**
  Recalls ran sequentially with a 10 s callback timeout each, so three
  wedged holders cost 30 s of blocked SETATTR. Per-session ordering is
  required (a back channel negotiates `ca_maxrequests=1`); nothing
  required it *across* sessions. Measured 30.43 s → 10.45 s at three deaf
  holders — linear to flat.
- **The MDS-fallback delay ceiling was re-armed by every restart.** The
  gate's age lived in a process-local `Instant`, so an MDS that bounced
  more often than the ceiling could DELAY a fallback client without
  bound — the exact livelock the ceiling exists to prevent.
- **LAYOUTGET and GETDEVICEINFO answered layout types they do not serve.**
  Both decoded type 4 (FFLv4) and replied `NFS4_OK` with a files-layout
  body; GETDEVICEINFO echoed the requested type back over it, so a
  type-4 caller got a structure explicitly labelled FFLv4. Both now return
  `NFS4ERR_UNKNOWN_LAYOUTTYPE`. LAYOUTRETURN stays lenient by design — it
  emits no body, so there is nothing to mislabel.

### Changed

- **The MDS state schema is a single version with no migrations.** Nine
  incremental versions and their stepwise `ALTER` chain are replaced by
  one `CREATE TABLE` batch that already contained every column they
  added. The migration code had zero test coverage — every backend test
  builds a fresh schema — so the one path that would run against real
  state was the only one nobody exercised.
- **The dead FFLv4 layout encoder is deleted** (~440 lines). It had no
  callers, was never advertised, and had never been on a wire, but five
  green unit tests asserted its own output. Two documents told a future
  implementer to "re-enable FFLv4, ~3 days"; both now say it must be
  written fresh, and list what a fresh one must satisfy.

### Known gaps

- **`Inv_NoStaleServe` still does not hold**, for one reason that is not
  F65: revocation is server-side, so it binds only clients the recall
  reaches. A client with no live back channel at all cannot be bound
  however well the callback is encoded. Closing it needs the DS to refuse
  reads past the pending size — a DsControl fence before the `set_len`
  fanout — not the MDS to ask more politely.
- **R1 is a liveness exposure, not a correctness one.** A client recalled
  by a truncate that then parks is refused a new layout for as long as
  the park lasts. Measured on hardware: `STILL TRYLATER after 251.01s —
  never converted to an error`, refuting the audit's predicted
  "90 s DELAY then NFS4ERR_IO" (that ceiling is on the *fallback* path,
  not the LAYOUTGET path — both readings were right about different
  code).
- **The fallback-ceiling and schema changes have no live gate.** F65
  itself was gated end-to-end on hardware with wire proof; these landed
  after that cluster was torn down.

## [1.22.0] - 2026-07-30

The maintenance-and-proof release. A routine `helm upgrade` used to be
able to take a replicated volume fully down with zero real failures;
this release makes the csi-node roll a drained, barriered, node-by-node
operation and turns it on by default. Alongside it, the correctness
argument moved from prose to machine-checked models — TLA+ now covers
the replica lifecycle, claims, snapshots, expansion, the availability
envelope, cutover, the pod layer, and the DaemonSet roll itself, and the
models found real bugs the tests could not (F56, F59, F61, F62).

### Added

- **The maintenance drain roll — DEFAULT ON, and the headline behaviour
  change of this release.** The csi-node DaemonSet now runs
  `updateStrategy: OnDelete` and the controller's maintenance roller
  drives every template change node-by-node: drain the node's serving
  legs out of the raid (the FENCE), delete the pod, and advance only at
  full redundancy judged on raid membership rather than pod readiness
  (the BARRIER), with marks that die with their holder (the LEASE).
  Without it, k8s `RollingUpdate` gates on pod readiness, which knows
  nothing of raid membership — TLC refutes that gate directly
  (`FlintReplicationRollUnfenced.cfg`).

  **Rolls are now partial by design.** A node hosting a serving
  composition is REFUSED rather than rolled, and skipped with an
  operator-facing event; the campaign converges "except for N announced
  refusals". A `helm upgrade` may therefore legitimately leave
  consumer-hosting nodes on the old revision until you relocate the
  consumer and re-run. This is deliberate — a loud incomplete roll
  instead of a silent outage. The LOCAL half (rolling the node a
  consumer sits on) remains design-only; see
  `docs/f62-local-half-outage-and-blind-barrier.md`. Restore the old
  unattended behaviour with `maintenance.drainRoll.enabled: false`.
- **Bounce-free RWX admission (S2).** The hot-rejoin window now runs on
  the live serving raid, replacing the NFS-server bounce with ~228 ms of
  quiesce. Live-gated with the kill switch both ON and OFF.
- **kube-Lease leader election for the orchestrators (P1).** The
  single-orchestrator invariant is mechanical now rather than a
  deployment convention.
- **`StorageId` / `StagedHandle` newtypes (P2).** The identity-domain
  crossing that produced the F44/F45/F46 family is a compile-time
  surface.
- **Bounded dead-target detection (P4).** Closes the TCP-blackhole gap
  behind the 150–177 s RWX stall; measured 36 s after the fix.
- **TLA+ models plus a TLC gate (`scripts/check-tla.sh`, run out of
  band — not wired into CI)** covering the replica lifecycle/writer
  set, claims (the F50/F53 multi-process layer), snapshots at
  block-content level, expansion, the availability envelope, cutover,
  the pod layer, and the DaemonSet roll — plus a deterministic
  crash-sweep sim harness for hot rejoin.
- **`rust-ci` actually runs the driver's test suite.** It had been a
  developer belt that nothing enforced on push.
- **Chaos drills 3.12, 3.13, 3.14 and 2.11.** 3.14 is the maintenance
  roll's live gate; 2.11 is the all-at-once upgrade shape.

### Fixed

- **F62 — a roll could destroy a live raid composition.** Rolling the
  node that hosts a serving composition tore it down while `staged`
  stayed set, so NodeStage was never called again and only the periodic
  strike repair could rebuild it. The roller now models the
  composition's lifetime and refuses the step.
- **F61 — the drain PASS was conflated with the MARK**, so a roll step
  could advance on evidence it had not actually earned.
- **F63 — a refusal hole on `plan_roll`'s marked-node completion path.**
- **F60 — the cutover bounce is belted** by a commit-time preflight,
  a bounce claim with a deadline, and attempt backoff. The model
  refuted the first draft belt as check-then-act.
- **F59 — two rollers could double-drain**; found by the model and the
  fix sharpened by it.
- **F56 — partial expand fan-out crossed with the §5 chase produced a
  permanent size livelock.** Catch-up owns size convergence now.
- **F57 standby replacement**, per-leg maintenance suppression, a
  device high-water floor, and forced-stale guards.
- **F55 — an NFS shutdown could truncate in-flight replies**, which
  clients saw as EIO and postgres as a PANIC. Shutdown is frame-atomic
  and drains in-flight replies before exit.
- **F54 §3 — the prestage consumer is identity-verified pre-connect**,
  so a zombie never rides into the window.
- **Two fail-open paths into a single-survivor direct serve (F36c).**
- **The node-agent's data-path repair is free of the monitor pass
  chain**, and a dead component is now fatal rather than silent.
- **Probe-not-parse everywhere it converges (P3)** — the F48/F54 class
  of "parse SPDK's untextual errors" audited out.

### Notes

- Drill 3.14 passed on runap (4 runs) and again on runar against these
  bits: fence + barrier + lease + the F62 refusal proven on the wire
  under load, zero unfenced degrades, never fewer than one in_sync leg,
  consumer through with zero PANIC/EIO/ESTALE and zero restarts.
- Drill 2.11 (all-at-once: every tgt under the volume killed at once,
  raid host included) passed on two clusters — never a degraded-direct
  serve, never an acked-tail risk, composition rebuilt in 105 s.
- This file skips 1.7.0 through 1.21.0; those releases were tagged and
  published without CHANGELOG entries.

## [1.6.0] - 2026-07-04

### Added

- **Two-altitude topology view.** The dashboard topology page is a real
  data-path graph now (React Flow): the volume altitude draws
  consumer → access device → RAID bdev → members → backing disks, with
  edge encodings for health (color), access path (solid local / dashed
  NVMe-oF), and recovery (animation); a rebuild renders as an animated
  source→target edge with live progress. The cluster altitude lays one
  card per node (disk-state ring, replica/capacity counts) with
  replica-placement links between nodes, drill-through into the volume
  view. Node/edge details, sync state, NQNs, and the RAID/NVMe-oF
  explainers live in an on-demand drawer instead of inline walls of
  text.
- **`identity.rs` owns every derived name** (Phases 0–4 + CI lint):
  lvols, snapshots, epochs, NQNs, lvstores — one constructor set, one
  published contract (`docs/identity-contract.md`), and now the inverse
  parser (`lvol_owner`) that maps any lvol name back to its owning
  volume.
- **NFS server-pod liveness reconciler.** A bare NFS server-pod death
  (node loss, OOM-kill, manual delete) now self-heals: the controller
  reconciles the pod back, republishes the Service endpoint, and
  clients resume in ~30–42 s.
- **Incremental replica-rebuild kuttl suite** joins `make test`
  (isolated run, exercises the epoch/catch-up orchestrators
  end-to-end).
- Dashboard sessions survive a page refresh (token in sessionStorage,
  per-tab; still expires server-side and on backend restart).

### Fixed

- **Every live lvol on a replicated cluster was reported as an
  orphaned "cleanup candidate."** Orphan detection allowlisted lvols
  against the legacy single-replica `lvol-uuid` PV attribute, which
  replica-set PVs don't carry — so replicas, user snapshots, and epoch
  snapshots all showed as deletable orphans, cloned onto every disk of
  their node. Classification now parses each lvol's owner from its
  name via the identity contract (robust to `_hr` recovery renames),
  fills the long-empty `provisioned_volumes` per disk, and attributes
  both provisioned entries and true orphans to the disk whose lvstore
  actually hosts them.
- **RWX client unpublish tore down the NFS server's live export.**
  `ControllerUnpublishVolume` treated every departing non-home node
  as a remote block consumer and removed the volume-level NVMe-oF
  target — but RWX/ROX consumers are NFS clients with no block path,
  and that target is the NFS server's backing export. One client
  finishing was enough to strand the server's initiator in a
  reconnect loop with its journal pinned (unkillable server pod
  until `ctrl_loss_tmo`). Unpublish now classifies shared volumes by
  PV access modes (the ControllerUnpublish side of the 1.4.0
  NodeUnstage fix) and leaves their target alone; `DeleteVolume`
  owns its teardown. RWO fencing semantics are unchanged, including
  when the PV is unreadable.
- **RWX teardown ordering.** `DeleteVolume` tore down the backing
  volume's NVMe-oF targets immediately after *issuing* the NFS server
  pod delete — while the pod, the volume's consumer, was still
  flushing its dirty ext4 journal through those targets. The kernel
  initiator then reconnect-looped against the vanished subsystem with
  the journal pinned in D-state, leaving the pod unkillable until
  `ctrl_loss_tmo` (~10 minutes). Deletion now waits (bounded, 90 s)
  for the pod object to be removed — kubelet's signal that the volume
  was unmounted and flushed — before target teardown proceeds.
- Clients are never bound to a Terminating NFS server pod; dead-NFS
  mountpoint probes are bounded and no longer misread as "not
  mounted"; `NodeGetVolumeStats` filesystem calls are bounded so a
  dead NFS mount cannot starve the node plugin.
- Shared (RWX/ROX) volume expansion is refused loudly instead of
  silently corrupting state (client-side expand cannot reach the
  server's backing filesystem).
- Snapshot timeline shows real creation times (VolumeSnapshotContent /
  sync-record timestamps) on two lanes — user snapshots and engine
  epochs — with CR-path user-snapshot deletion that keeps the CR and
  SPDK content in step; the old always-empty "Topology View" tab and
  its dead-code renderer are gone.
- Disk-delete refusals surface the node agent's actual status and
  reason (e.g. 409 "N logical volumes still exist") instead of a
  generic 502; the snapshot detail modal's disabled "coming soon"
  Delete/Clone buttons are removed.

### Changed

- The legacy `spdk-controller-operator` deployment defaults **off**
  (verified unreachable in the identity audit); remove any explicit
  `spdkOperator.enabled: true` override when upgrading.

## [1.5.0] - 2026-07-03

Dashboard release: the operations dashboard gains structure (URL
routing, a real test safety net, this repo's first CI), a coherent
visual system, and sheds its last fabricated data. No changes to the
public API surface (CSI gRPC verbs, StorageClass parameters,
`volume_context` keys).

### Added

- **Deep-linkable dashboard state.** Tabs, cross-tab filters, and
  volume/snapshot detail selections live in the URL (react-router);
  refresh and back/forward are safe, and any view can be shared as a
  link.
- **Frontend safety net + CI.** 73 Vitest/RTL tests with MSW fixtures
  typed against the generated OpenAPI schema (contract drift is a
  compile error), and two GitHub Actions gates: the dashboard suite
  and OpenAPI-spec freshness in both directions (the Rust structs are
  the schema's sole author).
- **Primitive UI kit with one status vocabulary.** Chip, ProgressBar,
  Card, Skeleton, AsyncView, and ConfirmModal primitives; a single
  status-color vocabulary aliased to semantic Tailwind tokens; errors
  never blank present data (stale banner instead); destructive flows
  gate on a typed phrase. Entry bundle code-split 1013 KB → 296 KB.
- **Node agent `POST /api/disks/delete`** — the strict inverse of
  disk initialize: a no-op on an uninitialized disk, a 409 refusal
  while any logical volume still exists on the store. The dashboard's
  delete proxy is now documented in the OpenAPI spec.
- **Committed end-to-end bulk-init drill**
  (`spdk-dashboard/scripts/bulk-init-drill.mjs`) — Step 0 of the
  remote-builder runbook: every fresh builder's pristine scratch NVMe
  exercises the full select → manifest → confirm → LVS-Ready flow
  against a real agent before being repurposed.

### Fixed

- **Epoch snapshots resolve to their volume.** `epoch-<pv>-<seq>`
  names now parse to their PV (right-anchored; the trailing segment
  must be the numeric sequence), so Tier-2 epoch snapshots no longer
  pile into a single "unknown" bucket in the snapshot tree. Tree
  entries are labeled with the PV name; the backend also re-derives
  ids as a fallback for older agents.
- **Disk lvol counts were always 0** (release-check-found). The SPDK
  lvol counter matched a `lvol_store_name` field that
  `bdev_get_bdevs` does not emit; live stores therefore always
  reported zero lvols — which also meant the new delete endpoint's
  refusal guard could not fire. The counter now matches
  `lvol_store_uuid` and the `<lvs>/<name>` alias, and
  `delete_blobstore` re-counts against fresh SPDK state immediately
  before the destructive RPC instead of trusting a discovery
  snapshot.
- Frontend strictness: zero `any` types; `noUncheckedIndexedAccess`.

### Removed

- **Fabricated dashboard data.** The Remote Storage tab (pure
  client-side mocks; no backend routes ever existed) and the snapshot
  list's invented per-snapshot storage consumption are gone. The
  snapshot tree's real backend analytics (SPDK bdev consumption)
  remain.

### Changed

- The frontend image's `nginx.conf` is the single source of truth;
  the chart no longer overlays it with a ConfigMap.

## [1.4.0] - 2026-07-03

Tier-2 hot rejoin ships: non-disruptive standby admission for
attached RWO volumes. Validated at 2–4 replicas through staggered
multi-failure drills: zero acked-write loss across 145,000+ fsync'd
writes, 5 controller deaths, 12+ leg kills, and one full raid
collapse.

### Added

- **Hot rejoin (Tier-2).** Leased quiesce windows (100–200 ms esnap
  path; O(delta) inline fenced-final-delta path, chosen adaptively by
  a delta estimator), epoch catch-up with coverage-aware source
  selection, esnap localization with local-chain resume, crash-decode
  reconciler (adopt/scrub/resume/demote), defensive unquiesce, and
  per-volume rejoin claims. `spdk-tgt` 1.4.0 = SPDK v26.05 + raid
  skip_rebuild / leased-quiesce patch v3. Operator runbook:
  `docs/tier2-operator-runbook.md`. Drill-only fault knob
  `FLINT_HOT_REJOIN_FAULT` (never set in production).
- **NFSv4 state persistence across server replacement** (`state.db`
  on the export volume) — closes 1.3.0's "dirty open state lost at
  bounce" limitation. Locks remain memory-only.
- Node agents reap dead reconnect-looping NVMe-oF controllers.
- Operations dashboard phases 0–2d: backend-enforced bearer auth,
  TanStack Query data layer + backend aggregate cache, live replica
  sync state, live volume detail, engine event timeline with
  hot-rejoin windows, bulk disk initialization, and OpenAPI-generated
  frontend types.

### Fixed

- **Latent 1.3.0 shared-volume unstage bug (found by this release's
  gate).** NodeUnstage classified NFS consumers by `findmnt` on the
  staging path, but RWX/ROX consumers mount at publish time — so
  every shared-volume consumer unstage ran the block teardown, whose
  per-replica sweep could delete the NFS server's live backing
  exports. Classification now reads the PV's access modes (`findmnt`
  only as a fallback).
- Staggered-failure fixes from the 3-failure drill campaign: chase and
  catch-up sources resolve via the record's live uuid and fail over by
  lineage coverage; E_f cuts on each survivor's live head; the
  localization backfill and phase-4 admission sources are
  coverage-probed; the orphan sweep learned the hot-rejoin name
  shapes; esnap-resume prefers the local chain.

## [1.3.0] - 2026-06-12

Self-healing release: every common single-failure (replica node loss,
consumer spdk-tgt restart, lone container restart, same-node reschedule
race) now heals autonomously, typically within ~3 minutes and without
workload restarts. All changes validated live on AWS i4i clusters with
forced failure injection.

### Added

- **Consumer data-path self-healing (4 layers).** Storage-baseline
  recovery re-adopts disks after a lone `spdk-tgt` restart (~30 s);
  data-path-lost detection flags volumes whose raid vanished under a
  live attachment (3-strike, PV annotation + events); in-place repair
  rebuilds the raid and loopback export with a **pinned NVMe namespace
  identity** so the kernel initiator reattaches without a workload
  restart; and the cutover orchestrator bounces as a last-resort
  fallback. Escape hatch: `FLINT_DATA_PATH_REPAIR=disabled`.
- **Scheduling escalation for cutover bounces.** Every bounce applies a
  self-expiring `NoSchedule` taint (`flint.csi.storage.io/bounce`,
  TTL `FLINT_CUTOVER_TAINT_SECS`, default 120 s) to the bounced node so
  the replacement cannot reuse the stale staged volume — reassembly
  bounces are now deterministic instead of scheduler-dependent.
- **Orphan sweep (§10-14).** Node agents reap lvols and NVMe-oF
  subsystems whose owning PV no longer exists (3-strike confirmation,
  strict parsers, ublk-verified ephemeral handling).
  `FLINT_ORPHAN_SWEEP=disabled` to opt out.
- Dashboard backend `/healthz` endpoint; liveness/readiness probes
  moved off the aggregate `/api/dashboard` endpoint.

### Fixed

- **RWX volume identity aliasing (six fixes).** An RWX volume's three
  identities (user PV, synthetic backing PV, volumeHandle) corrupted
  each other: zombie raids at unstage blocked every later restage; a
  permanent data-path false positive drove endless NFS-pod bounce
  loops; duplicate epoch/catch-up streams broke snapshot lineage and
  standby admission; replica exports were squatted under alias NQNs;
  an RWX consumer's unstage could detach the live raid's legs; and NFS
  server bounces invalidated every client file handle (now pinned per
  volume via `PNFS_INSTANCE_ID`; foreign handles answer `NFS4ERR_STALE`
  so clients recover by re-walking).
- Retention pin lifecycle: held until standby admission (not copy
  completion) and advanced with the standby's chase mark — epoch
  history no longer grows unbounded behind a chasing standby.
- Dashboard: unreachable nodes can no longer hang the aggregate fetch
  past the liveness deadline (bounded per-node timeouts), and the
  frontend no longer substitutes mock data when the backend is
  unreachable — it keeps last-known data and shows an error banner.

### Known limitations

- **RWX cutover transparency requires clean client state.** A client
  holding dirty open state (unsynced writes) across an NFS server
  bounce can have those writes dropped: the server's NFSv4 state is
  in-memory and does not survive pod replacement. Read-mostly and
  fsync-disciplined workloads ride through transparently. Persistent
  state (SQLite backend on the exported volume) is the next milestone.
- Migration from ≤1.2.0: existing volumes cross onto the pinned
  namespace identity at their next detach/restage; existing NFS server
  pods mint stable file-handle ids at their next recreation.

## [1.2.0] - 2026-06-11

- **Incremental replica rebuild** (phases 1–5b) and superblock-less
  raids.
- **Bounded unstage umount** — a wedged NFS mount can no longer hang
  `NodeUnstageVolume` indefinitely.

## [1.1.1] - 2026-06-10

- **NVMe-oF fencing admits the consumer node.**
  `ControllerPublishVolume` whitelisted the controller pod's host NQN
  instead of the consuming node's, so every cross-node single-replica
  attach was fenced out with EIO. (1.1.0 introduced the phase-0
  fencing and was superseded by this tag without a standalone
  release.)

## [1.0.0] - 2026-05-04

First stable release. Production-ready for SPDK-based deployments;
no-SPDK deployments supported with documented feature subsets. From
this release onward, breaking changes to the CSI gRPC surface,
StorageClass parameters, or `volume_context` keys require a `MAJOR`
version bump.

### Storage architecture

- **High-performance local block storage via SPDK userspace I/O.**
  Bypasses the kernel block layer; delivers full NVMe bandwidth from
  a userspace target backed by `ublk` on each worker. Per-worker
  hugepage and disk requirements documented in the README.
- **Multi-replica volumes via NVMe-oF RAID across nodes.** RAID-1
  mirrors and optional RAID-5f, transparent to the NFS protocol layer.
  Survives single-disk and single-node loss without client-visible
  outages beyond the underlying NVMe-oF reconnect window.
- **pNFS data path** (RFC 8881 FILE layout). Parallel-server NFSv4.1
  with stripes across multiple data servers; opt-in via StorageClass
  `parameters.layout: pnfs`. Single-host bench shows ~1.6× write
  throughput over single-server NFS at fsync=1 (ADR 0003); cross-host
  scaling measurable via the included Kubernetes bench harness
  (`make test-pnfs-cross-host`).
- **Volume snapshots and clones** in SPDK mode via `bdev_lvol_snapshot`
  and `bdev_lvol_clone`. Instant copy-on-write; space-efficient.
- **Online volume expansion** without downtime.
- **CSI inline ephemeral volumes** for pod-scoped temporary storage.

### pNFS production hardening

- **Persistent NFSv4.1 / pNFS server state** (`Phase B`). Client IDs,
  sessions, stateids, layouts, and pNFS file handles survive MDS pod
  restarts via a SQLite-backed `StateBackend` (WAL + NORMAL crash-
  safe). Kernel clients reconnecting after a restart resume against
  the same record set with no `STALE_CLIENTID` or `BAD_STATEID` storm.
  Verified end-to-end via `make test-pnfs-restart` with byte-for-byte
  hash matching across restart.
- **DS death recovery** (`Phase A`). Heartbeat monitor detects a dead
  data server, fans out `CB_LAYOUTRECALL` to all affected client
  sessions via the back-channel, and forcibly revokes layouts after
  the RFC 5661 §12.5.5.2 deadline if clients don't return them.
  Verified end-to-end via `make test-pnfs-recall`.
- **NFSv4.1 RFC conformance.** Pynfs full suite: 167 PASS / 4 FAIL /
  91 SKIP (5.8× the original audit baseline of 26 PASS). Six suites
  at 100%, nine more above 70%. The four remaining failures are
  documented niche cases that do not cascade or corrupt data.

### CSI integration

- **StorageClass `parameters.layout: pnfs`** opts a volume into the
  pNFS data path. Default StorageClasses use single-server NFS or
  direct SPDK block per existing chart configuration.
- **`volume_context` namespaces.** Production keys live under
  `flint.csi.storage.io/*` (SPDK mode) and `pnfs.flint.io/*`
  (pNFS mode). These namespaces are stable from 1.0.0; new keys may
  be added in `MINOR` releases, removals or renames require `MAJOR`.
- **VolumeSnapshot CRD preflight.** At controller startup, the driver
  checks for the cluster-wide `VolumeSnapshot{,Class,Content}` CRDs
  and logs a one-line warning with the install command if any are
  missing. Non-fatal: non-snapshot RPCs work without the CRDs.
- **Snapshot guards for unsupported volume types.** `CreateSnapshot`
  and `CreateVolume`-from-snapshot/PVC return `FAILED_PRECONDITION`
  (final, non-retryable per CSI) for pNFS volumes, replacing a prior
  `NOT_FOUND`-induced retry loop in `external-snapshotter`.

### Operations & ergonomics

- **Helm chart** for installation under Kubernetes 1.21+. Optional
  pNFS mode (`pnfs.enabled: true`); SPDK enabled by default.
- **Web dashboard** for disk discovery, initialization, and monitoring.
- **`NOTES.txt`** rendered after `helm install` surfacing the
  `VolumeSnapshot` CRD prerequisite explicitly.
- **Test surface:** 330 Rust unit tests, KUTTL system tests across
  SPDK + pNFS paths, Lima e2e harnesses for pNFS protocol / restart /
  recall flows, and a scaffolded cross-host bench harness.

### Deployment modes

| Mode | Storage backend | Snapshots | Replication | Status |
|---|---|---|---|---|
| Production-SPDK | SPDK blobstore | ✅ Native COW | ✅ NVMe-oF RAID | Recommended |
| Production-no-SPDK (single-server NFS) | Filesystem | ⏸️ Roadmap | ❌ Customer-provided | Supported |
| Production-no-SPDK (pNFS) | Filesystem | ❌ Not supported | ❌ Customer-provided | Supported with limits |
| Dev/QE (Kind/Lima) | Loopback | Optional | None | Dev only |

### Container images

Published to Docker Hub under the `dilipdalton/` namespace for
`linux/amd64`:

```
dilipdalton/flint-csi-driver:1.0.0
dilipdalton/spdk-target:1.0.0
dilipdalton/flint-dashboard:1.0.0
```

Aliases: `:1.0`, `:1`, `:latest`. **Production deployments should pin
to an immutable tag (`:1.0.0`).** The chart's `values.yaml` defaults
to `:latest` for development convenience; production users should set
each `images.<component>.tag` to `"1.0.0"`.

### Known limitations

- **pNFS volumes do not support snapshots in any deployment mode.**
  Snapshot RPCs against pNFS sources return `FAILED_PRECONDITION`.
  Workaround: use a non-pNFS StorageClass for volumes that need
  snapshots, or use SPDK mode for performance + snapshot capability.
- **No-SPDK volumes have no Flint-level replication.** Durability
  comes from the underlying block volume (EBS/PD/Ceph RBD/etc.). For
  cross-node redundancy without external durable storage, use SPDK
  mode with NVMe-oF RAID.
- **`linux/arm64` container images are not published in this release.**
  ARM64 is a planned target; x86-64 ships first to match the primary
  deployment fleet (Cloudera customer infrastructure and current QE/CI).
  ARM64 builds will follow in a subsequent release.
- **`VolumeSnapshot` CRDs are a cluster-wide prerequisite** not
  installed by the Flint chart (cluster-singleton; bundling them
  would conflict with other CSI drivers). Without them, the bundled
  `snapshot-controller` Deployment will `CrashLoopBackOff`. See
  README "Snapshot Prerequisites" for the install command. The Flint
  controller logs a startup warning if missing.
- **pNFS Flex Files (FFL) layout is not implemented and is deferred
  indefinitely.** Replication is handled at the SPDK NVMe-oF RAID
  layer (below the protocol); FFL would duplicate that capability
  with client-side write amplification and a separate rebuild
  scanner. Decision recorded in
  `docs/plans/pnfs-production-readiness.md`.

### Upgrade notes

This is the first tagged release. There are no prior stable versions
to upgrade from. Operators running pre-1.0 builds should reinstall
fresh against `v1.0.0`. The pre-1.0 git history is preserved at the
`archive/config` and `archive/disk_mgmt` tags for forensic reference;
neither tag represents a supported upgrade source.

### Security

No security advisories at this release.

[Unreleased]: https://github.com/ddalton/flint/compare/v1.5.0...HEAD
[1.5.0]: https://github.com/ddalton/flint/compare/v1.4.0...v1.5.0
[1.4.0]: https://github.com/ddalton/flint/compare/v1.3.0...v1.4.0
[1.3.0]: https://github.com/ddalton/flint/compare/v1.2.0...v1.3.0
[1.2.0]: https://github.com/ddalton/flint/compare/v1.1.1...v1.2.0
[1.1.1]: https://github.com/ddalton/flint/compare/v1.0.0...v1.1.1
[1.0.0]: https://github.com/ddalton/flint/releases/tag/v1.0.0
