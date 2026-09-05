# Flow dashboard — plan (2026-09-05)

A dashboard screen that shows network throughput BETWEEN components
(git server ↔ S3, hub ↔ S3, clients ↔ hub, …) in near-real-time with an
animated flow diagram (D3). This document is the plan of record. No code
has been written for it.

**Revision 2 (2026-09-05, same day):** after a mock review, the page
gains fleet and group views for 100+ hubs and forges (§6), and it is
packaged as its own cluster-singleton chart with an in-cluster collector
(§7). The collector makes the CORS work of revision 1 unnecessary; WP3
is reduced accordingly and WP7 is new. The mock decisions are in §8.

The plan has three parts: what each component can report TODAY (§1), the
answer that follows from it (§2), and the work packages that close the
gaps and build the page (§3–§5).

## 1. Inventory — what each component exposes today

Every number below was read from the code, not from a doc. Paths are
relative to the repository root.

| Component | Surface | Byte counters? | Rates? | Push or pull | CORS |
|---|---|---|---|---|---|
| **Hub** (lite / strict), `spdk-csi-driver` | `GET /status` on the health port (default 8080), warp; `pnfs/mds/status.rs` | **Yes.** `tier.meters` is `tier::meter::MeterSnapshot`: `bytesUploaded`, `bytesCopied`, `hydrationBytes`, `bytesEvicted`, `partsUploaded`, `partsCopied`, `publishes`, `hydrationsStarted/Completed`, … all monotonic relaxed atomics | No (counters only; rate = Δ/Δt at the poller) | Pull. The handler never touches S3 (two local DB reads per poll, per the module doc), so a 1 s poll is cheap | None |
| Hub, NFS wire (clients ↔ hub) | same `/status`, `activity` = `nfs::activity::ActivitySnapshot` | **No bytes.** `dataOps`, `namespaceOps`, `browseOps` — op counts by class, bare SEQUENCE/GETATTR excluded by design | No | Pull | — |
| Hub `monitoring.prometheus` config block | `pnfs/config.rs` `PrometheusConfig` (port 9090, `/metrics`) | — | — | **Dead.** Nothing in the crate reads it | — |
| **SPDK block path** (disk product) | `bdev_get_iostat` RPC, called by `spdk_dashboard_backend_minimal.rs` `fetch_bdev_stats` | **Yes at the source** (`bytes_read`, `bytes_written` per bdev) but the backend's `BdevStats` keeps only `read_iops`, `write_iops`, latency; bytes are dropped | IOPS only, derived from the previous sample in `iostat_history` | Pull | n/a (same origin via nginx `/api/`) |
| **Forge syncer** (git server), `forge/syncer` | `GET /status` on `FLINT_FORGE_STATUS_ADDR` (default 127.0.0.1:9848), hand-rolled HTTP in `server.rs`; document in `status.rs` | **No.** `repo.{refs,packs,snapshotSeq}`, `activity.{lastActivityUnix,idleSecs}`, `phase`, `epoch`, `rpoClean`, `fenced` | No | Pull. `Facts` are SNAPSHOTTED by `publish()` at phase changes and batch ends — not live | None |
| Forge door (git HTTP transport, port 8090) | `flint-forge-chart/templates/door.yaml` | No accounting | — | — | — |
| **Lean sidecar**, `lean/sidecar` | `GET /metrics` (opt-in, `FLINT_SYNC_METRICS_PORT` default 9847), Prometheus text rendered from `Gauges` only (`metrics.rs`, D15) | **No cumulative bytes.** Gauges: `flint_lean_staged_uncited_bytes/objects`, `rpo_seconds`, `visibility_lag_seconds`, `last_boundary_seq`, `sentinel_budget_remaining`, … | No | Pull; `gauges.json` is rewritten on boundary events, not every heartbeat | None |
| **Passthrough** (`s3csi`, Mountpoint) | none in our code; Mountpoint's own metrics are not wired | No | No | — | — |
| **`flint-store::ObjectStore`** (`crates/flint-store`) | the trait every S3 byte from hub, lean, forge and the operator passes through (`S3Store`, `MemoryStore`) | **No accounting today.** This is the one choke point that covers every "→ S3" edge at once | — | — | — |
| **Dashboard** (`spdk-dashboard`) | React + tanstack-query; `useDashboardData` polls at `BASE_POLL_MS`, events every 10 s; nginx proxies `/api/` → backend :8080 | — | — | Pull. No SSE or WebSocket anywhere in the repo | — |

Two details that shape the design:

- The only production `ObjectStore` implementations are `S3Store` and
  `MemoryStore`. The wrappers that exist are test doubles in
  `lean/sidecar/src/tests.rs` (`VersionStripping`, `AuthRefusing`, …) and
  they do **not** forward the trait's defaulted methods (`presign_get`,
  `presign_put`, `lifecycle_rules`, `ensure_noncurrent_retention`,
  `list_versions`, …). A production wrapper that copied that shape would
  silently turn a versioned S3 bucket into "this backend cannot presign /
  cannot read lifecycle rules" — forge bundles, LFS and lean's gated mode
  all depend on those. Forwarding every method is a correctness
  requirement, and it needs its own test.
- The operator's parser of the hub/forge status document
  (`lite_operator/hubstatus.rs`) has no `deny_unknown_fields`, so adding a
  section to the document does not break the suspend ladder.

## 2. The answer

- **Do the components have the information?** Partly. Hub ↔ S3: yes.
  SPDK volumes: yes at the source, dropped one hop later. Git server ↔
  S3: no. Lean ↔ S3: no. NFS wire and Mountpoint: no bytes anywhere.
- **Can it be real time?** Everything is pull-only. The counters are
  process-local atomics, so a browser polling each endpoint once a second
  and animating the delta gives 1 s granularity at negligible cost. That
  is the same mechanism the existing dashboard already uses for IOPS.
  Nothing pushes; server-sent events are not needed for a flow animation
  and are not in this plan.
- **Git server ↔ S3 specifically** needs one new counter surface (WP1 +
  WP2 below). It is the smallest piece of work in the plan and the one
  that unblocks the example.

## 3. Work packages

### WP1 — Store meter in `flint-store` (prerequisite for forge and lean)

New module `crates/flint-store/src/metered.rs`:

- `StoreMeter`: relaxed `AtomicU64` counters in the `tier::meter` idiom
  (a `counters!`-style snapshot struct, serde camelCase, `delta_since`):
  - `bytesToStore` — `put_whole` body length; `compose_generation`
    `PartSource::Local` lengths (the bytes that cross the NIC upward)
  - `bytesFromStore` — `get_whole`, `get_version`, `get_range` returned
    lengths (downward)
  - `bytesCopiedInStore` — `compose_generation` `PartSource::BaseCopy`
    lengths (server-side copy; no wire, shown differently on the page)
  - request counts by class: `puts`, `gets`, `heads`, `lists`, `deletes`,
    `epochOps`, `mpuOps`, `other`; `requestFailures`
  - `inFlight` gauge (inc on entry, dec on exit via a drop guard, the
    same shape the peer just added to `MemoryStore::get_range`)
- `MeteredStore { inner: Arc<dyn ObjectStore>, meter: Arc<StoreMeter> }`
  implementing `ObjectStore` by delegation for **every** method,
  defaulted ones included.
- Counting rules, written into the module doc: put bytes count when the
  call returns whether or not it succeeded (the wire carried them; a 412
  after a full upload is still throughput); get bytes count on success
  only (we only know what arrived). No allocation, no lock, no await
  added on the hot path.
- Tests (memory store, no S3):
  1. each method attributes to its counter and to `requests`;
  2. **defaulted methods forward** — `list_versions`, `presign_get`,
     `lifecycle_rules` through the wrapper return the inner's answer,
     not the trait default's refusal (this is the test the lean doubles
     lack);
  3. a failing put counts `requestFailures` and still counts its bytes;
  4. `inFlight` returns to zero after an error return.

Build: `flint-store` is its own crate with a warm target; minutes.

### WP2 — Forge: bytes on `/status`, live

- `Syncer::new` wraps whatever store it is handed in `MeteredStore` and
  keeps the `Arc<StoreMeter>`; the bin and the test `Rig` need no change.
- `status::Facts` carries the `Arc<StoreMeter>`; `status::document`
  reads `meter.snapshot()` **at render time**, so the section is live
  even though the rest of `Facts` is a snapshot from the last
  `publish()`. Nothing else in `Facts` changes.
- New section, additive:
  ```json
  "store": { "bytesToStore": 0, "bytesFromStore": 0, "bytesCopiedInStore": 0,
             "requests": 0, "requestFailures": 0, "inFlight": 0,
             "puts": 0, "gets": 0, "heads": 0, "lists": 0, "deletes": 0,
             "epochOps": 0, "mpuOps": 0 }
  ```
