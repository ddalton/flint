#!/usr/bin/env python3
"""Build `flint-forge.vsdx` — the forge module as five Visio pages.

Sources: docs/architecture/forge/flint-forge-architecture.md and the code
it points at — forge/syncer/src/{lib,batch,packio,snapshot,gitcmd,lease,
pktline,hook}.rs and spdk-csi-driver/src/forge_operator/.

Every box's height is DERIVED from its text by `vsdxkit.Stack`, and each
enclosure is resized to its contents once they are placed, so the page
carries neither an overflowing label nor dead white space. Run with no
arguments to write the .vsdx and the gate's report; --preview also
writes an SVG per page, and --pdf renders those to one PDF, which is
how the result is LOOKED at without Visio. A non-zero exit means the
gate found something.
"""

import os
import re
import shutil
import subprocess
import sys

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import vsdxkit as k

# ---------------------------------------------------------------- palette
INK, SUB, MUTE = "#14181D", "#3D4650", "#6B7683"
PAPER = "#FFFFFF"

CLIENT_F, CLIENT_L = "#E4EEF9", "#2E6FB7"
DOOR_F, DOOR_L = "#DCF1EE", "#1F8C82"
GIT_F, GIT_L = "#FCE7D6", "#C0651A"
SYNC_F, SYNC_L = "#EAE1F7", "#6F45B5"
OPER_F, OPER_L = "#E3F1DA", "#4B8B2B"
S3_F, S3_L = "#FFF3D6", "#B0862A"
NOTE_F, NOTE_L = "#F6F7F9", "#C3C9D1"
WARN_F, WARN_L = "#FDE8E8", "#C0392B"
ZONE_L = "#AEB6C0"

FLOW, DUR, CTL, BYPASS = "#2E6FB7", "#6F45B5", "#4B8B2B", "#B0862A"

PAGE_W, PAGE_H = 30.0, 18.0


def header(p, num, title, standfirst):
    """The title band. Returns the y at which content may start."""
    p.text(0.45, 0.34, 8.0, "flint-forge  ·  %s / 5" % num,
           size=9.5, color=MUTE, bold=True)
    p.text(0.45, 0.62, PAGE_W - 0.9, title, size=19, color=INK, bold=True)
    s = p.text(0.45, 1.14, PAGE_W - 0.9, standfirst, size=9.5, color=SUB)
    return s.y + s.h + 0.18


def zone(p, x, y, w, label, tint="#FBFCFD"):
    """A dashed enclosure, drawn before its contents for z-order and
    resized to them afterwards. Returns (box, inner_y)."""
    z = p.box(x, y, w, 1.0, "", "", fill=tint, line=ZONE_L, dashed=True,
              rounding=0.12, line_weight=0.008)
    lab = p.text(x + 0.16, y + 0.11, w - 0.32, label, size=9.5,
                 color="#59636E", bold=True)
    return z, lab.y + lab.h + 0.12


def pill(p, cx, cy, n, color=FLOW):
    """A numbered flow marker, centred on (cx, cy)."""
    return p.box(cx - 0.15, cy - 0.15, 0.3, 0.3, str(n), "", fill=color,
                 line=color, title_size=9.5, title_color=PAPER,
                 rounding=0.15, halign=1, valign=1, ok_overlap=True)


