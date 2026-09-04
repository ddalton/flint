#!/bin/bash
# ---------------------------------------------------------------------------
# Regenerate docs/architecture/ — the canonical PDF, and the Google Docs /
# Word rendition, both from ONE source of prose.
#
#   docs/architecture/build.sh            # validate, rasterize, render, emit both
#   docs/architecture/build.sh --check    # validate only; no Chrome needed
#   docs/architecture/build.sh --geometry # MEASURE text overflow in Chrome
#
# WHAT IS SOURCE AND WHAT IS BUILT
#
# Source, edited by hand:
#   flint-front-ends-architecture.html   layout, ALL prose, and the docs-only
#                                        tables/figures (div.docs, never printed)
#   diagrams/*.svg                       nine A3-landscape plates (the PDF)
#   diagrams/portrait/*.svg              five portrait figures (the Docs version)
#
# Built, never edited by hand:
#   flint-front-ends-architecture.pdf      the canonical deck, 1 page per section
#   flint-front-ends-architecture.md       the A3 rendition, referencing the SVGs
#   flint-front-ends-architecture.docs.md  the Docs/Word rendition: native tables
#                                          + portrait figures, referencing PNGs
#   diagrams/png/**.png                    rasters, because Docs cannot place SVG
#
# WHY TWO RENDITIONS AND ONE SOURCE
#
# The PDF is a fixed-page deck: nine dense A3 plates. Google Docs is a flowing
# document people comment and edit in, it cannot place SVG at all, and at its
# default Letter portrait an A3 plate renders at 19% scale — measured, and
# unreadable. So four of the pages ship as NATIVE TABLES (searchable and
# commentable, which an image can never be) and the rest get portrait figures.
#
# What is NOT duplicated is the prose. It lives once, in the HTML, and both
# Markdown files are extracted from it — two hand-maintained copies of one
# argument drift, and the drift is silent.
#
# The exit status of a pipeline is the LAST command's, so nothing here pipes a
# check into anything. Each step's status is tested on its own.
# ---------------------------------------------------------------------------
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
html="$here/flint-front-ends-architecture.html"
pdf="$here/flint-front-ends-architecture.pdf"
md="$here/flint-front-ends-architecture.md"
docsmd="$here/flint-front-ends-architecture.docs.md"

mode=${1:---build}
case "$mode" in --build|--check|--geometry) ;; *)
    echo "usage: $0 [--build|--check|--geometry]" >&2; exit 2 ;;
esac

command -v python3 >/dev/null || { echo "python3 not found" >&2; exit 2; }

# ── 1. every diagram is well-formed, and every reference resolves ───────────
echo "==> validating diagrams and references"
python3 - "$here" "$html" <<'PYEOF'
import glob, os, re, sys, xml.dom.minidom

here, html_path = sys.argv[1], sys.argv[2]
bad = []

svgs = sorted(glob.glob(os.path.join(here, "diagrams", "**", "*.svg"), recursive=True))
if not svgs:
    bad.append("no diagrams found under diagrams/")
for p in svgs:
    rel = os.path.relpath(p, here)
    try:
        doc = xml.dom.minidom.parse(p)
    except Exception as e:
        bad.append(f"{rel}: not well-formed XML — {e}")
        continue
    root = doc.documentElement
    # A diagram that renders only when inlined is not a separate file. Each one
    # must declare its own namespace, carry its own styles and describe itself.
    if root.tagName != "svg" or not root.getAttribute("xmlns"):
        bad.append(f"{rel}: root must be <svg> with an xmlns (it is loaded standalone)")
    if not root.getAttribute("viewBox"):
        bad.append(f"{rel}: no viewBox — it will not scale to the page")
    if not doc.getElementsByTagName("style"):
        bad.append(f"{rel}: no <style> — an <img>-loaded SVG inherits nothing from the page")
    if not root.getAttribute("aria-label"):
        bad.append(f"{rel}: no aria-label — the PDF and any conversion lose the description")

    # A <text> that carries a class AND a fill= attribute renders in the
    # CLASS's colour: a CSS declaration outranks a presentation attribute.
    # That is how three header bars shipped dark-on-dark once. Colour comes
    # from a class here, always.
    for t in doc.getElementsByTagName("text"):
        if t.getAttribute("fill") and t.getAttribute("class"):
            txt = "".join(n.data for n in t.childNodes if n.nodeType == n.TEXT_NODE)[:40]
            bad.append(f"{rel}: <text class=.. fill=..> loses to the class — {txt!r}")

    # A box whose last line sits on (or past) its own bottom edge clips the
    # descenders. Nothing errors; the text is simply cut, and only a render
    # shows it. Compare every text baseline against every box it falls in.
    boxes = []
    for r in doc.getElementsByTagName("rect"):
        try:
            boxes.append((float(r.getAttribute("x")), float(r.getAttribute("y")),
                          float(r.getAttribute("width")), float(r.getAttribute("height")),
                          r.getAttribute("class")))
        except ValueError:
            pass
    for t in doc.getElementsByTagName("text"):
        try:
            tx, ty = float(t.getAttribute("x")), float(t.getAttribute("y"))
        except ValueError:
            continue
        txt = "".join(n.data for n in t.childNodes if n.nodeType == n.TEXT_NODE)[:40]
        for (x, y, w, h, cls) in boxes:
            if x < tx < x + w and y < ty <= y + h + 10 and ty > y + h - 4:
                bad.append(f"{rel}: text clips box[{cls}] bottom={y+h:.0f} at y={ty:.0f} — {txt!r}")