- Optional, same change: `pushesAcked` counter on `Syncer`, bumped where
  `last_push_unix` is set, so the git-clients → forge edge can pulse per
  push (today the page would have to infer a push from
  `lastActivityUnix` moving).
- `Access-Control-Allow-Origin: *` on `GET /status` only (the response
  head is assembled in one place in `server.rs`; the header is added for
  that branch, not for the LFS batch API).
- Tests: extend `the_status_document_is_the_shape_the_ladder_reads`
  (section present with zeros after `start()`; `bytesToStore > 0` and
  `puts > 0` after one acknowledged push, reusing the
  `an_acknowledged_push_is_in_the_bucket_before_the_ref_moves` shape);
  a CORS assertion on the HTTP path if a serve_http test exists,
  otherwise a curl in the MinIO check below.
- Real-endpoint check (Docker is up — 29.7.2): `forge/e2e/composition/rig.sh`
  brings up MinIO and a real syncer with a status address; a push loop
  plus two `curl /status` samples 1 s apart must show `bytesToStore`
  rising by about the pack size. This is the only step that proves the
  numbers are S3 bytes and not the memory double's.

**Coordination:** a peer session (the forge latency rig / restore fan-out work of 2026-09-05) has ~330 uncommitted lines in
`forge/syncer` (`packio.rs`, `restore.rs`, `tests.rs`, `lib.rs`, …) and
`crates/flint-store/src/memory.rs`. WP2 edits `lib.rs`, `status.rs`,
`server.rs`, `tests.rs`. Edits must be targeted, the suite must pass with
both sets of changes, and nothing of theirs gets committed by this work.

### WP3 — Hub: nothing required (revised)

Revision 1 put a CORS header on the hub's `/status` here. The collector
(WP7) polls the hub from inside the cluster and serves the page from the
same origin, so no hub change is needed for the hub ↔ S3 edge: the byte
counters are already on `/status`.

Optional, phase 2: wrap the hub's store in `MeteredStore`, which adds
request counts and `inFlight` to `tier` for free. That is the only
reason left to build the hub crate for this feature, and it is batched
with WP5.

### WP4 — Lean: deferred, needs a design decision

`/metrics` is rendered from `Gauges` alone by design (D15: one renderer
over one struct, zero bucket requests per scrape). Store counters would
be a second source feeding the renderer, so they are a design change to
D15, not a drop-in. The page shows lean as a node with its gauges
(staged uncited bytes, RPO, lag) and says "no rate data" on its edges.
Revisit if lean ↔ S3 throughput is wanted.

### WP5 — SPDK volume throughput: deferred to phase 2

Pass `bytes_read` / `bytes_written` through `BdevStats` and the
`/api/dashboard` volume rows (about 20 lines next to the existing IOPS
delta), then a MB/s column and a per-volume edge on the page. Needs the
hub crate build; batch with WP3.

### WP6 — The flow page

Location: the page is static files served by the collector (WP7) from
the `flow/` crate's own directory, **not** inside `spdk-dashboard`.
Reasons: it spans products that are deployed by different charts; the
disk dashboard is suppressed under the lite profile, which is exactly
the failure a cross-product page must avoid; and it has no build step.
The disk dashboard may link to it by URL.

The page has three views, chosen by source count (§6): **fleet** (a
table), **group** (a summed diagram for one namespace, bucket or label)
and **detail** (the single-source diagram). All three are URL-addressable
and the page keeps no state outside the browser tab.

Files:
- `index.html` — the page. D3 v7 from cdnjs, pinned; inline CSS;
  theme-aware (light/dark).
- `flow.js` — all logic as pure functions plus a thin render loop:
  `computeRates(prev, next, dtMs)` (Δcounter/Δt, a decrease = restart =
  0 for that tick), the three adapters (`forge`, `hub`, `lean`) that map
  a status document to nodes + edges, a Prometheus text parser (~15
  lines) for lean, and a formatter (B/s, KiB/s, MiB/s).
- `flow.test.mjs` — `node --test` for every pure function: rates,
  reset handling, each adapter against a captured real document, the
  Prometheus parser.
- `mock/status_server.py` — stdlib-only dev harness serving
  `/forge/status`, `/hub/status`, `/lean/metrics` in the real shapes with
  CORS and a scripted load pattern (idle → burst → idle), so the page can
  be developed and demonstrated with no cluster and no Docker.
- `README.md` — how to point it at a cluster:
  `kubectl port-forward` the hub health port and the forge status port,
  open `index.html?forge=http://127.0.0.1:9848&hub=http://127.0.0.1:8080`.

