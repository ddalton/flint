#!/usr/bin/env python3
"""The source of every diagram under docs/architecture/forge/diagrams/.

    python3 docs/architecture/forge/forge-diagrams.py     # writes diagrams/*.svg

Seven A3 plates for the flint-forge architecture document, drawn with the
deck's kit (../diagrams.py) so the two documents share one visual grammar:
the forge hue means forge, neutral means everything else, solid coloured
arrows carry data, dashed neutral arrows carry control, red is reserved for
a hazard or a trust boundary. The one addition here is a second dark
solid line, `l-ink`, for the DURABLE PATH — the git-to-S3 core — so the
three planes the document is organised around are three line styles a
reader can tell apart: forge-coloured solid for the data plane, dark solid
for the durable path, dashed for the control plane.

Every card asserts that its wrapped text fits its box (an estimate);
build.sh --geometry measures the truth in Chrome.
"""
import os
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.join(HERE, ".."))
import diagrams as kit  # noqa: E402  the deck's kit, imported, not copied

kit.OUT = os.path.join(HERE, "diagrams")
SVG, write, wrap, est = kit.SVG, kit.write, kit.wrap, kit.est
W, H, M, GAP = kit.W, kit.H, kit.M, kit.GAP
FE = "forge"


def chips(s, x, y, planes):
    """The plane membership of a component, as pills: DATA in the forge hue,
    DURABLE dark, CONTROL neutral. Drawn here rather than with the kit's
    chip() so the pill carries margin the letter-spaced caps need: the
    geometry probe wants 3 px of air past the last glyph."""
    cx = x
    for p in planes:
        label = {"data": "DATA", "durable": "DURABLE", "control": "CONTROL"}[p]
        cls = {"data": f"chip-{FE}", "durable": "chip-ink", "control": "chip"}[p]
        w = est(label, s.fs["cap"], True) * 1.45 + 22
        s.rect(cx, y - 11, w, 16, cls, rx=8)
        s.text(cx + 9, y + 1, label, "cap on" if p != "control" else "cap")
        cx += w + 6
    return cx


class Plate(SVG):
    """The kit's SVG plus one class: a dark filled chip for the durable path."""

    def render(self):
        out = super().render()
        return out.replace("</style>", "  .chip-ink{fill:#334155}\n</style>", 1)


# ═════════════════════════════════════════════════════════════════════════
# 1 · the three planes
# ═════════════════════════════════════════════════════════════════════════
def plate_01():
    s = Plate(W, H, "flint-forge's components placed on three planes. The data plane: an agent's stock git, the door, "
                    "the flint-forge-gitcgi runner and git's own receive-pack and upload-pack, every byte of every clone and "
                    "push. The durable path: the hooks, the syncer, content-named packs and one CAS'd snapshot in S3 — the "
                    "only path that can write the bucket. The control plane: the operator and its FlintRepo, the door's "
                    "admission, routing and wake, the status poll and the lease cell. A membership matrix places every "
                    "component; the runner sits on the data plane alone.")
    # ── the topology ──
    s.card(M, 22, 250, 236, "the agent pod — any namespace",
           ["a stock git client; the working tree is local disk",
            ("m", "helper: flint-forge-credential"),
            "presents the pod's projected ServiceAccount token (audience forge.chert.us) as the Basic password on every call",
            ("b", "no bucket credential, no sidecar, no privilege")], FE)
    chips(s, M + 16, 246, ["data"])
    s.arrow(278, 120, 296, 120, FE, both=True)
    s.card(300, 22, 250, 236, "the door — the gateway's git arm",
           ["N stateless replicas; TokenReview per session, cached ≤ 60 s by token hash",
            "spec.consumers must list the ServiceAccount: 401 before any wake",
            "routes to the repository's pod; forwards Git-Protocol and X-Remote-User, never Authorization",
            ("b", "the one client-facing bound: no bytes either way for 300 s cuts the request")], FE)
    chips(s, 316, 246, ["data", "control"])
    s.arrow(554, 120, 572, 120, FE, both=True)
    # the server pod
    s.box(576, 22, 560, 236, FE)
    s.text(594, 46, "one server pod per FlintRepo — headless Service, emptyDir cache, Recreate", f"t2 c-{FE}")
    s.hair(594, 56, 1118, 56)
    s.card(590, 64, 262, 180, "git container",
           [("m", "flint-forge-gitcgi :8080"),
            ("m", "→ git http-backend per request"),
            ("m", "→ receive-pack · upload-pack"),
            "hooks: pre-receive names the rule; proc-receive hands the push to the syncer over a socket and WAITS",
            ("b", "no bucket credential in this container")], None, strip=False)
    chips(s, 606, 232, ["data", "durable"])
    s.card(866, 64, 256, 180, "syncer container",
           ["the batch · the lease renewer · restore · sweep · repack · export · bundles · LFS batch API",
            ("m", "/status :9848"),
            ("b", "the only writer of the bucket; the credential lands here only (envFrom)")], None, strip=False)
    chips(s, 882, 232, ["durable", "control"])
    s.arrow(1140, 120, 1158, 120, FE)
    # S3
    s.box(1162, 22, 414, 236, None, "s3")
    s.text(1180, 46, "S3 — <prefix>/", "t1")
    rows = [("git/objects/pack/*", "immutable, content-named — the packs", "durable"),
            ("git/snapshot", "THE pointer: refs + pack list; one CAS", "durable"),
            ("git/epoch · git/claim", "the lease cell · the operator's claim", "control"),
            ("git/bundles/<oid>", "clone bundles, presigned — the storm lever", "data"),
            ("lfs/objects/<oid>", "LFS, presigned: client to store", "data"),
            ("files/<path>", "the export: a lean workspace", "durable")]
    y = 70
    for key, what, plane in rows:
        s.text(1180, y, key, "mono")
        s.text(1180, y + 14, what, "t4")
        chips(s, 1470, y + 6, [plane])
        y += 30
    s.text(1180, y + 2, "a bare repository: clonable with the server down", "t4b")
    # ── the membership matrix ──
    comps = [("agent git", ["data"]), ("credential helper", ["data"]), ("the door", ["data", "control"]),
             ("gitcgi runner", ["data"]), ("http-backend, receive-pack, upload-pack", ["data"]),
             ("pre-receive, proc-receive", ["data", "durable"]), ("the syncer", ["durable", "control"]),
             ("packs, snapshot", ["durable"]), ("epoch, claim", ["control"]), ("operator, FlintRepo, apiserver", ["control"])]
    lab_w, cw, top, hh = 214, 133, 282, 56
    s.rect(M, top, lab_w, hh, "hd-lbl", rx=6)
    s.text(M + 14, top + 33, "plane  ·  component", "on")
    cx = M + lab_w + 4
    for name, _ in comps:
        s.rect(cx, top, cw - 4, hh, "panel", rx=6)
        lines = wrap(name, cw - 18, s.fs["t4b"], bold=True)[:3]
        ty = top + {1: 33, 2: 26, 3: 19}[len(lines)]
        for ln in lines:
            s.text(cx + (cw - 4) / 2, ty, ln, "t4b", "middle")
            ty += 14
        cx += cw
    planes = [("data plane", "every byte of a clone or push", "data", f"f-{FE}"),
              ("durable path", "what can write the bucket", "durable", "f-ink"),
              ("control plane", "placement, admission, the lease", "control", "f-ctl")]
    s.add('<style>.f-ctl{fill:#94a3b8}</style>')
    ry = top + hh + 6
    for i, (name, sub, key, cls) in enumerate(planes):
        rh = 46
        s.rect(M, ry, lab_w, rh, "lbl", rx=0)
        if i % 2:
            s.rect(M + lab_w, ry, cw * len(comps), rh, "rowb", rx=0)
        s.text(M + 14, ry + 20, name, "t2")
        s.text(M + 14, ry + 36, sub, "t4")
        cx = M + lab_w
        for _, member in comps:
            if key in member:
                s.add(f'<circle cx="{cx + cw / 2:.0f}" cy="{ry + rh / 2:.0f}" r="8" class="{cls}"/>')
            else:
                s.text(cx + cw / 2, ry + rh / 2 + 4, "—", "t4 mute", "middle")
            cx += cw
        s.hair(M, ry + rh, M + lab_w + cw * len(comps), ry + rh)
        ry += rh
    s.rect(M, top + hh + 6, lab_w + cw * len(comps), ry - top - hh - 6, "frame", rx=0)
    # ── the three definitions ──
    cw3 = (W - 2 * M - 2 * GAP) / 3
    y0 = 500
    s.card(M, y0, cw3, 206, "The data plane — every byte, nothing durable",
           ["A clone, fetch or push is the smart protocol from a stock client to stock git: the door proxies it, the runner execs git http-backend per request and streams both directions as they arrive, receive-pack and upload-pack do the git.",
            "A 40 GiB push crosses the runner twice: the pack in, the sideband keepalives and the report out. That is why fcgiwrap's buffering mattered: it held the keepalives that keep a long push alive.",
            ("b", "One client-facing bound, the door's 300 s inactivity cut. The runner has no timeout of its own and answers 503 past a declared ceiling rather than queueing (run 7: five pushes and eight clones at once, a 70 s stall, all served).")], FE)
    s.card(M + cw3 + GAP, y0, cw3, 206, "The durable path — what can write the bucket",
           ["proc-receive hands the push to the syncer and waits. The syncer judges it under the agreed view, renews the lease once, uploads every complete pack as immutable content-named objects, CASes ONE snapshot naming the refs and the packs, applies update-ref, and only THEN reports ok.",
            "The pack is verified by git, uploaded by the syncer, named by the CAS; the runner implements no git, holds no credential and never touches S3.",
            ("b", "The process that parses untrusted HTTP is not the process that can write the bucket. Machine-checked: told ok ⇒ durable; every landed pack complete; no straggler lands after a restore.")], None, "panel")
    s.card(M + 2 * (cw3 + GAP), y0, cw3, 206, "The control plane — placement, admission, liveness",
           ["The operator turns a FlintRepo into a ConfigMap, a headless Service, a one-pod Deployment and a NetworkPolicy, polls the pod's own /status for its phase, and parks an idle repository at replicas 0.",
            "The door admits by TokenReview and consumers, routes by repository, and wakes a parked one — 401 before any wake, the request held up to 180 s on the CR, never the pod.",
            "The lease cell is the writer's coordination: a token that must keep moving, six quiet polls before a takeover, a rotation of the snapshot on every claim.",
            ("b", "The operator never reads the bucket, and A3 touched none of this: chart, CRD, door and rigs did not change.")], None, "panel")
    # ── reading the placement ──
    s.box(M, 724, W - 2 * M, 134, None, "panel")
    s.text(M + 22, 750, "Reading the placement", "t1")
    yy = s.para(M + 22, 770, "The runner (A3) is squarely on the data plane and nowhere else. It replaced nginx and fcgiwrap in the git container after the runbx drill found three of its four front-layer defects in those two processes — buffered keepalives, two 60 s client timeouts nobody had listed, a four-worker ceiling that queued in silence — and it removed that class rather than tuning it. Nothing on the durable path or the control plane moved, which is why the chart, the CRD, the door's URL formula and the rigs did not change.", W - 2 * M - 44)
    s.para(M + 22, yy + 2, "Two boundaries are drawn by the planes. The bucket credential exists in the syncer container only, so a compromise of the HTTP-facing process cannot write S3. And X-Remote-User is the principal only behind the NetworkPolicy that admits the door's pods alone: reached directly, the git port believes the header (measured, run-rights.sh: with the policy deleted a forged header merges into main).", W - 2 * M - 44)
    s.legend(M + 4, 872, [("data", "durable path"), (FE, "data plane"), ("ctl", "control"), ("red", "hazard / trust boundary")])
    return s


