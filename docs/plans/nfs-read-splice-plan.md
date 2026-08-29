# NFS READ path: `splice()` plan

Status: **PLAN ONLY — no code.** Written 2026-08-28, after the buffer-pool
work (`b1c1e351`, `5863468e`) took the READ path from
`vec![0u8; count]`-per-request to a pooled, zero-copy `Bytes`.

`splice()` is the last remaining lever on the read path (~52% of flint's
CPU at the last profile). It is also the most dangerous change proposed
so far, because it moves file content to the wire **without the server
ever seeing the bytes** — which silently removes the ability to retract,
checksum, or cache a reply.

This plan exists because the two previous perf attempts in this area both
went wrong in ways a plan would have caught:

- the first segmentation fix measured **0.989x — a null result** — because
  the dispatcher's trial encode produced `raw_reply`, so the RPC layer
  short-circuited and the new path was inert;
- a gate run **PASSED with a flattering ratio** because a runaway
  `systemd-logind` was eating one of the VM's two cores.

So: prove the mechanism before wiring it, and never trust a single
favourable number.

---

## 1. What the copies are today

| # | Step | Copy? |
|---|---|---|
| 1 | `file.read_at(buf, offset)` — `ioops.rs:1648` | page cache -> user buffer |
| 2 | `Bytes::from_owner` — `read_pool.rs:110` | none (pooled, zero-copy) |
| 3 | `encode_segments()` — payload becomes its own segment | none |
| 4 | `frame_reply()` — header segment + body segments | none (non-GSS) |
| 5 | `write_all(seg)` — `back_channel.rs:191` | user buffer -> socket |

Two copies remain: **1 and 5**. `splice()` removes both by moving page
references through a pipe: `file -> pipe -> socket`, never entering
userspace.

## 2. Constraints, each anchored to code

| # | Constraint | Anchor | Consequence |
|---|---|---|---|
| C1 | GSS binds a MIC over / seals the body as one octet stream | `server_v4.rs` `seal_reply_body` | splice only when `gss.is_none()` |
| C2 | Slot reply cache stores the bytes (`bytes.to_vec()`) | `dispatcher.rs:231` | splice only when `ctx.cache_slot.is_none()` |
| C3 | The writer is a **`BufWriter`**, not a raw fd | `back_channel.rs:98` | must `flush()` before splicing, and hold the same `Mutex` across the whole frame |
| C4 | CB frames share that writer | `back_channel.rs:178` | a slow client now blocks CB_LAYOUTRECALL for longer than `write_all` did |
| C5 | Tier re-consult runs **after** the read and can turn it into DELAY | `ioops.rs:1666` `read_window_intact` | **must stay retractable** — see below |
| C6 | Record marker needs total length up front; payload needs XDR padding | `back_channel.rs:186` | layout is `[marker][hdr+prefix][payload][pad 0-3][suffix]` |
| C7 | `splice()` is Linux-only; dev machine is macOS | — | `#[cfg(target_os = "linux")]` + fallback; **all legs run in lima** |
| C8 | Socket is non-blocking; splice returns partial counts | — | loop on `AsyncFd` readiness |
| C9 | DS path keeps aligned O_DIRECT buffers | `pnfs/ds/io.rs` | excluded from v1 |
| C10 | A pipe costs 2 fds; default capacity is 64 KiB vs 1 MiB rsize | — | bounded pipe pool + `F_SETPIPE_SZ`, mirroring `read_pool` |

### C5 is the one that decides the design

`read_window_intact` exists because a tier eviction can land *between*
the consult and the `pread`; without the re-check the server serves a
truncated stub as if it were file content. It was caught live by the
chaos drill — git once read an empty `.git/config`.

If we splice **file -> socket** directly, the bytes are on the wire
before that check can run, and it becomes unenforceable: a data-
correctness regression, not a performance one.

Therefore the design is mandatory, not preferred:

    file --splice--> pipe        (retractable: nothing is on the wire yet)
    run read_window_intact
      fail -> drain pipe, answer DELAY   (identical to today)
      pass -> pipe --splice--> socket

