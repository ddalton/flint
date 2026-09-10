#!/usr/bin/env python3
"""Shared plumbing for the one-page data-flow posters.

`forge/forge-dataflow.py` drew the first of these by hand; this module is
what the other three front ends share so that the four read as one set —
same role palette, same four arrow classes, same glossary strip, and the
same gates run before anything is written.

The palette is by ROLE, not by product: a store is a store in all four
drawings. What varies per poster is the accent, which is the front end's
own hue from `diagrams.py` — the deck and the poster must agree on which
colour means "lite".

Run gates, always. `emit()` prints every problem `vsdxkit` can find and
returns a non-zero exit code if there is one, so a poster that overflows
a box or lays one label over another fails the build rather than being
discovered in print.
"""

import os
import re
import shutil
import subprocess
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import vsdxkit as k          # noqa: E402
import vsdxemf as emf        # noqa: E402

# ---------------------------------------------------------------------
# ink
# ---------------------------------------------------------------------
INK, SUB, MUTE, PAPER = "#14181D", "#3D4650", "#6B7683", "#FFFFFF"

# the four front ends' hues, as `diagrams.py` defines them
ACCENT = {"lite": "#184f95", "lean": "#0f7a55", "pass": "#7d530c",
          "forge": "#8c2449"}

# roles — the same fills the forge poster uses
CLIENT_F, CLIENT_L = "#E4EEF9", "#2E6FB7"
DOOR_F, DOOR_L = "#D6F0EC", "#17847A"
SERV_F, SERV_L = "#FCE3CD", "#C0651A"
WORK_F, WORK_L = "#E6DBF6", "#6F45B5"
OPER_F, OPER_L = "#DFEFD4", "#4B8B2B"
S3_F, S3_L = "#FFF1CF", "#B0862A"
S3_HOT_F = "#FFE9B8"
CACHE_F, CACHE_L = "#EDEFF2", "#78828E"
CLOUD_F, CLOUD_L = "#FFFCF2", "#C9A44C"
PLAIN_F, PLAIN_L = "#EEF1F5", "#6B7683"
ZONE_L = "#A9B2BD"

# the four arrow classes — identical across the set
FLOW, DUR, CTL, ALT = "#2E6FB7", "#6F45B5", "#4B8B2B", "#B0862A"

W = 0.014          # standard flow weight
WT = 0.022         # the thick one, for the path every byte takes


# ---------------------------------------------------------------------
# pieces
# ---------------------------------------------------------------------
def node(p, kind, x, y, w, h, title="", body="", **kw):
    """A component: centred text, and the symbol its role deserves."""
    kw.setdefault("halign", 1)
    kw.setdefault("valign", 1)
    kw.setdefault("title_size", 10.5)
    kw.setdefault("body_size", 7.8)
    kw.setdefault("body_color", SUB)
    return p.symbol(kind, x, y, w, h, title, body, **kw)


def zone(p, x, y, w, h, label, color=ZONE_L, sub=""):
    """A dashed boundary with its name at the TOP — a cluster, a node, a
    namespace: something the drawing is inside of, not a component."""
    p.box(x, y, w, h, "", "", fill="#FCFDFE", line=color, dashed=True,
          rounding=0.14, line_weight=0.009)
    p.text(x + 0.18, y + 0.13, w - 0.36, label, size=9.5, color="#5A646F",
           bold=True)
    if sub:
        p.text(x + 0.18, y + 0.34, w - 0.36, sub, size=7.6, color=MUTE)


def container(p, x, y, w, h, label, foot=0.05):
    """A container inside a pod. The label sits at the FOOT, because one
    at the top pushes the component that matters off its spine."""
    p.box(x, y, w, h, "", "", fill=PAPER, line="#B9C0C8", dashed=True,
          rounding=0.1, line_weight=0.009)
    p.text(x, y + h + foot, w, label, size=9, color="#5A646F", bold=True,
           halign=1)


def flabel(p, cx, cy, text, color=FLOW, size=7.8, w=1.9):
    """An arrow's label — TRANSPARENT, and therefore placed BESIDE the
    arrow rather than on it. A paper fill would break the line it names
    and hide whatever else runs under it; `label_on_line_report()` is
    what keeps the placement honest."""
    return p.text(cx - w / 2.0, cy - 0.115, w, text, size=size, color=color,
                  bold=True, halign=1, label=True)


def caption(p, x, y, w, text, size=7.2, color=MUTE, halign=1):
    """A note that belongs to a component rather than to an arrow."""
    return p.text(x, y, w, text, size=size, color=color, halign=halign)