# =====================================================================
# 1 · components and data flow
# =====================================================================
def page1(doc):
    p = doc.page("1 Components and data flow", PAGE_W, PAGE_H)
    top = header(
        p, 1, "Components and data flow",
        "A git server per repository: stock git does every git operation, S3 is the only durable state, and the syncer beside git "
        "is the only process that can write the bucket. Blue is the data plane (every byte of a clone or push), purple the durable "
        "path, green the control plane. Nothing on the data plane holds a bucket credential; the control plane never reads an object.")

    # the bypass lane gets its own strip above the enclosures, because the
    # bytes it carries are the ones that never enter them
    lane_y = top + 0.22
    p.text(8.6, top - 0.02, 15.5,
           "the bytes that never touch the pod — a bundle URI or an LFS object is a presigned S3 GET the client makes itself",
           size=7.6, color=BYPASS, bold=True)
    zone_y = lane_y + 0.4

    # ---- consumer clusters ------------------------------------------
    cz, cy = zone(p, 0.45, zone_y, 5.0, "Consumer Kubernetes cluster(s)")
    c = p.stack(0.75, cy, 4.4, gap=0.2)
    c.box("Agent pod — stock git",
          "git clone / fetch / push, smart protocol v2 over HTTPS. The "
          "credential helper presents the pod's OWN projected "
          "ServiceAccount token as the Basic password.\n"
          "No forge client library, no S3 credential, no git extension: a "
          "stock client ≥ 2.40 is the whole requirement.",
          fill=CLIENT_F, line=CLIENT_L)
    c.box("CI runner pod / build job",
          "Same door, same token shape. A clone storm is eight concurrent "
          "upload-packs — or none at all, when the client opts into the "
          "bundle URI (9).",
          fill=CLIENT_F, line=CLIENT_L)
    c.box("Application / browser — no git at all",
          "The REST file API: GET / PUT / DELETE one path, If-Match on the "
          "blob's etag, a blind overwrite refused with 428. The server "
          "commits on the client's behalf.",
          fill=CLIENT_F, line=CLIENT_L)
    c.box("Another cluster",
          "Not drilled off-cluster yet. Same door, same spec.consumers "
          "list: the boundary is the token, not the network.",
          fill=PAPER, line=ZONE_L, dashed=True, title_size=9.5,
          title_color=MUTE)
    c.box("What a client never learns",
          "Which packs exist, where the bucket is, which pod served it, or "
          "that there was a restore. A parked repository answers the same "
          "clone as a warm one, 7 s later.",
          fill=NOTE_F, line=NOTE_L, title_size=9.5)
    cz.resize_bottom(c.bottom + 0.3)

    # ---- the flint hub cluster --------------------------------------
    fz, fy = zone(p, 5.9, zone_y, 12.6, "flint hub cluster")

    ctl = p.stack(6.5, fy, 3.3, gap=0.24)
    # the wide gap under the apiserver is where flow marker 2 sits, on
    # the arrow rather than on a box's border
    api = ctl.box("kube-apiserver",
                  "TokenReview for the door; the CR, the Deployment and the "
                  "four rendered objects for the operator.",
                  fill=NOTE_F, line=MUTE, title_size=9.5, gap=0.55)
    door = ctl.box("flint-hub-gateway — “the door”",
                   "Stateless. FIVE STEPS, AND THE ORDER IS THE SECURITY "
                   "PROPERTY:\n"
                   "\n"
                   "1 · AUTHENTICATE — the Basic password is a token; "
                   "TokenReview at the apiserver, cached ≤ 60 s by the "
                   "token's hash. A refusal is cached, a transport failure "
                   "is not. A clone is 2–4 requests, so 1,000 clones would "
                   "otherwise be 3–4,000 reviews.\n"
                   "2 · AUTHORISE — spec.consumers must list the "
                   "ServiceAccount. A credential-less peer gets 401 and is "
                   "never dialled, so it cannot wake a parked repository.\n"
                   "3 · ROUTE — one headless Service per FlintRepo, by "
                   "name. X-Remote-User is set from the verified token and "
                   "cannot be smuggled past the allowlist.\n"
                   "4 · WAKE — annotate the CR and wait on the CR, never "
                   "the pod, up to 180 s. git clients do not retry a 503.\n"
                   "5 · BOUND — 300 s with no bytes either way cuts the "
                   "request. The one client-facing timeout, by design.\n"
                   "\n"
                   "TWO ROUTE TABLES, ONE LISTENER, ONE PORT:",
                   fill=DOOR_F, line=DOOR_L, title_size=10.5, gap=0.08)
    ctl.box("/git/<ns>/<repo>.git       →  git container",
            "", fill=PAPER, line=DOOR_L, title_size=7.8, title_mono=True,
            title_bold=False, title_color=SUB, rounding=0.04, gap=0.05)
    ctl.box("/repo/<ns>/<name>/files    →  syncer :9850",
            "", fill=PAPER, line=DOOR_L, title_size=7.8, title_mono=True,
            title_bold=False, title_color=SUB, rounding=0.04)
    oper = ctl.box("forge operator",
                   "Watches FlintRepo. Claims git/claim with this project's "
                   "id — a foreign id is Refused, exit 78, judged BEFORE "
                   "the pod's phase, so a refusal cannot read as a checkout "
                   "in progress.\n"
                   "\n"
                   "Renders four objects and applies them:\n"
                   "  · ConfigMap — the branch policy both enforcers read\n"
                   "  · headless Service\n"
                   "  · Deployment, replicas 1\n"
                   "  · NetworkPolicy — the door's pods alone\n"
                   "\n"
                   "Polls the pod's own /status and consumes exactly two "
                   "fields: phase (Ready = serving) and activity.idleSecs. "
                   "lastRenewUnix, fenced, rpoClean and progress are read "
                   "by nothing. Parks an idle repository at replicas 0 — "
                   "the emptyDir goes, the bucket stays.\n"
                   "\n"
                   "IT NEVER READS THE BUCKET, holds no S3 credential and "
                   "sits in no transfer, which is why the runner could be "
                   "swapped underneath it without changing a line of it.",
                   fill=OPER_F, line=OPER_L, title_size=10.5)

    # ---- the pod ----------------------------------------------------
    pz, py = zone(
        p, 10.05, fy - 0.28, 8.15,
        "Forge pod — ONE per FlintRepo  ·  Deployment replicas 1 → 0 when idle",
        "#FDFDFE")

    g = p.stack(10.35, py, 7.55, gap=0.2)
    gitc = g.box("container: git — the data plane, nothing durable",
                 "flint-forge-gitcgi (the runner) — execs `git "
                 "http-backend` once per request and streams the request "
                 "body in and its output back out as each arrives. 64 "
                 "concurrent, then 503 at once rather than a silent queue. "
                 "It sets NO timeout and NO body limit: the door owns the "
                 "client-facing bound. It replaced nginx + fcgiwrap, which "
                 "buffered the keepalives and cut a 40 GiB push 311 s into "
                 "its hook wait.\n"
                 "\n"
                 "  git upload-pack — serves clone/fetch out of the packs "
                 "on disk. uploadpack.allowFilter = true, so --filter is "
                 "honoured instead of silently ignored.\n"
                 "  git receive-pack — takes the push: index-pack "
                 "--fix-thin into a quarantine objdir, receive.fsckObjects, "
                 "and an empty sideband packet every 5 s while the hooks "
                 "wait.\n"
                 "\n"
                 "  hook pre-receive — the branch policy at the edge; "
                 "refuses the whole push, and its message names the rule.\n"
                 "  hook proc-receive — A RELAY AND NOTHING ELSE: reads "
                 "git's pkt-line command list, hands it to the syncer over "
                 "a Unix socket, waits, writes back the per-ref report.\n"
                 "\n"
                 "receive.procReceiveRefs = refs/ routes EVERY ref through "
                 "proc-receive, so git checks no old-oid and no "
                 "fast-forward — the syncer's judgement is the only one. No "
                 "process in this container holds a bucket credential.",
                 fill=GIT_F, line=GIT_L, title_size=10.5)

    disk = p.fitbox(10.35, g.y, 4.55, "emptyDir /repo — a CACHE, not state",
                    "The bare repository: objects/pack/*.pack and .idx, "
                    "refs, HEAD. Deleted with the pod and rebuilt from the "
                    "bucket. A pack on this disk is not durable until the "
                    "syncer has named it in the snapshot.",
                    fill="#F3F4F6", line=MUTE)
    p.text(15.05, g.y + 0.06, 1.95,
           "proc-receive hands the syncer a REQUEST over /run/forge.sock — "
           "never a credential — and blocks until the bucket has it.",
           size=7.4, color=DUR)
    g.skip(disk.h + 0.2)

    sync = g.box("container: syncer — the durable path, and it is ONE process",
                 "flint-forge-syncer. The ONLY holder of the S3 credential "
                 "(envFrom, this container alone) and the only writer of "
                 "the bucket. Everything that can change what a restore "
                 "sees happens here, under one writer lock, in one "
                 "process.\n"
                 "\n"
                 "  lease    an epoch cell in S3, moved only by CAS. Renew "
                 "every 10 s, gated on the operation's byte counter so a "
                 "wedged pod stops renewing; six consecutive quiet polls "
                 "before a challenger takes over; every claim rotates the "
                 "snapshot so a straggler's If-Match is already stale.\n"
                 "  batch    judge → renew → upload → ONE CAS → update-ref "
                 "→ report.  Page 2.\n"
                 "  packio   content-named PUTs, unconditional by "
                 "construction; multipart above 64 MiB with CRC-64/NVME "
                 "accumulated per part.  Page 3.\n"
                 "  fold     the tiers: consolidate small packs into bigger "
                 "ones. Supersede is object coverage, not reachability.\n"
                 "  bundle · lfs · export · the file API on :9850\n"
                 "  restore  rebuild the whole emptyDir from the snapshot "
                 "alone — 40 GiB in 139 s, 38–40 MiB of memory flat in the "
                 "pack size.\n"
                 "\n"
                 "It writes /status, and the operator believes it: rpoClean "
                 "is true by construction, because nothing is acknowledged "
                 "before the CAS.",
                 fill=SYNC_F, line=SYNC_L, title_size=10.5)
    pz.resize_bottom(g.bottom + 0.3)

    # the operator column is the longer of the two, so the space under the
    # pod goes to the open items rather than to nothing
    open_items = p.fitbox(
        10.05, pz.y + pz.h + 0.3, 8.15, "What is open on this page",
        "X6 — a rollout during a long push SIGKILLs the batch at the 30 s "
        "grace. Run 7 measured exactly what that costs: the client told "
        "failed, the bucket unchanged, the retry converging, the orphaned "
        "upload swept. Clean, and it loses the push. Whether a roll should "
        "WAIT for the batch is the decision still to take.\n"
        "X7 — a failed /status poll against a Ready pod reads as Starting, "
        "and takes a live repository out of rotation on a blind poll.\n"
        "X8 — the idle clock counts PUSHES only, because a fetch never "
        "reaches the syncer; so a clone-only repository is parked one "
        "threshold after its wake.\n"
        "\n"
        "Also recorded as a decision rather than a question: KEDA's HTTP "
        "add-on could replace routing, wake and idle-to-zero with no flint "
        "code — but not the auth, and the gateway already exists.",
        fill=NOTE_F, line=NOTE_L, title_size=9.5)
    fz.resize_bottom(max(ctl.bottom, open_items.y + open_items.h) + 0.3)

    # ---- S3 ----------------------------------------------------------
    sz, sy = zone(p, 18.95, zone_y, 10.6, "AWS S3 — the only durable state",
                  "#FFFDF6")
    s = p.stack(19.3, sy, 9.9, gap=0.2)
    s.box("One bucket, one prefix per repository",
          "Conditional writes are what make the pointer possible without a "
          "database: If-None-Match arrived in August 2024 and If-Match on "
          "the ETag in November 2024, and flint-store was written against "
          "them for lean before forge existed.",
          fill=S3_F, line=S3_L)
    s.box("IMMUTABLE — content-named by git, so the PUT is unconditional",
          "<prefix>/git/objects/pack/pack-<sha>.pack    the objects\n"
          "                          pack-<sha>.idx     THE GATE: a pack is\n"
          "                                             listed only if its\n"
          "                                             index is on disk\n"
          "                          pack-<sha>.rev\n"
          "                          pack-<sha>.bitmap\n"
          "<prefix>/git/log/<seq>.json     one entry per batch, written once\n"
          "<prefix>/git/undo/<seq>.json    X15: 7 days of undo points before\n"
          "                                a destructive push\n"
          "<prefix>/git/bundles/<name>     clone bundles, presigned out\n"
          "<prefix>/lfs/objects/<oid>      LFS, presigned both ways",
          fill=PAPER, line=S3_L, title_size=9.5, mono=True)
    s.box("MUTABLE — and there is exactly one",
          "<prefix>/git/snapshot    THE pointer:  version, seq, epoch,\n"
          "  refs{name→oid}, packs[], bundles[], exported_commit,\n"
          "  writer, unix\n"
          "\n"
          "Written ONLY by If-Match:<the etag we last read> — or by\n"
          "If-None-Match:* to create, which closes the create race\n"
          "exactly as If-Match closes the update race.\n"
          "Under the writer lock a 412 cannot mean “someone else's\n"
          "push”; it can only mean a second server holds this\n"
          "repository, which is a FENCE, not a retry.",
          fill="#FFF8E8", line=S3_L, title_size=9.5, mono=True)
    s.box("COORDINATION",
          "<prefix>/git/epoch    the lease cell — CAS only, by everyone\n"
          "<prefix>/git/claim    the operator's claim; the syncer READS it\n"
          "                      and never writes it",
          fill=PAPER, line=S3_L, title_size=9.5, mono=True)
    s.box("DERIVED — written for other readers, never read back",
          "<prefix>/git/info/refs             git's dumb protocol: a\n"
          "<prefix>/git/objects/info/packs    read-only clone straight\n"
          "<prefix>/git/HEAD                  out of the bucket\n"
          "<prefix>/export/…                  the legible workspace the\n"
          "                                   rest of flint mounts, with\n"
          "                                   no git in it",
          fill=PAPER, line=S3_L, title_size=9.5, mono=True)
    s.box("What the ordering buys",
          "Packs go up BEFORE the snapshot names them, so a crash between "
          "the two leaves an unnamed object the sweep removes — never a "
          "name without an object. The snapshot is one object carrying "
          "every ref and the full pack list, so a concurrent reader never "
          "sees half a batch. The local ref transaction follows the CAS, "
          "and the report follows the transaction.\n"
          "Five theorems hold over this in formal/ForgeSync.tla — "
          "2,776,804 distinct states to depth 52 — and five mutations must "
          "each lose one.",
          fill=NOTE_F, line=NOTE_L, title_size=9.5)
    s.box("One residual, kept rather than hidden",
          "A client that hangs up before the report is never noticed by "
          "the syncer, so the batch lands anyway and the bucket names a "
          "tip the client saw fail. Not a loss and not a corruption — the "
          "retry finds the ref already there — but the argument runs in "
          "one direction only. It is a required-fail probe in the model, "
          "and it was seen on the wire.",
          fill=WARN_F, line=WARN_L, title_size=9.5)
    s.box("Cold restore, from the bucket alone",
          "GET the snapshot → fan out 8 MiB ranged GETs over exactly the "
          "packs it names → install the refs → git fsck "
          "--connectivity-only → serve. Nothing else is consulted; the "
          "operator is not asked.",
          fill=S3_F, line=S3_L, title_size=9.5)
    sz.resize_bottom(s.bottom + 0.3)

    # ---- flows -------------------------------------------------------
    door_mid = door.y + 1.1
    # 1 client -> door
    p.arrow([(5.45, door_mid), (6.5, door_mid)], color=FLOW)
    pill(p, 5.98, door_mid - 0.42, 1, FLOW)
    p.text(5.5, door_mid - 0.96, 0.96, "clone / fetch\n/ push", size=7.2,
           color=FLOW, halign=1)

    # 2 door and operator -> apiserver
    p.arrow([(7.6, door.y), (7.6, api.y + api.h)], color=CTL, dashed=True)
    p.arrow([(6.5, oper.y + 0.35), (6.15, oper.y + 0.35),
             (6.15, api.y + api.h * 0.5), (6.5, api.y + api.h * 0.5)],
            color=CTL, dashed=True)
    pill(p, 8.35, api.y + api.h + 0.2, 2, CTL)

    # 3 door -> git container
    g_mid = gitc.y + 1.0
    p.arrow([(9.8, g_mid), (10.35, g_mid)], color=FLOW)
    pill(p, 10.07, g_mid - 0.4, 3, FLOW)

    # 4 door -> the syncer's file API
    f_mid = sync.y + 0.9
    p.arrow([(9.8, f_mid), (10.1, f_mid), (10.1, f_mid), (10.35, f_mid)],
            color=FLOW)
    pill(p, 10.07, f_mid - 0.4, 4, FLOW)

    # git <-> disk, disk <-> syncer, and proc-receive -> syncer beside them
    p.arrow([(11.6, gitc.y + gitc.h), (11.6, disk.y)], color=FLOW,
            begin_arrow=k.ARROW_FILLED)
    p.arrow([(11.6, disk.y + disk.h), (11.6, sync.y)], color=DUR,
            begin_arrow=k.ARROW_FILLED)
    p.arrow([(17.4, gitc.y + gitc.h), (17.4, sync.y)], color=DUR)
    pill(p, 17.4, (gitc.y + gitc.h + sync.y) / 2.0, 5, DUR)

    # 6 syncer -> S3, 7 restore back
    p.arrow([(17.9, sync.y + 1.4), (18.95, sync.y + 1.4)], color=DUR)
    pill(p, 18.42, sync.y + 1.0, 6, DUR)
    p.arrow([(18.95, sync.y + 2.3), (17.9, sync.y + 2.3)], color=DUR,
            dashed=True)
    pill(p, 18.42, sync.y + 2.7, 7, DUR)

    # 8 operator -> the pod
    p.arrow([(9.8, oper.y + 1.3), (10.05, oper.y + 1.3)], color=CTL)
    p.arrow([(10.05, oper.y + 2.1), (9.8, oper.y + 2.1)], color=CTL,
            dashed=True)
    pill(p, 9.92, oper.y + 0.85, 8, CTL)

    # 9 the bypass lane
    p.arrow([(2.95, zone_y), (2.95, lane_y), (24.25, lane_y), (24.25, zone_y)],
            color=BYPASS, dashed=True)
    pill(p, 8.2, lane_y, 9, BYPASS)

    # ---- legend ------------------------------------------------------
    legend_y = max(fz.y + fz.h, sz.y + sz.h, cz.y + cz.h) + 0.25
    # one flow per line: runs of spaces collapse when a line wraps, so a
    # single wrapped paragraph ran the nine numbers together
    p.fitbox(0.45, legend_y, 18.05, "The nine flows",
             "1   clone / fetch / push over HTTPS; the Basic password is the pod's own projected SA token\n"
             "2   TokenReview at the apiserver, cached ≤ 60 s — the door authenticates BEFORE it wakes anything\n"
             "3   routed to the headless Service: the runner → git http-backend → upload-pack | receive-pack\n"
             "4   the REST file API — same auth, same consumers list, same wake, on the syncer's own port\n"
             "5   proc-receive hands the syncer a request over a Unix socket and WAITS for the bucket\n"
             "6   the batch: every pack up first, then ONE snapshot CAS, then the local refs, then the report\n"
             "7   cold start, or a wake from replicas 0: restore the emptyDir from the snapshot alone\n"
             "8   the operator applies four objects and polls /status — it reads no object in the bucket\n"
             "9   bundle URI / LFS: presigned, client-to-store — the pod sees a few hundred bytes of JSON",
             fill=NOTE_F, line=NOTE_L, title_size=9.5, body_size=7.6)
    p.fitbox(18.95, legend_y, 10.6,
             "Where the planes meet, one-directional at every joint",
             "Control decides which bytes flow at all and makes the pod they "
             "will reach; control's lease produces the fence the durable path "
             "relies on, and its grace period decides whether a roll lands as "
             "a clean release or a SIGKILL mid-batch; the durable path writes "
             "/status and control believes it; and the data plane hands the "
             "durable path exactly one thing, through proc-receive, a request.",
             fill=NOTE_F, line=NOTE_L, title_size=9.5)
    return p.trim()


