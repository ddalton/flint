#!/usr/bin/env python3
"""Build `flint-lean-dataflow.vsdx` — ONE page, the data flow, symbols.

The lean companion to `forge/forge-dataflow.py`. Lean's whole claim is
that nothing sits between the pod and its files: the app writes plain
local disk, and a worker beside it — holding the credential the app must
not — reconciles that same directory with the bucket at a boundary the
agent declares.

The second thing the page has to say is HOW the tree gets there, because
it is not what a reader expects: the mount is delivered by the CSI node
DaemonSet, in the two moments kubelet gives it, and there is no webhook
anywhere in the picture.

Run:  python3 lean-dataflow.py [outdir] [--preview] [--pdf] [--emf]
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

GLOSSARY = [
    ("CSI ephemeral inline", "a volume declared in the pod spec itself — no "
                             "PVC, no PV, nothing to bind beforehand"),
    ("NodePublishVolume", "the CSI call kubelet BLOCKS every container on. "
                          "The checkout happens inside it"),
    ("NodeUnpublishVolume", "the call kubelet makes only after every "
                            "container has exited — the final barrier"),
    ("bind mount · hostPath", "two names for ONE directory: the plugin's "
                              "tree, seen from the pod and from the worker"),

    ("CAS", "compare-and-swap — a write that lands only if the object is "
            "still the version you read"),
    ("ETag / If-Match", "S3's precondition: the ETag you read, offered back "
                        "as the terms of the write"),
    ("the pointer", ".flint/lean/current — the ONE mutable metadata object. "
                    "Entries live in immutable manifests"),
    ("fence", "a lost CAS. Another syncer holds this workspace, so this one "
              "stops publishing and says so"),

    ("epoch · lease", "the bucket-side single-writer cell. It lives in the "
                      "BUCKET, so it fences a writer in ANY cluster"),
    ("claim", "the project identity stamped on the prefix: adopt your own, "
              "refuse a foreign one"),
    ("boundary verb", "a file the agent writes to declare a coherent point, "
                      "and an ack file it reads back"),
    ("gated mode", "upload everything first — durable and invisible — then "
                   "install the whole set with one CAS"),

    ("loopback door", "AWS_CONTAINER_CREDENTIALS_FULL_URI: how short-lived "
                      "keys reach the worker, and only the worker"),
    ("TokenReview", "the Kubernetes API that says whether a ServiceAccount "
                    "token is still valid — online, not offline"),
    ("FlintLeanWorkspace", "the custom resource one lean project is declared "
                           "as, in the tenant's own namespace"),
    ("RPO", "recovery point. For lean it is the last BARRIER, never the last "
            "write"),

    ("HITL", "human in the loop: a write that reaches the workspace from a "
             "UI rather than from the agent"),
    ("the inbox", "the queue of UI writes waiting to be adopted. Entries "
                  "name an object and its ETag, never a manifest edit"),
    ("the window", "the barrier-in-progress token, in the SAME cell as the "
                   "inbox — carrying a deadline, so it cannot wedge"),
    ("epoch-validated", "checked PER REQUEST against the cell's current "
                        "epoch, so a deposed worker's write is refused"),
]

STEPS = [
    ("parse · resolve",
     "the FlintLeanWorkspace named in volumeAttributes, in the TOKEN's "
     "namespace — never a request field"),
    ("authorise the SA",
     "spec.consumers must list the pod's ServiceAccount. Absent means DENY"),
    ("CREATE the worker",
     "the plugin creates it: flint-sync, one pod per volume, system "
     "namespace, owned by the Node"),
    ("credential",
     "the broker's short-lived keys, on a loopback door the app container "
     "cannot reach"),
    ("CHECKOUT",
     "the tree is materialised from the bucket HERE — before the agent's "
     "first line runs"),
    ("bind into the pod",
     "one Bidirectional view of the same directory. Only now does kubelet "
     "start the containers"),
]


def build():
    doc = k.Document(
        title="flint-lean — data flow",
        creator="flint",
        description="One page: the components that matter, the data flow "
                    "between them, and a glossary.")
    p = doc.page("Data flow", 22.4, 14.0)

    d.header(p, "lean", "Data flow",
             "Nothing sits between the pod and its files. A worker beside it "
             "— holding the credential the pod must not — reconciles the "
             "same directory with the bucket at a boundary the agent "
             "declares.")

    spine = 3.375

    # ---- boundaries -----------------------------------------------------
    d.zone(p, 0.5, 2.20, 5.15, 3.15, "tenant namespace",
           sub="PodSecurity restricted — and it stays that way")
    d.container(p, 0.72, 2.76, 4.70, 2.30,
                "tenant pod — the image is unchanged")
    d.node(p, "cloud", 15.40, 2.05, 6.50, 8.55, "", "", fill=d.CLOUD_F,
           line=d.CLOUD_L, line_weight=0.012)
    p.text(15.60, 2.45, 6.10, "S3-compatible object storage", size=10.5,
           color="#8A6A1E", bold=True, halign=1)

    # ---- the pod, and the only interface it has --------------------------
    d.node(p, "rect", 0.92, 2.90, 4.30, 0.95, "/workspace",
           "plain local files, on real local disk. No FUSE, no wire, no "
           "credential — git, sqlite, hard links and index locks just work",
           fill=d.CLIENT_F, line=d.CLIENT_L, line_weight=0.014,
           title_size=10.5, body_size=7.3)
    d.node(p, "document", 0.92, 4.05, 4.30, 0.80,
           ".flint/publish   →   .flint/publish.ack",
           "ok  ·  partial  ·  refused-fenced",
           fill="#FFF6E2", line="#B0862A", line_weight=0.013,
           title_size=8.8, body_size=7.4)

    # ---- the one directory, and the two views of it ----------------------
    d.node(p, "datastore", 6.35, 2.80, 2.75, 1.15, "the plugin-owned tree",
           "volumes/<vid>/tree\nONE tree, TWO views",
           fill=d.CACHE_F, line=d.CACHE_L, line_weight=0.013,
           title_size=9.6, body_size=7.4)

    d.node(p, "rect", 12.30, 2.40, 2.15, 4.05, "flint-sync",
           "the ONLY writer of the prefix\n\nunprivileged, in a system "
           "namespace: non-root, all capabilities dropped, no ServiceAccount "
           "token — and it holds the S3 credential the agent must not",
           fill=d.WORK_F, line=d.WORK_L, line_weight=0.017, title_size=11.5,
           body_size=7.4)

    # ---- the stores ------------------------------------------------------
    d.node(p, "cylinder", 16.70, 2.95, 4.15, 0.85, "files",
           "<prefix>/files/<path> — whole objects: checked out at start, and a touched file is re-uploaded",
           fill=d.S3_F, line=d.S3_L, line_weight=0.013, cap=0.28,
           body_size=7.4)
    d.node(p, "cylinder", 16.70, 3.95, 4.15, 0.85, ".flint/lean/current",
           "THE pointer. ONE CAS installs a whole boundary — entries in manifests/ + chunks/",
           fill=d.S3_HOT_F, line=d.S3_L, line_weight=0.015, cap=0.28,
           body_size=7.4)
    d.node(p, "cylinder", 16.70, 4.95, 4.15, 0.85, ".flint/lean/epoch",
           "the lease, and the claim beside it — claimed, renewed, and fenced on a lost CAS",
           fill=d.S3_F, line=d.S3_L, line_weight=0.013, cap=0.28,
           body_size=7.4)
    d.node(p, "cylinder", 16.70, 5.95, 4.15, 0.85, ".flint/lean/inbox",
           "ONE CAS cell that is BOTH the HITL inbox and the barrier-window "
           "token — opened at barrier intent, cleared after the manifest CAS",
           fill=d.S3_HOT_F, line=d.S3_L, line_weight=0.015, cap=0.28,
           body_size=7.4)
    p.text(17.90, 7.20, 2.95, "\u2026workspace #1's prefix, above",
           size=7.4, color=MUTE, halign=2)
    d.node(p, "cylinder", 16.70, 8.82, 4.15, 0.85, "workspace #2's prefix",
           "its own files/, current, epoch, claim and inbox — DISJOINT. One "
           "writer per prefix, and these two never meet",
           fill=d.S3_F, line=d.S3_L, line_weight=0.013, cap=0.28,
           body_size=7.4)

    # ---- the flows -------------------------------------------------------
    # 1 · the workspace: one directory, two views. NOT a wire
    p.arrow([(5.22, spine), (6.35, spine)], color=FLOW, weight=WT,
            dashed=True, begin_arrow=k.ARROW_FILLED)
    d.flabel(p, 6.00, spine - 0.24, "bind mount", w=1.3, size=7.4)
    p.arrow([(9.10, spine), (12.30, spine)], color=FLOW, weight=WT,
            dashed=True, begin_arrow=k.ARROW_FILLED)
    d.flabel(p, 10.55, spine - 0.24,
             "hostPath — the SAME directory, never a wire", w=3.2, size=7.4)
    d.flabel(p, 10.50, spine + 0.22,
             "the pod is never blocked on S3, and never sees a credential",
             MUTE, w=3.4, size=7.2)

    # 2 · the boundary verbs — the agent's whole API, and it is files
    p.arrow([(3.07, 3.85), (3.07, 4.05)], color=ALT, weight=W,
            begin_arrow=k.ARROW_FILLED)

    # 3 · the durable path — the worker, and nothing else, writes the prefix
    p.arrow([(14.45, spine), (16.70, spine)], color=DUR, weight=WT,
            begin_arrow=k.ARROW_FILLED)
    d.flabel(p, 15.02, spine - 0.24, "upload · checkout", DUR, w=1.30,
             size=7.4)
    p.arrow([(14.45, 4.375), (16.70, 4.375)], color=DUR, weight=WT,
            begin_arrow=k.ARROW_FILLED)
    d.flabel(p, 15.02, 4.135, "ONE CAS", DUR, w=1.00, size=7.4)
    p.arrow([(14.45, 5.375), (16.70, 5.375)], color=DUR, weight=W,
            begin_arrow=k.ARROW_FILLED)
    d.flabel(p, 15.02, 5.135, "claim · renew", DUR, w=1.20, size=7.4)
    p.arrow([(14.45, 6.375), (16.70, 6.375)], color=DUR, weight=WT,
            begin_arrow=k.ARROW_FILLED)
    d.flabel(p, 15.02, 6.135, "consume", DUR, w=1.10, size=7.4)
    p.arrow([(11.05, 7.55), (11.60, 7.55)], color=ALT, weight=WT)
    p.arrow([(15.27, 7.55), (17.50, 7.55), (17.50, 6.80)], color=ALT,
            weight=WT)
    p.arrow([(17.50, 7.55), (17.50, 9.02)], color=ALT, weight=WT)

    # ---- how the tree gets there: CSI, and no webhook ---------------------
    p.box(0.5, 5.60, 9.30, 2.60, "", "", fill="#FBFCFD", line=d.ZONE_L,
          dashed=True, rounding=0.14, line_weight=0.009)
    p.text(0.72, 5.72, 8.9,
           "s3.csi.chert.us  —  the node DaemonSet: the mount is delivered "
           "by CSI, and there is NO webhook", size=10, color=INK, bold=True)
    p.text(0.72, 5.96, 8.9,
           "One privileged process per node. kubelet calls "
           "NodePublishVolume with a pod-bound ServiceAccount token and "
           "BLOCKS every container in the pod until it returns — so the "
           "checkout finishes before the agent's first line. "
           "NodeUnpublishVolume runs only after every container has exited: "
           "that is where the final barrier goes.", size=7.8, color=SUB)
    d.steps(p, 0.72, 6.62, 8.90, STEPS[:3], color=CTL)
    d.steps(p, 0.72, 7.42, 8.90, STEPS[3:], color=CTL, start=4)

    p.arrow([(1.05, 5.60), (1.05, 5.06)], color=CTL, weight=W, dashed=True)
    d.flabel(p, 1.95, 5.45, "bind into the pod", CTL, w=1.5, size=7.2)
    p.arrow([(7.70, 5.60), (7.70, 3.95)], color=CTL, weight=W, dashed=True)
    d.flabel(p, 8.87, 4.80, "the plugin OWNS this tree", CTL, w=2.0,
             size=7.2)

    d.node(p, "document", 10.10, 7.05, 0.95, 0.95, "", "", fill=d.CLIENT_F,
           line=d.CLIENT_L, line_weight=0.013)
    p.text(9.55, 8.08, 2.05, "browser / UI", size=9.6, color=INK, bold=True,
           halign=1)
    d.node(p, "rect", 11.60, 6.70, 3.65, 1.75,
           "flint-lean-gateway — ONE, for every workspace",
           "one Deployment, two replicas, N workspaces: /lean/v1/{workspace} "
           "against a map of id=prefix pairs, and an unknown id is a 404 — "
           "never a guessed prefix.\n"
           "It talks to the BUCKET, never to the pod: no CR to resolve, no "
           "hub, no proxy. GET /snapshot · /files · /status — and the HITL "
           "write: PUT the object FIRST, append the inbox entry SECOND, and "
           "NEVER edit the manifest.",
           fill="#FFF6E2", line="#B0862A", line_weight=0.016,
           title_size=9.8, body_size=7.2)

    d.zone(p, 0.5, 8.60, 14.90, 1.05,
           "workspace #2 — another FlintLeanWorkspace, another prefix, "
           "served by the SAME plugin, broker and gateway")
    d.node(p, "rect", 0.72, 8.95, 3.50, 0.58, "tenant pod #2  ·  /workspace",
           "", fill=d.CLIENT_F, line=d.CLIENT_L, line_weight=0.012,
           title_size=9.2)
    d.node(p, "datastore", 4.42, 8.95, 3.00, 0.58, "its own tree", "",
           fill=d.CACHE_F, line=d.CACHE_L, line_weight=0.012, title_size=9.2)
    d.node(p, "rect", 7.62, 8.95, 2.60, 0.58, "its own flint-sync", "",
           fill=d.WORK_F, line=d.WORK_L, line_weight=0.013, title_size=9.2)
    p.arrow([(4.22, 9.24), (4.42, 9.24)], color=FLOW, weight=W, dashed=True,
            begin_arrow=k.ARROW_FILLED)
    p.arrow([(7.42, 9.24), (7.62, 9.24)], color=FLOW, weight=W, dashed=True,
            begin_arrow=k.ARROW_FILLED)
    p.arrow([(10.24, 9.24), (16.70, 9.24)], color=DUR, weight=WT,
            begin_arrow=k.ARROW_FILLED)

    # ---- the standing pieces ---------------------------------------------
    p.box(0.5, 10.85, 7.20, 1.05, "flint-s3-broker — the only standing "
          "credential",
          "TokenReview, online · a registration nonce the pod cannot mint · "
          "spec.consumers · then short-lived keys, on a loopback door. It "
          "reads no tenant Secret.",
          fill=d.PLAIN_F, line=d.PLAIN_L, line_weight=0.012, title_size=9.6,
          body_size=7.4, body_color=SUB)
    p.box(7.90, 10.85, 6.70, 1.05,
          "gated mode — durable NOW, visible on ONE CAS",
          "every changed file is uploaded as a new version at once — durable, "
          "and invisible — and one CAS cites the whole set. A reader sees the "
          "whole change or none of it. Refused without a lag bound.",
          fill="#F3EEFB", line=d.WORK_L, line_weight=0.012, title_size=9.6,
          body_size=7.4, body_color=SUB)
    p.box(15.40, 10.85, 6.50, 1.05, "lean operator — thin, and optional",
          "claim stamping · bucket posture · the MPU sweep. The syncer "
          "claims the LEASE itself, so a workspace mounts with the operator "
          "absent.",
          fill=d.OPER_F, line=d.OPER_L, line_weight=0.012, title_size=9.6,
          body_size=7.4, body_color=SUB)

    p.arrow([(4.10, 10.85), (4.10, 9.65)], color=CTL, weight=W, dashed=True)
    d.flabel(p, 5.55, 10.30, "keys for EVERY worker, never the app", CTL,
             w=2.3, size=7.2)
    p.arrow([(18.65, 10.85), (18.65, 10.60)], color=CTL, weight=W, dashed=True)
    d.flabel(p, 19.95, 10.72, "bucket-side work no pod should do", CTL,
             w=2.4, size=7.2)

    # the question the drawing raises and cannot answer: the gateway and
    # the syncer are BOTH writers, and their guarantees are opposite
    p.box(0.5, 12.15, 7.00, 1.45,
          "the agent's write — VISIBLE first, durable later",
          "It lands on local disk and the agent sees it at once, but it is "
          "NOT durable until the next barrier, when the syncer uploads it "
          "and CASes the pointer. .flint/publish.ack then says ok. RPO = the "
          "last barrier. The only refusal is a lost lease: refused-fenced.",
          fill=d.CLIENT_F, line=d.CLIENT_L, line_weight=0.012,
          title_size=9.6, body_size=7.3, body_color=SUB)
    p.box(7.70, 12.15, 7.00, 1.45,
          "the UI's write — DURABLE, and UNCITED",
          "PUT /files/{path} is a CONDITIONAL whole-object PUT (If-Match on "
          "what it read, else 409 concurrent-write). When it returns, the "
          "BYTES are in S3 — but nothing cites them: not the manifest, not "
          "the agent's tree. ADOPTION is a separate event at the syncer's "
          "next barrier, and it can go against them. Durable is not "
          "committed. Refused 409 + Retry-After while a window is open, and "
          "stamped epoch: 0 — deliberately the SECOND writer.",
          fill="#FFF6E2", line="#B0862A", line_weight=0.012,
          title_size=9.6, body_size=7.3, body_color=SUB)
    p.box(14.90, 12.15, 7.00, 1.45,
          "and when the two collide — the bytes are NOT deleted",
          "On consume the syncer HEADs each entry If-Match: superseded → "
          "dropped, object missing → conflict, path uncontainable → "
          "conflict. If the agent also touched that path, LOCALLY-DIRTY "
          "WINS — but the UI's bytes are COPIED to "
          ".flint/lean/conflicts/<uuid>/<path> (If-None-Match, so a "
          "preserve never clobbers) BEFORE the local version publishes over "
          "files/<path>, and a ConflictRecord names path, foreign ETag and "
          "preserved key. v1 gap: that record is written to the POD's "
          ".flint-sync/conflicts.jsonl and rides only a sync ack — the UI is "
          "never told that it lost.",
          fill="#FDECEC", line="#C0392B", line_weight=0.012,
          title_size=9.6, body_size=7.3, body_color="#8C2F22")

    # ---- the notes the picture cannot carry ------------------------------
    y = 13.95
    y = d.notes(p, 0.55, y, 21.3, [
        "THE ORDERING IS THE ARCHITECTURE. kubelet blocks every container in "
        "the pod on NodePublishVolume, so the checkout completes before the "
        "agent's first line runs — no init container, no readiness dance — "
        "and calls NodeUnpublishVolume only after every container has "
        "exited, which is where the final barrier goes. In between, the app "
        "touches plain files on real local disk with zero interception.",

        "THE MOUNT IS DELIVERED BY CSI, AND THERE IS NO WEBHOOK. The tenant "
        "pod declares a csi: volume naming a FlintLeanWorkspace in its own "
        "namespace and stays admissible under PodSecurity restricted: no "
        "injected sidecar, no mutating admission, no cert bootstrap, no "
        "privileged container and no S3 credential anywhere in the tenant "
        "namespace. Privilege is concentrated instead — the node plugin "
        "(one per node, holding no S3 credential and no Secrets RBAC), the "
        "worker (non-root, no ServiceAccount token) and the broker (the only "
        "standing credential, every issuance TokenReview-verified and "
        "audit-logged).",

        "THE BOUNDARY VERBS ARE FILES, because the workspace is the "
        "interface. An agent that can write a file can declare a coherent "
        "point — echo > .flint/publish — and learn when its bytes are "
        "durable, from .flint/publish.ack: ok means the bytes are in S3, "
        "refused-fenced means this syncer lost the lease and is saying so "
        "rather than leaving the agent waiting forever. No client library, "
        "no credential, no network path.",

        "A UI IS POWERED BY THE BUCKET, NOT BY THE POD, and that is forced "
        "rather than chosen: the workspace tree is local disk that dies with "
        "the pod, and the pod may not exist at all, so there is nothing for "
        "a UI to call. flint-lean-gateway — opt-in, its own bearer, an "
        "explicit workspace map, and deliberately NOT the lite gateway — "
        "reads and writes the same CAS cells the syncer uses. GET /snapshot "
        "returns {manifest, manifest_etag, inbox} in one read; GET "
        "/files/{path} reads via the manifest citation and falls back to an "
        "uncited-but-tracked inbox entry; GET /status reports seq, window, "
        "inbox depth and the epoch cell.",

        "THE HITL WRITE IS THREE ORDERED STEPS AND THE MANIFEST IS NOT ONE "
        "OF THEM. PUT /files/{path} writes the OBJECT first and appends an "
        "INBOX entry second — never a manifest edit — so the syncer stays "
        "the only writer of the pointer. The worker consumes the inbox at "
        "its next barrier (HEAD each entry If-Match; a superseded entry is "
        "dropped, not an error), materialises it into the tree and cites it "
        "in the next manifest. A write is refused 409 + Retry-After while a "
        "barrier window is open, and every gateway replica reads that window "
        "from the CELL rather than from its own memory — which is what makes "
        "two stateless replicas safe.",

        "THE WINDOW CANNOT WEDGE, AND A DEPOSED WORKER CANNOT WRITE. The "
        "window carries a deadline and a successor epoch may override a "
        "stale one, so a dead worker does not block HITL forever; and every "
        "worker-facing verb — window/open, window/clear, inbox/drop, "
        "manifest — is epoch-validated PER REQUEST, so a write whose claimed "
        "epoch is not the cell's current epoch is rejected. Rotation alone "
        "leaves that door open, which is exactly what the model's "
        "LeanNoEpochCheck mutation proves.",

        "AND ONE VERB IS DELIBERATELY CARRIED, NEVER PERFORMED. \u201cPlease "
        "publish\u201d from outside the pod is honoured; \u201cplease pull\u201d is "
        "recorded and left to the agent. A boundary publishes what is "
        "already on disk and touches no local file, while a sync re-derives "
        "the tree and DELETES local files for remotely-deleted paths — so "
        "performing it on a remote\u2019s say-so would upgrade what a leaked "
        "gateway bearer can do from \u201cpublish, plus hand over these N named "
        "objects\u201d to \u201crewrite and delete across a running agent\u2019s tree, at "
        "my timing, under a scope I choose\u201d. v1\u2019s recorded limits: "
        "whole-object HITL writes under a cap, one shared bearer, and no "
        "HITL delete verb yet.",

        "NOTHING HERE IS INJECTED, AND THERE IS NO WEBHOOK IN ANY OF IT. "
        "The flint-sync worker is CREATED by the node plugin during "
        "NodePublishVolume — one pod per published volume, in a system "
        "namespace, pinned with nodeName so it skips the scheduler, and "
        "owned by the Node object so a vanished node garbage-collects it. "
        "It is not a container in the tenant's pod and no mutating "
        "admission rewrites the tenant's spec; the crate is still named "
        "\u201csidecar\u201d only for historical reasons. The gateway, the broker "
        "and the operator are not injected either — each is an ordinary "
        "Deployment installed by Helm, and the gateway is opt-in: its chart "
        "REFUSES to render without a token Secret and a workspace map, "
        "because an unauthenticated gateway is an open writer to every "
        "workspace configured.",

        "WHY THE GATEWAY APPENDS TO AN INBOX AND NEVER CASes THE "
        "MANIFEST. Not because a second CAS writer would corrupt it — CAS "
        "would arbitrate that perfectly well. Because a manifest write is "
        "not a metadata edit, it is a CLAIM ABOUT A TREE: a boundary means "
        "\u201ceverything ordered-before T\u201d, and the entries it cites are one "
        "half of a pair whose other half is the syncer\u2019s on-disk baseline "
        "— classify() derives uploads from \u201cchanged vs the baseline\u201d and "
        "deletes from \u201cabsent twice AND present in the baseline\u201d. The "
        "gateway has no tree (its root is literally /nonexistent), no "
        "baseline and no scan, so it cannot honestly say what a coherent "
        "boundary contains. It CAN honestly say \u201cthese bytes exist and "
        "somebody asked for them\u201d, and that is exactly what an inbox entry "
        "is: a proposal about content, not a claim about a tree.",

        "CONCURRENT UI WRITERS ARE HANDLED IN TWO PLACES AND MISSED IN A "
        "THIRD. The inbox cell is CAS\u2019d with a five-attempt retry, so two "
        "browsers appending at once serialise and the loser retries — past "
        "five it fails the request rather than dropping the entry. And the "
        "queue holds ONE entry per path: a newer write supersedes the "
        "queued one, and at consume an entry whose ETag no longer matches "
        "the object is dropped as superseded. So one path yields one "
        "adoption of the last write, however many browsers raced. The "
        "object PUT is conditional as well — but on a FRESH HEAD the "
        "gateway takes immediately before writing, which closes the "
        "HEAD-to-PUT window and is NOT end-to-end optimistic concurrency: "
        "so an overwrite must now NAME what it read: 428 "
        "precondition-required without an If-Match, 412 file-changed on a "
        "stale one, and the current etag on the 412\u2019s own header. That is "
        "forge\u2019s taxonomy, adopted so the three doors are one shape. Until "
        "this landed the route accepted no caller If-Match at all, and two "
        "browsers that each read v1 and then wrote BOTH succeeded — the "
        "second silently winning.",

        "AND WHY NOT SIMPLY ROUTE TO THE WORKER, the way lite\u2019s gateway "
        "routes to a hub? Because half the time there is nothing to route "
        "TO. A lean worker is created by the node plugin at "
        "NodePublishVolume and DELETED at NodeUnpublish after the drain, so "
        "a workspace no tenant pod is mounting has no flint-sync anywhere — "
        "it is only a prefix in a bucket, and a UI must still be able to "
        "list and write it. A lite hub is a Deployment an operator can "
        "scale 0\u21921 and wake; a lean worker is a side effect of a POD "
        "being scheduled, and no gateway can conjure a tenant pod. Nor is "
        "there a door to route to: flint-sync\u2019s control surface is a UDS "
        "in the pod\u2019s emptyDir, and the code says why it stays that way — "
        "\u201cthere is no TCP listener and no authentication, because the "
        "trust boundary is the pod; a TCP listener would be a new remote "
        "surface, which is a stated non-goal\u201d.",

        "AND ROUTING WOULD HAND THE UI THE WRONG GUARANTEE, which is the "
        "part that surprises. A write placed into the worker\u2019s tree is "
        "visible at once and durable only at the next barrier — the "
        "AGENT\u2019s bargain. The UI\u2019s 200 would then precede the bytes "
        "reaching S3, and a pod that dies in between loses the write. The "
        "inbox inverts exactly that: durable when the call returns, adopted "
        "later. The same line is already drawn inside the pod — the UDS "
        "sync verb EXECUTES, \u201cbecause the caller is inside the pod: it is "
        "the agent asking for its own tree to be updated, which is the "
        "agent\u2019s own decision to make\u201d, while the gateway\u2019s sync request "
        "is carried and never performed. Inside the pod is a decision; "
        "outside it is a proposal.",

        "AND THREE CONSEQUENCES FOLLOW FROM THAT ONE. A manifest write "
        "would need the lease the gateway does not hold — it stamps epoch: "
        "0 — so either the gateway becomes a second lease holder and "
        "single-writer is gone, or manifest writes stop being fenced, which "
        "is the deposed-straggler hole. It would move the pointer\u2019s ETag "
        "on every UI write, so baseline.manifest_etag would no longer match "
        "and the syncer would take its adopt-a-foreign-manifest path on "
        "every one of them — an exceptional path made the common path. And "
        "it would read-modify-write an entries set that runs to hundreds of "
        "MiB at a million files, racing the barrier\u2019s own commit. The "
        "inbox is a few hundred bytes, needs agreement with nobody, and "
        "doubles as the barrier-window token, so \u201cis a barrier in "
        "flight?\u201d and \u201cqueue this write\u201d are one read and one CAS.",

        "THE GATEWAY IS A WRITER, BUT NOT THE SAME KIND OF WRITER, and "
        "the two guarantees are exact opposites: the agent's write is "
        "visible before it is durable, the UI's is durable before it is "
        "visible. What keeps that safe is that only ONE of them may write "
        "the manifest. The gateway stamps epoch: 0 and never edits the "
        "pointer; the syncer holds the lease and cites the inbox at its own "
        "barrier. So \u201cwritten\u201d means different things at the two doors, "
        "and a UI that reports success on a PUT is reporting durability, "
        "never adoption.",

        "TWO WORKSPACES SHARE THE MACHINERY AND NOTHING ELSE. The node "
        "plugin is one per node, the broker is one Deployment, and the "
        "gateway is one process whose workspace map is a list of id=prefix "
        "pairs — an unknown id is a 404, never a guessed prefix, and in v1 "
        "ONE bearer covers every workspace in that map, which is the "
        "recorded limit to know before pointing a UI at a multi-tenant one. "
        "Everything "
        "below that is per volume: its own worker pod (one per PUBLISHED "
        "volume, created by the plugin on its own node and owned by the "
        "Node so a vanished node GCs it), its own tree, its own credential, "
        "and its own prefix with its own epoch, claim, pointer and inbox. "
        "One writer per prefix is a mechanism inside a product; across "
        "products it is only a convention, so what assigns prefixes is what "
        "keeps two workspaces apart.",

        "THREE QUESTIONS DECIDE AGAINST LEAN, cheapest disqualifier first: "
        "does the tree fit the disk (the manifest is the wall — about 250k "
        "files); is one writer per subtree enough; is snapshot freshness "
        "acceptable. A no on any of them means the hub. And a worker is "
        "never taken away from a tenant still using its tree by ORDERING, "
        "not by a PodDisruptionBudget — a PriorityClass for kubelet's "
        "graceful shutdown and a preStop that waits for the release, "
        "because on a spot fleet the eviction API is not involved at all.",
    ])

    # ---- legend ----------------------------------------------------------
    y += 0.22
    d.legend(p, 0.55, y, [
        ("the workspace — one directory, two views, never a wire", FLOW,
         True),
        ("durable path — the worker is the only writer of the prefix", DUR,
         False),
        ("control plane — never carries a file", CTL, True),
        ("the file protocol — boundary verbs in the tree, REST at the "
         "gateway", ALT, False),
    ])

    d.glossary(p, 0.55, y + 0.45, 21.3, GLOSSARY)
    return doc, p


def main():
    args = [a for a in sys.argv[1:] if not a.startswith("--")]
    outdir = args[0] if args else _here
    doc, page = build()
    return d.emit(doc, page, outdir, "flint-lean-dataflow", sys.argv)


if __name__ == "__main__":
    sys.exit(main())
