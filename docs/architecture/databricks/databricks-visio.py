#!/usr/bin/env python3
"""Build `flint-vs-databricks.vsdx` — the figures of the Databricks
comparison document, one Visio page per figure.

The question behind the document: Databricks gives its users POSIX paths
over object storage (/Volumes, /Workspace, /dbfs) through FUSE, and it
works well enough to carry a very large business. How does it mount the
buckets, does it use a sidecar or a CSI node driver, and how does that
compare with flint's passthrough and lean?

Databricks is closed. Every Databricks box on these pages therefore wears
an EVIDENCE PILL saying how much the claim rests on:

    DOC    stated by Databricks — docs, KB, blog, paper, conference talk
    SEEN   observed by a third party — a forum post, a blog's command output
    INFER  inferred here from documented behaviour; stated nowhere

The pills are the point of the pages, not decoration: a reader must be
able to tell "Databricks says" from "we think" at a glance. The sources
behind each pill are listed in the document's last page. flint's boxes
carry no pill; their source of record is this repository
(spdk-csi-driver/src/s3csi/, lean/, docs/flint-lean-for-agent-fleets.md
and the passthrough and lean posters beside this directory).

Run:  python3 databricks-visio.py [outdir] [--svg] [--emf] [--pdf]

  --svg   write each page as diagrams/NN-name.svg for the HTML document
  --emf   write one EMF per page, for pasting into Office
  --pdf   render the pages alone to flint-vs-databricks-figures.pdf

A non-zero exit means a gate found something. Run the gates, read
"N shapes, M pages, 0 problems", and then LOOK at the render: the gates
are necessary, not sufficient.
"""

import os
import sys

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import dataflowkit as d      # noqa: E402

_here = os.path.dirname(os.path.abspath(__file__))
k = d.k
emf = d.emf

INK, SUB, MUTE, PAPER = d.INK, d.SUB, d.MUTE, d.PAPER
FLOW, DUR, CTL, ALT = d.FLOW, d.DUR, d.CTL, d.ALT
W, WT = d.W, d.WT

# Databricks is not a flint front end, so it gets a neutral slate rather
# than a hue from the deck's palette; flint's columns keep their own.
DBX = "#3F4B5B"
PASS = d.ACCENT["pass"]
LEAN = d.ACCENT["lean"]

RED_F, RED_L, RED_T = "#FDECEC", "#C0392B", "#8C2F22"
OK_F, OK_L, OK_T = "#E8F5EC", "#2F7D4A", "#1F5A34"
ROW_F, ROW_L = "#F6F7F9", "#DCE0E5"

GRADE = {"DOC": ("#E3F2E8", "#2F7D4A"),
         "SEEN": ("#E4EEF9", "#2E6FB7"),
         "INFER": ("#FFF1D6", "#A8741A")}

LINE = 0.012


# ---------------------------------------------------------------------
# pieces
# ---------------------------------------------------------------------
def pill(p, box, grade, dx=0.0):
    """An evidence pill straddling the top edge of `box`, at its right."""
    f, l = GRADE[grade]
    return p.box(box.x + box.w - 0.60 - dx, box.y - 0.10, 0.50, 0.20, grade,
                 "", fill=f, line=l, title_size=6.4, title_color=l, halign=1,
                 valign=1, rounding=0.1, line_weight=0.008, ok_overlap=True)


def bar(p, x, y, w, text, color, h=0.40, size=10.5):
    return p.box(x, y, w, h, text, "", fill=color, line=color,
                 title_size=size, title_color=PAPER, halign=1, valign=1,
                 rounding=0.05)


def rect(p, x, y, w, h, title, body, f, l, **kw):
    kw.setdefault("title_size", 9.4)
    kw.setdefault("body_size", 7.8)
    kw.setdefault("line_weight", LINE)
    return d.node(p, "rect", x, y, w, h, title, body, fill=f, line=l, **kw)


def rows(p, xs, w, y, table, gap=0.08, key_size=7.4, body_size=8.0):
    """Attribute rows under a set of columns, levelled across columns."""
    for key, vals in table:
        boxes = [p.fitbox(x, y, w, key, v, pad=0.10, title_size=key_size,
                          body_size=body_size, title_color=MUTE,
                          body_color=INK, fill=ROW_F, line=ROW_L,
                          line_weight=0.008, rounding=0.04)
                 for x, v in zip(xs, vals)]
        tall = max(b.h for b in boxes)
        for b in boxes:
            b.h = tall
        y += tall + gap
    return y


def fit_h(title, body, w, title_size=9.4, body_size=7.8, pad=0.16):
    """The height a centred node needs for its text."""
    _, _, need = k.measure(title, body, w, title_size, body_size)
    return need + pad


