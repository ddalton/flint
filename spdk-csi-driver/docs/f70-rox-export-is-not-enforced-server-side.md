# F70 — a ROX export is read-only only because the CLIENT was asked nicely; the NFS server never enforces it

Status: **FIXED SERVER-SIDE 2026-09-28, unit-tested; the cluster test
(`rox-multi-pod` step 08) has NOT yet been run against the fixed server.
2026-09-29: the export's `access:` list is enforced too (per-network
`ro`/`rw`, unlisted peers refused), the client reads the access mode, and
both charts grew a `readOnly` knob — see the end.**
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
  arrives from outside. *(As found. Since 2026-09-29 it does read the access
  mode — `mount_opts::publish_is_read_only` — see the end.)*

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
run this suite (see F71's run record). (2) ~~`NodePublishVolume` still does
not read the access mode~~ — **it does since 2026-09-29** (below); the
client-side `ro` is now kubelet's bit OR a reader-only access mode. (3) ~~The MDS's
`ExportConfig.options` is still never read~~ — **wired the same day**:
`ExportConfig::read_only()` reads `options` exports(5)-style (`ro` ⇒
read-only; `rw` or nothing ⇒ read-write; the knfsd options every shipped
config carries, `sync`/`no_subtree_check`, stay ignored; `ro`+`rw` on one
export refuses startup, like standalone+dataServers) and `MetadataServer::new`
hands it to `with_read_only`. Tests: the reader's table
(`export_options_ro_is_read_only_and_ro_plus_rw_is_refused`); a standalone MDS
built from a real YAML config with `[ro, sync, no_subtree_check]` answers ROFS
to REMOVE and the file stays, and the control `[rw, sync, no_subtree_check]`
(what the chart, the lite operator and every lima config render) performs it
(`ro_in_export_options_makes_the_mds_read_only_and_rw_does_not`); `[ro, rw]`
refuses to construct (`ro_and_rw_together_refuse_to_start`). **`FlintShare`
gained `spec.readOnly` the same day** (CRD schema version 7): the lite
operator renders `options: [ro, sync, no_subtree_check]` for it, the render
test parses the result back through the server's own reader, and flipping the
field rolls the hub (the line is in the rollout checksum). ~~The pNFS chart's
MDS template still hard-codes `rw`. `access[].permissions` is rendered `ro`
too but is still unread by the server.~~ Both closed 2026-09-29, below.

## The follow-on (2026-09-29): `access[]`, the access mode, the charts

**The export's `access:` list is enforced** (`nfs::export_access`, new).
Every config carried `access: [{network: 0.0.0.0/0, permissions: rw}]` and
the example config a `10.0.0.0/8 rw` list, and the server read none of it —
the same class as F70. Now `ExportConfig::access_policy()` parses each entry
(a CIDR or bare address; `ro` or `rw`; anything else refuses startup naming
the export and the entry) and `MetadataServer::new` hands the list to
`CompoundDispatcher::with_export_access`. The decision is made ONCE PER
CONNECTION, in the shared connection handler (`server_v4::handle_tcp_connection`,
which both the MDS and the per-volume server use), from the peer address the
listener accepted, and rides in `CompoundContext::peer` for every COMPOUND
on that connection, AUTH_SYS and RPCSEC_GSS alike:

- the MOST SPECIFIC network containing the peer decides (longest prefix; a
  tie goes to the first listed), as knfsd prefers a host entry over a
  network entry;
- `permissions: ro` ⇒ that connection is read-only: the F70 refusal list
  answers `NFS4ERR_ROFS` and ACCESS grants no write bits, exactly as the
  export-wide `ro` does for everyone. `options: [ro]` folds in on top;
- a peer matching NO entry of a non-empty list has no access: PUTROOTFH,
  PUTPUBFH and PUTFH answer `NFS4ERR_ACCESS`, so its mount fails with EACCES
  and nothing downstream can reach a file. Session ops (EXCHANGE_ID, ...)
  are still served — the session is the server's, the export is what is
  refused, as with knfsd;
- an empty or absent list restricts nothing (the previous behaviour);
- a `/0` network (`0.0.0.0/0`, `::/0`) matches every peer of EITHER IP
  family, so the shipped catch-all does not lock the IPv6 pods of a
  dual-stack cluster out; a longer prefix matches its own family, with an
  IPv4-mapped IPv6 peer unmapped first.

Tests: the decision table (`nfs::export_access::tests`); the config reader
(`export_access_permissions_are_read_per_network_and_bad_entries_are_refused`);
the dispatcher, through the per-connection entry point
(`a_peer_in_a_read_only_network_gets_rofs_and_one_in_a_read_write_network_does_not`,
`an_ro_export_overrides_a_read_write_network`,
`a_peer_outside_every_access_network_is_refused_at_the_filehandle`,
`a_read_only_peer_is_granted_no_write_bits_by_access`); the MDS built from a
real YAML config (`access_permissions_reach_the_dispatcher_of_a_constructed_mds`,
`an_access_entry_with_unknown_permissions_refuses_to_start`); and the WIRING,
over a real loopback TCP connection through the production handler
(`server_v4::export_access_wiring_tests`: 127.0.0.1 against `10.0.0.0/8` is
refused at PUTROOTFH with ACCESS and against `127.0.0.0/8` served; a
`127.0.0.1/32 ro` entry answers ROFS to REMOVE and the file stays, `rw`
performs it). The proxy's CIDR matcher moved into the new module so the two
lists are matched by one implementation.

**`NodePublishVolume` reads the access mode.** `mount_opts::publish_is_read_only`
is kubelet's bit OR a reader-only access mode (`MULTI_NODE_READER_ONLY`,
`SINGLE_NODE_READER_ONLY` — modes the CSI spec defines as "can only be
published as readonly"; kubelet sends the PV's first access mode with every
publish, so a `ReadOnlyMany` PV whose pod forgot `readOnly: true` arrives as
one). Better than the bit alone: `open(2)` for write fails with EROFS on the
client, and the client never sends the write. It is forced, not refused — a
pod that reads a ROX PVC without saying `readOnly` is an ordinary manifest.
Tested (`a_reader_only_access_mode_or_kubelets_bit_makes_the_publish_read_only`);
the one-line wiring in `main.rs` is read, not unit-tested (`main.rs` has no
tests). NOTE THE COST: the F71 pod now fails on the client, so `rox-multi-pod`
step 07/08 can no longer observe the server fence through a CSI mount; a
cluster leg for the server needs a direct `mount -t nfs4 -o rw` from a
privileged pod (not written).

**The charts.** `pnfs.server.readOnly` (flint-csi-driver-chart) and
`readOnly` (flint-lite-chart) render `options: [ro, sync, no_subtree_check]`
and `permissions: ro`; rendered both ways with `helm template`. The lite
chart's config is in `checksum/config`, so it rolls the hub; the pNFS chart
has no config checksum, so the MDS pods must be restarted (as for every knob
in that chart today — said in `values.yaml`). The DS template's export block
(`pnfs-ds.yaml`, `options: [rw, sync]`) is left as is: the DS never reads
its `exports` (its config is bdevs, bind and MDS endpoints), and the MDS is
the layout authority — an `ro` MDS grants no read-write layout.

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
