# F70 — a ROX export is read-only only because the CLIENT was asked nicely; the NFS server never enforces it

Status: **FIXED SERVER-SIDE 2026-09-28, unit-tested; the cluster test
(`rox-multi-pod` step 08) has NOT yet been run against the fixed server.**
Found 2026-09-22 by a code read while answering a scoping question ("can
flint lean run in ROX mode?"), not by any suite — and no suite could have
found it, which is [F71](f71-rox-multi-pod-cannot-fail.md). The sections
below describe the server as it was when found; the fix is at the end.

`disk.csi.chert.us` genuinely supports `ReadOnlyMany`: it is on the
`ValidateVolumeCapabilities` allowlist (`main.rs:2946-2951`) and a ROX claim
is routed to the single-server NFS back end exactly like RWX
(`main.rs:1866-1875`). The controller then tells the server pod it is a
read-only export. **The server is told, and does nothing with it.**

## The chain, every link verified

| # | what happens | where |
| --- | --- | --- |
| 1 | `is_rox` is computed from the capability and passed into the NFS server pod spec | `main.rs:1866-1873`, `main.rs:2651` |
| 2 | ROX pushes `--read-only` onto the server's argv | `rwx_nfs.rs:436-438` |
| 3 | the flag is a real clap arg | `nfs_main.rs:44-46` |
| 4 | it is logged as a startup banner | `nfs_main.rs:117` |
| 5 | it is stored: `read_only: args.read_only` | `nfs_main.rs:196` |
| 6 | it lands in the config struct | `nfs/server_v4.rs:39` |
| 7 | **nothing ever reads it** | — |

Step 7 is the finding. Every occurrence of `read_only` under
`spdk-csi-driver/src/nfs/` is one of three, and none is a read:

```
nfs/server_v4.rs:39     pub read_only: bool,      # the declaration
nfs/server_v4.rs:49     read_only: false,         # Default
nfs/server_v4.rs:1766   read_only: false,         # one test literal
```

(`nfs/v4/operations/ioops.rs:4904` also matches a grep for `read_only`, but
it is the *name of a test function* about delegations —
`read_only_open_grants_no_delegation_by_default` — not a use of this field.)

`NfsServer::new` never consults `config.read_only`, and
`CompoundDispatcher::new(fh_mgr, state_mgr, lock_mgr)` is never told about
it. There is no WRITE/SETATTR/CREATE/REMOVE/RENAME refusal anywhere keyed on
it. **The export is read-write.**

## So what actually makes a ROX volume read-only today

One thing, entirely on the client: a forced `ro` in the mount option string.

```rust
// mount_opts.rs:179-182
let forced: Vec<String> = if readonly { vec!["ro".to_string()] } else { Vec::new() };
```

`ro` is *forced*, not defaulted, so an operator's `mountOptions: ["rw"]`
cannot defeat it — that part is right, and `pnfs_csi.rs:1599-1606` does the
same for the pNFS branch with an explicit comment that a default "would have
silently defeated a read-only publish". The client-side story is careful.

But `readonly` there is `NodePublishVolumeRequest.readonly`, which is
kubelet's to set. The guarantee therefore holds only for a mount kubelet
makes on flint's behalf with that bit set. It does not hold for:

- anything that mounts the export directly (a debug mount, a host mount, a
  pod in another cluster reaching the service, a node with the export
  reachable on 2049);
- a publish where the bit is not set for any reason — and note
  `NodePublishVolume` **never reads the access mode** (`main.rs:4487-5090`
  inspects `req.readonly` and `access_type` only), so the access mode is not
  a second, independent source of the truth. There is exactly one bit, and it
  arrives from outside.

A ROX PV's promise is "many readers, no writers". What is implemented is
"many mounters, each of whom we asked to mount `ro`".

## The bit is carried faithfully right up to the point of use

Worth saying, because it makes the fix smaller than it looks. The role
classifier already models this correctly: `role_from_csi_capabilities` and
`role_from_modes` produce `Role::NfsShared { read_only: true }` for ROX
(`identity.rs:808-830`), it is encoded on the wire as `"nfs-shared-ro"`
(`identity.rs:772`) and stamped into `volume_context` under
`disk.chert.us/role`. The information is present and typed.

It has exactly one consumer in the whole driver, and that consumer throws the
payload away:

```rust
// main.rs:4269  (NodeUnstageVolume)
Ok(role) => matches!(role, spdk_csi_driver::identity::Role::NfsShared { .. }),
```

`grep -rn "NfsShared" --include='*.rs' spdk-csi-driver/src/ | grep -v identity.rs`
returns that single line. Downstream of the classifier, ROX and RWX are
indistinguishable.

## What a fix has to do

Server-side refusal, because that is the only place a guarantee can live that
does not depend on the mounter's cooperation. `NfsConfig.read_only` must
reach the dispatcher and refuse the mutating operations with
`NFS4ERR_ROFS` — WRITE, SETATTR (size), CREATE, REMOVE, RENAME, LINK, OPEN
with a write share, and the pNFS LAYOUTGET/LAYOUTCOMMIT write path. An
export-level check is cheaper and safer than a per-object one.

**Do not fix this without F71 first.** The existing ROX test passes whether
or not the server enforces anything, so a fix landed against it proves
nothing. The order is: make the test able to fail (F71), watch it fail
against today's server, then fix.

## The fix (2026-09-28)

`CompoundDispatcher` gained a `read_only` flag (`with_read_only`,
`is_read_only`; `nfs/v4/dispatcher.rs`). `NfsServer::new` sets it from
`NfsConfig.read_only` — the link that was missing — and logs
`export is READ-ONLY (ROX)` at startup. `CompoundDispatcher::new`'s
signature is unchanged, so the MDS, the file API and the NFS proxy build
read-write dispatchers exactly as before.

**Where the refusal lives.** At the top of `dispatch_operation_inner`,
after the minor-version check and before the operation `match`:
`read_only_refusal(&op)` answers `NFS4ERR_ROFS` for every mutating
operation before any handler, stateid, share reservation, tier mark or
filesystem call runs. The refused op ends the COMPOUND with ROFS as its
top-level status, so nothing behind it executes. The function is a total
match over `Operation`, like `minor_version_2_opcode`, so a new mutating
op added to the enum is a compile error here rather than a hole.

Refused: WRITE, COMMIT, SETATTR (whole — every attribute it sets is on-disk
state), CREATE, REMOVE, RENAME, LINK, OPEN that asks for write access or
would create (`OPEN4_CREATE` in any createmode), LOCK of a write type
(WRITE_LT, WRITEW_LT), ALLOCATE, DEALLOCATE, COPY, CLONE, LAYOUTGET with an
iomode other than READ, LAYOUTCOMMIT. Never refused: every read and lookup,
and every release — CLOSE, OPEN_DOWNGRADE, LOCKU, FREE_STATEID, DELEGRETURN,
LAYOUTRETURN, LOCKT, read-type LOCK, read OPEN — so a client can always let
go of state it holds. ACCESS still reports the write bits as *supported*
but never *grants* MODIFY, EXTEND or DELETE, the way knfsd answers on an
`ro` export; Linux consults ACCESS before OPEN, so `open(2)` for write
returns EROFS on the client.

**Tests** (all in `#[cfg(test)]`, run 2026-09-28):

- `a_read_only_export_refuses_every_mutating_op_with_rofs` — the list above
  with null stateids (so the refusal is proven to land BEFORE stateid
  validation), plus the file is unchanged and no directory, rename target or
  link exists afterwards.
- `a_read_write_export_never_answers_rofs_to_the_same_ops` — the control:
  the same operations on the same dispatcher built read-write never answer
  ROFS. Without it a refusal that fired for every export would pass.
- `a_read_only_export_still_serves_reads_and_releases` — ACCESS, LOOKUP,
  GETATTR, a read OPEN, READ (bytes compared), a read LOCK, LOCKU and CLOSE
  all succeed on the read-only export.
- `a_refused_op_ends_the_compound_with_rofs` — through the public session
  path: `SEQUENCE, PUTROOTFH, REMOVE, GETFH` answers ROFS with three
  results; GETFH never ran; the file is still there.
- `a_constructed_read_only_server_refuses_a_write_and_a_read_write_one_does_not`
  (`nfs/server_v4.rs`) — the production wiring on a server built the way
  `main` builds one, both arms, with the victim file's presence following
  the flag. This is the test whose absence was the whole bug.
- Positive controls, run the same day: with the server passing `false`
  instead of its config, only the wiring test fails; with the OPEN(write |
  create) refusal removed, only the refusal-list test fails. Both lines are
  load-bearing and both are pinned. (Trap met on the way: a file restored
  by moving its backup back keeps the backup's OLD mtime, so cargo reused
  the mutated test binary and reported a failure that was not there —
  `touch` the restored sources before the next run.)

**Still owed.** (1) The cluster leg: `rox-multi-pod` steps 07/08 (a pod
that mounts the ROX PVC without `readOnly` and must be refused) have not
been run against the fixed server; the kind tier on the build box cannot
run this suite (see F71's run record). (2) `NodePublishVolume` still does
not read the access mode; the client-side `ro` remains kubelet's bit. That
is now belt to the server's braces rather than the guarantee. (3) The MDS's
`ExportConfig.options` is still never read, so a lite/pNFS export cannot be
declared read-only; `with_read_only` is available to it when that is wanted.

## Scope note — this is not lean

`s3.csi.chert.us`, which serves lean workspaces and passthrough mounts, is
inline-ephemeral only and has no access modes at all; it is unaffected. Its
read-only path is a different and *sounder* mechanism: `Access::Read` is
stamped into the worker's environment as `FLINT_SYNC_ACCESS=read`
(`s3csi/node.rs:1569-1576`) and the syncer itself then refuses to publish
(`lean/syncer/src/sentinel.rs:725-735`) — enforcement at the writer, not at
the mount. It also declines to narrow silently: a read-only ServiceAccount
that asks for read-write is refused outright
(`s3csi/resolve.rs:168-193`), because a plugin-side read-only bind is
remounted `rw` by the container runtime — measured on EC2, access drill run
2, 2026-09-15.
