#!/usr/bin/env python3
"""The source of every diagram under diagrams/ — run it, then build.sh.

    python3 docs/architecture/diagrams.py        # writes diagrams/*.svg and diagrams/portrait/*.svg

Why a generator and not hand-written SVG: twelve A3 plates and seven
portrait figures share one visual grammar, and the grammar is the point.
Colour means exactly one thing here — WHICH FRONT END — and it is the same
four hues the approach radar uses (docs/radar), validated as a set with the
dataviz palette script (all-pairs CVD separation, normal-vision floor).
Everything that is not a front end is neutral: clusters, nodes, S3, the
apiserver. The single status colour is red, reserved for a hazard or a
trust boundary; a good property is written in bold, never coloured green,
because green is a front end. Solid coloured arrows carry data; dashed
neutral arrows carry control. A component box wears its front end as a
thin strip along its top edge rather than a filled header, so a plate with
forty boxes is not forty slabs of colour.

Text is wrapped here with an estimated advance width and each card ASSERTS
that its lines fit its box; build.sh --geometry then measures the real
widths in Chrome, which is the oracle. Every SVG repeats the style block on
purpose: each one is loaded standalone through <img>, and an <img>-loaded
SVG inherits nothing from the page.
"""
import os

HERE = os.path.dirname(os.path.abspath(__file__))
OVERFLOWS = []   # every card that does not fit, reported together at the end
OUT = os.path.join(HERE, "diagrams")

# ── the palette ──────────────────────────────────────────────────────────
# hue: swatch, strip, arrow. deep: text and filled table headers (the hue of
# lean is too light for text on white — 2.8:1 — so text uses a darker step
# of the same hue). tint: a box fill.
FE = {
    "lite":  dict(name="flint-lite",        hue="#184f95", deep="#184f95", tint="#eef3fa"),
    "lean":  dict(name="flint-lean",        hue="#1baf7a", deep="#0f7a55", tint="#ecf8f2"),
    "pass":  dict(name="flint-passthrough", hue="#a06b12", deep="#7d530c", tint="#faf3e6"),
    "forge": dict(name="flint-forge",       hue="#b03060", deep="#8c2449", tint="#faedf2"),
}
INK, INK2, MUTE, HAIR, LINE, PANEL = "#0f172a", "#334155", "#64748b", "#e2e8f0", "#94a3b8", "#f8fafc"
RED, RED_TINT = "#b4232a", "#fdf2f2"
S3_STROKE = "#64748b"

FONT = '"Helvetica Neue",Helvetica,Arial,sans-serif'
MONO = 'Menlo,"SF Mono",Consolas,monospace'


# ── text measurement ─────────────────────────────────────────────────────
def est(s, size, bold=False, mono=False):
    """Estimated advance width of `s` at `size`px. Deliberately a little
    generous for proportional text; --geometry measures the truth."""
    if mono:
        return len(s) * 0.62 * size
    w = 0.0
    for ch in s:
        if ch == " ":
            w += 0.29
        elif ch in "iljt.,:;'|!I()[]-/·":
            w += 0.31
        elif ch in "fr":
            w += 0.37
        elif ch in "mwMW@":
            w += 0.88
        elif ch.isupper():
            w += 0.68
        elif ch.isdigit():
            w += 0.57
        else:
            w += 0.545
    return w * size * (1.07 if bold else 1.0)


def wrap(s, maxw, size, bold=False, mono=False):
    words, lines, cur = s.split(), [], ""
    for wd in words:
        trial = wd if not cur else cur + " " + wd
        if est(trial, size, bold, mono) <= maxw or not cur:
            cur = trial
        else:
            lines.append(cur)
            cur = wd
    if cur:
        lines.append(cur)
    return lines


def esc(s):
    return s.replace("&", "&amp;").replace("<", "&lt;").replace(">", "&gt;")


# ── the drawing kit ──────────────────────────────────────────────────────
class SVG:
    """One diagram. `compact` is the portrait variant: same grammar, smaller
    type, no strips on tiny boxes."""

    def __init__(self, w, h, label, compact=False):
        self.w, self.h, self.label, self.compact = w, h, label, compact
        k = 0.88 if compact else 1.0
        self.fs = dict(t1=17 * k, t2=13 * k, t3=12 * k, t4=11 * k, t4b=11 * k,
                       mono=10.5 * k, cap=9.5 * k, on=13 * k)
        self.lh = dict(t4=15 * k, t4b=15 * k, mono=15 * k, t3=16 * k)
        self.parts = []

    def add(self, s):
        self.parts.append(s)

    # text ---------------------------------------------------------------
    def text(self, x, y, s, cls="t4", anchor=None, extra=""):
        a = ' style="text-anchor:middle"' if anchor == "middle" else (' style="text-anchor:end"' if anchor == "end" else "")
        if cls.startswith("mono"):
            extra += ' xml:space="preserve"'   # SVG collapses leading spaces otherwise; YAML needs them
        self.add(f'<text class="{cls}" x="{x:.0f}" y="{y:.0f}"{a}{extra}>{esc(s)}</text>')

    def para(self, x, y, s, maxw, cls="t4", anchor=None):
        """Wrap `s` at `maxw`; returns the y of the next free baseline."""
        size = self.fs["t4b" if cls.endswith("b") else ("mono" if cls == "mono" else cls.split()[0])]
        bold = cls.startswith("t4b") or cls.startswith("t2") or cls.startswith("t1")
        base = cls.split()[0]
        lh = self.lh.get(base, 15)
        for ln in wrap(s, maxw, size, bold=bold, mono=(base == "mono")):
            self.text(x, y, ln, cls, anchor)
            y += lh
        return y

    # shapes -------------------------------------------------------------
    def rect(self, x, y, w, h, cls, rx=7):
        self.add(f'<rect x="{x:.0f}" y="{y:.0f}" width="{w:.0f}" height="{h:.0f}" rx="{rx}" class="{cls}"/>')

    def strip(self, x, y, w, fe, r=7, t=5):
        """A coloured band along a rounded box's top edge."""
        self.add(f'<path class="f-{fe}" d="M{x+r:.0f},{y:.0f} H{x+w-r:.0f} A{r},{r} 0 0 1 {x+w:.0f},{y+r:.0f} V{y+t:.0f} H{x:.0f} V{y+r:.0f} A{r},{r} 0 0 1 {x+r:.0f},{y:.0f} Z"/>')

    def lbar(self, x, y, h, cls="f-red", r=7, t=5):
        """A coloured bar along a rounded box's left edge — the hazard mark."""
        self.add(f'<path class="{cls}" d="M{x:.0f},{y+r:.0f} A{r},{r} 0 0 1 {x+r:.0f},{y:.0f} H{x+t:.0f} V{y+h:.0f} H{x+r:.0f} A{r},{r} 0 0 1 {x:.0f},{y+h-r:.0f} Z"/>')

    def box(self, x, y, w, h, fe=None, tone=None, strip=True):
        """tone: None white · 'tint' front-end tint · 'panel' grey · 'warn' red edge · 's3' the store."""
        if tone == "tint":
            self.rect(x, y, w, h, f"b-{fe}")
        elif tone == "panel":
            self.rect(x, y, w, h, "panel")
        elif tone == "warn":
            self.rect(x, y, w, h, "bx")
            self.lbar(x, y, h)
        elif tone == "s3":
            self.rect(x, y, w, h, "s3")
        else:
            self.rect(x, y, w, h, "bx")
        if fe and strip and tone != "tint":
            self.strip(x, y, w, fe)

    def group(self, x, y, w, h, label=None, sub=None):
        self.rect(x, y, w, h, "grp", rx=10)
        if label:
            self.text(x + 16, y + 22, label.upper(), "cap")
        if sub:
            self.text(x + 16 + est(label.upper(), self.fs["cap"], True) * 1.12 + 10, y + 22, sub, "t4")

    def hair(self, x1, y1, x2, y2):
        self.add(f'<line class="hair" x1="{x1:.0f}" y1="{y1:.0f}" x2="{x2:.0f}" y2="{y2:.0f}"/>')

    # arrows -------------------------------------------------------------
    def arrow(self, x1, y1, x2, y2, fe=None, dashed=False, both=False):
        cls = f"l-{fe}" if fe else "l-ctl"
        mk = fe or "ctl"
        d = ' stroke-dasharray="6 5"' if dashed else ""
        ms = f' marker-start="url(#a-{mk})"' if both else ""
        self.add(f'<line class="{cls}" x1="{x1:.0f}" y1="{y1:.0f}" x2="{x2:.0f}" y2="{y2:.0f}"{d}{ms} marker-end="url(#a-{mk})"/>')

    def path(self, d, fe=None, dashed=False, arrow=True):
        cls = f"l-{fe}" if fe else "l-ctl"
        mk = fe or "ctl"
        dd = ' stroke-dasharray="6 5"' if dashed else ""
        me = f' marker-end="url(#a-{mk})"' if arrow else ""
        self.add(f'<path class="{cls}" d="{d}"{dd}{me}/>')

    def alabel(self, x, y, s, fe=None, sub=None):
        """A label centred on x, with a white halo so it can sit on an arrow."""
        w = max(est(s, self.fs["t4b"], True), est(sub, self.fs["t4"]) if sub else 0) + 10
        self.rect(x - w / 2, y - 11, w, 30 if sub else 15, "halo", rx=3)
        self.text(x, y, s, f"t4b c-{fe}" if fe else "t4b", "middle")
        if sub:
            self.text(x, y + 15, sub, "t4", "middle")

    def numdot(self, x, y, n, fe=None):
        self.add(f'<circle cx="{x:.0f}" cy="{y:.0f}" r="11" class="{"f-"+fe if fe else "f-ink"}"/>')
        self.text(x, y + 4.5, str(n), "num", "middle")

    def chip(self, x, y, s, fe=None, tone=None):
        """A small pill. Returns its width."""
        size = self.fs["cap"]
        w = est(s.upper(), size, True) * 1.12 + 16
        cls = f"chip-{fe}" if fe else ("chip-red" if tone == "red" else "chip")
        self.rect(x, y - 11, w, 16, cls, rx=8)
        self.text(x + 8, y + 1, s.upper(), f"cap {'on' if fe or tone else ''}".strip())
        return w

    # composites ---------------------------------------------------------
    def card(self, x, y, w, h, head, items=(), fe=None, tone=None, head_cls=None, pad=16, strip=True, lh=None):
        """A box with a heading and a wrapped body. Items: str (body) ·
        ('b', str) bold · ('m', str) mono · ('r', str) red bold · ('h', str)
        a sub-heading · ('gap',). Asserts the body fits."""
        self.box(x, y, w, h, fe, tone, strip)
        cy = y + 24
        if head:
            hc = head_cls or (f"t2 c-{fe}" if fe else "t2")
            if tone == "warn":
                hc = "t2 c-red"
            self.text(x + pad, cy, head, hc)
            cy += 20
        maxw = w - 2 * pad
        lh4 = lh or self.lh["t4"]
        for it in items:
            if isinstance(it, str):
                for ln in wrap(it, maxw, self.fs["t4"]):
                    self.text(x + pad, cy, ln, "t4")
                    cy += lh4
            else:
                kind = it[0]
                if kind == "gap":
                    cy += lh4 * 0.45
                    continue
                s = it[1]
                if kind == "b":
                    for ln in wrap(s, maxw, self.fs["t4b"], bold=True):
                        self.text(x + pad, cy, ln, "t4b")
                        cy += lh4
                elif kind == "r":
                    for ln in wrap(s, maxw, self.fs["t4b"], bold=True):
                        self.text(x + pad, cy, ln, "t4b c-red")
                        cy += lh4
                elif kind == "h":
                    cy += 4
                    self.text(x + pad, cy, s, "t3b")
                    cy += lh4 + 1
                elif kind in ("m", "mm"):
                    if est(s, self.fs["mono"], mono=True) > maxw:
                        OVERFLOWS.append(f"{self.label[:28]!r}: mono too wide in {head!r}: {s!r} ({est(s, self.fs['mono'], mono=True) - maxw:.0f}px over)")
                    self.text(x + pad, cy, s, "mono" if kind == "m" else "mono mute")
                    cy += lh4
        used = cy - lh4 + 6          # last baseline + descender room
        if used > y + h - 4:
            OVERFLOWS.append(f"{self.label[:28]!r}: card {head!r} needs {used - y:.0f}px, has {h}px")
        return cy

    def header(self, x, y, w, fe, title, sub=None):
        """A filled column header — the one place a front end's deep colour
        is a slab, because a table needs a column head you can find."""
        self.rect(x, y, w, 46 if sub else 34, f"hd-{fe}")
        self.text(x + w / 2, y + 22, title, "on", "middle")
        if sub:
            self.text(x + w / 2, y + 38, sub, "onsub", "middle")

    def legend(self, x, y, items):
        """items: list of (kind, label) — kind in fe keys, 'ctl', 'data', 'red'."""
        cx = x
        for kind, label in items:
            if kind == "ctl":
                self.add(f'<line class="l-ctl" x1="{cx}" y1="{y-4}" x2="{cx+26}" y2="{y-4}" stroke-dasharray="6 5"/>')
                cx += 32
            elif kind == "data":
                self.add(f'<line class="l-ink" x1="{cx}" y1="{y-4}" x2="{cx+26}" y2="{y-4}"/>')
                cx += 32
            elif kind == "red":
                self.rect(cx, y - 11, 5, 14, "f-red", rx=2)
                cx += 12
            else:
                self.rect(cx, y - 11, 14, 14, f"f-{kind}", rx=3)
                cx += 20
            self.text(cx, y, label, "t4")
            cx += est(label, self.fs["t4"]) + 22

    def table(self, x, y, colw, header, rows, lab_w, min_row=0, zebra=True):
        """header: [(title, fe)] per data column. rows: [(label, sub, [cell,...])]
        where a cell is a list of card items. Row heights come from the
        wrapped text. Returns the bottom y."""
        hh = 38
        self.rect(x, y, lab_w, hh, "hd-lbl", rx=6)
        self.text(x + 14, y + 24, "dimension", "on")
        cx = x + lab_w + 6
        for (title, fe), w in zip(header, colw):
            self.rect(cx, y, w - 6, hh, f"hd-{fe}", rx=6)
            self.text(cx + (w - 6) / 2, y + 24, title, "on", "middle")
            cx += w
        cy = y + hh + 6
        lh = self.lh["t4"]
        f4, f4b = self.fs["t4"], self.fs["t4b"]

        def measure(cell, maxw):
            n = 0
            for it in cell:
                if isinstance(it, str):
                    n += len(wrap(it, maxw, f4))
                elif it[0] == "gap":
                    n += 0.45
                elif it[0] in ("b", "r"):
                    n += len(wrap(it[1], maxw, f4b, bold=True))
                else:
                    n += 1
            return n

        for i, (label, sub, cells) in enumerate(rows):
            nmax = max(measure(c, w - 30) for c, w in zip(cells, colw))
            rh = max(min_row, int(nmax * lh) + 26)
            self.rect(x, cy, lab_w, rh, "lbl", rx=0)
            if zebra and i % 2:
                self.rect(x + lab_w, cy, sum(colw), rh, "rowb", rx=0)
            self.text(x + 14, cy + 22, label, "t2")
            if sub:
                self.para(x + 14, cy + 40, sub, lab_w - 24, "t4")
            cx = x + lab_w
            for cell, w in zip(cells, colw):
                ty = cy + 22
                for it in cell:
                    if isinstance(it, str):
                        for ln in wrap(it, w - 30, f4):
                            self.text(cx + 14, ty, ln, "t4")
                            ty += lh
                    elif it[0] == "gap":
                        ty += lh * 0.45
                    elif it[0] == "b":
                        for ln in wrap(it[1], w - 30, f4b, bold=True):
                            self.text(cx + 14, ty, ln, "t4b")
                            ty += lh
                    elif it[0] == "r":
                        for ln in wrap(it[1], w - 30, f4b, bold=True):
                            self.text(cx + 14, ty, ln, "t4b c-red")
                            ty += lh
                    elif it[0] == "m":
                        self.text(cx + 14, ty, it[1], "mono")
                        ty += lh
                cx += w
            self.hair(x, cy + rh, x + lab_w + sum(colw), cy + rh)
            cy += rh
        # frame + column rules
        self.rect(x, y + hh + 6, lab_w + sum(colw), cy - (y + hh + 6), "frame", rx=0)
        cx = x + lab_w
        for w in colw[:-1]:
            cx += w
            self.add(f'<line class="hair" x1="{cx}" y1="{y+hh+6}" x2="{cx}" y2="{cy}"/>')
        return cy

    # output -------------------------------------------------------------
    def render(self):
        fs = self.fs
        style = f"""
  text{{font-family:{FONT};fill:{INK}}}
  .t1{{font-size:{fs['t1']}px;font-weight:600}}
  .t2{{font-size:{fs['t2']}px;font-weight:600}}
  .t3{{font-size:{fs['t3']}px}}
  .t3b{{font-size:{fs['t3']}px;font-weight:600;fill:{INK2}}}
  .t4{{font-size:{fs['t4']}px;fill:{MUTE}}}
  .t4b{{font-size:{fs['t4b']}px;font-weight:600;fill:{INK2}}}
  .mono{{font-family:{MONO};font-size:{fs['mono']}px;fill:{INK2}}}
  .mute{{fill:{MUTE}}}
  .cap{{font-size:{fs['cap']}px;font-weight:600;letter-spacing:.1em;fill:{MUTE}}}
  .num{{font-size:{fs['t4']}px;font-weight:600;fill:#ffffff}}
  .on{{font-size:{fs['on']}px;font-weight:600;fill:#ffffff}}
  .onsub{{font-size:{fs['t4']}px;fill:#ffffff;opacity:.85}}
  .c-red{{fill:{RED}}}
  {' '.join(f'.c-{k}{{fill:{v["deep"]}}}' for k, v in FE.items())}
  {' '.join(f'.f-{k}{{fill:{v["hue"]}}}' for k, v in FE.items())}
  .f-red{{fill:{RED}}} .f-ink{{fill:{INK2}}}
  {' '.join(f'.b-{k}{{fill:{v["tint"]};stroke:{v["hue"]};stroke-width:1.4}}' for k, v in FE.items())}
  {' '.join(f'.hd-{k}{{fill:{v["deep"]}}}' for k, v in FE.items())}
  .hd-lbl{{fill:{INK2}}}
  {' '.join(f'.l-{k}{{stroke:{v["hue"]};stroke-width:2.2;fill:none}}' for k, v in FE.items())}
  .l-ctl{{stroke:{LINE};stroke-width:1.6;fill:none}}
  .l-ink{{stroke:{INK2};stroke-width:2.2;fill:none}}
  .l-red{{stroke:{RED};stroke-width:2.2;fill:none}}
  .bx{{fill:#ffffff;stroke:{HAIR};stroke-width:1.4}}
  .panel{{fill:{PANEL};stroke:{HAIR};stroke-width:1.4}}
  .s3{{fill:#f1f5f9;stroke:{S3_STROKE};stroke-width:1.6}}
  .grp{{fill:none;stroke:{LINE};stroke-width:1.3;stroke-dasharray:7 5}}
  .hair{{stroke:{HAIR};stroke-width:1}}
  .lbl{{fill:{PANEL}}} .rowb{{fill:#fbfcfd}}
  .frame{{fill:none;stroke:{HAIR};stroke-width:1.2}}
  .halo{{fill:#ffffff}}
  .chip{{fill:{PANEL};stroke:{HAIR};stroke-width:1}}
  .chip-red{{fill:{RED}}}
  {' '.join(f'.chip-{k}{{fill:{v["deep"]}}}' for k, v in FE.items())}
  .chip .cap{{fill:{INK2}}}
  /* On a filled shape. Last on purpose: a class beats a presentation
     attribute, so this must win over .t4/.cap/.mono. */
  .on,.num,.onsub{{fill:#ffffff}}
  .cap.on{{fill:#ffffff}}
"""
        markers = "".join(
            f'<marker id="a-{k}" viewBox="0 0 10 10" refX="9" refY="5" markerWidth="7" markerHeight="7" orient="auto-start-reverse"><path d="M0,0 L10,5 L0,10 z" fill="{c}"/></marker>'
            for k, c in [(k, v["hue"]) for k, v in FE.items()] + [("ctl", LINE), ("ink", INK2), ("red", RED)]
        )
        body = "\n".join(self.parts)
        return (f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {self.w} {self.h}" role="img"\n'
                f'     aria-label="{esc(self.label)}">\n<style>{style}</style>\n<defs>{markers}</defs>\n{body}\n</svg>\n')