# =====================================================================
# 1 · three places a FUSE daemon can live
# =====================================================================
def fig_delivery(doc):
    p = doc.page("1 Delivery models", 17, 9)
    cw, gap = 4.95, 0.30
    xs = [0.3, 0.3 + cw + gap, 0.3 + 2 * (cw + gap)]
    bar(p, xs[0], 0.3, cw, "A  ·  a sidecar inside the tenant's pod", RED_L)
    bar(p, xs[1], 0.3, cw, "B  ·  a CSI node plugin and a worker pod", PASS)
    bar(p, xs[2], 0.3, cw, "C  ·  a daemon the platform's runtime starts", DBX)
    for x, t in zip(xs, [
            "how flint delivered mounts until v1.45.0 — retired",
            "flint passthrough and flint lean today  ·  s3.csi.chert.us",
            "Databricks classic compute on AWS and Azure"]):
        p.text(x, 0.76, cw, t, size=8.2, color=MUTE, halign=1)

    top, zh = 1.12, 3.30

    # ---- A: the sidecar ------------------------------------------------
    x = xs[0]
    d.zone(p, x, top, cw, zh, "a Kubernetes node")
    wh = rect(p, x + 0.25, top + 0.50, 1.95, 0.72, "mutating webhook",
              "rewrites the pod spec at admission", d.OPER_F, d.OPER_L)
    p.text(x + 2.45, top + 0.52, 2.30,
           "the S3 credential is a Secret in the tenant's own namespace",
           size=7.8, color=RED_T, bold=True)
    pz = top + 1.55
    p.box(x + 0.25, pz, cw - 0.50, 1.55, "", "", fill=PAPER, line="#B9C0C8",
          dashed=True, rounding=0.1, line_weight=0.009)
    p.text(x + 0.40, pz + 0.08, cw - 0.8, "tenant pod", size=8.6,
           color="#5A646F", bold=True)
    app = rect(p, x + 0.45, pz + 0.45, 1.75, 0.92, "app container",
               "reads and writes /mnt/s3", d.CLIENT_F, d.CLIENT_L)
    sc = rect(p, x + 2.75, pz + 0.45, 1.95, 0.92, "FUSE sidecar",
              "PRIVILEGED, so its mount can propagate to the app",
              RED_F, RED_L, body_color=RED_T)
    p.arrow([(app.x + app.w, pz + 0.91), (sc.x, pz + 0.91)], color=FLOW,
            weight=W)
    d.flabel(p, (app.x + app.w + sc.x) / 2, pz + 0.70, "FUSE", w=0.5,
             size=7.2)
    p.arrow([(wh.x + 0.6, wh.y + wh.h), (wh.x + 0.6, pz)], color=CTL,
            weight=W, dashed=True)
    d.flabel(p, wh.x + 1.45, pz - 0.17, "injects it", CTL, w=0.95,
             size=7.2)

    # ---- B: CSI node plugin + worker ------------------------------------
    x = xs[1]
    d.zone(p, x, top, cw, zh, "a Kubernetes node")
    tp = rect(p, x + 0.25, top + 0.50, 2.05, 1.05, "tenant pod",
              "a csi: volume and nothing else — no sidecar, no credential",
              d.CLIENT_F, d.CLIENT_L)
    wk = rect(p, x + 2.65, top + 0.50, 2.05, 1.05, "worker pod",
              "mount-s3 or flint-sync, non-root, in flint-workers",
              d.WORK_F, d.WORK_L)
    p.arrow([(tp.x + tp.w, top + 1.03), (wk.x, top + 1.03)], color=FLOW,
            weight=W)
    np_ = rect(p, x + 0.25, top + 2.30, cw - 0.50, 0.80,
               "s3.csi.chert.us node plugin",
               "privileged, one per node. kubelet calls NodePublishVolume "
               "with the pod's ServiceAccount token",
               d.DOOR_F, d.DOOR_L)
    ax1, ax2 = tp.x + 0.55, wk.x + wk.w - 0.55
    p.arrow([(ax1, np_.y), (ax1, tp.y + tp.h)], color=CTL, weight=W)
    d.flabel(p, ax1 + 0.62, top + 1.92, "bind mount", CTL, w=1.0,
             size=7.2)
    p.arrow([(ax2, np_.y), (ax2, wk.y + wk.h)], color=ALT, weight=W)
    d.flabel(p, ax2 - 1.05, top + 1.92, "creates · hands the fd", ALT,
             w=1.55, size=7.2)

    # ---- C: the platform daemon -----------------------------------------
    x = xs[2]
    d.zone(p, x, top, cw, zh, "a VM Databricks provisioned in your account",
           color=DBX)
    p.box(x + 0.25, top + 0.50, cw - 0.50, 2.60, "", "", fill=PAPER,
          line="#B9C0C8", dashed=True, rounding=0.1, line_weight=0.009)
    p.text(x + 0.40, top + 0.58, cw - 0.8, "Databricks Runtime container",
           size=8.6, color="#5A646F", bold=True)
    ua = rect(p, x + 0.45, top + 1.00, 1.90, 0.78, "user A's code",
              "Linux user spark-6166…", d.CLIENT_F, d.CLIENT_L)
    ub = rect(p, x + 2.60, top + 1.00, 1.90, 0.78, "user B's code",
              "Linux user spark-5a9e…", d.CLIENT_F, d.CLIENT_L)
    fd = rect(p, x + 0.45, top + 2.22, 4.05, 0.72,
              "FUSE daemons, started by the runtime",
              "goofys-dbr → /dbfs  ·  wsfs → /Workspace  ·  unnamed → /Volumes",
              d.WORK_F, d.WORK_L)
    for b in (ua, ub):
        cx = b.x + b.w / 2
        p.arrow([(cx, b.y + b.h), (cx, fd.y)], color=FLOW, weight=W)
    d.flabel(p, x + 2.475, top + 2.00, "open()", w=0.7, size=7.2)
    pill(p, ua, "SEEN")
    pill(p, fd, "DOC")

    # ---- the attributes --------------------------------------------------
    y = rows(p, xs, cw, top + zh + 0.25, [
        ("WHERE THE PRIVILEGE IS", [
            "a privileged container inside every tenant pod",
            "one privileged node plugin per node; the worker that serves "
            "the mount runs non-root",
            "the platform's own runtime, on a machine the platform "
            "provisioned"]),
        ("HOW MANY MOUNTS", [
            "one per pod",
            "one per pod — one worker per published volume",
            "one per filesystem per node, shared by every user and every "
            "volume on it"]),
        ("HOW THE CALLER IS KNOWN", [
            "as the pod — but the tenant controls the container that "
            "serves the mount",
            "the ServiceAccount kubelet asserts at NodePublishVolume, "
            "checked against spec.consumers",
            "a Linux user the platform created for each user; how the "
            "daemon checks it is not public"]),
        ("WHERE THE CREDENTIAL IS", [
            "a Secret in the tenant's namespace",
            "short-lived keys from flint-s3-broker, to the worker only",
            "short-lived, path-scoped credentials from Unity Catalog"]),
    ])
    return p


