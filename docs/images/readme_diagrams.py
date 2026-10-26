#!/usr/bin/env python3
"""The README's three diagrams, drawn as standalone SVG.

    python3 docs/images/readme_diagrams.py      # writes docs/images/*.svg

  flint-overview.svg         the two storage families and what each component is
  flint-s3-data-paths.svg    how bytes reach the bucket for each S3-backed front end
  flint-csi-architecture.svg the SPDK block driver: components, data path, setup

Edit this file, not the SVGs: they are regenerated. Each SVG carries its own
<style>, a light palette and a dark one under prefers-color-scheme, so it reads
on GitHub in either theme. The front-end hues are the ones the architecture
deck uses (docs/architecture/diagrams.py); block storage adds a violet.

`--check` also writes <name>.check.html beside each SVG: open it (or load it
in headless Chrome with --dump-dom) and it reports every text that spills
out of its box or overlaps another text, measured with the browser's own
font metrics rather than the estimate used for wrapping here.
"""
import html
import os
import sys

OUT = os.path.dirname(os.path.abspath(__file__))

SANS = ('-apple-system, BlinkMacSystemFont, "Segoe UI", "Noto Sans", Helvetica, '
        'Arial, sans-serif')
MONO = 'ui-monospace, SFMono-Regular, "SF Mono", Menlo, Consolas, monospace'

# Font sizes in px, read by the CSS and by wrap(). A README image is shown at
# about 0.74x on GitHub (1280 wide in a ~950 px column), so none is below 11.
CLASS_SIZE = {"h1": 24, "h2": 17, "h3": 14.5, "tag": 14, "b": 13.5, "m": 13,
              "sub": 14.5, "lbl": 12.5, "cap": 11, "pill": 11, "mono": 12.5, "num": 13}

# token: (light, dark). Neutrals follow GitHub's own light and dark canvases.
NEUTRAL = {
    "bg":    ("#ffffff", "#0d1117"),
    "edge":  ("#d0d7de", "#30363d"),
    "soft":  ("#f6f8fa", "#151b23"),
    "card":  ("#ffffff", "#1c2129"),
    "ink":   ("#1f2328", "#e6edf3"),
    "ink2":  ("#424a53", "#c9d1d9"),
    "mut":   ("#656d76", "#9198a1"),
    "ctl":   ("#8c959f", "#7d8590"),
}
# family: hue (strips, arrows), deep (text), tint (fills) -- light, then dark.
FAMILY = {
    "block": (("#6e56cf", "#5a3fc0", "#f4f1fd"), ("#a18bf5", "#bcaaf9", "#221d36")),
    "lite":  (("#184f95", "#184f95", "#eef3fa"), ("#58a6ff", "#79b8ff", "#142236")),
    "lean":  (("#1baf7a", "#0f7a55", "#ecf8f2"), ("#3dd68c", "#56e0a0", "#13281f")),
    "pass":  (("#a06b12", "#7d530c", "#faf3e6"), ("#d29922", "#e3b341", "#2b2213")),
    "forge": (("#b03060", "#8c2449", "#faedf2"), ("#f778ba", "#ff9bce", "#2f1a25")),
    "s3":    (("#57606a", "#424a53", "#f6f8fa"), ("#9198a1", "#c9d1d9", "#151b23")),
}


def _vars(i):
    out = [f"--{k}:{v[i]}" for k, v in NEUTRAL.items()]
    for k, pal in FAMILY.items():
        hue, deep, tint = pal[i]
        out += [f"--{k}:{hue}", f"--{k}-deep:{deep}", f"--{k}-tint:{tint}"]
    return ";".join(out)


