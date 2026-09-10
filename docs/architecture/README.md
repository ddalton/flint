# flint front ends — architecture deck

The architecture of **flint-lite**, **flint-lean**, **flint-passthrough** and
**flint-forge** — in one word each, NFS, sync, FUSE and git: what each one is, the fleet shape each implies when one user launches
agents across several Kubernetes clusters, what identity is actually enforced on the
data path, the security posture of each, where S3 sits in all four, and the rule under
which they compose on one bucket.

Start with **`flint-front-ends-architecture.pdf`** (13 pages, A3 landscape).

The forge front end has its own document under **`forge/`** — `forge/flint-forge-architecture.pdf` (8 pages, A3 landscape): the components on three planes (data plane, durable path, control plane), the push transaction and its theorems, the lease, the operator and the door, the boundaries and every campaign, and the prior art with a verdict. It is drawn with this deck's kit (`forge/forge-diagrams.py` imports `diagrams.py`) and built by `forge/build.sh` with the same checks.

## The one-page data-flow posters

Beside the deck there is one **data-flow poster per front end** — a single sheet you
can put on a wall: the components that matter, the arrows between them, a label on
each arrow rather than a paragraph in each box, four notes, and a glossary of every
abbreviation on the page.

| poster | build it with |
|---|---|
| `forge/flint-forge-dataflow.pdf` | `python3 forge/forge-dataflow.py --pdf --emf` |
| `lite/flint-lite-dataflow.pdf` | `python3 lite/lite-dataflow.py --pdf --emf` |
| `lean/flint-lean-dataflow.pdf` | `python3 lean/lean-dataflow.py --pdf --emf` |
| `passthrough/flint-passthrough-dataflow.pdf` | `python3 passthrough/passthrough-dataflow.py --pdf --emf` |
| `migration/flint-migration-dataflow.pdf` | `python3 migration/migration-dataflow.py --pdf --emf` |

Each script writes a `.vsdx` (editable in Visio), a `.pdf` (via Chrome), and with
`--emf` a metafile for pasting into Office. They are drawn with `vsdxkit.py`, and the
four non-forge ones share `dataflowkit.py`: the role palette, the four arrow classes,
the numbered-step strip, the glossary grid, and the gates.

The fifth is not a front end but a **use case**: `migration/` draws a workload whose
data is on an NFS share it already mounts, moving that data into a bucket through a
passthrough mount, with ordinary code in the pod doing the copy. It is drawn with the
same kit deliberately, so a reader who has seen the four recognises every shape.

It exists because the question behind it is usually asked in the wrong shape — *what
label do I put on the pod so the driver injects the mount?* There is no label and no
webhook. `chert.us/mount` is domain-prefixed like a label but it is a KEY in
`volumeAttributes` on an inline `csi:` volume, and the mount is made when kubelet calls
`NodePublishVolume` at schedule time. Nothing mutates the pod. The poster draws that,
and draws the indirection that goes with it: the pod names a **CR**, and the CR names
the bucket — `bucket`, `keyPrefix`, `endpoint`, `region`, `image` and credentials are
refused BY NAME from a pod's `volumeAttributes` (`s3csi/attrs.rs`), because those
attributes are attacker-controlled input to a privileged process.

### The five as one book

`flint-dataflow-posters.pdf` is all five in one file — lite, passthrough, lean, forge,
then the migration use case — each page kept at its own size, since they differ and
scaling to a common sheet would shrink 7.5 pt body text to illegible:

```sh
pdfunite lite/flint-lite-dataflow.pdf passthrough/flint-passthrough-dataflow.pdf \
         lean/flint-lean-dataflow.pdf forge/flint-forge-dataflow.pdf \
         migration/flint-migration-dataflow.pdf flint-dataflow-posters.pdf
```

Verify it with `pdfinfo -f 1 -l 5`: five pages, and each page's size is a fingerprint
of which poster it is, so the sizes are how you check the ORDER as well as the count.

**The gates are the point, and they are not advisory.** Before anything is written each
script runs seven checks: `check()` (text that overflows its box, a shape off the page),
`overlap_report()` (two components on top of one another), `label_overlap_report()`
(two arrow labels on top of one another — the second silently eats the first),
`label_on_line_report()` (a label an arrow is drawn straight through), and two in
`dataflowkit`: `ink_collision_report()`, `edge_strike_report()` and
`arrow_through_text_report()`. Every one of these
is invisible in the `.vsdx` and obvious in print, so a poster with any of them exits
non-zero. Run the script and read the count: `N shapes, 0 problems`.