# =====================================================================
# 2 · the push as one transaction
# =====================================================================
def page2(doc):
    p = doc.page("2 The push transaction", PAGE_W, PAGE_H)
    top = header(
        p, 2, "A push is one transaction, and the order of its steps is the argument",
        "Under proc-receive, git's receive-pack serialises nothing and checks nothing for the commands it hands off — no old-oid "
        "check, no fast-forward test — and two concurrent pushes to one ref run their hooks fully overlapped. So the hook decides "
        "nothing. The syncer does: one process per repository, holding the writer lock for every path to S3.")

    lanes = [
        ("client", 0.45, 3.5, CLIENT_F, CLIENT_L),
        ("git receive-pack  ·  container: git", 4.15, 5.6, GIT_F, GIT_L),
        ("the hooks  ·  pre-receive, proc-receive", 9.95, 4.2, GIT_F, GIT_L),
        ("the syncer  ·  under the writer lock", 14.35, 7.4, SYNC_F, SYNC_L),
        ("S3", 21.95, 7.6, S3_F, S3_L),
    ]
    zones = []
    for name, x, w, f, l in lanes:
        z = p.box(x, top, w, 1.0, "", "", fill="#FCFDFE", line=ZONE_L,
                  dashed=True, rounding=0.1, line_weight=0.008)
        p.box(x, top, w, 0.42, name, "", fill=f, line=l, title_size=9.5,
              rounding=0.1, halign=1, valign=1)
        zones.append(z)
    lane_top = top + 0.62

    # The syncer column is built FIRST, because it is the clock: the other
    # lanes anchor their "waiting" boxes to the steps they are waiting on,
    # rather than to a hand-guessed offset.
    sy = p.stack(14.5, lane_top, 7.1, gap=0.16)
    steps = [
        ("collect",
         "Every push that arrived together becomes ONE batch. Four agents "
         "proposing refs/for/main at once land in one.", PAPER, MUTE),
        ("judge — against the batch's own RUNNING view",
         "Staleness against BOTH the local ref AND the last-synced "
         "snapshot: the local ref alone would let a syncer that lost a CAS "
         "accept a push against a ref the bucket has already moved. And "
         "against the running view, not the view at collection time, "
         "because two pushes to one ref routinely arrive inside a single "
         "window — checking both against the same base is exactly the "
         "defect falsifier 2 exists to catch, where both are told ok and "
         "one is lost. Then the policy again, and the refs/for/* merges, "
         "packing the objects they create.", PAPER, MUTE),
        ("renew the lease — once, for the batch",
         "Gated on the byte counter: renew only if the operation moved.",
         PAPER, MUTE),
        ("upload every pack the bucket does not have",
         "With its siblings. Content-named, so the PUT is unconditional and "
         "a retry is byte-identical — and its POINT is to refresh the "
         "object's age, which the sweep reads, so a retried upload must "
         "never be skipped as “already there”. Multipart above 64 MiB, "
         "CRC-64/NVME accumulated per part beside its own PUT so a 40 GiB "
         "pack is read once.  Page 3.", PAPER, MUTE),
        ("ONE snapshot CAS",
         "If-Match on the etag this syncer last saw, carrying every "
         "accepted ref and the full pack list in one object. A 412 here is "
         "a FENCE — another server holds this repository — not a retry: "
         "stop writing AND stop reading, exit 1.", "#FFF8E8", S3_L),
        ("apply the local refs as ONE update-ref transaction",
         "ONE entry per ref, however many commands moved it. update-ref "
         "refuses two updates to one ref, and it refuses them HERE — after "
         "the pack was built, uploaded and the CAS landed. That is how "
         "runcj ended with a snapshot naming a commit and no log entry for "
         "the batch that wrote it (F14, fixed 8381b557, verified on runck "
         "22/0).", WARN_F, WARN_L),
        ("…and ONLY THEN report ok",
         "A report interleaved with the updates would acknowledge a subset "
         "the snapshot already holds in full.", PAPER, MUTE),
    ]
    step_box = []
    for i, (t, b, f, l) in enumerate(steps):
        s = sy.box("%d · %s" % (i + 1, t), b, fill=f, line=l, title_size=9.2)
        step_box.append(s)
        pill(p, s.x - 0.16, s.y + 0.19, i + 1, DUR)
    sy.box("afterwards: the derived files, the sweep, the fold",
           "info/refs, objects/info/packs and HEAD; the sweep if a repack "
           "happened; the tiers' fold when the floor is crossed.",
           fill=NOTE_F, line=NOTE_L, title_size=9.2)
    sy.box("the lock all of this runs under",
           "One syncer per repository, one writer lock — and the epoch cell "
           "in S3 says which syncer that is.  Page 5.",
           fill=SYNC_F, line=SYNC_L, title_size=9.2)

    # -- S3 lane, aligned to the steps that touch it
    p.fitbox(22.1, lane_top, 7.3, "the bucket, seen from the transaction",
             "Only steps 3, 4 and 5 touch it, and only step 5 can change "
             "what a restore sees.", fill=S3_F, line=S3_L, title_size=9.2)
    # The three writes, stacked in the lane and each ARROWED to the step
    # that makes it. Anchoring them to the step's own y put a tall label
    # on top of the next one, so the lane owns the vertical order and the
    # arrows carry the correspondence.
    step_boxes = step_box[2:5]          # steps 3, 4 and 5 — the only writers
    writes = [
        ("PUT …/git/epoch\n  If-Match: <the token we hold>", PAPER),
        ("PUT …/git/objects/pack/pack-<sha>.pack\n"
         "PUT …                  pack-<sha>.idx\n"
         "PUT …                  pack-<sha>.rev\n"
         "PUT …                  pack-<sha>.bitmap\n"
         "\nunconditional — the name IS the content", PAPER),
        ("PUT …/git/snapshot\n"
         "  If-Match: <etag last seen>\n"
         "  (or If-None-Match: * to create)\n"
         "\n→ 200   the push is durable\n"
         "→ 412   FENCED, and reads stop too", "#FFF8E8"),
    ]
    tail = p.stack(22.1, step_boxes[0].y, 7.3, gap=0.2)
    for sb, (txt, f) in zip(step_boxes, writes):
        w = tail.box(txt, "", fill=f, line=S3_L, title_size=8.2,
                     title_mono=True, title_bold=False, title_color=SUB)
        p.arrow([(21.6, sb.y + 0.22), (21.85, sb.y + 0.22),
                 (21.85, w.y + 0.22), (22.1, w.y + 0.22)], color=DUR)
    tail.box("PUT …/git/log/<seq>.json\nPUT …/git/undo/<seq>.json", "",
             fill=PAPER, line=S3_L, title_size=8.2, title_mono=True,
             title_bold=False, title_color=SUB)
    tail.box("What crashing anywhere costs",
             "Between 4 and 5 — an unnamed object, which the sweep removes. "
             "Never a name without an object.\n"
             "Between 5 and 6 — the bucket holds the batch and the pod does "
             "not; the restore installs the refs from the snapshot and they "
             "agree again.\n"
             "Between 6 and 7 — told-failed but durable: the one residual, "
             "carried in the model as a required-fail probe.\n"
             "A rollout mid-push — SIGKILL at the 30 s grace. The client is "
             "told failed, the bucket is unchanged, the retry converges and "
             "the orphaned multipart upload is swept once the successor "
             "serves (X6, measured on run 7). Whether a roll should wait "
             "for the batch is the decision still to take.\n"
             "\n"
             "Drilled on real S3: a 40 GiB push acknowledged in 1113 s over "
             "641 parts with the CRC accepted; 40 of 40 pushes told ok "
             "across two takeover arms were in the bucket; and four kills "
             "placed INSIDE a multipart upload, by watching "
             "list-multipart-uploads, held told-failed ⇒ unchanged and "
             "told-ok ⇒ durable.",
             fill=NOTE_F, line=NOTE_L, title_size=9.2)

    # -- the three lanes before the syncer, each anchored to its step
    wait_y = step_box[2].y                     # the hooks go quiet here
    report_y = step_box[6].y + step_box[6].h + 0.45   # after step 7 reports

    cl = p.stack(0.6, lane_top, 3.2, gap=0.22)
    cl.box("git push",
           "The commands, then the pack, chunked. The server answers "
           "nothing until the pack is in — and this one HTTP request can "
           "last twenty minutes.", fill=CLIENT_F, line=CLIENT_L,
           title_size=9.2)
    p.fitbox(0.6, max(wait_y, cl.y), 3.2, "…and then silence, filled",
             "An empty sideband packet every 5 s. Through the runner a "
             "232 s hook wait carried 48 packets with a longest gap of "
             "5.8 s; through the nginx control, 49 packets arrived in ONE "
             "burst at 237.9 s. The door's own clock is touched by every "
             "chunk in either direction, so the keepalives must REACH it "
             "as they are written.",
             fill=NOTE_F, line=NOTE_L, title_size=9.2)
    creport = p.fitbox(0.6, report_y, 3.2, "the per-ref report",
                       "ok / ng <ref> <reason>, emitted as it is written. "
                       "The client learns the outcome only after the bucket "
                       "already holds it.",
                       fill=CLIENT_F, line=CLIENT_L, title_size=9.2)

    rp = p.stack(4.3, lane_top, 5.3, gap=0.22)
    rp.box("index-pack --fix-thin, into a quarantine objdir",
           "The thin pack on the wire is completed with the bases it "
           "referenced, written as pack-<sha>.pack and indexed. "
           "receive.unpackLimit = 1: a push is always a pack, never loose "
           "objects, so the unit the syncer uploads is the unit git wrote. "
           "receive.fsckObjects refuses a malformed object here, at the "
           "door, rather than at a restore.",
           fill=GIT_F, line=GIT_L, title_size=9.2)
    rp.box("migrate the quarantine",
           "git moves .keep, .pack, .rev, .idx — IN THAT ORDER. A "
           "neighbouring push's pack is therefore on disk before its index "
           "for a moment (X1, found by reading tmp-objdir.c).  Page 3.",
           fill=GIT_F, line=GIT_L, title_size=9.2)
    p.fitbox(4.3, max(wait_y, rp.y), 5.3, "keepalive while the hooks are quiet",
             "receive.keepAlive = 5, pinned in the syncer's config so the "
             "guarantee does not rest on git's default. This is the whole "
             "of git's involvement while the bucket is written.",
             fill=GIT_F, line=GIT_L, title_size=9.2)
    p.fitbox(4.3, report_y, 5.3, "…and only now is git's own work done",
             "receive.procReceiveRefs = refs/ took every ref out of git's "
             "own atomic transaction, so --atomic is honoured HERE or "
             "nowhere (fixed 17c40d3b; its first test was vacuous).",
             fill=GIT_F, line=GIT_L, title_size=9.2)

    hk = p.stack(10.1, lane_top, 3.9, gap=0.22)
    hk.box("pre-receive — the policy, at the edge",
           "Sees every command, including the refs/for/* proposals, applies "
           "the rendered policy against REMOTE_USER, and refuses the WHOLE "
           "push if any command is refused — which is git's semantics for "
           "this hook and not a choice. Its refusal is the one the pusher "
           "reads, so the message names the rule. It is NOT the guarantee: "
           "the syncer applies the same document again, because a "
           "repository whose hooks were misconfigured would otherwise "
           "accept a push to main from anyone who could reach the door.",
           fill=GIT_F, line=GIT_L, title_size=9.2)
    procr = hk.box("proc-receive — a relay, and nothing else",
                   "git spawns it once per push. It negotiates the "
                   "proc-receive version, reads the command list and the "
                   "push options, hands them to the syncer over the Unix "
                   "socket, WAITS, and writes back the per-ref report it is "
                   "given.\n"
                   "\n"
                   "pkt-line is the ONLY wire format forge implements, and "
                   "it implements it because proc-receive speaks it on the "
                   "hook's stdin and stdout and nothing else will.  Page 4.",
                   fill="#FFF1E4", line=GIT_L, title_size=9.2)
    p.fitbox(10.1, report_y, 3.9, "…and the report goes back out",
             "Per-ref, in the order the syncer emitted it. proc-receive "
             "decides nothing about it.",
             fill=GIT_F, line=GIT_L, title_size=9.2)

    bottom = max(cl.bottom, rp.bottom, hk.bottom, sy.bottom, tail.bottom,
                 creport.y + creport.h)
    for z in zones:
        z.resize_bottom(bottom + 0.25)

    # -- the hops between lanes, and ONE return path rather than stubs
    p.arrow([(3.8, lane_top + 0.25), (4.3, lane_top + 0.25)], color=FLOW)
    p.arrow([(9.6, lane_top + 0.25), (10.1, lane_top + 0.25)], color=FLOW)
    p.arrow([(14.0, procr.y + 0.3), (14.5, procr.y + 0.3)], color=DUR)
    p.arrow([(14.5, step_box[6].y + 0.2), (14.15, step_box[6].y + 0.2),
             (14.15, report_y - 0.28), (2.2, report_y - 0.28),
             (2.2, report_y)], color=DUR)
    # a fill, because this caption sits on the return path it describes
    p.text(4.5, report_y - 0.63, 8.0,
           "the report travels back out the way it came in — syncer → "
           "proc-receive → receive-pack → the client — and only after the "
           "snapshot CAS and the ref transaction have both landed",
           size=7.4, color=DUR, fill=PAPER)

    foot = bottom + 0.5
    p.fitbox(0.45, foot, 13.55, "The finding that shaped the whole path",
             "Two concurrent pushes to one ref run their proc-receive hooks "
             "fully overlapped, and git checks nothing for them. So the hook "
             "cannot serialise and cannot decide; everything that must be "
             "decided once, in an order, is decided in the syncer under one "
             "lock, in one process.",
             fill=NOTE_F, line=NOTE_L, title_size=9.5)
    p.fitbox(14.35, foot, 15.2, "refs/for/<target> — the anti-livelock door",
             "A proposal, never a ref: refs/for/* is never stored in the "
             "snapshot. The server merges at a FRESH base with no old-oid "
             "check, so four agents pushing at once do not livelock — on "
             "runck arm D landed 20/20 on the first attempt with 0 refusals, "
             "against arm A's 30. On conflict: a deterministic refusal "
             "naming the paths, and nothing written. -o strategy=ours|theirs "
             "is the only server-side escape.",
             fill=NOTE_F, line=NOTE_L, title_size=9.5)
    return p.trim()