def write(name, svg):
    path = os.path.join(OUT, name)
    os.makedirs(os.path.dirname(path), exist_ok=True)
    open(path, "w", encoding="utf-8").write(svg.render())
    return path


# ═════════════════════════════════════════════════════════════════════════
# The plates
# ═════════════════════════════════════════════════════════════════════════
W, H = 1600, 880
M = 24
COL4 = 374
GAP = 18


def col_x(i):
    return M + i * (COL4 + GAP)


def plate_01():
    s = SVG(W, H, "Four front ends over one bucket. flint-lite serves live shared POSIX from a hub pod over NFSv4.2; "
                  "flint-lean materialises plain local files and publishes boundaries with a worker; flint-passthrough mounts "
                  "an S3 prefix directly with Mountpoint for S3; flint-forge serves real git from a per-repository pod whose "
                  "every push is acknowledged only once it is in S3. All four read and write one bucket, in which each "
                  "front end has its own layout and control namespace.")
    cols = [
        ("lite", "flint-lite — the hub", "NFS · live shared POSIX · many readers and writers",
         ("the consumer pod — any cluster", ["nothing from flint is installed in it", ("m", "/data  (an ordinary PVC)"),
                                             "the node's kernel NFS client carries every byte — one mount per node, shared with its page cache",
                                             ("b", "full POSIX: byte-range locks, atomic rename, O_EXCL")]),
         ("the hub — ONE pod per volume", [("m", "flint-pnfs-mds · mode: standalone"), ("m", ":2049  NFSv4.2 — the data plane"),
                                           ("m", ":8080  status + file API — ClusterIP only"),
                                           "the PVC is the working set and a CACHE of the bucket, never the only copy",
                                           ("b", "single replica · Recreate · exclusive flock")]),
         ("coordination — the hub IS the authority", ["leases, locks and close-to-open are enforced in one process, so every client sees one coherent tree; a second hub on the prefix is fenced by the epoch cell"]),
         ("publish · hydrate", "RPO = the flush cadence")),
        ("lean", "flint-lean — checkout / publish", "sync · one agent, one workspace, one writer",
         ("the agent pod", ["the app image is unchanged and holds no S3 credential", ("m", "/workspace  (local disk — an emptyDir)"),
                            "plain files with no interception anywhere in the read or write path",
                            ("b", "git, sqlite and build caches work, at disk speed")]),
         ("flint-sync — an unprivileged worker", [("m", "delivered by the s3.csi.chert.us driver"), "checkout at start, before the agent's first line; a barrier at cadence from an mtime/size scan",
                                                  ("m", ".flint/publish  →  .flint/publish.ack"),
                                                  ("b", "preStop drains — a graceful stop loses nothing")]),
         ("coordination — a lease in the BUCKET", ["one writer per subtree, admitted by a CAS on the epoch cell; a second syncer is REFUSED, never merged; readers see the last published boundary"]),
         ("checkout · publish", "RPO = the last barrier")),
        ("pass", "flint-passthrough — the mount", "FUSE · an S3 prefix mounted as-is · no flint semantics",
         ("the tenant pod", ["no sidecar, no label, no credential, no privilege — admissible under PodSecurity restricted",
                             ("m", "/mnt/s3  (a csi: volume naming a CR)"), "objects appear as files, served by Mountpoint for S3",
                             ("b", "NOT POSIX: no rename, no append, no in-place write")]),
         ("s3.csi.chert.us — a CSI node DaemonSet", ["one privileged process per NODE, not per pod: it opens /dev/fuse and calls mount(2) itself, then hands the fd to an unprivileged worker running an unchanged mount-s3",
                                                     ("b", "the plugin holds no S3 credential — a broker mints them")]),
         ("coordination — NONE, by design", ["no cache, no manifest, no publish boundary, no coordination between pods sharing a prefix; S3's own consistency is the entire contract"]),
         ("GET · PUT", "no RPO — nothing is buffered")),
        ("forge", "flint-forge — the git server", "git · a remote per repository · S3 behind it",
         ("the agent pod", ["a stock git client; the working tree is local disk", ("m", "credential.helper = flint-forge-credential"),
                            "the pod's projected ServiceAccount token, audience forge.chert.us, is the Basic password",
                            ("b", "the durable unit is a commit — a push")]),
         ("the door, and one server pod per repository", ["the door is lite's gateway with a git arm: TokenReview, consumers, routing, wake",
                                                          ("m", "gitcgi + git-http-backend + one syncer"),
                                                          "the local repository is an emptyDir CACHE; the pod idles to zero",
                                                          ("b", "one syncer owns every write to the bucket")]),
         ("coordination — one CAS per batch", ["a push is acknowledged only after its pack and ONE CAS'd snapshot are in S3; a stale push is refused by name; a merge is a push to refs/for/<target>"]),
         ("push · restore", "RPO = the last acknowledged push")),
    ]
    for i, (fe, title, sub, pod, comp, coord, (alab, asub)) in enumerate(cols):
        x = col_x(i)
        s.header(x, 16, COL4, fe, title, sub)
        s.card(x, 74, COL4, 128, pod[0], pod[1], fe)
        s.card(x, 214, COL4, 152, comp[0], comp[1], fe, "tint")
        s.card(x, 378, COL4, 96, coord[0], coord[1], None, "panel")
        cx = x + COL4 / 2
        s.arrow(cx, 474, cx, 528, fe)
        s.alabel(cx, 496, alab, fe, asub)

    # the bucket
    s.box(M, 536, W - 2 * M, 250, None, "s3")
    s.text(M + 20, 564, "One bucket. Four layouts — and a volume can move between the file-shaped three.", "t1")
    s.text(M + 20, 584, "Whole-file objects plus a per-front-end control namespace; for forge, a bare git repository that git can clone read-only with the server down.", "t4")
    writes = [
        ("lite", "flint-lite writes", [("m", "<prefix>/<path>"), ("mm", "<prefix>/.flint/epoch · manifest · owner")]),
        ("lean", "flint-lean writes", [("m", "<prefix>/files/<path>"), ("mm", "<prefix>/.flint/lean/epoch · claim"), ("mm", "<prefix>/.flint/lean/current · inbox")]),
        ("pass", "flint-passthrough writes", [("m", "<prefix>/<key>"), "and nothing else — no control namespace, because there is no state to keep"]),
        ("forge", "flint-forge writes", [("m", "<prefix>/git/objects/pack/*"), ("mm", "<prefix>/git/snapshot (CAS) · epoch · claim"), ("mm", "<prefix>/lfs/objects/<oid>"), ("mm", "<prefix>/files/<path>  — the export")]),
    ]
    for i, (fe, head, items) in enumerate(writes):
        s.card(col_x(i) + (0 if i else 20) - (0 if i else 0), 600, COL4 - (20 if i == 0 else 0) - (0), 120, head, items, fe) if False else None
    inner_w = (W - 2 * M - 40 - 3 * GAP) / 4
    for i, (fe, head, items) in enumerate(writes):
        s.card(M + 20 + i * (inner_w + GAP), 600, inner_w, 120, head, items, fe)
    s.para(M + 20, 752, "Read the columns as an escalation ladder, cheapest first: passthrough asks nothing of the workload and gives it nothing back; lean gives real POSIX at local speed to one writer; the hub is the only one where several pods share one live tree; forge adds history, a merge policy and an acknowledgement that means durable — for code.", W - 2 * M - 40)
    s.legend(M + 4, 826, [("lite", "flint-lite"), ("lean", "flint-lean"), ("pass", "flint-passthrough"), ("forge", "flint-forge"),
                          ("data", "data path"), ("ctl", "control"), ("red", "hazard / trust boundary")])
    s.text(M + 4, 856, "A box wears its front end as the strip on its top edge. S3 and Kubernetes are neutral: they belong to nobody.", "t4")
    return s

def plate_01b():
    s = SVG(W, H, "The data flow of each front end as a column: what the pod touches, what carries the bytes, and what puts them "
                  "in the bucket. flint-lite: an NFS mount reaches the hub over the wire, live; the hub publishes on a cadence and "
                  "hydrates on demand. flint-lean: the pod writes a local directory a sidecar copies to S3 at a boundary. "
                  "flint-passthrough: every file operation is intercepted by mount-s3 and becomes an S3 request. flint-forge: a push "
                  "carries packs to the server pod, whose syncer uploads them and acknowledges only once they are in S3. Control "
                  "sits beside each column and never carries a file.")
    cols = [
        ("lite", "NFS", "flint-lite — live, shared, over the wire",
         ("the pod", [("m", "/data — an NFS mount"), ("b", "every read and write crosses the wire")]),
         ("NFSv4.2", "both ways, live, every operation", True, False),
         ("the hub pod", ["one process holds the tree: leases, locks, close-to-open", ("m", "PVC = the working set, a cache")]),
         ("publish on a cadence  ↑   hydrate on demand  ↓", "RPO = the cadence"),
         ("m", "<prefix>/<path>"),
         ["epoch cell in the bucket — who is the hub", "operator — suspend, wake", "port 2049 — no credential; reachability"]),
        ("lean", "sync", "flint-lean — local disk, then reconciled",
         ("the pod", [("m", "/workspace — local disk"), ("b", "reads and writes touch nothing but the disk")]),
         ("the same directory", "no wire, no interception — the sidecar reads it later", False, True),
         ("flint-sync sidecar", ["scans for changed files; copies them with the plain S3 API", ("m", "unprivileged, in a system namespace")]),
         ("publish at a boundary  ↑   checkout at start  ↓", "RPO = the last barrier"),
         ("m", "<prefix>/files/<path>"),
         ["lease in the bucket — one writer", "broker → short-lived keys → sidecar", ".flint/publish → .flint/publish.ack"]),
        ("pass", "FUSE", "flint-passthrough — every operation is a request",
         ("the pod", [("m", "/mnt/s3 — a FUSE mount"), ("b", "every file operation is intercepted")]),
         ("FUSE", "per operation, in the pod's critical path", True, False),
         ("mount-s3 worker", ["translates each operation into GET, PUT, LIST", ("m", "no cache of flint's, no state")]),
         ("GET · PUT · LIST, per operation", "no RPO — nothing is buffered"),
         ("m", "<prefix>/<key>"),
         ["node plugin — mount(2), hands over the fd", "broker → short-lived keys → worker", "CR consumers — who may mount"]),
        ("forge", "git", "flint-forge — bytes move on push",
         ("the pod", [("m", "a git clone — local disk"), ("b", "commits are local; a push moves them")]),
         ("git smart HTTP", "push ↑ · fetch ↓ — through the door", True, False),
         ("server pod — gitcgi + syncer", ["git-http-backend serves; the syncer uploads", ("m", "the local repository is a cache")]),
         ("packs on push, ack after  ↑   restore at start  ↓", "RPO = the last acknowledged push"),
         ("m", "<prefix>/git/…"),
         ["door — TokenReview → X-Remote-User", "lease + ONE snapshot CAS per batch", "operator — idle to zero, wake"]),
    ]
    for i, (fe, word, sub, pod, (alab, asub, both, dashed), comp, (blab, bsub), key, ctl) in enumerate(cols):
        x = col_x(i); cx = x + COL4 / 2
        s.header(x, 16, COL4, fe, word, sub)
        s.card(x, 76, COL4, 84, pod[0], pod[1], fe)
        s.arrow(cx, 166, cx, 228, fe, dashed=dashed, both=both)
        s.alabel(cx, 190, alab, fe, asub)
        s.card(x, 232, COL4, 100, comp[0], comp[1], fe, "tint")
        s.arrow(cx, 338, cx, 424, fe, both=True)
        s.alabel(cx, 374, blab, fe, bsub)
        s.text(x + 16, 470, key[1], "mono")
        s.card(x, 560, COL4, 118, "control — never on the data path", ctl, None, "panel")
    # the store
    s.box(M, 430, W - 2 * M, 96, None, "s3")
    s.text(M + 16, 456, "S3 — the durable store. One bucket; each front end has its prefix; the bytes are the same bytes.", "t1")
    s.text(M + 16, 508, "Whole-file objects for the file-shaped three; a bare git repository for forge. What the arrow above says is when the bytes arrive, and that is the recovery point.", "t4")
    s.text(M + 4, 720, "Solid arrows carry file contents; the dashed arrow is a shared directory, not a wire. The word on each header is the technology between the pod and the bucket.", "t4b")
    s.legend(M + 4, 748, [("lite", "flint-lite"), ("lean", "flint-lean"), ("pass", "flint-passthrough"), ("forge", "flint-forge"), ("data", "data path"), ("ctl", "control")])
    s.h = 772
    return s


