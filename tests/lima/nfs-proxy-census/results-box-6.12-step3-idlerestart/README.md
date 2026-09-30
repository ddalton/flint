# step 3 `idlerestart` — an idle lock holder across a hub restart

Box, Linux 6.12.0, 2026-09-30. `step3-drills.sh idlerestart`.

A process takes a write lock on `ws-a/lk` through the proxy and stays idle.
The proxy mount uses `actimeo=3600`, so the kernel sends no revalidation of
its own. After 2 leases, hub A restarts (state persisted); after 3 more
leases, a contender on a direct mount of hub A tries the lock, then the
holder writes.

| Build | Runs | Contender | Holder's write | Result |
|---|---|---|---|---|
| before the fix (`7e1220c8` proxy) | 2 | ACQUIRED | EIO | 1/3 each (lock and write fail) |
| keepalive re-attach | 2 (`fixed-1.txt`, `fixed-2.txt`) | REFUSED | ok | 3/3 each |
| keepalive re-attach, whole step 3 suite | 1 (`all-fixed.txt`) | — | — | 14/14 |

The known-bad runs were printed, not saved. Their lines:
`contender: ACQUIRED; holder's write after: errno 5; client-driven attaches
to ws-a before/after the restart: 1/1`, with the no-client-op check PASS
and both lock checks FAIL.

**The cause:** after a hub restart the proxy's keepalive hit a closed
connection (it only logged it) or BADSESSION (it dropped the backend). An
idle client sends nothing that reaches the hub, so nothing re-attached.
The hub reaped the backend client one lease later, the lock with it.

**Why the first runs passed:** without `actimeo`, the kernel's own GETATTR
of the ws-a root (about once a minute, seen in a capture) re-attached the
backend when it landed within a lease of the restart. The arm then passed
or failed on the kernel's timing. Hence `actimeo=3600`, plus a check that
no client-driven attach happened across the restart.

**The fix:** `Proxy::reattach` registers again under the same owner and
verifier. The hub, which kept its state, answers with the same clientid
(`re-attached by the keepalive: clientid 0x1` in both fixed runs).