The pipe is the staging area that keeps the existing correctness
guarantee intact.

## 3. Staged landing — nothing defaults on until measured

**S0 — Prove the mechanism, outside flint. GO/NO-GO gate.**
A standalone lima microbench: `pread`+`write` vs `splice` on the same
file and socket, 2 vCPU aarch64 VZ guest, page-cache-warm and cold.
If splice does not beat the current path by a margin larger than the
rig's spread (~10%), **the plan stops here** and no flint code is
written. This is the step that would have killed the 0.989x null before
it cost a day.

Also establishes the **size threshold** — two syscalls plus pipe
management is a loss for small reads; find where it crosses over.

**S1 — Pipe pool + splice helper, Linux-only, unit-tested, NOT wired in.**

**S2 — Wire into the MDS READ path behind `FLINT_NFS_SPLICE=1`, default OFF.**

**S3 — Differential A/B**, flag on vs off, interleaved paired reps, with
the identity guard and RPC-counter guard the rig already carries — plus a
**falsifiability leg**: flag on with the size threshold forced above
rsize must measure ~1.0. If it does not, the harness is lying.

> **AMENDED BY S0 — the gate metric must change.** S0 measured a 72% CPU
> reduction that produced only a **21% MiB/s** gain, and in 2 of 5 reps
> splice's wall time was *worse* than the baseline's. The existing gate
> scores **MiB/s ratios**, so gating splice on throughput would very
> likely score ~1.0 and be read as "no effect" — the same shape as the
> 0.989x null. S3 must therefore score **cpu-ms/GiB under CONCURRENT
> readers**, which is where the win cashes out (same axis as the
> `mmap_lock` bug: flint 520->600 cpu-ms/GiB from 1 to 8 streams while
> knfsd went 280->235). Single-stream MiB/s is the wrong instrument.

**S4 — Conformance with the flag ON**, both binaries: pynfs (171/0/91),
nfstest, on Linux as root.

**S5 — Default ON** only if S3 shows a real gain and S4 is clean.

## 4. Regression surface, and the guard for each

Every leg below is **mutation-checked**: it must be demonstrated to fail
with its guard removed, or it is not evidence.

| Risk | Leg |
|---|---|
| C2 replay returns wrong bytes | `cachethis=true` READ takes the copy path; replay is byte-identical |
| C1 krb5i/krb5p corruption | GSS READ takes the copy path |
| **C5 evicted stub served as content** | eviction mid-read -> DELAY, and **no payload bytes reach the wire** |
| C3 foreign bytes in a frame | reuse the existing `pipeline.rs:450` oracle |
| C4 CB frame interleaving | CB_LAYOUTRECALL concurrent with a spliced READ |
| C6 wrong length / missing pad | short read at EOF; COMPOUND with ops after READ |
| C8 stream desync | client disconnect mid-splice -> **connection closed**, never `Err` back into the loop |
| C7 build break | macOS builds and uses the fallback |

**C8 deserves emphasis:** once the record marker is on the wire, a
half-written frame cannot be recovered. Every error path after that
point must terminate the connection. Returning an error into the read
loop would desync the stream permanently and look like random client
corruption much later.

## 5. What would make me abandon this

- S0 shows no margin on this hardware.
- The threshold turns out to be above typical rsize (splice would never fire).
- Holding the writer `Mutex` across a splice measurably starves the back channel.

A null result at S0 is a **cheap success**, not a failure — it costs a
microbench instead of a subtly-corrupting read path.


---

## 6. S0 RESULT — 2026-08-28: **GO**

Standalone C microbench in `flint-nfs-client` (2 vCPU aarch64 VZ guest),
page-cache-warm, ephemeral port, `RUSAGE_THREAD` so the drain thread's CPU
is excluded. VM **100% idle before and after, load 0.00** — no repeat of
the `systemd-logind` contention that once produced a flattering ratio.
5 reps, arms interleaved per rep, paired per-rep ratios.

