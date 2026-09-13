#!/usr/bin/env python3
"""Build `flint-acid-passthrough-vs-lean.vsdx` — ONE page: the four ACID
properties, asked of the two front ends that write plain objects.

The sixth poster is not a front end but a COMPARISON, drawn with the same
kit as the five so a reader who has seen them recognises every shape.
Its question is the one a reader asks after the passthrough and lean
posters: both put whole objects in a bucket, so what does each one
actually promise about a write? The answer is that they keep different
ledgers. Passthrough keeps one — the bucket — and inherits exactly S3's
guarantees: strong per object, nothing across objects. Lean keeps two —
the objects and a manifest that cites them — and the CAS on the manifest
is what turns N object writes into one transaction. Everything lean adds
is paid for with a recovery point on the agent's writes that passthrough
does not have.

Sources of record: spdk-csi-driver/src/passthrough/spec.rs (the write
model and the one-mounter rule), docs/flint-approach-radar.html
("Mountpoint for S3 completes the upload on fsync/close"),
lean/e2e/perf/results/door-drill-2026-09-10.md (no data cache, Minimal
metadata TTL without --cache), the lean poster and the syncer/gateway
code under lean/, and lean/formal/LeanSubtree.tla for the invariants.

Run:  python3 acid-poster.py [outdir] [--preview] [--pdf] [--emf]
"""

import os
import sys

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import dataflowkit as d      # noqa: E402

_here = os.path.dirname(os.path.abspath(__file__))
k = d.k

INK, SUB, MUTE = d.INK, d.SUB, d.MUTE
FLOW, DUR, CTL, ALT = d.FLOW, d.DUR, d.CTL, d.ALT
W, WT = d.W, d.WT

# the two columns wear their front ends' own hues, as the deck defines them
PASS_L, PASS_F = d.ACCENT["pass"], "#FAF3E6"
LEAN_L, LEAN_F = d.ACCENT["lean"], "#ECF8F2"

GLOSSARY = [
    ("ACID", "atomic, consistent, isolated, durable: the four questions a "
             "database answers about one transaction"),
    ("the transaction", "the unit the four questions are asked of. For "
                        "passthrough, ONE object; for lean, ONE boundary"),
    ("close()", "where a passthrough file becomes an object: the mounter "
                "completes the upload on close or fsync, and not before"),
    ("metadata TTL", "how long the mount trusts a listing or a stat before "
                     "asking S3 again — Minimal unless --cache is set"),

    ("CAS", "compare-and-swap — a write that lands only if the object is "
            "still the version you read"),
    ("ETag / If-Match", "S3's precondition: the ETag you read, offered back "
                        "as the terms of the write"),
    ("boundary", "a coherent point: the set of files one manifest generation "
                 "cites, installed by ONE CAS on the pointer"),
    ("the pointer", ".flint/lean/current — the ONE mutable metadata object. "
                    "Entries live in immutable, chunked manifests"),

    ("epoch · lease", "the bucket-side single-writer cell. A syncer that loses "
                      "the CAS is fenced and says so"),
    ("the inbox", "the queue of UI writes waiting to be cited. An entry names "
                  "an object and its ETag, never a manifest edit"),
    ("CRC-64", "the checksum the manifest carries per entry, computed by the "
               "client that moved the bytes; every fetch is checked"),
    ("RPO", "recovery point. Passthrough has none to state; lean's is the "
            "last BARRIER, never the last write"),
]


def cell(p, x, y, w, title, body, line, fill):
    return p.fitbox(x, y, w, title, body, pad=0.14, title_size=8.8,
                    body_size=7.35, title_color=line, body_color=SUB,
                    fill=fill, line=line, line_weight=0.011, rounding=0.06)