# =====================================================================
# 2 · where Databricks compute runs — four shapes
# =====================================================================
def fig_shapes(doc):
    p = doc.page("2 Deployment shapes", 17, 10)
    cw, gap = 3.70, 0.25
    xs = [0.3 + i * (cw + gap) for i in range(4)]
    heads = [("classic  ·  AWS and Azure", "VMs in your cloud account"),
             ("classic  ·  Google Cloud, 2021–2025",
              "GKE in your project — deprecated 2025-03-17, now GCE VMs"),
             ("serverless", "Kubernetes in Databricks' own account"),
             ("on-premises", "no Databricks compute runs there")]
    for x, (h, s) in zip(xs, heads):
        bar(p, x, 0.3, cw, h, DBX, size=10)
        p.text(x, 0.76, cw, s, size=8.0, color=MUTE, halign=1)

    top, zh = 1.12, 3.20

    # ---- classic AWS / Azure ---------------------------------------------
    x = xs[0]
    d.zone(p, x, top, cw, zh, "one cluster node = one VM")
    host = rect(p, x + 0.22, top + 0.50, cw - 0.44, 0.62,
                "host  ·  host-subnet IP", "Databricks management traffic",
                d.OPER_F, d.OPER_L)
    pill(p, host, "DOC")
    p.box(x + 0.22, top + 1.32, cw - 0.44, 1.72, "", "", fill=PAPER,
          line="#B9C0C8", dashed=True, rounding=0.1, line_weight=0.009)
    p.text(x + 0.32, top + 1.38, cw - 0.64,
           "runtime container  ·  container-subnet IP", size=8.2,
           color="#5A646F", bold=True)
    sp = rect(p, x + 0.40, top + 1.74, cw - 0.80, 0.52,
              "Spark driver or executor", "", d.SERV_F, d.SERV_L)
    fu = rect(p, x + 0.40, top + 2.38, cw - 0.80, 0.52,
              "FUSE daemons", "wsfs, goofys-dbr, /Volumes", d.WORK_F,
              d.WORK_L)
    pill(p, fu, "DOC")

    # ---- classic GCP on GKE -----------------------------------------------
    x = xs[1]
    d.zone(p, x, top, cw, zh, "a GKE cluster in your project")
    sysp = rect(p, x + 0.22, top + 0.50, cw - 0.44, 0.80,
                "system node pool",
                "workspace-wide trusted services", d.OPER_F, d.OPER_L)
    pods = rect(p, x + 0.22, top + 1.55, cw - 0.44, 1.45,
                "cluster nodes as pods",
                "“the first fully container-based Databricks runtime on "
                "any cloud” — how /dbfs or /Volumes reached the pods was "
                "never described",
                d.SERV_F, d.SERV_L)
    pill(p, pods, "DOC")

    # ---- serverless ----------------------------------------------------------
    x = xs[2]
    d.zone(p, x, top, cw, zh, "a bare-metal cloud machine")
    kb = rect(p, x + 0.22, top + 0.50, cw - 0.44, 0.46,
              "kubelet  ·  containerd  ·  Kata shim", "", d.OPER_F, d.OPER_L,
              title_size=8.8)
    va = rect(p, x + 0.22, top + 1.10, 1.52, 0.80, "Kata VM",
              "customer A's pod", RED_F, RED_L, body_color=RED_T)
    vb = rect(p, x + 1.96, top + 1.10, 1.52, 0.80, "Kata VM",
              "customer B's pod", RED_F, RED_L, body_color=RED_T)
    pill(p, vb, "DOC")
    csi = rect(p, x + 0.22, top + 2.12, cw - 0.44, 0.90,
               "Databricks' own CSI driver",
               "SPDK vhost-user → a local-SSD block device inside the VM "
               "(Kata Direct Volume)", d.DOOR_F, d.DOOR_L)
    for b in (va, vb):
        cx = b.x + b.w / 2
        p.arrow([(cx, csi.y), (cx, b.y + b.h)], color=ALT, weight=W)

    # ---- on-premises -----------------------------------------------------------
    x = xs[3]
    d.zone(p, x, top, cw, zh, "your data center")
    st = d.node(p, "cylinder", x + 0.22, top + 0.50, cw - 0.44, 0.95,
                "S3-compatible storage",
                "MinIO, Pure, Qumulo, VAST …", fill=d.S3_F, line=d.S3_L,
                line_weight=LINE, cap=0.22, title_size=9.2, body_size=7.6)
    os_ = rect(p, x + 0.22, top + 1.62, cw - 0.44, 0.62,
               "OpenSharing endpoint",
               "Databricks' cloud compute reads through it",
               d.DOOR_F, d.DOOR_L)
    pill(p, os_, "DOC")
    yours = rect(p, x + 0.22, top + 2.42, cw - 0.44, 0.62,
                 "your own servers",
                 "fuse4dbricks: a third-party FUSE over the Files API",
                 d.CLIENT_F, d.CLIENT_L)
    pill(p, yours, "SEEN")
    p.arrow([(x + cw / 2, st.y + st.h), (x + cw / 2, os_.y)], color=DUR,
            weight=W)

    rows(p, xs, cw, top + zh + 0.25, [
        ("WHAT ORCHESTRATES IT", [
            "Databricks' cluster manager, in its control plane. No public "
            "source says these nodes run a kubelet",
            "Kubernetes (GKE), until Databricks moved classic compute on "
            "Google Cloud to plain GCE VMs",
            "Kubernetes: Kata micro-VMs for customer pods, NetworkPolicy "
            "and cloud security groups between tenants",
            "nothing on-prem. Databricks' cloud compute reaches your "
            "storage over OpenSharing (announced 2026-06-10)"]),
        ("WHERE THE FUSE DAEMONS RUN", [
            "inside the runtime container, started by the runtime "
            "(KB init script: wsfs /Workspace)",
            "not described publicly",
            "not described publicly; the talk's CSI driver serves local "
            "disk, not object storage",
            "no official client. A community FUSE mounts /Volumes over "
            "the Files API with personal tokens"]),
        ("THE SOURCE", [
            "Azure VNet-injection docs; Container Services docs; a KB "
            "article (since withdrawn)",
            "the Google Cloud launch blog (2021); a Databricks employee "
            "on the deprecation",
            "Databricks' KubeCon EU 2023 talk on hard multi-tenancy with "
            "Kata Containers",
            "the storage-ecosystem blog; fuse4dbricks on GitHub; the "
            "Private Cloud legal schedule"]),
    ])
    return p