| chunk | A cpu-ns/MiB | B cpu-ns/MiB | **B/A** | spread | C/A | A MiB/s | B MiB/s |
|---|---|---|---|---|---|---|---|
| 4 KiB | 811312 | 593750 | 0.700 | 22.3% | 1.001 | 1237 | 1656 |
| 16 KiB | 343750 | 103188 | **0.300** | 29.0% | 1.034 | 2926 | 7608 |
| 64 KiB | 218781 | 73156 | **0.334** | 28.8% | 1.076 | 4406 | 7875 |
| 256 KiB | 210219 | 60547 | **0.284** | 27.3% | 1.060 | 4585 | 6743 |
| 1 MiB | 187516 | 53078 | **0.283** | 22.5% | 1.095 | 5333 | 6468 |

- **splice uses ~28% of the CPU at rsize — a 72% reduction**, stable from
  16 KiB up and far outside the spread.
- **Falsifiability arm C passes** at 16 KiB and above (C/A 1.03-1.10):
  the rig resolves one extra copy, so the B/A numbers mean something.
  At 4 KiB C/A = 1.001 — a 4 KiB memcpy is genuinely negligible next to
  syscall overhead, so this is physics, not a broken rig. The 4 KiB
  B/A=0.700 is still a real win but is the least trustworthy row.
- **No size threshold is required** — splice wins at every size measured.
  C10's pipe pool can therefore skip threshold logic in v1.
- **CPU and throughput diverge**: 72% CPU saved, only 21% MiB/s gained.
  See the S3 amendment above; this is the single most important result
  of S0 and it changes how the change must be gated.

### New constraint discovered by S0

The C5 design stages the **whole payload** in the pipe before
`read_window_intact` runs, so pipe capacity must be >= rsize.
`F_SETPIPE_SZ` to 1 MiB succeeded here, but `/proc/sys/fs/pipe-max-size`
defaults to 1 MiB, and a 64-entry pool at 1 MiB each pins **64 MiB** of
kernel pipe buffers. S1 must check the `F_SETPIPE_SZ` return value and
fall back to the copy path when capacity < payload, rather than
silently splicing in fragments and defeating the retract check.

---

## 7. S2 approach — worked up 2026-08-28

S1 shipped `nfs::splice` (pipe pool + retractable `Staged`), 9 tests green on
real Linux, 3 invariants mutation-checked. S2 wires it in. Every claim below
was checked against code, not assumed.

### The seam already exists

`compound.rs::encode_segments` (line ~2254) already cuts the READ payload
into its OWN segment (`segs.push(res.data)`) — a by-product of the earlier
segmentation work. So S2 does not need a new data path, only a wider
segment type.

### Four blockers

**B1 — `results.clone()` in the size check (`dispatcher.rs` ~657).**
`Staged` owns a pipe and cannot be `Clone`, so a piped payload cannot live
inside `OperationResult` while that clone exists. The clone only exists so
the oversize path can rebuild a stripped reply, and that path uses just
`results[0]` (the SEQUENCE result). **Fix: clone only the first result.**
Independently valuable — it also drops a whole-Vec clone from every
session-bound compound.

**B2 — the writer type. (REVISED 2026-08-28 — the earlier design here was
wrong and has been replaced.)**

`drain_to` needs a `&TcpStream` for `writable()`/`try_io()`, and the
connection uses `stream.into_split()` into `BufWriter<OwnedWriteHalf>`
(`server_v4.rs:475`, `back_channel.rs:98`).

It is true that tokio 1.53.1 has **no `impl AsyncWrite for &TcpStream`** —
but that was the wrong thing to check. What is needed is a `&TcpStream` for
READINESS, and tokio already provides it:
`impl AsRef<TcpStream> for OwnedWriteHalf`
(`tokio/src/net/tcp/split_owned.rs:503`), plus `OwnedWriteHalf::writable()`
and `try_write()`/`try_write_vectored()` on `&self`.