src = open(html_path, encoding="utf-8").read()
refs = re.findall(r'<img src="([^"]+)"', src)
for r in refs:
    if not os.path.exists(os.path.join(here, r)):
        bad.append(f"the HTML references a missing file: {r}")
for p in svgs:
    rel = os.path.relpath(p, here)
    if rel not in refs:
        bad.append(f"{rel}: on disk but referenced by no page")
if len(refs) != len(set(refs)):
    bad.append("the same diagram is referenced by more than one page")

# Every section must offer a Docs rendition, or the Docs build silently drops
# a page's only illustration.
sections = re.split(r'<section class="page', src)[1:]
for i, sec in enumerate(sections[1:], start=1):
    if 'class="docs"' not in sec:
        bad.append(f"page {i}: no <div class=\"docs\"> — the Docs rendition would lose it")

if bad:
    print("\n".join("  FAIL " + b for b in bad), file=sys.stderr)
    sys.exit(1)
print(f"  {len(svgs)} diagrams, {len(refs)} references, {len(sections)-1} pages, all resolved")
PYEOF

if [ "$mode" = "--check" ]; then
    echo "==> check only; nothing rendered"
    exit 0
fi

# ── locate a renderer ──────────────────────────────────────────────────────
chrome=""
for c in "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome" \
         "/Applications/Chromium.app/Contents/MacOS/Chromium" \
         "$(command -v google-chrome || true)" \
         "$(command -v chromium || true)"; do
    if [ -n "$c" ] && [ -x "$c" ]; then chrome="$c"; break; fi
done
[ -n "$chrome" ] || { echo "no Chrome/Chromium found — cannot render" >&2; exit 2; }

# ── 1b. MEASURED horizontal text overflow (--geometry) ─────────────────────
# The vertical check in step 1 is exact: a baseline and a box edge are both
# numbers in the file. Horizontal is not — it needs the rendered advance width
# of a string in a real font. Estimating it from an average em width was tried
# and was WRONG IN BOTH DIRECTIONS: it flagged six files at up to +28px when
# only one had a real overflow of +11px. So this measures, in Chrome, with
# getComputedTextLength().
#
# Run it after editing a diagram, and ALWAYS after an identifier rename —
# `s3.flint.io` -> `s3.csi.chert.us` made every driver name 4 characters
# longer, which is exactly the change that silently pushes a label out of its
# box. Opt-in because it costs one Chrome launch per diagram.
if [ "$mode" = "--geometry" ]; then
    echo "==> measuring text geometry in Chrome"
    python3 - "$here" "$chrome" <<'PYEOF'
import glob, json, os, re, subprocess, sys, tempfile
here, chrome = sys.argv[1], sys.argv[2]
PROBE = """
<script>
window.addEventListener('load', function () {
  var out = [], svg = document.querySelector('svg');
  var rects = Array.from(svg.querySelectorAll('rect')).map(function (r) {
    return { x:+r.getAttribute('x'), y:+r.getAttribute('y'),
             w:+r.getAttribute('width'), h:+r.getAttribute('height'),
             cls:r.getAttribute('class') || '' };
  }).filter(function (b) { return !isNaN(b.x) && !isNaN(b.w); });
  Array.from(svg.querySelectorAll('text')).forEach(function (t) {
    var x = +t.getAttribute('x'), y = +t.getAttribute('y');
    if (isNaN(x) || isNaN(y)) return;
    var txt = t.textContent || ''; if (!txt.trim()) return;
    var w = t.getComputedTextLength(), a = getComputedStyle(t).textAnchor;
    var left = a === 'middle' ? x - w/2 : (a === 'end' ? x - w : x);
    var cand = rects.filter(function (b) {
      return b.x < x && x < b.x + b.w && b.y < y && y <= b.y + b.h + 10; });
    if (!cand.length) return;
    var b = cand.reduce(function (m, c) { return c.w*c.h < m.w*m.h ? c : m; });
    var over = (left + w) - (b.x + b.w);
    if (over > -3) out.push({ over:Math.round(over), box:b.cls,
                              boxRight:Math.round(b.x+b.w), text:txt.slice(0,58) });
  });
  document.title = 'RESULT' + JSON.stringify(out);
});
</script>
"""
bad = 0
for p in sorted(glob.glob(os.path.join(here, "diagrams", "**", "*.svg"), recursive=True)):
    # ONE SVG per run: inlining several collides on class names, which would
    # corrupt the font sizes the measurement depends on.
    html = "<!DOCTYPE html><meta charset=utf-8><body style='margin:0'>" + open(p).read() + PROBE
    with tempfile.NamedTemporaryFile("w", suffix=".html", delete=False) as f:
        f.write(html); tmp = f.name
    try:
        dom = subprocess.run([chrome, "--headless", "--disable-gpu",
                              "--virtual-time-budget=1500", "--dump-dom", "file://" + tmp],
                             capture_output=True, text=True, timeout=90).stdout
    finally:
        os.unlink(tmp)
    m = re.search(r"<title>RESULT(\[.*?\])</title>", dom, re.S)
    rel = os.path.relpath(p, here)
    if not m:
        # A probe that did not run is not a pass. Say so.
        print(f"  FAIL {rel}: the probe did not run — cannot conclude"); bad += 1; continue
    for h in sorted(json.loads(m.group(1)), key=lambda h: -h["over"]):
        print(f"  FAIL {rel}: +{h['over']}px past box[{h['box']}] right={h['boxRight']} — {h['text']!r}")
        bad += 1