def plate_02():
    s = SVG(W, H, "flint-lite, drawn as a flow: agent pods on a node share one kernel NFS client, which carries every read and "
                  "write live over NFSv4.2 to one hub pod; the hub's PVC is a cache, and the hub publishes to the S3 prefix on a "
                  "cadence and hydrates from it on demand. The epoch cell, the operator and the unauthenticated port are control, "
                  "off the data path.")
    fe = "lite"
    # ── consumer cluster: the pods, the node, the one client ──
    s.group(M, 22, 470, 420, "consumer cluster", "installs nothing from flint")
    s.box(44, 56, 430, 356, None, "panel")
    s.text(62, 80, "node", "cap")
    for i in range(3):
        bx = 64 + i * 132
        s.box(bx, 96, 118, 52, None)
        s.text(bx + 59, 127, "agent pod", "t3", "middle")
        s.arrow(bx + 59, 148, bx + 59, 178, fe)
    s.card(64, 182, 390, 66, "kernel NFS client — one mount per node", ["shared by every pod on it, with its page cache"], fe, "tint")
    s.text(64, 278, "PV: nfs server=<hub> path=/", "mono")
    s.text(64, 296, "nfsvers=4.1 · hard · nconnect=4   (≥ 2, or no trunk)", "mono")
    s.text(64, 330, "the node is the client: one mount, one identity, its own uid", "t4b")
    s.text(64, 348, "full POSIX across pods and clusters: locks, rename, O_EXCL", "t4")
    # ── the wire ──
    s.arrow(494, 215, 612, 215, fe, both=True)
    s.alabel(553, 199, "NFSv4.2 :2049", fe, "every read and write")
    # ── hub cluster ──
    s.group(616, 22, 520, 420, "hub cluster", "one FlintShare per volume")
    s.box(636, 56, 480, 212, fe)
    s.text(654, 82, "flint-lite hub — ONE pod · Recreate · exclusive flock", f"t2 c-{fe}")
    s.card(654, 98, 212, 70, ":2049  NFSv4.2", ["the data plane: locks, close-to-open, rename"], fe, "tint", strip=False)
    s.card(886, 98, 212, 70, ":8080  status + files", ["ClusterIP only · one bearer per share"], None, "warn")
    s.text(654, 196, "both doors are NFS compounds in one process — one tree", "t4")
    s.text(654, 214, "Service :2049 — advertiseAddress for remote clusters", "mono")
    s.text(654, 244, "the coherence authority: N pods, N clusters, ONE tree", "t4b")
    s.arrow(876, 268, 876, 296, fe, both=True)
    s.card(636, 300, 480, 60, "PVC — the working set, a CACHE", ["losing it is a rebuild from the bucket, not a loss"], None, "panel")
    s.group(636, 372, 480, 60, "control — off the data path")
    s.text(654, 412, "epoch cell: one hub per prefix, a second is fenced · operator: suspend idle, wake on request", "t4")
    # ── publish / hydrate ──
    s.box(1290, 56, 286, 304, None, "s3")
    s.arrow(1136, 150, 1286, 150, fe)
    s.arrow(1286, 182, 1136, 182, fe, True)
    s.alabel(1211, 132, "publish · on a cadence", fe)
    s.alabel(1211, 206, "hydrate · on demand", fe, "RPO = the cadence")
    # ── S3 ──
    s.text(1308, 84, "S3 — the durable copy", "t1")
    s.text(1308, 114, "<prefix>/<path>", "mono")
    s.text(1308, 132, "whole-file generations, untorn", "t4")
    s.text(1308, 164, ".flint/epoch · manifest · owner", "mono")
    s.text(1308, 182, "who is the hub · the DR document", "t4")
    s.hair(1308, 200, 1558, 200)
    s.text(1308, 226, "the bucket trusts ONE principal:", "t4b")
    s.text(1308, 244, "the hub. Consumers never hold a", "t4")
    s.text(1308, 262, "credential; they use :2049.", "t4")
    s.text(1308, 294, "everything durable is here,", "t4")
    s.text(1308, 312, "everything fast is on the disk.", "t4")
    # ── hazard ──
    s.box(M, 464, W - 2 * M, 54, None, "warn")
    s.text(48, 486, "port 2049 authenticates nobody — reachability is the boundary; the :8080 token is per share, not per user; a second hub on one prefix is split-brain, which the epoch cell fences.", "t4b c-red")
    s.legend(M + 4, 548, [("data", "data path"), ("ctl", "control"), ("red", "hazard / trust boundary")])
    s.h = 572
    return s


def plate_03():
    s = SVG(W, H, "One flint-lite hub serving agents in three clusters at once. The wire is identical from each and the hub sees "
                  "only client names, not clusters. Three things the fleet must supply that the protocol and the chart cannot: a "
                  "unique client name per node, a reachability boundary drawn outside networkPolicy, and a keepalive so a "
                  "partitioned cluster is not mistaken for an idle one.")
    fe = "lite"
    s.text(M + 6, 34, "consumer clusters — identical bytes on the wire from each", "cap")
    s.text(M + 6, 54, "the hub does not know, and cannot ask, how many clusters are on it", "t4")
    clusters = [("Cluster A — eu-west", "each agent may carry a different short-lived token", 'identity: hostname "agent-7"', False),
                ("Cluster B — us-east", "same human, different cluster, different token", 'identity: hostname "agent-7"   ← COLLIDES', True),
                ("Cluster C — the hub's own cluster", "in-cluster mounts arrive from the NODE ip", 'identity: hostname "builder-2"', False)]
    for i, (name, note, ident, warn) in enumerate(clusters):
        y = 68 + i * 196
        s.group(M, y, 360, 180, name)
        s.card(44, y + 34, 320, 58, "agent pods · the user's tokens", [note], None)
        s.card(44, y + 102, 320, 60, "kernel NFS client", [("m", ident)], fe, "warn" if warn else "tint", strip=False)
    s.path("M384 206 L470 206 L470 396", fe)
    s.path("M384 402 L470 402", fe)
    s.path("M384 598 L470 598 L470 408", fe)
    s.alabel(470, 182, "NFSv4.2", fe)
    s.text(470, 640, "one wire, three clusters,", "t4", "middle")
    s.text(470, 656, "no difference at all", "t4", "middle")
    # the hub
    s.box(500, 240, 440, 344, fe)
    s.text(520, 268, "ONE hub — one project, one prefix", f"t2 c-{fe}")
    s.text(520, 288, "unchanged image, unchanged chart, unchanged mode", "t4")
    s.hair(520, 300, 920, 300)
    s.card(520, 312, 400, 70, "Service :2049", [("m", "spec.service.advertiseAddress"), "an address only the hub cluster can route is not an address"], fe, "tint", strip=False)
    s.card(520, 394, 400, 76, "leases · locks · close-to-open · delegations", ["the coherence authority for the whole tree, wherever the client happens to be running"], None)
    s.card(520, 482, 400, 86, "A hub does not count clusters.", [("b", "It sees CLIENTS, and a client is a NAME."), "Nothing in the protocol, the CRD or the chart makes a second cluster visible as a second cluster."], None, "panel")
    s.arrow(940, 390, 1010, 390, fe)
    s.arrow(1010, 420, 940, 420, fe, True)
    s.alabel(975, 378, "publish", fe)
    s.alabel(975, 446, "hydrate", fe)
    s.card(1014, 300, 230, 226, "S3 — the durable copy", ["one prefix, one hub, one coherence authority", ("m", ".flint/epoch"), "fences a second hub — but only when the prefixes are EQUAL:", ("m", "tenant-a/  vs  tenant-a/sub/"), "mint different cells and never contend. Own prefix allocation."], None, "s3")
    # what the fleet must supply
    s.box(1268, 68, 306, 578, None, "panel")
    s.text(1288, 96, "What the FLEET must supply", "t1")
    s.para(1288, 116, "three things neither the protocol nor the chart can do for you", 270)
    y = 158
    s.numdot(1300, y - 4, 1, None)
    s.text(1322, y, "A unique name per client", "t2")
    y = s.para(1288, y + 22, "The identity is the hostname and nothing else. Two clusters sharing a node name are ONE client, and RFC 8881 says the server MUST read the second as the first REBOOTING — so the incumbent's locks and opens are dropped, silently, correctly, on a false premise.", 270)
    s.text(1288, y + 4, "hostname = <cluster>-<node>", "mono")
    s.text(1288, y + 20, "nfs.nfs4_unique_id=<cluster>", "mono")
    y += 52
    s.numdot(1300, y - 4, 2)
    s.text(1322, y, "A boundary at the network layer", "t2")
    y = s.para(1288, y + 22, "Port 2049 is AUTH_SYS, so reachability IS the boundary — and networkPolicy cannot draw it: kube-proxy SNATs a NodePort or LB client to an address in the HUB's cluster before the packet arrives.", 270)
    y = s.para(1288, y + 2, "Measured: 1486 of 1486 connections from two remote clusters arrived as the hub cluster's gateway — none as a remote node.", 270, "t4b c-red")
    y = s.para(1288, y + 2, "Use peering, security groups or a gateway.", 270)
    y += 12
    s.numdot(1300, y - 4, 3)
    s.text(1322, y, "A keepalive per remote mount", "t2")
    y = s.para(1288, y + 22, "The idle ladder suspends a quiet share, and a partitioned cluster's busy agents look exactly like quiet ones. Drive POST /wake from the thing that knows whether work is happening.", 270)
    s.para(1288, y + 2, "suspendWithSessions DEFAULTS to suspending anyway.", 270, "t4b")
    s.box(M, 664, W - 2 * M, 44, None, "warn")
    s.text(48, 691, "a colliding client gets NO error: the protocol says it is the same client returning, and the loser's locks and opens are simply gone — six defects in v1.37.0 traced to this one cause.", "t4b c-red")
    s.legend(M + 4, 740, [("data", "data path"), ("ctl", "control"), ("red", "hazard / trust boundary")])
    s.h = 764
    return s


def plate_04():
    s = SVG(W, H, "flint-lean, drawn as a flow: the agent pod writes plain local files in a tree the CSI node plugin owns and bind-mounts; "
                  "the same tree is hostPathed into an unprivileged flint-sync worker, which checks it out from S3 at start and publishes "
                  "changed files at a boundary or on cadence. The lease, the broker and the boundary verbs are control, off the data path. "
                  "Gated mode makes durability and visibility separate events with one CAS.")
    fe = "lean"
    s.group(M, 22, 1020, 310, "one node", "no FUSE, no privileged container in the tenant pod, no webhook")
    s.box(44, 56, 430, 160, fe)
    s.text(62, 82, "the agent pod — tenant namespace", f"t2 c-{fe}")
    s.card(62, 98, 394, 100, "app container — the image is unchanged", [("m", "/workspace — plain local files"), "no S3 credential; git, sqlite and locks just work"], None)
    s.arrow(474, 150, 556, 150, fe, dashed=True, both=True)
    s.text(515, 134, "bind mount", "t4", "middle")
    s.card(560, 98, 210, 100, "the plugin-owned tree", [("m", "volumes/<vid>/tree"), "one tree, two views"], fe, "tint")
    s.arrow(770, 150, 846, 150, fe, dashed=True, both=True)
    s.text(808, 134, "hostPath", "t4", "middle")
    s.card(850, 56, 174, 160, "flint-sync — the worker", ["unprivileged, in a system namespace", ("b", "holds the credential the agent must not")], fe)
    s.card(44, 232, 980, 84, "s3.csi.chert.us — the node DaemonSet, one per node", ["kubelet blocks the pod on NodePublishVolume, so the checkout completes before the agent's first line; the final barrier runs at NodeUnpublish", ("b", "a published tree is live data: the plugin refuses to clean it up")], None, "panel")
    # publish / checkout
    s.box(1200, 22, 376, 310, None, "s3")
    s.arrow(1044, 118, 1196, 118, fe)
    s.arrow(1196, 150, 1044, 150, fe, True)
    s.alabel(1120, 100, "publish · at a boundary", fe)
    s.alabel(1120, 174, "checkout · at start", fe, "RPO = the last barrier")
    # S3
    s.text(1220, 50, "S3 — the durable copy", "t1")
    s.text(1220, 80, "<prefix>/files/<path>", "mono")
    s.text(1220, 98, "whole objects — a touched file is re-uploaded", "t4")
    s.text(1220, 130, ".flint/lean/  epoch · claim · manifest · inbox", "mono")
    s.text(1220, 148, "the lease · the owner · the last boundary · HITL", "t4")
    s.hair(1220, 166, 1556, 166)
    s.text(1220, 192, "the lease lives in the BUCKET, not in Kubernetes,", "t4b")
    s.text(1220, 210, "so it fences a second writer in any cluster.", "t4")
    s.text(1220, 242, "the manifest is the wall: ~277 B per entry, ~250k files.", "t4")
    s.text(1220, 274, "readers see the last published boundary,", "t4")
    s.text(1220, 292, "never your last write.", "t4")
    # the loop
    s.box(M, 354, W - 2 * M, 92, None, "panel")
    s.text(44, 382, "the loop — one agent, start to durable", "t1")
    steps = [("pod starts", "checkout first — never an empty tree"), ("the agent works", "local files at local speed"),
             ("echo > .flint/publish", "declares a boundary — the whole API"), (".flint/publish.ack", "ok = in S3 · partial · refused-fenced")]
    sw = (W - 2 * M - 44 - 3 * 14) / 4
    for i, (head, body) in enumerate(steps):
        x = 44 + i * (sw + 14)
        s.numdot(x + 12, 410, i + 1, fe)
        s.text(x + 34, 414, head, "t2" if i < 2 else "mono")
        s.text(x + 34, 432, body, "t4")
    # control · gated
    s.group(M, 468, 760, 96, "control — off the data path")
    s.text(44, 508, "lease in the bucket — one writer; a second syncer is refused, never merged", "t4")
    s.text(44, 526, "broker → short-lived keys → the worker, over a loopback door · consumers absent = deny", "t4")
    s.text(44, 544, "the boundary verbs are files: an agent that can write a file can declare a coherent point", "t4")
    s.card(808, 468, 768, 96, "gated mode — durable now, visible on ONE CAS", ["every changed file is uploaded as a new version at once: durable, and invisible; one CAS cites the whole set", ("b", "a reader sees the whole change or none of it; refused without a lag bound")], fe)
    s.legend(M + 4, 596, [("data", "data path"), ("ctl", "control"), ("red", "hazard / trust boundary")])
    s.h = 620
    return s


