# Build radar_data.json from wf_result.json per the reconciliation spec.
import json, sys, os

SRC = os.path.join(os.path.dirname(os.path.abspath(__file__)), 'wf_result.json')
DST = os.path.join(os.path.dirname(os.path.abspath(__file__)), 'radar_data.json')

wf = json.load(open(SRC))
dims = {d['dim']: d for d in wf['dimensions']}

def axes_of(dim):
    return [{'name': a['name'], 'short': a['short'], 'definition': a['definition']}
            for a in dims[dim]['proposal']['axes']]

# ---------- axes ----------
ax_cons = axes_of('consistency')
ax_perf = axes_of('performance')
ax_sec = axes_of('security')
ax_day2 = axes_of('day2')

# cold-start definition replacement (critic re-anchor)
for a in ax_perf:
    if a['short'] == 'Cold start':
        a['definition'] = ("Time from a new pod starting against a live, warm share to a usable "
                           "workspace; after-idle and hibernated wake are priced under Day-2 Idle & wake.")

# ---------- scores + rationales ----------
# helper shorthand
C = {}   # C[(dim, series)] = ([scores], [rationales]) in axis order

C[('consistency','NFS')] = ([5,5,3,3.5,5,1.5],[
 '"opens, byte-range locks, O_EXCL and rename are real across every client of the share"; shared sqlite/git is shipped and cluster-proven. krb5p identical — GSS wraps the RPCs, not what they mean (flint-lite-consistency.html).',
 'Enforced close-to-open: "the hub is the one coherence authority"; only residuals are a held-open file\'s open()-time snapshot and readdir cached up to acdirmax 60s (flint-lite-consistency.html).',
 'v1.30 If-Match on PUT/DELETE/move — "a competing write answers 412 instead of silently losing your edit" — but detection between API callers, not exclusion against the mount; evict/hydrate flips ETags (fleets guide).',
 '"an acked write is crash-durable on the hub\'s PVC immediately" — survives hub crash, reschedule, node crash; only PVC loss converts RPO to loss. Not 5: fsync still does not mean S3 (flint-lite-consistency.html).',
 'Two writers, one file: "serialized — locks and share reservations are real" — the hub round-trip is where opens, locks and renames get ordered; a second writer blocks or is denied, never corrupts (flint-consistency-comparison.html).',
 'S3-side writes vs a live hub: "three fates: silently destroyed by local-wins, undetected while resident, or adopted as truth by a hydration 412"; REST-vs-mount race: "the ETag will not save you" (flint-lite-consistency.html).',
])
C[('consistency','FUSE')] = ([1.5,2.5,3,1.5,1.5,2],[
 '"no cross-pod locks, no shared sqlite / git — refused, not discouraged. ENOLCK and EXDEV"; one survivor: "O_EXCL as a synchronous If-None-Match:* PUT — the one strongly consistent cross-pod primitive" (flint-fuse-consistency.html).',
 'Another pod reads "a consistent snapshot at most one flush interval + one revalidation old; never read-your-peers\'-writes" — bounded automatic staleness, between NFS close-to-open and Lean\'s explicit sync (flint-fuse-consistency.html).',
 'Verifier raised 2.5 to 3: publishes are 100% CAS-guarded ("If-Match on every publish") and "bucket policy enforces conditional writes (s3:if-none-match)" binds every bucket writer — unlike the hub\'s mount-exempt CAS (fuse-architecture).',
 'fsync ack is "durable to the pod\'s emptyDir... NOT the pod. NOT S3"; "the loss boundary is POD DEATH" — routine on spot; preStop drains ~9–15 GiB inside a 120s notice, a hard crash drains nothing (flint-fuse-consistency.html).',
 'Two writers on one file are "out of contract — one writer per subtree, lease-enforced"; the lease is cooperative CAS that "binds only writers that cooperate" — a credentialed non-cooperator is detected, not stopped (comparison doc).',
 '"same three fates as hub mode — destroyed, undetected, or adopted" — mitigated only structurally: the read-only project role and prefix-scoped IRSA narrow who can write foreign at all (flint-fuse-consistency.html).',
])
C[('consistency','Lean')] = ([1,1.5,4,1.5,2.5,4],[
 '"PLAIN FILES — a real local filesystem, full POSIX, zero interception" — nothing can be made cross-pod, not even FUSE\'s O_EXCL; cross-pod is "snapshots + sync only"; sqlite/git single-pod only (flint-lean-architecture.html).',
 'v1 does not solve "cross-pod live visibility (snapshots + sync only)" and sync is "harness-invoked... never background" — a reader pod\'s staleness is unbounded until explicit sync, weaker than FUSE\'s cadence (lean plan §3).',
 '"the gateway is the enforcement point these cells always lacked — detection becomes refusal"; pods hold zero bucket write credentials; must-FAIL arms (stale If-Match MUST 412) and per-request epoch validation (flint-lean-architecture.html).',
 '"Loss bounds: RPO per pod; preStop drain best-effort... hard crash drains nothing" — the same emptyDir domain as FUSE, plus a stated residual: torn whole-file uploads of files written during a forced barrier (lean plan §7).',
 'Enforced, not cooperative: takeover fence rotation makes "a deposed predecessor\'s in-flight manifest CAS 412", per-request epoch validation rejects below-cell epochs; the inbox cell serializes sidecar-vs-HITL writers (lean plan §2.2).',
 'Foreign manifest entries "are PRESERVED and queued into the inbox for the next sync — never dropped, never deleted"; 412s park into a conflict report; oracle: "both versions recoverable... never a silent winner" (flint-lean-plan.md).',
])

