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
    ("fence", "whose turn it is to commit: held for one barrier's commit, "
              "then handed on. A deposed holder's late CAS is refused"),

    ("epoch · lease", "the bucket-side commit cell: one holder, a FIFO of waiters. It lives in the "
                      "BUCKET, so it fences a writer in ANY cluster"),
    ("claim", "the project identity stamped on the prefix: adopt your own, "
              "refuse a foreign one"),
    ("boundary verb", "a file the agent writes to declare a coherent point, "
                      "and an ack file it reads back"),
    ("boundary", "one fused barrier: upload the changed files, then install "
                 "the whole set with one CAS"),

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
    ("the request cell", "the two verb requests from outside the pod, please "
                         "publish and please pull. Not an inbox any more: its "
                         "key keeps the old name, .flint/lean/inbox"),
    ("P2", "since 2026-09-25 a UI verb COMMITS: one pointer CAS, judged "
           "against the version the UI read, else 412"),
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
           "ok  ·  partial  ·  refused-scope",
           fill="#FFF6E2", line="#B0862A", line_weight=0.013,
           title_size=8.8, body_size=7.4)

    # ---- the one directory, and the two views of it ----------------------
    d.node(p, "datastore", 6.35, 2.80, 2.75, 1.15, "the plugin-owned tree",
           "volumes/<vid>/tree\nONE tree, TWO views",
           fill=d.CACHE_F, line=d.CACHE_L, line_weight=0.013,
           title_size=9.6, body_size=7.4)

    d.node(p, "rect", 12.30, 2.40, 2.15, 4.05, "flint-sync",
           "a WRITER of the prefix — several share one, taking turns at the fence to commit\n\nunprivileged, in a system "
           "namespace: non-root, all capabilities dropped, no ServiceAccount "
           "token — and it holds the S3 credential the agent must not",
           fill=d.WORK_F, line=d.WORK_L, line_weight=0.017, title_size=11.5,
           body_size=7.4)

    # ---- the stores ------------------------------------------------------
    d.node(p, "cylinder", 16.70, 2.95, 4.15, 0.85, "files",
           "<prefix>/files/<path> — whole objects, every fetch verified against the manifest's CRC-64; ranged GETs and parallel parts above 8 MiB",
           fill=d.S3_F, line=d.S3_L, line_weight=0.013, cap=0.28,
           body_size=7.4)
    d.node(p, "cylinder", 16.70, 3.95, 4.15, 0.85, ".flint/lean/current",
           "THE pointer. ONE CAS per boundary or per UI verb — entries in manifests/ + chunks/",
           fill=d.S3_HOT_F, line=d.S3_L, line_weight=0.015, cap=0.28,
           body_size=7.4)
    d.node(p, "cylinder", 16.70, 4.95, 4.15, 0.85, ".flint/lean/epoch",
           "the publish fence, and the claim beside it — held per BARRIER, handed to a FIFO of waiters",
           fill=d.S3_F, line=d.S3_L, line_weight=0.013, cap=0.28,
           body_size=7.4)
    d.node(p, "cylinder", 16.70, 5.95, 4.15, 0.85, "the request cell",
           "ONE CAS document: please publish, please pull. No UI writes — "
           "not an inbox since P2 (key: .flint/lean/inbox)",
           fill=d.S3_HOT_F, line=d.S3_L, line_weight=0.015, cap=0.28,
           body_size=7.4)
    p.text(17.90, 7.20, 2.95, "\u2026workspace #1's prefix, above",
           size=7.4, color=MUTE, halign=2)
    d.node(p, "cylinder", 16.70, 8.82, 4.15, 0.85, "workspace #2's prefix",
           "its own files/, current, epoch, claim and request cell — DISJOINT. "
           "Writers share one prefix, never two, and these two never meet",
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
    d.flabel(p, 15.02, 5.135, "claim · hand off", DUR, w=1.20, size=7.4)
    p.arrow([(14.45, 6.375), (16.70, 6.375)], color=DUR, weight=WT,
            begin_arrow=k.ARROW_FILLED)
    d.flabel(p, 15.02, 6.135, "requests", DUR, w=1.10, size=7.4)
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
           "flint-lean-gateway — ONE door, for every workspace; and a CRATE",
           "one Deployment, N workspaces: /lean/v1/{workspace} against a map "
           "of id=prefix pairs — an unknown id is a 404, never a guessed "
           "prefix. Or no process at all: flint-lean-gateway on crates.io is "
           "the same verbs called in-process by your own backend.\n"
           "It talks to the BUCKET, never to the pod. GET /snapshot · /files · "
           "/status · drafts · DELETE /files · POST /rename — and the HITL "
           "write: PUT the object at a fresh handle, then ONE pointer CAS "
           "cites it (P2), never waiting on the writers' lease.",
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
          "a boundary — uploaded, then visible on ONE CAS",
          "every floor tick and every publish touch runs one fused barrier: the "
          "changed files are uploaded, then one CAS cites the whole set. A reader "
          "sees the whole boundary or none of it.",
          fill="#F3EEFB", line=d.WORK_L, line_weight=0.012, title_size=9.6,
          body_size=7.4, body_color=SUB)
    p.box(15.40, 10.85, 6.50, 1.05, "lean operator — thin, and optional",
          "claim stamping · bucket posture · the MPU sweep. The syncer "
          "claims the FENCE itself, so a workspace mounts with the operator "
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
          "last barrier. A boundary that lost its turn at the fence is retried.",
          fill=d.CLIENT_F, line=d.CLIENT_L, line_weight=0.012,
          title_size=9.6, body_size=7.3, body_color=SUB)
    p.box(7.70, 12.15, 7.00, 1.45,
          "the UI's write — DURABLE, and COMMITTED",
          "PUT /files/{path} names the version it read (If-Match, else 428). "
          "The bytes go to a fresh handle, then ONE pointer CAS cites them, "
          "landing only over that version: a stale save is a 412 that "
          "records nothing. Acknowledged once cited, so every reader and the "
          "next consume see it. Stamped epoch: 0, it never takes the writers' "
          "lease or waits for a barrier: a syncer mid-publish loses its CAS "
          "and merges again onto the save.",
          fill="#FFF6E2", line="#B0862A", line_weight=0.012,
          title_size=9.6, body_size=7.3, body_color=SUB)
    p.box(14.90, 12.15, 7.00, 1.45,
          "and when the two collide — the bytes are NOT deleted",
          "A syncer's commit is a three-way merge onto the CURRENT document, "
          "its baseline the merge base. Where the agent also changed a path "
          "the UI saved, the AGENT'S version wins — but the UI's is first "
          "COPIED to .flint/lean/conflicts/<uuid>/<path> (If-None-Match, so "
          "a preserve never clobbers) and a ConflictRecord names it (R7). An "
          "agent deleting a path the UI saved since deletes it, and the UI's "
          "version is preserved the same way (M3). v1 gap: the record is "
          "written to the POD's "
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
        "partial names what a foreign write kept out, and a boundary that "
        "lost its turn at the fence is retried rather than refused. No client "
        "library, no credential, no network path.",

        "A UI IS POWERED BY THE BUCKET, NOT BY THE POD, and that is forced "
        "rather than chosen: the workspace tree is local disk that dies with "
        "the pod, and the pod may not exist at all, so there is nothing for "
        "a UI to call. flint-lean-gateway — opt-in, its own bearer, an "
        "explicit workspace map, and deliberately NOT the lite gateway — "
        "reads and writes the same CAS cells the syncer uses, and since 0.1.0 "
        "it is a CRATE too: a backend depends on flint-lean-gateway and calls "
        "the verbs in-process, holding nothing but credentials for the "
        "prefixes it serves. GET /snapshot returns {manifest, manifest_etag, "
        "inbox} in one read — the field keeps the old name and carries only "
        "the request cell's two standing requests; "
        "GET /files/{path} reads the handle the manifest cites, and since "
        "every UI edit commits, that is the newest version and a read is one "
        "fetch; delete and rename are ONE pointer CAS each (a delete stops "
        "citing the path and deletes no object, its tombstone naming what it "
        "retired; a rename moves the citation, no bytes move); drafts are "
        "durable edits nobody sees until promoted.",

        "THE HITL WRITE IS TWO ORDERED STEPS AND THE SECOND IS THE COMMIT "
        "(P2, since 2026-09-25). PUT /files/{path} writes the OBJECT at a "
        "fresh handle first, then ONE pointer CAS cites it, judged against "
        "the version the UI read: a save over a version someone else "
        "published since is a 412 that records nothing, and the UI re-reads "
        "and reconciles. It never takes the writers' lease and never waits "
        "for a barrier — a syncer mid-publish loses its CAS, merges again "
        "onto the save and retries — so the cost of heavy saving falls on "
        "the writers, never on the person saving. Before step 5 a write was "
        "an object plus an inbox entry a syncer later cited; that cell now "
        "carries only the two verb requests.",

        "A DEPOSED WORKER CANNOT WRITE, AND NOTHING CAN WEDGE THE UI. The "
        "worker-facing verb (POST /manifest) is epoch-validated PER "
        "REQUEST, so a write whose claimed epoch is not the cell's current "
        "epoch is rejected — rotation alone leaves that door open, which is "
        "exactly what the model's LeanNoEpochCheck mutation proves. And "
        "there is no barrier window any more: a UI save never waits on a "
        "worker, so a dead one blocks nobody (Prop_UISaveCompletes, fair to "
        "the gateway's CAS alone).",

        "AND ONE VERB IS DELIBERATELY CARRIED, NEVER PERFORMED. \u201cPlease "
        "publish\u201d from outside the pod is honoured; \u201cplease pull\u201d is "
        "recorded and left to the agent. A boundary publishes what is "
        "already on disk and touches no local file, while a sync re-derives "
        "the tree and DELETES local files for remotely-deleted paths — so "
        "performing it on a remote\u2019s say-so would upgrade what a leaked "
        "gateway bearer can do from \u201cpublish, plus hand over these N named "
        "objects\u201d to \u201crewrite and delete across a running agent\u2019s tree, at "
        "my timing, under a scope I choose\u201d. v1\u2019s recorded limits: "
        "whole-object HITL writes under a cap, and one shared bearer.",

        "NOTHING HERE IS INJECTED, AND THERE IS NO WEBHOOK IN ANY OF IT. "
        "The flint-sync worker is CREATED by the node plugin during "
        "NodePublishVolume — one pod per published volume, in a system "
        "namespace, pinned with nodeName so it skips the scheduler, and "
        "owned by the Node object so a vanished node garbage-collects it. "
        "It is not a container in the tenant's pod and no mutating "
        "admission rewrites the tenant's spec: the component is the SYNCER, "
        "and flint-sync is its binary. The gateway, the broker "
        "and the operator are not injected either — each is an ordinary "
        "Deployment installed by Helm, and the gateway is opt-in: its chart "
        "REFUSES to render without a token Secret and a workspace map, "
        "because an unauthenticated gateway is an open writer to every "
        "workspace configured.",

        "WHY THE GATEWAY MAY CAS THE MANIFEST AFTER ALL. The old reason it "
        "did not still holds: a BOUNDARY is a claim about a tree, and the "
        "gateway has no tree (its root is literally /nonexistent), no "
        "baseline and no scan. P2 makes that not matter. A UI verb edits "
        "ONE path's citation, judged against the version the UI read, and "
        "claims nothing about any other path. And a writer no longer "
        "adopts a pointer that moved under it: its baseline IS its merge "
        "base, what its tree is owed is derived from the document at each "
        "consume (P1-lite), and its commit is a three-way merge onto the "
        "CURRENT document — so a save that landed between a syncer's read "
        "and its CAS is merged in, never lost.",

        "CONCURRENT UI WRITERS ARE SETTLED BY THE ONE CAS. Every overwrite "
        "NAMES what it read: 428 precondition-required without an If-Match, "
        "412 file-changed on a stale one, and the current etag on the "
        "412\u2019s own header — forge\u2019s taxonomy, adopted so the three "
        "doors are one shape. Two browsers that each read v1 and then save: "
        "one CAS lands, the other is a 412 that records nothing and "
        "re-reads. A verb that loses its CAS to the WRITERS rather than to "
        "another save re-reads, re-judges and retries. Before If-Match was "
        "required, two such browsers BOTH succeeded — the second silently "
        "winning.",

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
        "gateway\u2019s commit inverts exactly that: durable and committed when "
        "the call returns. The same line is already drawn inside the pod — the UDS "
        "sync verb EXECUTES, \u201cbecause the caller is inside the pod: it is "
        "the agent asking for its own tree to be updated, which is the "
        "agent\u2019s own decision to make\u201d, while the gateway\u2019s sync request "
        "is carried and never performed. Inside the pod is a decision; "
        "outside it is a proposal.",

        "WHAT P2 COSTS, AND WHERE IT FALLS. Every UI verb moves the "
        "pointer every syncer and reader checks, so an editor autosaves to "
        "a DRAFT, never with a commit: drafts live under the reserved "
        "namespace no scan, checkout or sweep can see, and a promote commits "
        "conditioned on the base it recorded. A syncer whose CAS loses to a "
        "save merges again, so heavy saving costs the writers. And what a "
        "save replaced is not deleted: nothing cites it, the retire log "
        "keeps it for the retire age G (600 s) and only then may a sweep "
        "take it, so a reader that loaded the document less than G ago can "
        "still fetch every handle it cites (Inv_ReaderFetches).",

        "THE GATEWAY IS A WRITER, AND THE TWO DOORS STILL MEAN DIFFERENT "
        "THINGS: the agent's write is visible before it is durable, the "
        "UI's is durable and committed when the call returns. The gateway "
        "stamps epoch: 0 and never takes the fence — its CAS is judged "
        "against the version the UI read, not against the lease — while a "
        "syncer holds the fence for its own commit. So \u201cwritten\u201d "
        "still means different things at the two doors, and a UI that "
        "reports success on a PUT is reporting a commit.",

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
        "and its own prefix with its own epoch, claim, pointer and request cell. "
        "Taking turns at one prefix's fence is a mechanism inside a product; across "
        "products it is only a convention, so what assigns prefixes is what "
        "keeps two workspaces apart.",

        "THREE QUESTIONS DECIDE AGAINST LEAN, cheapest disqualifier first: "
        "does the tree fit the disk, and its file count the checkout budget "
        "(the manifest is chunked and a publish is O(changed), but every "
        "checkout still materialises every file); is a shared log with "
        "per-pod working copies enough (a same-file edit is the later "
        "boundary's, the other kept); is snapshot freshness "
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
        ("durable path — the writers and the gateway, a CAS at a time", DUR,
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