# ═════════════════════════════════════════════════════════════════════════
# 2 · the data plane
# ═════════════════════════════════════════════════════════════════════════
def plate_02():
    s = Plate(W, H, "The data plane hop by hop. A clone or fetch: stock git in the agent pod, the door, the gitcgi runner, "
                    "git http-backend and upload-pack reading the emptyDir cache. A push: the same hops inbound with the pack, "
                    "then receive-pack's index-pack, the pre-receive policy hook, and proc-receive waiting on the syncer while "
                    "receive-pack sends a keepalive every 5 s; the sideband and the report flow back out. Below: the parties "
                    "between the client and git and the bound each one has after A3, and what run 7 measured through the runner "
                    "against the nginx control.")
    hops = ["agent pod · stock git", "the door", "gitcgi runner", "git http-backend", "receive-pack / upload-pack", "emptyDir cache"]
    hw = (W - 2 * M - 5 * 30) / 6
    # lane 1: clone / fetch
    s.text(M, 44, "Clone / fetch — what crosses each hop", "t1")
    y = 58
    for i, h in enumerate(hops):
        x = M + i * (hw + 30)
        s.box(x, y, hw, 110, FE, strip=(i in (1, 2)))
        s.text(x + 14, y + 22, h, f"t2 c-{FE}" if i in (1, 2) else "t2")
        body = [
            ["GET info/refs?service=git-upload-pack, then POST git-upload-pack: ls-refs with a prefix (protocol v2), fetch", ("m", "Basic x:<SA token>")],
            ["TokenReview → 401 or forward; consumers; the wake if parked", ("m", "X-Remote-User · Git-Protocol"), "HTTP, cleartext in-tree"],
            ["execs http-backend per request with the CGI environment from the request; streams the request body in and stdout out", ("m", "REMOTE_USER · GIT_PROTOCOL")],
            ["stock git's CGI: negotiates, forwards to upload-pack; sets no policy", ("m", "GIT_PROJECT_ROOT · EXPORT_ALL")],
            ["upload-pack with bitmaps; advertises a presigned bundle URI when the client opted in", ("m", "uploadpack.advertiseBundleURIs")],
            ["the packs the snapshot named, restored at start; a clone reads local NVMe", ("m", "objects/pack/*.pack .idx .bitmap")],
        ][i]
        yy = y + 40
        for it in body:
            if isinstance(it, str):
                yy = s.para(x + 14, yy, it, hw - 28)
            else:
                s.text(x + 14, yy, it[1], "mono")
                yy += 15
        if i < 5:
            s.arrow(x + hw, y + 55, x + hw + 30, y + 55, FE, both=True)
    # lane 2: push
    s.text(M, 204, "Push — the pack in, the sideband out", "t1")
    y = 218
    for i, h in enumerate(hops):
        x = M + i * (hw + 30)
        s.box(x, y, hw, 150, FE, strip=(i in (1, 2)))
        s.text(x + 14, y + 22, h, f"t2 c-{FE}" if i in (1, 2) else "t2")
        body = [
            ["POST git-receive-pack: the commands, then the pack, chunked; reads sideband packets until the report", ("b", "a 40 GiB push is one request of ~19 minutes")],
            ["the inactivity clock is touched by every chunk either way; 300 s of silence cuts the request", ("b", "the one client-facing bound, by design")],
            ["stdin from the request body as it arrives; stdout to the client as it is written; the child killed when the client goes", ("b", "no timeout, no body limit; 503 past the ceiling (64)")],
            ["execs receive-pack --stateless-rpc; index-pack writes the pack into quarantine, then migrates .keep .pack .rev .idx", ("b", "the index lands LAST — X1")],
            ["pre-receive: the policy, by principal. proc-receive: the push to the syncer, then WAIT", ("m", "receive.keepAlive = 5 s"), "a keepalive every 5 s of hook silence"],
            ["the pack is on local disk; the syncer reads it from here to upload it and names it only once its .idx exists", ("b", "durable only after the snapshot CAS")],
        ][i]
        yy = y + 40
        for it in body:
            if isinstance(it, str):
                yy = s.para(x + 14, yy, it, hw - 28)
            elif it[0] == "m":
                s.text(x + 14, yy, it[1], "mono")
                yy += 15
            else:
                yy = s.para(x + 14, yy, it[1], hw - 28, "t4b")
        if i < 5:
            s.arrow(x + hw, y + 75, x + hw + 30, y + 75, FE, both=True)
    # the parties and their bounds
    s.box(M, 392, W - 2 * M, 236, None, "panel")
    s.text(M + 22, 418, "The parties between the client and git, and the knob each one has — after A3", "t1")
    cols = [("git client", "no inactivity bound unless http.lowSpeedLimit/Time is set (off by default); the rigs set only the token header"),
            ("the door", "no bytes either way for upstreamTimeoutSecs (300) cuts the request; a wake holds a slot ≤ 180 s; TokenReview cached ≤ 60 s"),
            ("the runner", "none of its own: no body limit, no timeout; a ceiling of 64 concurrent requests that answers 503 at once. Removed: nginx's two 60 s defaults (X3) and fcgiwrap's four workers (X4)"),
            ("receive-pack", "receive.keepAlive pinned at 5 s in the syncer's config (X5): an empty sideband packet whenever the hooks have been quiet that long"),
            ("the hook", "none, correct as is: it waits for the syncer's report, and the process's exit closes its socket"),
            ("the syncer", "no batch deadline; the wedge detector is the renewer, gated on progress: a batch that moves no bytes for 6 × 10 s is taken over"),
            ("kubernetes", "terminationGracePeriodSeconds 30 against batches of minutes; SIGTERM is seen between batches, so a roll mid-push is a SIGKILL at 30 s (X6, measured: the push is told failed, cleanly)")]
    cw = (W - 2 * M - 44 - 6 * 12) / 7
    for i, (head, body) in enumerate(cols):
        x = M + 22 + i * (cw + 12)
        s.box(x, 434, cw, 180, FE, strip=(i in (1, 2)))
        s.text(x + 12, 456, head, f"t2 c-{FE}" if i in (1, 2) else "t2")
        s.para(x + 12, 476, body, cw - 24)
    # measured
    cw4 = (W - 2 * M - 3 * GAP) / 4
    y0 = 646
    s.card(M, y0, cw4, 210, "A stalled client — S5",
           ["1 GiB push, the client SIGSTOPped 3 s into the body for 70 s, then resumed.",
            ("b", "runner: acknowledged, and the bucket holds it."),
            ("r", "nginx control: HTTP 502 — client_body_timeout's 60 s default cut the client, the door relayed it.")], FE)
    s.card(M + cw4 + GAP, y0, cw4, 210, "Five pushes at once, eight clones at once — S6, S10",
           ["Five 256 MiB pushes stopped mid-body; an advertisement request timed beside them. Eight single-branch clones of a 1 GiB branch, a push into the storm.",
            ("b", "runner: 5 receive-packs, the request answered in 0 s, 5/5 durable; 8 upload-packs at the peak, all 8 at the tip, 51 s wall against 16 s alone."),
            ("r", "control: 4 receive-packs, the request queued 60 s, 1/5; 4 upload-packs, clones in pairs.")], FE)
    s.card(M + 2 * (cw4 + GAP), y0, cw4, 210, "Keepalives through the front — S9, and its control S8",
           ["20 GiB push, a 232 s hook wait, every packet the client read stamped by git's own trace.",
            ("b", "runner: 48 packets, the first 5.8 s after the pack, longest gap 5.8 s."),
            ("r", "control: 49 packets in ONE burst with the report, 237.9 s after the pack."),
            "S8: keepalive off and the door at 30 s — the connection closed 30.5 s after the upload; the batch landed anyway."], FE)
    s.card(M + 3 * (cw4 + GAP), y0, cw4, 210, "The storm lever — bundle URIs (F8, 2026-09-04)",
           ["A thousand clones are ~130 CPU-s but 43 GB from one NIC: egress binds first. A bundle is cut on a floor, uploaded beside the packs, advertised as a presigned URL.",
            ("b", "1,000 clones: 5.7 MiB of server egress with the client opted in (transfer.bundleURI=true), 40,409 MiB without — 7,000×."),
            "Gitea on the same corpus, 100 clones: 4,019 MiB, level with forge's control arms."], FE)
    s.legend(M + 4, 872, [(FE, "data plane"), ("red", "the control arm's failure — the class the runner removed")])
    return s


