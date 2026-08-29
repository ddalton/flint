# xattrs and ACLs — a plan, and the preconditions it is gated on

> **Status: design, NO code.** Written 2026-08-28 against `b871595e` + the
> uncommitted tier fixes. Every claim cites the path that carries it.
> These are **two projects**, not one, and only the first is small.

## 0. Where things actually stand

Neither feature exists. There is no `GETXATTR`/`SETXATTR` anywhere in
`src/nfs`; `fileops.rs` answers `FATTR4_ACL` with an **empty ACL** and
refuses `SETATTR` of attr 12 with `ATTRNOTSUPP`.

That is worth stating precisely, because it changes who owns the work:
**these are not tier gaps.** The tier is not losing xattrs on the round
trip — there is no front door for a client to set one through. Both are
NFS *server* features, and the tier only inherits a carrier problem once
the front door exists.

## 1. The two mechanisms, and which one is real

NFSv4 has carried xattr-shaped functionality twice:

- **Named attributes** (`OPENATTR`, RFC 7530 §5.3) — the original 4.0
  design: a hidden per-file directory of attribute *files*, each
  OPEN/READ/WRITE/CLOSEd. **The Linux client never wired this to
  `getxattr(2)`.** Implementing it buys interop with essentially
  nothing.
- **RFC 8276 extended attributes** (NFSv4.2) — `GETXATTR` (72),
  `SETXATTR` (73), `LISTXATTRS` (74), `REMOVEXATTR` (75), gated on the
  server advertising `FATTR4_XATTR_SUPPORT` (82). **This is what Linux
  ≥5.9 actually uses.**

**Decision: implement RFC 8276 only. Do not implement `OPENATTR`.**
Opcodes 72–75 are free in `protocol.rs` (the table stops at `CLONE` =
71) and attr 82 is unassigned there.

## 2. The precondition that gates everything

`FLINT_NFS_ENFORCE_PERMISSIONS` **defaults to warn** — permission checks
are evaluated and logged but never enforced (`tests/lima/pjdfstest-baseline.json`
records the two postures as 645 and 239 flint-only failures).

**Shipping ACLs on a server that does not enforce mode is a capability
lie of exactly the class we just fixed in `FATTR4_LINK_SUPPORT`** —
advertise the feature, evaluate nothing. So:

> **P0 — flip `FLINT_NFS_ENFORCE_PERMISSIONS` to enforce-by-default, and
> re-baseline pjdfstest against it, BEFORE any ACL code.**

That flip is its own project with its own blast radius (it is the
difference between 645 and 239 failing assertions) and it must not be
smuggled in as a sub-task of ACL work.

Two more preconditions, both real:

- **P1 — an identity story.** An NFSv4 ACE's `who` is a *string*
  principal (`user@domain`, or the specials `OWNER@`/`GROUP@`/
  `EVERYONE@`). Under `sec=sys` every client is trusted to assert its
  own uid. Decide up front whether flint's ACLs express only the three
  specials plus numeric-string uids (honest, cheap, and what an
  AUTH_SYS export can actually mean) or whether an idmapper arrives
  first. **Do not ship string principals the server cannot resolve.**
- **P2 — the fidelity differential** (`posix-fidelity-measurement-plan.md`,
  Arm D). Both features add per-file state the tier must round-trip, and
  there is currently no instrument that would notice it being lost.

## 3. Phase 1 — xattrs (RFC 8276). Effort: M.

### 3.1 The wire
New `nfs/v4/operations/xattrops.rs`; decode in `compound.rs`, dispatch in
`dispatcher.rs`, opcodes 72–75. Advertise `FATTR4_XATTR_SUPPORT`
**truthfully and per-export** — the `LINK_SUPPORT` lesson applies
verbatim: if the tier is on and cannot carry xattrs, the bit is FALSE.

### 3.2 THE SECURITY ITEM — read this one twice

The tier's eviction marker is an xattr in the **`user.*` namespace**:

```
tier/evict.rs:53  pub const EVICTED_XATTR: &str = "user.flint.tier.evicted";
```

`user.*` is precisely the namespace RFC 8276 exposes to unprivileged
clients. **The moment `SETXATTR` exists, any client can forge or clear an
eviction marker** — fabricate one on a resident file and reads answer
from a bucket object that does not exist; clear one on an evicted file
and the 0-byte stub becomes the truth, so `READ` returns EOF under
`NFS4_OK` and the next flush **publishes the empty file over good data.**

This is a privilege escalation into the tier's state machine, reachable
from an ordinary unprivileged mount.

> **Mandatory: `user.flint.` is a reserved xattr prefix. `SETXATTR` and
> `REMOVEXATTR` refuse it with `NFS4ERR_PERM`; `GETXATTR` and
> `LISTXATTRS` do not report it.** Same shape as `epoch::is_reserved_component`
> for path names — and it should be one predicate, in one place, for the
> same reason.

Also refuse `system.*`, `trusted.*` and `security.*`:
- `system.posix_acl_access` / `_default` **must never be settable this
  way** or it is a back door around the entire ACL model in Phase 2;
- `security.*` is SELinux labelling — a separate policy question, not a
  free rider on this one;