# =====================================================================
# 3 · a commit becomes a pack becomes an S3 object
# =====================================================================
def page3(doc):
    p = doc.page("3 Commit to pack to S3 object", PAGE_W, PAGE_H)
    top = header(
        p, 3, "How the files in a commit become a pack, and how the pack becomes an S3 object",
        "Forge writes none of this format. git's own pack-objects builds it on the client, git's own index-pack completes and "
        "indexes it on the server, and what the syncer moves to S3 is the byte-for-byte file git wrote. The name of that file is "
        "the checksum of its own contents, which is the property the whole storage design leans on.")

    # -- A: the client's object database
    az, ay = zone(p, 0.45, top, 6.6, "on the client — git's object database")
    a = p.stack(0.75, ay, 6.0, gap=0.2)
    a.box("A · the files in a commit are three kinds of object",
          "Each file's CONTENT becomes a blob. Each directory becomes a "
          "tree, listing the mode, name and oid of every entry. The commit "
          "names one tree, its parents, an author, a committer and a "
          "message.\n"
          "An object is stored as its type and length, a NUL, then the "
          "bytes — and its NAME is the SHA-1 (SHA-256 if the repository is) "
          "of that whole header-plus-body. Identical content anywhere in "
          "history is therefore the same object, stored once.",
          fill=CLIENT_F, line=CLIENT_L)
    a.box("commit c3f9…\n"
          "  tree     8a11…\n"
          "    100644 README.md  → blob e69d…\n"
          "    040000 src        → tree 5b2c…\n"
          "                           100644 main.rs → blob 4fa0…\n"
          "  parent   9d02…", "",
          fill=PAPER, line=CLIENT_L, title_size=7.8, title_mono=True,
          title_bold=False, title_color=SUB)
    a.box("B · loose on disk, packed on the wire",
          "A new object is written LOOSE: zlib-deflated, one file, at "
          ".git/objects/e6/9de29b…. That is a fine store and a poor "
          "transport — no delta between versions of the same file, and a "
          "round of syscalls per object. So a push does not send loose "
          "objects; git runs pack-objects.",
          fill=CLIENT_F, line=CLIENT_L)
    a.box("C · pack-objects, and the THIN pack",
          "The two sides negotiate — “I have these tips, you have those.” "
          "pack-objects then emits only the objects the server lacks, "
          "delta-compressed, and it is allowed to delta against a base the "
          "server ALREADY HAS but which is not in the pack. That is a thin "
          "pack: correct on the wire, unusable as a file.",
          fill=CLIENT_F, line=CLIENT_L)
    a.box("What forge does NOT do to a pack",
          "It does not parse it. It does not re-chunk it, deduplicate "
          "inside it, transcode it, or store objects individually, and it "
          "keeps no database of oids.\n"
          "\n"
          "The consequences are the design:\n"
          "  · a pack in the bucket is a pack git can open, so a restore is "
          "a download and not a rebuild;\n"
          "  · the key is a checksum of the body, so two writers cannot "
          "disagree and a retry is free;\n"
          "  · the only thing needing a conditional write is the ONE object "
          "that says which packs and refs are current;\n"
          "  · and forge's correctness argument is about ORDERING and "
          "COORDINATION, not about a format it would otherwise have to get "
          "right.\n"
          "\n"
          "The cost is that the unit of storage is git's unit. That is what "
          "the tiers, the fold cap and the pack-pinning question on runcl "
          "are all about.",
          fill=NOTE_F, line=NOTE_L)
    az.resize_bottom(a.bottom + 0.3)

    # -- B: the pack format
    bz, by = zone(p, 7.4, top, 11.3,
                  "the pack format — what is actually in the file")
    b = p.stack(7.7, by, 10.7, gap=0.16)
    b.box("D · the .pack file, end to end",
          "Every length is big-endian; the file is a byte stream with no "
          "padding and no index inside it.",
          fill=GIT_F, line=GIT_L)

    strip_y = b.y
    segs = [(1.25, "'P' 'A' 'C' 'K'", "4 B magic", "#FFE7CE"),
            (1.0, "version", "4 B — 2 or 3", "#FFF0E0"),
            (1.15, "object count", "4 B", "#FFF0E0"),
            (1.5, "object 1", "entry", PAPER),
            (1.5, "object 2", "entry", PAPER),
            (0.75, "…", "", PAPER),
            (1.5, "object N", "entry", PAPER),
            (2.05, "trailer: the SHA of everything above", "20 B — 32 B for SHA-256", "#FFE7CE")]
    x = 7.75
    for w, lab, sub, f in segs:
        p.box(x, strip_y, w, 0.86, lab, sub, fill=f, line=GIT_L,
              title_size=7.8, body_size=6.8, rounding=0.03, halign=1,
              valign=1)
        x += w
    b.skip(0.86 + 0.1)
    b.text("THAT TRAILER IS THE NAME: git calls the file pack-<that sha>.pack, so the "
           "file name is a checksum of the file — which is why the S3 PUT can be "
           "unconditional, why a retry is byte-identical, and why two servers writing "
           "the same pack cannot disagree.",
           size=7.8, color=GIT_L, bold=True)

    p.arrow([(11.9, strip_y + 0.86), (11.9, b.y)], color=GIT_L)
    b.box("one object entry, expanded", "", fill=GIT_F, line=GIT_L,
          title_size=9, halign=1, valign=1, gap=0.1)
    ex_y = b.y
    segs2 = [(2.6, "type + size, as a varint",
              "3 bits of type, then 4 + 7·n bits of the\ninflated size, low bits first", "#FFF0E0"),
             (3.3, "and only for a delta:",
              "OBJ_OFS_DELTA → a varint saying how far BACK\nin this same pack the base sits\nOBJ_REF_DELTA → the base's 20-byte oid", "#F3F4F6"),
             (4.8, "zlib-deflated data",
              "a whole object, or a delta script: a run of COPY\n(offset, length — take these bytes from the base)\nand INSERT (length, literal bytes) instructions", PAPER)]
    x, hmax = 7.75, 0.0
    for w, lab, sub, f in segs2:
        s = p.fitbox(x, ex_y, w, lab, sub, fill=f, line=GIT_L,
                     title_size=8.2, body_size=6.9, rounding=0.03)
        hmax = max(hmax, s.h)
        x += w
    b.skip(hmax + 0.16)

    ty = b.y
    s1 = p.fitbox(7.75, ty, 5.2, "the seven type codes",
                  "1 OBJ_COMMIT   2 OBJ_TREE   3 OBJ_BLOB   4 OBJ_TAG\n"
                  "6 OBJ_OFS_DELTA   7 OBJ_REF_DELTA   (5 is unused)\n"
                  "A delta chain bottoms out at a full object, and git bounds "
                  "the chain's depth so a read is not unbounded work.",
                  fill=PAPER, line=GIT_L, title_size=9)
    s2 = p.fitbox(13.15, ty, 5.25, "core.bigFileThreshold = 1m",
                  "Objects above this skip the delta search in EVERY "
                  "pack-objects, upload-pack's included. A 256 MiB blob tier "
                  "cost 17 CPU-seconds per clone at git's 512m default, and "
                  "0.6 s at 1m.",
                  fill=PAPER, line=GIT_L, title_size=9)
    # a side-by-side pair advances the column by the TALLER of the two,
    # and both are levelled so the row reads as one
    s1.h = s2.h = max(s1.h, s2.h)
    b.skip(s1.h + 0.16)

    b.box("E · on the server: index-pack --fix-thin, in a quarantine",
          "receive-pack reads the thin pack into a temporary object "
          "directory, resolves every delta, APPENDS the bases the client "
          "left out so the file stands alone, and writes the .idx beside "
          "it. Only then does it migrate the quarantine into objects/pack/ "
          "— moving .keep, .pack, .rev, .idx, in that order.\n"
          "receive.unpackLimit = 1 forces this path for every push: a push "
          "is always a pack, never loose objects, so the unit the syncer "
          "uploads is exactly the unit git wrote.",
          fill=GIT_F, line=GIT_L)
    b.box("F · the .idx — the file that makes a pack readable",
          "  \\377 t O c   4 B magic        version 2       4 B\n"
          "  fanout[256]  4 B each    fanout[b] = how many oids start ≤ b\n"
          "  oid[N]       sorted, so a lookup is a bisect in one bucket\n"
          "  crc32[N]     4 B each    over each object's PACKED bytes\n"
          "  offset[N]    4 B each    MSB set ⇒ an index into the 8 B table\n"
          "  offset64[]   8 B each    for packs over 2 GiB\n"
          "  pack sha     20 B        ties this index to THAT pack\n"
          "  idx sha      20 B",
          fill=PAPER, line=GIT_L, title_size=9.5, mono=True)
    b.box("THE INDEX IS A GATE, NOT A DETAIL — X1",
          "The pack listing that feeds the upload step REQUIRES the pack's "
          ".idx to be on disk. Because git migrates the quarantine as "
          ".keep, .pack, .rev, .idx, a NEIGHBOURING push's pack is on disk "
          "before its index for a moment — and a batch that listed it then "
          "would have named, in a snapshot, a pack whose index never "
          "followed. A restore of that snapshot refuses: exit 78, "
          "unrecoverable.\n"
          "Found by reading git's tmp-objdir.c, not by a test. Fixed by "
          "requiring the index; the mutation that removes the gate is one "
          "of the five the TLA+ model must lose.",
          fill=WARN_F, line=WARN_L)
    bz.resize_bottom(b.bottom + 0.3)

    # -- C: the S3 write
    cz, cy = zone(p, 19.05, top, 10.5,
                  "the syncer's write — the pack becomes an object")
    c = p.stack(19.35, cy, 9.9, gap=0.2)
    c.box("G · the key is the name git already chose",
          "<prefix>/git/objects/pack/pack-<sha>.pack\n"
          "<prefix>/git/objects/pack/pack-<sha>.idx\n"
          "<prefix>/git/objects/pack/pack-<sha>.rev\n"
          "<prefix>/git/objects/pack/pack-<sha>.bitmap\n"
          "\n"
          "No transcoding, no re-chunking, no container: the object's body "
          "IS the file's bytes.",
          fill=S3_F, line=S3_L, mono=True)
    c.box("H · and therefore the PUT is UNCONDITIONAL",
          "The one place in flint where that variant is correct, and for "
          "the reason its doc comment gives. Content naming means a "
          "re-upload is byte-identical, so there is nothing to lose to a "
          "race — and its POINT is to refresh the object's age, which the "
          "sweep reads. A retried upload must therefore never be skipped "
          "as “already there”.",
          fill=PAPER, line=S3_L)
    c.box("I · ≤ 64 MiB is one PUT; above it, a multipart compose",
          "put_whole holds the whole object in RAM, and a folded "
          "repository is ONE pack — the largest object forge ever writes — "
          "so 64 MiB is where the grid takes over.\n"
          "The part grid is contiguous from zero, covers the size exactly, "
          "uses at most max_parts, and makes every part but the last at "
          "least min_part: the rule S3 enforces, mirrored by the memory "
          "store, because a grid that is wrong only above 640 GiB is still "
          "wrong and EntityTooSmall from a real bucket is a poor place to "
          "discover it.\n"
          "The CRC-64/NVME that S3 judges at CompleteMultipartUpload is "
          "accumulated PER PART beside its own PUT, so a 40 GiB pack is "
          "read once. The ~70 s pre-pass it replaced ticked no progress and "
          "let the lease go quiet inside a live push.",
          fill=PAPER, line=S3_L)
    c.box("J · nothing is durable until the snapshot names it",
          "{\"version\":1, \"seq\":412, \"epoch\":7,\n"
          " \"refs\":{\"refs/heads/main\":\"c3f9…\"},\n"
          " \"packs\":[\"pack-1a2b….pack\",\"pack-9f0c….pack\"],\n"
          " \"bundles\":[…], \"exported_commit\":\"c3f9…\",\n"
          " \"writer\":\"forge-7f9-x2q\", \"unix\":1757…}\n"
          "\n"
          "PUT <prefix>/git/snapshot   If-Match: <etag last seen>",
          fill="#FFF8E8", line=S3_L, mono=True)
    c.box("K · later, the fold: many packs become fewer",
          "A pack per push makes an unbounded list, so the tiers "
          "consolidate them — pack-objects writes a new pack, "
          "content-named like any other, and the snapshot's list is "
          "replaced. Supersede is OBJECT COVERAGE, not reachability, which "
          "is why unreachable objects from a refused merge can pin a whole "
          "pack. Measured on content that deltifies: 81% of the "
          "snapshot-named bytes are residue — n=3, 81/81/81, and 82 on a "
          "fourth, differently-timed run. An earlier 58% came from one "
          "~80 KB repository and read low.",
          fill=PAPER, line=S3_L)
    c.box("L · and the way back",
          "A restore reads the snapshot, fetches exactly the packs it names "
          "as 8 MiB ranged GETs at a bounded fan-out, drops them in "
          "objects/pack/, installs the refs, runs fsck "
          "--connectivity-only and serves. 40 GiB in 139 s from the delete, "
          "fsck clean, 25 MiB of anon RSS — because a pack from the bucket "
          "is a pack git can open, with no conversion at all.",
          fill=S3_F, line=S3_L)
    cz.resize_bottom(c.bottom + 0.3)

    mid = top + 3.0
    p.arrow([(7.05, mid), (7.4, mid)], color=FLOW)
    p.arrow([(18.7, mid), (19.05, mid)], color=DUR)

    foot = max(az.y + az.h, bz.y + bz.h, cz.y + cz.h) + 0.3
    p.fitbox(0.45, foot, 29.1,
             "The one place forge touches a wire format at all",
             "pkt-line — four hex digits of length counting themselves, then the payload, and 0000 is a flush. That is the whole "
             "format, and forge implements it because proc-receive speaks it on the hook's stdin and stdout and nothing else will. "
             "Not the capability advertisement, not the ref advertisement, not the negotiation, not the sideband multiplexing, not "
             "the pack stream itself: stock git does every one of those, on both sides.",
             fill=NOTE_F, line=NOTE_L, body_size=8.2)
    return p.trim()