def style():
    fam = []
    for k in FAMILY:
        fam.append(
            f".hue-{k}{{fill:var(--{k})}} .txt-{k}{{fill:var(--{k}-deep)}} "
            f".tint-{k}{{fill:var(--{k}-tint);stroke:var(--{k});stroke-width:1.2}} "
            f".ln-{k}{{stroke:var(--{k});stroke-width:2;fill:none}}")
    return f"""
svg{{{_vars(0)}}}
@media (prefers-color-scheme: dark){{svg{{{_vars(1)}}}}}
.bg{{fill:var(--bg);stroke:var(--edge)}}
.lane{{fill:var(--soft);stroke:var(--edge)}}
.card{{fill:var(--card);stroke:var(--edge)}}
.chip{{fill:var(--card);stroke:var(--edge)}}
.frame{{fill:none;stroke:var(--ctl);stroke-width:1.2;stroke-dasharray:6 5}}
.rule{{stroke:var(--edge);stroke-width:1}}
.ln-ctl{{stroke:var(--ctl);stroke-width:1.6;fill:none;stroke-dasharray:5 4}}
.hue-ctl{{fill:var(--ctl)}} .hue-ink2{{fill:var(--ink2)}}
text{{font-family:{SANS};fill:var(--ink)}}
.h1{{font-size:{CLASS_SIZE['h1']}px;font-weight:700}}
.h2{{font-size:{CLASS_SIZE['h2']}px;font-weight:700}}
.h3{{font-size:{CLASS_SIZE['h3']}px;font-weight:600}}
.tag{{font-size:{CLASS_SIZE['tag']}px;font-weight:600;fill:var(--ink2)}}
.b{{font-size:{CLASS_SIZE['b']}px;fill:var(--ink2)}}
.m{{font-size:{CLASS_SIZE['m']}px;fill:var(--mut)}}
.sub{{font-size:{CLASS_SIZE['sub']}px;fill:var(--mut)}}
.lbl{{font-size:{CLASS_SIZE['lbl']}px;fill:var(--ink2)}}
.cap{{font-size:{CLASS_SIZE['cap']}px;font-weight:700;letter-spacing:.08em}}
.pill{{font-size:{CLASS_SIZE['pill']}px;font-weight:700;letter-spacing:.06em}}
.mono{{font-family:{MONO};font-size:{CLASS_SIZE['mono']}px;fill:var(--ink2)}}
.num{{font-size:{CLASS_SIZE['num']}px;font-weight:700;fill:var(--bg)}}
{chr(10).join(fam)}
""".strip()


# Advance widths, as a fraction of the font size, for wrapping. Deliberately
# generous: Segoe UI and Noto run wider than San Francisco, and --check
# measures the real thing.
def text_w(s, size, kind="sans", weight=400):
    if kind == "mono":
        return len(s) * size * 0.61
    k = 0.545 if weight >= 600 else 0.53
    return len(s) * size * k


def wrap(s, width, size, kind="sans", weight=400):
    lines, cur = [], ""
    for word in s.split():
        trial = word if not cur else cur + " " + word
        if text_w(trial, size, kind, weight) <= width or not cur:
            cur = trial
        else:
            lines.append(cur)
            cur = word
    if cur:
        lines.append(cur)
    return lines




class Svg:
    def __init__(self, w, h, label):
        self.w, self.h, self.label = w, h, label
        self.parts, self.defs = [], []
        self.n = 0

    def add(self, s):
        self.parts.append(s)

    def uid(self, p):
        self.n += 1
        return f"{p}{self.n}"

    def rect(self, x, y, w, h, cls, rx=10, extra=""):
        self.add(f'<rect x="{x}" y="{y}" width="{w}" height="{h}" rx="{rx}" class="{cls}"{extra}/>')

    def text(self, x, y, s, cls, anchor="start", box=None):
        a = "" if anchor == "start" else f' text-anchor="{anchor}"'
        b = "" if box is None else ' data-box="%s"' % ",".join(str(v) for v in box)
        n = len(s) - len(s.lstrip(" "))
        s = "\u00a0" * n + s[n:]
        self.add(f'<text x="{x}" y="{y}" class="{cls}"{a}{b}>{html.escape(s, quote=False)}</text>')

    def lines(self, x, y, ls, cls, lh, box=None, anchor="start"):
        for i, s in enumerate(ls):
            self.text(x, y + i * lh, s, cls, anchor, box)
        return y + (len(ls) - 1) * lh

    def path(self, d, cls, end=None, start=None):
        m = ""
        if end:
            m += f' marker-end="url(#{self.marker(end)})"'
        if start:
            m += f' marker-start="url(#{self.marker(start)})"'
        self.add(f'<path d="{d}" class="{cls}"{m}/>')

    def marker(self, fam):
        mid = f"ah-{fam}"
        if not any(f'id="{mid}"' in d for d in self.defs):
            self.defs.append(
                f'<marker id="{mid}" viewBox="0 0 10 10" refX="8.5" refY="5" markerWidth="7" '
                f'markerHeight="7" orient="auto-start-reverse" markerUnits="userSpaceOnUse">'
                f'<path d="M0,0 L10,5 L0,10 z" class="hue-{fam}"/></marker>')
        return mid

    def clip(self, x, y, w, h, rx):
        cid = self.uid("c")
        self.defs.append(f'<clipPath id="{cid}"><rect x="{x}" y="{y}" width="{w}" height="{h}" rx="{rx}"/></clipPath>')
        return cid

    def render(self, check=False):
        lab = html.escape(self.label, quote=True)
        body = "\n  ".join(self.parts)
        defs = "\n    ".join(self.defs)
        svg = (f'<svg xmlns="http://www.w3.org/2000/svg" width="{self.w}" height="{self.h}" '
               f'viewBox="0 0 {self.w} {self.h}" role="img" aria-label="{lab}">\n'
               f'  <title>{lab}</title>\n'
               f'  <defs>\n    <style>\n{style()}\n    </style>\n    {defs}\n  </defs>\n'
               f'  <rect x="0.5" y="0.5" width="{self.w - 1}" height="{self.h - 1}" rx="14" class="bg"/>\n'
               f'  {body}\n</svg>\n')
        if not check:
            svg = "".join(svg.split(' data-box="')[0:1] + [
                p.split('"', 1)[1] for p in svg.split(' data-box="')[1:]])
        return svg