- `trusted.*` is root-only by definition and `sec=sys` cannot mean it.

### 3.3 Caps
Enforce a per-value size and a per-file count **before** the local
`setxattr`, so ext4's own limits (one block, ~4 KiB of names+values on a
default inode) surface as a documented `NFS4ERR_XATTR2BIG` rather than
as sporadic EIO from the local filesystem.

### 3.4 The carrier
Add `xattrs: Option<BTreeMap<String, String>>` (base64 values) to
`manifest::Entry`. This is an **additive optional field**, which old
readers ignore — safe at `MANIFEST_VERSION 1`. (An added `EntryKind`
*variant* would not be: there is no `serde(other)`, so it hard-fails an
old hub's parse. Fields yes, variants no.)

Two constraints on that:
- **Spill above a threshold.** The manifest is one JSON object rewritten
  at every barrier and can already run large; inlining every xattr of
  every file is how it becomes unusable. Above N bytes, write
  `.flint/xattr/<content-hash>` and cite it.
- **Never parse a new stamp key with `?`.** `PosixStamps::from_meta`
  (`crates/flint-store/src/lib.rs`) uses `?` on every field and returns
  None-as-a-set, and the sweep lane reads `None` as "use the hub's own
  uid/gid" (`import.rs`). A required new key parsed with `?` silently
  **reassigns ownership of every object published before the upgrade.**
  Optional, `unwrap_or`, always.

Count what is dropped (`skipped_xattrs`), and surface it — an
uncounted loss is the failure mode this whole workstream exists to end.

## 4. Phase 2 — NFSv4 ACLs. Effort: XL.

### 4.1 The fork that decides the project

**(a) Translate to POSIX ACLs** (what knfsd does). NFSv4 ACL ⇄
`system.posix_acl_access`. Kernel enforces it on the local FS for free;
local tools and the tier see a coherent world; the existing uid/gid model
still means something. Lossy: DENY, AUDIT and ALARM ACEs, and the
fine-grained masks (`WRITE_ACL`, `WRITE_OWNER`, `DELETE_CHILD`) have no
POSIX equivalent.

**(b) Store the NFSv4 ACL verbatim and enforce it in the server.** Full
fidelity, and wrong here: flint's export has **more than one door**. The
tier walks and rewrites the tree, `import::apply_posix` restores only
mode/uid/gid, and the HTTP file API is a second write path. An ACL only
the NFS dispatcher understands is an ACL those three bypass.

> **Recommendation: (a), with loud refusal.** `SETATTR` of an ACL
> carrying anything POSIX cannot express is answered `NFS4ERR_ATTRNOTSUPP`,
> not silently downgraded. `FATTR4_ACLSUPPORT` then advertises
> `ACL4_SUPPORT_ALLOW_ACL` **only** — again, truthfully.

### 4.2 The work behind the fork
- `GETATTR(acl)`: synthesize an NFSv4 ACL from mode + POSIX ACL.
- `SETATTR(acl)`: translate, or refuse.
- **mode ⇄ ACL coherence (RFC 8881 §6.4).** A `SETATTR` of mode must
  rewrite the ACL and vice versa. This is the subtle half and it is
  where implementations get it wrong.
- **Inheritance** → POSIX *default* ACLs on directories. `INHERIT_ONLY`
  and `NO_PROPAGATE` do not map cleanly; refuse what does not.
- The access check itself moves from "mode bits" to "ACL evaluation",
  which is a rewrite of the path P0 just turned on.

### 4.3 The tier asymmetry worth naming
POSIX ACLs live in `system.posix_acl_*`, which §3.2 says clients may
**not** set. The tier must nonetheless **carry** it. So the rule is not
one list: *refuse `system.*` at the NFS door, carry `system.posix_acl_*`
in the manifest.* Write that down where both sides can see it, or one
side will "tidy" it away.

## 5. Verification
pjdfstest barely touches either feature. The real gates:
- **xfstests generic/** — `generic/020`, `062`, `097`, `377` and the ACL
  group; the differential harness at `tests/lima/pnfs/xfstests-differential.sh`
  already exists and already treats `_notrun` as a coverage loss.
- **`nfs4-acl-tools`** (`nfs4_getfacl`/`nfs4_setfacl`) for the ACL wire.
- **Arm D** gains an xattr/ACL column — the round trip, not just the mount.

## 6. What NOT to build
1. `OPENATTR` / named attributes — Linux never wired them to `getxattr`.
2. Server-enforced verbatim NFSv4 ACLs — three doors bypass them (§4.1b).
3. Either feature while `FLINT_NFS_ENFORCE_PERMISSIONS` defaults to warn.
4. `security.*` xattrs as a side effect of `user.*` support.

## 7. The question to answer before starting
**Who is asking?** xattrs matter for SELinux labelling, `user.*`
application metadata, and macOS resource forks. ACLs matter for
Windows-interop and multi-group sharing. Neither appears in any drill,
workload or customer note in this repo today.

Phase 1 is defensible on its own — it is bounded, and §3.2 is a real
hole that `SETXATTR` would open whether or not anyone asked for xattrs.
**Phase 2 is XL and should not start without a named workload.**
