# Lean on Azure Blob and Google Cloud Storage: implementation plan (2026-11-10)

Scope: lean only, which covers the syncer, the gateway, the operator, and
lean volumes through the s3csi node plugin. Forge, the block tier and
passthrough are out of scope.

## 1. Verdict

There is **no protocol change, no model change and no proof change**, on one
condition: every new backend must meet the store contract in §2. Azure and
GCS meet all of it natively except C3, "equal ETags mean equal bytes". The
backends meet C3 by construction, as §3 describes.

If a backend cannot meet C3, the other route (§3.3) needs three things: a
model switch, a gate run, and the TLAPS proof re-run.

## 2. The store contract lean relies on

| | property | where lean relies on it | S3 today | Azure Blob | GCS |
|---|---|---|---|---|---|
| C1 | Conditional PUT is enforced (create-only and If-Match), and every failed condition is reported as `PreconditionFailed` or `Conflict` | Every upload is create-only to a fresh handle. If-Match is used on `current` (`manifest.rs:1381-1398`), the inbox (`inbox.rs:86`) and the lease cell. Probed at start (`conformance.rs`) | yes | Put Blob / Put Block List with `If-None-Match: *` / `If-Match`. A create conflict may come back as 409 `BlobAlreadyExists`; map it | `ifGenerationMatch` (0 = create-only) answers 412 |
| C2 | Conditional DELETE is enforced | Only the probe uses it. The collector has deleted unconditionally since handles (`untracked.rs:304`, `manifest.rs:1542`); if C2 is broken, the collector is turned off | yes | `If-Match` | `ifGenerationMatch` |
| C3 | Equal ETags mean equal bytes | Several places compare ETags of *different* handles at one path: the merge (`manifest.rs:1767`, `:1949`), the consume's skip (`barrier.rs:368`), sync (`sync.rs:268`), and the gateway's If-Match (`workspace.rs:975`). `lean/SAFETY.md` §2 lists it as an assumption; the models build on it (`LeanSubtree.tla:352`, `MaxSameBytes`) | yes for a plain PUT (the MD5); see §3.4 | **no**: an ETag names a write, not bytes | **no**: a generation number names a write of one object name |
| C4 | LIST is strongly consistent and carries last-modified | chunk GC (`manifest.rs:1605-1612`), sweeps (`untracked.rs:79`) | yes | yes | yes |
| C5 | A finished upload appears atomically, and an unfinished one is invisible under `files/` | compose (`barrier.rs:1926`) | MPU Complete | Put Block List | compose |
| C6 | A copy keeps the bytes and the CRC, not the stamps | the `copy_object` contract (`crates/flint-store/src/lib.rs:770-789`) | CopyObject | Copy Blob From URL / Put Block From URL | `objects.rewrite` |
| C7 | Every lean-written object returns its full CRC-64/NVME | Without it lean refuses to cite (`barrier.rs:1936-1945`); the torn-response check (`barrier.rs:1957-1962`) | computed by the server | checked per request (Azure's CRC64 is CRC-64/NVME); the whole value lives in metadata | the server checks CRC32C; CRC-64 lives in metadata |
| C8 | The store takes every key `key_fits` accepts | `lean/syncer/src/lib.rs:693` (L-145): the barrier holds a path that doesn't fit, the gateway refuses it | 1,024 bytes | 1,024 characters, at most 254 segments; avoid names ending in `.` or `/` | 1,024 bytes of UTF-8; no CR or LF; not `.` or `..` |

## 3. C3: content ETags

### 3.1 The rule

The Azure and GCS backends present as `etag` a content hash of the
object's bytes: the first 128 bits of its SHA-256, quoted, as 32 hex
digits. It is the same derivation as lean's chunk addresses
(`lean/syncer/src/chunk.rs:84-88`) and the same length as an S3 MD5 ETag.
It is stored as metadata (`flint-sha256`; on Azure, `flint_sha256`) and
written atomically with the object. Inside the backend, the provider's own token (the Azure ETag,
the GCS generation) carries the conditions.

- **put_whole:** the hash of the body. The server checks the bytes by CRC
  (Azure `x-ms-content-crc64`, GCS `crc32c`). Neither store checks a
  SHA-256, so the hash travels as metadata.