# =====================================================================
# 3 · a classic Databricks node, and its three FUSE trees
# =====================================================================
def fig_node(doc):
    p = doc.page("3 Classic node", 17, 10)

    # ---- the control plane ------------------------------------------------
    cx0, cw0 = 0.3, 3.20
    d.zone(p, cx0, 0.3, cw0, 5.9, "Databricks control plane", color=DBX,
           sub="Databricks' account")
    cm = rect(p, cx0 + 0.22, 1.05, cw0 - 0.44, 1.05, "cluster manager",
              "provisions the VMs; creates each sandbox and proxies its "
              "traffic", d.OPER_F, d.OPER_L)
    pill(p, cm, "DOC")
    uc = rect(p, cx0 + 0.22, 2.55, cw0 - 0.44, 1.70, "Unity Catalog",
              "grants read and write on each volume; hands out short-lived "
              "storage credentials, seeing both who asks and what kind of "
              "compute asks", d.DOOR_F, d.DOOR_L)
    pill(p, uc, "DOC")
    ws = rect(p, cx0 + 0.22, 4.95, cw0 - 0.44, 1.20, "workspace",
              "notebooks and workspace files — what /Workspace shows",
              d.CLIENT_F, d.CLIENT_L)

    # ---- the VM ---------------------------------------------------------------
    vx, vw = 4.45, 8.05
    d.zone(p, vx, 0.3, vw, 5.9, "one cluster node — a VM in your cloud "
           "account", color=DBX, sub="host-subnet IP for management, "
           "container-subnet IP for Spark")
    # sandboxes
    sbx = vx + 0.25
    p.box(sbx, 1.05, 3.55, 2.30, "", "", fill=PAPER, line="#B9C0C8",
          dashed=True, rounding=0.1, line_weight=0.009)
    p.text(sbx + 0.12, 1.12, 3.3, "sandbox containers — standard access "
           "mode", size=8.2, color="#5A646F", bold=True)
    sa = rect(p, sbx + 0.20, 1.50, 3.15, 0.78, "user A: Python REPL, UDFs",
              "Linux user spark-6166…, low privilege, no metadata service",
              d.CLIENT_F, d.CLIENT_L, body_size=7.6)
    pill(p, sa, "SEEN")
    sb = rect(p, sbx + 0.20, 2.43, 3.15, 0.78, "user B: Python REPL, UDFs",
              "Linux user spark-5a9e…, network egress controlled",
              d.CLIENT_F, d.CLIENT_L, body_size=7.6)
    pill(p, sb, "DOC")

    # runtime container
    rx = vx + 4.20
    p.box(rx, 1.05, 3.60, 4.90, "", "", fill=PAPER, line="#B9C0C8",
          dashed=True, rounding=0.1, line_weight=0.009)
    p.text(rx + 0.12, 1.12, 3.4, "Databricks Runtime container",
           size=8.2, color="#5A646F", bold=True)
    spk = rect(p, rx + 0.20, 1.50, 3.20, 0.95, "Spark driver or executor",
               "reads tables and dbfs:/Volumes/… by URI — not through FUSE",
               d.SERV_F, d.SERV_L)
    pill(p, spk, "DOC")
    gd = rect(p, rx + 0.20, 2.75, 3.20, 0.80, "goofys-dbr  →  /dbfs",
              "internal fork of goofys; DBFS root and mounts, deprecated",
              d.WORK_F, d.WORK_L)
    pill(p, gd, "DOC")
    wsf = rect(p, rx + 0.20, 3.85, 3.20, 0.80, "wsfs  →  /Workspace",
               "/databricks/spark/scripts/fuse/wsfs",
               d.WORK_F, d.WORK_L)
    pill(p, wsf, "DOC")

    # the /Volumes daemon: placement unknown, so it is drawn dashed
    vd = p.box(sbx, 3.95, 3.55, 1.85, "the /Volumes FUSE daemon",
               "not named publicly, and neither is where it runs: one "
               "daemon checking each caller, or one per sandbox. Its "
               "limits — no append, no random write — match goofys'",
               fill="#FBF8FF", line=d.WORK_L, dashed=True,
               line_weight=LINE, title_size=9.4, body_size=7.6,
               body_color=SUB, halign=1, valign=1)
    pill(p, vd, "INFER")

    # ---- object storage -------------------------------------------------------
    sx, sw = 13.10, 3.60
    d.zone(p, sx, 0.3, sw, 5.9, "your object storage", color=d.S3_L,
           sub="S3, ADLS or GCS")
    mv = d.node(p, "cylinder", sx + 0.22, 4.35, sw - 0.44, 1.40,
                "volumes", "managed: Unity Catalog's location · external: "
                "a location you registered", fill=d.S3_F, line=d.S3_L,
                line_weight=LINE, cap=0.2, title_size=9.2, body_size=7.6)
    pill(p, mv, "DOC")
    tb = d.node(p, "cylinder", sx + 0.22, 1.05, sw - 0.44, 0.90,
                "Delta tables", "read and written by Spark directly",
                fill=d.S3_F, line=d.S3_L, line_weight=LINE, cap=0.22,
                title_size=9.2, body_size=7.6)
    dr = d.node(p, "cylinder", sx + 0.22, 2.75, sw - 0.44, 0.80,
                "DBFS root (legacy)", "off for new accounts",
                fill=d.CACHE_F, line=d.CACHE_L, line_weight=LINE, cap=0.22,
                title_size=9.2, body_size=7.6)

    # ---- arrows ---------------------------------------------------------------------
    # sandbox -> /Volumes daemon
    ax = sbx + 1.00
    p.arrow([(ax, sb.y + sb.h), (ax, vd.y)], color=FLOW, weight=WT)
    d.flabel(p, ax + 0.90, 3.58, "open() /Volumes/…", w=1.45, size=7.4)
    # /Volumes daemon <-> Unity Catalog
    uy = 4.10
    p.arrow([(sbx, uy), (cx0 + cw0 - 0.22, uy)], color=CTL, weight=W,
            dashed=True, begin_arrow=4)
    d.flabel(p, 3.975, uy - 0.60, "may this caller?", CTL, w=1.05, size=7.2)
    d.flabel(p, 3.975, uy + 0.24, "credential", CTL, w=1.05, size=7.2)
    # /Volumes daemon -> volumes: route under everything
    ly = 6.55
    p.arrow([(sbx + 2.70, vd.y + vd.h), (sbx + 2.70, ly), (15.0, ly),
             (15.0, mv.y + mv.h)], color=DUR, weight=WT)
    d.flabel(p, 10.3, ly + 0.24, "GET  ·  whole-object PUT — the path "
             "every /Volumes byte takes", DUR, w=4.4, size=7.4)
    # sandbox <-> Spark: Spark Connect
    p.arrow([(sa.x + sa.w, 1.89), (spk.x, 1.89)], color=FLOW, weight=W,
            begin_arrow=4)
    d.flabel(p, (sa.x + sa.w + spk.x) / 2, 2.72, "Spark\nConnect", w=0.62,
             size=7.0)
    # Spark -> tables, and -> volumes by URI
    sr = spk.x + spk.w
    p.arrow([(sr, 1.75), (sx + 0.22, 1.75)], color=DUR, weight=W)
    p.arrow([(sr, 2.20), (12.80, 2.20), (12.80, 5.05), (sx + 0.22, 5.05)],
            color=DUR, weight=W)
    # goofys -> DBFS root
    p.arrow([(sr, 3.15), (sx + 0.22, 3.15)], color=DUR, weight=W)
    # cluster manager -> the VM
    p.arrow([(cx0 + cw0 - 0.22, 1.55), (sbx, 1.55)], color=CTL, weight=W,
            dashed=True)
    d.flabel(p, 3.975, 1.30, "sandboxes", CTL, w=0.9, size=7.2)
    # wsfs -> workspace: route under the /Volumes daemon
    wy = 6.08
    p.arrow([(wsf.x + 0.6, wsf.y + wsf.h), (wsf.x + 0.6, wy),
             (cx0 + cw0 - 0.22, wy)], color=CTL, weight=W, dashed=True)
    d.flabel(p, 3.975, 5.84, "/Workspace", CTL, w=0.9, size=7.2)
    return p