# ═════════════════════════════════════════════════════════════════════════
# 3 · the durable path
# ═════════════════════════════════════════════════════════════════════════
def plate_03():
    s = Plate(W, H, "The durable path: a push as a transaction. proc-receive hands the commands to the syncer and waits; "
                    "the syncer judges under the agreed view, renews the lease once, uploads every complete pack as an "
                    "immutable content-named object (multipart above 64 MiB with the CRC accumulated as the parts are read), "
                    "CASes one snapshot on the etag it last saw, applies the ref transaction and only then reports ok. S3 holds "
                    "the packs, the snapshot, the lease cell, bundles, LFS objects and the export. The theorems the model checks, "
                    "the one residual, and how a fresh pod restores from the bucket alone.")
    # the transaction band
    s.box(M, 22, W - 2 * M, 300, None, "panel")
    s.text(M + 22, 48, "One batch, one transaction — receive-pack serialises nothing under proc-receive, so the syncer does", "t1")
    steps = [("collect and judge", "every pending push, in arrival order, against the local refs, the snapshot AND the syncer's running view; old-oid must match both or ng stale, fetch first; fast-forward and policy here, since receive-pack no longer checks them; a refs/for/<target> runs the merge and packs what it created", "durable"),
             ("renew the lease, once", "epoch_renew with If-Match on our token; a 412 takes the lost-response rule (re-read; adopt if the cell still names us) else it is the fence: ng to every hook, stop serving reads too, exit", "control"),
             ("hash and upload the packs", "every local pack the snapshot does not name and whose .idx exists, with .idx .bitmap .rev: unconditional PUTs of content-named keys, siblings four in flight; multipart above 64 MiB, the CRC-64/NVME accumulated per part beside its PUT and judged by S3 at Complete", "durable"),
             ("ONE snapshot CAS", "If-Match on the etag this syncer last synced (If-None-Match:* when none): refs and the full pack list move as one object; a reader never sees half a batch. Under the lock a 412 can only be another server — the fence", "durable"),
             ("the ref transaction", "git update-ref --stdin applies every accepted ref locally as one transaction", "durable"),
             ("THEN the report", "ok or ng per ref to each waiting hook; per-ref ok reaches the client as it is emitted, so the report follows the whole transaction. A crash before this fails every push at the client; the restart restores from the snapshot", "durable"),
             ("after the report", "best effort, never a gate: derived info/packs then info/refs (in that order), the export, the sweep, a repack when the pack count passes the threshold", "durable")]
    sw = (W - 2 * M - 44 - 6 * 12) / 7
    for i, (head, body, plane) in enumerate(steps):
        x = M + 22 + i * (sw + 12)
        s.box(x, 66, sw, 240, FE, strip=False)
        s.numdot(x + 20, 88, i + 1, FE)
        s.para(x + 38, 92, head, sw - 50, "t2")
        yy = 92 + 20 * len(wrap(head, sw - 50, s.fs["t2"], bold=True))
        s.para(x + 14, yy + 2, body, sw - 28)
        if i < 6:
            s.arrow(x + sw, 180, x + sw + 12, 180, "ink")
    # lower band: S3 objects | theorems | restore | residual + measured
    cw = (W - 2 * M - 3 * GAP) / 4
    y0 = 340
    s.box(M, y0, cw, 440, None, "s3")
    s.text(M + 18, y0 + 26, "What S3 holds — <prefix>/", "t1")
    objs = [("git/objects/pack/pack-<sha>.pack", "immutable, content-named by git; with .idx .bitmap .rev beside it; unconditional PUT"),
            ("git/snapshot", "THE pointer: {seq, epoch, refs{name: oid}, packs[], bundles[], exported_commit}; CAS'd by etag; the only mutable object the server trusts"),
            ("git/epoch", "the single-writer lease cell: {epoch, token, holder, released}; every write a CAS"),
            ("git/claim", "the operator's project claim: a foreign projectId refuses the start"),
            ("git/objects/info/packs · git/info/refs · git/HEAD", "derived, for the dumb protocol and for humans; the server never reads them"),
            ("git/bundles/<oid>.bundle", "clone bundles, listed in the snapshot, advertised presigned"),
            ("lfs/objects/<oid>", "LFS objects, presigned both ways: the pod sees a few hundred bytes of JSON"),
            ("files/<path> + .flint/lean/", "the export: a valid lean workspace, published after the report by the shipped flint-sync barrier")]
    y = y0 + 50
    for key, what in objs:
        for ln in wrap(key, cw - 36, s.fs["mono"], mono=True):
            s.text(M + 18, y, ln, "mono")
            y += 15
        y = s.para(M + 18, y, what, cw - 36) + 3
    s.text(M + 18, y + 2, "Versioning OFF: nothing pins a version", "t4b")
    s.card(M + cw + GAP, y0, cw, 440, "The theorems — ForgeSync.tla, eight runs in the gate",
           ["Two syncers, two pushes, the batch at store-request granularity with a crash between any two steps, the renewer's sensor kept distinct from real movement, a challenger, a successor's claim, rotation, sweep and restore, git's .idx-last migration, a client that can hang up.",
            ("h", "Invariants — strict run, 2,776,804 states, depth 52"),
            ("b", "Told ok ⇒ the ref landed in the snapshot and the pack is in the bucket WITH its index."),
            ("b", "Every landed pack is complete with its index."),
            ("b", "The renewer never skips a heartbeat while the holder moved: the sensor is honest."),
            ("b", "No CAS lands from a deposed syncer after its successor restored."),
            ("b", "A restore never refuses: the bucket is always restorable."),
            ("h", "Mutations that must each find their loss"),
            "early ack (option B1) · no index gate (X1) · the checksum pass ticking nothing (run 3) · no rotation · the CAS before the packs (§4 reversed).",
            ("h", "What the model found"),
            "Two rotation gaps in the code (X11, X12), fixed the same day with tests; then two gaps in itself, found by the liveness run."], None, "panel")
    s.card(M + 2 * (cw + GAP), y0, cw, 440, "Restore — a fresh pod from the bucket alone",
           ["Claim. Acquire the lease. Rotate the snapshot (seq + 1, same content; created empty if none) so any straggler's If-Match is stale before this pod serves a byte. Sweep every in-flight multipart upload: nothing of ours is in flight at this moment.",
            "GET the snapshot; fetch the packs it names with .idx .bitmap .rev — one bounded fan-out across files and 8 MiB ranged chunks, 38-40 MiB of memory flat in the pack size, temporaries renamed only when every chunk has landed; write packed-refs and HEAD to EXACTLY the snapshot's refs; git fsck --connectivity-only; open the socket; serve.",
            ("b", "A named pack the bucket lacks: re-read the snapshot once, then refuse loudly (exit 78, judged before the pod phase) rather than serve a repository git cannot see."),
            ("b", "Measured: 40 GiB restored in 139 s from the delete (297 MiB/s), refs exactly the snapshot's, fsck clean, anon RSS 25 MiB; 1 GiB in 9 s.")], FE)
    s.card(M + 3 * (cw + GAP), y0, cw, 440, "The residual, and what the drills held",
           [("h", "Told failed, but durable"),
            "A client that hangs up before the report is never noticed by the syncer: the batch completes and the bucket names the pushed tip while the client saw a failed push. Not a loss and not a corruption — the retry finds the ref already there — but a transition the argument carries in the other direction, kept as a required-fail probe in the model and seen on the wire (run 3; run 7's S7 and S8).",
            ("h", "Acknowledged means durable, on real S3"),
            ("b", "runbx: 40 GiB push acknowledged in 1113 s, 641 parts, the CRC accepted at Complete, the snapshot at the tip; 40 of 40 pushes told ok were in the bucket across both takeover arms."),
            ("b", "S4, four kills placed INSIDE the multipart upload by watching list-multipart-uploads: told failed ⇒ the bucket unchanged; told ok ⇒ durable; every orphaned upload swept once the successor served."),
            ("h", "The one write the pack directory has"),
            "git's own gc is off. The syncer repacks under its lock between batches, uploads the new pack, CASes a snapshot naming only it, then sweeps what no snapshot names — after a listing, after re-reading the snapshot, after a HEAD past a grace that outlives the longest upload. Measured cost: a full repack re-uploads the repository (every 24 pushes at the shipped threshold); geometric repack is the lever and needs a multi-pack bitmap."], FE)
    s.box(M, 796, W - 2 * M, 62, None, "panel")
    s.text(M + 22, 820, "What is NOT on this path", "t2")
    s.para(M + 22, 838, "The runner and the door carry bytes and never write the bucket; the operator never reads it; the export and the derived dumb-protocol files are written after the report and are never a gate; a refused push's pack is never uploaded. Everything that can change what a restore sees goes through steps 3 and 4, under one lock, in one process.", W - 2 * M - 44)
    s.legend(M + 4, 872, [("data", "durable path"), ("ctl", "the lease, on the control plane")])
    return s