# -- shared shapes ----------------------------------------------------------

def strip_card(s, x, y, w, h, fam, top=True):
    """A card wearing its family as a strip along the top (or left) edge."""
    s.rect(x, y, w, h, "card")
    cid = s.clip(x, y, w, h, 10)
    if top:
        s.add(f'<rect x="{x}" y="{y}" width="{w}" height="5" class="hue-{fam}" clip-path="url(#{cid})"/>')
    else:
        s.add(f'<rect x="{x}" y="{y}" width="5" height="{h}" class="hue-{fam}" clip-path="url(#{cid})"/>')


def pill(s, x_right, y, label, fam):
    """A small capsule, right-aligned at x_right, top at y."""
    w = round(text_w(label, CLASS_SIZE["pill"], weight=700) * 1.12 + 18)
    s.rect(x_right - w, y, w, 20, f"tint-{fam}", rx=10)
    s.text(x_right - w / 2, y + 14, label, f"pill txt-{fam}", "middle")
    return w


def chip(s, x, y, label, cls="lbl", fam=None, h=26, pad=12, kind="sans", within=None):
    """A capsule label. `within` is the card it must stay inside (for --check)."""
    size = CLASS_SIZE["mono" if kind == "mono" else cls.split()[0]]
    w = round(text_w(label, size, kind) + 2 * pad)
    s.rect(x, y, w, h, f"tint-{fam}" if fam else "chip", rx=h / 2)
    s.text(x + w / 2, y + h / 2 + 4, label, cls, "middle", box=within or (x, y, w, h))
    return w


def cylinder(s, x, y, w, h, fam):
    ry = 5
    s.add(f'<path d="M{x},{y + ry} v{h - 2 * ry} a{w / 2},{ry} 0 0 0 {w},0 v{-(h - 2 * ry)}" '
          f'class="tint-{fam}"/>')
    s.add(f'<ellipse cx="{x + w / 2}" cy="{y + ry}" rx="{w / 2}" ry="{ry}" class="tint-{fam}"/>')



def arrow_label(s, x, y0, y1, label, w, fam, cls="lbl"):
    """A vertical arrow from y0 to y1 at x, with its label to the right."""
    s.path(f"M{x},{y0 + 2} V{y1 - 2}", f"ln-{fam}", end=fam)
    ls = wrap(label, w, CLASS_SIZE[cls])
    mid = (y0 + y1) / 2
    top = mid + 4 - (len(ls) - 1) * 8
    s.lines(x + 14, top, ls, cls, 16, box=(x + 10, y0, w + 8, y1 - y0))


# -- 1. overview --------------------------------------------------------------

def fe_card_layout(x, w, tagline, items):
    """Baselines for a card's tagline and bullets; returns (tag_lines, bullet_lines, height)."""
    tl = wrap(tagline, w - 32, CLASS_SIZE["tag"], weight=600)
    bl = [wrap(it, w - 40, CLASS_SIZE["b"]) for it in items]
    last = 60 + (len(tl) - 1) * 18 + 28 + sum(len(b) * 17 + 7 for b in bl) - 17 - 7
    h = last + 24 + 36
    return tl, bl, h


def fe_card(s, fam, x, y, w, h, name, tech, tl, bl, foot):
    box = (x, y, w, h)
    strip_card(s, x, y, w, h, fam)
    s.text(x + 16, y + 34, name, "h2", box=box)
    pill(s, x + w - 14, y + 19, tech, fam)
    ty = s.lines(x + 16, y + 60, tl, "tag", 18, box) + 28
    for ls in bl:
        s.add(f'<circle cx="{x + 18.5}" cy="{ty - 4}" r="2.2" class="hue-{fam}"/>')
        ty = s.lines(x + 28, ty, ls, "b", 17, box) + 17 + 7
    if foot:
        fk, fv = foot
        s.path(f"M{x + 1},{y + h - 36} H{x + w - 1}", "rule")
        s.text(x + 16, y + h - 14, fk, "m", box=box)
        s.text(x + 16 + text_w(fk, CLASS_SIZE["m"]) + 8, y + h - 14, fv, "mono", box=box)