Behaviour:
- Sources come from the collector's roster (WP7), not from a URL list;
  the query string selects the view (`?view=fleet&ns=…&sort=…`,
  `?hub=ns/name`, `?repo=ns/name`). The focused source polls at 1 s,
  everything else at 5 s, `performance.now()` for Δt. A manual source
  list remains available for a port-forwarded single hub with no
  collector.
- Diagram: nodes for git clients, forge, S3, NFS clients, hub, lean
  workspace (only those with a configured source, plus S3). Directed
  edges with animated particles whose density and speed follow the
  current rate on a log scale; a rate label per edge; a 60-sample
  sparkline per edge; a server-side-copy edge drawn dashed (bytes moved
  inside S3, not over the wire).
- Honest labelling: edges that only have op counts (NFS clients → hub)
  are labelled ops/s; lean's edges say "no rate data"; an unreachable
  source greys its node and shows the last poll age and HTTP status.
- A raw-counters panel per source, so a number on an edge can be traced
  back to the field it came from.
- A **phase legend** on every view (§8): each phase word with its
  meaning, grouped into the four colour bands the dots and roll-ups use.
- `?demo=1` runs an in-page simulator instead of fetching, so the page
  can be opened anywhere (and published as a preview) with no backend.

Verification: the node tests; the mock server; then the real forge over
MinIO from WP2 with a push loop, opened in the page, and a headless
Chrome screenshot (`/Applications/Google Chrome.app` is present:
`--headless --screenshot`) as the artefact of the run.

### WP7 — The collector and its chart (new in revision 2)

A small in-cluster service, `flint-flow`, in its own crate and its own
chart. See §7 for why it is packaged this way.

- **Roster** from the API server: list `FlintShare` and `FlintRepo`
  objects (and lean workspaces by their pod label) on start and every
  30 s, via a watch. Phase, address and server id come from the CR;
  suspended, hibernated, failed and terminating sources are reported
  from the CR because there is no pod to poll.
- **Discovery by CRD presence:** at each roster refresh, ask which of the
  product CRDs exist and list only those. A product that is not installed
  is reported as `notInstalled` in the fleet header, never as an error.
- **Fan-out:** for each serving pod, `GET /status` on the pod IP at the
  product's status port (hub: the health port; forge: 9848, which the
  operator already binds on `0.0.0.0`), bounded concurrency (16), 1 s for
  the focused set the page asks for, 5 s for the rest, 2 s timeout. A
  timeout is `unreachable`, the collector's own verdict, distinct from
  any phase the component reports.
- **Aggregate endpoint:** `GET /fleet` returns every source's last
  document plus the collector's per-source metadata (polled-at, latency,
  verdict). `GET /source/<kind>/<ns>/<name>` returns one. Rates are
  computed by the page from consecutive samples, so the collector holds
  only the last two documents per source in memory. A restart costs one
  interval.
- **Serves the page** (static files) on the same listener, so the browser
  talks to one origin and no component needs a CORS header.
- **Posture:** ClusterIP only, reached by `kubectl port-forward`, the
  same posture as the status ports it reads. Never a LoadBalancer.
- **RBAC:** one ClusterRole: list/watch on `flintshares.chert.us`,
  `flintrepos.chert.us`, the lean workspace CRD, and pods. Rules naming a
  CRD that is not installed are legal, so one manifest serves any
  combination of products.
- **Build:** its own crate (`flow/collector`), dependencies limited to
  `kube`, `hyper`/`reqwest`, `serde_json`. It does not depend on the hub
  crate; the hub's status shape is consumed as JSON, not as a type.
- Tests: the roster with one, two and zero product CRDs present; a pod
  that times out is `unreachable` while its CR says `Ready`; a CR in
  `IdleSuspended` is never polled; the fan-out bound is observed (the
  memory-store pattern: a delay makes overlap observable).

## 4. Sequencing and effort

| Step | Package | Effort | Build cost |
|---|---|---|---|
| 1 | WP1 store meter + tests | ½ day | flint-store only (warm) |
| 2 | WP2 forge status section + MinIO check | ½ day | forge/syncer only (warm) |
| 3 | WP7 collector + chart + tests | 1 day | new small crate |
| 4 | WP6 page (fleet, group, detail) + node tests + mock | 1½ days | none |
| 5 | WP5 SPDK bytes, WP4 lean, optional hub meter | phase 2 | hub crate / design change |

After step 2 the git server ↔ S3 example works against a real forge over
MinIO with the page in manual-source mode. After step 4 the fleet works
on a real cluster with every installed product. No hub crate build is
on the path to step 4.