# ═════════════════════════════════════════════════════════════════════════
# 4 · the lease
# ═════════════════════════════════════════════════════════════════════════
def plate_04():
    s = Plate(W, H, "The single-writer lease. A holder's life: claim the epoch cell by CAS, rotate the snapshot, sweep, "
                    "restore, serve with a renewal every 10 s, and during a restore or a push renew only if the byte counter "
                    "moved; a 412 on any CAS of ours is the fence — stop reads too, exit. A challenger polls the cell and takes "
                    "over only after six consecutive polls saw the same token; a clean release on SIGTERM lets the successor "
                    "claim at once. What the model found in the rotation, and what the drills measured against a real "
                    "challenger.")
    # state boxes — the holder's life, left to right
    bw, bh, y = 178, 118, 70
    states = [("claim", "epoch_acquire: If-None-Match:* on an empty cell, or If-Match(observed token) with epoch + 1 once the token has been quiet for QUIET_POLLS; a released mark claims at once", "control"),
              ("rotate", "the snapshot CAS'd to seq + 1 with the same content — created empty when none exists — so every If-Match a straggler holds is stale before we serve", "durable"),
              ("sweep", "abort every in-flight multipart upload under the prefix: at this moment nothing of ours is pending, so what is pending is a predecessor's", "durable"),
              ("restore", "fetch the packs the snapshot names, install its refs exactly, fsck; the renewer beats through it if the byte counter moves", "durable"),
              ("serving", "renew every 10 s unconditionally; reads and writes served; /status says serving", "control"),
              ("pushing / importing", "the batch or the restore: the renewer renews ONLY if progress moved since its last renewal — a wedged pod lets the token go quiet on purpose", "control"),
              ("fenced", "a 412 on renew or on the snapshot CAS: ng to every waiting hook, stop serving reads as well as writes, exit; the pod restarts and claims again", "control"),
              ("released", "SIGTERM between batches: the cell's released mark is written, a successor claims without waiting out the polls; queued pushes are told ng by the closed sockets", "control")]
    sw = (W - 2 * M - 7 * 14) / 8
    for i, (head, body, plane) in enumerate(states):
        x = M + i * (sw + 14)
        tone = "warn" if head == "fenced" else None
        s.box(x, y, sw, 150, FE, tone, strip=(plane == "control" and tone is None))
        s.text(x + 14, y + 22, head, "t2 c-red" if tone else (f"t2 c-{FE}" if plane == "control" else "t2"))
        s.para(x + 14, y + 42, body, sw - 28)
        if i < 5:
            s.arrow(x + sw, y + 60, x + sw + 14, y + 60, "ctl" if False else None)
    s.text(M, 44, "The holder's life — the lease cell is FlintTierEpoch's, and every write to it is a CAS", "t1")
    s.arrow(M + 4 * (sw + 14) + sw, y + 100, M + 5 * (sw + 14), y + 100, None, both=True)
    s.text(M + 5 * (sw + 14) - 7, y + 92, "a batch", "t4", "middle")
    # 412 paths to fenced
    s.path(f"M{M + 5 * (sw + 14) + sw:.0f},{y + 130:.0f} H{M + 6 * (sw + 14):.0f}", "red")
    s.alabel(M + 6 * (sw + 14) - 7, y + 148, "412", "red")
    s.path(f"M{M + 4 * (sw + 14) + sw / 2:.0f},{y + 150:.0f} V{y + 178:.0f} H{M + 7 * (sw + 14) + sw / 2:.0f} V{y + 152:.0f}", None, dashed=True)
    s.alabel(M + 6 * (sw + 14), y + 190, "SIGTERM", None, "between batches only")
    # the challenger and the renewer
    cw = (W - 2 * M - 2 * GAP) / 3
    y0 = 262
    s.card(M, y0, cw, 236, "The challenger — a second syncer for the same repository",
           ["A roll against a wedged pod, a takeover of a dead node, or a hand-made challenger. It polls the cell every heartbeat and judges the holder dead ONLY by an unchanged token across six consecutive polls (60 s), then acquires with If-Match on that token and epoch + 1.",
            ("b", "A renewing holder is undeposable, structurally: its own renew rotates the token, so the challenger never counts six. The scheduling axiom the model states and cannot discharge: the polls and the heartbeats share a period."),
            ("b", "runbx, the seize arm CLOSED: a challenger beside a live 40 GiB restore for 398 s never claimed, the holder was never fenced, 24/24 pushes durable under the contention.")], FE)
    s.card(M + cw + GAP, y0, cw, 236, "The renewer's gate — a sensor, and a sensor can lie",
           ["The renewer is its own task from the moment the claim lands. While the phase must progress (importing, pushing) it renews only if the operation's byte counter advanced since its last renewal; renewing for a wedged pod would trade a live pod losing its repository for a dead one keeping it forever.",
            ("r", "Run 3: the checksum pre-pass over a 40 GiB pack (~70 s) did work and ticked nothing, so the token sat quiet for a whole takeover window inside a live push. The pass now ticks; then it was removed (D4: the CRC is accumulated as the parts are read)."),
            ("b", "The model keeps sensorMoved and realMoved apart and proves the sensor honest; the mutation that ticks nothing on the hash must lose.")], FE)
    s.card(M + 2 * (cw + GAP), y0, cw, 236, "What the model found in the rotation — X11, X12",
           [("b", "X11: a takeover of a repository nobody had published skipped the rotation (\"the first CAS's If-None-Match is the fence\")."),
            "A straggler mid-batch on the old epoch landed its If-None-Match:* create after the successor served, and the successor's own first CAS was what 412'd. The rotation now creates the empty snapshot.",
            ("b", "X12: a successor that died between its takeover CAS and its rotation came back through self-recognition, which skipped the rotation."),
            "The straggler from the epoch before still held a valid If-Match. Every claim but a released cell's now rotates: only a releaser has proven it fenced itself before writing the mark. Both fixed with tests that fail against the old code."], FE)
    # measured + the lifecycle numbers
    y1 = 516
    s.card(M, y1, cw, 316, "Measured on the wire — the window, before and after",
           [("h", "runbw, before the renewer task (window OPEN)"),
            "10 GiB push acknowledged in 262 s with the token silent 125 s; the cold restore 136 s with the token silent 141 s; a challenger claimed the live, importing pod 62 s after arriving; the two seized each other through epochs 4, 5, 6 until the challenger was removed at +392 s; 21 of 23 pushes acknowledged and every one in the bucket. The fence held and durability held; the cost was availability.",
            ("h", "runbx, after (window CLOSED)"),
            ("b", "40 GiB push: 1113 s, the hook wait over 8 minutes, token silent ≤ 11 s; restore 139 s at 297 MiB/s, silent 11 s; the seize arm never claimed; run 5 at 10 GiB: silent ≤ 11 s, sampler blind 0 s."),
            ("h", "A blind spot is not silence"),
            "The sampler's head-object hung 64 s on the CLI's own timeouts and read as 66 s of a quiet renewer. It now marks a sample blind, counts silence only across contiguous observations, and calls a blind spot past the bound inconclusive."], FE)
    s.card(M + cw + GAP, y1, cw, 316, "Numbers that shape the lease",
           [("m", "heartbeat 10 s · QUIET_POLLS 6 · window 60 s"),
            ("m", "renew: If-Match(own token) → new token"),
            ("m", "claim: If-Match(quiet token), epoch + 1"),
            ("m", "release: released = true, token rotated"),
            ("m", "orphan grace: outlives the longest upload"),
            ("gap",),
            "A takeover costs one small CAS on the snapshot plus the restore. A clean roll costs one request: the successor claims the released cell at once. A crash costs the quiet polls, then the restore.",
            ("b", "Nothing pins a straggler's writes: its packs are content-named and unnamed by any snapshot, so a straggler that completes an upload after the sweep leaves an object the next sweep removes — hygiene, not integrity, which is why no mutation run exists for the sweep."),
            "In-flight multipart parts are billed until aborted: one interrupted 2 GiB push left 384 MiB of parts on runbw. The sweep runs at the claim and between batches, with no grace, because at those two moments nothing of ours is in flight."], FE)
    s.card(M + 2 * (cw + GAP), y1, cw, 316, "How the lease relates to the other two planes",
           ["The lease cell is coordination — the control plane — and the fence it produces is what the durable path relies on: a 412 on any CAS of ours stops acknowledging AND stops serving reads. Pushes fail while S3 is unreachable; clones keep working for as long as the outage lasts, because the holder has no term of its own — a renewal that errors without a 412 is not judged (X13, found by the Continuity comparison).",
            "The operator does not read the cell. It takes the pod's word for its phase through /status; restart is not an operator decision — a fence exits 1, a refusal exits 78, and the kubelet restarts the pod.",
            ("b", "Two servers for one repository is not a state the Deployment (Recreate) creates on its own; it takes a roll against a wedged pod, a lost node whose pod is still counted, or a hand. Every such case ends with the straggler's next CAS 412ing."),
            ("r", "Open: X6 — a rollout during a long push SIGKILLs the batch at 30 s (measured, run 7: told failed, cleanly, the push lost); whether a roll should wait for the batch is the decision.")], FE)
    s.legend(M + 4, 872, [("ctl", "control plane"), ("data", "durable path"), ("red", "the fence / a hazard")])
    return s