def plate_05():
    s = SVG(W, H, "flint-passthrough, drawn as a flow: every file operation in the tenant pod is intercepted by FUSE and served by an "
                  "unprivileged worker running mount-s3, which turns it into GET, PUT or LIST against the S3 prefix; nothing is buffered. "
                  "The node plugin's mount(2), the descriptor hand-off and the broker are control, off the data path. The bucket is "
                  "mounted as-is, with no flint control namespace.")
    fe = "pass"
    # ── the tenant pod ──
    s.group(M, 22, 452, 330, "tenant namespace", "PodSecurity restricted — and it stays that way")
    s.box(44, 56, 412, 190, fe)
    s.text(62, 82, "the tenant pod", f"t2 c-{fe}")
    for i, ln in enumerate(["volumes:", "  - name: data", "    csi:", "      driver: s3.csi.chert.us", "      volumeAttributes:", "        chert.us/mount: datasets"]):
        s.text(62, 106 + i * 17, ln, "mono")
    s.text(62, 228, "nine lines of pod spec — no sidecar, label, credential or privilege", "t4")
    s.card(44, 262, 412, 76, "FlintPassthroughMount — in the pod's namespace", [("m", "bucket · keyPrefix · readOnly · uid · consumers")], None)
    # ── the data path: pod ↔ worker ↔ S3 ──
    s.arrow(456, 120, 576, 120, fe, both=True)
    s.alabel(516, 104, "FUSE", fe, "every file op")
    s.card(580, 56, 300, 130, "mount-s3 worker — unprivileged", [("m", "serves the fd it was given"), "non-root · no caps · read-only rootfs · no SA token", ("b", "needs no privilege: mount(2) already happened")], fe)
    s.arrow(880, 120, 1214, 120, fe, both=True)
    s.alabel(1047, 104, "GET · PUT · LIST · per operation", fe, "no RPO — nothing is buffered")
    s.card(580, 214, 300, 118, "flint-s3-broker — the only standing credential", ["online TokenReview · registration · consumers", ("m", "→ short-lived keys, on a loopback door")], None)
    s.arrow(730, 214, 730, 190, None, True)
    s.box(1218, 56, 358, 276, None, "s3")
    s.text(1238, 84, "S3 — and it is just S3", "t1")
    s.text(1238, 114, "<prefix>/<key>", "mono")
    s.text(1238, 132, "objects appear as files, and nothing else is written", "t4")
    s.hair(1238, 150, 1554, 150)
    s.text(1238, 176, "no control namespace: a prefix someone else's", "t4")
    s.text(1238, 194, "tooling owns stays exactly as it was.", "t4")
    s.text(1238, 226, "durability, consistency and access control are", "t4b")
    s.text(1238, 244, "S3's, unmediated — flint adds and removes none.", "t4b")
    s.text(1238, 276, "one uid inside the pod; two pods on one prefix", "t4")
    s.text(1238, 294, "do not see each other.", "t4")
    # ── control: the node plugin ──
    s.group(M, 374, 816, 132, "s3.csi.chert.us — the node DaemonSet · control", "one privileged process per node — no S3 credential, no Secrets RBAC")
    s.arrow(250, 374, 250, 346, None, True)
    s.text(262, 364, "NodePublishVolume", "t4")
    s.arrow(730, 374, 730, 336, None, True)
    s.text(742, 364, "/dev/fuse fd", "t4")
    steps = [("resolve the CR", "in the token's namespace"), ("authorise the SA", "consumers — absent means deny"), ("mount(2) itself", "the one privileged act"),
             ("hand the fd over", "SCM_RIGHTS, to the worker"), ("bind into the pod", "one Bidirectional view")]
    sw = (816 - 40 - 4 * 12) / 5
    for i, (head, body) in enumerate(steps):
        x = 44 + i * (sw + 12)
        s.numdot(x + 12, 444, i + 1, fe)
        s.text(x + 32, 448, head, "t2")
        s.para(x + 32, 466, body, sw - 40)
    # ── what it is not ──
    s.box(M, 528, W - 2 * M, 54, None, "warn")
    s.text(48, 550, "NOT POSIX — no rename, no append, no in-place write   ·   NOT coordinated — two pods on one prefix do not see each other   ·   NOT per-user — one uid   ·   NOT self-healing — a dead mounter strands the pod on ENOTCONN", "t4b c-red")
    s.text(48, 570, "privilege: the node plugin, one per node · the workers, none · the tenant pod, none · the broker holds the only standing credential — the same chain flint-lean uses", "t4")
    s.legend(M + 4, 612, [("data", "data path"), ("ctl", "control"), ("red", "hazard / trust boundary")])
    s.h = 636
    return s


def plate_06():
    s = SVG(W, H, "flint-forge, drawn as a flow: an agent pod's stock git pushes and fetches over smart HTTP through the door, which "
                  "verifies the pod's ServiceAccount token and routes to one server pod per repository; there gitcgi and "
                  "git-http-backend serve, hooks hand every push to one syncer, and the syncer uploads packs, CASes one snapshot and "
                  "only then acknowledges. S3 holds a bare repository. The lease, the operator and the presigned levers are control.")
    fe = "forge"
    # ── the path ──
    s.card(M, 22, 250, 178, "the agent pod — any namespace", [("m", "helper: flint-forge-credential"), "stock git; the pod's token is the Basic password", ("b", "no bucket credential, no sidecar, no privilege")], fe)
    s.arrow(274, 110, 384, 110, fe, both=True)
    s.alabel(329, 94, "git smart HTTP", fe, "push ↑ · fetch ↓")
    s.card(388, 22, 270, 178, "the door — the lite gateway's git arm", ["TokenReview ≤ 60 s · consumers · routes · wakes", ("m", "forwards X-Remote-User only"), ("b", "stateless; holds a wake request ≤ 180 s")], fe)
    s.arrow(658, 110, 770, 110, fe, both=True)
    s.alabel(714, 94, "HTTP", fe, "+ X-Remote-User")
    s.box(774, 22, 466, 178, fe)
    s.text(792, 48, "one server pod per FlintRepo — emptyDir cache, idles to zero", f"t2 c-{fe}")
    s.card(792, 62, 208, 122, "git container", [("m", "gitcgi + git-http-backend"), "the hooks are the syncer binary", ("b", "holds no bucket credential")], None, strip=False)
    s.card(1012, 62, 210, 122, "syncer container", ["batches pushes; uploads packs four at a time", ("b", "the only writer of the bucket")], None, strip=False)
    s.arrow(1240, 110, 1352, 110, fe, both=True)
    s.alabel(1296, 90, "packs ↑ on push", fe, "restore ↓ at start")
    s.text(1296, 146, "RPO = last ack", "t4", "middle")
    s.box(1356, 22, 220, 178, None, "s3")
    s.text(1372, 48, "S3 — <prefix>/", "t1")
    for k, t in enumerate(["git/objects/pack/*", "git/snapshot — ONE CAS", "git/epoch · git/claim", "git/bundles · lfs/…", "files/<path> — export"]):
        s.text(1372, 74 + k * 17, t, "mono")
    s.text(1372, 176, "a bare repo; clonable, server down", "t4")
    # ── the push ──
    s.box(M, 224, W - 2 * M, 100, None, "panel")
    s.text(M + 22, 252, "the push — acknowledged means durable", "t1")
    steps = [("the hook hands over", "the commands, judged in order against local refs, the snapshot, the batch"),
             ("policy, twice", "pre-receive names the rule; the syncer enforces it"),
             ("renew the lease", "412 ⇒ this server fences itself and exits"),
             ("upload the packs", "content-named, immutable, four at a time"),
             ("ONE snapshot CAS", "If-Match: thirty refs move as one object"),
             ("ref transaction, then the report", "only THEN ok or ng — a crash before fails the push")]
    sw = (W - 2 * M - 44 - 5 * 14) / 6
    for i, (head, body) in enumerate(steps):
        x = M + 22 + i * (sw + 14)
        s.numdot(x + 12, 282, i + 1, fe)
        s.text(x + 32, 286, head, "t2")
        s.para(x + 32, 304, body, sw - 40)
    # ── control · export ──
    s.group(M, 346, 760, 104, "control — off the data path")
    s.text(44, 386, "lease heartbeat on its own task, gated on progress — a wedged pod loses its lease", "t4")
    s.text(44, 404, "operator: idle to zero on pushes only; the door arms a wake and holds the request", "t4")
    s.text(44, 422, "bundles and LFS answer with presigned URLs — those bytes never cross the pod", "t4")
    s.card(808, 346, 768, 104, "the export — a legible mirror, by the shipped lean binary", ["after the report, never a second writer of the snapshot; lean and passthrough readers mount main read-only", ("r", "never repaired: a foreign write is not noticed; a manifest-verifying reader refuses it, a raw mount takes it")], fe)
    # ── drilled · hazards ──
    s.box(M, 472, W - 2 * M, 54, None, "panel")
    s.text(48, 494, "drilled: 12 falsifiers green (EC2 2026-09-04, rights on Cilium 2026-09-05) · 10 GiB and 40 GiB pushes on real S3 · the takeover window closed against a real challenger", "t4b")
    s.text(48, 514, "open: a pack's parts upload one at a time · a full repack every 24 pushes re-uploads the repository · a roll mid-push costs that push · agents in another cluster are not drilled", "t4")
    s.box(M, 542, W - 2 * M, 44, None, "warn")
    s.text(48, 569, "nothing in-tree terminates TLS — the token rides two cleartext hops; X-Remote-User is a NetworkPolicy the operator renders only when told where the door runs; the server runs as root", "t4b c-red")
    s.legend(M + 4, 618, [("data", "data path"), ("ctl", "control"), ("red", "hazard / trust boundary")])
    s.h = 642
    return s


def plate_07():
    s = SVG(W, H, "Where an end user's token can and cannot go, per front end. One user launches agents on several clusters, each "
                  "agent holding a short-lived token that may differ per agent. For flint-lite the NFS wire has no field for it and "
                  "the hub authenticates nobody. For flint-lean and flint-passthrough the enforced identity is the pod's "
                  "kubelet-asserted ServiceAccount, verified online by a broker. For flint-forge it is the same ServiceAccount, "
                  "verified by the door's TokenReview and carried to the server as X-Remote-User. The user's token reaches storage "
                  "in no case.")
    s.box(M, 16, W - 2 * M, 78, None, "panel")
    s.text(M + 22, 42, "The premise — one human, many clusters, many tokens", "t1")
    s.text(M + 22, 62, "One end user launches agents on clusters A, B and C; each agent holds THAT user's token, short-lived, from an IdP, and they may all differ. Where does it go, and what does storage actually check?", "t4")
    s.text(M + 22, 80, "The answer, for all four: it does not reach the data path. Every front end authenticates the WORKLOAD, never the human behind it.", "t4b c-red")
    lanes = [
        ("lite", "flint-lite", "ONE hub, reached from every cluster",
         ("Nothing on the wire carries a user identity.", "NFS has no field for a bearer token and no place to add one. krb5 exists in the server, is not surfaced by the chart, and is not an IdP token anyway.", True),
         ["the user's token — and it never sends it anywhere, because no door on the data path would read it", ("b", "The pod is not even the client. The NODE is:"), "one mount and one identity per node, asserting its own uid and hostname"],
         ("the door — :2049, AUTH_SYS", ["The client asserts its own uid and the server takes it. No token is validated because none is presented.", ("m", "security.enforcePermissions: false"), "is the default — the mode is evaluated, LOGGED, and the operation allowed anyway; even enforced, the hub holds CAP_DAC_OVERRIDE.", ("r", "sec=sys buys IDENTITY, not ENFORCEMENT.")], True),
         ("what the hub sees", ["a client NAME — the hostname — and a self-declared uid. Not a user, not a cluster: two clusters sharing a node name are ONE client, and the protocol drops the incumbent's locks silently.", "The file API's bearer is per-SHARE, not per-user."]),
         ("what isolates user A from user B", [("r", "Nothing inside the hub."), "Draw the boundary outside it: one hub per project — one CR, one prefix, one coherence domain — and network reachability by peering, SGs or a gateway. networkPolicy cannot draw it."])),
        ("lean", "flint-lean", "one syncer per workspace, in every cluster",
         ("The identity IS checked — it is just not the user's.", "One CSI driver and one broker PER CLUSTER, because TokenReview runs against the LOCAL API server. Clusters meet in the bucket, not at each other's brokers.", False),
         ["the user's token — unused for storage —", ("b", "and NO S3 credential at all."), "The credential lives in the worker, in a system namespace, on a loopback door the agent container cannot reach. A compromised agent gets the tree it already had — not the bucket."],
         ("the door — flint-s3-broker", ["validates the POD's ServiceAccount token: kubelet-minted, pod-bound, audience s3.csi.chert.us", "1 · TokenReview, ONLINE — a deleted pod's token dies within 60 s, not at exp", "2 · a session name matching a LIVE registration the plugin made for this pod-uid + CR — a pod cannot self-mint it", "3 · the CR lists the SA in consumers, in the TOKEN's namespace"], False),
         ("what S3 sees", ["short-lived keys scoped to the project's prefix, minted per pod.", ("b", "Never the user. Never a standing key held by anything a tenant can reach."), "The broker's rest backend is the ONE seam where a user-scoped token could be honoured — it carries the pod's identity today."]),
         ("what isolates user A from user B", [("b", "The prefix, and the lease."), "one workspace = one prefix and the keys are scoped to it; the subtree lease admits exactly ONE writer and lives in the BUCKET, so it holds across clusters; consumers lists who may mount it — absent means DENY"])),
        ("pass", "flint-passthrough", "the same identity chain as flint-lean",
         ("Same broker, same TokenReview, same consumers gate.", "The two front ends differ in what the worker RUNS, not in who is trusted. What differs is what happens INSIDE the pod.", False),
         ["the user's token — unused for storage —", ("b", "and NO S3 credential at all."), "The pod is given nothing: no sidecar, no label, no privilege, no webhook — just a csi: volume naming a CR. The smallest blast radius of the four, and the least it can promise."],
         ("the door — flint-s3-broker", ["identical to the lean lane: online TokenReview, a registration binding the pod cannot self-mint, and the CR's consumer list.", "Every issuance leaves an audit line:", ("m", "(ns, sa, pod-uid, cr, expiry)"), "One broker per cluster, again — every cluster's broker points at the same store, so the bucket still trusts one principal."], False),
         ("what S3 sees", ["short-lived keys scoped to the bucket and prefix in the CR.", ("r", "And inside the pod: ONE uid."), "The mount reports a single owner for every file, because NodePublish never sees the pod's securityContext — two processes, or two users, are indistinguishable at the mount."]),
         ("what isolates user A from user B", [("b", "The prefix and the CR — nothing finer than a POD."), "consumers decides which ServiceAccounts may mount it; the keys are scoped to the prefix; there is no per-user view, so give each user their own CR and their own pod"])),
        ("forge", "flint-forge", "one door, one server pod per repository",
         ("The pod's identity, proven to the CLUSTER rather than to the store.", "TokenReview at the door — so a pod in another cluster carries nothing this door can verify. Cross-cluster agents fall back to the human bearer path.", False),
         ["the user's token — unused —", ("b", "and a projected ServiceAccount token, audience forge.chert.us,"), "presented as the Basic password by a helper that re-reads it on every call. No bucket credential of any kind: those land on the syncer container of the server pod, only."],
         ("the door — flint-hub-gateway, Door::Git", ["TokenReview per session, cached ≤ 60 s by token hash; refusals cached, transport failures not.", "spec.consumers must list the ServiceAccount; a credential-less peer is answered 401 before any wake.", ("m", "X-Remote-User: <sa>  →  REMOTE_USER"), "read by pre-receive and by the syncer: the same principal, two enforcers, one policy document."], False),
         ("what the server sees", ["a ServiceAccount — which many pods share.", ("b", "agentPattern bounds a branch NAME, not its owner:"), "one pod can push over a sibling's agent/* branch. Protected refs, refs/for proposals and non-fast-forward refusal bind per principal; the server never sees the bucket credential the agent never had."]),
         ("what isolates user A from user B", [("b", "The repository: its pod, its prefix and claim, its consumers, its branch policy."), ("r", "The X-Remote-User boundary is a NetworkPolicy admitting only the door — opt-in (door.namespace), and only where the CNI enforces it."), "Otherwise reaching the port is the authorisation."])),
    ]
    LY, LH_ = 104, 184
    for i, (fe, name, sub, (lead, body, hazard), holds, (dname, ditems, dwarn), (sname, sitems), (iname, iitems)) in enumerate(lanes):
        y = LY + i * (LH_ + 4)
        s.rect(M, y, W - 2 * M, LH_, "bx", rx=10)
        s.strip(M, y, W - 2 * M, fe, r=10)
        # lane head
        s.text(M + 18, y + 30, name, f"t2 c-{fe}")
        s.text(M + 18, y + 48, sub, "t4")
        s.hair(M + 18, y + 58, M + 218, y + 58)
        yy = s.para(M + 18, y + 78, lead, 200, "t4b c-red" if hazard else "t4b")
        s.para(M + 18, yy + 2, body, 200)
        # boxes
        bw = [286, 336, 300, 300]
        bx = M + 236
        s.card(bx, y + 14, bw[0], LH_ - 28, "what the agent holds", holds, None, pad=14, lh=14)
        s.arrow(bx + bw[0], y + 70, bx + bw[0] + 26, y + 70, fe)
        bx += bw[0] + 26
        s.card(bx, y + 14, bw[1], LH_ - 28, dname, ditems, None, "warn" if dwarn else None, pad=14, lh=14)
        s.arrow(bx + bw[1], y + 70, bx + bw[1] + 26, y + 70, fe)
        bx += bw[1] + 26
        s.card(bx, y + 14, bw[2], LH_ - 28, sname, sitems, None, pad=14, lh=14)
        bx += bw[2] + 12
        s.card(bx, y + 14, bw[3], LH_ - 28, iname, iitems, None, "panel", pad=14, lh=14)
    s.legend(M + 4, 872, [("lite", "flint-lite"), ("lean", "flint-lean"), ("pass", "flint-passthrough"), ("forge", "flint-forge"), ("red", "hazard / trust boundary")])
    return s