# =====================================================================
# 4 · six doors to one volume, and what the FUSE door refuses
# =====================================================================
def fig_doors(doc):
    p = doc.page("4 Volume doors", 17, 10)
    p.text(0.3, 0.3, 8.6, "six doors to one volume — every one of them "
           "asks Unity Catalog", size=10.5, color=INK, bold=True)
    doors = [
        ("FUSE  ·  /Volumes/…",
         "/Volumes/<catalog>/<schema>/<volume>/… for Python open(), %sh "
         "and ML libraries — on Databricks compute only"),
        ("Spark and dbutils.fs",
         "by URI: dbfs:/Volumes/… — JVM file APIs cannot open the "
         "/Volumes path on standard compute"),
        ("Files API (REST)",
         "PUT and GET /api/2.0/fs/files/Volumes/…, up to 5 GiB a PUT; "
         "the SDKs, the CLI and Databricks Apps"),
        ("SQL connectors",
         "PUT INTO · GET · REMOVE through a driver; the notebook and SQL "
         "editor run LIST only"),
        ("the workspace UI", "upload up to 5 GB a file"),
        ("external engines",
         "temporary-volume-credentials: short-lived, scoped to the "
         "volume's path — Public Preview"),
    ]
    dx, dw, y = 0.3, 3.70, 0.78
    boxes = []
    for t, b in doors:
        s = p.fitbox(dx, y, dw, t, b, pad=0.12, title_size=8.8,
                     body_size=7.6, title_color=INK, body_color=SUB,
                     fill=d.CLIENT_F, line=d.CLIENT_L, line_weight=LINE,
                     rounding=0.06)
        boxes.append(s)
        y += s.h + 0.14
    pill(p, boxes[0], "DOC")
    bottom = y - 0.14

    ucx, ucw = 4.80, 1.90
    uc = p.box(ucx, 0.78, ucw, bottom - 0.78, "Unity Catalog",
               "USE CATALOG\nUSE SCHEMA\nREAD VOLUME\nWRITE VOLUME\n\n"
               "and EXTERNAL USE SCHEMA for an external engine",
               fill=d.DOOR_F, line=d.DOOR_L, line_weight=LINE,
               title_size=10, body_size=7.8, body_color=SUB, halign=1,
               valign=1)
    pill(p, uc, "DOC")
    vol = d.node(p, "cylinder", 7.45, 0.78 + (bottom - 0.78) / 2 - 0.85,
                 1.55, 1.70, "a volume",
                 "one cloud storage path, managed or external",
                 fill=d.S3_F, line=d.S3_L, line_weight=LINE, cap=0.16,
                 title_size=9.6, body_size=7.6)
    for s in boxes:
        yy = s.y + s.h / 2
        p.arrow([(dx + dw, yy), (ucx, yy)], color=FLOW, weight=W)
    vy = vol.y + vol.h / 2
    p.arrow([(ucx + ucw, vy), (vol.x, vy)], color=DUR, weight=WT)

    # ---- the contract ---------------------------------------------------------------
    cx, cw = 9.55, 7.15
    p.text(cx, 0.3, cw, "what the FUSE door refuses", size=10.5, color=INK,
           bold=True)
    kw, vw = 2.75, cw - 2.75 - 0.12
    y = 0.78
    table = [
        (True, "write sequentially, then close()",
         "the file becomes one object"),
        (False, "append to an existing file",
         "OSError errno 95, Operation not supported"),
        (False, "random write — seek, then write",
         "not supported: zip and Excel writers fail"),
        (False, "a sparse file",
         "not supported: copy with cp --sparse=never"),
        (False, "open /Volumes from JVM file APIs on standard compute",
         "not supported; Scala gets FUSE on dedicated compute, "
         "DBR 14.3 and above"),
        (False, "/dbfs FUSE on standard compute",
         "DBFS root and mounts have no FUSE there"),
    ]
    first = None
    for ok, what, result in table:
        f, l, t = (OK_F, OK_L, OK_T) if ok else (RED_F, RED_L, RED_T)
        a = p.fitbox(cx, y, kw, "", what, pad=0.10, body_size=8.2,
                     body_color=INK, fill=ROW_F, line=ROW_L,
                     line_weight=0.008, rounding=0.04, valign=1)
        b = p.fitbox(cx + kw + 0.12, y, vw, "", result, pad=0.10,
                     body_size=8.2, body_color=t, fill=f, line=l,
                     line_weight=0.009, rounding=0.04, valign=1)
        tall = max(a.h, b.h, 0.42)
        a.h = b.h = tall
        first = first or b
        y += tall + 0.10
    pill(p, first, "DOC")

    # the documented way round
    y += 0.20
    p.text(cx, y, cw, "the documented way round, for zip, Excel and "
           "appends", size=9.4, color=INK, bold=True)
    y += 0.36
    b1 = rect(p, cx, y, 2.30, 0.80, "write locally",
              "/local_disk0/tmp/report.xlsx", d.CLIENT_F, d.CLIENT_L,
              body_mono=False)
    d.node(p, "block_arrow", cx + 2.45, y + 0.12, 1.55, 0.56, "cp", "",
           fill=d.WORK_F, line=d.WORK_L, line_weight=LINE, title_size=9,
           head=0.30, shaft=0.60, ok_overlap=True)
    b2 = rect(p, cx + 4.15, y, 3.00, 0.80, "into the volume",
              "/Volumes/main/reports/out/report.xlsx", d.S3_F, d.S3_L)
    pill(p, b2, "DOC")
    p.text(cx, y + 0.95, cw,
           "and for appends that must survive across sessions, a KB "
           "article says to ask the account team for an NFS mount",
           size=8.0, color=SUB)
    return p