C[('performance','sys')] = ([3.5,3,1.5,2,4,1.5],[
 'Measured 164 MB/s vs 147 MB/s local (1.1×) — "Streaming is fine; metadata is the constraint" — competitive per client but capped by the wire and the hub\'s single NIC (flint-lite-for-agent-fleets.md).',
 'Measured: 512 MiB sequential write 104 MB/s vs 173 MB/s local — 0.6× of local on the app\'s write path (flint-lite-for-agent-fleets.md "What it performs like").',
 'Measured create 12.5× slower (123/s), delete 222× — "each is a synchronous round trip — and that is what dominates the tools agents actually run"; the guide\'s own advice: move this to emptyDir (flint-lite-for-agent-fleets.md).',
 '"npm install, a build tree, or git clone... will be paced by create/delete, not by bandwidth"; sqlite/git are correct through the mount but every lock/fsync/commit is a network round trip (flint-lite-for-agent-fleets.md).',
 'The working set persists on the hub\'s PVC, so agent pod churn pays only a mount — the best per-pod story against a live share; evicted files hydrate at a measured 131 s/GiB over default fan-out 6 (flint-lite-for-agent-fleets.md).',
 '"Not a parallel filesystem. One hub is one pod and one NIC" — every client\'s bytes aggregate into one pod\'s NIC; graduating is the pNFS profile, not migration (flint-lite-for-agent-fleets.md "What this is not").',
])
C[('performance','krb5p')] = ([2,1.5,1,1.5,3.5,1],[
 'Design estimate, dormant: the only mention is a "dormant pure-Rust RPCSEC_GSS/Kerberos implementation (krb5/krb5i/krb5p)"; privacy decrypts every READ payload per RPC, unquantified — below sys\'s measured 164 MB/s (pnfs-operator-runbook.md).',
 'Every WRITE payload GSS-wrapped per message on the client and the hub\'s pure-Rust path; unquantified in docs — qualitatively below sys\'s measured 0.6×-of-local (pnfs-operator-runbook.md).',
 'The same synchronous per-op round trips, each additionally GSS-wrapped and unwrapped; dormant, unquantified — strictly below sys\'s measured 12.5×/222× cliff (pnfs-operator-runbook.md).',
 'Same RTT pacing plus per-message GSS crypto on each small synchronous RPC; dormant in-tree, overhead unquantified — scored as the designed posture (pnfs-operator-runbook.md).',
 'Same PVC-warm hub machinery, plus GSS context establishment in the mount path — "KDC, per-node keytabs, rpc.gssd" — an added first-access dependency; dormant, design estimate (pnfs-operator-runbook.md).',
 'The same one-pod one-NIC ceiling, and the hub\'s CPU must GSS-wrap/unwrap every byte for every client — the aggregate cap arrives at NIC or crypto CPU, whichever saturates first (pnfs-operator-runbook.md).',
])
C[('performance','FUSE')] = ([4,4,3.5,4,3.5,4.5],[
 'As designed: resident reads are "full local POSIX at disk speed", but "v1 ships the classic daemon data path" — every read byte transits the userspace daemon; passthrough/io-uring are opportunistic only (flint-fuse-architecture.html p3).',
 'Writes land on the emptyDir working set at local speed minus the classic-FUSE copy path; the 8–13 s/GiB publish rate is background and never blocks the app\'s write (flint-fuse-architecture.html p3–p4).',
 'Own-subtree create/delete are local — no network RTT — but each op pays a FUSE transition plus "one note*() from every mutating op" into state.db; never measured (flint-fuse-architecture.html p3).',
 'Single-pod sqlite/git run on the local working set at "disk speed"; fsync-heavy loops still pay per-op daemon transitions; cross-pod shared sqlite/git is refused with errno — a scoping choice, not a slowdown (fuse-architecture p3).',
 'Boot "imports manifest stubs, no bytes moved" — fastest time-to-first-file — but each touched byte pays hydration: 72.5 s/GiB measured pre-fan-out; 1000-pod correlated starts risk 503 storms (flint-fuse-architecture.html p3, p5).',
 'No hub in the data plane — "the only always-on component is S3 itself"; throughput scales with pods against S3, bounded by the bucket: 503 SlowDown bursts and an N× re-fetch duplication tax (flint-fuse-architecture.html p1, p4).',
])
C[('performance','Lean')] = ([5,5,5,5,3,4],[
 '"PLAIN FILES — a real local filesystem, full POSIX, zero interception" — reads come from the node\'s own disk and page cache with nothing of flint\'s in the path (flint-lean-architecture.html).',
 'The app writes plain local files with zero interception; publish is an asynchronous cadence scan, measured 8.0 s/GiB sidecar-side, outside the write path (flint-lean-0b-measurements.md).',
 'Storms hit the local filesystem natively; the measured 100k-file leg puts all cost in the background barrier (117 s first publish, 2.8 s idle tick, 27 MiB manifest), not app-facing op latency (flint-lean-0b-measurements.md).',
 'Proven with real binaries: "git fsck --strict clean... sqlite integrity_check ok, all rows present" on plain local files across 3 publish barriers — the loop itself is never intercepted (flint-lean-0b-measurements.md e2e leg).',
 '"cold start downloads the whole workspace; fine at GiB scale" — measured floor 3.3 s/GiB, 49.5 s for 100k files; every pod start pays the full checkout even against a live share; fan-out not yet built (lean 0b measurements).',
 'Verifier lowered 4.5 to 4: grade-1 proxy is the decided posture — "Proxy posture (decided): grade-1 primary" — every publish and checkout byte transits the proxy tier; grade-2 presigned direct-to-S3 is deferred (lean plan §2.2, §8).',
])