# ═════════════════════════════════════════════════════════════════════════
# 5 · the control plane
# ═════════════════════════════════════════════════════════════════════════
def plate_05():
    s = Plate(W, H, "The control plane. The operator watches FlintRepo objects and renders each into a ConfigMap carrying the "
                    "policy, a headless Service, a one-pod Deployment and, when told where the door runs, a NetworkPolicy; it "
                    "polls the pod's own /status for its phase, computes Ready, and parks an idle repository at replicas 0. "
                    "The door admits by TokenReview and consumers, routes by repository, and wakes a parked one by annotating "
                    "the CR and waiting on it. The lease cell coordinates writers. None of it reads the bucket, and the "
                    "runner touched none of it.")
    # operator loop
    s.text(M, 44, "The operator — one FlintRepo becomes three objects and one idle rung", "t1")
    ow = 250
    s.card(M, 58, ow, 190, "FlintRepo (CRD, short name fr)",
           [("m", "spec.bucket · spec.keyPrefix"), ("m", "spec.consumers.serviceAccounts"), ("m", "spec.branches: protected,"), ("m", "  pushers, agentPattern,"), ("m", "  mergeInto"), ("m", "spec.suspendAfterSecs"),
            "keyPrefix is immutable: a new prefix is a new repository"], FE)
    s.arrow(M + ow, 150, M + ow + 26, 150, None, dashed=True)
    s.card(M + ow + 30, 58, 300, 190, "reconcile",
           ["claim git/claim with this project's id (a foreign id: Refused, exit 78)",
            "render a ConfigMap (the policy document pre-receive and the syncer both read), a headless Service, a Deployment of one pod — syncer + git container, Recreate, emptyDir, 30 s grace — and a NetworkPolicy admitting the door's pods only",
            "apply the children; poll /status; compute the phase; write conditions"], FE)
    s.arrow(M + ow + 330, 150, M + ow + 356, 150, None, dashed=True)
    s.card(M + ow + 360, 58, 300, 190, "/status — the pod's own word",
           [("m", "{phase, activity.idleSecs, rpoClean,"), ("m", " epoch.lastRenewUnix, fenced, progress}"),
            "phases: starting · importing · serving · pushing · draining · released. Ready = serving. Read only from the operators' namespace; the door refuses to proxy it.",
            ("b", "the operator consumes phase and idleSecs; nothing reads lastRenewUnix, fenced, rpoClean or progress")], FE)
    s.arrow(M + ow + 660, 150, M + ow + 686, 150, None, dashed=True)
    s.card(M + ow + 690, 58, W - M - (M + ow + 690), 190, "the idle rung — replicas 0",
           ["no git traffic for suspendAfterSecs, judged on the polled document only (a poll failure holds forever) ⇒ replicas 0 and chert.us/idle-state; the emptyDir is gone, the repository is bucket objects and nothing else",
            ("r", "X8: the activity clock counts pushes only — a fetch never reaches the syncer — so a clone-only repository is parked one threshold after its wake"),
            ("r", "X7: a failed poll with a Ready pod yields Starting, and the door waits on it")], FE)
    # the door
    s.text(M, 284, "The door — admission, routing, wake", "t1")
    dw = (W - 2 * M - 4 * GAP) / 5
    door = [("1 · authenticate", "Basic x:<token> → TokenReview at the apiserver, the verdict cached ≤ 60 s by token hash; a refusal is cached, a transport failure is not. A clone is two to four requests, so 1,000 clones would otherwise be 3–4,000 reviews."),
            ("2 · authorise", "spec.consumers must list the ServiceAccount; a credential-less peer gets 401 and is never dialled — authentication precedes the wake, so a peer cannot scale a parked repository up."),
            ("3 · route", "one Service per FlintRepo, by name; the door's URL formula /git/<ns>/<repo>.git; X-Remote-User = system:serviceaccount:<ns>:<sa> is set from the verified token and cannot be smuggled past the allowlist."),
            ("4 · wake", "a parked repository: arm chert.us/requested-at on the CR, wait on the CR — never the pod — up to 180 s (git clients do not retry a 503); the restore is the packs the snapshot names."),
            ("5 · bound", "no bytes either way for 300 s cuts the request; a wake holds a slot ≤ 180 s. HTTPS at the door is a line in the diagram: nothing in-tree terminates it.")]
    for i, (head, body) in enumerate(door):
        x = M + i * (dw + GAP)
        s.card(x, 298, dw, 132, head, [body], FE)
    # planes interplay + never + open
    cw = (W - 2 * M - 2 * GAP) / 3
    y0 = 452
    s.card(M, y0, cw, 286, "What the control plane never touches",
           [("b", "The bucket. The operator holds no S3 credential and reads no object; the door holds none either."),
            "A credential-free principal that could read the bucket would become a read-everything one for fields no decision reads — which is why C2 (the operator reading the bucket) was declined in the simplification note.",
            ("b", "Restart. A fence exits 1, a refusal exits 78 judged before the pod phase, and the kubelet restarts the pod; the claim-against-a-healthy-holder case is the lease's, not the operator's."),
            ("b", "The transfer. No control-plane request sits in a clone or a push; the TCP liveness probe reaches the status listener on its own task and cannot kill a batch."),
            "A3 changed none of this: the chart's server.tag, the CRD, the door's URL formula and every rig were untouched by the runner."], None, "panel")
    s.card(M + cw + GAP, y0, cw, 286, "Where the planes meet",
           [("h", "control → data"), "the door's admission and routing decide which bytes flow at all; the wake creates the pod the bytes will reach; the NetworkPolicy is what makes X-Remote-User a principal.",
            ("h", "control → durable"), "the lease cell's fence is what stops a deposed syncer from writing; the claim cell refuses a foreign project; the grace period decides whether a roll lands as a clean release or a SIGKILL mid-batch (X6).",
            ("h", "durable → control"), "/status is written by the syncer; the operator believes it; rpoClean is always true by construction because nothing is acknowledged before the CAS.",
            ("h", "data → durable"), "only through the hook: proc-receive is the one place the data plane hands anything to the writer, and it hands a request, never a credential."], FE)
    s.card(M + 2 * (cw + GAP), y0, cw, 286, "Open on the control plane — recorded, not hidden",
           [("r", "X6 grace: a rollout during a long push SIGKILLs the batch at 30 s. Measured on run 7: told failed, bucket unchanged, retry converges, orphans swept — clean, and the push is lost. Decide whether a roll waits for the batch."),
            ("r", "X7 a failed /status poll with a Ready pod reads as Starting and takes a live repository out of rotation on a blind poll."),
            ("r", "X8 the idle clock counts pushes only; clone-only repositories are parked and rewoken."),
            ("r", "X9 readiness is Serving-only, so Pushing answers 503 and the headless DNS is withdrawn during a long push — unverified."),
            "Not on the list because it is decided: KEDA's HTTP add-on could replace routing, wake and idle-to-zero with no flint code but not the auth, and the gateway exists (decision 12)."], None, "warn")
    s.box(M, 756, W - 2 * M, 100, None, "panel")
    s.text(M + 22, 782, "What the control plane has been through", "t2")
    yy = s.para(M + 22, 800, "The wake: an 11 s clone through the door on a pod the request created (F8's neighbour, EC2). The published artifact: the shipped chart could not install itself — it pinned the one image whose gateway rejected --git-only — found by a drill that installs the chart as a user would, which nothing had done; one server.tag now names both server images and the operator warns when the two references it is handed disagree (X10). The idle rung: the lite operator's ladder cut to one rung by copy-and-trim, not import, because ~800 of its lines are PVC and hibernate logic that is dead under an emptyDir cache.", W - 2 * M - 44)
    s.legend(M + 4, 872, [("ctl", "control plane"), (FE, "data plane"), ("red", "open item / hazard")])
    return s