- **compose:** the hash is computed in the same streaming pass as the CRC-64,
  with no pre-pass. It is set as metadata at the commit (Put Block List /
  compose), which is the point where it becomes known.
- **copy_object:** the hash is carried over from the source's metadata.
- **If-Match on PUT, DELETE and GET:**
  - The backend takes the provider token from its cache of the last read or
    write of that key, or from one HEAD on a miss.
  - It checks the stored hash, then sends the provider's precondition on that
    token. That precondition is the atomic check.
  - A 412 on a cached token is re-checked with a HEAD. If the current hash is
    the expected one, the write retries with the fresh token. If not, the
    412 is reported.
  - The answer is therefore the same as S3's in every case, including a body
    that changes and changes back. The inbox does exactly that: an empty
    cell is the same bytes every time.
- **Ranged GETs:** the first response pins the provider token, and every
  later range uses it.
- **LIST** returns the hash from metadata (Azure `include=metadata`; GCS list
  includes metadata).
- **Objects written by outside tools** carry no hash.
  - They are presented as `"x-<key digest>-<token>"`: unique per version,
    and never equal to another object's ETag.
  - Only the ingress sweep reads them (`untracked.rs:80-125`). It names its
    copy after that ETag, so it never makes two copies of one version.
  - When lean adopts such an object, the backend reads it once to stamp the
    handle's hash and CRC. Lean already does this for Ozone's missing CRC.

### 3.2 Why this keeps the proof

The models represent a version's bytes as `Content(h)` (`LeanP1.tla:262`).
The SAFETY.md §2 row "an etag names the bytes" is what ties the code's ETag
comparisons to that. With content ETags, the row holds by construction on
both backends. The row is rewritten to say how each store meets it.

### 3.3 The other route (not recommended)

Use the provider tokens as ETags directly. Two things then go wrong:
- copies of the same bytes get different ETags, so the code sees a change
  where the model sees none;
- two different objects can in principle share an ETag (Azure's is
  time-based; a GCS generation is unique only within one object name).

Closing those gaps needs:
- a constant in `LeanP1.tla` for the ETag semantics, with every code ETag
  comparison mapped to it;
- the gate worlds re-run;
- the TLAPS proof re-run for the actions it touches.

### 3.4 Side finding: S3 itself

AWS documents that objects encrypted with SSE-KMS or SSE-C have ETags that
are not the MD5 of their data. On such a bucket, lean already runs outside
SAFETY.md §2's row today. The same content-ETag layer would cover it. To be
confirmed and tracked separately.

Putting the layer on S3 too would also take MD5 out of lean's comparisons
everywhere. MD5 collisions can be made on demand: a writer could publish
two different files with the same ETag, and peers that hold the first would
skip the second as unchanged. Nothing is deployed, so no migration would be
needed.

### 3.5 Cost

- **Throughput:** the hash runs once, inside the one streaming pass.
  - With the CPU's SHA instructions, SHA-256 runs at roughly 1.5–2 GB/s per
    core. `sha2` 0.10.9 picks them up at run time through `cpufeatures`.
    Recent x86 server CPUs and Graviton have them.
  - Without them it drops to about 0.4–0.5 GB/s, below the measured
    part-parallel upload rate of 537–638 MiB/s (`flint_sync.rs:235-240`).
  - Measure it in phase 1 on the node types lean targets.
- **e2e oracles:** some predict an ETag as the MD5 of the bytes. That holds
  on S3 only:
  - `lean/e2e/writers-live/agent.sh`, `ui.sh`, `storm-local.sh`,
    `agent_localtest.sh`, `sampler_selftest.py`;
  - `lean/formal/trace/storm2conf.py`.

  They need to compute the backend's form, or better, read the ETag from the
  store or the gateway instead of predicting it.
- **Requests:** the HEAD before a conditional write happens only on a cache
  miss. Lean reads `current` on every tick, which fills the cache before
  the CAS.

## 4. Phases

All builds and tests run on the Linux box.

### Phase 0: groundwork, with no change in behaviour on S3

- **flint-store:**
  - add a provider choice (`S3 | Azure | Gcs`) and one `connect` factory;
  - add Cargo features `azure` and `gcs`;
  - add a per-backend key rule for C8, which `key_fits` consults.