# =====================================================================
# 5 · flint on one Kubernetes node
# =====================================================================
def fig_flint(doc):
    p = doc.page("5 flint on Kubernetes", 17, 9)
    zt, zb = 0.75, 4.45

    plug = p.box(0.3, zt, 2.30, zb - zt, "s3.csi.chert.us node plugin",
                 "flint-system · privileged · one per node\n\n"
                 "kubelet calls NodePublishVolume with the pod's "
                 "ServiceAccount token\n\n"
                 "passthrough: opens /dev/fuse and calls mount(2) itself\n\n"
                 "creates the worker pod, then binds the mount or the tree "
                 "into the tenant pod",
                 fill=d.DOOR_F, line=d.DOOR_L, line_weight=0.014,
                 title_size=9.6, body_size=7.8, body_color=SUB, halign=1,
                 valign=1)

    d.zone(p, 3.10, zt, 3.70, zb - zt, "tenant namespace",
           sub="PodSecurity restricted")
    pp = rect(p, 3.30, 1.40, 3.30, 1.20, "passthrough pod",
              "/mnt/s3 is a FUSE mount: every open, read and write is an "
              "S3 request", d.CLIENT_F, d.CLIENT_L)
    lp = rect(p, 3.30, 2.95, 3.30, 1.25, "lean pod",
              "/workspace is plain local disk: append, rename, mmap, "
              "sqlite, git", d.CLIENT_F, d.CLIENT_L)

    d.zone(p, 7.50, zt, 3.70, zb - zt, "flint-workers",
           sub="non-root, all capabilities dropped")
    mw = rect(p, 7.70, 1.40, 3.30, 1.20, "mount-s3 worker",
              "an unchanged mount-s3, serving the /dev/fuse descriptor "
              "it was handed", d.WORK_F, d.WORK_L)
    lw = rect(p, 7.70, 2.95, 3.30, 1.25, "flint-sync worker",
              "checks the tree out before the pod starts; publishes "
              "boundaries; drains on delete", d.WORK_F, d.WORK_L)

    d.zone(p, 12.10, zt, 3.70, zb - zt, "S3-compatible bucket",
           color=d.S3_L, sub="AWS S3, MinIO, Ozone …")
    ob = d.node(p, "cylinder", 12.30, 1.40, 3.30, 1.20, "<prefix>/<key>",
                "passthrough: the objects are the files; flint writes "
                "nothing else", fill=d.S3_F, line=d.S3_L, line_weight=LINE,
                cap=0.2, title_size=9.4, body_size=7.8)
    fo = d.node(p, "cylinder", 12.30, 2.80, 3.30, 0.75, "files/<path>",
                "lean: whole objects, CRC-64 each", fill=d.S3_F,
                line=d.S3_L, line_weight=LINE, cap=0.24, title_size=9.2,
                body_size=7.6)
    cur = d.node(p, "cylinder", 12.30, 3.62, 3.30, 0.72,
                 ".flint/lean/current", "one CAS installs a whole boundary",
                 fill=d.S3_HOT_F, line=d.S3_L, line_weight=0.015, cap=0.24,
                 title_size=9.2, body_size=7.6)

    # plugin -> pods
    for yy in (2.00, 3.58):
        p.arrow([(2.60, yy), (3.30, yy)], color=CTL, weight=W)
    d.flabel(p, 2.95, 1.78, "bind", CTL, w=0.5, size=7.2)
    d.flabel(p, 2.95, 3.36, "bind", CTL, w=0.5, size=7.2)
    # pods -> workers
    p.arrow([(6.60, 2.00), (7.70, 2.00)], color=FLOW, weight=WT)
    d.flabel(p, 7.15, 1.78, "FUSE", w=0.6, size=7.2)
    p.arrow([(6.60, 3.58), (7.70, 3.58)], color=FLOW, weight=WT,
            dashed=True, begin_arrow=4)
    d.flabel(p, 7.15, 3.36, "same dir", w=0.7, size=7.2)
    # workers -> bucket
    p.arrow([(11.00, 2.00), (12.30, 2.00)], color=DUR, weight=WT)
    d.flabel(p, 11.65, 1.78, "GET · PUT", DUR, w=0.8, size=7.2)
    p.arrow([(11.00, 3.18), (12.30, 3.18)], color=DUR, weight=WT)
    d.flabel(p, 11.65, 2.96, "upload", DUR, w=0.8, size=7.2)
    p.arrow([(11.00, 3.98), (12.30, 3.98)], color=DUR, weight=WT)
    d.flabel(p, 11.65, 4.20, "then CAS", DUR, w=0.8, size=7.2)
    # plugin -> workers, over the top and under the bottom
    p.arrow([(1.45, zt), (1.45, 0.45), (10.40, 0.45), (10.40, mw.y)],
            color=ALT, weight=W)
    d.flabel(p, 5.30, 0.21, "creates the worker  ·  hands it /dev/fuse over "
             "SCM_RIGHTS", ALT, w=4.0, size=7.4)
    p.arrow([(1.45, zb), (1.45, 4.75), (9.35, 4.75), (9.35, lw.y + lw.h)],
            color=CTL, weight=W)
    d.flabel(p, 5.30, 4.99, "creates the worker  ·  it checks the tree out "
             "before any container starts", CTL, w=4.8, size=7.4)

    br = p.box(7.50, 5.35, 3.70, 0.95, "flint-s3-broker  ·  one Deployment",
               "TokenReview · a registration nonce the pod cannot mint · "
               "spec.consumers → short-lived keys, to workers only",
               fill=d.OPER_F, line=d.OPER_L, line_weight=LINE,
               title_size=9.2, body_size=7.6, body_color=SUB, halign=1,
               valign=1)
    p.arrow([(10.60, br.y), (10.60, lw.y + lw.h)], color=CTL, weight=W)
    d.flabel(p, 10.02, 4.97, "keys", CTL, w=0.5, size=7.2)
    return p