# ═════════════════════════════════════════════════════════════════════════
# 6 · boundaries, composition, the record
# ═════════════════════════════════════════════════════════════════════════
def plate_06():
    s = Plate(W, H, "Boundaries and the record. The credential boundary: the bucket key exists in the syncer container only. "
                    "The principal boundary: X-Remote-User means something only behind the NetworkPolicy that admits the door. "
                    "Per-principal policy on the wire. Composition on one bucket with lean and passthrough and the four accepted "
                    "conditions. Then every campaign that stands behind this document, with its numbers.")
    cw = (W - 2 * M - 2 * GAP) / 3
    s.card(M, 22, cw, 268, "Three boundaries, and what each one actually holds",
           [("h", "The credential boundary"), "AWS keys reach the syncer container by envFrom and nothing else: not the git container, not the door, not the operator, not an agent. The process that parses untrusted HTTP cannot write the bucket.",
            ("h", "The principal boundary"), "REMOTE_USER is the door's X-Remote-User, set from a verified TokenReview. Reached directly, the git port believes the header — so the operator renders a NetworkPolicy admitting the door's pods only, and reaching the port is the authorisation where it is not rendered.",
            ("r", "Measured (run-rights.sh, Cilium): with the policy in place a forged header cannot open 8080; with it deleted the same header merges into main. 17 legs, green, twice. kind's default CNI enforces no policy at all."),
            ("h", "The wire"), "two cleartext hops as built; the pod's audience-bound token rides both as a Basic password; server-to-S3 is TLS."], None, "warn")
    s.card(M + cw + GAP, 22, cw, 268, "Policy — who moves main, enforced twice",
           [("m", "protected: [main, release/*]"), ("m", "pushers: {main: [release-bot]}"), ("m", "agentPattern: agent/*"), ("m", "mergeInto: {main: [agent-runner]}"),
            "pre-receive names the rule at the edge; the syncer enforces the same document in its judge step, so a missing hook does not open the repository.",
            ("b", "Merge is a push: HEAD:refs/for/main runs merge-tree --write-tree in the syncer, packs what it created, and the batch CAS carries main; refs/for is never stored. One path to S3, structurally."),
            "Agents never hold an S3 credential; humans use the gateway's bearer today. The Rights axis of the radar has three of seven cells on the wire: passthrough per-pod readOnly, forge per-SA policy and the NetworkPolicy boundary."], FE)
    s.card(M + 2 * (cw + GAP), 22, cw, 268, "Composition on one bucket — with lean and passthrough",
           ["The export publishes main as a lean workspace after the report; lean and passthrough readers mount it read-only with no forge code in them. Disjoint prefixes and disjoint lease cells.",
            ("h", "Accepted conditions (C1–C5, 2026-09-05)"),
            ("r", "A1 forge and lean on one prefix do not arbitrate: detection shipped (flint_store::layout), prevention belongs to whatever assigns prefixes."),
            ("r", "A2 a nested export prefix is admitted; the sweep lists only pack/ and bundles/, so nothing is destroyed."),
            ("r", "A3 a foreign write into the export is neither seen nor repaired; A4 a reader with no manifest takes the foreign bytes. Declined: it needs a second writer on the export prefix, a misconfiguration."),
            "Fixed: C2 a second writer on the export prefix wedged the repository; C4 the manifest-checking readers refuse."], FE)
    # the record
    s.box(M, 306, W - 2 * M, 356, None, "panel")
    s.text(M + 22, 332, "The record — every campaign behind this document, 2026-09-04 → 2026-09-05", "t1")
    rows = [("falsifiers F1–F11, EC2", "acknowledged-means-durable under 8 mid-push kills; two pushes to one ref; a merge surviving a cold restore; the fence deposing a straggler within one heartbeat; a byte-identical restore and a dumb clone with the server down; protected main (found a fresh repository unable to create main); idle-to-zero with an 11 s clone through the door on a pod the request created; the storm (5.7 MiB vs 40,409 MiB); the export; the sweep; an S3 outage — clones served, pushes refused."),
            ("composition C1–C5, MinIO rig", "two fixed (a second writer wedging the repository; readers taking foreign bytes), three accepted by decision, each leg reporting KNOWN and turning STALE if its condition stops reproducing."),
            ("rights, kind + Cilium", "17 legs: a reader refused a direct push and a merge, a writer merging, a forged header overridden at the door and believed at the port without the policy."),
            ("latency rig, toxiproxy", "a push costs 5.1 round trips + 225 ms with the sibling fan-out and 7.1 without; a 33-file restore 14.3 round trips against 38.6 serial; the renewer through an 8.8 s restore: 7 rotations, longest silence 1.2 s."),
            ("runbw, 3 × i4i.xlarge, real S3", "10 GiB composed as 161 parts, CRC accepted; the token silent 125 s in the push and 141 s in the restore — the window OPEN; a challenger seized a live restoring pod at 62 s; every acknowledged push in the bucket; one interrupted upload left 384 MiB of billed parts."),
            ("runbx, the confirmation", "40 GiB pushed in 1113 s and restored in 139 s with the token never silent past 11 s; the seize arm closed against a real challenger; told-ok ⇒ durable 40/40; three front-layer defects found in nginx and fcgiwrap — the case for A3 — and a rig sampler that lied."),
            ("runby, the runner's acceptance", "S5–S10 through flint-forge-gitcgi: a 70 s client stall acknowledged; five pushes and eight clones served concurrently; 48 keepalives across a 232 s hook wait with a 5.8 s longest gap; a rollout mid-push told failed cleanly. The nginx control failed exactly those legs: 502 at 60 s, four workers, one burst, four upload-packs."),
            ("the model, ForgeSync.tla", "2,776,804 states strict; five mutations that must lose; a required-fail probe; two code defects found in the rotation and two model defects found by the liveness run, all the same day.")]
    lab_w = 250
    y = 350
    for i, (head, body) in enumerate(rows):
        lines = wrap(body, W - 2 * M - 44 - lab_w - 20, s.fs["t4"])
        rh = max(30, 12 + 15 * len(lines))
        if i % 2:
            s.rect(M + 22, y - 4, W - 2 * M - 44, rh, "rowb", rx=0)
        s.text(M + 32, y + 14, head, "t2")
        yy = y + 14
        for ln in lines:
            s.text(M + 32 + lab_w, yy, ln, "t4")
            yy += 15
        s.hair(M + 22, y - 4 + rh, W - M - 22, y - 4 + rh)
        y += rh
    s.card(M, 690, W - 2 * M, 166, "Not drilled, not built — the edges as they stand",
           ["Agents in another cluster (the door is reachable from one). Production: nothing here has served a team; every number is a rig's. A bought forge on a flint volume: the buy-option control ran on a local disk, so \"git over NFS is slow\" is unmeasured. Read replicas: one pod serves each repository, and the storm lever is the bucket, not a second server. A replayable log: the snapshot is state, not history; provenance is git's own.",
            ("r", "Known and kept: told-failed-but-durable (the retry converges); a rollout mid-push loses the push (X6); the export is a mirror nobody repairs (A3); a reader with no manifest takes foreign bytes (A4); the 30 s grace, the push-only idle clock (X8) and the blind-poll Starting (X7) are open; geometric repack needs a multi-pack bitmap before the re-upload-everything cost of a full repack goes away."),
            "What the model does not carry: time (the six-quiet-polls window is a stated axiom), sizes, the door, the operator, and git's own correctness."], None, "warn")
    s.legend(M + 4, 872, [(FE, "forge"), ("red", "boundary / accepted condition / edge")])
    return s