- **lean:** the syncer (`flint_sync.rs:251`), the gateway
  (`flint_lean_gateway.rs:96`, plus the public API at `lean/gateway/src/lib.rs:30-62`)
  and the operator (`flint_lean_operator.rs:70`) all build their store
  through the factory.
  - Add a new `FLINT_SYNC_PROVIDER`.
  - The S3-only settings are refused for other providers: `rawReads`
    (`lean_operator/crd.rs:171`) and `FLINT_SYNC_COPY_WHOLE_MAX_MB`.
- **A store contract suite**, `crates/flint-store/tests/contract.rs`:
  - C1–C8 as tests, run against the memory double, plus MinIO and S3
    (ignored, env-driven, like `presign_live.rs`);
  - every later backend must pass it.
- **Exit:** the existing suites are green, and the contract suite is green
  on memory, MinIO and S3.

### Phase 1: the Azure backend (`azure` feature)

- **Client:** REST on flint-store's existing hyper/rustls stack (decision D1),
  with the API version pinned.
- **Auth:** shared key and SAS first (a static Secret). Entra comes in
  phase 4.
- **Operations:**
  - `put_whole`: Put Blob with `x-ms-content-crc64` and the hash as metadata.
  - `compose`: Put Block for each part, each checked with
    `x-ms-content-crc64`, then Put Block List with `If-None-Match: *` and the
    metadata.
  - `copy_object`: Copy Blob From URL with `x-ms-source-if-match`. Above its
    synchronous size limit, use Put Block From URL per range, then Put Block
    List.
  - Reads: head, get, ranged get, and list with `include=metadata`. Normalize
    ETags, because LIST returns them unquoted while HEAD quotes them.
  - Deletes: delete, and `delete_many` through Blob Batch (256 per request).
- **The lease:**
  - `epoch_*` runs on the cell's body with If-Match.
  - **Not** native blob leases: liveness is judged by the token changing
    (`tier/epoch.rs:13-16`, `lean/syncer/src/lease.rs`), and an Azure lease
    ID stays the same across renewals.
- **Upload sweep:** `list_uploads` returns nothing and `abort_upload` is a
  no-op, because Azure drops uncommitted blocks after 7 days.
- **bootstrap:**
  - checks the container is reachable and makes a probe write;
  - versioning and lifecycle are account settings in the management plane,
    so they are reported as not checked.
- **Part limits:** a block is at most 4,000 MiB, and a blob holds at most
  50,000 blocks.
- **Metadata names:** Azure forbids hyphens, so `flint-gen` ↔ `flint_gen`
  both ways, for every stamp (`crates/flint-store/src/lib.rs:331-367`).
- **Errors:**
  - `ConditionNotMet` (412), and `BlobAlreadyExists` (409) on a create →
    `PreconditionFailed`;
  - 404 → `NotFound`;
  - 401/403, clock skew included → `Auth`;
  - `ServerBusy` (503) and other 5xx → retried, with the same classes as
    the S3 path.
- **Tests:** emulator first. The contract suite runs against Azurite,
  Microsoft's emulator, in CI and on the box. Only once it is green there
  does it run against a real storage account (ignored; D4). Where Azurite
  and the account disagree, the account wins, and the difference is
  written down beside the test.
- **Exit:** the contract suite is green on a real account, and the hash cost
  is measured on a multi-GiB file.

### Phase 2: lean on Azure, end to end

- **CRD:** a `provider` field (default `s3`) plus the Azure account and
  container. Validation refuses S3-only fields. `sync_env.rs` renders the
  provider env.
- **Credentials:** s3csi's static-Secret mode carries Azure keys or SAS, and
  so does the lean chart for the gateway.
- **The startup conformance probe** (`conformance.rs`) runs unchanged,
  through the trait.
- **e2e:**
  - the kind rig with Azurite;
  - on a real account, the writers drill (`lean/e2e/writers-live`);
  - the door drill's `small` leg, against the S3 baseline.
- **Exit:**
  - the writers drill is clean (no lost acked write, no refusal);
  - the probe answers `Conformant`;
  - performance is recorded.

### Phase 3: the GCS backend (`gcs` feature), and lean on GCS

- **Client:** the JSON API. Conditions use generations: `ifGenerationMatch`,
  `ifSourceGenerationMatch`, and `ifGenerationMatch=0` for create-only.