# =====================================================================
# 4 · who parses the protocol
# =====================================================================
def page4(doc):
    p = doc.page("4 Who parses the protocol", PAGE_W, PAGE_H)
    top = header(
        p, 4, "Forge does not interpret the git protocol; it runs the git server and stands behind it",
        "The same shape — S3 as the only durable state, the disk a cache, one CAS'd pointer as the transaction boundary — was "
        "reached by several teams inside the same eighteen months. Where forge and its nearest neighbour walgit differ is in who "
        "owns the protocol: walgit reimplements receive-pack in Rust; forge lets git be the server and hooks in at one seam.")

    fz, fy = zone(p, 0.45, top, 14.3, "flint-forge — stock git, one seam", "#FBFDFB")
    wz, wy = zone(p, 15.25, top, 14.3, "walgit — the server reimplemented", "#FDFBFB")

    rows = [
        ("The client talks to git http-backend",
         "A runner execs `git http-backend` once per request and streams "
         "the body in and the output back out. The runner implements no "
         "git: it is a CGI host. Swapping it for nginx + fcgiwrap and back "
         "again changed the chart, the CRD, the door's URL formula and "
         "every rig by nothing — which is the test of the claim.",
         "The client talks to walgit",
         "The server terminates the smart HTTP protocol itself and speaks "
         "receive-pack's half of it in Rust: the ref advertisement, the "
         "command list, the sideband, the report. upload-pack, repack and "
         "bundles are still stock git."),
        ("Every git decision is made by git",
         "The negotiation, the delta search, index-pack --fix-thin, the "
         "connectivity check, the ref advertisement, the sideband "
         "multiplexing, --filter, protocol v2, the reflog. A git bug is a "
         "git bug and a git release is an upgrade. When gitqual found "
         "uploadpack.allowFilter unset, the fix was one line of config, "
         "not a feature.",
         "Every git decision on the write path is walgit's",
         "Whatever receive-pack does — old-oid checks, the hook contract, "
         "the report's exact grammar, the keepalive discipline, quarantine "
         "semantics — has to be re-derived, and then kept in step with git "
         "as it changes. The payoff is that the writer can be exactly what "
         "the storage wants, with no hook waiting on a socket."),
        ("One seam, and it is a documented one: proc-receive",
         "receive.procReceiveRefs = refs/ routes every ref through the "
         "hook. proc-receive negotiates its version, reads the command "
         "list, hands it to the syncer over a Unix socket, waits, and "
         "writes back the report. The acknowledgement the client sees IS "
         "the syncer's report, so “told ok” means the CAS landed — not "
         "that a disk was written.\n"
         "pkt-line is the ONLY wire format forge implements, and only "
         "because the hook's stdin and stdout speak it.",
         "No seam — the writer IS the server",
         "The acknowledgement is the server's own, which is simpler to "
         "reason about, and it is why walgit needs no keepalive story on "
         "the write path: nothing is waiting on a hook. Continuity states "
         "the same durability guarantee and does not say how it is "
         "carried."),
        ("What that costs forge, honestly",
         "A push's acknowledgement is a hook blocked on a Unix socket for "
         "as long as the bucket takes, so every party on the path needs an "
         "opinion about silence: receive.keepAlive = 5 s, a runner that "
         "does not buffer, a door whose only bound is 300 s of no bytes at "
         "all. Three of the campaign's four front-layer defects were in "
         "that chain — and all three were in nginx and fcgiwrap rather "
         "than in git or in the syncer.",
         "What that costs walgit, honestly",
         "The protocol surface is the thing that must not drift, and it is "
         "large. Forge's own measurement is the fair one to quote: after "
         "the tiers and the window, forge's worst push fell from 816 s to "
         "0.83 s and its rate rose to 14.1 pushes/s against walgit's 10.3 "
         "— while walgit still uploads about a third of the bytes."),
    ]
    y = fy
    for ft, fb, wt, wb in rows:
        _, _, fn = k.measure(ft, fb, 13.6, 10.0, 7.6)
        _, _, wn = k.measure(wt, wb, 13.6, 10.0, 7.6)
        h = max(fn, wn) + 0.12
        p.box(0.8, y, 13.6, h, ft, fb, fill=OPER_F, line=OPER_L)
        p.box(15.6, y, 13.6, h, wt, wb, fill="#FBECEC", line="#B05A5A")
        y += h + 0.2
    fz.resize_bottom(y + 0.1)
    wz.resize_bottom(y + 0.1)

    y += 0.4
    s = p.fitbox(0.45, y, 29.1, "The seam, drawn as one sentence",
                 "client git  →  HTTPS  →  the door (authenticate, authorise, route, wake, bound)  →  the runner  →  "
                 "git http-backend  →  git receive-pack  →  [ pre-receive: the policy ]  →  "
                 "[ proc-receive: pkt-line in, a request over a Unix socket, WAIT ]  →  "
                 "the syncer: judge, renew, upload the packs, ONE snapshot CAS, update-ref  →  "
                 "the report back out through proc-receive  →  git writes the per-ref ok  →  the client.\n"
                 "Everything before the second bracket is stock git. Everything after it is forge.",
                 fill=NOTE_F, line=NOTE_L, body_size=8.4)
    y += s.h + 0.35

    _, _, h1 = k.measure("What is convergent", "x", 14.3, 10.0, 7.6)
    left = p.fitbox(
        0.45, y, 14.3, "What is convergent",
        "“S3 as the only durable state, the on-disk repository a cache, one "
        "CAS'd pointer as the transaction boundary, stock git doing every "
        "git operation” is what Cursor published as Continuity on "
        "2026-08-18, and what walgit — an open-source Rust server on that "
        "design — ships with bundle-uri, LFS and per-repository push "
        "policy. Behind both is Palantir's Stemma on JGit's DfsRepository, "
        "and Google's Bigtable-backed JGit before it; AWS CodeCommit stored "
        "packs in S3 with metadata in DynamoDB from 2015. Forge's design is "
        "dated 2026-09-04 and cites neither, because its author had not "
        "seen them. The SHAPE is not forge's claim.",
        fill=NOTE_F, line=NOTE_L)
    right = p.fitbox(
        15.25, y, 14.3, "What is forge's own — in the joints, not the shape",
        "The writer is stock receive-pack with the syncer as its only "
        "client and the acknowledgement carried by proc-receive waiting on "
        "it. Coordination is a single-writer lease with a progress-gated "
        "renewer and a rotation on every claim, chosen over “any host may "
        "CAS” because at fleet rates every self-inflicted 412 would be a "
        "full restore. The control plane is Kubernetes-native per "
        "repository: the pod's own SA token as the credential, one pod "
        "parked at replicas 0, woken by a door that authenticates before it "
        "wakes. And the legible export republishes the repository as a "
        "workspace with no git in it, which nothing in the table attempts.",
        fill=NOTE_F, line=NOTE_L)
    h = max(left.h, right.h)
    left.h = right.h = h
    y += h + 0.35

    p.fitbox(0.45, y, 29.1, "The verdict, stated so that it can be wrong",
             "Innovative in the integration's joints — how the acknowledgement is carried, how one writer is kept and deposed, how "
             "the server is placed and woken, how the repository is re-published for readers that are not git — and convergent in "
             "its shape, which is now the shape of the field. A fifth thing is method rather than architecture: a TLA+ model whose "
             "mutations are the campaign's findings, eleven falsifiers, three campaigns on real S3 and a control arm that must fail, "
             "all in the repository, where Continuity's verification is production and walgit's is a seeded fault-injection simulation.",
             fill=OPER_F, line=OPER_L, body_size=8.2)
    return p.trim()


