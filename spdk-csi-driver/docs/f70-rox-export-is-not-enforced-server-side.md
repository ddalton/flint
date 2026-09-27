# F70 — a ROX export is read-only only because the CLIENT was asked nicely; the NFS server never enforces it

Status: **FOUND 2026-09-22 by a code read, NOT FIXED, NOT reproduced on a
rig.** Found while answering a scoping question ("can flint lean run in ROX
mode?"), not by any suite — and no suite could have found it, which is
[F71](f71-rox-multi-pod-cannot-fail.md).

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