So **no refactor is required.** Compile-verified:

```rust
w.flush().await?;                             // MUST precede any splice
let sock: &TcpStream = w.get_ref().as_ref();  // from the EXISTING BufWriter
sock.writable().await?;
sock.try_io(Interest::WRITABLE, || /* splice(pipe_r, .., sock_fd, ..) */ )?;
```

> **The `Arc<TcpStream>` design previously written here is WITHDRAWN.** It
> would have replaced `into_split()`, and `OwnedWriteHalf::drop` shuts the
> write half down (`split_owned.rs:452`) while an `Arc<TcpStream>` does not
> — silently removing the mechanism that makes the peer see FIN promptly.
> `server_v4.rs`'s teardown comment records F18, where a lingering strong
> `Arc` pinned the fd, gave permanent CLOSE_WAIT, and pegged two runtime
> workers at ~60% CPU / 83% sys. Keeping `into_split()` avoids that risk
> entirely rather than mitigating it. **This was the largest risk in the
> S2 plan and it is now gone.**

Runtime alternatives (monoio, glommio, tokio-uring) were considered and
REJECTED: they are thread-per-core and completion-based, so `AsyncRead`/
`AsyncWrite` do not exist and buffers pass by ownership — a rewrite of the
server's I/O layer. The measured cost here is memcpy, not reactor
dispatch, and their headline 2-3x figures are echo/accept benchmarks
dominated by per-connection syscall overhead, which does not describe a
1 MiB READ.

**B3 — spliceability must be decided BEFORE the READ executes.** GSS
(C1) and `cachethis` (C2) are known at the RPC / SEQUENCE layer, but the
fetch decision happens down in `ioops`. `context.cache_slot` is set by
SEQUENCE, which is op 0, so it IS already available. **Fix: a `can_splice`
flag on `CompoundContext`** = plain TCP (no GSS) && `cache_slot.is_none()`
&& `FLINT_NFS_SPLICE` && MDS path. `ioops` consults it and otherwise takes
the pooled-buffer path unchanged.

**B4 — widen the segment type.** `Vec<Bytes>` -> `Vec<Segment>` where
`Segment = Mem(Bytes) | Piped(Staged)`, through `encode_segments` ->
`CompoundResponse::raw_reply` -> `dispatch_nfsv4` -> `frame_reply` ->
`send_record_segments`. `replay_reply` stays `Bytes` (a cached replay is by
definition already materialised).

### A shortcut I recommend AGAINST

A dedicated fast path for the common `[SEQUENCE, PUTFH, READ]` shape would
confine the change — but this codebase has already paid for a second
framing path. `frame_reply`'s own doc records NULL answering GARBAGE_ARGS
on the GSS path because two paths had drifted, and the fix was to give them
ONE framing function. Adding a second reply-assembly path recreates exactly
that failure mode. Take the general change.

### Ordering: three byte-identical refactors, then one gated change

| step | change | how it is proven |
|---|---|---|
| S2a | de-clone the size check (B1) | existing suites + pynfs CSESS26 (the oversize gate) |
| S2b | ~~`Arc<TcpStream>` writer~~ **DROPPED — B2 needs no refactor** | n/a |
| S2c | `Segment` enum, `Mem` only (B4), **no splice** | byte-identical |
| S2d | `can_splice` + staging + `Piped` (B3) | `FLINT_NFS_SPLICE`, **default OFF** |

Only S2d changes behaviour, and it is off by default. S2a-c must measure
and behave identically — if any of them moves a byte, stop.

### Regression legs specific to S2

- flush the `BufWriter` before every splice, else bytes interleave mid-frame
- a CB frame concurrent with a spliced READ (hold the mutex across the frame)
- any error after the record marker is on the wire must CLOSE the connection
- the oversize path must RETRACT the staged payload (it discards the READ anyway)
- `cachethis` and GSS must never splice
- non-Linux must never construct `Piped`
- ~~teardown still shuts the write half down~~ — moot: `into_split()` is kept


