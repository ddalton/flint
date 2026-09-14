#!/usr/bin/env python3
"""Build `flint-lean-protocol.vsdx` — ONE page: the lean protocol.

The seventh poster draws what the lean data-flow poster cannot: several
writers on one workspace, and the rule that keeps them coherent. Lean is a
STRONGLY CONSISTENT SHARED LOG with EVENTUALLY CONSISTENT WORKING COPIES —
git with automatic push and pull. The bucket orders every boundary with one
conditional write of the manifest pointer; each writer's tree is a working
copy that its TICK keeps close to that log, and a UI reads and writes the
bucket directly.

The page is built for the DYNAMICS, because that is where the two
consistency claims live: a sequence diagram with time running down (two
writers and a UI, one scenario), the state machine of one writer's tick,
and two small drawings of WHY the log is strong (two writers racing one
conditional write) and WHY a copy is eventual (a tree hears the log only on
its own ticks). The systems in the same class, and the ones that are not,
close the page.

Sources of record: lean/syncer/src/barrier.rs (the tick's idle path, the
pull-only arm, the commit section and step 7), lean/syncer/src/lease.rs (the
per-barrier fence, the FIFO ticket, the head poll, the handoff), sentinel.rs
(the floor tick, remote.seq), lean/syncer/AGENTS.md (the contract every
mount carries), lean/gateway (the UI's write and read), lean/formal/README.md
(the model), and lean/e2e/writers-live/results/2026-09-14-contention/ (the
numbers: six writers, one workspace, 5 s floor, real S3).

Run:  python3 lean-protocol-poster.py [outdir] [--preview] [--pdf] [--emf]
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
LEAN_L, LEAN_F = d.ACCENT["lean"], "#ECF8F2"
UI_F, UI_L = "#FFF6E2", "#B0862A"
LAG_F = "#DCEAF7"
RED_L, RED_F = "#C0392B", "#FDECEC"

GLOSSARY = [
    ("tick", "a writer's own poll for work, every floorSecs: scan the tree, "
             "GET the inbox, GET the pointer — then stop, pull or publish"),
    ("floorSecs", "the tick interval (default 60 s). A publish touch does not "
                  "wait for it: a 1 s local poll runs that barrier at once"),
    ("barrier · boundary", "the work, and what it makes: one barrier installs "
                           "one boundary — every changed file, cited together"),
    ("the pointer", ".flint/lean/current — the ONE mutable metadata object. A "
                    "boundary exists when a CAS on it lands"),

    ("generation · seq", "one immutable manifest, numbered. seq only grows, "
                         "so a reader never goes backwards"),
    ("CAS · If-Match", "a write that lands only if the object is still the "
                       "version it read — S3's conditional PUT and DELETE"),
    ("the fence", ".flint/lean/epoch — whose turn it is to commit. Held for "
                  "the commit section only, then handed to the queue head"),
    ("FIFO ticket", "a waiter enqueues once; the holder's handoff names the "
                    "head, which polls every 200 ms; the rest every second"),

    ("the inbox · window", "UI writes waiting to be cited, and the 'a barrier "
                           "is committing' sign in the same cell"),
    ("foreign queue", "a writer's local list of peers' changes its merge "
                      "carried into the log but not yet into its tree"),
    ("consume", "the first thing a barrier does: apply inbox entries and "
                "queued peer changes onto files the agent has not modified"),
    ("pull-only", "a barrier with nothing of its own to publish: it takes the "
                  "peers' changes and their manifest as its base, no lock"),

    ("remote.seq", ".flint/remote.seq — observed vs integrated seq: whether "
                   "this tree is behind the log, read locally at no cost"),
    ("conflict record", "two writers changed one file: the later boundary "
                        "wins, the other bytes are preserved and named"),
    ("deposal", "a holder whose cell stood still 60 s is taken over, and the "
                "manifest rotated so its late CAS cannot land"),
    ("write skew", "two agents each act on a stale read of the OTHER's file; "
                   "both commits are valid, and nothing flags it"),
]

CLASS = [
    ("git + a hosted remote",
     "LOG: a branch ref, moved only from the commit you pushed onto — a "
     "non-fast-forward push is refused; --force-with-lease is literally a "
     "CAS. COPIES: clones, current as of their last pull."),
    ("Subversion · Perforce",
     "LOG: one server revision counter orders every atomic commit. COPIES: "
     "working copies that svn update / p4 sync; a commit on an out-of-date "
     "file is refused until you update and resolve."),
    ("Dropbox",
     "LOG: the server's journal of every file change. COPIES: each device's "
     "folder, synced when it next talks to the server; two edits at once "
     "leave a “conflicted copy” beside the file — lean's kept loser."),
    ("Kubernetes",
     "LOG: etcd; every update is conditional on resourceVersion and a stale "
     "one gets 409 Conflict. COPIES: each controller's informer cache, kept "
     "near by a watch and a periodic resync — lean's tick."),
    ("Delta Lake · Apache Iceberg",
     "LOG: a table's history, advanced by one conditional write — "
     "put-if-absent of the next log entry, or a CAS of the metadata pointer "
     "— retried on conflict. Lean's commit, almost exactly. COPIES: each "
     "engine's snapshot, refreshed on read."),
    ("Kafka · event sourcing",
     "LOG: an ordered, replicated partition log. COPIES: every consumer's "
     "view — a cache, an index, a read model — catching up at its own "
     "pace from its offset."),
]


def build():
    doc = k.Document(
        title="flint-lean — the protocol",
        creator="flint",
        description="One page: several writers on one workspace — a strongly "
                    "consistent shared log with eventually consistent working "
                    "copies, drawn in time: the sequence, the tick, and why "
                    "each consistency holds.")
    p = doc.page("Protocol", 22.4, 22.0)

    d.header(p, "lean", "The protocol — one shared log, many working copies",
             "Lean is a strongly consistent shared log with eventually "
             "consistent working copies: git with automatic push and pull. "
             "One conditional write on the bucket orders every boundary; each "
             "writer's TICK keeps its tree close to that log, and a UI reads "
             "and writes the bucket directly.")

    # =====================================================================
    # 1 · the structure: working copies on the left, the log on the right
    # =====================================================================
    zy = 2.05
    d.zone(p, 0.5, zy, 9.15, 4.30, "working copies — EVENTUALLY consistent",
           sub="each pod has one tree and one syncer; every writer does "
               "everything drawn for A and B")

    def writer(y, name, agent_body, sync_body):
        d.node(p, "actor", 0.72, y + 0.06, 0.46, 0.72, "", "",
               fill=d.CLIENT_F, line=d.CLIENT_L, line_weight=0.012)
        d.node(p, "rect", 1.38, y, 2.90, 0.86,
               "agent %s  ·  /workspace" % name, agent_body, fill=d.CLIENT_F,
               line=d.CLIENT_L, line_weight=0.013, title_size=9.2,
               body_size=7.1)
        d.node(p, "rect", 5.25, y, 4.15, 0.86,
               "flint-sync %s — a TICK every floorSecs" % name, sync_body,
               fill=d.WORK_F, line=d.WORK_L, line_weight=0.015,
               title_size=9.2, body_size=7.1)
        p.arrow([(4.28, y + 0.43), (5.25, y + 0.43)], color=FLOW, weight=WT,
                dashed=True, begin_arrow=k.ARROW_FILLED)
        d.flabel(p, 4.765, y + 0.19, "same dir", w=0.95, size=7.0)

    writer(2.70, "A", "plain local files; its own edits visible at once",
           "publishes A's changes; pulls the others' into A's tree")
    writer(3.82, "B", "edits other files, or the same one",
           "publishes B's; its tick hears A's commit and applies it")
    d.node(p, "actor", 0.72, 5.00, 0.46, 0.72, "", "", fill=UI_F, line=UI_L,
           line_weight=0.012)
    d.node(p, "rect", 1.38, 4.94, 2.90, 0.86, "a person  ·  a UI",
           "edits while agents run — no pod to call", fill=UI_F, line=UI_L,
           line_weight=0.013, title_size=9.2, body_size=7.1)
    d.node(p, "rect", 5.25, 4.94, 4.15, 0.86,
           "flint-lean-gateway — a door, or a crate",
           "no tick: reads the bucket when asked; a write is durable when it "
           "returns", fill=UI_F, line=UI_L, line_weight=0.015,
           title_size=9.2, body_size=7.1)
    p.arrow([(4.28, 5.37), (5.25, 5.37)], color=ALT, weight=W,
            begin_arrow=k.ARROW_FILLED)
    d.flabel(p, 4.765, 5.13, "HTTP · in-process", ALT, w=1.05, size=6.8)

    lx = 13.05
    d.zone(p, lx, zy, 8.85, 4.30, "the shared log — STRONGLY consistent",
           color=d.S3_L, sub="one bucket prefix; every write below is "
                             "conditional on the version it read")
    gx, gy, gw, gh, gg = 13.30, 2.70, 1.72, 0.72, 0.38
    for i, (t, b) in enumerate([("seq 42", "immutable"), ("seq 43", "A's x"),
                                ("seq 44", "B's y")]):
        hot = i == 2
        d.node(p, "rect", gx + i * (gw + gg), gy, gw, gh, t, b,
               fill=d.S3_HOT_F if hot else d.S3_F, line=d.S3_L,
               line_weight=0.015 if hot else 0.011, title_size=9.0,
               body_size=6.9)
        if i:
            p.arrow([(gx + i * (gw + gg) - gg, gy + gh / 2),
                     (gx + i * (gw + gg), gy + gh / 2)], color=d.S3_L, weight=W)
    d.node(p, "cylinder", 19.55, 2.60, 2.10, 0.92, ".flint/lean/current",
           "THE pointer — one CAS per boundary", fill=d.S3_HOT_F, line=d.S3_L,
           line_weight=0.016, cap=0.18, title_size=8.6, body_size=6.9)
    p.arrow([(19.55, 3.06), (gx + 2 * (gw + gg) + gw, 3.06)], color=DUR,
            weight=W)
    d.node(p, "cylinder", 13.30, 3.72, 4.05, 0.80, "files/<path>",
           "whole objects; PUT and GC delete If-Match", fill=d.S3_F,
           line=d.S3_L, line_weight=0.013, cap=0.18, title_size=9.0,
           body_size=6.9)
    d.node(p, "cylinder", 17.60, 3.72, 4.05, 0.80, ".flint/lean/inbox",
           "UI writes to cite · the barrier window", fill=d.S3_F, line=d.S3_L,
           line_weight=0.013, cap=0.18, title_size=9.0, body_size=6.9)
    d.node(p, "cylinder", 13.30, 4.74, 8.35, 0.90,
           ".flint/lean/epoch — THE FENCE: whose turn it is to commit",
           "one holder, a FIFO of waiters, a handoff to the head — for the "
           "commit section only. Uploads and pulls take no lock",
           fill=d.S3_F, line=d.S3_L, line_weight=0.013, cap=0.20,
           title_size=9.0, body_size=6.9)
    d.caption(p, 13.30, 5.78, 8.35,
              "a reader — a fresh checkout, the gateway — sees a whole "
              "boundary or the one before it: never a mix, never going back",
              size=7.0, halign=0)

    p.arrow([(9.40, 3.13), (13.05, 3.13)], color=DUR, weight=WT)
    d.flabel(p, 11.22, 2.89, "PUBLISH", DUR, w=1.2, size=7.6)
    d.flabel(p, 11.22, 3.39, "upload → claim → merge → CAS", DUR, w=3.3,
             size=6.9)
    p.arrow([(13.05, 4.25), (9.40, 4.25)], color=FLOW, weight=WT)
    d.flabel(p, 11.22, 4.01, "PULL, on a tick", FLOW, w=2.0, size=7.6)
    d.flabel(p, 11.22, 4.51, "GET pointer · GET inbox — no lock", FLOW,
             w=3.3, size=6.9)
    p.arrow([(9.40, 5.37), (13.05, 5.37)], color=ALT, weight=W,
            begin_arrow=k.ARROW_FILLED)
    d.flabel(p, 11.22, 5.13, "object, then inbox entry", ALT, w=2.6, size=7.2)
    d.flabel(p, 11.22, 5.63, "reads: citation + inbox overlay", ALT, w=3.1,
             size=6.9)

    # =====================================================================
    # 2 · the dynamics: one scenario in time, and one writer's state machine
    # =====================================================================
    sy = 6.70
    SW = 14.60
    SH = 8.00
    p.box(0.5, sy, SW, SH, "", "", fill="#FBFCFD", line=d.ZONE_L,
          dashed=True, rounding=0.14, line_weight=0.010)
    p.text(0.72, sy + 0.12, SW - 0.4, "IN TIME — two writers and a UI, one "
           "scenario (time runs down)", size=10, color=INK, bold=True)
    p.text(0.72, sy + 0.38, SW - 0.4, "Solid bars: who holds the fence. "
           "Shaded bands: a tree behind the log — that band IS eventual "
           "consistency, and a tick ends it.", size=7.6, color=SUB)

    lane_y = sy + 0.72
    LANES = [
        ("agent A", 1.30, 1.25, d.CLIENT_F, d.CLIENT_L),
        ("flint-sync A", 3.30, 1.55, d.WORK_F, d.WORK_L),
        ("the log · the fence", 7.10, 2.30, d.S3_F, d.S3_L),
        ("flint-sync B", 10.60, 1.55, d.WORK_F, d.WORK_L),
        ("agent B", 12.35, 1.15, d.CLIENT_F, d.CLIENT_L),
        ("UI", 14.30, 0.80, UI_F, UI_L),
    ]
    X = {}
    for name, cx, w, fill, line in LANES:
        d.node(p, "rect", cx - w / 2, lane_y, w, 0.40, name, "", fill=fill,
               line=line, line_weight=0.012, title_size=8.6)
        X[name] = cx
    top, bottom = lane_y + 0.40, sy + SH - 0.15
    for name, cx, *_ in LANES:
        p.arrow([(cx, top), (cx, bottom)], color="#B9C0C8", weight=0.008,
                dashed=True, end_arrow=0)

    row0, step = lane_y + 0.78, 0.46

    def ry(i):
        return row0 + i * step

    def msg(i, a, b, text, color, weight=W, dashed=False, dy=-0.20, w=None,
            lx=None, size=6.9):
        y = ry(i)
        xa, xb = X[a], X[b]
        p.arrow([(xa, y), (xb, y)], color=color, weight=weight, dashed=dashed)
        if not text:
            return
        cx = lx if lx is not None else (xa + xb) / 2
        span = w if w is not None else abs(xb - xa) - 0.25
        d.flabel(p, cx, y + dy, text, color, w=span, size=size)

    def tick(i, lane, side):
        # beside the lifeline, on the side its arrow does NOT leave from
        x0 = X[lane] + 0.10 if side > 0 else X[lane] - 0.60
        p.box(x0, ry(i) - 0.12, 0.50, 0.24, "tick", "",
              fill=d.WORK_F, line=d.WORK_L, line_weight=0.010,
              title_color=d.WORK_L, title_size=6.6, halign=1, valign=1,
              rounding=0.08, ok_overlap=True)

    # the fence bars on the log's lifeline
    p.box(X["the log · the fence"] - 0.20, ry(2) - 0.06, 0.13,
          ry(5) - ry(2) + 0.12, "", "", fill=d.WORK_L, line=d.WORK_L,
          rounding=0.02, ok_overlap=True)
    p.box(X["the log · the fence"] + 0.07, ry(5) - 0.06, 0.13,
          ry(8) - ry(5) + 0.12, "", "", fill=d.CLIENT_L, line=d.CLIENT_L,
          rounding=0.02, ok_overlap=True)
    # the lag bands on the agents' lifelines
    p.box(X["agent B"] - 0.16, ry(4), 0.32, ry(11) + 0.06 - ry(4), "", "",
          fill=LAG_F, line=d.CLIENT_L, line_weight=0.006, rounding=0.03,
          ok_overlap=True)
    p.box(X["agent A"] - 0.16, ry(7), 0.32, ry(13) - ry(7), "", "",
          fill=LAG_F, line=d.CLIENT_L, line_weight=0.006, rounding=0.03,
          ok_overlap=True)

    msg(0, "agent A", "flint-sync A", "edits x", FLOW, dashed=True)
    msg(0, "agent B", "flint-sync B", "edits y", FLOW, dashed=True)
    tick(1, "flint-sync A", -1)
    tick(1, "flint-sync B", +1)
    msg(1, "flint-sync A", "the log · the fence", "PUT files/x If-Match — no lock",
        DUR)
    msg(1, "flint-sync B", "the log · the fence", "PUT files/y If-Match — no lock",
        DUR)
    msg(2, "flint-sync A", "the log · the fence", "claim the fence: held", CTL)
    msg(3, "flint-sync B", "the log · the fence", "claim: queued behind A", CTL)
    msg(4, "flint-sync A", "the log · the fence",
        "CAS If-Match 42 → seq 43  ★ commit", DUR, weight=WT)
    msg(5, "flint-sync A", "the log · the fence", "hand off to the head: B", CTL)
    msg(6, "flint-sync A", "agent A", "ack ok · 43", FLOW, dashed=True)
    msg(6, "flint-sync B", "the log · the fence",
        "read 43 · merge y onto it", DUR)
    msg(7, "flint-sync B", "the log · the fence",
        "CAS If-Match 43 → seq 44  ★", DUR, weight=WT)
    msg(8, "flint-sync B", "the log · the fence", "hand off · queue A's x", CTL)
    msg(8, "flint-sync B", "agent B", "ack ok · 44", FLOW, dashed=True)
    msg(9, "UI", "the log · the fence", "", ALT)
    d.flabel(p, 8.85, ry(9) - 0.20, "UI: PUT files/z, then inbox — durable now",
             ALT, w=2.95, size=6.9)
    tick(10, "flint-sync B", +1)
    msg(10, "the log · the fence", "flint-sync B", "tick: inbox has z", FLOW)
    msg(11, "flint-sync B", "agent B", "apply x, z", FLOW, dy=-0.20)
    d.caption(p, X["agent B"] + 0.22, ry(11) + 0.10, 1.60,
              "B in sync", size=7.0, color=d.CLIENT_L, halign=0)
    tick(12, "flint-sync A", -1)
    msg(12, "the log · the fence", "flint-sync A",
        "tick: pointer moved → pull, no lock", FLOW)
    msg(13, "flint-sync A", "agent A", "apply y, z", FLOW)
    d.caption(p, 0.52, ry(13) + 0.10, 0.62, "A in sync", size=7.0,
              color=d.CLIENT_L, halign=0)
    d.caption(p, 0.55, ry(0) + 0.10, 0.60, "time ↓", size=7.4,
              color=MUTE, halign=0)
    d.caption(p, X["agent B"] + 0.22, ry(5) - 0.10, 1.55,
              "B behind: no x", size=7.0, color=d.CLIENT_L, halign=0)
    d.caption(p, 0.55, ry(8) - 0.10, 0.62, "A behind: no y", size=7.0,
              color=d.CLIENT_L, halign=0)

    # ---- the state machine of one writer ------------------------------------
    mx = 15.35
    p.box(mx, sy, 21.9 - mx, SH, "", "", fill="#FBFCFD", line=d.WORK_L,
          dashed=True, rounding=0.14, line_weight=0.010)
    p.text(mx + 0.22, sy + 0.12, 6.1, "ONE WRITER — the tick, as states",
           size=10, color=INK, bold=True)
    p.text(mx + 0.22, sy + 0.38, 6.1, "every writer runs this loop; only the "
           "purple states touch the fence", size=7.6, color=SUB)

    def state(x, y, w, title, body, fill, line):
        return d.node(p, "rect", x, y, w, 0.62, title, body, fill=fill,
                      line=line, line_weight=0.013, title_size=8.8,
                      body_size=6.9, rounding=0.12)

    SX = mx + 2.05
    state(SX, sy + 0.78, 2.60, "IDLE", "until floorSecs, or a publish touch",
          d.PLAIN_F, d.PLAIN_L)
    state(SX, sy + 1.72, 2.60, "TICK", "scan · GET inbox · GET pointer",
          d.CLIENT_F, d.CLIENT_L)
    state(mx + 0.25, sy + 2.86, 2.25, "PULL",
          "queue peers' changes · no lock", d.CLIENT_F, d.CLIENT_L)
    state(mx + 3.55, sy + 2.86, 2.70, "CONSUME · UPLOAD",
          "apply inbox + queue · PUT If-Match", d.CLIENT_F, d.CLIENT_L)
    state(mx + 3.55, sy + 3.86, 2.70, "WAIT FOR THE FENCE",
          "enqueue · the head polls 200 ms", "#F3EEFB", d.WORK_L)
    state(mx + 3.55, sy + 4.86, 2.70, "COMMIT",
          "re-read · merge · CAS the pointer", "#F3EEFB", d.WORK_L)
    state(mx + 3.55, sy + 5.86, 2.70, "GC · QUEUE · HAND OFF",
          "delete If-Match · ack ok", "#F3EEFB", d.WORK_L)

    cxs = SX + 1.30
    p.arrow([(cxs, sy + 1.40), (cxs, sy + 1.72)], color=FLOW, weight=W)
    # nothing moved: back to IDLE
    p.arrow([(SX + 2.60, sy + 2.03), (SX + 3.00, sy + 2.03),
             (SX + 3.00, sy + 1.09), (SX + 2.60, sy + 1.09)], color=FLOW,
            weight=W)
    d.flabel(p, SX + 3.72, sy + 1.56, "nothing moved: 2 GETs", FLOW, w=1.35,
             size=6.6)
    # pointer moved, nothing local: PULL
    p.arrow([(SX + 0.40, sy + 2.34), (SX + 0.40, sy + 2.60),
             (mx + 1.375, sy + 2.60), (mx + 1.375, sy + 2.86)], color=FLOW,
            weight=W)
    d.flabel(p, mx + 1.10, sy + 2.46, "log moved", FLOW, w=1.1, size=6.6)
    p.arrow([(mx + 0.25, sy + 3.17), (mx + 0.12, sy + 3.17),
             (mx + 0.12, sy + 1.09), (SX, sy + 1.09)], color=FLOW, weight=W)
    # local work: PUBLISH
    p.arrow([(SX + 2.20, sy + 2.34), (SX + 2.20, sy + 2.60),
             (mx + 4.90, sy + 2.60), (mx + 4.90, sy + 2.86)], color=DUR,
            weight=W)
    d.flabel(p, mx + 5.55, sy + 2.46, "local work", DUR, w=1.0, size=6.6)
    for yy in (3.48, 4.48, 5.48):
        p.arrow([(mx + 4.90, sy + yy), (mx + 4.90, sy + yy + 0.38)],
                color=DUR, weight=W)
    # a CAS on a stale read: re-merge
    p.arrow([(mx + 3.55, sy + 5.30), (mx + 3.25, sy + 5.30),
             (mx + 3.25, sy + 4.98), (mx + 3.55, sy + 4.98)], color=RED_L,
            weight=W)
    d.flabel(p, mx + 2.45, sy + 5.14, "412: re-merge", RED_L, w=1.25,
             size=6.6)
    # hand off: back to IDLE
    p.arrow([(mx + 6.25, sy + 6.17), (mx + 6.42, sy + 6.17),
             (mx + 6.42, sy + 0.86), (SX + 2.60, sy + 0.86)], color=DUR,
            weight=W)
    d.caption(p, mx + 0.25, sy + 3.70, 3.10,
              "The fence is held only from WAIT to HAND OFF — 0.4 s at the "
              "median under six writers. A holder still for 60 s is deposed "
              "and the manifest rotated.", size=7.0, halign=0)
    d.caption(p, mx + 0.25, sy + 6.72, 6.10,
              "measured, six writers, 5 s floor, real S3: claim wait p50 0, "
              "publish acked p50 1.9 s / p90 2.9 s, idle 2 GETs a tick",
              size=7.0, halign=0)

    # =====================================================================
    # 3 · WHY: the log is strong, the copies are eventual
    # =====================================================================
    wy = sy + SH + 0.30
    WW = 10.55
    p.box(0.5, wy, WW, 2.66, "", "", fill=LEAN_F, line=LEAN_L,
          rounding=0.10, line_weight=0.012)
    p.text(0.72, wy + 0.12, WW - 0.4, "WHY THE LOG IS STRONG — one "
           "conditional write decides the order", size=10, color=LEAN_L,
           bold=True)
    d.node(p, "rect", 0.80, wy + 1.00, 1.45, 0.62, "seq 42", "both read this",
           fill=d.S3_F, line=d.S3_L, line_weight=0.012, title_size=9.0,
           body_size=6.8)
    d.node(p, "rect", 4.45, wy + 0.55, 1.45, 0.62, "seq 43", "A's CAS lands",
           fill=d.S3_HOT_F, line=d.S3_L, line_weight=0.014, title_size=9.0,
           body_size=6.8)
    d.node(p, "rect", 4.45, wy + 1.48, 1.45, 0.62, "412", "B's CAS refused",
           fill=RED_F, line=RED_L, line_weight=0.013, title_size=9.0,
           body_size=6.8, title_color=RED_L)
    d.node(p, "rect", 8.35, wy + 1.00, 1.45, 0.62, "seq 44",
           "B, merged onto 43", fill=d.S3_HOT_F, line=d.S3_L,
           line_weight=0.014, title_size=9.0, body_size=6.8)
    p.arrow([(2.25, wy + 1.20), (4.45, wy + 0.86)], color=DUR, weight=WT)
    d.flabel(p, 3.30, wy + 0.78, "A: CAS If-Match 42", DUR, w=1.9, size=6.9)
    p.arrow([(2.25, wy + 1.42), (4.45, wy + 1.79)], color=RED_L, weight=W)
    d.flabel(p, 3.30, wy + 1.86, "B: CAS If-Match 42", RED_L, w=1.9, size=6.9)
    p.arrow([(5.90, wy + 1.79), (8.35, wy + 1.42)], color=DUR, weight=W)
    d.flabel(p, 7.10, wy + 2.00, "re-read 43, merge, If-Match 43", DUR,
             w=2.4, size=6.9)
    p.arrow([(5.90, wy + 0.86), (8.35, wy + 1.20)], color=d.S3_L, weight=W,
            dashed=True)
    d.flabel(p, 7.10, wy + 0.62, "43 is B's new base", d.S3_L, w=2.0,
             size=6.9)
    d.caption(p, 0.72, wy + 2.24, WW - 0.4,
              "Every boundary is a conditional PUT of ONE object, and S3 "
              "answers conditional writes with strong consistency: two writers "
              "can never both extend 42, and nothing extends a version it did "
              "not read. seq is a total order every reader sees. The fence "
              "only takes turns, so writers do not burn retries on each other.",
              size=7.3, color=SUB, halign=0)

    ex = 0.5 + WW + 0.30
    EW = 21.9 - ex
    p.box(ex, wy, EW, 2.66, "", "", fill=d.CLIENT_F, line=d.CLIENT_L,
          rounding=0.10, line_weight=0.012)
    p.text(ex + 0.22, wy + 0.12, EW - 0.4, "WHY A COPY IS EVENTUAL — a tree "
           "hears the log only on its own ticks", size=10, color=d.CLIENT_L,
           bold=True)
    # the log on top, B's tree below, the tick bridging them
    ty1, ty2 = wy + 0.62, wy + 1.52
    d.node(p, "rect", ex + 0.30, ty1, 1.55, 0.52, "log: seq 43", "x = A's",
           fill=d.S3_HOT_F, line=d.S3_L, line_weight=0.013, title_size=8.6,
           body_size=6.8)
    d.node(p, "rect", ex + 0.30, ty2, 1.55, 0.52, "B's tree", "x = old",
           fill=LAG_F, line=d.CLIENT_L, line_weight=0.013, title_size=8.6,
           body_size=6.8)
    d.node(p, "rect", ex + 4.05, ty2, 1.55, 0.52, "B's tree", "x = A's",
           fill=d.CLIENT_F, line=d.CLIENT_L, line_weight=0.013,
           title_size=8.6, body_size=6.8)
    d.node(p, "rect", ex + 7.70, ty2, 2.70, 0.52, "B modified x?",
           "never overwritten: conflict record", fill=RED_F, line=RED_L,
           line_weight=0.013, title_size=8.6, body_size=6.8,
           title_color=RED_L)
    p.arrow([(ex + 1.85, ty2 + 0.26), (ex + 4.05, ty2 + 0.26)], color=FLOW,
            weight=WT)
    d.flabel(p, ex + 2.95, ty2 + 0.02, "B's next tick", FLOW, w=1.6, size=6.9)
    p.arrow([(ex + 1.075, ty1 + 0.52), (ex + 1.075, ty2)], color="#B9C0C8",
            weight=W, dashed=True)
    d.flabel(p, ex + 2.20, ty1 + 0.58, "the gap: 1-2 ticks", MUTE, w=1.5,
             size=6.9)
    p.arrow([(ex + 5.60, ty2 + 0.26), (ex + 7.70, ty2 + 0.26)], color=RED_L,
            weight=W, dashed=True)
    d.flabel(p, ex + 6.65, ty2 + 0.02, "unless", RED_L, w=0.8, size=6.9)
    d.caption(p, ex + 0.22, wy + 2.24, EW - 0.4,
              "A tree is local disk: an agent reads its own writes at once, and "
              "a peer's change only when a tick sees the pointer move and the "
              "next applies it — or at once, on .flint/sync. Nothing foreign "
              "lands on a file the agent modified. So trees converge on every "
              "file nobody is editing, and remote.seq says when one is behind.",
              size=7.3, color=SUB, halign=0)

    # =====================================================================
    # 4 · what it is not, and the systems in the same class
    # =====================================================================
    ny = wy + 2.96
    nb = p.fitbox(0.5, ny, WW, "NOT a transaction across files",
          "The fence orders COMMITS, not an agent's read-think-write. Disjoint "
          "files merge with no lost update; one file edited by two agents is "
          "the later boundary's, the earlier kept. Write skew is possible: A "
          "reads x and writes y while B reads y and writes x — both commits "
          "are valid and nothing flags it.", pad=0.20,
          fill=RED_F, line=RED_L, line_weight=0.012, title_size=9.6,
          body_size=7.3, title_color=RED_L, body_color=SUB)
    gb = p.fitbox(ex, ny, EW, "git, mapped",
          "the branch → the manifest log · commit + push → a publish "
          "barrier, whose CAS re-merges where git would refuse a "
          "non-fast-forward · pull → the tick, or .flint/sync · working "
          "copy → /workspace · merge conflict → the later boundary "
          "wins, the loser is kept, nothing blocks · push lock → the "
          "fence, for the commit only", pad=0.20,
          fill="#F3EEFB", line=d.WORK_L, line_weight=0.012, title_size=9.6,
          body_size=7.3, title_color=d.WORK_L, body_color=SUB)
    nb.h = gb.h = max(nb.h, gb.h)

    cy = ny + nb.h + 0.30
    p.text(0.55, cy, 21.3, "THE SAME CLASS — a strongly consistent log, "
           "eventually consistent copies", size=10, color=INK, bold=True)
    cy += 0.32
    cw, cg = (21.4 - 5 * 0.18) / 6.0, 0.18
    boxes = []
    for i, (t, b) in enumerate(CLASS):
        boxes.append(p.fitbox(0.5 + i * (cw + cg), cy, cw, t, b, pad=0.18,
                              title_size=9.0, body_size=7.0, title_color=INK,
                              body_color=SUB, fill=d.PLAIN_F, line=d.PLAIN_L,
                              line_weight=0.010, rounding=0.06))
    tall = max(b.h for b in boxes)
    for b in boxes:
        b.h = tall
    cy += tall + 0.14
    s = p.text(0.55, cy, 21.3,
               "NOT this class: S3 alone and a passthrough mount are strong "
               "per object with no log across objects · NFS and flint-lite "
               "keep one live copy, with nothing to converge · CRDT and "
               "peer-to-peer sync (Syncthing, Automerge) keep no single log — "
               "replicas converge by merging each other.",
               size=7.8, color=SUB)
    y = cy + s.h + 0.26

    y = d.notes(p, 0.55, y, 21.3, [
        "A TICK IS A POLL FOR WORK, NOT A HEARTBEAT. Idle, it is two GETs and "
        "writes nothing; there is no liveness object in the bucket, so an "
        "idle writer and a dead one look the same from outside. The one "
        "liveness signal is the fence's token while a writer holds it: claim "
        "and handoff move it, and a holder that stands still for 60 s is "
        "deposed. A publish touch does not wait for the tick.",

        "WHAT DOES NOT CONVERGE ON ITS OWN. A writer lost for good between "
        "its upload and its commit leaves an uncited object that the trees "
        "and a fresh checkout disagree about until someone rewrites the "
        "path; nothing acknowledged is lost. On Ozone 2.2.x a DELETE ignores "
        "If-Match, so the garbage collector's guard is void there: run one "
        "writer per workspace until 2.3.0.",
    ])

    y += 0.22
    d.legend(p, 0.55, y, [
        ("a publish: bytes and the commit, into the log", DUR, False),
        ("a pull: the log, into a working copy", FLOW, False),
        ("a UI: straight to the bucket, no pod", ALT, False),
        ("one directory, two views · an ack back to the agent", FLOW, True),
    ], span=5.35)
    d.glossary(p, 0.55, y + 0.45, 21.3, GLOSSARY)
    return doc, p


def main():
    args = [a for a in sys.argv[1:] if not a.startswith("--")]
    outdir = args[0] if args else _here
    doc, page = build()
    return d.emit(doc, page, outdir, "flint-lean-protocol", sys.argv)


if __name__ == "__main__":
    sys.exit(main())
