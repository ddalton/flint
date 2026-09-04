# flint front ends — architecture deck

The architecture of **flint-lite**, **flint-lean** and **flint-passthrough**: what each
one is, the fleet shape each implies when one user launches agents across several
Kubernetes clusters, what identity is actually enforced on the data path, and where
S3 sits in all three.

Start with **`flint-front-ends-architecture.pdf`** (10 pages, A3 landscape).

## What is source, and what is built

| file | |
|---|---|
| `flint-front-ends-architecture.html` | **source** — layout, all of the prose, and the Docs-only tables and figures |
| `diagrams/*.svg` | **source** — nine A3-landscape plates (the PDF) |
| `diagrams/portrait/*.svg` | **source** — five portrait figures (the Docs version) |
| `build.sh` | **source** — the generator |
| `flint-front-ends-architecture.pdf` | built — the canonical deck |
| `flint-front-ends-architecture.md` | built — the A3 rendition |
| `flint-front-ends-architecture.docs.md` | built — the Google Docs / Word rendition |
| `diagrams/png/**.png` | built — rasters, because Docs cannot place SVG |

Edit the HTML or a diagram, then run `./build.sh`. Do not edit the `.md` files, the
`.pdf` or the PNGs: they are regenerated, and hand edits are lost on the next run.

```
./build.sh            # validate, rasterize, render the PDF, emit both Markdowns
./build.sh --check    # validate only — no rendering, no Chrome needed
```

## Two renditions, one source of prose

The **PDF is canonical**: nine dense A3-landscape plates, for reading and printing.

The **Docs rendition is for review** — where people comment and edit. It exists
because a fixed-page deck does not survive the trip:

- Google Docs cannot place SVG at all, so every diagram must be a raster.
- At Docs' default Letter portrait the A3 plates render at **19% scale** — measured,
  and genuinely unreadable. At A3 landscape they render at 93% and read fine, but
  that requires everyone to set the page size and never reset it.
- Text inside an image is not searchable and cannot be commented on — which is most
  of the point of putting something in Google Docs.

So four pages that are *actually tables* — the identity chain, fleet shape and blast
radius, the bucket layout, and the comparison matrix — ship as **native Markdown
tables** that become real, commentable Google Docs tables. The other five get
portrait figures authored at 640 units wide, so they land near 1:1 in a default
Letter-portrait doc.

What is **not** duplicated is the prose. It lives once, in the HTML, inside
`<div class="docs">` blocks that are `display:none` in print. Both Markdown files
are extracted from it, so the two renditions cannot drift.

### Getting it into Google Docs

```sh
pandoc flint-front-ends-architecture.docs.md -o deck.docx --resource-path=.
```

Upload `deck.docx` to Drive and open it with Google Docs — the `.docx` embeds the
images, so they come across and the tables arrive as native tables. Keep the page
at Letter portrait; the Docs rendition is built for it.

Importing the `.md` directly also works (Drive converts Markdown), but relative
image paths do not resolve, so you would insert the five PNGs by hand.

`build.sh` needs `python3` and a Chrome or Chromium (which is what produced every
other PDF under `docs/`). `pdfinfo` and `pdftotext`, if present, are used for the
post-render checks.

## Converting to another format

The diagrams are deliberately **referenced, not inlined** — `<img src="diagrams/…">`
— so they stay reusable. For Google Docs, use the Docs rendition above. Every SVG is self-contained: its own `xmlns`, its own
`<style>` block and its own `aria-label`, so it renders correctly on its own, in a
browser, in a slide, or through pandoc. That is also why each one repeats the style
block; do not factor it out.

```sh
pandoc flint-front-ends-architecture.md -o deck.docx          # Word
pandoc flint-front-ends-architecture.md -o deck.odt           # ODF
pandoc flint-front-ends-architecture.md -o deck.tex           # LaTeX
pandoc -t revealjs -s flint-front-ends-architecture.md -o deck.html
```

Converters that cannot place SVG (older Word paths, some LaTeX flows) want PNG or
PDF versions of the diagrams; `rsvg-convert` or Inkscape will produce them, and the
Markdown's image paths can be pointed at the result:

```sh
for f in diagrams/*.svg; do rsvg-convert -w 3200 "$f" -o "${f%.svg}.png"; done
sed 's/\.svg)/.png)/' flint-front-ends-architecture.md > /tmp/png.md
```

## What `build.sh` checks, and why

Each check is here because the thing it looks for shipped at least once and was
invisible until something was rendered and looked at:

- **Every SVG is well-formed, has a `viewBox`, a `<style>` and an `aria-label`.**
  A diagram that only renders when inlined is not a separate file.
- **Every referenced diagram exists, and every diagram on disk is referenced.**
  Both directions — an orphan is as much a mistake as a missing file.
- **No `<text>` carries both a `class` and a `fill=` attribute.** A CSS declaration
  outranks a presentation attribute, so `fill="#ffffff"` on a classed text is
  ignored. Three header bars shipped dark-on-dark this way, and it was only visible
  at full resolution.
- **No text baseline sits within 4px of the bottom of a box it falls inside.**
  Nothing errors; the descenders are simply clipped.
- **The PDF has exactly one page per `<section>`.** A caption that outgrows its page
  silently becomes two pages.
- **The Markdown has as many contents entries as pages, and no empty sections.**
  The contents list was silently empty once, because the extraction's non-greedy
  match stopped at a nested `</div>`.
- **Every page offers a Docs rendition**, and every image either Markdown
  references exists on disk. A page with no `div.docs` would silently lose its
  only illustration in the Docs version.
- **No Docs-only table leaked into the printed PDF.**

## Sources of record

`docs/plans/csi-node-mount-design.md` (the CSI delivery — read §0 first),
`docs/plans/flint-lean-plan.md`, `docs/flint-lite-architecture.html`,
`docs/flint-lean-architecture.html`, the `flint-lite-chart`, `flint-lean-chart`,
`flint-passthrough-chart` and `flint-s3-csi-chart` values, and the code under
`spdk-csi-driver/src/{s3csi,passthrough,tier,lite_operator,lean_operator}` and
`lean/sidecar/src`.

This deck describes the three front ends. It does not cover the pNFS/block data
path, `flint-fuse` (`docs/flint-fuse-architecture.pdf`), or the hub gateway
(`docs/flint-hub-gateway.md`).