def notes(p, x, y, w, lines, size=8.6, gap=0.10):
    """The paragraphs the picture cannot carry, under the drawing.

    Stacked by MEASURED height, never by a fixed step: a note that wraps
    to two lines is the ordinary case here, and a fixed step lays the
    next paragraph on top of it — which `check()` cannot see, because a
    caption is allowed to sit on anything.
    """
    for line in lines:
        s = p.text(x, y, w, line, size=size, color=SUB)
        y += s.h + gap
    return y


def steps(p, x, y, w, items, color=CTL, gap=0.15, head_size=8.4,
          body_size=7.3, dot=0.30, start=1):
    """A numbered sequence in a row — the shape of a CALL, not a flow.

    Used where the drawing has to say what one RPC does in order, which
    an arrow between two boxes cannot: the numbers are the ordering, and
    the ordering is usually the design. `start` continues the count on a
    second row — a strip split into two rows that both begin at 1 says
    there are two sequences, which is a different claim.
    """
    cw = (w - gap * (len(items) - 1)) / len(items)
    bottom = y
    for i, (head, body) in enumerate(items):
        cx = x + i * (cw + gap)
        p.symbol("circle", cx, y, dot, dot, str(i + start), "", fill=color,
                 line=color, title_color=PAPER, title_size=7.6, halign=1,
                 valign=1, ok_overlap=True)
        p.text(cx + dot + 0.10, y + 0.015, cw - dot - 0.10, head,
               size=head_size, color=INK, bold=True)
        s = p.text(cx + dot + 0.10, y + 0.21, cw - dot - 0.10, body,
                   size=body_size, color=SUB)
        bottom = max(bottom, s.y + s.h)
    return bottom


def legend(p, x, y, items, span=5.4):
    """One row: an arrow of each class, and what that class means."""
    for text, color, dash in items:
        p.arrow([(x, y), (x + 0.62, y)], color=color, weight=W, dashed=dash)
        p.text(x + 0.72, y - 0.115, span - 0.9, text, size=8, color=SUB)
        x += span
    return y


def glossary(p, x, y, w, terms, cols=4, heading="What the abbreviations mean"):
    """A reference strip: term above gloss, rows levelled so it reads as
    a grid. Every poster carries one, because the drawing is the place a
    reader meets these words for the first time."""
    p.text(x, y, w, heading, size=10, color=INK, bold=True)
    y += 0.32
    gap = 0.36
    cw = (w - gap * (cols - 1)) / cols
    for row in range(0, len(terms), cols):
        boxes = []
        for i, (term, gloss) in enumerate(terms[row:row + cols]):
            boxes.append(p.fitbox(
                x + i * (cw + gap), y, cw, term, gloss, pad=0.02,
                title_size=8.6, body_size=7.5, title_color=INK,
                body_color=SUB, no_fill=True, no_line=True))
        tall = max(b.h for b in boxes)
        for b in boxes:
            b.h = tall
        y += tall + 0.18
    return y


def header(p, kind, title, standfirst, w=21.4, name=None):
    """The three lines every poster opens with.

    `kind` picks the accent from the deck's own palette; `name` is what
    the eyebrow says, for the one front end whose palette key ("pass")
    is not its name.
    """
    p.text(0.5, 0.34, 9.0, "flint-%s" % (name or kind), size=9.5,
           color=ACCENT.get(kind, MUTE), bold=True)
    p.text(0.5, 0.6, w, title, size=20, color=INK, bold=True)
    p.text(0.5, 1.06, w, standfirst, size=10, color=SUB)


# ---------------------------------------------------------------------
# rendering
# ---------------------------------------------------------------------
def find_chrome():
    for c in ("/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
              "/Applications/Chromium.app/Contents/MacOS/Chromium",
              shutil.which("google-chrome"), shutil.which("chromium")):
        if c and os.path.isfile(c) and os.access(c, os.X_OK):
            return c
    return None