C[('security','sys')] = ([0.5,0.5,2.5,0.5,1,1.5],[
 '"Port 2049 is AUTH_SYS: the client asserts its own uid and the server takes it"; B9: "AUTH_SYS trusts wire uid 0, no squash, no in-process authz" — identity is self-declared, not proven (flint-lite-architecture.html).',
 'Cleartext RPC: the shipped posture gives "no per-user authentication or wire encryption"; RPC-with-TLS (xprtsec=mtls) is only prospective, attractive once client kernels reach 6.5 (pnfs-operator-runbook.md).',
 'B5–B8 fixed at HEAD (state-table quota, slot cap, idle deadline, READ clamp), but the surface stays a full stateful NFSv4.1 machine on an open port — "no credential needed"; the defense is caps, not auth (nfs-server-hardening-plan.md).',
 '"no auth on 2049 — the network is the boundary"; "nfsClientCIDRs cannot express a client in another cluster"; B12 prefix-reuse adoption open at HEAD — "B serves A\'s files, no race required" (nfs-server-hardening-plan.md B12).',
 'Any compromised pod reaching 2049 "can claim any uid" with no root-squash — root on every reachable share, no per-client credential to revoke; saving grace: "the bucket trusts one principal: the hub" (flint-lite-for-agent-fleets.md).',
 'Verifier raised 1 to 1.5: the flagship footgun is fixed at HEAD a8a1f5a — SECINFO now advertises AUTH_SYS first / AUTH_NONE last, order pinned by a unit test, and "a stock mount now negotiates sec=sys" (compound.rs).',
])
C[('security','krb5p')] = ([4,4.5,2.5,2,3.5,2],[
 'Verifier lowered 4.5 to 4: strongest mechanism in the tree, but the server "accepts AUTH_UNIX unconditionally alongside GSS" — no flavor-enforcement policy exists in the design, so wiring cannot gate acceptance (server_v4.rs:733).',
 'Per-message GSS privacy plus integrity on every NFS RPC, mutually authenticated — "the strongest wire posture in the tree" — but it is the dormant path, scored as designed (pnfs-operator-runbook.md).',
 'Same binary, same port, same B5–B8 fixes and the same pre-auth TCP/session surface; no doc claims GSS shrinks the floodable surface — scored equal to sys (nfs-server-hardening-plan.md B5–B8).',
 'Proven per-user identity makes POSIX modes bind to a real principal, but the tenancy machinery is unchanged — same per-prefix hubs, same open-at-HEAD B12 adoption hole, same network-shaped cross-cluster boundary (pnfs-operator-runbook.md).',
 'A compromised pod yields only that user\'s short-lived ticket — "KDC, per-node keytabs, rpc.gssd": keytabs live on nodes, and the KDC can disable the principal; bounded to one identity; never production-drilled (pnfs-operator-runbook.md).',
 'A sec=krb5p mount fails closed client-side, but the server accepts AUTH_UNIX alongside GSS with no refusal policy; KDC/keytab/rpc.gssd wiring is classic sprawl with zero in-tree tooling — dormant, pynfs-only (pnfs-operator-runbook.md).',
])
C[('security','FUSE')] = ([3.5,4.5,4.5,3.5,1.5,3.5],[
 'SigV4/IRSA machine identity, unforgeable at the store: "every pod holds prefix-scoped credentials (IRSA)" and "IAM principal tags enforce it independently" — strong proof, workload-granular not per-user; designed (fuse-architecture).',
 'All data-plane traffic is S3 over TLS — "plain S3, no flint server anywhere in the read path"; no cleartext NFS hop exists in this design (flint-lean-architecture.html p2, covering direct-mode FUSE and lean alike).',
 'No flint-operated door to flood: "the only always-on component is S3 itself" — the reachable data plane is S3, which refuses unsigned requests; the only cluster service is a create-time webhook (flint-fuse-architecture.html).',
 '"IAM scope ends at ws-a/" with principal-tag conditions as "a second, unforgeable layer"; the two-principal split keeps bucket-wide levers out of workspace policies — store-side and unforgeable, but designed-only (fuse-architecture).',
 'Creds narrow cross-tenant reach "while irreducibly widening what a compromised pod can do to its own subtree"; the pod carries a "privileged · Bidirectional · /dev/fuse" sidecar — node reach; IRSA revocation slow (fuse-architecture).',
 '"The webhook must not be the security boundary... a webhook bug is caught by a second, unforgeable layer"; failurePolicy: Fail mandated. Docked for the bootstrap 403 footgun and stale-sidecar exposure (flint-fuse-architecture.html).',
])
C[('security','Lean')] = ([3,4,4,4,4.5,4],[
 'Verifier lowered 3.5 to 3: the decided v1 auth is a static bearer, not even per-workspace ("Auth v1: bearer only vs + SigV4 — open"); SigV4/TokenReview deferred — a weaker proof of identity than FUSE\'s per-pod IRSA (lean plan §9.3, §8).',
 'Verifier lowered 4.5 to 4: no lean doc claims TLS anywhere; grade-1 proxy is the decided posture and carries every publish and checkout byte; grade-2 "bytes go pod to S3 DIRECT" is deferred (lean plan §2.2, §8).',
 'The gateway is stateless ("no PVC, no epoch of its own, no state") and a successful DoS is availability-only — publishes pause, checkouts wedge; no B6-class state minting, though no B5–B8-style battery yet (flint-lean-architecture.html).',
 '"Proxy tenancy: project-granular, dedicated bucket or prefix per project (DECIDED)"; claim identity with "foreign-on-reused-prefix must refuse" arms closes the B12 class; within-project residual accepted for v1 (lean plan §9.6, Phase 1).',
 '"pods hold ZERO bucket write credentials", the sidecar an "ordinary process — UNPRIVILEGED, no /dev/fuse... no PSA conflict", "revocation in seconds — kill the token"; epoch validation refuses deposed writers (flint-lean-architecture.html).',
 '"detection becomes refusal" at the gateway; no token means "404, not 401", the chart refuses to render on credential misconfig; must-FAIL arms "refusing barriers while drifted" — fail-closed; Phase 4 operator unbuilt (lean plan §2.2).',
])