if bad:
    sys.exit(1)
print("  no text overflows its box")
PYEOF
    echo "==> geometry only; nothing rendered"
    exit 0
fi

# ── 2. rasterize every diagram (Docs and Word cannot place SVG) ────────────
# Not optional: the Docs Markdown points at these, and a Markdown referencing
# PNGs that were never generated is exactly the kind of break nothing reports.
echo "==> rasterizing diagrams"
count=0
cached=0
while IFS= read -r svg; do
    rel=${svg#"$here"/diagrams/}
    outp="$here/diagrams/png/${rel%.svg}.png"
    mkdir -p "$(dirname "$outp")"
    if [ -s "$outp" ] && [ "$outp" -nt "$svg" ]; then
        cached=$((cached + 1)); count=$((count + 1)); continue
    fi
    # The window must match the viewBox or Chrome letterboxes the screenshot.
    read -r vw vh < <(python3 -c "
import re,sys
m=re.search(r'viewBox=\"([-\d.]+) ([-\d.]+) ([\d.]+) ([\d.]+)\"', open(sys.argv[1]).read())
print(int(float(m.group(3))), int(float(m.group(4))))" "$svg")
    "$chrome" --headless --disable-gpu --force-device-scale-factor=2 \
        --default-background-color=FFFFFF --window-size="$vw,$vh" \
        --screenshot="$outp" "file://$svg" 2>/dev/null
    [ -s "$outp" ] || { echo "  FAIL: no PNG for $rel" >&2; exit 1; }
    count=$((count + 1))
done < <(find "$here/diagrams" -name '*.svg' | sort)
echo "  $count PNGs under diagrams/png/ ($cached already current)"

# ── 3. both Markdown renditions, extracted from the one HTML ───────────────
echo "==> generating Markdown"
python3 - "$html" "$md" "$docsmd" "$here" <<'PYEOF'
import html as H, os, re, sys

src = open(sys.argv[1], encoding="utf-8").read()
here = sys.argv[4]

def inline(s):
    """HTML fragment -> Markdown inline. Order matters: tags before entities,
    or an escaped < in the prose becomes a tag."""
    s = re.sub(r"<br\s*/?>", " ", s)
    s = re.sub(r"<code>(.*?)</code>", r"`\1`", s, flags=re.S)
    s = re.sub(r"<b>(.*?)</b>", r"**\1**", s, flags=re.S)
    s = re.sub(r"<i>(.*?)</i>", r"*\1*", s, flags=re.S)
    s = re.sub(r"<[^>]+>", "", s)
    s = H.unescape(s).replace("|", r"\|")
    return re.sub(r"\s+", " ", s).strip()

def grab(pat, text, flags=re.S):
    m = re.search(pat, text, flags)
    return m.group(1) if m else None

def as_table(block):
    """<table> -> a Markdown table. Cells must stay on one line, which inline()
    guarantees by collapsing whitespace."""
    rows = []
    for tr in re.findall(r"<tr>(.*?)</tr>", block, re.S):
        rows.append([inline(c) for c in re.findall(r"<t[hd]>(.*?)</t[hd]>", tr, re.S)])
    if not rows:
        return []
    out = ["| " + " | ".join(rows[0]) + " |",
           "|" + "|".join(["---"] * len(rows[0])) + "|"]
    for r in rows[1:]:
        out.append("| " + " | ".join(r) + " |")
    return out

def render(docs_mode):
    out, sections = [], re.split(r'<section class="page', src)[1:]

    cover = sections[0]
    out += ["# " + inline(grab(r"<h1>(.*?)</h1>", cover)), ""]
    out += ["> " + inline(grab(r'<p class="sub">(.*?)</p>', cover)), ""]
    for p in re.findall(r"<p[^>]*>(.*?)</p>", grab(r'<div class="meta">(.*?)</div>', cover)):
        out += [inline(p), ""]
    out += ["## Contents", ""]
    # NOT a non-greedy grab: the toc div CONTAINS divs, so `.*?</div>` stops at
    # the first child's close and yields nothing.
    entries = re.findall(r"<div>(.*?)</div>", cover.split('<div class="toc">', 1)[1], re.S)
    out += ["- " + inline(d) for d in entries] + [""]

    for sec in sections[1:]:
        out += [f"## {inline(grab(r'<h1>(.*?)</h1>', sec))}", ""]
        out += [f"*{inline(grab(r'<span class=.kicker.>(.*?)</span>', sec))}*", ""]
        out += [inline(grab(r'<p class="dek">(.*?)</p>', sec)), ""]

        docs = grab(r'<div class="docs">(.*)</div>', sec)   # greedy: may hold a table
        if docs_mode and docs and "<table" in docs:
            out += as_table(docs) + [""]
        else:
            if docs_mode and docs:
                img, alt = grab(r'<img src="([^"]+)"', docs), grab(r'alt="([^"]*)"', docs)
            else:
                img, alt = grab(r'<figure>.*?<img src="([^"]+)"', sec), grab(r'<figure>.*?alt="([^"]*)"', sec)
            if docs_mode:                       # Docs and Word cannot place SVG
                img = re.sub(r"^diagrams/(.*)\.svg$", r"diagrams/png/\1.png", img)
            out += [f"![{H.unescape(alt or '')}]({img})", ""]

        for p in re.findall(r"<p>(.*?)</p>", grab(r"<figcaption>(.*?)</figcaption>", sec)):
            out += [inline(p), ""]

    if len(entries) != len(sections) - 1:
        sys.exit(f"  FAIL: {len(entries)} contents entries for {len(sections)-1} pages")
    for i, line in enumerate(out):
        if line.startswith("#") and i + 2 < len(out) and not "".join(out[i+1:i+4]).strip():
            sys.exit(f"  FAIL: empty section {line!r}")
    return "\n".join(out).rstrip() + "\n"

for path, docs_mode in ((sys.argv[2], False), (sys.argv[3], True)):
    text = render(docs_mode)
    for ref in re.findall(r"!\[[^\]]*\]\(([^)]+)\)", text):
        if not os.path.exists(os.path.join(here, ref)):
            sys.exit(f"  FAIL: {os.path.basename(path)} references a missing {ref}")
    open(path, "w", encoding="utf-8").write(text)
    kind = "Docs/Word" if docs_mode else "A3"
    n_tables = sum(1 for ln in text.splitlines() if ln.startswith("|---"))
    n_figs = len(re.findall(r"!\[", text))
    print(f"  {kind:9s} -> {os.path.basename(path)}  ({n_tables} tables, {n_figs} figures)")
PYEOF

# ── 4. the canonical PDF ───────────────────────────────────────────────────
echo "==> rendering PDF"
"$chrome" --headless --disable-gpu --no-pdf-header-footer \
    --print-to-pdf="$pdf" "file://$html" 2>/dev/null
[ -s "$pdf" ] || { echo "chrome wrote no PDF" >&2; exit 1; }

# ── 5. the PDF says what the HTML said ─────────────────────────────────────
# A page that overflows silently becomes two pages, and a diagram that failed
# to load leaves a blank frame with no error anywhere. Count both.
expected=$(grep -c '<section class="page' "$html")
if command -v pdfinfo >/dev/null; then
    got=$(pdfinfo "$pdf" | awk '/^Pages:/{print $2}')
    if [ "$got" != "$expected" ]; then
        echo "  WARN: $got PDF pages for $expected sections — a page is overflowing" >&2
    else
        echo "  $got pages, one per section"
    fi
fi
if command -v pdftotext >/dev/null; then
    txt=$(pdftotext "$pdf" - 2>/dev/null || true)
    for probe in "flint-lite" "flint-lean" "flint-passthrough" "AUTH_SYS" "TokenReview"; do
        case "$txt" in
            *"$probe"*) ;;
            *) echo "  WARN: '$probe' is in the source but not in the rendered text" >&2 ;;
        esac
    done
    # The docs-only blocks must never reach the printed deck.
    case "$txt" in
        *"Isolating user A from user B"*) echo "  WARN: a div.docs table leaked into the PDF" >&2 ;;
    esac
fi

echo "==> done"
ls -la "$pdf" "$md" "$docsmd"