## 5. Decisions for the user, and risks

1. **Resolved in revision 2:** no CORS header on any status endpoint.
   The collector is same-origin with the page. The forge CORS line in
   WP2 is dropped.
2. **Counting semantics** (put bytes on attempt, get bytes on success)
   are stated in the module doc and in the page's raw panel. Either way
   is defensible; mixing them silently is not.
3. **Live vs snapshotted status.** Forge's `Facts` are a snapshot; the
   store section is read live. The document will therefore show a
   `serving` phase with bytes moving under it — correct, but a reader of
   `status.rs` must know the two halves have different freshness.
4. **Peer session overlap** in `forge/syncer` and `flint-store/memory.rs`
   (§WP2). Sequence WP1/WP2 with the peer, or wait for their commit.
5. **Never build the hub for WP1/WP2/WP6.** They do not need it, and the
   hub build is the disk-filling one.
6. **Not in scope:** NFS wire bytes (needs a counter in the RPC
   ingress/egress path of `nfs/`), Mountpoint metrics, forge door bytes,
   any push transport (SSE/WebSocket), and multi-cluster federation (one
   collector per cluster).
7. **The collector's namespace** (§7): the recommendation is the
   operators' namespace, because forge admits its status port only from
   there. Confirm that, or the forge operator needs a second admitted
   peer.

## 6. Navigation at fleet scale (100+ hubs and forges)

The single-source diagram stops working at about eight sources, so the
page has three views, chosen by count and joined by breadcrumbs
(fleet › namespace › source). Every view is a URL; browser back is the
history; nothing is stored anywhere.

- **Fleet** (default above eight sources): a table, not a diagram. Six
  roll-up tiles (sources by band, bytes to and from the store, NFS ops,
  git pushes, endpoints polled), one filter row (free text; kind; band;
  sort), then one row per hub, forge or lean workspace with phase, rate
  to store, rate from store, ops or pushes, backlog, epoch and last poll,
  each rate with a 60 s sparkline. Sorted by total rate so the busiest
  sit on top. Rows are virtualized, so 3,000 costs what 100 costs.
- **Group** (one namespace, bucket or label): the detail diagram with one
  box per component kind and a count ("hubs ×39"), every edge summed
  over the group, the largest contributor named on the edge and the top
  three in the side panel; the members table below is the fleet table
  filtered to the group.
- **Detail**: the single-source diagram at 1 s, reached by clicking a row
  or a contributor.
- **Polling budget:** the focused set at 1 s, the rest at 5 s. A hundred
  serving hubs at 5 s is 20 requests/s against a handler that does two
  local reads. A 3,000-workspace fleet with most of it suspended costs
  the same, because only serving pods are polled.
- **Rendering cost:** particles run only on the focused diagram; the
  fleet table is sparklines only.

### 6.1 How the views connect

Every view is a URL; view changes push browser history, filter and sort
edits replace it. Nothing is stored outside the tab, so a link opens
exactly what its author saw.

- **URL grammar.** `?view=fleet[&q=…&kind=…&band=…&sort=…]`;
  `?view=group&ns=…` or `&bucket=…&prefix=…` or `&label=…`;
  `?hub=ns/name`, `?repo=ns/name`, `?lean=ns/name` for detail;
  `?src=kind=url` (repeatable) for manual sources with no collector.
- **Fleet → group:** click a namespace in a row, a bucket in the store
  tile, or a label. **Group → fleet:** the breadcrumb, or the
  table/diagram toggle: the group view *is* the fleet filtered to one
  namespace, bucket prefix or label, plus the summed picture on top.
- **Fleet → detail:** click a row, press Enter on the focused row, or
  click a "top:" name in a roll-up tile. **Group → detail:** a member
  row, or a contributor on an edge or in the side panel.
- **Detail → group:** the breadcrumb, or the object-store box (group by
  bucket prefix). **Detail → detail:** ← and → page through sources in
  the table's current sort order; a side list shows the other sources in
  the same namespace.
- **Search:** `/` focuses the filter box on any view; typing narrows the
  fleet; Enter opens the first match's detail; Esc returns.
- **What carries across:** filters and sort (URL), the 60-sample rings
  per source (tab, kept at 5 s for every source so a detail view never
  opens with an empty sparkline), poll cadence, theme, and whether the
  legend is hidden. Nothing else.
- **Focus set:** whatever the current view shows is polled at 1 s (one
  source in detail, the members in group, the visible rows in fleet);
  everything else at 5 s. Scrolling the fleet table changes the 1 s set.