def overview():
    W = 1280
    s = Svg(W, 0, "Flint: replicated block volumes on node NVMe through flint-csi-driver, "
                  "and four S3-backed front ends over one S3-compatible bucket: flint-lite (NFS), "
                  "flint-lean (sync), flint-passthrough (FUSE) and flint-forge (git).")
    s.text(32, 48, "Flint — storage for Kubernetes", "h1")
    s.text(32, 73, "Replicated block volumes on node NVMe, and four S3-backed front ends "
                   "that put a bucket behind a workload.", "sub")

    # workloads band
    s.rect(32, 92, 1216, 50, "lane")
    s.text(52, 122, "Kubernetes workloads", "h3")
    x = 232
    for c in ["Stateful apps & databases", "AI agent fleets", "Training & inference",
              "CI & build farms", "Shared team workspaces"]:
        x += chip(s, x, 104, c) + 10

    # -- block lane: one product, read left to right
    L1, CT = 166, 210
    BH = 150
    s.rect(32, L1, 1216, CT - L1 + BH + 16, "lane")
    s.text(50, L1 + 27, "BLOCK  ·  NODE-LOCAL NVMe", "cap txt-block")
    box = (48, CT, 640, BH)
    strip_card(s, 48, CT, 640, BH, "block")
    s.text(64, CT + 34, "flint-csi-driver", "h2", box=box)
    pill(s, 48 + 640 - 14, CT + 19, "BLOCK", "block")
    s.text(64, CT + 60, "Replicated block volumes, served by an SPDK target on every node",
           "tag", box=box)
    cols = [["RWO block and filesystem volumes",
             "Multi-replica: SPDK RAID1 over NVMe-oF",
             "Snapshots, clones, online expansion"],
            ["RWX through a per-volume NFS server",
             "pNFS striped volumes (opt-in)",
             "Web dashboard for disk setup"]]
    for ci, items in enumerate(cols):
        bx = 64 + ci * 304
        for j, it in enumerate(items):
            by = CT + 92 + j * 22
            s.add(f'<circle cx="{bx + 2.5}" cy="{by - 4}" r="2.2" class="hue-block"/>')
            s.text(bx + 12, by, it, "b", box=box)

    box = (704, CT, 252, BH)
    s.rect(*box, "card")
    s.text(722, CT + 30, "CSI volume", "h3", box=box)
    s.text(722, CT + 50, "disk.csi.chert.us", "mono", box=box)
    s.lines(722, CT + 74, wrap("NVMe-oF/TCP (default) or ublk into the node kernel", 216, CLASS_SIZE["m"]),
            "m", 16, box)
    s.text(722, CT + 126, "StorageClass", "m", box=box)
    s.text(722 + text_w("StorageClass", CLASS_SIZE["m"]) + 8, CT + 126, "flint", "mono", box=box)

    box = (988, CT, 244, BH)
    s.rect(*box, "card")
    s.text(1006, CT + 30, "Node NVMe / SSD disks", "h3", box=box)
    s.lines(1006, CT + 50, wrap("One SPDK lvstore per disk; an lvol per volume replica", 208, CLASS_SIZE["m"]),
            "m", 16, box)
    for i in range(3):
        cylinder(s, 1006 + i * 34, CT + 88, 26, 42, "block")
    s.path(f"M958,{CT + BH / 2} H986", "ln-block", end="block")

    # -- S3 lane: four products in columns, read top to bottom
    L2 = CT + BH + 16 + 24
    CT2 = L2 + 44
    cards = [
        ("lite", "flint-lite", "NFS",
         "One live POSIX tree, shared by many pods in many clusters",
         ["NFSv4.2 locks, close-to-open",
          "PVC working set, S3 tier",
          "HTTP API via the hub gateway"],
         ("CR", "FlintShare")),
        ("lean", "flint-lean", "SYNC",
         "Plain local files, synced to S3 atomically",
         ["Checked out before the pod starts",
          "git and sqlite at disk speed",
          "No S3 credential in the pod"],
         ("CR", "FlintLeanWorkspace")),
        ("pass", "flint-passthrough", "FUSE",
         "An S3 prefix, mounted as-is with Mountpoint for S3",
         ["Lazy reads, whole-object writes",
          "No controller, no extra objects",
          "Mounts data other tools own"],
         ("CR", "FlintPassthroughMount")),
        ("forge", "flint-forge", "GIT",
         "A git server per repository, with S3 as its only durable state",
         ["Stock git clients, real git server",
          "Pushes acked only once in S3",
          "Branch policy, LFS, scale to zero"],
         ("CR", "FlintRepo")),
    ]
    CW = 284
    laid = [fe_card_layout(48 + i * (CW + 16), CW, c[3], c[4]) for i, c in enumerate(cards)]
    CH2 = max(h for _, _, h in laid)
    DT, DH = CT2 + CH2 + 16, 70
    MT, MH = DT + DH + 34, 86
    LB = MT + MH + 16
    s.rect(32, L2, 1216, LB - L2, "lane")
    s.text(50, L2 + 27, "S3-BACKED  ·  THE BUCKET IS THE DURABLE COPY", "cap txt-s3")
    centers = []
    for i, ((fam, name, tech, _, _, foot), (tl, bl, _)) in enumerate(zip(cards, laid)):
        x = 48 + i * (CW + 16)
        fe_card(s, fam, x, CT2, CW, CH2, name, tech, tl, bl, foot)
        centers.append((fam, x + CW / 2))

    consume = [
        (48, CW, "nfs: volume", "The node's own NFS client; nothing to install"),
        (348, 2 * CW + 16, "csi: volume  ·  s3.csi.chert.us",
         "flint-s3-csi: one node driver and flint-s3-broker serve both front ends; "
         "no sidecar, credential or privilege in the pod"),
        (948, CW, "git over HTTP", "Through flint-hub-gateway, with the pod's own token"),
    ]
    for x, w, title, line in consume:
        box = (x, DT, w, DH)
        s.rect(*box, "card")
        s.text(x + 16, DT + 25, title, "h3", box=box)
        s.lines(x + 16, DT + 44, wrap(line, w - 32, CLASS_SIZE["m"]), "m", 16, box)

    box = (48, MT, 1184, MH)
    s.rect(*box, "card")
    s.text(66, MT + 27, "S3-compatible object store", "h3", box=box)
    s.text(66 + text_w("S3-compatible object store", CLASS_SIZE["h3"], weight=600) + 12, MT + 27,
           "AWS S3, MinIO or any S3 endpoint. One bucket can hold all four, "
           "one prefix per volume, workspace, mount or repository.", "m", box=box)
    for (fam, cx), label in zip(centers, ["<path>  ·  .flint/", "files/<path>  ·  .flint/lean/",
                                          "<key>   (objects only)", "git/ packs · snapshot · lfs/"]):
        x = cx - CW / 2 + 16
        s.rect(x, MT + 44, CW - 32, 26, f"tint-{fam}", rx=13)
        s.text(cx, MT + 61, label, "mono", "middle", box=(x, MT + 44, CW - 32, 26))
    for fam, cx in centers:
        s.path(f"M{cx},{DT + DH + 2} V{MT - 2}", f"ln-{fam}", end=fam)

    s.h = LB + 24
    return s