---

## 8. S3 RESULT — 2026-08-28: **splice costs 36% of the CPU in-server**

`tests/lima/pnfs/splice-differential.sh`. One release binary, two units on
private ports, `FLINT_NFS_SPLICE` toggled. 4 concurrent readers, client
`O_DIRECT`, cache-warm, 8 passes per measurement, arms interleaved per rep.

| arm | cpu-ms/GiB |
|---|---|
| off (copy path) | ~300 |
| **on (splice)** | **110** |

Two runs, 10 paired reps: **median ratio 0.358**, range 0.344-0.397.
**2.8x less server CPU per byte.**

### All four guards passed — and each exists because of a past failure

- **CORRECTNESS** — md5 of the file on disk vs served through the mount,
  plus exact byte counts, both arms. Added after noticing the rig read to
  `/dev/null` and so was structurally blind to wrong bytes: a path serving
  short or garbled reads would have scored as FAST. Every failure mode
  this design guards against produces bad data, not slow data.
- **EXECUTION** — pipe fds in `/proc/PID/fd`: **off 0, on 18**. A pipe is
  created only by the splice pool, so this is a direct observation of the
  mechanism. Without it, "both arms measure the same" is
  indistinguishable from "the path never fired" — the 0.989x null.
- **IDENTITY** — a `WHICH_ARM` marker read back through each mount. Two
  units once bound the same port and a mount silently attached to the
  wrong server.
- **RIG HEALTH** — VM idle before and after. A runaway process once made
  a gate PASS with a flattering ratio.

### Consistent with S0, and not identical to it

S0's standalone microbench said 0.283; in-server is 0.358. The gap is the
work splice does not touch — XDR encoding, state lookups, the tier
consult. A number that matched S0 exactly would have been MORE suspicious.

### What is NOT claimed

- **No knfsd comparison.** Recorded knfsd figures (210-280 cpu-ms/GiB)
  come from a different session, build profile and workload. Comparing
  cpu-ms/GiB across sessions already misled once this month, when knfsd's
  own number moved 560 -> 400 between runs. That needs a knfsd arm in
  THIS rig.
- **One configuration.** 4 readers, 64 MiB files, warm cache, 2 vCPU,
  O_DIRECT. Not a general statement.

### Harness bug worth remembering

`setup_arm` stopped the server BEFORE unmounting, which leaves an NFS
mount pointing at a dead server — `umount -f` then blocks in D-state
forever (observed: 286s, load 1.00 at 0% CPU). **`umount -l` before
stopping the server** is the only safe order, and the script now also
unmounts on EXIT. It surfaced only on the third run, because runs 1-2
silently left mounts behind.


---

## 9. THROUGHPUT — measured 2026-08-28, and it corrects an earlier claim

S3's first runs scored CPU only. Asked for throughput, the rig was
extended to record wall time per rep. Third independent run, same guards:

| arm | wall for 2 GiB | MiB/s |
|---|---|---|
| off (copy) | 656-685 ms | ~3000-3120 |
| **on (splice)** | **382-431 ms** | **~4750-5360** |

**Median speedup 1.59x — +59% throughput**, spread ~12%. CPU reproduced
exactly (220 ms on, 590-610 off), so both axes are stable.

### This corrects the framing used earlier in this plan

Sections 6 and 8 argue "the win is CPU, NOT throughput; a MiB/s gate
would read ~1.0 and be called no-effect". **That is true of S0 and FALSE
of the server.** S0's standalone bench had one sender thread and a fast
drain, so the server was never the constraint; extrapolating its
bottleneck to the real server was the error. With 4 concurrent readers
on 2 vCPUs the server IS CPU-bound, so freed CPU converts almost
directly into throughput.

Scoring on cpu-ms/GiB remains the right choice — it is the metric that
still means something when the bottleneck moves elsewhere, e.g. more
cores or a slower network. But the claim that a throughput gate would be
UNINFORMATIVE here was overstated: it would have shown +59%.


