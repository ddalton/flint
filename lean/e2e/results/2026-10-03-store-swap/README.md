# 2026-10-03: B1-B25 and C1-C12 on two stores, after re-deriving 16 legs

Kind on the Linux box (one node per run, private kubeconfig). The images
were built from origin/main `56930b6d`: `flint-sync:e2e` and
`flint-lean-gateway:e2e` (x86_64 musl, `--features s3`), and
`flint-sync:predo` from `69b35978` for B14. The stores were
`cgr.dev/chainguard/minio:latest` (`minio.yaml`) and `rustfs/rustfs:latest`
(`rustfs.yaml`). The drills read which store answered and print it.

| run | log | result |
|---|---|---|
| verbs on MinIO | `verbs-minio.log` | 15 passed, 1 failed (B8, see below) |
| B8 alone on MinIO, fixture fixed | `verbs-minio-B8-rerun.log` | 1 passed |
| verbs on RustFS | `verbs-rustfs.log` | 15 passed, 0 failed, 1 skipped (B8, by name: it needs MinIO's admin trace) |
| chaos on MinIO | `chaos-minio.log` | 12 passed |
| chaos on RustFS | `chaos-rustfs.log` | 12 passed |

Another session's kind cluster (`flint-probe`, s3csi) ran beside these
runs until 22:40Z. No timing-sensitive leg failed, so none was rerun for
that.

## B8 in the full run

B8's workspace had never published, so neither the pointer nor the legacy
`manifest` existed. Its idle tick was 6 requests, all 404s:

- 3 GETs of `current`;
- the inbox GET;
- a GET and a HEAD of the absent `manifest`.

The new shape check counted the legacy GET as "the document is fetched
again". The fixture now publishes once before each measured window. On a
published workspace the idle tick is **2 requests**: the inbox GET and the
pointer GET. There is no lease traffic, no legacy-manifest read and no
entries or chunk fetch, and sentinels on add nothing (delta 0).

The trace also shows that the never-published tick reads `current` three
times. That is minor, but it contradicts the comment at the skip-on-no-diff
site ("The consume just read it: no second GET").

## How the suites got here (from 3/16 and 2/12)

The first runs of the day ran with Chainguard's MinIO image. On MinIO the
verbs suite passed 3 of 16 legs and the chaos suite 2 of 12. On RustFS the
verbs suite refused to start (no `Server` header), and chaos also passed
2 of 12. No product defect was found. The suites were stale in five ways:

1. **The pointer layout.** The manifest reader knew only `entries_key`; the
   syncer writes the CHUNKED layout. Every manifest read was empty.
   Fixed in `3a7fd8c2`, together with a store that pulls, the roster and
   the exec bit.
2. **Handles.** Files live at `files/<path>@<flush>`. The suites read the
   bare `files/<path>`, which `mc cat` cannot find and which `mc stat`
   finds BY PREFIX whenever any handle exists, retired ones included. On
   the box, a stat of a missing bare key gave rc 0 beside `b.txt@u1` and
   rc 1 with no handle. Files are now read through the citation (`fcat`,
   `cited`), and objects are checked by an exact listing of the path's
   handles (`haskey`).
3. **The per-barrier lease.** There is no lease record until the first
   commit and no idle renewal, and a fence is a retry: there is no
   `refused-fenced` ack and no `fenced` state. B8, B17 and B18 were
   re-derived; B18 and B17 now assert clean handoffs and no deposal from
   the claim trace (`FLINT_SYNC_EVENT_TRACE=1`).
4. **P2.** A UI write commits. `window/open`, `window/clear` and
   `inbox/drop` are gone, and an overwrite needs `If-Match` (428
   otherwise). C5, C6, C7 and C11 were re-derived. C7 is now the
   opposite of its old claim: a save during a HELD commit is admitted
   at once, and both files end up cited.
5. **Retire grace.** An un-cited handle stays 600 s and is collected at a
   later commit. The collector runs on every store under handles,
   conditional DELETE or not. C4, C10 and C11 assert that the handle
   survives inside the grace and is collected by a commit run with the
   grace at 0.

Rig fixes:

- **Upload serialization.** `FLINT_SYNC_FANOUT` now bounds fetches only,
  so `verbs.yaml` sets `FLINT_SYNC_UPLOAD_FANOUT=1` for B2's and B18's
  mid-upload windows.
- **C8 restores the gateway on every exit.** A failing C8 used to leave it
  at zero replicas, and C11 then failed with curl's `000`.
- **The dangling check** is an exact jq set difference. It fails on an
  unreadable manifest, and it names the missing keys.
- **B13's baseline plant** works against the compact JSON the baseline
  has been written in since `73e34dfc`.

The C1/C3 "dangling" keys of the third attempt existed. A stat found
every one, and an 8000-key listing survived `kubectl exec` intact three
times. The comparison was `sort | comm` under uutils on en_US.UTF-8, and
`comm` complained that `sort`'s own output was out of order. That was
never reproduced in isolation, and the check no longer sorts.