# -- 2. S3 data paths -------------------------------------------------------------

def data_paths():
    W = 1280
    s = Svg(W, 0, "How bytes reach the bucket for each S3-backed front end. flint-lite: pods mount "
                  "a hub over NFSv4.2 and the hub flushes to S3 on a cadence. flint-lean: the pod "
                  "writes a local directory and the flint-sync worker publishes it atomically at "
                  "each boundary. flint-passthrough: every file operation goes through FUSE to a "
                  "mount-s3 worker and becomes an S3 request. flint-forge: git pushes go through "
                  "the gateway to a per-repository server whose syncer acknowledges only once the "
                  "push is in S3.")
    s.text(32, 48, "How bytes reach the bucket", "h1")
    s.text(32, 73, "Each column follows one write: what the pod touches, what carries the bytes, "
                   "and when they reach S3.", "sub")

    CW, G = 292, 16
    cols = [
        ("lite", "flint-lite", "NFS", [
            ("App pods, any node, any cluster", "an nfs: volume at /data"),
            ("Kernel NFS client", "one mount per node, shared by its pods"),
            ("flint-lite hub pod", "holds every lock, lease and open; its PVC is the working set"),
        ], ["every read and write, live", "NFSv4.2 over TCP 2049",
            "flushed on a cadence; cold reads hydrate on demand"], "<path>  ·  .flint/"),
        ("lean", "flint-lean", "SYNC", [
            ("Agent pod", "plain files at /workspace; no S3 credential"),
            ("Node-local directory", "checked out before the pod's first container starts"),
            ("flint-sync worker", "unprivileged, in flint-workers; holds a short-lived key"),
        ], ["no interception: local disk", "read after the fact",
            "atomic publish at each boundary (one manifest CAS)"], "files/<path>  ·  .flint/lean/"),
        ("pass", "flint-passthrough", "FUSE", [
            ("App pod", "objects as files at /mnt/s3"),
            ("Kernel FUSE", "mounted by the node plugin; its fd is handed to the worker"),
            ("mount-s3 worker", "Mountpoint for S3, unprivileged; holds a short-lived key"),
        ], ["every file operation", "FUSE requests",
            "each operation is an S3 request; a file becomes an object at close()"], "<key>   (objects only)"),
        ("forge", "flint-forge", "GIT", [
            ("Agent pod", "stock git; its ServiceAccount token is the credential"),
            ("flint-hub-gateway", "the door: TokenReview, routing, wakes a parked repository"),
            ("Repository server pod", "git-http-backend and hooks; one syncer; its disk is a cache"),
        ], ["git fetch / push over smart HTTP", "routed to the repository",
            "packs uploaded, snapshot CAS, and only then ok"], "git/ packs · snapshot · lfs/"),
    ]
    HT = 100
    T = [(148, 72), (268, 72), (388, 72)]
    BT, BH = 516, 78
    # the bucket, drawn once under all four columns' prefix chips
    s.rect(32, BT, 1216, BH, "lane")
    s.text(48, BT + 25, "S3 bucket", "h3", box=(32, BT, 1216, 40))
    s.text(122, BT + 25, "one prefix per volume, workspace, mount or repository; "
                         "what each front end writes there:", "m", box=(32, BT, 1216, 40))
    for i, (fam, name, tech, tiers, arrows, prefix) in enumerate(cols):
        x = 32 + i * (CW + G)
        s.rect(x, HT, CW, 32, f"tint-{fam}", rx=8)
        s.text(x + 14, HT + 21, name, f"h3 txt-{fam}", box=(x, HT, CW, 32))
        s.text(x + CW - 14, HT + 21, tech, f"pill txt-{fam}", "end", box=(x, HT, CW, 32))
        for (ty, th), (title, line) in zip(T, tiers):
            box = (x, ty, CW, th)
            strip_card(s, x, ty, CW, th, fam, top=False)
            s.text(x + 18, ty + 25, title, "h3", box=box)
            s.lines(x + 18, ty + 45, wrap(line, CW - 34, CLASS_SIZE["m"]), "m", 16, box)
        ax = x + 40
        gaps = [(HT + 32, T[0][0]), (T[0][0] + T[0][1], T[1][0]),
                (T[1][0] + T[1][1], T[2][0]), (T[2][0] + T[2][1], BT)]
        # the header-to-pod gap carries no arrow; the other three carry the write
        for (y0, y1), lab in zip(gaps[1:], arrows):
            arrow_label(s, ax, y0, y1, lab, CW - 66, fam)
        s.rect(x + 12, BT + 44, CW - 24, 24, f"tint-{fam}", rx=12)
        s.text(x + CW / 2, BT + 60, prefix, "mono", "middle", box=(x + 12, BT + 44, CW - 24, 24))
    s.path(f"M32,{BT + BH + 24} H1248", "rule")
    foot = ("Control never carries file bytes. Lean and passthrough volumes are mounted by the "
            "s3.csi.chert.us node plugin, and each worker's short-lived S3 key comes from "
            "flint-s3-broker after a TokenReview; the application container holds no credential.")
    fy = BT + BH + 48
    for j, ln in enumerate(wrap(foot, 1216, CLASS_SIZE["m"])):
        s.text(32, fy + j * 17, ln, "m", box=(28, fy - 17, 1224, 50))
    s.h = fy + 17 + 22
    return s