def plate_08():
    s = SVG(W, H, "What each front end requires you to install in each cluster of a fleet, and what an attacker who compromises an "
                  "agent pod actually reaches. flint-lite installs nothing in consumer clusters but concentrates one hub; flint-lean "
                  "and flint-passthrough install a CSI driver and broker in every cluster and meet in the bucket; flint-forge installs "
                  "a door and the repository servers in one cluster, and an agent elsewhere must be able to prove its token to that "
                  "cluster's apiserver.")
    cols = [
        ("lite", "flint-lite — one hub, N clusters",
         ("the HUB cluster — install here, once per project", ["flint-lite-operator (or the chart, one release per hub); the hub Deployment, Service, ConfigMap and PVC it renders; optionally flint-hub-gateway — one door holding ONE credential for the fleet", ("b", "This cluster is now a dependency of every other one.")]),
         ("every CONSUMER cluster — install NOTHING", ["The data path is the node's own kernel NFS client: no driver, no DaemonSet, no CRD, no flint image. At most a PV and PVC, or the stock nfs-subdir provisioner, which never carries a byte.", ("b", "The cheapest fleet onboarding of the four, by far.")]),
         ("what it costs you", ["One pod is the coherence authority, so one pod is also the single point of failure and the NIC ceiling. A hard NFS mount against a suspended hub HANGS — something must write the wake annotation before an address is handed out."])),
        ("lean", "flint-lean — N syncers, one bucket",
         ("EVERY cluster — install the same three things", ["flint-s3-csi — the node DaemonSet, the workers namespace and its admission policy; flint-s3-broker — one per cluster, because TokenReview runs against the LOCAL API server; flint-lean — the CRD plus a thin controller"]),
         ("and no cluster depends on any other", ["The clusters never talk. They meet in the bucket, and the arbitration that keeps them honest lives there too — a CAS on the subtree lease no cluster can route around.", ("b", "Single-writer holds ACROSS clusters, which is the one guarantee lite's admission check cannot make.")]),
         ("what it costs you", ["A per-cluster install kept in version step, and a broker per cluster to configure and watch. A lean volume still mounts with its controller absent — the syncer claims the lease itself — so the controller is an operability component, not a data-path one."])),
        ("pass", "flint-passthrough — N mounts, one bucket",
         ("EVERY cluster — install two things", ["flint-s3-csi — the same node DaemonSet and broker as lean; one install serves both front ends. flint-passthrough — the CRD, and nothing else: no workload in the chart, no controller behind the CR.", ("b", "Nothing about a passthrough mount converges, so there is nothing to reconcile.")]),
         ("the fleet story is: there isn't one", ["Pods in twenty clusters can mount one prefix and none of them coordinates, because none has anything to coordinate. S3 is already a multi-cluster service.", ("b", "Scaling out is free precisely because nothing is shared except the objects.")]),
         ("what it costs you", ["Everything the other three provide. No POSIX, no rename, no boundary, no manifest, no single-writer rule. Two pods writing one key race, and the loser's write is simply gone — with no cell recording that it happened."])),
        ("forge", "flint-forge — one door, one pod per repository",
         ("the HOME cluster — install here", ["flint-forge-operator and the FlintRepo CRD; per repository, the Deployment, headless Service, ConfigMap and — when door.namespace is set — a NetworkPolicy it renders; the door: door.deploy=true, or a lite gateway that has the git arm", ("b", "Agents in this cluster need two lines of pod spec and stock git.")]),
         ("another cluster — an identity problem, not an install", ["Nothing of flint's is in the data path, but the door proves a token against ITS apiserver, so an agent elsewhere carries nothing it can verify. The bucket is a rendezvous for READERS only: a dumb clone works with the server down; a second server is fenced into a straggler that exits.", ("b", "Cross-cluster agents fall back to the human bearer path. Not drilled.")]),
         ("what it costs you", ["One pod per repository in the data path — egress binds before CPU on a clone storm, and the lever (bundles from S3) needs the CLIENT to opt in. The server runs as root in both images."])),
    ]
    for i, (fe, title, home, cons, cost) in enumerate(cols):
        x = col_x(i)
        s.header(x, 16, COL4, fe, title)
        s.card(x, 60, COL4, 150, home[0], home[1], fe, "tint")
        s.card(x, 222, COL4, 150, cons[0], cons[1], fe)
        s.card(x, 384, COL4, 110, cost[0], cost[1], None, "panel")
        s.arrow(x + COL4 / 2, 494, x + COL4 / 2, 526, fe)
    s.box(M, 530, W - 2 * M, 44, None, "s3")
    s.text(M + 22, 558, "S3 — the one thing every cluster, every front end and every fleet shape has in common. It is where the data lives; everything above is a way of reaching it.", "t2")
    # blast radius
    s.box(M, 590, W - 2 * M, 266, None, "panel")
    s.text(M + 22, 618, "Blast radius — an attacker owns one agent pod. What do they now have?", "t1")
    s.text(M + 22, 636, "Assume the agent container is fully compromised and holds the user's token. This is the question the token cannot answer, because storage never sees it.", "t4")
    cards = [
        ("lite", "flint-lite — the whole share", True,
         ["The pod already has the mount. AUTH_SYS means it asserts its own uid, and the default POSIX check LOGS rather than refuses — so it reads and writes every file in the share, as anyone. It cannot reach the BUCKET: the hub holds that credential. It can reach any other hub its network can route to, and the next hub cannot tell it apart from a legitimate client.", ("r", "Containment = one hub per project + a real network boundary. Everything sharing a hub shares a fate.")]),
        ("lean", "flint-lean — the workspace, not the bucket", False,
         ["The pod has the tree it was already working in and no S3 credential — the worker does, in another namespace, behind a loopback door the agent cannot reach. The attacker gets the files, published under the workspace's own prefix; not the bucket, other workspaces, or a credential that outlives the pod.", ("b", "The honest ceiling: a deposed syncer's writes land as UNCITED versions a raw-key reader can still see — refused at the control plane, not fenced on the data plane.")]),
        ("pass", "flint-passthrough — the prefix, as mounted", False,
         ["The pod has whatever the mount has: the prefix in the CR, at the CR's readOnly setting, with keys that are short-lived, scoped, and never in the pod's env. The tightest of the four, for a dull reason: there is nothing else there to take.", ("b", "Scope the CR and you have scoped the breach; readOnly: true is a real control. The residual is the one uid: pod-level isolation is user-level isolation.")]),
        ("forge", "flint-forge — a ServiceAccount's pushes", False,
         ["No bucket credential of any kind — those sit on the syncer container of the server pod — and no privilege: no sidecar, no /dev/fuse, no webhook. What it gains is the pod's projected token: push as that ServiceAccount to repositories listing it, bounded by policy to agentPattern branches and refs/for proposals. Revocation is pod deletion or SA rotation, felt within the ≤ 60 s review cache.", ("b", "Not smaller: a shared ServiceAccount reaches every sibling's agent/* branch, and the server pod runs as root.")]),
    ]
    cw = (W - 2 * M - 44 - 3 * GAP) / 4
    for i, (fe, head, warn, items) in enumerate(cards):
        s.card(M + 22 + i * (cw + GAP), 648, cw, 194, head, items, fe, "warn" if warn else None, lh=14)
    s.legend(M + 4, 872, [("lite", "flint-lite"), ("lean", "flint-lean"), ("pass", "flint-passthrough"), ("forge", "flint-forge"), ("data", "data path"), ("red", "hazard / trust boundary")])
    return s


def plate_09():
    s = SVG(W, H, "S3 is the durable store for all four front ends. Two views exist: the live view — a hub's port 2049, a lean "
                  "workspace's local disk, a forge clone's working tree — which is strongly consistent, and the published view in the "
                  "bucket, which is a set of RPO-consistent whole-file snapshots or, for forge, a bare git repository. The control "
                  "namespace holds the cells that make concurrent access safe, and the recovery point differs per front end.")
    s.h = 800
    hw = (W - 2 * M - GAP) / 2
    s.card(M, 18, hw, 190, "The LIVE view — strong, and never in the bucket",
           ["a hub's port 2049 and its file API · a lean workspace's local disk · a forge clone's working tree, and the server's local repository",
            ("b", "close-to-open · byte-range locks · atomic rename · no RPO lag"),
            "Reads and writes land on the working set, through whatever holds coherence for it — the hub process, the agent's own kernel, or git.",
            "Who reads it: front doors, editors, agents — anything touching a tree that is being changed right now. The hub's live view offers one thing no real S3 can: assembly under a temp name completed by an ATOMIC RENAME under a deny-both OPEN. A lean or forge live tree is visible to exactly one pod."], None)
    s.card(M + hw + GAP, 18, hw, 190, "The PUBLISHED view — the bucket, and the durable copy",
           ["RPO-consistent snapshots per subtree, made of untorn whole-file generations — or, for forge, a bare repository whose refs move as ONE CAS'd object.",
            ("b", "whole objects · versioned or content-named · read-only against a live subtree"),
            "A reader never sees half a file or half a batch. It may well see a file from before your last write — that is the contract, not a defect. Anything that is not a mount reads this view as plain S3, with no flint server in the read path at all.",
            ("r", "Do not write into a live subtree from the console: a hand-made PUT under a prefix a hub, syncer or server owns is overwritten at the next flush, reported as a conflict, or silently uncited.")], None, "warn")
    # inside the prefix
    s.box(M, 224, W - 2 * M, 272, None, "s3")
    s.text(M + 22, 252, "Inside the prefix — the bytes, and the cells that make concurrency safe", "t1")
    s.text(M + 22, 272, "Every cell below exists to answer one question that cannot be answered from the object listing alone.", "t4")
    cw = (W - 2 * M - 44 - 4 * GAP) / 5
    boxes = [
        ("lite", "flint-lite writes", [("m", "<prefix>/<path>"), "the published generations", ("m", ".flint/epoch"), "who is the hub right now? — a lease", ("m", ".flint/manifest"), "what is the whole tree? — DR from one GET", ("m", ".flint/owner")]),
        ("lean", "flint-lean writes", [("m", "<prefix>/files/<path>"), "the workspace, file for file", ("m", ".flint/lean/epoch · claim"), "who may write this subtree? — one holder, by CAS, across every cluster", ("m", ".flint/lean/current"), "what does the last boundary contain?", ("m", ".flint/lean/inbox · conflicts/")]),
        ("pass", "flint-passthrough writes", [("m", "<prefix>/<key>"), ("mm", "the objects, and nothing else"), ("b", "There is no control namespace here, and that is the entire feature."), "A prefix owned by somebody else's tooling stays exactly as it was; flint claims no lease and leaves no trace. You also get no answer to any question on the left."]),
        ("forge", "flint-forge writes", [("m", "git/objects/pack/*"), "immutable, content-named — never overwritten", ("m", "git/snapshot"), "the refs, packs, bundles and exported commit — ONE CAS'd pointer", ("m", "git/epoch · git/claim"), "who is the server? whose prefix?", ("m", "lfs/objects/<oid>"), "and, under its OWN prefix, the export"]),
        (None, "Reserved, and enforced", [".flint is control state: a client file under it can never shadow a control object, and a NESTED .flint/ is another share's namespace — never tiered into this one. Both are tested, because both were once a way to overwrite a live share's lease.", ("b", "The file-shaped formats are mutually convertible, and forge's export is one of them: choosing today does not strand data.")]),
    ]
    for i, (fe, head, items) in enumerate(boxes):
        s.card(M + 22 + i * (cw + GAP), 286, cw, 196, head, items, fe, "panel" if fe is None else None, lh=14)
    # durability
    s.box(M, 512, W - 2 * M, 268, None, "panel")
    s.text(M + 22, 540, "What “durable” actually means here — the recovery point, per front end, in the language of a hard pod kill", "t1")
    cw4 = (W - 2 * M - 44 - 3 * GAP) / 4
    cards = [
        ("lite", "flint-lite — RPO is the flush cadence", ["The PVC is a working set the hub flushes on a timer; losing the disk is a REBUILD from the bucket, not a loss. A cold read hydrates on demand and both doors say so: NFS4ERR_DELAY, or 503.", ("b", "Hibernation deletes the PVC, so it is VERIFIED, never assumed:"), "the operator scales back to one, polls until the hub reports rpoClean, and only then deletes. A share with no bucket reports rpoClean: null — a refusal, not a pass."]),
        ("lean", "flint-lean — RPO is the last barrier", ["A hard kill loses at most floorSecs (default 60 s); a graceful stop loses nothing, because preStop runs a final barrier. A restart over a live tree RESUMES from its baseline.", ("b", "Publish at a breakpoint and the RPO becomes zero, knowably:"), ("m", "echo > .flint/publish → publish.ack = ok"), "means the bytes are in S3 — the difference between “probably durable” and “durable, and I was told so”."]),
        ("pass", "flint-passthrough — there is no RPO", ["Nothing is buffered on flint's behalf, so nothing can be lost by flint. A completed PUT is durable the instant S3 says so; an interrupted one did not happen.", ("b", "The strongest durability story of the four — and strongest because flint is not in it."), "The trade: a write is a whole new object, there is no rename to make it atomic against a reader, and two writers racing one key resolve as S3 resolves them — last writer wins, unrecorded."]),
        ("forge", "flint-forge — RPO is the last acknowledged push", ["No path acknowledges a push the bucket does not hold: the pack is uploaded, the snapshot CAS lands, the ref transaction applies, and only THEN is ok reported. A crash between the CAS and the report fails the push at the client; the restart restores from the snapshot and passes fsck.", ("b", "The unit is a push. Uncommitted work has no RPO —"), "git's contract, which forge keeps; a harness that wants one runs a snapshot script in its own pod, force-pushing refs/wip/<pod>."]),
    ]
    for i, (fe, head, items) in enumerate(cards):
        s.card(M + 22 + i * (cw4 + GAP), 556, cw4, 212, head, items, fe, lh=14)
    s.legend(M + 4, 794, [("lite", "flint-lite"), ("lean", "flint-lean"), ("pass", "flint-passthrough"), ("forge", "flint-forge"), ("red", "hazard / trust boundary")])
    return s


