# Where a wake's time goes (box, 2026-10-02)

Rig: `../step6-box-wakeparts.sh`.
- **Setup:** kind on the build box; one hub with 1,000 files; RustFS.
- **Images:** the operator from HEAD at 66e47b9a plus the reconcile-loop
  fix; the hub `flint-pnfs:step5-dev`.
- **Reps:** three wakes from suspend and three from hibernate, each on a
  fresh hard mount through the proxy.
- **Timestamps:** offsets from the client's first access. The proxy logs
  every DELAY it answers, so its lines are the client's actual retries.
  Kubernetes events and conditions have 1 s resolution. Raw data is in
  `run.txt` and the per-wake directories.

## Before the fixes: hibernate (CR alone: new disk, import from the bucket), 3 reps, same every time

| offset | step | cost |
|---|---|---|
| 0.0 s | first DELAY; proxy requests the wake | |
| ~0.4 s | operator creates Deployment, PVC and pod | operator: <0.5 s |
| 3.0–3.7 s | PVC provisioned (kind local-path) | provisioning: ~3 s |
| 4.0–4.7 s | pod scheduled, container started | ~1 s |
| **5.3–5.4 s** | **hub listening, bucket import done** (1,010 entries in 0.03 s) | hub: ~0.7 s |
| 14.4–15.7 s | pod Ready | **startup probe: ~9.5 s idle** |
| 15.0–15.7 s | operator writes the share's status (address) | ~0.5 s |
| 26.5–26.8 s | client's first byte | **client DELAY backoff: ~11 s idle** |

The client retried at 0.03, 0.13, 0.34, 0.75, 1.6, 3.3, 6.5 and 13.2 s,
then at about 25.8 s: Linux's NFS4ERR_DELAY backoff, 100 ms doubling to
a 15 s cap.

**The hub was serving at 5.4 s; the user got a byte at 26.7 s.** The
21 s between them are two waits that have nothing to do with the work:
1. **The startup probe (~9.5 s).** Tiered hubs get a startupProbe with
   `periodSeconds: 10` (`render.rs:936`). Its first check runs before
   the hub listens and fails, so the pod waits for the next check, 10 s
   later. Readiness, the Endpoints, and the operator's status (which
   the proxy routes by) all wait on that.
2. **The client's DELAY backoff (~11 s).** The proxy answers DELAY the
   instant a hub is down. By the time the share was routable (~15.5 s),
   the client's next retry was ~25.8 s.

## Before the fixes: suspend (disk kept), 3 reps

- **Reps 1 and 3:** pod started at 0.2–0.6 s, hub listening at 1.3 s,
  Ready at 10.2–10.6 s (the same startup-probe wait), status at
  11.2–11.6 s, first byte at 13.2–13.4 s (the retry at ~13.0 s).
- **Rep 2 (26.75 s) is a rig artifact, with a real cause.** It started
  less than 30 s after rep 1's wake. The proxy wakes a hub at most once
  per 30 s, so the first six DELAYs requested nothing, and the wake
  went out only at 6.5 s, on the seventh. A user who re-touches a share
  within 30 s of its last wake (say, after an immediate re-suspend) pays
  the same.

## The fixes, and the result (`run-final.txt`, `final/`)

Two fixes cut the two idle waits:
1. **The startup probe checks every second** (was 10 s), with
   `failureThreshold` ×10 so the 10-minute budget is unchanged. The
   readiness probe also checks every second, with no initial delay.
2. **The proxy holds a compound for a waking hub** instead of answering
   DELAY at once. It re-asks for the wake every 5 s (was at most once per
   30 s), re-routes on each poll (a hibernated hub comes back with a new
   server id), and answers DELAY only after `wakeHoldSecs` (20 s, below
   the client's 60 s RPC timeout).

| | before | after (run 6) |
|---|---|---|
| suspend, 3 reps | 13.2–13.4 s (one rep 26.75 s) | 2.4–2.9 s |
| hibernate, 3 reps | 26.5–26.8 s | 5.7, 7.3, 7.2 s |

The hibernate wakes are now provisioning + pod start + import, plus about
a second. Every hibernation verified, reclaimed its disk and parked as its
CR; the share logged exactly six wakes, one per rep.

### Three operator bugs the fixes exposed, one cause

The operator clears `chert.us/requested-at` when it starts a wake. With
the proxy re-asking every 5 s while the hub starts, one ask lands AFTER
the clear, and a stale stamp is left on a running share. Three places
read the stamp as presence, and each one broke:
- **Hibernate verification aborted** (`run-fix2-readiness-rewake.txt`,
  rep 2: "HibernateAborted: wake requested during hibernate
  verification").
- **A parked share woke itself** a second after parking
  (`fix4-selfwake/`: parked 21:40:30, Woken 21:40:31; the proxy's last
  wake request was at 21:38:48, during rep 1). It took a run with the
  full proxy and operator logs to see it: the first failure had only the
  logs around each wake, and the cluster was gone.
- **The disk was never reclaimed** (`fix5-halfparked/`): Hibernated,
  Deployment at 0, PVC still Bound; `reclaim_hibernated_disk` read the
  stamp as a pending wake. The self-wake had been hiding this one.

The rule now, in all three: a stamp older than `idle-since` is stale
(`idle::woken_since_idle`). A real waiter is not lost, since the proxy
re-asks while the share is down, after `idle-since`.

The lesson: the producer changed (re-asking during the hold) and only
the reader that failed was fixed, three times. All the readers should
have been listed at the first one.

On flint-spdk, provisioning differs from kind's local-path; the AWS run
saw flint-spdk PVCs bind and pods start in seconds.
