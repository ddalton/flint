# The publish-path drill, runcs — what the cluster said

Cluster `runcs`, 2 x i4i.large **all-spot including the control plane**,
us-west-1, one AZ. Bucket `flint-publish-drill-20260911`. n=3, arms
interleaved within each rep. Everything under test shipped earlier the
same day on local tests and mutation controls only.

**TWO OF MY OWN CONTROLS CAME BACK VACUOUS AND BOTH HAD TO BE REDONE.**
That is the most useful thing in this file, so it is at the top rather
than in a footnote.

---

## L1 — the publish reads each file ONCE. Confirmed.

| workload | tree | single (HEAD) | prepass (old code) | speedup | rchar single -> prepass |
|---|---|---|---|---|---|
| **big** 6x1 GiB | 6,442,450,944 | **22.43-23.11 s** | **35.64-36.62 s** | **1.55-1.63x** | 1.0003x -> **2.0003x** |
| **mixed** 4 GiB+2k | 4,303,159,296 | **17.88-20.32 s** | **27.37-28.65 s** | **1.41-1.51x** | 1.0005x -> **1.9986x** |
| **small** 20k x 8 KiB | 163,840,000 | 21.00-21.32 s | 20.89-21.24 s | **none** | 1.00x -> 1.00x |

Ranges do not overlap on `big` or `mixed`. **~13.2 s saved per 6 GiB,
~9 s per 4 GiB.**

`small` is the arm that makes the other two mean anything: files at or
under `whole_put_max` never reach `upload_compose` at all, so it MUST NOT
move — and across three reps it does not, on either counter. Had it
moved, `big` and `mixed` would have been weather.

**The mechanism is NOT what the estimate claimed.** The pre-pass was
described as ~11 s at the disk's read ceiling. `read_bytes` is
IDENTICAL across the two arms (6.458e9 vs 6.458e9), so the second read
never reached the disk — it came from page cache, and it still cost
13 seconds. That is the redundant CRC hash: ~6.4 GB at roughly 500 MB/s
on 2 vCPUs. **The win is CPU, not I/O**, so it holds even where the tree
fits in RAM, and it scales with core speed rather than disk speed.

### Two rig fixes without which L1 says nothing

1. **Drop the page cache before every timed run.** The seed had just
   written these files; without dropping, the FIRST read is cached too
   and neither arm shows anything.
2. **`rchar`, not `read_bytes`.** `read_bytes` counts what came off the
   BLOCK DEVICE, and the pre-pass's second read is served from cache —
   so the guard reported ratio **1.00** and announced "the arms did not
   differ" about two binaries that differ exactly as designed. `rchar`
   counts bytes returned by `read(2)` regardless of origin, which is the
   actual question: *did the code read the file twice?* It answers
   2.0003 and 1.9986.

The guard also cried wolf at its own null control, because it expected
2x everywhere. It now takes the expected ratio per workload. A guard
that fails on a passing control is one people learn to ignore.

---

## L2 — parallel part upload with a store-computed checksum

Correctness: **VERIFIED** (see L2c). Performance: **it is SLOWER here.**

| arm | wall clock (n=3) |
|---|---|
| `part_parallelism=1` | **21.31-23.78 s** |
| `part_parallelism=8` | **27.29-30.88 s** |

**8-way part upload is 1.22-1.36x SLOWER than sequential on i4i.large.**
Two vCPUs have to run eight concurrent TLS streams AND eight concurrent
part hashes; the box is CPU-bound long before the network is the limit.

The honest reading: `crc64_combine` removed a CONSTRAINT — a
store-computed checksum no longer forces sequential uploads — but on
this instance size the freed option is not worth taking. The constraint
was still worth removing (it was false, and it was the reason lean paid
for a second read), but `part_parallelism > 1` should not be a default
on small instances. Whether it pays on a bigger box is UNMEASURED.

## L2c — and the first version of it was VACUOUS