# =====================================================================
# 5 · the lease and the bucket over time
# =====================================================================
def page5(doc):
    p = doc.page("5 The lease and the bucket over time", PAGE_W, PAGE_H)
    top = header(
        p, 5, "One writer per repository, and what the bucket looks like while it changes",
        "One writer is a mechanism, not a convention: an epoch cell in S3 that every participant moves only by compare-and-swap. A "
        "renewing holder is undeposable by construction, so the whole design is about when it stops renewing — and about making a "
        "straggler's writes stale before its successor serves a byte.")

    lz, ly = zone(p, 0.45, top, 14.3, "a holder's life")
    rz, ry = zone(p, 15.25, top, 14.3, "the challenger, and the fence")

    steps = [
        ("claim",
         "CAS the epoch cell. A 412 here means someone else got there "
         "first: wait, and poll again.", SYNC_F, SYNC_L),
        ("ROTATE the snapshot",
         "The same content, a NEW etag, written BEFORE the successor "
         "restores — so any If-Match a straggler still holds is stale "
         "before the successor serves a byte. Every claim but a released "
         "cell's rotates, because only a releaser has proven it fenced "
         "itself before writing the mark (X12); and a repository nobody "
         "had published rotates too, by creating the empty snapshot "
         "(X11).", SYNC_F, SYNC_L),
        ("sweep",
         "Orphaned multipart uploads, and unnamed packs from the "
         "predecessor's crash.", SYNC_F, SYNC_L),
        ("restore",
         "The snapshot's packs into the emptyDir, the refs installed "
         "exactly, fsck --connectivity-only.", SYNC_F, SYNC_L),
        ("serve, and renew every 10 s",
         "The renewer is its OWN task from the moment the claim lands, so "
         "the restore, every batch and every export beat through it. And "
         "it is gated: while a phase must progress, renew ONLY if the "
         "operation's byte counter advanced since the last renewal — "
         "because renewing for a wedged pod would trade a live pod losing "
         "its repository for a dead one keeping it forever, which is the "
         "case the quiet polls exist for.", SYNC_F, SYNC_L),
        ("a 412 on any CAS of ours = THE FENCE",
         "Stop writing AND stop reading, exit 1. A deposed server that "
         "kept answering upload-pack would serve stale refs forever. The "
         "kubelet restarts the pod: a fence exits 1, a refusal exits 78, "
         "and restart is not an operator decision.", WARN_F, WARN_L),
    ]
    ls = p.stack(1.15, ly, 13.25, gap=0.3)
    for i, (t, b, f, l) in enumerate(steps):
        s = ls.box("%d · %s" % (i + 1, t), b, fill=f, line=l)
        if i:
            p.arrow([(0.85, s.y - 0.3), (0.85, s.y + 0.2), (1.15, s.y + 0.2)],
                    color=DUR)
    lz.resize_bottom(ls.bottom + 0.3)

    rs = p.stack(15.6, ry, 13.6, gap=0.22)
    rs.box("the challenger",
           "Polls the epoch cell, and judges the holder dead only after SIX "
           "CONSECUTIVE polls saw the SAME token — never on one quiet poll, "
           "and never on a clock. A clean release on SIGTERM writes the mark "
           "and lets the successor claim at once, which is the difference "
           "between a rollout and a crash.",
           fill=OPER_F, line=OPER_L)
    rs.box("the sensor can lie, and it did",
           "The renewer's gate reads a byte counter. Run 3's checksum "
           "pre-pass did 70 seconds of real work and ticked nothing, so the "
           "counter said “no progress” while the process was busy. The TLA+ "
           "model keeps the SENSOR and the REAL MOVEMENT as separate "
           "variables for exactly this reason.\n"
           "The renewer's first shape renewed only inside a push, and only "
           "between an upload's parts: on runbw a 10 GiB push's token was "
           "silent for 125 s and a restore's for 141 s, inside a 60 s "
           "window. A challenger claimed the live, importing pod 62 s after "
           "arriving, and the two seized each other through three epochs — "
           "while every acknowledged push still reached the bucket.",
           fill=WARN_F, line=WARN_L)
    rs.box("what the model found that the code had wrong",
           "The first shape exempted two cases from the rotation, and the "
           "model's first two strict runs refuted both against the code the "
           "same day.\n"
           "X11 — a takeover of a repository nobody had published skipped "
           "the rotation, on the theory that the first CAS's If-None-Match "
           "would be the fence. But a straggler mid-batch on the old epoch "
           "could land its create after the successor served, and then the "
           "SUCCESSOR's own first CAS was what 412'd, fencing it with its "
           "predecessor's push.\n"
           "X12 — a successor that died between its takeover CAS and its "
           "rotation came back through self-recognition, which skipped the "
           "rotation because “our own previous process died with its "
           "writes” — while the straggler from the epoch before still held "
           "a valid If-Match.\n"
           "Both are now unit tests that fail against the old code.",
           fill=NOTE_F, line=NOTE_L)
    rs.box("what the drills measured",
           "On runbx a challenger sat beside a live 40 GiB restore for "
           "398 s and never claimed, the holder was never fenced, and 24 of "
           "24 pushes under that contention were durable.\n"
           "Two servers for one repository is not a state the Deployment "
           "creates on its own: it takes a roll against a wedged pod, a "
           "lost node whose pod is still counted, or a hand — and every "
           "such case ends with the straggler's next CAS 412ing.",
           fill=OPER_F, line=OPER_L)
    rz.resize_bottom(rs.bottom + 0.3)

    # -- the bucket over time
    bt = max(lz.y + lz.h, rz.y + rz.h) + 0.35
    bz, byy = zone(p, 0.45, bt, 29.1,
                   "the bucket over time — what accumulates, what is removed, and by whom")
    cols = [
        ("the packs", S3_F, S3_L,
         "Added by every batch, content-named, never meaningfully "
         "overwritten. Removed only by the sweep, and only when the "
         "snapshot no longer names them AND they are older than the grace. "
         "A re-upload REFRESHES the object's age, which is why the PUT must "
         "not be skipped as redundant."),
        ("the snapshot", "#FFF8E8", S3_L,
         "One object, rewritten by CAS on every batch and on every claim's "
         "rotation. seq is monotonic so that a rotation changes the bytes — "
         "and therefore the etag — even when the content would be "
         "identical. version is bumped only for a change an older syncer "
         "could MISREAD, and a reader meeting a higher version refuses "
         "rather than parsing what it can and concluding the repository is "
         "empty."),
        ("the fold, and the tiers", NOTE_F, NOTE_L,
         "A pack per push is an unbounded list, so the tiers consolidate. "
         "The cadence used to repack the WHOLE repository every 24 pushes — "
         "33× the bytes pushed, and one push held for 816 s. The tiers with "
         "a floor replaced it: 1.67–1.83× against 2.90–4.33× without, "
         "measured on a cluster. Supersede is object coverage, not "
         "reachability, so dead objects can pin live packs — 81% of the "
         "snapshot-named bytes are residue, n=3, 81/81/81."),
        ("the log, and the undo points", PAPER, S3_L,
         "log/<seq>.json: one immutable entry per batch, written once, so a "
         "follower may cache what it reads. undo/<seq>.json: before a batch "
         "that would make a state unreachable, the snapshot it is about to "
         "replace is kept for 7 days and both sweeps know it (X15, "
         "--undo-list)."),
        ("what the sweep must never do", WARN_F, WARN_L,
         "Delete an object a snapshot still names, or an object younger "
         "than the grace — a pack uploaded in step 4 of a batch whose CAS "
         "has not landed yet is exactly that. A fold running while a sweep "
         "runs is a modelled case, not a hoped-for one."),
    ]
    cw = (29.1 - 0.6 - 4 * 0.3) / 5
    x, hmax = 0.75, 0.0
    boxes = []
    for t, f, l, b in cols:
        s = p.fitbox(x, byy, cw, t, b, fill=f, line=l)
        boxes.append(s)
        hmax = max(hmax, s.h)
        x += cw + 0.3
    for s in boxes:
        s.h = hmax
    bz.resize_bottom(byy + hmax + 0.3)

    p.fitbox(0.45, bz.y + bz.h + 0.3, 29.1,
             "The quantitative axiom, stated because it cannot be discharged",
             "The model proves the renewer's sensor honest ONLY under the assumption that the challenger's polls and the holder's "
             "heartbeats share a period. That is a real-time property no model checker of this kind can establish; it is a "
             "configuration invariant the chart must keep true, and it is written down here so that it can be checked rather than "
             "assumed. The other edges, kept and named: told-failed-but-durable; a rollout mid-push losing the push; the export as "
             "a mirror nobody repairs; the push-only idle clock; and the blind poll that reads a Ready pod as Starting.",
             fill=NOTE_F, line=NOTE_L, body_size=8.2)
    return p.trim()