# -- 3. the block driver ----------------------------------------------------------

def csi():
    W, H = 1280, 876
    s = Svg(W, H, "flint-csi-driver: a controller Deployment and a dashboard in flint-system, and on "
                  "every worker node a flint-csi-node pod running spdk-tgt. An application pod's I/O "
                  "goes through the kernel NVMe-oF/TCP initiator to the local spdk-tgt, which mirrors "
                  "it with RAID1 to a replica on another node and stores it in lvols on the node's "
                  "NVMe disks. Setup is three steps: install the chart, prepare each node, "
                  "initialize the disks.")
    s.text(32, 48, "flint-csi-driver — block volumes on node NVMe", "h1")
    s.text(32, 73, "Controller, dashboard and a per-node SPDK target; the default backend is "
                   "NVMe-oF/TCP into the node's kernel.", "sub")
    # legend, top right
    s.path("M968,58 H1004", "ln-block", end="block")
    s.text(1012, 62, "volume I/O", "lbl")
    s.path("M1100,58 H1136", "ln-ctl", end="ctl")
    s.text(1144, 62, "control", "lbl")

    s.rect(32, 96, 1216, 580, "frame", rx=14)
    s.text(52, 122, "KUBERNETES CLUSTER", "cap txt-s3")

    # top row
    TT, TH = 140, 120
    box = (52, TT, 236, TH)
    s.rect(52, TT, 236, TH, "card")
    s.text(70, TT + 28, "PersistentVolumeClaim", "h3", box=box)
    s.text(70, TT + 50, "storageClassName: flint", "mono", box=box)
    s.lines(70, TT + 72, ["RWO block or filesystem; RWX", "through NFS; pNFS opt-in"], "m", 16, box)

    box = (332, TT, 548, TH)
    strip_card(s, *box, "block")
    s.text(350, TT + 32, "flint-csi-controller", "h2", box=box)
    pill(s, 866, TT + 17, "DEPLOYMENT", "block")
    s.lines(350, TT + 53, wrap("flint-driver, CSI_MODE=controller: provisions, attaches, expands "
                               "and snapshots volumes, and places each replica on a node",
                               512, CLASS_SIZE["m"]), "m", 16, box)
    s.text(350, TT + 101, "CSI sidecars", "m", box=box)
    x = 350 + text_w("CSI sidecars", CLASS_SIZE["m"]) + 10
    for c in ["provisioner", "attacher", "resizer", "snapshotter", "livenessprobe"]:
        x += chip(s, x, TT + 84, c, h=24, pad=8, within=box) + 5

    box = (920, TT, 308, TH)
    strip_card(s, *box, "block")
    s.text(938, TT + 32, "flint-dashboard", "h2", box=box)
    pill(s, 1214, TT + 17, "DEPLOYMENT", "block")
    s.text(938, TT + 58, "frontend", "h3", box=box)
    s.text(1014, TT + 58, "nginx :3000, the web UI", "m", box=box)
    s.text(938, TT + 82, "backend", "h3", box=box)
    s.text(1014, TT + 82, ":8080 /api, disk discovery", "m", box=box)
    s.text(1014, TT + 98, "and initialization", "m", box=box)

    s.path(f"M290,{TT + TH / 2} H330", "ln-ctl", end="ctl")
    s.text(310, TT + TH / 2 - 8, "watch", "lbl", "middle", box=(286, TT, 48, TH / 2))

    # nodes
    NT, NH = 300, 360
    s.rect(52, NT, 772, NH, "lane", rx=12)
    s.text(70, NT + 26, "WORKER NODE A  ·  RUNS THE APPLICATION", "cap txt-s3")
    s.rect(844, NT, 384, NH, "lane", rx=12)
    s.text(862, NT + 26, "WORKER NODE B  ·  HOLDS A REPLICA", "cap txt-s3")

    # node A, left column: the pod and the kernel
    box = (72, NT + 48, 264, 76)
    s.rect(*box, "card")
    s.text(90, NT + 74, "Application pod", "h3", box=box)
    s.text(90, NT + 94, "/data → PVC", "mono", box=box)
    s.text(90, NT + 112, "sees an ordinary filesystem", "m", box=box)

    box = (72, NT + 172, 264, 76)
    s.rect(*box, "card")
    s.text(90, NT + 198, "Linux kernel", "h3", box=box)
    s.text(90, NT + 218, "nvme_tcp initiator (or ublk)", "m", box=box)
    s.text(90, NT + 236, "/dev/nvmeXnY → ext4 / xfs", "mono", box=box)
    arrow_label(s, 132, NT + 124, NT + 172, "read / write", 200, "block")

    def node_pod(x, w, replica):
        top = NT + 44
        s.rect(x, top, w, 220, "frame", rx=10)
        s.text(x + 16, top + 22, "flint-csi-node  ·  DaemonSet pod", "cap txt-s3")
        b = (x + 12, top + 34, w - 24, 56)
        s.rect(*b, "card")
        s.text(x + 28, top + 56, "flint-csi-driver", "h3", box=b)
        s.text(x + 28, top + 76, "CSI_MODE=node · node agent :9081" if replica else
               "NodeStage, NodePublish · node agent :9081", "m", box=b)
        b = (x + 12, top + 98, w - 24, 28)
        s.rect(*b, "card")
        s.text(x + 28, top + 117, "node-driver-registrar", "lbl", box=b)
        b = (x + 12, top + 134, w - 24, 74)
        strip_card(s, b[0], b[1], b[2], b[3], "block", top=False)
        s.text(x + 30, top + 157, "spdk-tgt", "h3", box=b)
        s.text(x + 30 + text_w("spdk-tgt", CLASS_SIZE["h3"], weight=600) + 10, top + 157,
               "privileged · hugepages", "m", box=b)
        line = ("exports the replica lvol over NVMe-oF/TCP" if replica else
                "SPDK v26.05: lvstore and lvols, RAID1 bdev, NVMe-oF/TCP target")
        s.lines(x + 30, top + 176, wrap(line, w - 60, CLASS_SIZE["m"]), "m", 16, b)
        return top + 134 + 37  # spdk-tgt's vertical centre

    spdk_a = node_pod(412, 392, False)
    spdk_b = node_pod(864, 344, True)

    # disks
    for x, w in [(412, 392), (864, 344)]:
        b = (x, NT + 280, w, 64)
        s.rect(*b, "card")
        for i in range(2):
            cylinder(s, x + 16 + i * 34, NT + 292, 26, 40, "block")
        s.text(x + 96, NT + 306, "NVMe disks", "h3", box=b)
        s.text(x + 96, NT + 325, "one blobstore (lvstore) per disk", "m", box=b)

    # volume I/O
    s.path(f"M338,{spdk_a} H{422}", "ln-block", end="block")
    s.text(380, spdk_a - 9, "NVMe/TCP", "lbl", "middle", box=(336, spdk_a - 26, 88, 24))
    s.path(f"M{794},{spdk_a} H{874}", "ln-block", end="block")
    s.text(834, spdk_a - 22, "RAID1", "lbl", "middle", box=(796, spdk_a - 36, 76, 36))
    s.text(834, spdk_a - 8, "mirror", "lbl", "middle", box=(796, spdk_a - 36, 76, 36))
    for x in (608, 1036):
        s.path(f"M{x},{NT + 252} V{NT + 278}", "ln-block", end="block")

    # control
    s.path(f"M700,{TT + TH + 2} V{NT + 76}", "ln-ctl", end="ctl")
    s.lines(710, TT + TH + 16, ["create lvols on", "each replica's node"], "lbl", 16,
            box=(704, TT + TH, 200, NT - TT - TH))
    s.path(f"M1150,{TT + TH + 2} V{NT + 76}", "ln-ctl", end="ctl")
    s.lines(1140, TT + TH + 16, ["discover and initialize", "disks on every node"], "lbl", 16,
            box=(890, TT + TH, 256, NT - TT - TH), anchor="end")

    # setup
    ST = 700
    s.text(32, ST + 6, "SET UP ONCE", "cap txt-block")
    steps = [
        ("Install the chart", ["helm install flint-csi ./flint-csi-driver-chart \\",
                               "  -n flint-system --create-namespace"], True,
         "Also install the VolumeSnapshot CRDs if you want snapshots."),
        ("Prepare every worker node", ["Reserve 2Mi hugepages (the chart requests 8Gi), load",
                                       "nvme_tcp and nvme_fabrics, then restart kubelet."], False,
         "The root disk cannot be used: each node needs a spare disk."),
        ("Initialize the disks", ["Dashboard → Disk Setup → select the spare disks →",
                                  "Initialize. Each gets an SPDK blobstore."], False,
         "Initializing erases the disk. Then create PVCs."),
    ]
    sw = (1216 - 32) / 3
    for i, (title, body, mono, note) in enumerate(steps):
        x = 32 + i * (sw + 16)
        b = (x, ST + 20, sw, 132)
        s.rect(*b, "card")
        s.add(f'<circle cx="{x + 30}" cy="{ST + 50}" r="13" class="hue-block"/>')
        s.text(x + 30, ST + 54.5, str(i + 1), "num", "middle")
        s.text(x + 54, ST + 55, title, "h3", box=b)
        s.lines(x + 20, ST + 88, body, "mono" if mono else "b", 18, b)
        s.text(x + 20, ST + 132, note, "m", box=b)
    return s