L2's entire oracle is "S3 refuses a mis-folded checksum at
CompleteMultipartUpload, so a publish that completed is a fold that was
right." That is worth nothing unless S3 actually validates.

The first attempt passed `--checksum-crc64nvme`. The AWS CLI's parameter
is `--checksum-crc64-nvme`. The CLI rejected the flag, exited non-zero,
and the guard — written as `if complete; then FAIL else OK` — read that
as **"S3 rejected the checksum"** and printed `guard OK`. An argument
error was reported as a security property. *An error must not return a
legal value*, and my own control did exactly that.

Re-run with the right flag:

```
An error occurred (BadDigest) when calling the CompleteMultipartUpload
operation: The crc64nvme you specified did not match the calculated checksum.
HeadObject -> 404 Not Found
```

S3 validates, and the object does not land. L2's oracle stands.

---

## L3 — scoped checkout, 3 of 2001

| arm | files materialised | bytes | checkout wall clock |
|---|---|---|---|
| unscoped | **2001** | 2,107,638,093 | **11.24-11.61 s** |
| scoped `inputs` | **3** | 12,582,912 | **0.48-0.55 s** |
| wide `inputs,out1.dat` | **4** | 13,631,488 | 0.49-0.53 s |

**167x less data, ~22x faster.** (File counts above exclude the control
file each tree also carries.)

**The safety claim, on a real bucket.** Two barriers from the 3-of-2001
workspace:

```
flint-sync: barrier seq=Some(1) up=0 del=0 parked=0 consumed=0
flint-sync: barrier seq=Some(1) up=0 del=0 parked=0 consumed=0
Total Objects: 2001   Total Size: 2107637760
```

`del=0` twice — TWO barriers, because a deletion must survive two
consecutive scans and one barrier would have passed for the wrong
reason. The 1,998 citations the workspace never materialised are intact.

**Caveat on `wide`, honestly.** It was described as "a scope admitting
EVERYTHING must equal the unscoped arm", which is what separates a
working filter from one that always yields nothing. As configured it
admits 4 of 2001, not all 2001. It does show the filter TRACKS the scope
(3 -> 4 as the scope grows), which rules out always-empty — but it is
weaker than the control described, and the stronger one was not run.

---

## L4 — the cross-key copy against REAL S3

Both arms PASS, and the binary now NAMES the arm rather than leaving it
to be inferred:

```
probe-copy ceiling=5368709120 bytes -> the CopyObject (payload is tiny) arm   PASS
probe-copy ceiling=0 bytes          -> the MPU + UploadPartCopy arm           PASS
```

Each pass covers all five checks: a stale copy-source etag REFUSES and
lands nothing; the copy is byte- and checksum-identical; **the
destination's stamps are its own** (`MetadataDirective: REPLACE` is a
server behaviour no memory double can verify); the source survives; an
occupied destination refuses.

The MPU run is `ComposeSpec::base_key`'s first execution anywhere —
`base_key: Some(..)` appeared nowhere in the repository before today.

**AND THE FIRST L4b WAS VACUOUS.** It ran with
`FLINT_SYNC_COPY_WHOLE_MAX_MB=1`, and the probe's payload is a few dozen
bytes — far under a 1 MiB ceiling — so it took the CopyObject path and
re-ran the arm already tested, reporting PASS. The ceiling has to be
**0** for a tiny payload to reach MPU. Hence the binary now prints which
arm it is about to take: an arm you cannot see is an arm you cannot
trust.

**Caveat:** the MPU arm exercised a ONE-PART MPU. It proves
`create_multipart_upload` + `upload_part_copy` + `base_key` + complete
work end to end against S3; it does NOT exercise a multi-part copy, and
a >5 GiB copy remains unmeasured.

---

## What this drill did NOT cover

The conflict-log rotation (pod-local; local tests are the right level),
the scoped-read safety claim beyond two barriers, `part_parallelism` on
anything larger than i4i.large, a multi-part cross-key copy, and
anything under node loss or spot reclaim.