def render_pdf(doc, svgs, out, chrome):
    """One PDF, one page per drawing, each at the drawing's own size.

    Chrome's `@page size` is per document, so each page is printed on its
    own and the results merged — scaling to a common sheet would shrink
    7.5 pt body text to illegible, and the point of the PDF is to be read.
    """
    parts = []
    for p, svg in zip(doc.pages, svgs):
        with open(svg) as fh:
            markup = fh.read()
        html = os.path.splitext(svg)[0] + ".html"
        with open(html, "w") as fh:
            fh.write(
                "<!doctype html><meta charset='utf-8'><style>"
                "@page { size: %.4fin %.4fin; margin: 0 }"
                "html,body { margin:0; padding:0; background:#fff }"
                "svg { display:block; width:%.4fin; height:%.4fin }"
                "* { -webkit-print-color-adjust: exact; "
                "print-color-adjust: exact }"
                "</style>%s" % (p.width, p.height, p.width, p.height, markup))
        part = os.path.splitext(svg)[0] + ".pdf"
        subprocess.run([chrome, "--headless", "--disable-gpu", "--no-sandbox",
                        "--no-pdf-header-footer", "--print-to-pdf=" + part,
                        "file://" + os.path.abspath(html)],
                       check=True, capture_output=True)
        if not os.path.getsize(part):
            raise SystemExit("chrome wrote no PDF for " + svg)
        parts.append(part)
        os.remove(html)

    if shutil.which("pdfunite") and len(parts) > 1:
        subprocess.run(["pdfunite"] + parts + [out], check=True)
    elif len(parts) == 1:
        shutil.move(parts[0], out)
        parts = []
    else:
        from pypdf import PdfWriter
        writer = PdfWriter()
        for part in parts:
            writer.append(part)
        writer.write(out)
    for part in parts:
        os.remove(part)
    return out


def check_pdf(doc, out):
    """The PDF says what the drawing said: a page each, at each size."""
    if not shutil.which("pdfinfo"):
        return ["pdfinfo absent — page count and sizes UNVERIFIED"]
    txt = subprocess.run(["pdfinfo", "-f", "1", "-l", str(len(doc.pages)), out],
                         capture_output=True, text=True).stdout
    # `Page    1 size:  1614 x 926.88 pts` — anchored, because a loose
    # number scan reads the PAGE NUMBER as the width and then agrees
    # with nothing for the right reason
    got = re.findall(r"^Page\s+\d+ size:\s+([\d.]+) x ([\d.]+)", txt, re.M)
    bad = []
    if len(got) != len(doc.pages):
        bad.append("PDF has %d sized pages, expected %d"
                   % (len(got), len(doc.pages)))
    for p, (gw, gh) in zip(doc.pages, got):
        w, h = float(gw) / 72.0, float(gh) / 72.0
        if abs(w - p.width) > 0.06 or abs(h - p.height) > 0.06:
            bad.append("%s: PDF page %.2f x %.2f in, drawing %.2f x %.2f in"
                       % (p.name, w, h, p.width, p.height))
    return bad


def edge_strike_report(page, pad=0.01):
    """A caption a box's own OUTLINE is drawn through.

    The shipped gates compare boxes to boxes and labels to lines, and
    miss this entirely: a label sitting on the dashed bottom edge of a
    zone is not overlapping any other box and is not on any arrow, so
    nothing reports it — and in print the border strikes the words
    through. Found by eye at 200 dpi on four labels across three
    posters, which is three too many to keep finding by eye.

    Measured on the INK, not on the padded fitbox: a text box carries
    0.04" of top margin and 0.06" of bottom pad, so the box overlaps
    edges the glyphs clear comfortably.
    """
    out = []
    texts = [s for s in page.shapes
             if s.kind == "box" and s.st.get("no_line")
             and (s.title_lines or s.body_lines)]
    # RECTANGLES ONLY. A `Poly` — cloud, hexagon, cylinder, box3d — is
    # drawn well inside its bounding box, so testing its bbox edges
    # reports strikes that do not exist. The first version did, on three
    # labels that were nowhere near the cloud's actual outline.
    edged = [s for s in page.shapes
             if type(s) is k.Box and not s.st.get("no_line")]
    for t in texts:
        box = _ink(t)
        if not box:
            continue
        x0, y0, x1, y1 = box
        x0, y0, x1, y1 = x0 + pad, y0 + pad, x1 - pad, y1 - pad
        if x1 <= x0 or y1 <= y0:
            continue
        for b in edged:
            corners = [(b.x, b.y), (b.x + b.w, b.y),
                       (b.x + b.w, b.y + b.h), (b.x, b.y + b.h)]
            for (ax, ay), (bx, by) in zip(corners, corners[1:] + corners[:1]):
                if k._seg_hits_rect(ax, ay, bx, by, x0, y0, x1, y1):
                    out.append("[%s] EDGE THROUGH LABEL  %r"
                               % (page.name,
                                  " ".join(t.title_lines or t.body_lines)[:38]))
                    break
            else:
                continue
            break
    return out