C[('day2','sys')] = ([2,2.5,1.5,3.5,4,2.5],[
 'Hub NODE crash is the worst shape on any page: "every client hard-hangs in D-state; Recreate + RWO + ~6 min force-detach; F29\'s manual arm" — sleep SIGKILL cannot touch; benign pod-crash (13s, park and resume) offsets (fuse-arch p4).',
 'Measured-benign: "restart + epoch re-claim ≈ 13s; clients park and resume — nothing durable at risk (PVC)", MDS roll ~40s; docked for single-replica Recreate and G15\'s trap: a schema bump makes rollback a crash-loop (hardening plan G15).',
 '"hub: 3000 Services (73% of a /20 CIDR) · 3000 PVCs · the ladder"; cold start at 3000 CRs was a deterministic OOMKill (B3); rate-term blockers are fixed but the per-share standing-object ladder is by design (flint-lite-fleet-scale-plan.md).',
 'Best-instrumented of the four: /status "tells importing from wedged" with recovery point, epoch holder, client list; hibernate is "verify-then-delete, never assume" on rpoClean; docked for the silent wrong-PV-address trap (fleets guide).',
 '"Nothing from flint installed" — the data path is the node kernel\'s NFS client; consumers never get bucket credentials. Residue is config traps: without nconnect 2+ the kernel "silently refuses every additional trunk" (lite-architecture).',
 'Two-rung ladder gives disk-only then S3-only cost, verified drain (wake ~41s / ~79s), but suspendWithSessions "defaults to suspending anyway" and suspending under a live hard mount is "not data loss; an indefinite hang" (fleets guide).',
])
C[('day2','krb5p')] = ([2,2,1,3,1,2.5],[
 'Same hub concentration, so the identical D-state/force-detach class applies unchanged — and it adds KDC/keytab/rpc.gssd as further mount-path dependencies; dormant, scored as designed (pnfs-operator-runbook.md).',
 'Everything the sys hub carries, plus per-node keytabs and rpc.gssd join every client-node lifecycle event; the stack is dormant — no roll of it has ever been exercised; scored as designed (pnfs-operator-runbook.md).',
 'Inherits the hub\'s entire 3000-object ladder and adds a KDC plus keytab distribution to every client node — standing security state sys does not carry, in every consumer cluster (pnfs-operator-runbook.md).',
 'Same /status + CRD surface, but the GSS path has no operational instrumentation — validated only "via pynfs --security=krb5" — so keytab/context/ticket failures arrive as a new uninstrumented diagnosis class (pnfs-operator-runbook.md).',
 '"Wiring it up (KDC, per-node keytabs, rpc.gssd, sec=krb5p mount opts)": every client node gains a keytab, a daemon and a realm dependency — the heaviest consumer requirement of the four, for a dormant stack (pnfs-operator-runbook.md).',
 'The ladder and its hazards are unchanged — the GSS layer rides the same :2049 data path and the runbook\'s Kerberos section states no interaction with park/wake; scored as designed on the dormant implementation (pnfs-operator-runbook.md).',
])
C[('day2','FUSE')] = ([3,2,3,1.5,1.5,3],[
 '"a dead mount is ENOTCONN — an error, never a D-state hang"; failure distributes into "frequent, per-pod, LOSS-shaped events" — errors beat hangs, but the loss class is permanent and hub mode does not have it (fuse-architecture p4).',
 '"upgrade = pod churn: injection is create-time only... a flint-fuse CVE reaches the fleet by pod churn"; restart is fragile — "the reflex... destroys exactly the dirty set" — plus the 80–110s lease tax on unclean death (fuse-arch).',
 '"mode: direct — NOTHING standing... one webhook, one bucket", but the page prices its own residue: "epoch heartbeats: 3000 cells at 10s defaults ≈ $3.9k/mo" pending lease-only-while-dirty; 1000-pod reclaim LIST risk (fuse-architecture p2).',
 'Deletes the fleet\'s best witness: "FUSE mode has no equivalent witness [to rpoClean], so the bucket... becomes the only global status surface"; per-pod metrics merely owed; loss rate "must be measured there" (fuse-architecture p4).',
 'The injected sidecar is "privileged · Bidirectional · /dev/fuse" in every matched pod; "PSA baseline / restricted refuse privileged sidecars" — forcing a node-broker endgame; kernel floor of plain fuse3 (flint-fuse-architecture.html p1/p2).',
 '"idle = Hibernated, structurally" — "the idle LADDER becomes vacuous, and with it the B6-class inversions"; the price: "every pod start is a Hibernated-class cold start" plus heartbeat economics (fuse-architecture p2).',
])
C[('day2','Lean')] = ([3.5,3.5,3.5,3,3.5,3.5],[
 '"gateway down: publishes pause... far softer than hub-down; reads never depended on it" — "the softest failure shape" on these pages; held to 3.5: an outage also wedges checkouts, restarts, sync and HITL writes (lean plan §7).',
 'Container restart is first-class in the shipped matrix — "never re-materialize: reload the persisted baseline, rescan to rebuild dirt, self-recognize the lease" — battery-tested; unprivileged sidecar lowers upgrade stakes (lean plan §2.1).',
 '"Idle = S3 only, structurally. Lease cells exist only while a writer runs" — nothing standing per share, one stateless gateway; costs per-activity: a hot 2 GiB file ≈ 2.9 TiB/day at 60s, "bounded by policy, not physics" (lean plan §6).',
 'Phase 3 names deliverables — "per-pod RPO gauge... publish-failure and checkout-wedged alerts, conflict-report surfacing" — and sync emits a machine-readable conflict report; gauges not confirmed shipped, no client list (lean plan §5).',
 'Sidecar is an "ordinary process — UNPRIVILEGED — no /dev/fuse... no PSA conflict", kernel floor "drops to nothing", zero pod bucket credentials; under sys: native sidecars (K8s 1.29+) and webhook injection are mandatory (lean-architecture).',
 '"Idle = S3 only, structurally. Lease cells exist only while a writer runs" — no ladder, no suspend hazard, no keepalive to get wrong; cold start is a full checkout through the proxy whose rate acceptance is still open (lean plan §6).',
])