def find_chrome():
    for c in ("/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
              "/Applications/Chromium.app/Contents/MacOS/Chromium",
              shutil.which("google-chrome"), shutil.which("chromium")):
        if c and os.path.isfile(c) and os.access(c, os.X_OK):
            return c
    return None


def render_pdf(doc, svgs, out, chrome):
    """One PDF, one page per drawing, each at the drawing's own size.

    Chrome's `@page size` is per document, and these pages differ in
    height, so each is printed on its own and the results are merged —
    scaling them to a common sheet would shrink 7.5 pt body text to
    illegible, and the point of the PDF is to be read.
    """
    parts = []
    for p, svg in zip(doc.pages, svgs):
        with open(svg) as fh:
            markup = fh.read()
        html = os.path.splitext(svg)[0] + ".html"
        with open(html, "w") as fh:
            fh.write(
                "<!doctype html><meta charset='utf-8'><style>"
                "@page { size: %.4fin %.4fin; margin: 0 }"
                "html,body { margin:0; padding:0; background:#fff }"
                "svg { display:block; width:%.4fin; height:%.4fin }"
                "* { -webkit-print-color-adjust: exact; print-color-adjust: exact }"
                "</style>%s" % (p.width, p.height, p.width, p.height, markup))
        part = os.path.splitext(svg)[0] + ".pdf"
        subprocess.run([chrome, "--headless", "--disable-gpu", "--no-sandbox",
                        "--no-pdf-header-footer", "--print-to-pdf=" + part,
                        "file://" + os.path.abspath(html)],
                       check=True, capture_output=True)
        if not os.path.getsize(part):
            raise SystemExit("chrome wrote no PDF for " + svg)
        parts.append(part)
        os.remove(html)

    if shutil.which("pdfunite"):
        subprocess.run(["pdfunite"] + parts + [out], check=True)
    else:
        from pypdf import PdfWriter
        w = PdfWriter()
        for part in parts:
            w.append(part)
        w.write(out)
    for part in parts:
        os.remove(part)
    return out