def _ink(s):
    """(x0, y0, x1, y1) of the glyphs a text box actually draws."""
    st = s.st
    rows = []
    for lines, size, mono in ((s.title_lines, st["title_size"], st["title_mono"]),
                              (s.body_lines, st["body_size"], st["mono"])):
        for ln in lines:
            rows.append((ln, size, mono))
    if not rows:
        return None
    h = sum(size / 72.0 * k._LINE_SPACING for _, size, _ in rows)
    widest = 0.0
    for ln, size, mono in rows:
        adv = k._ADVANCE["Consolas" if mono else "Segoe UI"] * (size / 72.0)
        widest = max(widest, len(ln) * adv)
    inner = s.w - 2 * k._H_MARGIN
    widest = min(widest, inner)
    if st["halign"] == 1:
        x0 = s.x + k._H_MARGIN + (inner - widest) / 2.0
    elif st["halign"] == 2:
        x0 = s.x + s.w - k._H_MARGIN - widest
    else:
        x0 = s.x + k._H_MARGIN
    y0 = s.y + k._V_MARGIN
    return (x0, y0, x0 + widest, y0 + h)


def arrow_through_text_report(page):
    """An arrow drawn through a caption — ANY caption, not just a label.

    `label_on_line_report` covers boxes marked `is_label`, which is the
    ones passed through `flabel`. A container's foot label, a zone's
    sub-caption and every free note are not, and an arrow through one of
    those looks exactly as struck-through in print. Measured on the ink,
    for the same reason the other two are.
    """
    out = []
    segs = []
    for s in page.shapes:
        if s.kind == "arrow":
            segs += list(zip(s.points, s.points[1:]))
    for t in page.shapes:
        if not (t.kind == "box" and t.st.get("no_line")
                and (t.title_lines or t.body_lines)):
            continue
        box = _ink(t)
        if not box:
            continue
        x0, y0, x1, y1 = box
        if x1 <= x0 or y1 <= y0:
            continue
        for (ax, ay), (bx, by) in segs:
            if k._seg_hits_rect(ax, ay, bx, by, x0, y0, x1, y1):
                out.append("[%s] ARROW THROUGH TEXT  %r"
                           % (page.name,
                              " ".join(t.title_lines or t.body_lines)[:38]))
                break
    return out


def ink_collision_report(page, tol=0.01):
    """Caption ON caption, measured on the ink.

    `overlap_report` skips anything with `ok_overlap`, and every caption
    sets it; `label_overlap_report` only sees boxes marked `is_label`.
    So two plain captions laid over one another are reported by nothing.
    """
    out = []
    texts = [s for s in page.shapes
             if s.kind == "box" and s.st.get("no_line")
             and (s.title_lines or s.body_lines)]
    for i in range(len(texts)):
        for j in range(i + 1, len(texts)):
            a, b = _ink(texts[i]), _ink(texts[j])
            if not a or not b:
                continue
            if (min(a[2], b[2]) - max(a[0], b[0]) > tol
                    and min(a[3], b[3]) - max(a[1], b[1]) > tol):
                out.append("[%s] INK ON INK  %r  x  %r"
                           % (page.name,
                              " ".join(texts[i].title_lines
                                       or texts[i].body_lines)[:28],
                              " ".join(texts[j].title_lines
                                       or texts[j].body_lines)[:28]))
    return out


def emit(doc, page, outdir, stem, argv):
    """Trim, gate, write. Returns the process exit code.

    The gates are the point: a box whose text overflows it, a shape off
    the page, two components on top of one another, two arrow labels on
    top of one another, or a label an arrow is drawn straight through
    are all invisible in the .vsdx and obvious in print.
    """
    page.trim()
    problems = (doc.check() + doc.overlap_report()
                + doc.label_overlap_report() + doc.label_on_line_report()
                + ink_collision_report(page) + edge_strike_report(page)
                + arrow_through_text_report(page))
    for msg in problems:
        print(msg)
    print("%d shapes, %d problems" % (len(page.shapes), len(problems)))

    out = os.path.join(outdir, stem + ".vsdx")
    doc.save(out)
    print("wrote", out)

    if "--emf" in argv:
        for path in emf.save_emf(doc, os.path.join(outdir, stem)):
            problems += ["%s: %s" % (path, m) for m in emf.validate_emf(path)]
            print("wrote", path)

    if "--preview" in argv or "--pdf" in argv:
        svgs = doc.save_svg(os.path.join(outdir, stem + "-preview"))
        if "--pdf" in argv:
            chrome = find_chrome()
            if not chrome:
                raise SystemExit("no Chrome/Chromium found")
            pdf = render_pdf(doc, svgs, os.path.join(outdir, stem + ".pdf"),
                             chrome)
            for msg in check_pdf(doc, pdf):
                print("  PDF CHECK:", msg)
                problems.append(msg)
            print("wrote", pdf)
            if "--preview" not in argv:
                for s in svgs:
                    if os.path.exists(s):
                        os.remove(s)
    return 1 if problems else 0