NOTES = {
 'consistency': "Consistency is arbitration, not proximity to S3: the hub sweeps primitives, freshness and arbitration because every operation takes the round-trip where ordering is manufactured. The inversion is the story — the hub is nearly weakest at foreign-write surfacing (the &ldquo;three fates&rdquo;), while Lean posts the best CAS and conflict scores: its gateway turns detection into refusal, and the inbox/merge manifest never picks a silent winner. Both direct modes share the 1.5 durability floor: fsync means emptyDir.",
 'performance': "The consistency chart inverted: the direct modes win every hot-path axis because the data path is local — Lean&rsquo;s plain files sweep streaming, small-file and hot-loop I/O, while the hub&rsquo;s measured metadata cliff (create 12.5&times;, delete 222&times; slower) is its worst cell. The hub&rsquo;s one win is cold start against a live share (pod churn pays only a mount). krb5p trails sec=sys everywhere: GSS privacy encrypts every RPC byte (unquantified; scored as designed). Envelope caveat: only the hub hosts working sets beyond node-local disk — Lean and FUSE writers are bounded by dirty-set caps.",
 'security': "The widest spread: sec=sys sits at the bottom on five of six axes — identity self-asserted, wire cleartext, the boundary is reachability. krb5p buys the strongest auth and wire on a dormant code path and pays for it on misconfiguration (the server accepts AUTH_SYS alongside GSS unless enforced). FUSE wins tenancy (unforgeable IAM principal tags) yet craters on pod compromise: credentials in every pod plus a privileged sidecar. Lean holds the best envelope — zero pod write credentials, seconds revocation, unprivileged sidecar. DoS resistance is quota-hardened at HEAD (B5–B8) but stays a pre-auth surface.",
 'day2': "Concentration costs the hub the most: both NFS flavors inherit share-wide hang-shaped failures (D-state clients, ~6 min force-detach) and a 3000-Service standing ladder — recurring fleet cost is priced inside Fleet scale (FUSE heartbeats &asymp; $3.9k/mo at 3000 shares; Lean publish amplification &asymp; 2.9 TiB/day for a hot 2 GiB file). krb5p subtracts further on every client-side axis (KDC, keytabs, rpc.gssd). Lean beats FUSE nearly everywhere by shedding privilege and the restart-in-place choreography; the hub keeps observability (rpoClean, /status) — and Lean&rsquo;s new wedge is a gateway/proxy outage: publishes pause AND checkouts, restarts, sync and HITL writes wedge.",
}

