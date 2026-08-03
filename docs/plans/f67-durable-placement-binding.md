# Durable placement binding — striped data must never read as silent zeros (F67)

Status: IMPLEMENTED 2026-08-03. The §7 restart drill
(`tests/lima/pnfs/restart-drill.sh`, `make test-pnfs-restart`) passes
all three legs: identical md5 across a full server restart on the
memory backend (this leg read 256 MiB of ZEROS before the fix), and the
stripped-binding leg fails loud (EIO + F67 log), never with zeros.
Unit suite 1146 green; smoke green.

IMPLEMENTATION REFINEMENT over §4 as first drafted: the grant-level
refusal maps to LAYOUTUNAVAILABLE (not NFS4ERR_IO), because the miss
branch is also the first LAYOUTGET of every MDS-native file — a file
whose data legitimately lives in the stub. The client falls back to MDS
I/O, where the fallback disposition finishes the funnel: dense stub
(blocks > 0) ⇒ Serve (native data, pre-F67 behavior); sparse-with-size
and bindingless ⇒ FailFast (either an all-holes native file, where EIO
is recoverable, or a destroyed striped binding, where Serve would be
silent corruption — loud wins the ambiguity). NFS4ERR_IO thus surfaces
exactly once, at the only layer that can tell the two shapes apart.

## 1. The defect (F67)

A striped file's data lives on the DSes in stripe files named by
`file_id` (`{file_id:016x}.stripeN`). The `file_id ↔ path` binding
exists in exactly one place: the MDS placement record
(`PlacementRecord`, via the `StateBackend`). `allocate_file_id()` is
uuid-random per allocation (`layout.rs:282`), minted at the single call
site `placement_for_grant` (`layout.rs:1204`) on first grant for a
`file_key`, and persisted only through `enqueue_write(PutPlacement)`.

If the MDS boots without its placement records while the export tree
(stubs) survives, `placement_for_grant` cannot tell "new file" from
"existing file whose binding I lost". It mints a FRESH random file_id,
layouts go out under the new id, and:

- **READs** ask the DSes for stripe files that do not exist under the
  new id. Since `ffb3ab9`, an absent stripe file is answered as a HOLE
  (eof + empty — a deliberate, correct fix for sparse-file legality;
  fsstress found 13 layout poisonings without it). The client
  zero-fills. **Every byte of the file reads as zero, quickly, with no
  error at any layer.**
- **WRITEs** create NEW stripe files under the new id. The file now has
  two disjoint generations on disk; the old one is permanently orphaned
  and the new one is sparse zeros everywhere the app didn't rewrite.
  This is corruption committed to durable storage, not just misreads.

Proven end-to-end on an x86 rig (2026-08-03): same-boot md5
`e73eac…` with 2,050 MiB crossing loopback; after MDS+DS restart the
md5 of the same file equals the md5 of exactly 2 GiB of zeros with
0 MiB crossing loopback, while ds1's strace shows the client asking for
`001ec487….stripe0` where disk holds `0081e87d….stripe0` — same path,
different boot's mint. It also silently VOIDED two copy-tax A/B gates
before being caught (the metered "serving" was the zero-fill path), so
this class corrupts measurements as effectively as it corrupts reads.

### Exposure

- **Production (chart)**: placements persist in sqlite on the MDS
  `/data` PVC — a plain pod restart is safe. **Losing or resetting that
  PVC while DS PVCs survive springs F67 on every striped file.** That
  is not hypothetical: the v1.12 campaign hit a landmine-struck MDS PVC
  and recovered by scale-cycling; under F67 the same event with
  surviving consumers yields silent zeros, not an outage.
- **`state.backend: memory`** (every lima config: smoke, fsx,
  fallback-drill): ANY server restart springs it. Every test that reads
  data written before a restart is structurally at risk of "passing" on
  zero-fill.
- The failure is **silent by construction**: fast, error-free reads of
  plausible length. Nothing in the fleet — client, MDS, DS, or
  operator — currently notices.

## 2. The principle

The system may lose the binding — disks die. What it may NOT do is
**answer confidently from a lost binding**. Two obligations follow:

1. Make the binding survive everything the stub survives (it is the
   stub's sibling; today it lives in a different failure domain).
2. When the binding is nonetheless gone for a file that provably has
   data, REFUSE — loud EIO beats quiet zeros, always.

The DS hole semantics (`ffb3ab9`) are NOT the bug and are not touched:
a DS cannot know a file's logical size, and absent-stripe-is-a-hole is
required for sparse correctness. The authority that knows a nonzero
stub must have reachable stripes is the MDS. The guard goes where the
knowledge is.

## 3. Leg 1 — bind through the stub (xattr), not just the backend

At mint time, write the placement onto the stub file itself as a user
xattr, so the binding lives in the SAME failure domain as the namespace
entry it serves:

    name:  user.flint.placement
    value: v1:{file_id:016x}:{stripe_size}:{device_id,device_id,...}

- Written by the MDS server (owner of `export_path`, `server.rs:29`)
  through a small `StubBinding` trait injected into `LayoutManager` —
  `layout.rs` stays filesystem-free:

      trait StubBinding: Send + Sync {
          fn stub_len(&self, file_key: &str) -> Option<u64>;   // None = no stub
          fn read(&self, file_key: &str) -> Option<PlacementRecord>;
          fn write(&self, file_key: &str, rec: &PlacementRecord) -> io::Result<()>;
      }

- **Ordering closes the crash window**: mint → `StubBinding::write`
  (fsync-less setxattr is atomic per xattr) → `enqueue_write
  (PutPlacement)` → return the grant. If the xattr write fails, the
  grant is REFUSED — an unbound id never reaches the wire. A crash
  after xattr, before backend write, recovers from the xattr on next
  boot; the reverse window cannot exist.
- **Recovery order in `placement_for_grant`** on an in-memory miss:
  backend-restored map (unchanged, seeded by `load_placement_records`)
  → stub xattr → only then §4's guard / fresh mint.
- **Backfill on boot**: after `load_persisted_state` seeds placements,
  walk the records and write any missing stub xattr. Existing fleets
  converge to full xattr coverage on the first post-upgrade boot, so
  leg 1 protects files that predate it.
- **Capability probe at startup**: setxattr+getxattr a probe value on
  the export root. If user xattrs are unsupported, the MDS logs the
  degraded mode at ERROR and — when the backend is `memory` — refuses
  to start. There is no configuration in which "restart = silent
  zeros" is permitted to boot quietly. (ext4/xfs — the chart's PVC
  filesystems — and APFS on the lima rig all support user xattrs.)

## 4. Leg 2 — the orphaned-data guard (fail loud, never mint over data)

`placement_for_grant`, on a miss in both the map and the xattr:

- `stub_len == None` (no stub) or `Some(0)` → genuinely new or empty
  file: mint fresh, write xattr, proceed. Nothing exists to lose.
- `stub_len == Some(n), n > 0` → **ORPHANED DATA. Refuse.** Return a
  distinct error; the dispatcher maps it to `NFS4ERR_IO` for LAYOUTGET
  and for MDS-path READ/WRITE (the F66 proxy also routes through
  placement, so proxied fallback I/O is covered by the same refusal).
  NOT `NFS4ERR_DELAY` — retry cannot repair a lost binding, and DELAY
  is this codebase's proven livelock shape (COPY verifier, F66).
  Rate-limited ERROR log with the path and the remediation:

      F67: '{file_key}' has {n} bytes of striped data but no placement
      binding (backend and stub xattr both empty). Refusing I/O rather
      than serving zeros. Restore the MDS state PVC from backup, or
      delete the stub to abandon the data.

Because `allocate_file_id()` has exactly one caller, the guard is
structurally exhaustive: no path can mint over existing data. Add a
debug assertion + test pinning the single-call-site property.

## 5. What this design deliberately does not do

- **No DS-side second-guessing**: the DS keeps answering absent
  stripes as holes. It has no way to know better, and inventing one
  (querying the MDS per miss) puts a control-plane RTT on the data
  path's rarest and least informed vantage point.
- **No automatic rebind by scanning DSes**: stripe filenames carry no
  path identity; guessing a mapping re-creates the corruption with
  extra confidence. (Future DR tooling may stamp `user.flint.path` on
  stripe files at create time to make an offline fsck possible — out
  of scope here.)
- **No moving the lima rigs off `backend: memory`**: with leg 1 the
  memory backend becomes restart-safe for placements, which is exactly
  the configuration the restart drill wants to exercise.

## 6. Sizing

- Leg 1 (xattr module + `StubBinding` + recovery path + backfill +
  probe + units): ~1 day. musl/libc setxattr on Linux, macOS setxattr
  behind cfg for the lima rig.
- Leg 2 (guard + error mapping + rate-limited log + units): ~0.5 day.
- §7 drill: ~0.5 day.

## 7. Gate — the restart drill (`tests/lima/pnfs/restart-drill.sh`)

1. Fresh boot, `backend: memory`. Write 256 MiB of urandom through the
   mount; md5 it same-boot (wire-verified — loopback/rx delta must
   equal the dataset, the void-gate lesson).
2. Kill MDS + both DSes; restart from the same binaries; remount; drop
   client cache; md5 again. **PASS requires: identical md5 AND wire
   bytes ≈ dataset again.** (Today this leg reads 256 MiB of zeros.)
3. Negative leg: strip `user.flint.placement` from the stub and
   restart again (simulates total binding loss). PASS requires: the
   read FAILS with EIO within the mount's timeout, the MDS logs the
   F67 ERROR naming the file, and no zeros are ever returned.
4. Unit tests: xattr round-trip; recovery-from-xattr equals the
   original placement (file_id, stripe_size, device order); guard
   refusal on nonzero stub; fresh mint on zero/absent stub; backfill
   idempotence; startup refusal on xattr-less fs + memory backend.

## 8. Relation to shipped behavior

- `ffb3ab9` (holes) stays. `84112c5` (fd-cache eviction) unaffected.
- F66's proxy inherits the guard through `placement_for_grant` — a
  proxied write can no longer mint a fresh id over orphaned data.
- The copy-tax gate protocol change this bug forced (fresh boot +
  fresh layout per iteration, wire-bytes honesty check) is recorded in
  `project_runaz_cluster` memory and stays mandatory for any rig that
  restarts servers.
