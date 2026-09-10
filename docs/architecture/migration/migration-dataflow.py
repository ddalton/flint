#!/usr/bin/env python3
"""Build `flint-migration-dataflow.vsdx` — ONE page, a use case.

The four front-end posters each draw one product. This one draws a JOB:
a workload whose data is on an NFS share it already mounts, moving that
data into a bucket through a flint-passthrough mount, with ordinary code
in the pod doing the copy.

It exists because the question it answers is asked in the wrong shape.
"How does the CSI driver inject the mount, and what label do I put on the
pod?" — there is no label and there is no webhook. The pod declares a
`csi:` volume whose `volumeAttributes` name a CR, kubelet calls
NodePublishVolume when it schedules the pod, and the mount is made then.
Nothing mutates the pod, and the pod cannot name the bucket at all.

Run:  python3 migration-dataflow.py [outdir] [--preview] [--pdf] [--emf]
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

NFS_F, NFS_L = "#E8EEF3", "#5A7387"

GLOSSARY = [
    ("CSI ephemeral inline", "a volume declared in the pod spec itself — no "
                             "PVC, no PV, nothing to bind beforehand"),
    ("volumeAttributes", "the map on that inline volume. It is where "
                         "chert.us/mount goes — NOT metadata.labels"),
    ("NodePublishVolume", "the CSI call kubelet makes when it schedules the "
                          "pod. This is when the mount happens"),
    ("FlintPassthroughMount", "the CR that names the bucket, prefix, uid and "
                              "consumers. The POLICY object"),

    ("chert.us/mount", "the only key that selects a destination. It names a "
                       "CR in the pod's own namespace, never a bucket"),
    ("spec.consumers", "the ServiceAccounts a mount may be used by. ABSENT "
                       "MEANS DENY, never means everyone"),
    ("mount(2)", "the syscall that attaches a filesystem to a path — the one "
                 "act here that genuinely needs privilege"),
    ("SCM_RIGHTS", "the Unix-socket message that passes an open file "
                   "descriptor from one process to another"),

    ("FUSE", "filesystem in userspace: the kernel hands each file operation "
             "to a process instead of a disk"),
    ("mount-s3", "Mountpoint for S3, unchanged upstream. It serves the "
                 "descriptor the node plugin already mounted"),
    ("kernel NFS client", "what serves /mnt/nfs. In-tree, nothing to do with "
                          "flint — the share is mounted as it always was"),
    ("flint-s3-broker", "the one Deployment holding a standing credential. "
                        "It issues short-lived keys to the worker only"),

    ("EACCES on every write", "what a uid mismatch looks like. The CR's uid "
                              "must match the pod's runAsUser"),
    ("no rename", "Mountpoint cannot rename. rsync's default temp-file-"
                  "then-rename fails; cp and rsync --inplace do not"),
    ("whole-object PUT", "each file is written once, sequentially. There is "
                         "no append and no in-place modification"),
    ("Job, not Deployment", "a copy that finishes should be a Job — and a "
                            "re-run is safe, because keys are idempotent"),
]

STEPS = [
    ("kubelet schedules the pod",
     "the csi: volume is inline — no PVC to bind, nothing to provision "
     "first"),
    ("NodePublishVolume",
     "kubelet passes a pod-bound SA token and its OWN csi.storage.k8s.io/* "
     "keys"),
    ("resolve the CR",
     "chert.us/mount names a FlintPassthroughMount in the TOKEN's namespace"),
    ("authorise the SA",
     "spec.consumers must list the pod's ServiceAccount. Absent means DENY"),
    ("mount(2) — ITSELF",
     "the plugin mounts, then passes /dev/fuse to a worker it created, "
     "over SCM_RIGHTS"),
    ("bind into the pod",
     "/mnt/s3 appears. The pod was never mutated: no webhook, no injected "
     "sidecar"),
]


def build():
    doc = k.Document(
        title="flint-passthrough — migration data flow",
        creator="flint",
        description="One page: moving an existing NFS share into a bucket "
                    "with a passthrough mount, and how that mount arrives.")
    p = doc.page("Migration", 22.4, 15.6)

    d.header(p, "pass", "Migration — an NFS share into a bucket",
             name="passthrough",
             standfirst=
             "The pod already mounts its NFS share. A SECOND volume in the "
             "same pod spec names a FlintPassthroughMount, and the CSI node "
             "plugin mounts the destination bucket at /mnt/s3. The copy "
             "itself is ordinary code in the pod — flint moves no bytes of "
             "its own.")

    spine = 3.40

    # ---- what is already there, and is not flint -------------------------
    d.zone(p, 0.5, 2.15, 3.60, 3.15, "outside flint",
           sub="the storage the workload has today")
    d.node(p, "box3d", 0.76, 2.85, 3.08, 1.10, "NFS server",
           "whatever exports it now — NFSv3 or NFSv4", fill=NFS_F, line=NFS_L,
           line_weight=0.014, depth=0.09, title_size=10.5, body_size=7.3)
    p.text(0.78, 4.15, 3.04,
           "NOT flint-lite. No FlintShare, no hub, no operator, no "
           "pNFS — an ordinary nfs: volume or a PVC bound to it, mounted "
           "by the kernel exactly as it always was.",
           size=7.2, color=MUTE, halign=1)

    # ---- the pod ---------------------------------------------------------
    d.zone(p, 4.50, 2.15, 8.95, 5.55, "tenant namespace   team-a",
           sub="PodSecurity restricted — and it stays that way")
    d.container(p, 4.72, 2.72, 8.51, 2.06,
                "migration pod — a Job. Two mounts, one process, no "
                "sidecar, no privilege, no credential")

    p.text(4.94, 2.75, 8.07,
           "serviceAccountName: migrator     ·     runAsUser: 1001     ·     "
           "the two values the FlintPassthroughMount has to agree with",
           size=7.4, color=MUTE, halign=1)

    d.node(p, "rect", 4.94, 3.00, 2.34, 0.80, "/mnt/nfs",
           "the existing share\nthe kernel's own NFS client serves it",
           fill=NFS_F, line=NFS_L, line_weight=0.014,
           title_size=10.2, body_size=7.2)

    # The copy is not a component, so it is not a box. It is an ACT
    # between the two mounts, and a block arrow from the one it reads to
    # the one it writes says that; a rectangle here named a process the
    # reader then looked for in the pod spec, where there is nothing but
    # a command.
    d.node(p, "block_arrow", 7.34, 3.00, 3.27, 0.80, "your copy code",
           "read()  then  write()", fill=d.OPER_F, line=d.OPER_L,
           line_weight=0.014, title_size=10.2, body_size=7.2,
           head=0.24, shaft=0.60)

    d.node(p, "rect", 10.67, 3.00, 2.34, 0.80, "/mnt/s3",
           "the destination\na FUSE mount — every op is an S3 request",
           fill=d.CLIENT_F, line=d.CLIENT_L, line_weight=0.014,
           title_size=10.2, body_size=7.2)

    p.text(4.94, 4.02, 8.07,
           "Once both mounts are present this is a plain file copy between "
           "two paths: read from one, write to the other. flint neither "
           "runs it nor sees it.", size=7.4, color=SUB, halign=1)
    p.text(4.94, 4.28, 8.07,
           "cp -a /mnt/nfs/. /mnt/s3/", size=8.4, color=INK, mono=True,
           bold=True, halign=1)

    # the two manifests, side by side
    p.text(4.72, 5.02, 4.12, "the pod spec — the ONE addition is a second "
                             "volume", size=8.2, color=INK, bold=True)
    for i, line in enumerate([
            "volumes:",
            "  - name: source            # already there",
            "    nfs: { server: nfs.corp, path: /exports/a }",
            "  - name: dest              # the addition",
            "    csi:",
            "      driver: s3.csi.chert.us",
            "      volumeAttributes:",
            "        chert.us/mount: migration-target",
            "volumeMounts:",
            "  - { name: source, mountPath: /mnt/nfs }",
            "  - { name: dest,   mountPath: /mnt/s3 }"]):
        p.text(4.72, 5.28 + i * 0.178, 4.20, line, size=7.2,
               color=SUB if "#" not in line else MUTE, mono=True)

    p.text(9.10, 5.02, 4.15, "FlintPassthroughMount — the policy object",
           size=8.2, color=INK, bold=True)
    for i, line in enumerate([
            "kind: FlintPassthroughMount",
            "metadata: { name: migration-target, ns: team-a }",
            "spec:",
            "  bucket: team-a-archive",
            "  keyPrefix: 2026/migration",
            "  uid: 1001                 # = runAsUser",
            "  gid: 1001",
            "  consumers:",
            "    serviceAccounts: [migrator]",
            "  identity: { mode: broker }"]):
        p.text(9.10, 5.28 + i * 0.178, 4.20, line, size=7.2,
               color=SUB if "#" not in line else MUTE, mono=True)

    # ---- the worker, its namespace, and the bucket ------------------------
    # The worker is a POD, and the question every reader asks next is
    # which namespace it is in. Drawing it loose on the page invited the
    # answer "the tenant's". It is not, and the boundary says so.
    d.zone(p, 14.00, 2.15, 3.20, 3.15, "flint-workers",
           sub="a system namespace — one for the whole cluster")
    d.node(p, "rect", 14.10, 2.85, 3.00, 1.10, "mount-s3  ·  the worker",
           "unprivileged, one per PUBLISHED VOLUME, serving the descriptor "
           "the node plugin already mounted",
           fill=d.WORK_F, line=d.WORK_L, line_weight=0.016, title_size=10.0,
           body_size=7.2)
    p.text(14.10, 4.12, 3.00,
           "NOT team-a. The tenant cannot exec into it — and the worker is "
           "what holds the credential. Labelled chert.us/tenant-namespace so "
           "you can still find it: kubectl -n flint-workers get pods -l "
           "chert.us/tenant-namespace=team-a",
           size=7.2, color=MUTE, halign=1)

    d.node(p, "cloud", 17.50, 2.15, 4.40, 3.15, "", "", fill=d.CLOUD_F,
           line=d.CLOUD_L, line_weight=0.012)
    p.text(17.90, 2.52, 3.60, "S3-compatible object storage", size=10.0,
           color="#8A6A1E", bold=True, halign=1)
    d.node(p, "cylinder", 18.00, 2.95, 3.40, 0.95, "destination bucket",
           "team-a-archive / 2026/migration / <the tree, key by key>",
           fill=d.S3_F, line=d.S3_L, line_weight=0.014, cap=0.24,
           title_size=9.6, body_size=7.2)
    p.text(17.90, 4.12, 3.60,
           "flint writes nothing of its own here — no manifest, no epoch, "
           "no .flint/. What lands is your tree, as objects.",
           size=7.2, color=MUTE, halign=1)

    # ---- who creates the worker ------------------------------------------
    # The answer is "the node plugin", and the plugin is in a THIRD
    # namespace. Three boundaries, one node: that is the whole shape.
    d.zone(p, 14.00, 5.70, 3.20, 1.95, "flint-system",
           sub="the driver's own namespace")
    d.node(p, "rect", 14.20, 6.32, 2.80, 1.05,
           "s3.csi.chert.us  ·  the node plugin",
           "a DaemonSet. It CREATES the worker pod, then mounts and hands "
           "over the fd", fill=d.SERV_F, line=d.SERV_L, line_weight=0.014,
           title_size=9.6, body_size=7.2)
    p.text(14.20, 7.44, 2.80,
           "pinned with nodeName — the scheduler never sees it",
           size=7.2, color=MUTE, halign=1)

    p.arrow([(15.60, 5.70), (15.60, 5.35)], color=CTL, weight=W, dashed=True)
    d.flabel(p, 14.42, 5.50, "creates the pod, then deletes it", CTL, w=2.2,
             size=7.2)

    # ---- the flows -------------------------------------------------------
    p.arrow([(3.84, spine), (4.94, spine)], color=FLOW, weight=WT)
    d.flabel(p, 4.17, spine - 0.25, "NFS", w=0.58, size=7.4)

    # no line arrows across the pod: the block arrow IS that flow

    p.arrow([(13.01, spine), (14.10, spine)], color=FLOW, weight=WT)
    # BETWEEN two zone edges now — the tenant's (13.45) and flint-workers'
    # (14.00). A vertical edge spans a zone's whole height, so no y clears
    # either one; the label lives in the 0.55 of open page between them.
    d.flabel(p, 13.72, spine - 0.25, "FUSE", w=0.44, size=7.4)

    p.arrow([(17.10, spine), (18.00, spine)], color=DUR, weight=WT)
    d.flabel(p, 17.55, spine - 0.25, "PUT", DUR, w=0.62, size=7.4)

    # ---- how the second mount arrives ------------------------------------
    p.box(0.5, 7.95, 13.35, 2.00, "", "", fill="#FBFCFD", line=d.ZONE_L,
          dashed=True, rounding=0.14, line_weight=0.009)
    p.text(0.72, 8.07, 12.95,
           "s3.csi.chert.us  —  how /mnt/s3 gets there. There is NO webhook, "
           "and the pod is never mutated", size=10, color=INK, bold=True)
    p.text(0.72, 8.31, 12.95,
           "One privileged process per node, holding no S3 credential and no "
           "Secrets RBAC. It is kubelet that calls it, at the moment the pod "
           "is scheduled — which is why nothing has to watch for a label.",
           size=7.8, color=SUB)
    d.steps(p, 0.72, 8.88, 12.95, STEPS, color=CTL, body_size=7.1)

    p.arrow([(11.90, 7.95), (11.90, 7.70)], color=CTL, weight=W, dashed=True)
    d.flabel(p, 10.30, 7.83, "bind into the pod", CTL, w=1.6, size=7.2)

    # ---- the correction, and the traps -----------------------------------
    p.box(14.15, 7.95, 7.75, 1.00,
          "there is no label, and the pod cannot name the bucket",
          "chert.us/mount is domain-prefixed like a label, but it is a KEY "
          "in volumeAttributes on the inline csi: volume — not "
          "metadata.labels, and nothing admits or mutates on it. Bucket, "
          "keyPrefix, endpoint, region, image and credentials are refused BY "
          "NAME from the pod: only chert.us/mount|workspace and "
          "chert.us/uid|gid are accepted. Kubelet's own csi.storage.k8s.io/* "
          "keys overwrite anything the pod puts there, and those are the "
          "only inputs the authorisation step trusts.",
          fill=d.PLAIN_F, line=d.PLAIN_L, line_weight=0.012, title_size=9.6,
          body_size=7.4, body_color=SUB)

    p.box(14.15, 9.10, 7.75, 0.85,
          "what bites a copy, specifically",
          "uid — NodePublish never sees securityContext, so the CR's uid "
          "must match runAsUser or every write is EACCES · rsync — its "
          "default writes a temp file and renames, and Mountpoint has no "
          "rename: use cp, or rsync --inplace · readOnly on the destination "
          "CR is policy, not presentation, and refuses the copy outright.",
          fill="#FDECEC", line="#C0392B", line_weight=0.012, title_size=9.6,
          body_size=7.4, body_color="#8C2F22")

    # ---- the notes the picture cannot carry ------------------------------
    y = 10.25
    y = d.notes(p, 0.55, y, 21.35, [
        "THE POD IS CREATED THE ORDINARY WAY, and that is the whole point of "
        "the shape. You apply a Job whose spec has two volumes: the nfs: one "
        "it already had, and a csi: one naming a FlintPassthroughMount. "
        "There is no mutating webhook in flint at all, so nothing rewrites "
        "the pod between kubectl and kubelet — what you applied is what "
        "runs. The mount appears because kubelet calls NodePublishVolume on "
        "the node plugin when it schedules the pod, and the plugin does the "
        "one privileged act itself. A typo in chert.us/mount is therefore "
        "not an admission error but a FailedMount event naming the CR and "
        "the namespace, and the pod simply never starts.",

        "IT USED TO BE A LABEL, and anyone who set this up before v1.45.0 will look for one. Until then a mutating webhook watched for flint.io/passthrough-mount on the pod — objectSelector, key Exists — and the label’s VALUE named the CR, exactly as the volumeAttributes value does now. What it injected was a PRIVILEGED native sidecar, and that privilege was not reducible: /dev/fuse alone would take CAP_SYS_ADMIN and a hostPath device, but the mount has to REACH the app container, which needs mountPropagation: Bidirectional, which the API server permits only on a privileged container. So a namespace enforcing PodSecurity baseline or restricted REJECTED the mutated pod — correctly. `fcac038f` replaced both sidecar-injection webhooks (passthrough’s and lean’s) with the node DaemonSet, because spec.volumes[*].csi is on the restricted allow-list. The information you supply did not change; its carrier did.",

        "THE WORKER IS A POD, AND IT IS NOT IN YOUR NAMESPACE. It lives in "
        "flint-workers — one system namespace for the whole cluster, not one "
        "per tenant — created by the node plugin during NodePublishVolume, "
        "pinned to this node with nodeName so the scheduler never places it, "
        "and owned by the Node object so a vanished node garbage-collects "
        "it. Three reasons it is not in team-a: the worker is what holds the "
        "credential, and anyone who can exec into pods in their own "
        "namespace could read it; PodSecurity is enforced differently, since "
        "flint-workers must be labelled privileged (the lean tree needs "
        "hostPath, forbidden under baseline) while your namespace stays "
        "restricted; and the plugin's own RBAC is a Role in flint-workers "
        "ALONE — pods create/get/list/watch/delete there and nowhere else, "
        "with a ValidatingAdmissionPolicy that further pins spec.nodeName to "
        "the node making the call, so a compromised node agent cannot place "
        "a pod anywhere but on itself. You can still find yours: "
        "chert.us/tenant-namespace and chert.us/tenant-pod are labels on the "
        "worker.",

        "THE POD NAMES A CR; THE CR NAMES THE BUCKET. That indirection is "
        "the security property worth drawing, because it is what makes the "
        "destination a decision the platform owns rather than one the "
        "workload asserts. The pod author cannot point a mount at an "
        "arbitrary bucket by editing their own manifest — volumeAttributes "
        "are attacker-controlled input to a privileged process, and every "
        "key but the selector and the two integers is refused by name. "
        "Whoever may create a FlintPassthroughMount in that namespace "
        "decides which buckets exist to be named, and spec.consumers "
        "decides which ServiceAccounts may name them.",

        "THE COPY IS YOUR CODE, running between two paths that happen to be "
        "backed by very different things: /mnt/nfs is served by the kernel's "
        "own NFS client and /mnt/s3 by a FUSE mount whose every operation is "
        "an S3 request. Nothing coordinates them and nothing needs to — the "
        "process reads a file and writes a file. What that costs is the "
        "S3 round trip in the copy's critical path, so throughput comes "
        "from concurrency: several files in flight, or several pods each "
        "given a subtree. What it saves is everything else — no staging "
        "disk, no intermediate format, no second copy of the data.",

        "A RE-RUN IS SAFE, because a key is idempotent: writing the same "
        "object again yields the same object. That makes a Job with a "
        "backoff the right shape and makes “did it finish?” answerable by "
        "listing both sides rather than by trusting an exit code — the "
        "destination is a bucket, so the census is a LIST and the "
        "comparison is name-and-size. What a re-run will NOT do is repair a "
        "partially written object in place: Mountpoint writes each file "
        "once, sequentially, as a whole-object PUT, so an interrupted file "
        "is re-written from the beginning rather than resumed.",

        "WHEN THIS IS THE WRONG TOOL. A passthrough mount is not POSIX — no "
        "rename, no append, no in-place modification — so it suits a copy "
        "and suits nothing that wants to keep editing afterwards. If the "
        "workload's own writes must continue against the destination with "
        "ordinary file semantics, the destination wants flint-lean (a local "
        "tree with a durable path behind it) or flint-lite (an NFS server "
        "whose working set is a PVC). Migrating INTO one of those is a "
        "different picture: the destination mount is theirs, and the copy "
        "step is the same plain code.",
    ])

    # ---- legend ----------------------------------------------------------
    y += 0.22
    d.legend(p, 0.55, y, [
        ("data plane — the bytes the copy moves", FLOW, False),
        ("the object path — the same bytes, as requests", DUR, False),
        ("control plane — how the mount arrives", CTL, True),
    ], span=7.1)

    d.glossary(p, 0.55, y + 0.45, 21.35, GLOSSARY)
    return doc, p


def main():
    args = [a for a in sys.argv[1:] if not a.startswith("--")]
    outdir = args[0] if args else _here
    doc, page = build()
    return d.emit(doc, page, outdir, "flint-migration-dataflow", sys.argv)


if __name__ == "__main__":
    sys.exit(main())