def main():
    check = "--check" in sys.argv
    for name, fn in [("flint-overview", overview), ("flint-s3-data-paths", data_paths),
                     ("flint-csi-architecture", csi)]:
        s = fn()
        path = os.path.join(OUT, name + ".svg")
        with open(path, "w") as f:
            f.write(s.render())
        print(f"wrote {os.path.relpath(path)}  ({s.w}x{s.h})")
        if check:
            with open(os.path.join(OUT, name + ".check.html"), "w") as f:
                f.write(CHECK_HTML.replace("@SVG@", s.render(check=True)))


# Every <text> with a data-box must sit inside that box (4px inset), and no two
# texts may overlap. Results land in <pre id="out"> for --dump-dom to read.
CHECK_HTML = """<!doctype html><html><head><meta charset="utf-8"></head><body style="margin:0">
@SVG@
<pre id="out"></pre>
<script>
const out = [];
const ts = [...document.querySelectorAll('svg text')];
const bb = ts.map(t => t.getBBox());
ts.forEach((t, i) => {
  const b = bb[i], d = t.getAttribute('data-box');
  if (!d) return;
  const [x, y, w, h] = d.split(',').map(Number);
  if (b.x < x + 4 || b.x + b.width > x + w - 4 || b.y < y + 2 || b.y + b.height > y + h - 2)
    out.push('SPILL ' + JSON.stringify(t.textContent) + ' bbox=' +
      [b.x, b.y, b.width, b.height].map(v => v.toFixed(1)).join(',') + ' box=' + d);
});
for (let i = 0; i < ts.length; i++) for (let j = i + 1; j < ts.length; j++) {
  const a = bb[i], b = bb[j];
  const ix = Math.min(a.x + a.width, b.x + b.width) - Math.max(a.x, b.x);
  const iy = Math.min(a.y + a.height, b.y + b.height) - Math.max(a.y, b.y);
  if (ix > 1 && iy > 2) out.push('OVERLAP ' + JSON.stringify(ts[i].textContent) + ' / ' + JSON.stringify(ts[j].textContent));
}
document.getElementById('out').textContent = 'texts=' + ts.length + ' problems=' + out.length + '\\n' + out.join('\\n');
</script></body></html>
"""

if __name__ == "__main__":
    main()