- **Entry points:** `/` opens the fleet, or the detail view when there is
  exactly one source; `/?hub=…` from the disk dashboard, a runbook or a
  chat message. Out-links: a source row offers a copyable
  `kubectl get flintshare -n … name`; the store box shows the bucket
  prefix as copyable text. The disk dashboard and the flow page link
  each other by URL only, with no chart dependency either way.

## 7. Packaging

Facts that decide it (read from the charts and operators):

- Seven charts, no umbrella, no dependencies between them. Each operator
  is cluster-scoped and watches all namespaces (`Api::all`, one
  `Controller` per operator), so there is one lite operator, one forge
  operator and one lean operator per cluster. "Multiple installs" means
  several products on one cluster, not several copies of one product.
- The disk dashboard ships inside `flint-csi-driver-chart` behind
  `dashboard.enabled` and is suppressed under the lite profile. A
  cross-product page inside one product chart vanishes whenever that
  product is not installed.
- Every product fences its status port with a NetworkPolicy. The lite
  hub chart admits the status/file-API port only to
  `networkPolicy.apiClientSelectors` / `apiClientCIDRs`. The forge
  operator's per-repo policy admits the status port only to the
  operator's own namespace, deliberately (`render.rs`: "the status port
  is deliberately NOT admitted here"). The lean chart admits its metrics
  port only from `metrics.networkPolicyNamespaces`, fail closed, and
  ships a PodMonitor selecting `chert.us/lean-workspace` across all
  namespaces. The gateway refuses to proxy `/status` at all.

Decision:

1. **Its own chart, `flint-flow-chart`, one release per cluster.** One
   Deployment (the collector, serving the page), one Service (ClusterIP),
   one ServiceAccount, one ClusterRole + binding. Versioned independently
   of the seven product charts. Not inside any product chart; no
   umbrella.
2. **Install it into the operators' namespace.** That is the namespace
   the product charts already trust for polling: forge admits it with no
   change; lite hubs need one values line adding the collector's pod
   selector to `apiClientSelectors`; lean needs
   `metrics.networkPolicyNamespaces` to include it. No product chart
   gains a component.
3. **Discovery by CRD presence, not by values.** The collector lists
   whatever product CRDs exist; nothing in its values names the products
   installed. Adding a product to a cluster adds it to the fleet view
   without touching the flow release.
4. **Version skew is tolerated, not managed.** Adapters ignore unknown
   fields and render absent sections as "no data". A hub at 1.45 and a
   forge at 0.2 render side by side; a forge that predates the WP2
   `store` section shows "no rate data" on its store edges.
5. **Same origin, no CORS.** The page is served by the collector. Reached
   by port-forward or ClusterIP only, matching the status ports.
6. **Multi-cluster is out of scope.** One collector per cluster; a
   federated view is a later concern and does not change this shape.

## 8. Mock review decisions (2026-09-05)

Recorded from the review of the scratch mocks (not in the repo):

- Light theme is the primary target; dark stays supported.
- Client boxes (git clients, NFS clients, agent pods, passthrough pods)
  are solid with a hairline, never dashed; dashed is reserved for edges
  with no data.
- **Passthrough is shown**, as a row whose edge reads "no rate data,
  Mountpoint ↔ S3 direct": the plugin launches Mountpoint with no
  metrics flag and has no status endpoint, and no Flint process is on
  that data path. Absence would read as an oversight; the row explains
  itself.
- The git clients → forge edge is **pushes per minute** over the 60 s
  window, not bytes; bytes on that edge would need the forge door
  instrumented.
- Edge colour is direction: blue to the store, orange from the store,
  aqua dashed for server-side copy, grey for ops-only, grey dashed for
  no data. Particle density and speed follow the rate on a log scale.
- A **phase legend** on every view, grouped into four bands: *serving*
  (serving, pushing, sweeping), *in progress* (starting, claiming,
  importing, reconciling, draining, released, pending), *scaled to zero*
  (idle-suspended, suspended, hibernated, terminating; from the CR),
  *needs attention* (fenced, failed, unreachable). Live phases are the
  component's own `/status` word; CR phases are used when there is no
  pod to poll; `unreachable` is the page's own verdict. A fenced server
  still answers `/status` and reports the fence itself.
- A raw-counters panel on the detail view, so every number on an edge is
  traceable to the field it came from.
- The page is stateless: last two documents per source and a 60-sample
  ring per edge, in the tab only.