def build():
    doc = k.Document(
        title="flint — ACID: passthrough vs lean",
        creator="flint",
        description="One page: the four ACID properties, asked of the two "
                    "front ends that write whole objects, side by side.")
    p = doc.page("ACID", 22.4, 14.0)

    d.header(p, "lean", "ACID — passthrough vs lean",
             "Both doors put whole objects in a bucket. Passthrough keeps ONE "
             "ledger and inherits S3's guarantees exactly; lean keeps TWO — "
             "the objects and a manifest that cites them — and pays for "
             "every property it adds with a recovery point on the agent's "
             "writes.",
             name="passthrough · flint-lean")

    # ---- the two write paths, side by side ------------------------------
    y0 = 2.05
    # passthrough: one file, one object, one PUT at close
    d.zone(p, 0.5, y0, 10.45, 2.50, "passthrough — one file IS one object",
           color=PASS_L,
           sub="the transaction is a PUT, and close() is its commit")
    d.node(p, "rect", 0.75, y0 + 0.72, 2.55, 1.05, "the pod",
           "write() … write() … close()\nbuffered in the mounter until close",
           fill=d.CLIENT_F, line=d.CLIENT_L, line_weight=0.013,
           title_size=9.6, body_size=7.2)
    d.node(p, "rect", 4.35, y0 + 0.72, 2.45, 1.05, "mount-s3",
           "sequential writes to a whole object; no rename, no append, no "
           "in-place edit",
           fill=d.WORK_F, line=d.WORK_L, line_weight=0.013, title_size=9.6,
           body_size=7.2)
    d.node(p, "cylinder", 7.95, y0 + 0.72, 2.75, 1.05, "<prefix>/<key>",
           "the object. Old or new, never torn — and nothing else in the "
           "bucket says which files belong together",
           fill=d.S3_F, line=d.S3_L, line_weight=0.013, cap=0.24,
           title_size=9.2, body_size=7.0)
    sp = y0 + 1.245
    p.arrow([(3.30, sp), (4.35, sp)], color=FLOW, weight=WT)
    d.flabel(p, 3.83, sp - 0.24, "FUSE", w=0.9, size=7.2)
    p.arrow([(6.80, sp), (7.95, sp)], color=DUR, weight=WT)
    d.flabel(p, 7.38, sp - 0.24, "PUT at close", DUR, w=1.1, size=7.2)
    d.caption(p, 0.75, y0 + 1.98, 9.95,
              "three edited files are three PUTs, in close() order. A reader "
              "between the second and the third sees two new files and one "
              "old one, and nothing tells it so.", size=7.3, halign=0)

    # lean: N files, one CAS
    x1 = 11.45
    d.zone(p, x1, y0, 10.45, 2.50, "lean — the transaction is the BOUNDARY",
           color=LEAN_L,
           sub="N whole objects, then ONE CAS on the pointer cites them all")
    d.node(p, "rect", x1 + 0.25, y0 + 0.72, 2.35, 1.05, "the pod",
           "write() lands on local disk: visible at once, durable later",
           fill=d.CLIENT_F, line=d.CLIENT_L, line_weight=0.013,
           title_size=9.6, body_size=7.2)
    d.node(p, "rect", x1 + 3.45, y0 + 0.72, 2.20, 1.05, "flint-sync",
           "the only writer of the prefix — it holds the lease",
           fill=d.WORK_F, line=d.WORK_L, line_weight=0.013, title_size=9.6,
           body_size=7.2)
    d.node(p, "cylinder", x1 + 6.75, y0 + 0.45, 3.45, 0.72, "files/<path>",
           "new whole objects, durable and uncited",
           fill=d.S3_F, line=d.S3_L, line_weight=0.013, cap=0.20,
           title_size=9.0, body_size=7.0)
    d.node(p, "cylinder", x1 + 6.75, y0 + 1.42, 3.45, 0.72,
           ".flint/lean/current",
           "ONE CAS installs the whole set",
           fill=d.S3_HOT_F, line=d.S3_L, line_weight=0.015, cap=0.20,
           title_size=9.0, body_size=7.0)
    p.arrow([(x1 + 2.60, sp), (x1 + 3.45, sp)], color=FLOW, weight=WT,
            dashed=True)
    d.flabel(p, x1 + 3.03, sp - 0.24, "same dir", w=0.9, size=7.2)
    p.arrow([(x1 + 5.65, y0 + 0.95), (x1 + 6.75, y0 + 0.81)], color=DUR,
            weight=WT)
    d.flabel(p, x1 + 6.20, y0 + 0.52, "upload", DUR, w=0.8, size=7.2)
    p.arrow([(x1 + 5.65, y0 + 1.55), (x1 + 6.75, y0 + 1.78)], color=DUR,
            weight=WT)
    d.flabel(p, x1 + 6.20, y0 + 1.92, "then CAS", DUR, w=0.9, size=7.2)
    d.caption(p, x1 + 0.25, y0 + 1.98, 9.95,
              "a checkout, a sync or the gateway reads the boundary entire or "
              "not at all. The price: the agent's write is not in S3 until "
              "the next barrier.", size=7.3, halign=0)

    # ---- the matrix -----------------------------------------------------
    y = y0 + 2.75
    qx, qw = 0.5, 2.55
    cw = (21.9 - qx - qw - 2 * 0.22) / 2.0
    px = qx + qw + 0.22
    lx = px + cw + 0.22

    p.box(px, y, cw, 0.36, "", "", fill=PASS_L, line=PASS_L, rounding=0.05)
    p.text(px, y + 0.045, cw, "flint-passthrough — one ledger: the bucket",
           size=9.4, color="#FFFFFF", bold=True, halign=1)
    p.box(lx, y, cw, 0.36, "", "", fill=LEAN_L, line=LEAN_L, rounding=0.05)
    p.text(lx, y + 0.045, cw, "flint-lean — two ledgers: the objects, and "
           "the manifest that cites them", size=9.4, color="#FFFFFF",
           bold=True, halign=1)
    y += 0.50

    ROWS = [
        ("A — atomic",
         "does a reader ever see half of a write? And what is the unit — "
         "one file, or a set?",
         ("PER OBJECT, YES — and that is the whole unit.",
          "A file becomes an object when it is closed; until then nothing is "
          "in S3 and no reader sees it. S3's PUT is all-or-nothing, so a "
          "reader sees the previous object or the new one, never a torn "
          "one. ACROSS FILES, NO: three edited files are three independent "
          "PUTs landing in close() order, and a reader between them sees one "
          "new and two old. No rename, no append and no in-place "
          "modification at any setting, so “edit” is always a "
          "whole-object replace — and a directory move is N copies."),
         ("PER BOUNDARY — N files, one CAS.",
          "Every changed file is uploaded as a whole new object, then ONE "
          "CAS on .flint/lean/current cites the whole set: a checkout, a sync "
          "or the gateway reads the boundary entire or not at all. A rename "
          "is one generation — destination entry and source removal in the "
          "same CAS. A UI's write is two steps, object then inbox entry, and "
          "is cited at the syncer's next barrier. The one reader with no "
          "atomicity is a raw-key reader on files/: a browser, or a "
          "passthrough mount on the same prefix, sees objects land one by "
          "one.")),
        ("C — consistent",
         "what invariant holds after every write, and who checks it?",
         ("S3's, UNMEDIATED.",
          "Read-after-write per object: a completed PUT is visible to every "
          "reader's next GET. No invariant spans two objects, and flint adds "
          "none — no manifest, no checksum of its own, no claim on the "
          "prefix; the bucket is left exactly as other tooling finds it. "
          "What the MOUNT adds is staleness: a metadata TTL (Minimal unless "
          "--cache is set; a CR may pass its own) during which a listing or "
          "a stat can be old, and no data cache unless --cache is set, so a "
          "re-read is a re-GET."),
         ("MANUFACTURED by the manifest, and CHECKED on every read.",
          "A generation cites, per path, the object's ETag and a CRC-64 the "
          "writer computed; every fetch is verified against it and a "
          "mismatch refuses the checkout with nothing written. The claim "
          "refuses a foreign prefix; the epoch fences a deposed writer on "
          "every request. The invariants are machine-checked "
          "(LeanSubtree.tla): every acked UI write stays tracked until "
          "cited, and a performed rename never shows the bytes under both "
          "names, or under neither.")),
        ("I — isolated",
         "can two writers, or a writer and a reader, corrupt each other?",
         ("NONE, BY DESIGN.",
          "Two pods on one prefix do not see each other's intent: two "
          "writers of one key are last-close-wins, undetected — no lock, no "
          "lease, no conditional PUT. Two readers can each see a different "
          "mix of generations across files. A reader never sees a writer's "
          "partial object, because it is not in S3 until close. The only "
          "lever is readOnly, per mount. A pod that wants git, pip or sqlite "
          "on the tree wants lean, whose boundary is what makes those "
          "safe."),
         ("ONE WRITER PER PREFIX, enforced in the bucket.",
          "The syncer holds the lease (the epoch cell) and is fenced on a "
          "lost CAS: a second syncer is refused, not merged. UI writers are "
          "the SECOND writer, serialised through the inbox — a write is "
          "refused 409 + Retry-After while a barrier window is open, an "
          "overwrite must name what it read (428 / 412), and a draft is "
          "invisible until promoted. When the agent and a UI touch the same "
          "path, locally-dirty wins and the UI's bytes are preserved as a "
          "conflict copy, never deleted.")),
        ("D — durable",
         "when the call returns, where are the bytes — and what can still "
         "lose them?",
         ("AT close().",
          "write() buffers in the mounter; close() or fsync completes the "
          "upload and returns only when S3 holds the object, so a returned "
          "close() is durable in S3 and there is no recovery point to state "
          "— nothing else is buffered. The same fact from the other side: "
          "an open file whose mounter dies is lost whole, nothing is on "
          "local disk, and every read is a request in the pod's critical "
          "path."),
         ("AT THE BARRIER.",
          "write() returns when local disk has the bytes — visible at once, "
          "durable later. The syncer uploads at cadence (floorSecs) or when "
          "the agent writes .flint/publish, and the ack says ok only when "
          "the boundary is in the bucket. RPO is the last barrier, never the "
          "last write: a pod that dies between barriers loses the "
          "difference (a graceful shutdown drains at NodeUnpublish). Gated "
          "mode uploads first and cites later, so bytes are durable NOW and "
          "visible on one CAS. A checkout fsyncs every file before it "
          "writes the marker that vouches for it; a UI's PUT is durable when "
          "it returns.")),
    ]

    for label, question, (pt, pb), (lt, lb) in ROWS:
        q = p.fitbox(qx, y, qw, label, question, pad=0.14, title_size=10.2,
                     body_size=7.4, title_color=INK, body_color=MUTE,
                     fill=d.PLAIN_F, line=d.PLAIN_L, line_weight=0.010,
                     rounding=0.06)
        a = cell(p, px, y, cw, pt, pb, PASS_L, PASS_F)
        b = cell(p, lx, y, cw, lt, lb, LEAN_L, LEAN_F)
        tall = max(q.h, a.h, b.h)
        for s in (q, a, b):
            s.h = tall
        y += tall + 0.16

    # ---- the tie, and the two it cannot give ------------------------------
    y += 0.06
    p.box(0.5, y, 10.45, 0.92, "what TIES — per object, both are S3",
          "untorn, read-after-write, strongly consistent since 2020-12. A "
          "single-file edit through either door is equally safe; the "
          "difference begins with the second file, and with the question "
          "of who else may be writing.",
          fill=d.PLAIN_F, line=d.PLAIN_L, line_weight=0.011, title_size=9.6,
          body_size=7.4, body_color=SUB)
    p.box(11.45, y, 10.45, 0.92, "what NEITHER gives",
          "multi-writer coherence, byte-range locks, cross-pod O_EXCL — "
          "those need an arbiter that answers in milliseconds, which is the "
          "hub. And neither is isolated from a raw-key writer holding the "
          "credential: lean DETECTS the foreign overwrite (If-Match on "
          "consume, a conflict copy); passthrough cannot.",
          fill="#FDECEC", line="#C0392B", line_weight=0.011, title_size=9.6,
          body_size=7.4, body_color="#8C2F22")
    y += 0.92 + 0.30

    # ---- the notes the matrix cannot carry --------------------------------
    y = d.notes(p, 0.55, y, 21.3, [
        "TWO LEDGERS IS THE WHOLE DIFFERENCE. Passthrough writes objects "
        "and nothing else, so the bucket is the transaction log and S3's "
        "per-object guarantees are the entire contract — which is exactly "
        "why a passthrough mount can be pointed at a prefix somebody else's "
        "tooling owns and leave it as it was. Lean writes objects AND a "
        "manifest that cites them, and the CAS on the manifest is what "
        "makes N objects one transaction, gives every fetch a checksum to "
        "verify against, and lets a lease say who may write. Every one of "
        "those is bought with the barrier: the agent's write is visible on "
        "local disk at once and in S3 only at the next boundary.",

        "THE UI'S WRITE IS A THIRD CONTRACT, and it is the agent's turned "
        "around. Through the gateway — or the flint-lean-gateway crate "
        "called in-process — a write is DURABLE when the call returns "
        "(object first, inbox entry second) and CITED at the syncer's next "
        "barrier; the agent's write is VISIBLE when write() returns and "
        "DURABLE at the barrier. Both keep the manifest single-writer, "
        "which is what keeps the two safe together, and every gateway "
        "reader sees the UI's bytes at once because the read door overlays "
        "the inbox on the citation.",

        "“CONSISTENT” MEANS SOMETHING DIFFERENT AT EACH DOOR. At "
        "passthrough it is S3's word: read-after-write on one key. At lean "
        "it is the database's word: an invariant that holds across the "
        "whole tree after every transaction — every cited object exists at "
        "the cited ETag with the cited CRC, no path is under two names, and "
        "no acked write is lost. The first is inherited; the second is "
        "manufactured, and it is manufactured by the same manifest that "
        "costs the recovery point.",

        "READ THE DURABILITY ROW WITH ITS COST. Passthrough's close() is "
        "the strongest single-file durability on the page — the bytes are "
        "in S3 before the call returns — and it is paid for on every read "
        "and every write with a round trip in the pod's critical path. "
        "Lean's local-disk ack is the weakest, and it is what lets git, "
        "sqlite and a build cache run at local speed; the boundary verbs "
        "exist so the agent can buy durability exactly when it wants it, "
        "for exactly the files it names.",
    ])

    # ---- legend + glossary ------------------------------------------------
    y += 0.22
    d.legend(p, 0.55, y, [
        ("the pod's write, as it leaves the pod", FLOW, False),
        ("the object path — what the bucket stores", DUR, False),
        ("one directory, two views — never a wire (lean)", FLOW, True),
    ], span=6.6)
    d.glossary(p, 0.55, y + 0.45, 21.3, GLOSSARY)
    return doc, p


def main():
    args = [a for a in sys.argv[1:] if not a.startswith("--")]
    outdir = args[0] if args else _here
    doc, page = build()
    return d.emit(doc, page, outdir, "flint-acid-passthrough-vs-lean",
                  sys.argv)


if __name__ == "__main__":
    sys.exit(main())