# =====================================================================
# 6 · where a write becomes visible, and where it becomes durable
# =====================================================================
def fig_writes(doc):
    p = doc.page("6 Write path", 17, 9)
    strips = [
        ("Databricks /Volumes — a file becomes an object at close()", DBX,
         ("user code", "write() … write() … close()"),
         ("the /Volumes FUSE daemon",
          "sequential writes; the object is written whole"),
         ("an object in your storage", "readers see the new file once it "
          "is written"),
         ("FUSE", "PUT"),
         (False, "refused", "append (errno 95) · random write · sparse "
          "files — the docs say: write under /local_disk0, then cp"),
         ["DOC", "INFER"]),
        ("flint passthrough — the same contract, served by mount-s3", PASS,
         ("tenant pod", "write() … write() … close()"),
         ("mount-s3 worker", "completes the upload on close or fsync"),
         ("<prefix>/<key>", "old or new, never torn"),
         ("FUSE", "PUT"),
         (False, "refused", "no rename, no append, no in-place "
          "modification at any setting"),
         []),
        ("flint lean — visible at once, durable at the next boundary", LEAN,
         ("agent pod", "write, append, rename, mmap, sqlite, git — on local "
          "disk"),
         ("flint-sync worker", "at its cadence, or when the agent writes "
          ".flint/publish"),
         ("files/<path> + the pointer", "upload the changed files, then ONE "
          "CAS cites them all"),
         ("same dir", "boundary"),
         (True, "nothing refused",
          "the cost: RPO is the last boundary; the tree must fit the "
          "node's disk; about 250k files"),
         []),
    ]
    y = 0.30
    for title, color, a, b, c, (la, lb), (ok, rt, rb), pills in strips:
        p.box(0.3, y, 16.4, 0.36, "", "", fill=color, line=color,
              rounding=0.04)
        p.text(0.42, y + 0.045, 16.1, title, size=9.8, color=PAPER,
               bold=True)
        yy = y + 0.58
        h = 1.0
        ba = rect(p, 0.3, yy, 2.90, h, a[0], a[1], d.CLIENT_F, d.CLIENT_L)
        bb = rect(p, 4.10, yy, 3.30, h, b[0], b[1], d.WORK_F, d.WORK_L)
        bc = d.node(p, "cylinder", 8.30, yy, 3.30, h, c[0], c[1],
                    fill=d.S3_F, line=d.S3_L, line_weight=LINE, cap=0.2,
                    title_size=9.4, body_size=7.8)
        f, l, t = (OK_F, OK_L, OK_T) if ok else (RED_F, RED_L, RED_T)
        br = rect(p, 12.20, yy, 4.50, h, rt, rb, f, l, body_color=t,
                  title_color=l)
        my = yy + h / 2
        p.arrow([(3.20, my), (4.10, my)], color=FLOW, weight=WT,
                dashed=(la == "same dir"), begin_arrow=4 if la == "same dir"
                else 0)
        d.flabel(p, 3.65, my - 0.22, la, w=0.85, size=7.2)
        p.arrow([(7.40, my), (8.30, my)], color=DUR, weight=WT)
        d.flabel(p, 7.85, my - 0.22, lb, DUR, w=0.85, size=7.2)
        for i, g in enumerate(pills):
            pill(p, [ba, bb][i], g)
        y = yy + h + 0.30

    p.box(0.3, y, 16.4, 0.72, "",
          "", fill="#F3EEFB", line=d.WORK_L, line_weight=LINE, rounding=0.06)
    p.text(0.45, y + 0.08, 16.1,
           "Databricks documents “write to /local_disk0, then copy to "
           "/Volumes” as the answer for zip, Excel and appends. flint lean "
           "is that answer done automatically, for a whole tree — with a "
           "manifest that makes the copy one transaction, a lease that "
           "says who may write, and the loser's bytes kept on a conflict.",
           size=8.8, color=INK)
    return p