The last three exist because the first four have a blind spot, and it cost a round of
"looks fine at 78 dpi". `overlap_report` skips anything marked `ok_overlap`, and every
caption is; `label_overlap_report` only sees boxes marked `is_label`. So a caption laid
across another caption, or a caption a zone's own dashed border is drawn through, was
reported by nothing at all — twelve of the latter were on the four posters at once —
and `label_on_line_report` has the same gap on the other axis: it tests only boxes made
by `flabel`, so an arrow drawn through a container's foot label or a free caption was
also unreported.
Both new checks measure the **ink**, not the fitbox: a text box carries 0.04" of top
margin and 0.06" of bottom pad, so two stacked caption lines always "overlap" without a
glyph touching, and the first version of the check drowned in that. `edge_strike_report`
also tests **rectangles only** — a `Poly` (cloud, hexagon, cylinder, box3d) is drawn
well inside its bounding box, and testing its bbox reports strikes that do not exist.

A clean run proves nothing until the checker has been shown to fail: inject a collision
(move one caption onto its neighbour, or onto a zone's edge) and confirm the count goes
to 1.

**Two instances, drawn as two instances — on all four.** Forge shows one door in front
of two repository pods, each with its own emptyDir, lease and prefix. Lite shows two
hubs, each with its own `:2049` Service and its own prefix, and a consumer node that
mounts both. Lean shows two workspaces, and passthrough two mounts, each with its own
worker pod (one per *published volume*), its own credential and its own bucket. That
contrast has to be *drawn* — a caption saying "there would be a second one" is not the
same picture — and drawing it is what makes each poster say plainly what is SHARED (the
node plugin, one per node; the broker, one Deployment; forge's door; lite's and lean's
gateways) versus what is per-instance (everything else).

**A shared component has to FAN OUT in the drawing.** Saying "shared" in a caption does
not answer "then where is the second one?" — a reader counts boxes. So every shared
door has two arrows leaving it, to both instances, and says so in its own title:
"the door — ONE door, EVERY repository", "flint-hub-gateway — ONE door in front of
EVERY hub's file API", "flint-lean-gateway — ONE, for every workspace",
"flint-s3-broker — ONE Deployment". One box with one arrow reads as one instance no
matter what the prose underneath it says.

**How a UI is powered is a different answer per front end, and each poster says which.**
Passthrough needs nothing built — the bucket is the API, and a UI lists and GETs the
same keys the mount presents, bringing its own credential (the broker will not issue
one; its grants are bound to a pod-uid registration the node plugin made). Lean cannot
do that, because its durable state has a manifest: `flint-lean-gateway` talks to the
BUCKET rather than to the pod — PUT the object, append an inbox entry, never edit the
manifest — and the syncer adopts the inbox at its next barrier. Lite's file API is
proxied to the hub itself. Three different shapes, and drawing one of them for all
three would be wrong three ways.

**And lite's asymmetry is internal, which the poster has to show too.** Its file API
*can* be fronted — `flint-hub-gateway` resolves a project id to its share, wakes it if
parked and proxies six file routes, so a fleet reaches every share through one endpoint
and one credential. Its mount cannot: NFS needs a routable address per hub. Drawing only
the direct `:8080` hop would have implied a per-hub REST endpoint, which is exactly what
the operator refuses to render.

The four are meant to be read as a set, so the palette is by ROLE — a store looks like
a store in all four — and the arrow classes are the same colours everywhere: data
plane, durable path, control plane, and one class per poster for the thing that front
end alone has (forge's presigned bypass, lite's second door, lean's boundary verbs,
passthrough's one privileged act).

## What is source, and what is built

| file | |
|---|---|
| `flint-front-ends-architecture.html` | **source** — layout, all of the prose, and the Docs-only tables and figures |
| `diagrams.py` | **source** — every diagram: twelve A3 plates and seven portrait figures, drawn with one small kit |
| `build.sh` | **source** — the build |
| `diagrams/*.svg` | built, and committed — the A3-landscape plates (the PDF) |
| `diagrams/portrait/*.svg` | built, and committed — the portrait figures (the Docs version) |
| `flint-front-ends-architecture.pdf` | built — the canonical deck |
| `flint-front-ends-architecture.md` | built — the A3 rendition |
| `flint-front-ends-architecture.docs.md` | built — the Google Docs / Word rendition |
| `flint-front-ends-architecture.docs.pdf` | built — that rendition printed at Letter portrait: real tables, portrait figures; the read-only twin of the Google Doc |
| `diagrams/png/**.png` | built — rasters, because Docs cannot place SVG |

Edit the HTML or `diagrams.py`, then run `./build.sh`. Do not edit an SVG, the `.md`
files, the `.pdf` or the PNGs: they are regenerated, and hand edits are lost on the
next run.

```
./build.sh            # generate diagrams, validate, rasterize, render the PDF, emit both Markdowns
./build.sh --check    # generate and validate only — no rendering, no Chrome needed
./build.sh --geometry # measure every label against its box in Chrome — run after editing a diagram
```

## The diagrams

`diagrams.py` draws every plate with one kit — a box that wears its front end as a thin
strip along its top edge, a dashed group for a cluster, node or namespace, solid
coloured arrows for the data path and dashed neutral ones for control, numbered steps,
and a table whose row heights come from the wrapped text. The grammar is deliberate:

- **Colour means one thing: which front end.** The four hues are the ones the approach
  radar uses (`docs/radar/`), validated as a set with the dataviz palette script for
  colour-vision separation; text uses a darker step of each so it clears 4.5:1 on white.
  S3, Kubernetes and the apiserver are neutral. The only status colour is red, for a
  hazard or a trust boundary — a good property is written in bold, never coloured green,
  because green is a front end.
- **The argument lives in the caption, the structure in the plate.** A card holds a
  heading and a few lines; the paragraph that explains it is in the HTML.
- **Every card asserts that its text fits.** The generator wraps with an estimated
  advance width and refuses to write a plate whose text overflows a box; `--geometry`
  then measures the real widths in Chrome. Both have caught real clipping.

## Two renditions, one source of prose

The **PDF is canonical**: twelve dense A3-landscape plates, for reading and printing.

The **Docs rendition is for review** — where people comment and edit. It exists
because a fixed-page deck does not survive the trip:

- Google Docs cannot place SVG at all, so every diagram must be a raster.
- At Docs' default Letter portrait the A3 plates render at **19% scale** — measured,
  and genuinely unreadable. At A3 landscape they render at 93% and read fine, but
  that requires everyone to set the page size and never reset it.
- Text inside an image is not searchable and cannot be commented on — which is most
  of the point of putting something in Google Docs.

So five pages that are *actually tables* — the identity chain, fleet shape and blast
radius, the bucket layout, the security posture and the comparison matrix — ship as
**native Markdown tables** that become real, commentable Google Docs tables. The other
seven get portrait figures authored at 640 units wide, so they land near 1:1 in a
default Letter-portrait doc.

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

`flint-front-ends-architecture.docs.pdf` is that same rendition already printed at
Letter portrait, by the build, without pandoc: the read-only copy for a phone, a
mailbox, or anyone who will not be commenting. The A3 deck is still the canonical
PDF; this one exists because the deck's plates are unreadable on a portrait page.

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
- **Every card's text fits its box** (`diagrams.py`, at generation time) — an estimate,
  and the reason `--geometry` exists.
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
`docs/plans/flint-lean-plan.md`, `docs/plans/flint-forge-design.md` (§13 for the
falsifiers as run on EC2, §17 for the composition drills), `docs/flint-lite-architecture.html`,
`docs/flint-lean-architecture.html`, the `flint-lite-chart`, `flint-lean-chart`,
`flint-passthrough-chart`, `flint-s3-csi-chart` and `flint-forge-chart` values, the
drills under `forge/e2e/`, and the code under
`spdk-csi-driver/src/{s3csi,passthrough,tier,lite_operator,lean_operator,forge_operator,lite_gateway}`,
`lean/sidecar/src` and `forge/syncer/src`. The security page also draws on the approach
radar's verified Security cells (`docs/radar/`).

This deck describes the four front ends. It does not cover the pNFS/block data
path, `flint-fuse` (`docs/flint-fuse-architecture.pdf`), or the hub gateway
(`docs/flint-hub-gateway.md`) beyond its git arm.
