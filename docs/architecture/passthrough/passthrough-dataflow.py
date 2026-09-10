#!/usr/bin/env python3
"""Build `flint-passthrough-dataflow.vsdx` — ONE page, the data flow.

The passthrough companion to `forge/forge-dataflow.py`. There is less
machinery here than in any other front end, and the page should look it:
one interception, one request per operation, and a bucket flint writes
nothing of its own into.

What the page has to explain, because it is the one surprising part, is
where the privilege went. The mount happens BEFORE the mounter runs: the
node DaemonSet opens /dev/fuse and calls mount(2) itself, then hands the
descriptor to an unprivileged worker over SCM_RIGHTS. No webhook, no
injected sidecar, and nothing privileged in the tenant's namespace.

Run:  python3 passthrough-dataflow.py [outdir] [--preview] [--pdf] [--emf]
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
    ("FUSE", "filesystem in userspace: the kernel hands each file "
             "operation to a process instead of a disk"),
    ("/dev/fuse", "the descriptor that IS the mount. Whoever holds it "
                  "serves the filesystem"),
    ("mount(2)", "the syscall that attaches a filesystem to a path — the "
                 "one act here that genuinely needs privilege"),
    ("SCM_RIGHTS", "the Unix-socket message that passes an open file "
                   "descriptor from one process to another"),

    ("CSI ephemeral inline", "a volume declared in the pod spec itself — no "
                             "PVC, no PV, nothing to bind beforehand"),
    ("requiresRepublish", "kubelet re-calling NodePublishVolume every 60-90 s "
                          "so the credential can be refreshed"),
    ("TokenReview", "the Kubernetes API that says whether a ServiceAccount "
                    "token is still valid — online, not offline"),
    ("registration nonce", "a binding the node plugin makes for this pod and "
                           "CR, which the pod itself cannot mint"),

    ("spec.consumers", "the ServiceAccounts a mount may be used by. ABSENT "
                       "MEANS DENY, never means everyone"),
    ("loopback door", "AWS_CONTAINER_CREDENTIALS_FULL_URI: how short-lived "
                      "keys reach the worker, and only the worker"),
    ("SigV4", "the signature an S3 wire accepts. No S3 endpoint takes a "
              "bearer token, which is why keys exist at all"),
    ("PodSecurity restricted", "the strictest built-in pod policy. A csi: "
                               "volume is on its allow-list; a privileged "
                               "container is not"),

    ("ENOTCONN", "what every open file returns once the mounter is gone. "
                 "Running containers are stranded, not retried"),
    ("Bidirectional", "the mount propagation that lets a mount made on the "
                      "host appear inside the pod"),
    ("FlintPassthroughMount", "the custom resource one mount is declared as, "
                              "in the tenant's own namespace"),
    ("PriorityClass · preStop", "how a worker outlives its tenant on a drain "
                                "or a spot reclaim — ordering, not a budget"),
]

STEPS = [
    ("resolve the CR",
     "FlintPassthroughMount, in the TOKEN's namespace — never a request "
     "field"),
    ("authorise the SA",
     "spec.consumers must list the pod's ServiceAccount. Absent means DENY"),
    ("mount(2) — ITSELF",
     "opens /dev/fuse and mounts. The one privileged act, once, per node"),
    ("hand the fd over",
     "SCM_RIGHTS, to a worker pod the plugin CREATED for this volume — no "
     "webhook, no injection"),
    ("credential",
     "the broker's short-lived keys, on a loopback door the tenant cannot "
     "reach"),
    ("bind into the pod",
     "one Bidirectional view. The mounter is now serving a mount that "
     "already exists"),
]


def build():
    doc = k.Document(
        title="flint-passthrough — data flow",
        creator="flint",
        description="One page: the components that matter, the data flow "
                    "between them, and a glossary.")
    p = doc.page("Data flow", 22.4, 14.0)

    d.header(p, "pass", "Data flow", name="passthrough",
             standfirst=
             "Every file operation in the pod is an S3 request. Nothing is "
             "buffered, flint writes nothing of its own into the bucket — "
             "and the privilege that makes the mount is not in the tenant's "
             "namespace.")

    spine = 3.375

    # ---- boundaries -----------------------------------------------------
    d.zone(p, 0.5, 2.20, 5.15, 3.55, "tenant namespace",
           sub="PodSecurity restricted — and it stays that way")
    d.container(p, 0.72, 2.76, 4.70, 2.72,
                "tenant pod — no sidecar, no label, no webhook, no credential")
    d.zone(p, 0.5, 6.05, 5.15, 1.30,
           "another tenant namespace — a different CR, a different bucket")
    d.node(p, "cloud", 13.20, 2.05, 8.70, 6.25, "", "", fill=d.CLOUD_F,
           line=d.CLOUD_L, line_weight=0.012)
    p.text(14.00, 2.45, 7.10, "S3-compatible object storage", size=10.5,
           color="#8A6A1E", bold=True, halign=1)

    # ---- the pod --------------------------------------------------------
    d.node(p, "rect", 0.92, 2.90, 4.30, 0.95, "/mnt/s3",
           "a FUSE mount. Every open, read, write and readdir in the pod is "
           "intercepted here — there is no local copy of anything",
           fill=d.CLIENT_F, line=d.CLIENT_L, line_weight=0.014,
           title_size=10.5, body_size=7.3)
    p.text(0.95, 3.96, 4.25, "nine lines of pod spec, and nothing else:",
           size=7.6, color=SUB, bold=True)
    for i, line in enumerate(["csi:",
                              "  driver: s3.csi.chert.us",
                              "  volumeAttributes:",
                              "    chert.us/mount: datasets"]):
        p.text(0.95, 4.16 + i * 0.185, 4.25, line, size=7.4, color=SUB,
               mono=True)
    p.text(0.95, 4.93, 4.30,
           "→ FlintPassthroughMount “datasets”, in the pod's OWN namespace",
           size=7.2, color=MUTE)
    p.text(0.95, 5.11, 4.30,
           "   bucket · keyPrefix · readOnly · uid · consumers",
           size=7.2, color=MUTE)

    d.node(p, "rect", 0.72, 6.50, 4.70, 0.68, "tenant pod #2  ·  /mnt/data",
           "nine more lines of pod spec, and nothing else changes",
           fill=d.CLIENT_F, line=d.CLIENT_L, line_weight=0.013,
           title_size=9.4, body_size=7.2)

    # ---- the mounter ----------------------------------------------------
    d.node(p, "rect", 7.60, 2.55, 3.60, 1.65, "mount-s3  ·  the worker",
           "an UNCHANGED upstream binary, serving the descriptor it was "
           "given. Non-root, all capabilities dropped, read-only rootfs, no "
           "ServiceAccount token — and no privilege, because the mount "
           "already happened",
           fill=d.WORK_F, line=d.WORK_L, line_weight=0.017, title_size=11,
           body_size=7.3)

    d.node(p, "rect", 7.60, 6.50, 3.60, 0.68, "mount-s3  ·  worker #2",
           "one worker per PUBLISHED VOLUME — its own fd, its own credential",
           fill=d.WORK_F, line=d.WORK_L, line_weight=0.014, title_size=9.4,
           body_size=7.2)

    # ---- the store, and what is NOT in it --------------------------------
    d.node(p, "cylinder", 15.15, 2.95, 4.80, 0.85, "objects",
           "<prefix>/<key> — they appear as files, and that is the whole "
           "mapping",
           fill=d.S3_F, line=d.S3_L, line_weight=0.014, cap=0.22,
           body_size=7.4)
    p.text(14.20, 4.20, 6.70,
           "AND NOTHING ELSE. flint writes no control namespace here — no "
           "lease, no manifest, no epoch, no .flint/ at all.", size=8,
           color="#8A6A1E", bold=True, halign=1)
    p.text(14.20, 4.44, 6.70,
           "Durability, consistency and access control are S3's, unmediated: "
           "flint adds none and removes none. Which is exactly why a "
           "passthrough mount can be pointed at a prefix somebody else's "
           "tooling already owns, and leave it as it was.",
           size=7.6, color=SUB, halign=1)

    d.node(p, "document", 11.40, 4.60, 0.90, 0.90, "", "", fill=d.CLIENT_F,
           line=d.CLIENT_L, line_weight=0.013)
    p.text(10.60, 5.56, 2.50, "browser / UI", size=9.6, color=INK, bold=True,
           halign=1)
    p.text(10.40, 5.76, 2.90, "the SAME objects, the SAME API", size=7.2,
           color=MUTE, halign=1)
    p.text(10.40, 5.94, 2.90, "its OWN credential — never the broker's",
           size=7.2, color=MUTE, halign=1)

    d.node(p, "cylinder", 15.15, 6.415, 4.80, 0.85, "another bucket",
           "or another prefix — DISJOINT. Nothing is shared between the two "
           "mounts, and nothing needs to be",
           fill=d.S3_F, line=d.S3_L, line_weight=0.014, cap=0.22,
           body_size=7.4)

    # ---- the flows -------------------------------------------------------
    p.arrow([(5.22, spine), (7.60, spine)], color=FLOW, weight=WT,
            begin_arrow=k.ARROW_FILLED)
    d.flabel(p, 6.58, spine - 0.24, "FUSE · every file operation", w=2.1)
    d.flabel(p, 6.58, spine + 0.23, "in the pod's critical path", MUTE,
             w=1.9, size=7.2)

    p.arrow([(11.20, spine), (15.15, spine)], color=DUR, weight=WT,
            begin_arrow=k.ARROW_FILLED)
    d.flabel(p, 12.15, spine - 0.24, "GET · PUT · LIST", DUR, w=1.5)
    d.flabel(p, 12.15, spine + 0.23, "per operation · no RPO, nothing is buffered", MUTE, w=2.6, size=7.2)

    p.arrow([(5.42, 6.84), (7.60, 6.84)], color=FLOW, weight=WT,
            begin_arrow=k.ARROW_FILLED)
    p.arrow([(11.20, 6.84), (15.15, 6.84)], color=DUR, weight=WT,
            begin_arrow=k.ARROW_FILLED)
    p.arrow([(12.30, 5.05), (13.60, 5.05), (13.60, 3.60), (15.15, 3.60)],
            color=DUR, weight=WT)

    # ---- how the mount gets there: CSI, and no webhook --------------------
    p.box(0.5, 7.60, 12.50, 1.95, "", "", fill="#FBFCFD", line=d.ZONE_L,
          dashed=True, rounding=0.14, line_weight=0.009)
    p.text(0.72, 7.72, 12.1,
           "s3.csi.chert.us  —  the node DaemonSet: the mount happens BEFORE "
           "the mounter runs, and there is NO webhook", size=10, color=INK,
           bold=True)
    p.text(0.72, 7.96, 12.1,
           "One privileged process per node, holding no S3 credential and no "
           "Secrets RBAC. kubelet calls NodePublishVolume with a pod-bound "
           "ServiceAccount token; the plugin does the one privileged act "
           "itself and gives the result away.", size=7.8, color=SUB)
    d.steps(p, 0.72, 8.53, 12.10, STEPS, color=CTL, body_size=7.1)

    p.arrow([(1.05, 7.60), (1.05, 7.18)], color=CTL, weight=W, dashed=True)
    d.flabel(p, 2.45, 7.42, "bind into the pod — one per volume", CTL,
             w=2.4, size=7.2)
    p.arrow([(9.40, 7.60), (9.40, 7.18)], color=ALT, weight=WT)
    d.flabel(p, 11.15, 7.42, "/dev/fuse fd  ·  SCM_RIGHTS", ALT, w=2.1,
             size=7.4)

    # ---- the standing pieces ---------------------------------------------
    p.box(0.5, 9.80, 6.90, 1.15,
          "flint-s3-broker — ONE Deployment, the only standing credential",
          "It serves every mount on every node. TokenReview, online · a "
          "registration nonce the pod cannot mint · "
          "spec.consumers · then short-lived keys, on a loopback door. It "
          "reads no tenant Secret, and in sts/rest mode holds no bucket key "
          "of its own.",
          fill=d.PLAIN_F, line=d.PLAIN_L, line_weight=0.012, title_size=9.6,
          body_size=7.4, body_color=SUB)
    p.box(7.60, 9.80, 6.90, 1.15,
          "privilege did not disappear — it was CONCENTRATED",
          "the node plugin (privileged, one per node, no S3 credential, no "
          "Secrets RBAC) · the workers (non-root, all caps dropped, "
          "read-only rootfs, no SA token) · the broker (the only standing "
          "credential, every issuance audit-logged). None of the three is in "
          "a tenant namespace.",
          fill=d.OPER_F, line=d.OPER_L, line_weight=0.012, title_size=9.6,
          body_size=7.4, body_color=SUB)
    p.box(14.70, 9.80, 7.20, 1.15,
          "what a passthrough mount is NOT",
          "not POSIX — no rename, no append, no in-place modification at any "
          "setting · not coordinated — two pods on one prefix do not see "
          "each other · not per-user — one uid, because NodePublish never "
          "sees the pod's securityContext · not self-healing — a dead "
          "mounter strands running containers on ENOTCONN and the pod must "
          "be recreated. A workload that needs any of those wants flint-lean.",
          fill="#FDECEC", line="#C0392B", line_weight=0.012, title_size=9.6,
          body_size=7.4, body_color="#8C2F22")

    p.arrow([(3.95, 9.80), (3.95, 9.55)], color=CTL, weight=W, dashed=True)
    d.flabel(p, 5.25, 9.68, "keys for every worker, never a tenant", CTL,
             w=2.5, size=7.2)

    # ---- the notes the picture cannot carry ------------------------------
    y = 11.25
    y = d.notes(p, 0.55, y, 21.3, [
        "THE MOUNT HAPPENS BEFORE THE MOUNTER RUNS, and that is the whole "
        "trick. The node plugin opens /dev/fuse and calls mount(2) itself — "
        "the one act that genuinely needs privilege, done once, per node — "
        "then hands the descriptor to an unprivileged worker pod over "
        "SCM_RIGHTS, where an unchanged mount-s3 serves it needing no "
        "privilege at all. The tenant pod is given nothing: no sidecar, no "
        "label, no webhook, no credential, no privileged container. It "
        "declares a csi: volume naming a CR in its own namespace and stays "
        "admissible under PodSecurity restricted.",

        "THERE IS NO CONTROLLER AND NO STATUS, because nothing about a "
        "passthrough mount converges. The CRD is the whole control surface "
        "and the node plugin reads it directly; what a "
        "(cluster issuer, namespace, ServiceAccount, CR) tuple is actually "
        "GIVEN is decided by the broker's backend — static for a rig, sts "
        "for a proxy that speaks AssumeRoleWithWebIdentity, rest for an "
        "application's own JWT-enforcing API. A CR on a cluster the "
        "project's policy never named yields nothing.",

        "A UI NEEDS NOTHING BUILT, which is the one place passthrough is "
        "the cheapest of the four. The bucket IS the API: a UI lists and "
        "GETs the same keys the mount presents, so there is no gateway to "
        "run, no inbox to drain and no manifest to respect. What it costs "
        "is the same thing the mount costs — no coordination. Two S3 "
        "clients with no lease between them can overwrite each other, and "
        "Mountpoint's own limits still bind what a UI can usefully do: no "
        "rename, no append, no in-place modification, so \u201cedit a file\u201d "
        "is always a whole-object PUT.",

        "EVERY OPERATION IS A REQUEST, so there is no recovery point to "
        "state: nothing is buffered and nothing is flushed. That is also the "
        "cost — the S3 round trip is in the pod's critical path, where lean "
        "has local disk and lite has one wire to one process. The bucket, "
        "meanwhile, is untouched by flint: this is the only front end you "
        "can point at a prefix someone else's tooling owns.",

        "A WORKER IS NEVER TAKEN AWAY FROM A TENANT STILL USING ITS MOUNT — "
        "by ORDERING, not by a PodDisruptionBudget. A budget only applies to "
        "the eviction API, and on a spot fleet the ordinary case is kubelet's "
        "graceful shutdown, where the eviction API is not involved at all. "
        "What is built instead is a PriorityClass that ranks a worker above "
        "its tenants, so kubelet tears it down last, and a preStop hook that "
        "waits for NodeUnpublishVolume to release the volume.",
    ])

    # ---- legend ----------------------------------------------------------
    y += 0.22
    d.legend(p, 0.55, y, [
        ("data plane — every file operation, intercepted", FLOW, False),
        ("the object path — the same operation, as a request", DUR, False),
        ("control plane — never carries a file", CTL, True),
        ("privilege — the one act that needs it, once per node", ALT, False),
    ])

    d.glossary(p, 0.55, y + 0.45, 21.3, GLOSSARY)
    return doc, p


def main():
    args = [a for a in sys.argv[1:] if not a.startswith("--")]
    outdir = args[0] if args else _here
    doc, page = build()
    return d.emit(doc, page, outdir, "flint-passthrough-dataflow", sys.argv)


if __name__ == "__main__":
    sys.exit(main())
