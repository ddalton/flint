#!/usr/bin/env python3
"""Build `flint-forge-dataflow.vsdx` — ONE page, the data flow, symbols.

The companion to `forge-visio.py`, which is the five-page reference. This
one is the picture you put on a wall: the components that matter, the
arrows between them, and a label on each arrow rather than a paragraph
inside each box. Every component is drawn with the symbol its role
deserves — a cylinder for a store, a hexagon for a gateway, a 3-D box
for the server, a data-flow store for the cache, an actor for the human,
a cloud for somebody else's boundary.

Run:  python3 forge-dataflow.py [outdir] [--preview] [--pdf]
"""

import os
import sys

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import vsdxkit as k

# reuse the sibling's Chrome plumbing rather than a second copy of it
_here = os.path.dirname(os.path.abspath(__file__))
_spec = __import__("importlib.util", fromlist=["util"]).spec_from_file_location(
    "forge_visio", os.path.join(_here, "forge-visio.py"))
_fv = __import__("importlib.util", fromlist=["util"]).module_from_spec(_spec)
_spec.loader.exec_module(_fv)

INK, SUB, MUTE, PAPER = "#14181D", "#3D4650", "#6B7683", "#FFFFFF"
CLIENT_F, CLIENT_L = "#E4EEF9", "#2E6FB7"
DOOR_F, DOOR_L = "#D6F0EC", "#17847A"
GIT_F, GIT_L = "#FCE3CD", "#C0651A"
SYNC_F, SYNC_L = "#E6DBF6", "#6F45B5"
OPER_F, OPER_L = "#DFEFD4", "#4B8B2B"
S3_F, S3_L = "#FFF1CF", "#B0862A"
CACHE_F, CACHE_L = "#EDEFF2", "#78828E"
CLOUD_F, CLOUD_L = "#FFFCF2", "#C9A44C"
ZONE_L = "#A9B2BD"

FLOW, DUR, CTL, BYPASS = "#2E6FB7", "#6F45B5", "#4B8B2B", "#B0862A"

W = 0.014          # standard flow weight
WT = 0.022         # the thick one, for the path every byte takes


def node(p, kind, x, y, w, h, title="", body="", **kw):
    """A component: centred text, and the symbol its role deserves."""
    kw.setdefault("halign", 1)
    kw.setdefault("valign", 1)
    kw.setdefault("title_size", 10.5)
    kw.setdefault("body_size", 7.8)
    kw.setdefault("body_color", SUB)
    return p.symbol(kind, x, y, w, h, title, body, **kw)


def zone(p, x, y, w, h, label, color=ZONE_L):
    p.box(x, y, w, h, "", "", fill="#FCFDFE", line=color, dashed=True,
          rounding=0.14, line_weight=0.009)
    p.text(x + 0.18, y + 0.13, w - 0.36, label, size=9.5, color="#5A646F",
           bold=True)


def flabel(p, cx, cy, text, color=FLOW, size=7.8, w=1.9):
    """An arrow's label — TRANSPARENT, and therefore placed BESIDE the
    arrow rather than on it. A paper fill would break the line it names
    and hide whatever else runs under it; `label_on_line_report()` is
    what keeps the placement honest."""
    return p.text(cx - w / 2.0, cy - 0.115, w, text, size=size, color=color,
                  bold=True, halign=1, label=True)


GLOSSARY = [
    ("CAS", "compare-and-swap — a write that lands only if the object is "
            "still the version you read"),
    ("ETag / If-Match", "S3's precondition: the ETag you read, offered back "
                        "as the terms of the write"),
    ("fence", "a lost CAS. Another server holds this repository, so this one "
              "stops READING too, and exits"),
    ("CGI", "Common Gateway Interface — how the runner invokes git "
            "http-backend, once per request"),

    ("UDS", "Unix domain socket — the hook-to-syncer channel, inside the pod, "
            "never over a network"),
    ("proc-receive", "the git hook that lets the server, not git, decide each "
                     "ref update"),
    ("pre-receive", "the git hook that applies the branch policy, and can "
                    "refuse the whole push"),
    ("emptyDir", "a Kubernetes volume that lives and dies with the POD — not "
                 "with a container"),

    ("SA token", "a Kubernetes ServiceAccount token, projected into the pod "
                 "and rotated by the kubelet"),
    ("TokenReview", "the Kubernetes API that says whether a ServiceAccount "
                    "token is still valid"),
    ("JWT", "JSON Web Token — a signed bearer token, verified at the door "
            "with no round trip"),
    ("JWKS", "JSON Web Key Set — the issuer's signing keys. The KEYS are "
             "cached, the verdicts are not"),

    ("iss / sub / exp", "a JWT's issuer, its subject — the principal — and "
                        "its expiry"),
    ("LFS", "Git Large File Storage — big blobs kept beside the repository, "
            "moved by presigned URL"),
    ("bundle URI", "git's own opt-in: the server names a pack the client "
                   "fetches straight from S3"),
    ("FlintRepo", "the custom resource that one forge repository is declared "
                  "as"),
]