# =====================================================================
# 7 · why one shared mount is safe there and not on a shared k8s node
# =====================================================================
def fig_identity(doc):
    p = doc.page("7 Identity", 17, 9)
    pw = 7.95
    lx, rx = 0.3, 0.3 + pw + 0.5

    # ---- Databricks ---------------------------------------------------
    d.zone(p, lx, 0.3, pw, 5.35, "a Databricks node", color=DBX,
           sub="the platform creates every user and every sandbox")
    ua = rect(p, lx + 0.35, 1.05, 3.40, 0.85, "user A's code",
              "Linux user spark-6166… — created by Databricks",
              d.CLIENT_F, d.CLIENT_L)
    pill(p, ua, "SEEN")
    ub = rect(p, lx + 4.20, 1.05, 3.40, 0.85, "user B's code",
              "Linux user spark-5a9e… — created by Databricks",
              d.CLIENT_F, d.CLIENT_L)
    kf = rect(p, lx + 0.35, 2.45, pw - 0.70, 0.70, "the kernel's FUSE "
              "request", "carries the caller's uid, gid and pid",
              d.CACHE_F, d.CACHE_L)
    dm = rect(p, lx + 0.35, 3.70, pw - 0.70, 0.95, "a /Volumes daemon",
              "uid → the user it was created for → that user's Unity "
              "Catalog grants", d.WORK_F, d.WORK_L)
    pill(p, dm, "INFER")
    for b in (ua, ub):
        cx = b.x + b.w / 2
        p.arrow([(cx, b.y + b.h), (cx, kf.y)], color=FLOW, weight=W)
    p.arrow([(lx + pw / 2, kf.y + kf.h), (lx + pw / 2, dm.y)], color=FLOW,
            weight=W)
    d.flabel(p, lx + pw / 2, 2.18, "open() on /Volumes", w=1.6, size=7.4)
    p.box(lx + 0.35, 4.85, pw - 0.70, 0.55, "", "", fill=OK_F, line=OK_L,
          line_weight=0.010, rounding=0.05)
    p.text(lx + 0.45, 4.93, pw - 0.90, "a uid names a person, because "
           "nobody but the platform can pick one", size=8.8, color=OK_T,
           bold=True, halign=1)

    # ---- Kubernetes -----------------------------------------------------
    d.zone(p, rx, 0.3, pw, 5.35, "a shared Kubernetes node",
           sub="every pod picks its own uid")
    t1 = rect(p, rx + 0.35, 1.05, 3.40, 0.85, "tenant 1's pod",
              "securityContext.runAsUser: 1000", d.CLIENT_F, d.CLIENT_L)
    t2 = rect(p, rx + 4.20, 1.05, 3.40, 0.85, "tenant 2's pod",
              "securityContext.runAsUser: 1000", d.CLIENT_F, d.CLIENT_L)
    kf2 = rect(p, rx + 0.35, 2.45, pw - 0.70, 0.70, "the kernel's FUSE "
               "request", "uid 1000 — and uid 1000", d.CACHE_F, d.CACHE_L)
    sd = rect(p, rx + 0.35, 3.70, pw - 0.70, 0.95, "one shared node daemon",
              "cannot tell the two tenants apart", RED_F, RED_L,
              body_color=RED_T, title_color=RED_L)
    for b in (t1, t2):
        cx = b.x + b.w / 2
        p.arrow([(cx, b.y + b.h), (cx, kf2.y)], color=FLOW, weight=W)
    p.arrow([(rx + pw / 2, kf2.y + kf2.h), (rx + pw / 2, sd.y)], color=FLOW,
            weight=W)
    d.flabel(p, rx + pw / 2, 2.18, "open() on the mount", w=1.6, size=7.4)
    p.box(rx + 0.35, 4.85, pw - 0.70, 0.55, "", "", fill=RED_F, line=RED_L,
          line_weight=0.010, rounding=0.05)
    p.text(rx + 0.45, 4.93, pw - 0.90, "a uid names nobody: two tenants "
           "chose the same one", size=8.8, color=RED_T, bold=True,
           halign=1)

    # ---- flint's answer ----------------------------------------------------
    y = 5.95
    p.box(0.3, y, 2 * pw + 0.5, 1.05, "", "", fill=d.DOOR_F, line=d.DOOR_L,
          line_weight=0.014, rounding=0.06)
    p.text(0.45, y + 0.08, 2 * pw + 0.2, "so flint mounts once PER POD, and "
           "identity is the ServiceAccount, not the uid", size=10,
           color=INK, bold=True)
    p.text(0.45, y + 0.36, 2 * pw + 0.2,
           "kubelet hands NodePublishVolume a token bound to this pod; the "
           "plugin checks it against the resource's spec.consumers and "
           "binds the mount into that pod alone. NodePublishVolume never "
           "sees the pod's securityContext, so inside a pod a passthrough "
           "mount is one uid — per pod, never per user.",
           size=8.6, color=SUB)
    return p


FIGS = [("01-delivery-models", fig_delivery),
        ("02-deployment-shapes", fig_shapes),
        ("03-classic-node", fig_node),
        ("04-volume-doors", fig_doors),
        ("05-flint-on-kubernetes", fig_flint),
        ("06-write-path", fig_writes),
        ("07-identity", fig_identity)]


def main():
    args = [a for a in sys.argv[1:] if not a.startswith("--")]
    outdir = args[0] if args else _here
    doc = k.Document(
        title="flint vs Databricks — POSIX over object storage",
        creator="flint",
        description="Seven figures: where a FUSE daemon can live; where "
                    "Databricks compute runs; a classic Databricks node; "
                    "the doors to a volume; flint on Kubernetes; the write "
                    "path; and why a shared mount is safe there and not "
                    "on a shared Kubernetes node.")
    for _, fn in FIGS:
        fn(doc).trim(margin=0.3)

    problems = (doc.check() + doc.overlap_report()
                + doc.label_overlap_report() + doc.label_on_line_report())
    for pg in doc.pages:
        problems += (d.ink_collision_report(pg) + d.edge_strike_report(pg)
                     + d.arrow_through_text_report(pg))
    for msg in problems:
        print(msg)
    print("%d shapes, %d pages, %d problems"
          % (sum(len(pg.shapes) for pg in doc.pages), len(doc.pages),
             len(problems)))

    out = os.path.join(outdir, "flint-vs-databricks.vsdx")
    doc.save(out)
    print("wrote", out)

    if "--svg" in sys.argv:
        ddir = os.path.join(outdir, "diagrams")
        os.makedirs(ddir, exist_ok=True)
        for (stem, _), pg in zip(FIGS, doc.pages):
            path = os.path.join(ddir, stem + ".svg")
            with open(path, "w") as fh:
                fh.write(k._page_svg(pg))
            print("wrote", path)

    if "--emf" in sys.argv:
        for path in emf.save_emf(doc, os.path.join(outdir,
                                                   "flint-vs-databricks")):
            problems += ["%s: %s" % (path, m) for m in emf.validate_emf(path)]
            print("wrote", path)

    if "--pdf" in sys.argv:
        chrome = d.find_chrome()
        if not chrome:
            raise SystemExit("no Chrome/Chromium found")
        svgs = doc.save_svg(os.path.join(outdir, "preview"))
        pdf = d.render_pdf(doc, svgs, os.path.join(
            outdir, "flint-vs-databricks-figures.pdf"), chrome)
        for msg in d.check_pdf(doc, pdf):
            print("  PDF CHECK:", msg)
            problems.append(msg)
        for s in svgs:
            if os.path.exists(s):
                os.remove(s)
        print("wrote", pdf)
    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(main())