FOOT = "Scores 0 (weakest) – 5 (strongest): a verified read of the architecture docs at repo HEAD (2026-08-25) — the flint-lite / strict / fuse / lean architecture pages, the three consistency contracts, the NFS hardening plan and the lean plan of record. Method: one proposer agent per dimension, one adversarial verifier per approach column (six score adjustments adopted), one cross-dimension critic (double-counted facts re-homed; cold start re-anchored to a live share). Everything is scored as designed, with maturity shown separately: hub shipped + drilled &middot; krb5p dormant in-tree, never wired &middot; FUSE designed only, deprioritized &middot; Lean built through Phase 3, operator/webhook + fleet drills open. Some Lean evidence (Phase-0b measurements) sits in uncommitted working-tree docs as of today. B1–B8 and B10 hardening fixes landed at HEAD are reflected; B12 prefix-reuse remains open."

SUBS = {
 'consistency': "krb5p ≡ sec=sys here — Kerberos wraps authentication, not coherence",
 'performance': "hot paths are local for the direct modes; measured numbers where they exist",
 'security': "scored as designed — krb5p is dormant in-tree",
 'day2': "what it takes to run each front end at fleet scale",
}

CHARTS = [
 ('consistency', 'Consistency', ['NFS','FUSE','Lean'], ax_cons),
 ('performance', 'Performance', ['sys','krb5p','FUSE','Lean'], ax_perf),
 ('security', 'Security', ['sys','krb5p','FUSE','Lean'], ax_sec),
 ('day2', 'Day-2 operations', ['sys','krb5p','FUSE','Lean'], ax_day2),
]

