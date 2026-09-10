#!/usr/bin/env python3
"""Build `flint-lite-dataflow.vsdx` — ONE page, the data flow, symbols.

The lite companion to `forge/forge-dataflow.py`: the components that
matter, the arrows between them, and a label on each arrow rather than a
paragraph inside each box.

The picture has one claim to make, and everything else serves it: ONE
process holds the tree, so N pods in N clusters see one coherent
filesystem — and that is also why it is one pod. The PVC beside it is a
cache; the bucket is the copy.

Run:  python3 lite-dataflow.py [outdir] [--preview] [--pdf] [--emf]
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
    ("NFSv4.2", "one protocol, one long-lived TCP flow, and a client "
                "already in every node's kernel"),
    ("close-to-open", "NFS's coherence rule: what a writer closed, the next "
                      "opener sees"),
    ("AUTH_SYS", "the flavour port 2049 speaks: the client asserts its own "
                 "uid, and nothing checks it"),
    ("nconnect", "how many TCP connections one mount opens. Below 2 there is "
                 "no trunking to spread"),

    ("NFS4ERR_DELAY", "the server telling a caller to come back — how a cold "
                      "read parks instead of hanging"),
    ("epoch cell", "the object that says who the hub is. A second hub loses "
                   "the CAS and is fenced"),
    ("flush cadence", "how often closed generations publish. It is the "
                      "recovery point, stated as a number"),
    ("hydrate", "fetching an evicted file back from the bucket on first "
                "touch, into its own inode"),

    ("evict · watermark", "cold files leave the PVC at a disk watermark — a "
                          "marker is written before the truncate"),
    (".flint/manifest", "the DR document: the metadata checkpoint a restore "
                        "rebuilds the tree from"),
    ("rpoClean", "/status answering \u201ccan the bucket rebuild this volume "
                 "right now?\u201d — ask it before deleting a PVC"),
    ("FlintShare", "the custom resource one lite volume is declared as. One "
                   "volume, one hub, N of them per project"),

    ("flint-hub-gateway", "the proxy that fronts every hub's file API: one "
                          "endpoint, one inbound credential"),
    ("headless Service", "a Service with no ClusterIP — addressable inside "
                         "the cluster, routable from nowhere"),
    ("derived token", "a per-share credential computed from one root key by "
                      "HMAC, so nothing has to store or fan out 3000 of them"),
    ("project id · volume id", "the labels a share is addressed by through "
                               "the gateway, instead of by its Service name"),

    ("Recreate", "the Deployment strategy that stops the old pod before it "
                 "starts the new one"),
    ("RWO attach fence", "the CSI attachment that makes two hub pods on one "
                         "PVC impossible, not merely unlikely"),
    ("nfs.nfs4_unique_id", "the client-side kernel parameter that gives a "
                           "node a client identity a rename survives"),
    ("capture · generation", "a mutation-complete record of what changed, and "
                             "the closed unit a flush publishes"),
]


def build():
    doc = k.Document(
        title="flint-lite — data flow",
        creator="flint",
        description="One page: the components that matter, the data flow "
                    "between them, and a glossary.")
    p = doc.page("Data flow", 24.5, 14.0)

    d.header(p, "lite", "Data flow",
             "One process holds the tree, so N pods in N clusters see one "
             "filesystem. The file API can be fronted by one door; the mount "
             "cannot.", w=23.0)

    spine = 3.91          # hub A's NFS door
    http = 5.575          # hub A's file-API door
    spine2 = 8.65         # hub #2's doors

    # ---- boundaries ------------------------------------------------------
    d.zone(p, 0.5, 1.95, 4.9, 3.30, "consumer cluster",
           sub="installs nothing from flint")
    d.zone(p, 0.5, 5.45, 4.9, 1.45, "consumer clusters B, C, \u2026",
           sub="the same wire — a hub does not count clusters, it sees NAMES")
    d.zone(p, 6.55, 2.15, 9.25, 7.80, "flint hub cluster",
           sub="one volume, one hub, N per project  ·  ONE shared door for "
               "the file API  ·  NONE for NFS")
    d.container(p, 0.75, 2.47, 4.4, 2.50, "node")
    d.container(p, 8.85, 2.70, 6.75, 3.75,
                "hub #1 — prefix vol1/")
    d.container(p, 8.85, 7.85, 6.75, 1.55,
                "hub #2 — prefix vol2/  ·  a SECOND pod, a SECOND NFS "
                "Service, a SECOND mount at the consumer")
    d.node(p, "cloud", 17.20, 2.40, 6.35, 7.55, "", "", fill=d.CLOUD_F,
           line=d.CLOUD_L, line_weight=0.012)
    p.text(17.40, 2.85, 5.95, "S3-compatible object storage", size=10.5,
           color="#8A6A1E", bold=True, halign=1)

    # ---- the consumers ---------------------------------------------------
    for i in range(3):
        x = 0.90 + i * 1.40
        d.node(p, "rect", x, 2.56, 1.30, 0.58, "agent pod", "",
               fill=d.CLIENT_F, line=d.CLIENT_L, line_weight=0.012,
               title_size=9.0)
        p.arrow([(x + 0.65, 3.14), (x + 0.65, 3.45)], color=FLOW, weight=W)

    d.node(p, "rect", 0.90, 3.45, 4.10, 0.92,
           "kernel NFS client — one mount per node",
           "shared by every pod on it, with its page cache",
           fill=d.CLIENT_F, line=d.CLIENT_L, line_weight=0.014,
           title_size=9.6, body_size=7.4)
    p.text(0.95, 4.48, 4.05, "PV:  nfsvers=4.1 · hard · nconnect=4",
           size=7.4, color=SUB, mono=True)
    p.text(0.95, 4.66, 4.05, "hostname = <cluster>-<node>", size=7.4,
           color=SUB, mono=True)

    d.node(p, "rect", 0.90, 5.98, 4.10, 0.70,
           "kernel NFS client — another cluster",
           "same protocol · same tree · same locks",
           fill=d.CLIENT_F, line=d.CLIENT_L, line_weight=0.013,
           title_size=9.2, body_size=7.2)

    d.node(p, "document", 3.30, 7.15, 1.00, 0.95, "", "", fill=d.CLIENT_F,
           line=d.CLIENT_L, line_weight=0.013)
    p.text(0.55, 7.28, 2.55, "app / control plane", size=10, color=INK,
           bold=True)
    p.text(0.55, 7.50, 2.60, "ONE endpoint  ·  ONE credential", size=7.8,
           color=SUB)
    p.text(0.55, 7.70, 2.60, "for a caller that cannot mount", size=7.8,
           color=SUB)

    # ---- the shared door, and the one it is not --------------------------
    d.node(p, "rect", 6.75, 4.60, 1.90, 3.20, "flint-hub-gateway",
           "ONE door in front of EVERY hub's file API.\n\n"
           "project id → its share · wakes it if parked · proxies exactly "
           "six file routes\n\n"
           "IN: one shared bearer. OUT: a per-share token DERIVED by HMAC "
           "from one root key — it stores none, and holds no Secrets RBAC.\n\n"
           "No route it has can produce /status.",
           fill="#FFF6E2", line="#B0862A", line_weight=0.016,
           title_size=10.2, body_size=7.1)

    # ---- hub #1 ----------------------------------------------------------
    d.node(p, "hexagon", 9.00, 3.21, 2.70, 1.40, ":2049   NFSv4.2",
           "locks · close-to-open · rename\nthe data plane",
           fill=d.DOOR_F, line=d.DOOR_L, line_weight=0.014, notch=0.16,
           title_size=10.0, body_size=7.4)
    d.node(p, "hexagon", 9.00, 4.95, 2.70, 1.25, ":8080   status + files",
           "ClusterIP only · never on the Service",
           fill="#FFF6E2", line="#B0862A", line_weight=0.014, notch=0.15,
           title_size=9.4, body_size=7.2)
    d.node(p, "box3d", 12.15, 3.25, 3.30, 3.00,
           "flint-pnfs-mds  ·  mode: standalone",
           "ONE process holds the tree: every lease, every lock, every open. "
           "Both doors are NFS compounds dispatched inside it — never two "
           "readers of one directory.",
           fill=d.SERV_F, line=d.SERV_L, line_weight=0.015, depth=0.1,
           body_size=7.3)

    d.node(p, "datastore", 12.15, 6.75, 3.30, 0.82, "PVC — the working set",
           "a CACHE. Losing it is a rebuild, not a loss",
           fill=d.CACHE_F, line=d.CACHE_L, title_size=9.5, line_weight=0.013,
           body_size=7.4)
    d.node(p, "hexagon", 8.85, 6.75, 3.00, 0.82, "operator",
           "FlintShare → the four objects  ·  suspend idle  ·  scale to zero",
           fill=d.OPER_F, line=d.OPER_L, line_weight=0.014, notch=0.13,
           title_size=9.6, body_size=7.0)

    # ---- hub #2 ----------------------------------------------------------
    d.node(p, "hexagon", 9.00, 8.15, 3.00, 1.00, ":2049  +  :8080",
           "its OWN two doors — a different Service, a different address",
           fill=d.DOOR_F, line=d.DOOR_L, line_weight=0.013, notch=0.15,
           title_size=9.6, body_size=7.2)
    d.node(p, "rect", 12.30, 8.15, 3.15, 1.00,
           "flint-pnfs-mds  ·  its own PVC",
           "another process, another tree. Nothing is shared with hub #1 — "
           "not a lock, not an open, not a byte",
           fill=d.SERV_F, line=d.SERV_L, line_weight=0.013, title_size=9.6,
           body_size=7.2)

    # ---- the stores ------------------------------------------------------
    d.node(p, "cylinder", 18.20, 3.30, 4.35, 0.85, "files",
           "<prefix>/<path> — whole-file generations, untorn. Published on "
           "the flush cadence, hydrated on first touch",
           fill=d.S3_F, line=d.S3_L, line_weight=0.013, cap=0.28,
           body_size=7.4)
    d.node(p, "cylinder", 18.20, 4.50, 4.35, 0.85, ".flint/manifest",
           "the DR document, written at every flush barrier — what a restore "
           "rebuilds from",
           fill=d.S3_HOT_F, line=d.S3_L, line_weight=0.015, cap=0.28,
           body_size=7.4)
    d.node(p, "cylinder", 18.20, 5.70, 4.35, 0.85, ".flint/epoch",
           "the single-writer lease — claimed, renewed, and re-verified "
           "before every publish",
           fill=d.S3_F, line=d.S3_L, line_weight=0.013, cap=0.28,
           body_size=7.4)
    p.text(18.20, 6.95, 4.35, "\u2026all of it under hub #1's prefix",
           size=7.4, color=MUTE, halign=1)
    d.node(p, "cylinder", 18.20, 8.20, 4.35, 0.90, "vol2/",
           "its own files, manifest and epoch — DISJOINT. One hub per "
           "prefix; nothing is shared",
           fill=d.S3_F, line=d.S3_L, line_weight=0.013, cap=0.28,
           body_size=7.4)

    # ---- the flows -------------------------------------------------------
    # 1 · the wire. Live, both ways, and it is every byte
    p.arrow([(5.00, spine), (9.00, spine)], color=FLOW, weight=WT,
            begin_arrow=k.ARROW_FILLED)
    d.flabel(p, 5.97, spine - 0.50, "NFSv4.2 · :2049", w=1.7)
    d.flabel(p, 5.97, spine - 0.22, "every read and write", w=2.2, size=7.2)

    # 1b · another cluster, onto the same door
    p.arrow([(5.00, 6.33), (5.95, 6.33), (5.95, 4.45), (9.00, 4.45)],
            color=FLOW, weight=WT)

    # 1c · a second volume is a SECOND MOUNT — nothing multiplexes NFS
    p.arrow([(5.00, 4.25), (6.35, 4.25), (6.35, 8.90), (9.00, 8.90)],
            color=FLOW, weight=WT)
    d.flabel(p, 5.97, 4.05, "a SECOND mount", w=1.5, size=7.2)

    # 2 · the caller that cannot mount reaches ONE endpoint
    p.arrow([(4.30, 7.625), (6.75, 7.625)], color=ALT, weight=W)
    d.flabel(p, 5.45, 7.42, "GET / PUT a path  ·  Bearer", ALT, w=1.6,
             size=7.4)

    # 3 · and the gateway fans out to every hub's file API
    p.arrow([(8.67, http), (9.00, http)], color=ALT, weight=W)
    p.arrow([(8.60, 7.80), (8.60, 8.40), (9.00, 8.40)], color=ALT, weight=W)

    # 4 · the tree, and the disk under it
    p.arrow([(11.70, spine), (12.15, spine)], color=FLOW, weight=WT)
    p.arrow([(11.70, http), (12.15, http)], color=ALT, weight=W)
    p.arrow([(13.80, 6.25), (13.80, 6.75)], color=FLOW, weight=W,
            begin_arrow=k.ARROW_FILLED)
    p.arrow([(12.00, spine2), (12.30, spine2)], color=FLOW, weight=W)

    # 5 · the bucket — each hub is the only writer of its own prefix
    p.arrow([(15.47, 3.725), (18.20, 3.725)], color=DUR, weight=WT,
            begin_arrow=k.ARROW_FILLED)
    d.flabel(p, 16.55, 3.51, "publish · hydrate", DUR, w=1.30, size=7.4)
    p.arrow([(15.47, 4.925), (18.20, 4.925)], color=DUR, weight=W,
            begin_arrow=k.ARROW_FILLED)
    d.flabel(p, 16.55, 4.71, "checkpoint", DUR, w=1.10, size=7.4)
    p.arrow([(15.47, 6.125), (18.20, 6.125)], color=DUR, weight=W,
            begin_arrow=k.ARROW_FILLED)
    d.flabel(p, 16.55, 5.91, "claim · renew", DUR, w=1.20, size=7.4)
    p.arrow([(15.47, spine2), (18.20, spine2)], color=DUR, weight=WT,
            begin_arrow=k.ARROW_FILLED)

    # 6 · the control plane, which carries no file
    p.arrow([(10.00, 6.75), (10.00, 6.20)], color=CTL, weight=W, dashed=True)
    d.flabel(p, 9.42, 6.32, "POST /wake", CTL, w=1.05, size=7.2)
    p.arrow([(11.10, 6.20), (11.10, 6.75)], color=CTL, weight=W, dashed=True)
    d.flabel(p, 11.95, 6.32, "GET /status · rpoClean", CTL, w=1.25, size=7.2)

    # ---- the notes the picture cannot carry ------------------------------
    y = 10.35
    y = d.notes(p, 0.55, y, 23.0, [
        "THE TWO DOORS FRONT DIFFERENTLY, and that is lite's real asymmetry. "
        "The FILE API can be multiplexed: flint-hub-gateway resolves a "
        "project id to its share, wakes it if it is parked, and proxies six "
        "file routes — so a fleet reaches three thousand shares through ONE "
        "endpoint and ONE credential, exactly as forge's door fronts every "
        "repository. THE MOUNT CANNOT. NFS needs a routable address per hub "
        "(spec.service.advertiseAddress → status.address), so a second "
        "volume is a second pod, a second Service and a second mount at the "
        "consumer. Nothing multiplexes NFS.",

        "AND THE GATEWAY EXISTS BECAUSE PER-SHARE EXPOSURE DOES NOT SCALE. A "
        "hub's file API answers at a HEADLESS in-cluster Service the "
        "operator deliberately never makes routable: 3000 ClusterIPs "
        "exhaust a GKE-default /20 at about 2048 shares — and once the "
        "allocator is dry, a new share's CONSUMER Service cannot be created "
        "either, so one tenant's share count would stop every other tenant "
        "mounting anything — while NetworkPolicy cannot be the guard (off by "
        "default, hubNamespaces defaults to [], some CNIs ignore every rule "
        "in silence). One gateway can be guarded; three thousand cannot.",

        "ONE KEY, NOT THREE THOUSAND SECRETS. Inbound is one shared bearer, "
        "re-read every 10 s so rotation is a Secret edit with no restart. "
        "Outbound is per-share and DERIVED — HMAC-SHA256(root, endpoint : "
        "bucket : keyPrefix : version) — so the gateway stores no hub "
        "credential and has no Secrets RBAC at all; the alternative was `get "
        "secrets` in every tenant namespace, where those tenants' S3 keys "
        "live. The caller's own credential is never forwarded upstream, and "
        "no route the gateway has can produce /status.",

        "ONE PROCESS IS THE COHERENCE AUTHORITY, and that is why each hub is "
        "one pod. Every lease, lock and open lives in it, so N pods across N "
        "clusters see one tree — held to one by Recreate, an exclusive flock "
        "and the RWO attach fence inside the cluster, and by the epoch cell "
        "in the bucket across clusters. A second hub on ONE prefix is "
        "split-brain: it loses the CAS and is fenced. Two hubs on two "
        "prefixes, as drawn, is the ordinary configuration.",

        "A HUB DOES NOT COUNT CLUSTERS — IT SEES CLIENT NAMES. The NFSv4 "
        "client identity is the hostname and nothing else, so two clusters "
        "sharing a node name are ONE client, and RFC 8881 §18.35.5 requires "
        "the server to read the second as the first rebooting: the "
        "incumbent's locks and opens go, silently, correctly, on a false "
        "premise. Set hostname = <cluster>-<node>, or nfs.nfs4_unique_id.",

        "THE DISK IS A CACHE AND THE BUCKET IS THE COPY, so the recovery "
        "point is the flush cadence. Cold files evict at a disk watermark "
        "and hydrate on first touch, and both doors SAY SO rather than "
        "hanging: NFS parks the caller with NFS4ERR_DELAY, HTTP answers 503 "
        "+ Retry-After. A file request on a parked share WAKES it — measured "
        "at 11 s on kind — which is why the gateway watches the CR and never "
        "polls a hub: a poll counts as activity and would pin every share it "
        "touched awake.",

        "REACHABILITY IS THE BOUNDARY ON :2049 — it authenticates nobody, "
        "and kube-proxy SNATs a remote client to an address in the hub's own "
        "cluster before the packet arrives (1486 of 1486 connections on a "
        "three-cluster rig), so networkPolicy cannot draw it: use peering, "
        "security groups or a gateway. And nothing here is a complete oracle "
        "of ACTIVITY: the gateway sees every file-API call, but consumers "
        "dial each hub's :2049 directly, so a partitioned cluster's busy "
        "agents look exactly like idle ones and POST /wake has to be driven "
        "by whatever actually knows work is happening.",
    ])

    # ---- legend ----------------------------------------------------------
    y += 0.22
    d.legend(p, 0.55, y, [
        ("data plane — every read and write, over the wire", FLOW, False),
        ("durable path — each hub is the only writer of ITS prefix", DUR,
         False),
        ("control plane — never reads a file", CTL, True),
        ("the file API — ONE shared door in front of every hub", ALT, False),
    ], span=5.75)

    d.glossary(p, 0.55, y + 0.45, 23.0, GLOSSARY)
    return doc, p


def main():
    args = [a for a in sys.argv[1:] if not a.startswith("--")]
    outdir = args[0] if args else _here
    doc, page = build()
    return d.emit(doc, page, outdir, "flint-lite-dataflow", sys.argv)


if __name__ == "__main__":
    sys.exit(main())
