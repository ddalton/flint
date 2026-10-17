# Part parallelism for forge's pack uploads — 2026-10-17

**Question.** A pack over 64 MiB is a multipart upload, and the store
uploaded its parts one at a time: `FLINT_FORGE_FANOUT` spreads uploads
across packs, so a push whose critical path is one large pack got one
stream (runbw 2026-09-05: 10 GiB in 262 s, "161 serial 64 MiB parts").
Does uploading parts in parallel pay, and at what width and memory?

**Answer.** Yes: 8 parts with a 512 MiB byte gate uploads **6.1–7.1×
faster** than serial on both instance sizes, at 529 MiB peak RSS. It is
the syncer's default from this change
(`FLINT_FORGE_UPLOAD_PART_PARALLELISM=8`, `FLINT_FORGE_UPLOAD_INFLIGHT_MB=512`).

## Method

`examples/upload_bench.rs` runs one pack through forge's own
`packio::upload_file`, with the store built as the syncer builds it, so
arms differ only in the two variables. `run.sh` and `userdata.sh` are the
instance's one script.

- Two spot instances in us-west-1c: **i4i.xlarge** (4 vCPU), the size the
  serial numbers came from, and **i4i.4xlarge** (16 vCPU), to separate a
  CPU limit from a network one (lean saw 8-wide go *slower* on a 2-vCPU
  i4i.large, `lean/e2e/perf/results/publish-path-drill-2026-09-11.md`).
- Random 4 GiB and 10 GiB packs on instance-store NVMe, read once before
  the first run (git has just written a pack when the syncer uploads it).
- Five arms per pack size per round, in an order rotated by the round.
  Each object is deleted unread after its run; all traffic is ingress to
  a bucket in the same region.
- **The width as it ran:** each run samples the bench's established :443
  connections every second and records the peak, so an arm that showed no
  gain could be told from an arm that never widened.

Two of the planned three rounds ran in full (44 runs, every one `rc=0`).
The third was stopped early: the rounds agreed to within 7% per cell, and
the effect is about 6–7× (by cell mean).

## Results (MiB/s; the range is over the two rounds)

| arm | parts | gate | i4i.xlarge 4 G | i4i.xlarge 10 G | i4i.4xlarge 4 G | i4i.4xlarge 10 G | conns | peak RSS |
|---|---|---|---|---|---|---|---|---|
| A (old default) | 1 | none | 88.5–88.8 | 86.7–89.6 | 88.8–89.2 | 88.7–89.3 | 1 | 79 MiB |
| B | 4 | 256 MiB | 313.7–317.4 | 342.0–342.1 | 317.3–338.5 | 345.8–349.3 | 4 | 272 MiB |
| **C (new default)** | **8** | **512 MiB** | **537.0–545.1** | **586.4–596.8** | **577.8–579.2** | **628.8–637.6** | **8** | **529 MiB** |
| D | 16 | 1 GiB | 573.0–608.4 | 647.5–649.0 | 726.9–739.8 | 808.6–826.7 | 16 | 1.04 GiB |
| E (lean's pair) | 8 | 256 MiB | 312.9–334.1 | 325.0–342.9 | 320.2–340.4 | 347.7–347.8 | **4** | 272 MiB |

## What it says

1. **Serial is one stream's speed, about 89 MiB/s, on both sizes.** This
   was the bound on any large push.
2. **The gate must be sized to the width.** It counts each 64 MiB part,
   so a 256 MiB gate admits 4 parts whatever the width: arm E (lean's
   shipped `FLINT_SYNC_UPLOAD_PART_PARALLELISM=8` /
   `FLINT_SYNC_UPLOAD_INFLIGHT_MB=256`) ran 4 connections wide and matched
   arm B. **Lean's own default has the same clamp; not changed here.**
3. **16 parts is CPU-bound on 4 vCPU.** On i4i.xlarge it adds 8–10% over
   8 for twice the memory; on i4i.4xlarge it adds 27–29%. 8 is the default
   because it is nearly all of the gain on the small size at half the
   memory; 16 is worth setting on large nodes.
4. **Memory is exact:** peak RSS is the gate plus 16–19 MiB in every gated
   run. The gate does not cover whole-PUT bodies (packs at or under
   64 MiB, `FANOUT` of them), so the syncer's worst case is the gate plus
   4 × 64 MiB. The operator sets a 32 Mi request and no limit on the
   syncer (`spdk-csi-driver/src/forge_operator/render.rs`), so this cannot
   OOM-kill it; the request understates a large push, as it already did.

## Not measured

- An end-to-end push through the door and a cluster: the upload is the
  part this changes, and the rest of a push (git's transfer and
  index-pack) is unchanged.
- Restore: it already fetches ranged GETs in parallel (~300 MiB/s on runbx).
- A slow link: a smoke test from a home Wi-Fi uplink (~6 MiB/s) hit an
  intermittent `upload_part: dispatch failure` on a 130 MiB pack at
  `par=1` and at `par=4` (it passed on retry). It is not the store's 10 s
  `read_timeout`: a 64 MiB part that took about 11 s to send went through.
  Open; it matters only to a syncer outside a cloud region.