def glossary(p, x, y, w, cols=4):
    """A reference strip: term above gloss, rows levelled so it reads as a grid."""
    p.text(x, y, w, "What the abbreviations mean", size=10, color=INK, bold=True)
    y += 0.32
    gap = 0.36
    cw = (w - gap * (cols - 1)) / cols
    for row in range(0, len(GLOSSARY), cols):
        boxes = []
        for i, (term, gloss) in enumerate(GLOSSARY[row:row + cols]):
            boxes.append(p.fitbox(
                x + i * (cw + gap), y, cw, term, gloss, pad=0.02,
                title_size=8.6, body_size=7.5, title_color=INK,
                body_color=SUB, no_fill=True, no_line=True))
        tall = max(b.h for b in boxes)
        for b in boxes:
            b.h = tall
        y += tall + 0.18
    return y


def build():
    doc = k.Document(
        title="flint-forge — data flow",
        creator="flint",
        description="One page: the components that matter, the data flow "
                    "between them, and a glossary.")
    p = doc.page("Data flow", 22.4, 14.0)

    p.text(0.5, 0.34, 9.0, "flint-forge", size=9.5, color=MUTE, bold=True)
    p.text(0.5, 0.6, 21.4, "Data flow", size=20, color=INK, bold=True)
    p.text(0.5, 1.06, 21.4,
           "Stock git serves every byte. One syncer beside it is the only "
           "process that can write S3.", size=10, color=SUB)

    spine = 3.4

    # ---- boundaries ---------------------------------------------------
    zone(p, 0.5, 2.5, 3.3, 4.4, "consumer cluster")
    zone(p, 3.95, 1.85, 11.6, 6.4, "flint hub cluster")
    zone(p, 8.0, 1.95, 7.45, 6.1, "forge pod — one per repository")
    node(p, "cloud", 15.7, 2.35, 5.95, 5.4, "", "", fill=CLOUD_F,
         line=CLOUD_L, line_weight=0.012)
    p.text(15.9, 2.82, 5.55, "S3-compatible object storage", size=10.5,
           color="#8A6A1E", bold=True, halign=1)

    # TWO containers, and their labels sit at the FOOT of each box: a
    # label at the top pushes the component that matters off the spine
    for cx, cw, cname in ((8.15, 4.05, "container:  git-http"),
                          (12.45, 2.6, "container:  syncer")):
        p.box(cx, 2.4, cw, 4.4, "", "", fill="#FFFFFF", line="#B9C0C8",
              dashed=True, rounding=0.1, line_weight=0.009)
        p.text(cx, 6.45, cw, cname, size=9, color="#5A646F", bold=True,
               halign=1)

    # ---- the clients --------------------------------------------------
    node(p, "actor", 1.72, 2.92, 0.95, 0.95, "", "", fill=CLIENT_F,
         line=CLIENT_L, line_weight=0.013)
    p.text(0.7, 3.95, 2.9, "developer / agent pod", size=10, color=INK,
           bold=True, halign=1)
    p.text(0.7, 4.16, 2.9, "stock git  ·  SA token", size=7.8, color=SUB,
           halign=1)

    node(p, "document", 1.72, 4.9, 0.95, 0.95, "", "", fill=CLIENT_F,
         line=CLIENT_L, line_weight=0.013)
    p.text(0.7, 5.93, 2.9, "app / browser", size=10, color=INK, bold=True,
           halign=1)
    p.text(0.7, 6.14, 2.9, "REST file API  ·  JWT", size=7.8, color=SUB,
           halign=1)

    # ---- the door and the two verifiers -------------------------------
    node(p, "hexagon", 4.35, 2.55, 2.95, 1.7, "the door",
         "authenticate\nauthorise · route · wake",
         fill=DOOR_F, line=DOOR_L, line_weight=0.014, notch=0.16)
    node(p, "hexagon", 4.05, 4.75, 1.85, 1.2, "kube-apiserver",
         "TokenReview\ncached ≤ 60 s", fill="#EEF1F5", line=MUTE,
         title_size=8.8, body_size=7.4, notch=0.13)
    node(p, "hexagon", 6.1, 4.75, 1.85, 1.2, "issuer JWKS",
         "verify offline\nverdicts never cached", fill="#E8E4F4",
         line="#7A6BB5", title_size=8.8, body_size=7.4, notch=0.13)
    node(p, "hexagon", 4.35, 6.5, 2.95, 1.4, "operator",
         "FlintRepo → pod\npoll /status · park at 0",
         fill=OPER_F, line=OPER_L, line_weight=0.014, notch=0.14)

    # ---- inside the pod ------------------------------------------------
    node(p, "box3d", 8.4, 2.575, 3.55, 1.65, "git",
         "http-backend\nreceive-pack · upload-pack",
         fill=GIT_F, line=GIT_L, line_weight=0.014, depth=0.1)
    node(p, "circle", 9.4, 4.6, 1.5, 1.45, "proc-receive", "the seam",
         fill="#FFF0DF", line=GIT_L, line_weight=0.014, title_size=9.2,
         body_size=7.4)
    p.text(8.3, 6.12, 3.75,
           "a git HOOK — a process receive-pack spawns per push, in this "
           "container. Same binary as the syncer.", size=7.2, color=MUTE,
           halign=1)
    node(p, "circle", 12.95, 2.5, 1.8, 1.8, "syncer", "the only\nwriter",
         fill=SYNC_F, line=SYNC_L, line_weight=0.017, title_size=11)

    node(p, "datastore", 8.6, 7.0, 6.7, 0.78, "emptyDir  /repo",
         "the bare repo — objects/pack, refs, HEAD  ·  mounted in BOTH "
         "containers  ·  dies with the pod",
         fill=CACHE_F, line=CACHE_L, title_size=9.5, line_weight=0.013)

    # ---- the stores ----------------------------------------------------
    node(p, "cylinder", 16.7, 3.3, 3.95, 0.95, "packs",
         "immutable · content-named", fill=S3_F, line=S3_L,
         line_weight=0.013, cap=0.3)
    node(p, "cylinder", 16.7, 4.5, 3.95, 0.95, "snapshot",
         "THE pointer · one CAS", fill="#FFE9B8", line=S3_L,
         line_weight=0.015, cap=0.3)
    node(p, "cylinder", 16.7, 5.7, 3.95, 0.95, "epoch",
         "the single-writer lease", fill=S3_F, line=S3_L,
         line_weight=0.013, cap=0.3)

    # ---- the flows -----------------------------------------------------
    # 1 · a client to the door
    p.arrow([(2.72, spine), (4.35, spine)], color=FLOW, weight=WT)
    flabel(p, 3.5, spine - 0.2, "push · clone")

    # 1b · the file API, in at the same door
    p.arrow([(2.72, 5.37), (3.25, 5.37), (3.25, 4.62), (5.2, 4.62),
             (5.2, 4.25)], color=FLOW, weight=W)
    flabel(p, 4.15, 4.44, "GET / PUT a path", w=1.55, size=7.4)

    # 2 · the fork: the door routes on the token's `iss`
    p.arrow([(5.82, 4.25), (5.82, 4.42), (4.97, 4.42), (4.97, 4.75)],
            color=CTL, weight=W, dashed=True)
    p.arrow([(5.82, 4.25), (5.82, 4.42), (7.02, 4.42), (7.02, 4.75)],
            color=CTL, weight=W, dashed=True)
    flabel(p, 5.72, 4.60, "SA token", CTL, w=1.00, size=7.4)
    flabel(p, 6.62, 4.60, "JWT", CTL, w=0.62, size=7.4)

    # 3 · the door to git
    p.arrow([(7.3, spine), (8.4, spine)], color=FLOW, weight=WT)
    flabel(p, 7.8, spine - 0.2, "CGI", w=0.7)

    # 4 · git hands the hook the commands
    p.arrow([(10.15, 4.225), (10.15, 4.6)], color=FLOW, weight=W)
    flabel(p, 11.0, 4.41, "commands", w=1.3)

    # 5 · the hook hands the syncer a request, and waits
    p.arrow([(10.9, 5.325), (12.32, 5.325), (12.32, spine), (12.95, spine)],
            color=DUR, weight=WT)
    flabel(p, 11.50, 4.78, "request · UDS", DUR, w=1.5)

    # 6 · git and the cache
    p.arrow([(9.05, 4.225), (9.05, 7.0)], color=FLOW, weight=W,
            begin_arrow=k.ARROW_FILLED)
    flabel(p, 8.50, 5.35, "objects", w=0.85)

    # 7 · the restore rebuilds the cache from the bucket
    p.arrow([(13.85, 4.3), (13.85, 7.0)], color=DUR, weight=W, dashed=True)
    flabel(p, 14.50, 5.70, "restore", DUR, w=1.1)

    # 8 · the syncer and the three stores — radial, so nothing elbows
    p.arrow([(14.72, 3.6), (16.7, 3.775)], color=DUR, weight=WT,
            begin_arrow=k.ARROW_FILLED)
    flabel(p, 15.50, 3.40, "PUT · GET", DUR, w=1.3)
    p.arrow([(14.6, 4.0), (16.7, 4.975)], color=DUR, weight=WT,
            begin_arrow=k.ARROW_FILLED)
    flabel(p, 15.65, 4.04, "CAS · read", DUR, w=1.4)
    p.arrow([(14.35, 4.25), (16.7, 6.175)], color=DUR, weight=W,
            begin_arrow=k.ARROW_FILLED)
    flabel(p, 15.65, 6.00, "renew · poll", DUR, w=1.2)

    # 9 · the control plane, which reads no object
    p.arrow([(7.3, 6.95), (8.0, 6.95)], color=CTL, weight=W, dashed=True)
    p.arrow([(8.0, 7.35), (7.3, 7.35)], color=CTL, weight=W, dashed=True)
    flabel(p, 7.65, 6.77, "reconcile", CTL, w=1.2, size=7.4)
    flabel(p, 7.65, 7.53, "/status", CTL, w=1.0, size=7.4)

    # 10 · the bytes that never enter the pod
    p.arrow([(2.2, 2.92), (2.2, 1.62), (21.35, 1.62), (21.35, 3.775),
             (20.65, 3.775)], color=BYPASS, weight=W, dashed=True)
    flabel(p, 10.4, 1.44, "bundle URI · LFS — presigned, client to store",
           BYPASS, w=4.4)

    # ---- the notes the picture cannot carry ----------------------------
    y = 8.55
    p.text(0.55, y, 21.3,
           "TWO CREDENTIAL KINDS, SERVED AT ONCE. The door routes on the "
           "token's iss: a pod's ServiceAccount token goes to TokenReview at "
           "the apiserver, an issuer-minted JWT to offline signature "
           "verification against the cached JWKS. A JWT's sub — a person — "
           "becomes the principal for spec.consumers, for the branch policy, "
           "and as the commit author.", size=8.6, color=SUB)
    p.text(0.55, y + 0.28, 21.3,
           "AND THE DOOR REFUSES TO START if the cluster's ServiceAccount "
           "issuer and the configured JWT issuer are the same string: every "
           "pod token would then route to the offline verifier, which cannot "
           "know a pod was deleted.", size=8.6, color=SUB)
    p.text(0.55, y + 0.56, 21.3,
           "ONE CAS ON THE SNAPSHOT is the transaction boundary. Told “ok” "
           "means that object moved; a 412 on it is a fence, not a retry.",
           size=8.6, color=SUB)

    # ---- legend --------------------------------------------------------
    y += 1.05
    items = [("data plane — every byte of a clone or push", FLOW, False),
             ("durable path — the only writer of the bucket", DUR, False),
             ("control plane — never reads an object", CTL, True),
             ("bypass — the pod sees a few hundred bytes", BYPASS, True)]
    x = 0.55
    for text, color, dash in items:
        p.arrow([(x, y), (x + 0.62, y)], color=color, weight=W, dashed=dash)
        p.text(x + 0.72, y - 0.115, 4.5, text, size=8, color=SUB)
        x += 5.4

    # ---- glossary ------------------------------------------------------
    glossary(p, 0.55, y + 0.45, 21.3)
    return doc, p


def main():
    args = [a for a in sys.argv[1:] if not a.startswith("--")]
    outdir = args[0] if args else _here
    doc, _ = build()

    problems = (doc.check() + doc.overlap_report()
                + doc.label_overlap_report() + doc.label_on_line_report())
    for msg in problems:
        print(msg)
    print("%d shapes, %d problems" % (len(doc.pages[0].shapes), len(problems)))

    out = os.path.join(outdir, "flint-forge-dataflow.vsdx")
    doc.save(out)
    print("wrote", out)

    if "--preview" in sys.argv or "--pdf" in sys.argv:
        svgs = doc.save_svg(os.path.join(outdir, "dataflow-preview"))
        if "--pdf" in sys.argv:
            chrome = _fv.find_chrome()
            if not chrome:
                raise SystemExit("no Chrome/Chromium found")
            pdf = _fv.render_pdf(doc, svgs, os.path.join(
                outdir, "flint-forge-dataflow.pdf"), chrome)
            for msg in _fv.check_pdf(doc, pdf):
                print("  PDF CHECK:", msg)
                problems.append(msg)
            print("wrote", pdf)
            if "--preview" not in sys.argv:
                for s in svgs:
                    os.remove(s)
    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(main())