def plate_10():
    s = SVG(W, H, "The security posture of the four front ends across seven questions: how a writer proves who it is, what "
                  "protects the wire, what a credential-less peer can do, what keeps tenants apart, whether two sessions on one "
                  "project can hold different rights, what an attacker who owns an "
                  "agent pod gains, and whether the posture fails closed under operator error. flint-lite's port 2049 authenticates "
                  "nobody and its boundary is the network; flint-lean and flint-passthrough prove the pod's ServiceAccount to a broker "
                  "and hand out short-lived scoped keys; flint-forge proves the same ServiceAccount at a door and rides two cleartext "
                  "hops inside the cluster.")
    header = [("flint-lite", "lite"), ("flint-lean", "lean"), ("flint-passthrough", "pass"), ("flint-forge", "forge")]
    colw = [332, 332, 332, 336]
    rows = [
        ("Who a writer must prove it is", "before storage accepts an operation",
         [[("r", "Nobody, on port 2049."), "AUTH_SYS: the client asserts its own uid and the server takes it. Identity is self-declared, not proven; the file API's bearer is per-share. krb5 exists in the server and is not surfaced by the chart."],
          [("b", "The pod's ServiceAccount, online."), "kubelet-minted, pod-bound, audience s3.csi.chert.us; TokenReview at the broker, a registration the pod cannot self-mint, and the CR's consumers list. The agent container holds no key at all."],
          [("b", "The pod's ServiceAccount, online."), "The identical chain: same broker, same TokenReview, same registration binding, same consumers gate, same audit line. Inside the pod every process is one uid."],
          [("b", "The pod's ServiceAccount, at the door."), "A projected token, audience forge.chert.us, as the Basic password; TokenReview per session, cached ≤ 60 s by hash. The principal is the SA, which many pods share — agentPattern bounds a branch name, not its owner."]]),
        ("What protects the wire", "confidentiality and integrity in transit",
         [[("r", "Cleartext RPC."), "sec=sys carries no per-user authentication and no encryption; RPC-with-TLS is prospective. Reachability is the boundary, so draw it at the network layer — peering, security groups, a gateway."],
          [("b", "Nothing of flint's crosses the network."), "The agent writes local disk; the worker publishes to the S3 endpoint over its TLS. The credential door is a loopback socket on the node; the broker is reached in-cluster and answers keys, not bytes."],
          [("b", "S3 over TLS, unmediated."), "mount-s3 talks to the endpoint directly; the FUSE descriptor crosses a node-local socket; keys arrive on a loopback door, never as pod env. No flint server is on the read path."],
          [("r", "Two cleartext hops, as built."), "HTTPS at the door is a line in the diagram; nothing in-tree terminates it, and the git container's runner listens on 8080 with no TLS. The pod's audience-bound token rides both hops as a Basic password — a network observer inside the cluster holds a live credential. Server-to-S3 is TLS."]]),
        ("What a reachable peer can do", "with no credential at all",
         [[("r", "Open a full NFSv4.1 session."), "Port 2049 needs no credential: the defence is caps — state-table quota, slot cap, idle deadline, READ clamp — not authentication. A merely-reachable peer holds a stateful session."],
          [("b", "Very little."), "No flint door sits on the data path; the broker refuses without a valid TokenReview, and S3 refuses unsigned requests. What it can flood is the broker's TokenReview against the local apiserver."],
          [("b", "Very little."), "The same: the reachable service is the broker, and the store refuses unsigned requests. The node plugin's socket is kubelet's, not the network's."],
          [("b", "A 401 before any wake"), "— authentication precedes the repository's wake, so a peer cannot scale a parked repository up, and a refused token is cached. The lever it keeps: every novel bad token is one TokenReview; behind the door the runner sets no body limit and no timeout of its own (the door's idle bound is the one cut), and a wake holds a door slot up to 180 s."]]),
        ("What keeps tenants apart", "reading or writing another's data",
         [[("r", "The network, and one hub per project."), "No auth on 2049 means the boundary is reachability, which networkPolicy cannot draw for remote clusters; one CR, one prefix, one coherence domain, and everything sharing a hub shares a fate."],
          [("b", "The prefix, its claim and its lease."), "Keys are scoped to the workspace prefix; the claim refuses a foreign project on a reused prefix; the lease admits one writer and lives in the bucket, so it holds across clusters; consumers absent means deny."],
          [("b", "The prefix and the CR."), "consumers decides which ServiceAccounts may mount it, keys are scoped to the bucket and prefix in the CR, and the CR lives in the pod's own namespace. Nothing finer than a pod: one uid per mount."],
          [("b", "The repository."), "Its own pod, its prefix with a claim that refuses a foreign projectId, arbitration of overlapping prefixes at the operator — export prefixes included — consumers at the door, and a per-principal branch policy read by two enforcers.", ("r", "X-Remote-User is the boundary, and it is a NetworkPolicy: opt-in, and only where the CNI enforces it.")]]),
        ("Can two sessions differ?", "in rights: one project, one reader, one writer",
         [[("r", "No — only a client-side ro mount."), "A read-only claim gets ro, and the CSI publish forces it over operator options; but the server authenticates nobody, so any pod that reaches 2049 mounts rw and asserts uid 0. enforcePermissions checks a claimed uid: defence against accident, not against a session that wants to write."],
          [("r", "No."), "A workspace is one writer: the publish path hardcodes read-write, the CRD has no readOnly, and the credential is one key per project. A second lean pod on the same prefix does not become a reader — it waits to become the writer. The read-only session is a passthrough mount of files/, readOnly (page 12)."],
          [("b", "Yes, per pod."), "Two pods on one CR, one declared readOnly: --read-only on mount-s3 plus a read-only bind mount, and the pod never holds the key. The broker gates by consumers and hands both pods the SAME scope — one static key or one role ARN. The REST backend is told the ServiceAccount, so an external service can scope the key per principal; nothing in-tree does."],
          [("b", "Yes, per ServiceAccount."), "consumers grants read; the branch policy decides writes per principal, in pre-receive and again in the syncer. A reader is listed in consumers with every ref protected and only the writer's SA in pushers; refs/for stays closed without mergeInto.", ("r", "Until a branches block exists, every consumer may push.")]]),
        ("What an owned agent pod gains", "and how fast the gain is revoked",
         [[("r", "The whole share, as anyone."), "It has the mount, asserts any uid, and the permission check logs by default. Not the bucket — the hub holds that credential. Nothing per-client to revoke; containment is the network boundary."],
          [("b", "Its own workspace, and nothing that outlives it."), "No S3 credential in the agent container in either deployment mode; the sidecar holds it, unprivileged. Revocation is killing the token or the pod. Ceiling: a deposed writer's uncited versions remain readable by a raw key."],
          [("b", "The prefix, as mounted."), "Short-lived scoped keys it never sees, at the CR's readOnly setting; no sidecar, no privilege. Scope the CR and you have scoped the breach; the residual is the single uid."],
          [("b", "Its ServiceAccount's push rights."), "No bucket credential — they land on the syncer container only — and no privilege. It can push as that SA to repositories listing it, within policy; revoked by pod deletion or SA rotation within the review cache. Ceiling: a shared SA reaches every sibling's agent/* branch, and the server runs as root."]]),
        ("Does it fail closed?", "secure defaults, refusal over silent degradation",
         [[("r", "No, by default."), "security.enforcePermissions is false — evaluate, log, allow — and even enforced the hub holds CAP_DAC_OVERRIDE. SECINFO now advertises AUTH_SYS first and AUTH_NONE last, pinned by a test, so a stock mount negotiates sec=sys."],
          [("b", "Detection becomes refusal."), "A syncer that lost its lease answers refused-fenced rather than publishing; the plugin refuses to clean up a published tree; the chart refuses to render on credential misconfig; gated mode is refused without a lag bound."],
          [("b", "Deny is the default."), "consumers absent means deny; the ValidatingAdmissionPolicy — not the namespace label — fences the workers namespace to the plugin's own node; a dead mounter strands the pod on ENOTCONN rather than serving stale bytes."],
          [("b", "Closed where it counts, open on two defaults."), "An unparseable policy refuses; a MISSING pre-receive does not open the repository; a snapshot naming a missing pack refuses to serve.", ("r", "No door.namespace renders no NetworkPolicy, and an absent branches block is permissive: both documented, neither refuses.")]]),
    ]
    bottom = s.table(M, 18, colw, header, rows, lab_w = W - 2 * M - sum(colw), min_row=0)
    yy = s.para(M, bottom + 26, "Read it down a column. The identity chain lean and passthrough share is the strongest thing on the page, and forge borrows its shape at the door; the hub's is the weakest, and its boundary is drawn with the network. Every cell is the deck's own claim, made on the pages before this one, or the approach radar's verified Security cell.", W - 2 * M)
    s.legend(M + 4, yy + 14, [("lite", "flint-lite"), ("lean", "flint-lean"), ("pass", "flint-passthrough"), ("forge", "flint-forge"), ("red", "hazard / trust boundary")])
    s.h = int(yy + 30)
    return s