---

## 10. vs knfsd — measured 2026-08-28, one session, one kernel

`tests/lima/pnfs/splice-vs-knfsd.sh`. Three arms interleaved per rep,
identical workload (4 readers, 64 MiB files, warm, O_DIRECT, 8 passes).

| arm | cpu-ms/GiB | MiB/s | CPU vs knfsd | tput vs knfsd |
|---|---|---|---|---|
| flint, copy path | 495 | 3195 | 1.83x | 56% |
| **flint, splice** | **290** | **4935** | **1.07x** | **86%** |
| knfsd | 270 | 5753 | — | — |

**Splice takes the CPU gap to knfsd from 1.83x to 1.07x, and throughput
from 56% to 86% of it.**

### Why the metric is total system CPU, not per-process

S3 read `/proc/PID/stat`, which is BLIND to knfsd: its work is in kernel
threads and softirq, not in any process the rig owns. Scoring flint's
process CPU against knfsd measured that way would flatter flint by
construction. Total system busy CPU (`/proc/stat`, minus idle and
iowait) is the only number that means the same thing for an in-kernel
server and a userspace one.

### That choice UNDERSTATES the remaining gap — do not quote 1.07x alone

The client's cost (dd, NFS client, TCP, softirq) is counted in every
arm, so it dilutes the differences. Cross-checking against S3's
per-process figures (110 splice / 300 copy) implies a client share of
~180-195 cpu-ms/GiB, which would put knfsd's SERVER-SIDE cost near ~90
against flint-splice's measured 110 — i.e. flint is probably still ~20%
dearer in the server itself. **That is an inference from subtracting an
estimated constant, not a measurement.** Treat 1.07x as the floor of
flint's remaining deficit, not the ceiling.

knfsd also remains 14% faster in wall time (356 ms vs 415 ms per 2 GiB).
Splice narrowed the throughput gap; it did not close it.

### Not general

One configuration: 4 readers, 64 MiB files, cache-warm, 2 vCPU,
loopback, O_DIRECT. No claim beyond it.


---

## 11. S4 CONFORMANCE — splice introduces NO regressions

pynfs 4.1, full suite, `flint-pnfs-mds --standalone` in-VM as root, run
twice on the same code with only the flag changed:

| run | PASS | FAIL | SKIP | failing test |
|---|---|---|---|---|
| `SPLICE=1` | 170 | 1 | 91 | `st_exchange_id.testNoUpdate100` |
| `SPLICE=0` | 170 | 1 | 91 | `st_exchange_id.testNoUpdate100` |

Identical. **Splice is clean on conformance** — which is what S4 existed
to establish, and the flag-off control is what makes the claim
attributable rather than assumed.

### A separate, pre-existing question: 171 -> 170 since the Aug-24 floor

`testNoUpdate100` is EID5e, RFC 8881 case 3: a client with NO state
(session created then destroyed) re-does EXCHANGE_ID with a different
principal AND verifier, so the server must replace the record and the
old clientid must go stale. flint answers NFS4_OK, i.e. the old clientid
still works. If the server believes the old client still HOLDS state it
takes a different branch and declines to replace — which points at
DESTROY_SESSION's cleanup or EXCHANGE_ID's case analysis. No READ is
involved; nothing splice touches.

Three candidates, one of them self-inflicted:

1. the POSIX fix wave (touched `state_backend/*` and `tier/*`, but for
   tier operations, not client records — no obvious mechanism);
2. the splice work — RULED OUT by the control above;
3. **the pynfs environment, changed today.** Its generated XDR modules
   were missing (`python3-ply` absent) and had to be regenerated before
   the suite could import at all. The Aug-24 baseline ran against the
   OLD generated code, so the delta could live in the harness rather
   than the server.

Cheapest decisive test: run today's pynfs against `5863468e`, the commit
before any of today's work. Failing there too means environmental;
passing means a real regression landed today.