- **compose:**
  - each part is uploaded as its own object, create-only and
    CRC32C-checked, under `<prefix>/.flint/lean/parts/<flush>/<n>`;
  - then compose: at most 32 sources per call, nested above that, with
    `ifGenerationMatch=0` on the destination, each source pinned by its
    generation, and the CRC-64, the hash and the stamps as metadata in the compose
    request;
  - then delete the parts.
  - **The server-side check:** the composite's CRC32C must equal the CRC32C
    computed in the pass.
  - The parts live under the reserved `.flint/lean/` namespace, which the
    sweeps don't list (they list `files/`, `manifests/` and `chunks/`). The
    operator's upload sweep deletes stale parts by age, in place of
    `list_uploads`/`abort_upload`.
- **put_whole:** one request with `crc32c`, which the server checks. The
  CRC-64 and the hash go in metadata.
- **copy_object:** `objects.rewrite`, looping on `rewriteToken`.
- **delete_many:** a JSON batch, 100 per request.
- **Key rule:** no CR or LF.
- **bootstrap:** a bucket GET; versioning, lifecycle and uniform access are
  reported as notes.
- **The write-rate limit:** GCS allows about one write per second per object
  name. These objects are hot:
  - the lease cell: claim, renew and handoff in every commit, plus one
    enqueue per waiter;
  - `current`: every commit and every gateway save;
  - the inbox;
  - drafts: body and meta on every autosave.

  429s are retried with backoff and counted (`request_counts`). Measure with
  the writers drill at 1, 3 and 6 writers plus gateway saves, recording the
  429 count and the commit-section time.
- **Tests:** emulator first: Google's `storage-testbench` (the emulator
  Google's own client libraries test against), in CI and on the box; then
  a real bucket (D4). `fake-gcs-server` is the fallback if the testbench
  lacks an operation. Check how faithfully the emulator handles
  preconditions before trusting it.
- **Exit:**
  - phase 2's exit, on GCS;
  - the rate-limit numbers recorded;
  - a decision on whether lean on GCS needs a cadence floor or only a note
    in the docs.

### Phase 4: workload identity

- **Azure:** a projected ServiceAccount token (audience
  `api://AzureADTokenExchange`) is exchanged for an Entra token by client
  assertion, and refreshed before it expires.
- **GCS:** the GKE metadata server. Off GKE, Workload Identity Federation:
  an STS exchange of the projected token, then service-account
  impersonation if one is configured.
- **s3csi:** the worker's credential modes get provider-native variants.
  Today the credential is the AWS tuple (`s3csi/creds.rs:46-54`), served
  through the container-credentials door (`creds.rs:320-329`).
- **Exit:** a lean volume on AKS and on GKE with no static secret.

### Phase 5: docs and release

- **Docs:**
  - SAFETY.md §2: C1–C8 per store, with the C3 row rewritten;
  - the store section of `lean/PROTOCOL.md`;
  - the guides and the CRD docs;
  - a CHANGELOG entry.
- **Crates:** flint-store, flint-lean and the gateway are published only
  after the gate passes and with the user's go-ahead.

## 5. Not in this plan

- Per-user read access through the broker. GCS can downscope a token to a
  prefix; Azure can only do that with a hierarchical-namespace directory
  SAS.
- The raw read fast path.
- Forge, the block tier and passthrough.

## 6. Decisions

Decided by the user on 2026-11-10:

- **D1: hand-written REST**, on flint-store's hyper/rustls stack, not the
  Azure or Google SDKs. The surface is small, and it keeps the SDK
  dependency trees out of the CVE scan.
- **D2: SHA-256, truncated to 128 bits**, for content ETags (§3.1), not MD5.
  MD5 collisions can be made on demand; SHA-256's cannot.
- **D3: Azure first**, then GCS.
- **Emulators first:** Azurite for Azure and `storage-testbench` for GCS,
  before any real account.

Still open:

- **D4: real accounts for the phase 1 and phase 3 tests**: one storage
  account and one bucket, at a cost of cents per day. Nothing is created
  without the user's approval.
- **D5: the S3 findings (§3.4)**: SSE-KMS ETags, and whether to put the
  content-ETag layer on S3 too.

## 7. Rough size

| phase | estimate |
|---|---|
| 0 | 3–4 days |
| 1 | 1.5–2 weeks |
| 2 | about 1 week |
| 3 | about 2 weeks |
| 4 | about 1 week |
| 5 | 2–3 days |