def check_pdf(doc, out):
    """The PDF says what the drawings said: a page each, at each size."""
    if not shutil.which("pdfinfo"):
        return ["pdfinfo absent — page count and sizes UNVERIFIED"]
    txt = subprocess.run(["pdfinfo", "-f", "1", "-l", str(len(doc.pages)), out],
                         capture_output=True, text=True).stdout
    # `Page    2 size:  2160 x 792.96 pts` — anchored, because a loose
    # number scan reads the PAGE NUMBER as the width and then agrees with
    # nothing for the right reason
    got = re.findall(r"^Page\s+\d+ size:\s+([\d.]+) x ([\d.]+)", txt, re.M)
    bad = []
    if len(got) != len(doc.pages):
        bad.append("PDF has %d sized pages, expected %d"
                   % (len(got), len(doc.pages)))
    for p, (gw, gh) in zip(doc.pages, got):
        w, h = float(gw) / 72.0, float(gh) / 72.0
        if abs(w - p.width) > 0.06 or abs(h - p.height) > 0.06:
            bad.append("%s: PDF page %.2f x %.2f in, drawing %.2f x %.2f in"
                       % (p.name, w, h, p.width, p.height))
    return bad


def main():
    args = [a for a in sys.argv[1:] if not a.startswith("--")]
    outdir = args[0] if args else os.path.dirname(os.path.abspath(__file__))
    doc = k.Document(
        title="flint-forge — components, data flow and the pack path to S3",
        creator="flint",
        description="Five pages: components and data flow; the push as one "
                    "transaction; a commit's files to a pack to an S3 object; "
                    "who parses the git protocol; the lease and the bucket "
                    "over time.")
    for fn in (page1, page2, page3, page4, page5):
        fn(doc)

    problems = doc.check() + doc.overlap_report()
    for msg in problems:
        print(msg)
    for msg in doc.slack_report(0.7):
        print(msg)
    print("%d shapes, %d pages, %d problems"
          % (sum(len(p.shapes) for p in doc.pages), len(doc.pages), len(problems)))

    out = os.path.join(outdir, "flint-forge.vsdx")
    doc.save(out)
    print("wrote", out)

    want_pdf = "--pdf" in sys.argv
    if "--preview" in sys.argv or want_pdf:
        svgs = doc.save_svg(os.path.join(outdir, "preview"))
        for path in svgs:
            print("wrote", path)
        if want_pdf:
            chrome = find_chrome()
            if not chrome:
                raise SystemExit("no Chrome/Chromium found — cannot render PDF")
            pdf = render_pdf(doc, svgs,
                             os.path.join(outdir, "flint-forge.pdf"), chrome)
            bad = check_pdf(doc, pdf)
            for msg in bad:
                print("  PDF CHECK:", msg)
            problems += bad
            print("wrote", pdf)
            if "--preview" not in sys.argv:
                for path in svgs:
                    os.remove(path)
    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(main())