# ═════════════════════════════════════════════════════════════════════════
# 7 · prior art
# ═════════════════════════════════════════════════════════════════════════
def plate_07():
    s = Plate(W, H, "Prior art beside forge, dimension by dimension: GitHub Spokes, GitLab Gitaly, AWS CodeCommit, Palantir's "
                    "Stemma on JGit's DFS backend, Cursor's Continuity and the walgit server built on it, and JGit's S3 dumb "
                    "transport — against flint-forge on the durable substrate, what serves git, the unit of durability, the "
                    "acknowledgement, writer coordination, the disk's role, restore and idle, read scaling, identity and how "
                    "each is verified.")
    header = [("GitHub Spokes", "lbl"), ("GitLab Gitaly", "lbl"), ("AWS CodeCommit", "lbl"), ("Stemma / JGit DFS", "lbl"),
              ("Continuity · walgit", "lbl"), ("flint-forge", FE)]
    colw = [222, 222, 222, 222, 232, 242]
    rows = [
        ("durable substrate", None,
         [["three replicas on local disks in three racks, replicated at the application level"],
          ["local SSD per Gitaly node, Praefect replicating; object storage for LFS and artifacts only"],
          ["packs in S3, metadata in DynamoDB; managed (closed 2024-07, GA again 2025-11-24)"],
          ["packs as blobs and refs as rows in a transactional store (AtlasDB; Google: Bigtable)"],
          ["a write-ahead log of per-push objects in S3/GCS plus a CAS'd index or manifest"],
          [("b", "content-named packs in S3 plus ONE CAS'd snapshot; nothing else is trusted")]]),
        ("what serves git", None,
         [["stock git per replica, behind a proxy"], ["stock git inside Gitaly, over gRPC"], ["proprietary"],
          ["JGit, in Java, on a DFS abstraction"], ["stock git on disk (Continuity); walgit: its own receive-pack in Rust, stock git for upload-pack, repack and bundles"],
          [("b", "stock git — receive-pack, upload-pack, hooks — behind a 382-line CGI runner; flint writes no git internals")]]),
        ("the write, and the ack", None,
         [["a git transaction on ≥ 2 of 3 replicas by three-phase commit; ok after that"],
          ["a transaction on the primary's disk, then replicated; ok after the primary"],
          ["a managed write; ok after it"], ["a database transaction over refs and pack ids; ok on commit"],
          ["one WAL entry — the packfile and its ref transaction — then the index CAS; \"never acknowledge a push until it has been fully persisted\""],
          [("b", "every complete pack uploaded, ONE snapshot CAS, the local ref transaction, THEN ok — carried by proc-receive, which waits on the syncer")]]),
        ("writer coordination", None,
         [["a proxy per push; consensus among the replicas"], ["Praefect elects a primary per repository"], ["the service"],
          ["database transactions"], ["the index/manifest CAS is the consensus: no leader, no election, any host may be primary"],
          [("b", "one writer per repository under an S3 epoch lease with a progress-gated renewer; every claim rotates the snapshot so a straggler's If-Match is stale first")]]),
        ("disk, idle, restore", None,
         [["the repository itself, three times; always on"], ["the repository itself; always on"], ["none visible; managed"],
          ["a DFS block cache; always on"], ["a warm cache materialised from the WAL; idle replicas evicted and rematerialised on the next fetch"],
          [("b", "an emptyDir cache restored from the snapshot at every start (40 GiB in 139 s); one pod per repository, parked at replicas 0, woken by the door")]]),
        ("reads and storms", None,
         [["three replicas serve reads"], ["Praefect distributes reads"], ["the service"], ["JGit caches"],
          ["read replicas kept current by conditional GET; linear to 100 replicas; 120 pushes/s on S3 Standard"],
          [("b", "one pod; the lever is bundle URIs on the bucket — 5.7 MiB against 40,409 MiB of egress for 1,000 clones")]]),
        ("identity and policy", None,
         [["the platform's"], ["the platform's"], ["IAM"], ["the platform's"], ["token or OIDC; per-repository push policy (walgit)"],
          [("b", "the pod's own ServiceAccount token, by TokenReview; consumers and a branch policy per principal, enforced twice; merge is a push")]]),
        ("verification", None,
         [["production at GitHub scale"], ["production at GitLab scale"], ["production since 2015"], ["production at Palantir"],
          ["production at Cursor; walgit: a seeded fault-injection simulation (crash, partition, stale, lost response), ~2.4k stars"],
          [("b", "a TLA+ model with mutations, eleven falsifiers, three campaigns on real S3, a control arm — and no production")]]),
    ]
    bottom = s.table(M, 22, colw, header, rows, 190)
    y0 = max(bottom + 14, 760)
    s.box(M, y0, W - 2 * M, 856 - y0, None, "panel")
    s.text(M + 22, y0 + 24, "The verdict", "t2")
    s.para(M + 22, y0 + 42, "Convergent, and independent: S3 as the only durable state, disk as a cache, one CAS'd pointer and stock git is a shape Cursor published on 2026-08-18 and forge reached on 2026-09-04, with CodeCommit (2015) and Stemma (2017) before both on other stores. Forge's own: the syncer as the sole writer behind stock receive-pack with the acknowledgement carried by proc-receive; the progress-gated lease with takeover rotation; a per-repository Kubernetes control plane with pod identity and idle-to-zero; the legible export; and the model-and-drill record. Not forge's: read replicas, a replayable log, a UI, review.", W - 2 * M - 44)
    return s


def main():
    plates = {"01-three-planes.svg": plate_01, "02-data-plane.svg": plate_02, "03-durable-path.svg": plate_03,
              "04-lease.svg": plate_04, "05-control-plane.svg": plate_05, "06-boundaries-record.svg": plate_06,
              "07-prior-art.svg": plate_07}
    for name, fn in plates.items():
        print(write(name, fn()))
    if kit.OVERFLOWS:
        print("\n".join("  OVERFLOW " + o for o in kit.OVERFLOWS))
        sys.exit(1)


if __name__ == "__main__":
    main()