charts = []
for dim, title, series, axes in CHARTS:
    scores = {}
    rats = {}
    for s in series:
        sc, ra = C[(dim, s)]
        scores[s] = sc
        rats[s] = ra
    charts.append({
        'title': title,
        'sub': SUBS[dim],
        'series': series,
        'axes': axes,
        'scores': scores,
        'rationales': rats,
        'note': NOTES[dim],
    })

out = {'charts': charts, 'foot': FOOT}

# ---------- validation ----------
errors = []
for ch in out['charts']:
    if len(ch['axes']) != 6:
        errors.append(f"{ch['title']}: {len(ch['axes'])} axes")
    for s in ch['series']:
        sc = ch['scores'][s]; ra = ch['rationales'][s]
        if len(sc) != 6: errors.append(f"{ch['title']}/{s}: {len(sc)} scores")
        if len(ra) != 6: errors.append(f"{ch['title']}/{s}: {len(ra)} rationales")
        for i, v in enumerate(sc):
            if not (0 <= v <= 5) or (v * 2) != int(v * 2):
                errors.append(f"{ch['title']}/{s}/axis{i}: bad score {v}")
        for i, r in enumerate(ra):
            if len(r) > 240:
                errors.append(f"{ch['title']}/{s}/{ch['axes'][i]['short']}: rationale {len(r)} chars")
            for bad in '&<>':
                if bad in r:
                    errors.append(f"{ch['title']}/{s}/{ch['axes'][i]['short']}: raw '{bad}' in rationale")

if errors:
    print("VALIDATION ERRORS:")
    for e in errors: print(" ", e)
    sys.exit(1)

json.dump(out, open(DST, 'w'), ensure_ascii=False, indent=1)
# round-trip check
json.load(open(DST))
print("OK — wrote", DST)

# summary table
for ch in out['charts']:
    print(f"\n{ch['title']}")
    shorts = [a['short'] for a in ch['axes']]
    hdr = f"{'axis':<14}" + ''.join(f"{s:>8}" for s in ch['series'])
    print(hdr)
    for i, ax in enumerate(shorts):
        row = f"{ax:<14}" + ''.join(f"{ch['scores'][s][i]:>8}" for s in ch['series'])
        print(row)