def plate_11():
    s = SVG(W, H, "The four front ends compose by prefix, and the composition is in the reading, never the writing. One agent pod holds "
                  "a forge clone for its code, a lean workspace for its hot working set, a passthrough mount for datasets and a hub "
                  "share for what several pods edit live; forge exports main into its own prefix as a real lean workspace that lean "
                  "and passthrough readers mount. One prefix has one writer — a mechanism within each product, a convention across "
                  "them, because forge and lean arbitrate on different cells and never meet — and an export is a mirror that is never "
                  "repaired: a foreign overwrite stands and is served to any reader that cannot verify it, a foreign delete is refused and never restored.")
    # ── one agent pod, four doors ──
    s.group(M, 22, 560, 470, "one agent pod", "four attachments; a stock image with no flint code in it")
    attach = [
        ("forge", "/src — a git clone", "code and history; a push is durable; a merge is a push to refs/for", "the door → the repository pod", "A", "repo/"),
        ("lean", "/workspace — a lean mount", "the hot working set: plain local files, one writer, a boundary it can name", "flint-sync → the bucket", "C", "ws/<agent>/"),
        ("pass", "/data — a passthrough mount", "read-mostly datasets, weights, artifacts — the bucket as-is, readOnly", "mount-s3 → the bucket", "D", "datasets/"),
        ("lite", "/shared — a hub share", "what several pods must edit LIVE: coordination files, sqlite, cross-pod locks", "the kernel NFS client → the hub", "E", "shared/"),
    ]
    for i, (fe, head, body, via, letter, prefix) in enumerate(attach):
        y = 56 + i * 108
        s.card(44, y, 372, 96, head, [body, ("b", via)], fe, "tint", lh=14)
        s.arrow(416, y + 48, 470, y + 48, fe)
        s.card(474, y + 16, 102, 64, "", [("b", letter), ("m", prefix)], fe, pad=12)
    s.card(M, 506, 560, 84, "a build-farm pod, elsewhere", ["mounts B/files/ — forge's export of main — read-only through lean or passthrough and never runs git. That is the composition: several readers of one writer's prefix.", ("b", "It writes nothing there. See the hazard on the right.")], "pass", lh=14)
    # ── the bucket ──
    s.box(608, 22, 470, 562, None, "s3")
    s.text(628, 50, "one bucket, five prefixes, one writer each", "t1")
    s.text(628, 70, "letters match the attachments on the left; the arrow is the only bridge", "t4")
    s.hair(628, 82, 1058, 82)
    pre = [
        ("forge", "A  repo/ — writer: the syncer", [("m", "git/objects/pack/*  git/snapshot"), ("m", "git/epoch · git/claim  lfs/objects/")]),
        ("forge", "B  repo-main/ — writer: the syncer, via the shipped flint-sync", [("m", "files/<path>  .flint/lean/{epoch,current,chunks/}"), ("b", "a REAL lean workspace; readers: lean, and passthrough aimed at B/files/")]),
        ("lean", "C  ws/<agent>/ — writer: the lean sidecar", [("m", "files/<path>  .flint/lean/{epoch,claim,current}")]),
        ("pass", "D  datasets/ — writer: whoever owns them", [("m", "<key>  — objects and nothing else; passthrough reads")]),
        ("lite", "E  shared/ — writer: the hub", [("m", "<path>  .flint/epoch · manifest · owner")]),
    ]
    y = 96
    ys = []
    for fe, head, items in pre:
        h = 34 + 15 * len(items) + 12
        s.card(628, y, 430, h, head, items, fe, lh=15)
        ys.append(y + h / 2)
        y += h + 10
    s.path(f"M{1058} {ys[0]} C 1090 {ys[0]}, 1090 {ys[1]}, {1058} {ys[1]}", "forge")
    s.text(1076, (ys[0] + ys[1]) / 2 + 4, "", "t4")
    s.text(628, y + 12, "A → B is the export: after a push moves main, the syncer materialises the tree and runs", "t4")
    s.text(628, y + 27, "flint-sync barrier over it. Nothing else crosses a prefix boundary, in either direction.", "t4")
    # ── the rule, and the hazard ──
    s.card(1102, 22, 474, 270, "One writer per prefix — where it is a mechanism, and where it is not",
           [("b", "Within a product, a mechanism:"), "an epoch lease plus one CAS'd pointer — forge's on git/epoch, lean's on .flint/lean/epoch. A second writer of the same product does not acquire: it observes the standing lease and waits, superseding only after six quiet polls have shown the holder dead; drilled both ways.",
            ("r", "Across products, a convention: the cells are disjoint."), "Point forge and lean at ONE prefix and they never meet — both acquire epoch 1 under different holders; no 412, no fence, no log line (drill C1). The operator's arbitration reasons over FlintRepos only: it refuses an export aimed at another repository and sees nothing involving a lean CR.",
            "The one arbitrated direction: a read-write lean sidecar on the EXPORT prefix contends on the same cell — and until today that wedged the repository, because the syncer awaited the blocked export inline with its own heartbeat (C2; now bounded by a timeout with backoff — pushes still stall for that long)."], None, lh=14)
    s.card(1102, 306, 474, 278, "An export is a mirror that is never repaired",
           ["The barrier computes uploads and deletes from a LOCAL scan against a LOCAL baseline; the only remote thing it reads is the pointer's etag. A foreign write into B moves no pointer and changes no local file, so no later export notices it (C3).",
            ("r", "A reader that cannot verify takes the foreign bytes (C4):"), "a passthrough or lite mount has no manifest to check against and serves them. A reader that verifies now refuses: the export marks its manifests as published by a sole writer, and lean's checkout refuses an object off its citation — after the drill first found it adopting. Detectable, not self-healing.",
            ("b", "A foreign DELETE is refused loudly by lean and never restored (C5)."), "For the unverifying reader an overwrite stands silently while a delete is refused loudly: the milder-looking operation is the dangerous one.",
            ("r", "So a read-write mount over an export prefix is unsupported, and readers of an export are readers.")], None, "warn", lh=14)
    # ── pick per data class ──
    s.box(M, 600, W - 2 * M, 256, None, "panel")
    s.text(M + 22, 628, "Pick one per data class, not one front end per team", "t1")
    s.text(M + 22, 646, "The choosing page scores each column as if it stood alone. In practice a workload has several kinds of data, each with a best door — and each in its own prefix.", "t4")
    picks = [
        ("forge", "code, with history", "durable on push, reviewed by policy, merged by the server; many agents proposing to one main"),
        ("lean", "the hot working set", "a repo-sized tree at disk speed, for one writer, with a boundary it can name; the cheapest thing that works"),
        ("pass", "datasets, weights, artifacts", "read-mostly, large, or owned by another system; the bucket as-is, and nothing to keep in step"),
        ("forge", "checkpoints and large binaries", "LFS, client-to-store — or a passthrough prefix, if nothing needs them in history"),
        ("lite", "coordination, sqlite, live RWX", "the only place several pods edit one tree with locks that mean something; size the hub by its blast radius"),
        ("pass", "a legible copy of main", "the export: read it through lean or passthrough with no git in the pod — and never write to it"),
    ]
    pw = (W - 2 * M - 44 - 2 * GAP) / 3
    for i, (fe, head, body) in enumerate(picks):
        x = M + 22 + (i % 3) * (pw + GAP)
        y = 660 + (i // 3) * 96
        s.card(x, y, pw, 86, head, [body], fe, lh=14)
    s.legend(M + 4, 872, [("lite", "flint-lite"), ("lean", "flint-lean"), ("pass", "flint-passthrough"), ("forge", "flint-forge"), ("data", "data path"), ("red", "hazard / trust boundary")])
    return s


def plate_12():
    s = SVG(W, H, "An eleven-row comparison of flint-lite, flint-lean, flint-passthrough and flint-forge across the technology that carries the bytes, what the pod sees, POSIX "
                  "fidelity, concurrent writers, reader consistency, multi-cluster shape, enforced identity, user isolation, "
                  "per-cluster install, recovery point, and the data class each is for.")
    header = [("flint-lite", "lite"), ("flint-lean", "lean"), ("flint-passthrough", "pass"), ("flint-forge", "forge")]
    colw = [332, 332, 332, 336]
    rows = [
        ("Technology", "in one word: what carries the bytes",
         [[("b", "NFS."), "a hub pod serves the tree over NFSv4.2; the node's kernel client carries every byte"],
          [("b", "Sync."), "the agent writes a local directory; a sidecar copies changed bytes to S3 with the plain S3 API, after the fact. No FUSE anywhere: nothing intercepts a file operation, nothing mounts the bucket"],
          [("b", "FUSE."), "Mountpoint for S3 intercepts every file operation in the pod and translates it into object requests"],
          [("b", "Git."), "real git, served per repository, with S3 as its only durable state"]]),
        ("What the pod sees", "the shape of the thing",
         [["a shared NFS mount served by one hub pod that every client reaches over the network — one tree, many pods, live"],
          ["plain files on local disk, checked out before the first line runs and published back on a boundary — one pod's own tree"],
          ["an S3 prefix presented as files by Mountpoint for S3 — objects, with a directory-shaped view over them"],
          ["a git remote. The working tree is a local clone the pod owns; history, branches and a merge policy live at the server"]]),
        ("POSIX fidelity", "what actually works",
         [[("b", "Full, and shared."), "byte-range locks, atomic rename, O_EXCL — across pods and clusters"],
          [("b", "Full, and local."), "a real filesystem, so git, sqlite and hard links work — for one pod"],
          [("r", "None to speak of."), "no rename, no append, no in-place write — at any setting, deliberately"],
          [("b", "Full, and local — for tracked files."), "the clone is a real disk; what is durable is what is committed"]]),
        ("Concurrent writers", "per tree",
         [[("b", "Many, coordinated."), "the only front end where two pods can edit one directory and both be right"],
          [("b", "Exactly one, enforced."), "a second syncer is REFUSED, not merged — several agents integrate through git instead"],
          [("r", "Many, uncoordinated."), "two writers on one key race; the loser's write is gone and nothing records it"],
          [("b", "Many, serialised by the server."), "one syncer per repository; a stale push is refused by name; a merge is a push to refs/for/<target>"]]),
        ("What a reader sees", "the consistency contract",
         [["the live tree, strongly — close-to-open, no RPO lag, through the coherence authority"],
          ["the last PUBLISHED boundary, never your last write; gated mode makes it all-or-nothing"],
          ["whatever S3 currently holds, with S3's own consistency and nothing added"],
          ["on fetch, every acknowledged push — served by the write authority, never stale, never from S3; between fetches, a snapshot with no signal"]]),
        ("Across clusters", "the fleet shape",
         [["ONE hub, mounted from N clusters. Works unchanged — but needs unique client names, a network boundary and a keepalive"],
          ["N independent syncers, one bucket. Clusters never talk; the lease lives in the bucket, so single-writer holds ACROSS clusters"],
          ["N independent mounts, one bucket. There is no fleet problem, because nothing is shared to get wrong"],
          ["one server per repository; the door proves tokens to ITS apiserver, so remote agents need the human bearer path. The bucket is a rendezvous for readers only — undrilled"]]),
        ("Identity that is enforced", "the user's token reaches NONE of these",
         [[("r", "Nobody, on the data path."), "AUTH_SYS: the client asserts its own uid, and the POSIX check defaults to log-only"],
          [("b", "The POD's ServiceAccount."), "kubelet-minted, pod-bound, verified ONLINE by TokenReview, checked against consumers"],
          [("b", "The POD's ServiceAccount."), "an identical chain to lean — same broker, same registration binding, same audit line"],
          [("b", "The POD's ServiceAccount, at the door."), "TokenReview per session, consumers per repository, then X-Remote-User to two enforcers of one policy"]]),
        ("Isolating user A from B", "when both launch agents on several clusters",
         [["Give each project its own hub and prefix, and bound reachability outside Kubernetes.", ("r", "Everything sharing a hub shares a fate.")],
          ["Give each user their own workspace prefix. Keys are scoped to it, the lease fences it, and consumers says who may mount it"],
          ["Give each user their own CR and pod. The mount presents ONE uid, so pod-level isolation IS user-level isolation"],
          ["Give each project its own repository and each agent its own branch pattern; a shared ServiceAccount reaches every sibling's agent/* branch"]]),
        ("Installed per cluster", "in a consumer cluster",
         [[("b", "Nothing."), "the node's own kernel NFS client is the data path; at most a PV and PVC"],
          ["the s3.csi.chert.us driver, a broker and the lean CRD + thin controller — in EVERY cluster; the broker must be local"],
          ["the same driver and broker, and the CRD — in every cluster. One CSI install serves lean and passthrough together"],
          [("b", "Nothing in the data path."), "stock git plus two lines of pod spec; the door and the servers live in the home cluster"]]),
        ("Recovery point", "what a hard kill costs",
         [["the flush cadence. The PVC is a cache — losing it is a rebuild from S3, not a loss"],
          ["the last barrier: ≤ floorSecs (default 60 s), or zero and acknowledged if you declare one"],
          [("b", "None — nothing is buffered."), "a completed PUT is durable; an interrupted one did not happen"],
          ["the last acknowledged push — ok means the pack and the snapshot are in S3. Uncommitted work: none, by git's contract"]]),
        ("Choose it for", "a data class, not a team",
         [[("b", "What several pods must edit LIVE."), "shared workspaces, cross-pod locking, sqlite, a dataset too big for a pod's disk"],
          [("b", "One agent's hot working set."), "a repo-sized tree, full POSIX at local speed, durable at breakpoints"],
          [("b", "The bucket, not a filesystem."), "read-mostly datasets, weights, artifacts, or a prefix another system already owns"],
          [("b", "Code, with history."), "durable on push, a merge policy the server enforces, many agents proposing to one main; large binaries in LFS"]]),
    ]
    bottom = s.table(M, 18, colw, header, rows, lab_w = W - 2 * M - sum(colw))
    s.text(M, bottom + 28, "Read the table down a column, not across a row: each front end is coherent with itself, and a row that looks like a weakness is usually the price of a strength two rows up.", "t4b")
    s.text(M, bottom + 46, "Then read the composition page: one workload usually has several of these data classes, and each has a best door. Because the file-shaped formats are common and forge exports one, a volume can change its mind later.", "t4")
    s.h = int(bottom + 64)
    return s


# ═════════════════════════════════════════════════════════════════════════
# Portrait figures — the Google Docs rendition, 640 wide
# ═════════════════════════════════════════════════════════════════════════
def portrait_p1():
    s = SVG(640, 400, "Four front ends over one S3 bucket: flint-lite serves a shared live tree over NFS, flint-lean checks a workspace "
                      "out to local disk and publishes boundaries, flint-passthrough mounts a prefix as-is, flint-forge serves git from a "
                      "per-repository pod with every push acknowledged only once it is in S3.", compact=True)
    rows = [("lite", "flint-lite — the hub · NFS", "One pod serves a LIVE shared tree over NFSv4.2. Many pods, many clusters, real locks."),
            ("lean", "flint-lean — checkout / publish · sync", "Plain files on local disk, checked out at start and published at a boundary. ONE writer. No FUSE."),
            ("pass", "flint-passthrough — the mount · FUSE", "An S3 prefix mounted as-is by Mountpoint for S3. No POSIX, no coordination, no boundary."),
            ("forge", "flint-forge — the git server · git", "Stock git against one server pod per repository; a push is acknowledged once it is in S3.")]
    for i, (fe, head, body) in enumerate(rows):
        y = 8 + i * 78
        s.card(8, y, 410, 70, head, [body], fe, "tint")
        s.arrow(418, y + 35, 448, y + 35, fe)
    s.box(452, 8, 180, 304, None, "s3")
    s.text(542, 32, "S3", "t1", "middle")
    s.text(542, 50, "the durable store", "t4b", "middle")
    s.hair(468, 62, 616, 62)
    yy = s.para(468, 82, "One bucket. Whole-file objects plus a control namespace per front end; a bare git repository for forge.", 148)
    s.text(468, yy + 6, "<prefix>/<path>", "mono")
    s.text(468, yy + 22, "<prefix>/.flint/…", "mono")
    s.text(468, yy + 38, "<prefix>/git/…", "mono")
    s.para(468, yy + 62, "Each front end owns its prefix; the bytes are the same bytes.", 148)
    s.card(8, 324, 624, 68, "An escalation ladder, cheapest first — and the file-shaped choice is reversible.", ["Passthrough asks nothing and gives nothing back. Lean gives real POSIX at local speed to one writer. The hub is the only one where several pods share one live tree. Forge adds history and a merge policy, for code."], None, "panel", lh=13)
    return s

def portrait_p1b():
    s = SVG(640, 470, "The data flow of the four front ends as four rows: the pod, what carries the bytes, and the S3 prefix, with the "
                      "moment the bytes move written on the arrows.", compact=True)
    rows = [("lite", "NFS", ["an NFS mount"], ("every op", "live"), "the hub pod", ["one process, one tree", "publishes on a cadence; hydrates on demand"], ("publish ↑", "hydrate ↓"), "<prefix>/<path>"),
            ("lean", "sync", ["local disk"], ("same dir", "no wire"), "flint-sync sidecar", ["copies changed files", "at a boundary; checkout at start"], ("publish ↑", "checkout ↓"), "<prefix>/files/…"),
            ("pass", "FUSE", ["a FUSE mount"], ("every op", "intercepted"), "mount-s3 worker", ["each op → a request", "nothing buffered"], ("GET · PUT", "per op"), "<prefix>/<key>"),
            ("forge", "git", ["a git clone"], ("push ↑", "fetch ↓"), "server pod", ["gitcgi + syncer", "ack after the packs land"], ("packs ↑", "restore ↓"), "<prefix>/git/…")]
    for i, (fe, word, pod, (a1, a1s), comp, compb, (a2, a2s), key) in enumerate(rows):
        y = 8 + i * 106
        s.box(8, y, 46, 92, fe, "tint")
        s.text(31, y + 52, word, f"t2 c-{fe}", "middle")
        s.card(62, y, 120, 92, "the pod", pod, fe, lh=13)
        s.arrow(182, y + 46, 258, y + 46, fe, dashed=(fe == "lean"), both=True)
        s.text(220, y + 30, a1, "cap", "middle")
        s.text(220, y + 72, a1s, "cap", "middle")
        s.card(262, y, 180, 92, comp, compb, fe, "tint", lh=13)
        s.arrow(442, y + 46, 518, y + 46, fe, both=True)
        s.text(480, y + 30, a2, "cap", "middle")
        s.text(480, y + 72, a2s, "cap", "middle")
        s.box(522, y, 110, 92, None, "s3")
        s.text(577, y + 40, "S3", "t2", "middle")
        s.text(577, y + 58, key, "mono", "middle")
    s.text(8, 448, "Solid arrows carry file contents; the dashed one is a shared directory, not a wire.", "t4b")
    return s


def portrait_p2():
    s = SVG(640, 430, "flint-lite topology: consumer pods share one node kernel NFS client reaching the hub on port 2049; the hub holds "
                      "coherence state and a PVC working set and publishes to the S3 prefix that holds the durable copy.", compact=True)
    fe = "lite"
    s.group(8, 8, 300, 250, "consumer cluster", "installs nothing")
    for i in range(3):
        s.box(24 + i * 92, 38, 84, 40, None)
        s.text(66 + i * 92, 62, "agent pod", "t4", "middle")
        s.arrow(66 + i * 92, 78, 66 + i * 92, 100, None)
    s.card(24, 104, 268, 70, "kernel NFS client — one per node", ["shared by every pod on it, with its page cache; full POSIX"], fe, "tint", lh=13)
    s.para(24, 190, "The node is the client: one mount, one identity, asserting its own uid (AUTH_SYS).", 268)
    s.arrow(308, 134, 356, 134, fe, both=True)
    s.alabel(332, 122, "NFSv4.2 :2049", fe)
    s.group(360, 8, 272, 250, "hub cluster")
    s.card(376, 38, 240, 100, "the hub — ONE pod", ["leases, locks, close-to-open in one process: the coherence authority", (":8080 file API — ClusterIP only, one bearer per share")], fe, lh=13)
    s.arrow(496, 138, 496, 160, fe)
    s.card(376, 164, 240, 74, "PVC — the working set", ["a CACHE of the bucket, flushed on a cadence; losing it is a rebuild"], None, "panel", lh=13)
    s.arrow(496, 258, 496, 284, fe, both=True)
    s.alabel(560, 276, "publish · hydrate", fe)
    s.card(8, 288, 624, 130, "S3 — the durable copy", [("m", "<prefix>/<path>   published generations, untorn"), ("m", "<prefix>/.flint/epoch · manifest · owner"), "Consumers never hold bucket credentials: the bucket trusts one principal, the hub. RPO = the flush cadence. Both doors say “not yet” on a cold read — NFS4ERR_DELAY, 503 — rather than hanging."], None, "s3", lh=13)
    return s


def portrait_p3():
    s = SVG(640, 400, "Three clusters mounting one flint-lite hub, which sees only client names, plus the three requirements the fleet must "
                      "supply: a unique client name per node, a network-layer boundary, and a keepalive.", compact=True)
    fe = "lite"
    names = [("Cluster A", 'hostname "agent-7"', False), ("Cluster B", 'hostname "agent-7"  ← COLLIDES', True), ("Cluster C", 'hostname "builder-2"', False)]
    for i, (n, ident, warn) in enumerate(names):
        y = 8 + i * 70
        s.card(8, y, 250, 60, n, [("m", ident)], fe, "warn" if warn else "tint", lh=13)
        s.path(f"M258 {y+30} L300 {y+30} L300 110", fe, arrow=(i == 1))
    s.card(304, 60, 150, 100, "ONE hub", ["it sees CLIENTS, and a client is a NAME — never a cluster"], fe, lh=13)
    s.arrow(454, 110, 484, 110, fe, both=True)
    s.card(488, 60, 144, 100, "S3", [".flint/epoch fences a second hub on an EQUAL prefix"], None, "s3", lh=13)
    s.box(8, 226, 624, 166, None, "panel")
    s.text(24, 250, "What the fleet must supply", "t2")
    reqs = [("1", "A unique name per client", "two clusters sharing a node name are ONE client; RFC 8881 reads the second as the first rebooting and drops its locks silently. hostname = <cluster>-<node>."),
            ("2", "A boundary at the network layer", "port 2049 is AUTH_SYS; networkPolicy cannot draw it because kube-proxy SNATs remote clients — 1486 of 1486 arrived as the hub cluster's gateway."),
            ("3", "A keepalive per remote mount", "a partitioned cluster's busy agents look idle; drive POST /wake. suspendWithSessions defaults to suspending anyway.")]
    y = 272
    for n, head, body in reqs:
        s.numdot(34, y - 4, int(n), fe)
        s.text(52, y, head, "t3b")
        y = s.para(52, y + 15, body, 560) + 6
    return s


def portrait_p4():
    s = SVG(640, 420, "flint-lean: the CSI node plugin owns a tree bind-mounted into the agent pod as plain local files and hostPathed "
                      "into an unprivileged flint-sync worker that checks out from and publishes to S3.", compact=True)
    fe = "lean"
    s.group(8, 8, 420, 250, "one node")
    s.card(24, 38, 180, 110, "the agent pod", [("m", "/workspace"), "plain local files, full POSIX, zero interception; no S3 credential"], None, lh=13)
    s.arrow(204, 92, 232, 92, fe)
    s.card(236, 60, 80, 64, "", [("m", "the tree"), "one tree, two views"], fe, "tint", pad=8, lh=13)
    s.arrow(316, 92, 344, 92, fe)
    s.card(348, 38, 64, 110, "", [("b", "flint-sync"), "the worker; holds the credential"], fe, pad=8, lh=13)
    s.card(24, 160, 388, 84, "s3.csi.chert.us — the node DaemonSet", ["kubelet blocks the pod on NodePublishVolume, so the checkout completes BEFORE the agent's first line; the final barrier runs on NodeUnpublish."], None, "panel", lh=13)
    s.arrow(428, 110, 468, 110, fe, both=True)
    s.alabel(448, 98, "checkout · publish", fe)
    s.card(472, 8, 160, 250, "S3", [("m", "files/<path>"), ("m", ".flint/lean/epoch"), ("m", "  claim · manifest"), ("m", "  inbox · conflicts/"), "the lease lives in the BUCKET, so one writer holds across clusters"], None, "s3", pad=10, lh=13)
    s.card(8, 272, 306, 138, "The loop", ["1 · pod starts: checkout first", "2 · the agent works at disk speed", ("m", "3 · echo > .flint/publish"), ("m", "4 · .flint/publish.ack = ok"), "ok means the bytes are in S3. RPO = the last barrier, ≤ floorSecs on a hard kill."], fe, lh=13)
    s.card(326, 272, 306, 138, "Gated mode", ["every changed file is uploaded as a new version at once — durable and INVISIBLE — and one CAS cites the whole pending set. A reader sees all of a change or none of it; gated is refused without a lag bound."], fe, lh=13)
    return s


def portrait_p5():
    s = SVG(640, 420, "flint-passthrough: the privileged node plugin resolves the CR, authorises the pod's ServiceAccount, calls mount(2) "
                      "itself and hands the FUSE descriptor to an unprivileged worker running mount-s3.", compact=True)
    fe = "pass"
    s.card(8, 8, 190, 150, "the tenant pod", [("m", "csi: driver: s3.csi.chert.us"), ("m", "  chert.us/mount: datasets"), "nine lines of pod spec; no sidecar, label, credential or privilege — restricted"], None, pad=12, lh=13)
    s.arrow(198, 60, 226, 60, fe)
    s.box(230, 8, 220, 150, fe)
    s.text(244, 30, "the node plugin", f"t2 c-{fe}")
    steps = ["resolve the CR (token's namespace)", "authorise: SA in consumers, else deny", "open /dev/fuse, mount(2) itself", "hand the fd over SCM_RIGHTS", "bind the result into the pod"]
    for i, t in enumerate(steps):
        s.numdot(252, 48 + i * 21, i + 1, fe)
        s.text(268, 52 + i * 21, t, "t4")
    s.arrow(450, 60, 478, 60, fe)
    s.card(482, 8, 150, 150, "the worker", ["unchanged mount-s3 on a fd it was given: non-root, no caps, no SA token, no privilege — the mount already happened", ("b", "keys from the broker")], fe, "tint", pad=10, lh=13)
    s.arrow(557, 158, 557, 184, fe, both=True)
    s.card(8, 188, 624, 70, "S3 — and it is just S3", [("m", "<prefix>/<key>  — objects and nothing else, no control namespace"), "durability, consistency and access control are S3's, unmediated; a prefix another system owns stays as it was"], None, "s3", lh=13)
    s.card(8, 272, 306, 138, "Where the privilege went", ["node plugin: privileged, one per node, NO S3 credential, no Secrets RBAC. Workers: unprivileged, one per volume, fenced by a ValidatingAdmissionPolicy. Broker: the only standing credential, every grant TokenReview-verified and audit-logged."], None, "panel", lh=13)
    s.card(326, 272, 306, 138, "What it is NOT", ["not POSIX — no rename, append or in-place write; not coordinated — two pods do not see each other; not per-user — ONE uid per mount; not self-healing — a dead mounter strands the pod on ENOTCONN."], None, "warn", lh=13)
    return s


def portrait_p6():
    s = SVG(640, 440, "flint-forge: a stock git client presents the pod's projected ServiceAccount token to the door, which verifies it by "
                      "TokenReview and routes to one server pod per repository; the syncer uploads packs, CASes one snapshot and only "
                      "then acknowledges the push. S3 holds a bare repository, a lease, bundles, LFS objects and an optional lean export.", compact=True)
    fe = "forge"
    s.card(8, 8, 150, 120, "the agent pod", ["stock git; the working tree is local", ("m", "helper: SA token as"), ("m", "the Basic password"), "no bucket credential"], fe, pad=10, lh=13)
    s.arrow(158, 60, 186, 60, fe, both=True)
    s.card(190, 8, 150, 120, "the door", ["the lite gateway's git arm: TokenReview (cached ≤ 60 s), consumers, 401 before any wake, X-Remote-User onward"], fe, pad=10, lh=13)
    s.arrow(340, 60, 368, 60, fe, both=True)
    s.card(372, 8, 130, 120, "server, 1 per repo", ["gitcgi + http-backend; hooks → the syncer: the only writer of the bucket; emptyDir cache; idles to zero"], fe, "tint", pad=10, lh=13)
    s.arrow(502, 60, 530, 60, fe)
    s.card(534, 8, 98, 120, "S3", [("m", "git/pack/*"), ("m", "git/snapshot"), ("m", "git/epoch"), ("m", "lfs/ · files/")], None, "s3", pad=8, lh=13)
    s.box(8, 144, 624, 132, None, "panel")
    s.text(24, 168, "The push — acknowledged means durable", "t2")
    steps = ["1 · the hook hands the commands to the syncer; a stale one is refused by name", "2 · policy is enforced twice: pre-receive names the rule, the syncer enforces the same document", "3 · renew the lease — a foreign holder fences this server", "4 · upload packs (immutable) · 5 · ONE snapshot CAS · 6 · the ref transaction, THEN ok"]
    y = 190
    for t in steps:
        y = s.para(24, y, t, 592) + 2
    s.text(24, y + 6, "A crash before step 6 fails every push in the batch at the client; the restart restores from the snapshot.", "t4b")
    s.card(8, 292, 306, 136, "Idle, storm, export", ["one idle rung to replicas 0; the door holds a wake up to 180 s. Clone bundles and LFS are presigned, client-to-store. The export publishes main as a lean workspace that lean and passthrough readers mount with no git."], fe, lh=13)
    s.card(326, 292, 306, 136, "Honest edges", ["nothing in-tree terminates TLS: the token rides two cleartext hops; the X-Remote-User boundary is an opt-in NetworkPolicy; the built idle clock counts pushes only; the server runs as root. Twelve falsifiers went green, eleven on EC2 and the twelfth on Cilium; agents in another cluster are not drilled."], None, "warn", lh=13)
    return s


def portrait_p7():
    s = SVG(640, 470, "Composition: one agent pod holds a forge clone, a lean workspace, a passthrough mount and a hub share, each in its "
                      "own prefix of one bucket; forge exports main into a further prefix as a real lean workspace that readers mount. "
                      "One writer per prefix is a mechanism within a product and a convention across them; an export is never repaired.", compact=True)
    s.group(8, 8, 346, 300, "one agent pod")
    rows = [("forge", "/src — git clone", "A  repo/"), ("lean", "/workspace — lean", "C  ws/<agent>/"), ("pass", "/data — passthrough", "D  datasets/"), ("lite", "/shared — hub share", "E  shared/")]
    for i, (fe, head, prefix) in enumerate(rows):
        y = 40 + i * 64
        s.card(24, y, 180, 52, head, [], fe, "tint")
        s.arrow(204, y + 26, 240, y + 26, fe)
        s.card(244, y + 6, 100, 40, "", [("m", prefix)], fe, pad=8)
    s.box(366, 8, 266, 300, None, "s3")
    s.text(382, 32, "one bucket, one writer per prefix", "t2")
    s.hair(382, 44, 616, 44)
    y = s.para(382, 64, "A  repo/ — the bare repository, its lease, LFS objects. Writer: the syncer.", 234)
    y = s.para(382, y + 6, "B  repo-main/ — forge's export of main, a REAL lean workspace (files/, .flint/lean/). Writer: the syncer; readers: lean, passthrough. The only bridge.", 234)
    y = s.para(382, y + 6, "C  ws/<agent>/ — one lean workspace. Writer: lean.", 234)
    y = s.para(382, y + 6, "D  datasets/ — objects as-is. E  shared/ — the hub's.", 234)
    s.para(382, y + 8, "Composition is in the reading, never the writing.", 234, "t4b")
    s.card(8, 320, 306, 140, "The rule: mechanism within, convention across", ["forge arbitrates on git/epoch, lean on .flint/lean/epoch: pointed at one prefix they never meet — no 412, no fence, no log line (C1). The operator sees FlintRepos only. A read-write lean sidecar on the export prefix does contend, and used to wedge the repository (C2, now bounded)."], None, lh=13)
    s.card(326, 320, 306, 140, "An export is a mirror that is never repaired", ["a foreign overwrite is not noticed by the next export (C3); a reader with no manifest serves it, and a reader that verifies now refuses it (C4); a foreign delete is refused loudly and never restored (C5). A read-write mount over an export prefix is unsupported."], None, "warn", lh=13)
    return s


PLATES = {
    "01-four-front-ends.svg": plate_01,
    "01b-data-flow.svg": plate_01b,
    "02-lite-hub.svg": plate_02,
    "03-lite-multicluster.svg": plate_03,
    "04-lean-workspace.svg": plate_04,
    "05-passthrough-csi.svg": plate_05,
    "06-forge-git-server.svg": plate_06,
    "07-multicluster-identity.svg": plate_07,
    "08-deployment-and-blast-radius.svg": plate_08,
    "09-s3-durable-store.svg": plate_09,
    "10-security-posture.svg": plate_10,
    "11-composition.svg": plate_11,
    "12-choosing.svg": plate_12,
    "portrait/p1-four-front-ends.svg": portrait_p1,
    "portrait/p1b-data-flow.svg": portrait_p1b,
    "portrait/p2-lite-hub.svg": portrait_p2,
    "portrait/p3-lite-multicluster.svg": portrait_p3,
    "portrait/p4-lean-workspace.svg": portrait_p4,
    "portrait/p5-passthrough-csi.svg": portrait_p5,
    "portrait/p6-forge-git-server.svg": portrait_p6,
    "portrait/p7-composition.svg": portrait_p7,
}


def main():
    import sys
    for name, fn in PLATES.items():
        n = len(OVERFLOWS)
        write(name, fn())
        print(f"  wrote diagrams/{name}" + (f"  ({len(OVERFLOWS) - n} overflow)" if len(OVERFLOWS) > n else ""))
    if OVERFLOWS:
        print("\n".join("  OVERFLOW " + o for o in OVERFLOWS), file=sys.stderr)
        sys.exit(1)


if __name__ == "__main__":
    main()
