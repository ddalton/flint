"""A small writer for Visio 2013+ `.vsdx` packages, with a layout gate.

No template and no Visio: a `.vsdx` is an OPC (zip) package of XML parts,
so the whole file is built from strings here. Only what these diagrams
need is implemented — rectangles carrying a bold title and a body, and
polyline connectors with arrowheads.

Two things this kit insists on, because both failure modes are invisible
until the file is opened in Visio:

* Text is wrapped HERE rather than left to Visio's own wrapping, so the
  line count is known; a box's height can then be DERIVED from its text
  (`Stack`, `Page.fitbox`), and `Document.check()` measures every
  hand-sized box against what it holds.
* The same shape list renders to an SVG preview, so the layout can be
  looked at rather than trusted.

Coordinates are inches and TOP-DOWN — `x, y` is a box's top-left corner
— which is not Visio's convention; every method flips on the way in.
"""

import math
import zipfile
from xml.sax.saxutils import escape

NS = "http://schemas.microsoft.com/office/visio/2012/main"
RNS = "http://schemas.openxmlformats.org/officeDocument/2006/relationships"
VREL = "http://schemas.microsoft.com/visio/2010/relationships"

ARROW_FILLED = 4  # from Visio's line-end list

# Mean advance width as a fraction of the em for the two faces used here.
# Deliberately a shade wide: over-estimating costs a wrap, under-
# estimating costs an overflow nobody sees.
_ADVANCE = {"Segoe UI": 0.505, "Consolas": 0.550}
_LINE_SPACING = 1.24
_H_MARGIN = 0.045
_V_MARGIN = 0.04


def pt(points):
    """Font sizes are inches in the ShapeSheet; authors think in points."""
    return round(points / 72.0, 6)


def _cell(name, value, formula=None):
    if formula is None:
        return "<Cell N='%s' V='%s'/>" % (name, escape(str(value), {"'": "&apos;"}))
    return "<Cell N='%s' V='%s' F='%s'/>" % (
        name, escape(str(value), {"'": "&apos;"}), escape(formula, {"'": "&apos;"}))


def wrap(text, width_in, size_pt, mono=False):
    """Wrap `text` to `width_in` inches at `size_pt`, honouring hard breaks."""
    em = size_pt / 72.0
    adv = _ADVANCE["Consolas" if mono else "Segoe UI"] * em
    budget = max(int((width_in - 2 * _H_MARGIN) / adv), 4)
    out = []
    for hard in text.split("\n"):
        if not hard:
            out.append("")
            continue
        # `None` rather than "" for "nothing placed yet": an empty string
        # is falsy, and testing truthiness ate the leading indentation
        # that every key map on these pages is aligned by.
        line = None
        for word in hard.split(" "):
            probe = word if line is None else line + " " + word
            if len(probe) <= budget or line is None:
                line = probe
            else:
                out.append(line)
                line = word
        out.append("" if line is None else line)
    return out


def _seg_hits_rect(ax, ay, bx, by, x0, y0, x1, y1):
    """Liang-Barsky: does the segment a→b touch the axis-aligned rect?"""
    dx, dy = bx - ax, by - ay
    t0, t1 = 0.0, 1.0
    for num, den in ((x0 - ax, dx), (ax - x1, -dx),
                     (y0 - ay, dy), (ay - y1, -dy)):
        if den == 0:
            if num > 0:          # parallel and outside this edge
                return False
            continue
        t = num / den
        if den > 0:
            if t > t1:
                return False
            t0 = max(t0, t)
        else:
            if t < t0:
                return False
            t1 = min(t1, t)
    return t0 <= t1


def measure(title, body, w, title_size, body_size, mono=False, title_mono=False):
    """Wrapped lines and the total height, in inches, that they need."""
    tl = wrap(title, w, title_size, title_mono) if title else []
    bl = wrap(body, w, body_size, mono) if body else []
    h = (len(tl) * (title_size / 72.0) + len(bl) * (body_size / 72.0)) * _LINE_SPACING
    return tl, bl, h + 2 * _V_MARGIN


# ---------------------------------------------------------------------
# shapes
# ---------------------------------------------------------------------
class Box:
    """A rectangle with text.

    Geometry stays mutable until save time, so an enclosure can be drawn
    first — z-order is document order — and resized once its contents
    are known.
    """

    kind = "box"
    is_label = False

    def __init__(self, page, x, y, w, h, title, body, **st):
        self.page, self.id = page, None
        self.x, self.y, self.w, self.h = float(x), float(y), float(w), float(h)
        self.ok_overlap = st.pop("ok_overlap", False)
        self.st = st
        self.title_lines, self.body_lines, self.need = measure(
            title, body, self.w, st["title_size"], st["body_size"],
            st["mono"], st["title_mono"])

    @property
    def geom(self):
        return (self.x, self.y, self.w, self.h)

    def resize_bottom(self, bottom):
        """Grow or shrink downwards to `bottom` (top-down inches)."""
        self.h = bottom - self.y
        return self

    def xml(self):
        st = self.st
        px = self.x + self.w / 2.0
        py = self.page.height - (self.y + self.h / 2.0)

        chars = (
            "<Row IX='0'>"
            + _cell("Color", st["title_color"]) + _cell("Size", pt(st["title_size"]))
            + _cell("Style", 17 if st["title_bold"] else 0)
            + _cell("Font", "Consolas" if st["title_mono"] else "Segoe UI")
            + "</Row><Row IX='1'>"
            + _cell("Color", st["body_color"]) + _cell("Size", pt(st["body_size"]))
            + _cell("Style", 0)
            + _cell("Font", "Consolas" if st["mono"] else "Segoe UI")
            + "</Row>")

        runs = []
        if self.title_lines:
            runs.append("<cp IX='0'/>" + escape("\n".join(self.title_lines)))
        if self.body_lines:
            runs.append("<cp IX='1'/>" + escape("\n".join(self.body_lines)))
        text = "\n".join(runs)

        return (
            "<Shape ID='%d' NameU='Box.%d' Type='Shape'>" % (self.id, self.id)
            + _cell("PinX", round(px, 5)) + _cell("PinY", round(py, 5))
            + _cell("Width", round(self.w, 5)) + _cell("Height", round(self.h, 5))
            + _cell("LocPinX", round(self.w / 2.0, 5), "Width*0.5")
            + _cell("LocPinY", round(self.h / 2.0, 5), "Height*0.5")
            + _cell("Angle", 0) + _cell("FlipX", 0) + _cell("FlipY", 0)
            + _cell("ResizeMode", 0)
            + _cell("FillForegnd", st["fill"]) + _cell("FillBkgnd", st["fill"])
            + _cell("FillPattern", 0 if st["no_fill"] else 1)
            + _cell("FillForegndTrans", 0) + _cell("FillBkgndTrans", 0)
            + _cell("LineColor", st["line"]) + _cell("LineWeight", st["line_weight"])
            + _cell("LinePattern", 0 if st["no_line"] else (2 if st["dashed"] else 1))
            + _cell("Rounding", st["rounding"]) + _cell("LineCap", 0)
            + _cell("BeginArrow", 0) + _cell("EndArrow", 0)
            + _cell("VerticalAlign", st["valign"])
            + _cell("LeftMargin", _H_MARGIN) + _cell("RightMargin", _H_MARGIN)
            + _cell("TopMargin", _V_MARGIN) + _cell("BottomMargin", _V_MARGIN)
            + _cell("TextBkgnd", 0)
            + "<Section N='Character'>" + chars + "</Section>"
            + "<Section N='Paragraph'><Row IX='0'>"
            + _cell("HorzAlign", st["halign"]) + _cell("SpLine", -1.1)
            + _cell("SpAfter", 0.0) + "</Row></Section>"
            + self.geometry_xml()
            + ("<Text>%s</Text>" % text if text else "")
            + "</Shape>")

    # -- geometry, overridable by every symbol ---------------------------
    def geometry_xml(self):
        st = self.st
        return ("<Section N='Geometry' IX='0'>"
                + _cell("NoFill", 1 if st["no_fill"] else 0)
                + _cell("NoLine", 1 if st["no_line"] else 0)
                + "<Row T='RelMoveTo' IX='1'>" + _cell("X", 0) + _cell("Y", 0) + "</Row>"
                + "<Row T='RelLineTo' IX='2'>" + _cell("X", 1) + _cell("Y", 0) + "</Row>"
                + "<Row T='RelLineTo' IX='3'>" + _cell("X", 1) + _cell("Y", 1) + "</Row>"
                + "<Row T='RelLineTo' IX='4'>" + _cell("X", 0) + _cell("Y", 1) + "</Row>"
                + "<Row T='RelLineTo' IX='5'>" + _cell("X", 0) + _cell("Y", 0) + "</Row>"
                + "</Section>")

    def svg_shape(self, px_in):
        st = self.st
        if st["no_fill"] and st["no_line"]:
            return []
        return ["<rect x='%.2f' y='%.2f' width='%.2f' height='%.2f' rx='%.2f' "
                "fill='%s' stroke='%s' stroke-width='%.2f'%s/>" % (
                    self.x * px_in, self.y * px_in, self.w * px_in, self.h * px_in,
                    st["rounding"] * px_in,
                    "none" if st["no_fill"] else st["fill"],
                    "none" if st["no_line"] else st["line"],
                    max(st["line_weight"] * px_in, 0.6),
                    " stroke-dasharray='6 4'" if st["dashed"] else "")]


class Poly(Box):
    """A symbol: one or more paths in TOP-DOWN relative (0..1) coordinates.

    Visio's local y runs upwards, which makes an asymmetric shape easy to
    author upside down, so paths are written here with y=0 at the TOP and
    flipped on the way into the XML. A path is either

        {"pts": [(rx, ry), ...], "close": bool, "fill": bool, "line": bool}

    or a true ellipse, which Visio has a row type for and which is worth
    using rather than approximating:

        {"ellipse": (cx, cy, rx, ry), "fill": bool, "line": bool}
    """

    def __init__(self, page, x, y, w, h, paths, title, body, **st):
        self.paths = paths
        super().__init__(page, x, y, w, h, title, body, **st)

    def geometry_xml(self):
        st = self.st
        out = []
        for i, path in enumerate(self.paths):
            fill = path.get("fill", True) and not st["no_fill"]
            line = path.get("line", True) and not st["no_line"]
            rows = []
            if "ellipse" in path:
                cx, cy, rx, ry = path["ellipse"]
                # centre, then a point at the right and a point at the top;
                # Visio's y is up, so the top-down cy is mirrored
                rows.append(
                    "<Row T='Ellipse' IX='1'>"
                    + _cell("X", round(cx * self.w, 5))
                    + _cell("Y", round((1 - cy) * self.h, 5))
                    + _cell("A", round((cx + rx) * self.w, 5))
                    + _cell("B", round((1 - cy) * self.h, 5))
                    + _cell("C", round(cx * self.w, 5))
                    + _cell("D", round((1 - (cy - ry)) * self.h, 5))
                    + "</Row>")
            else:
                pts = list(path["pts"])
                if path.get("close", True) and pts[0] != pts[-1]:
                    pts.append(pts[0])
                for j, (rx, ry) in enumerate(pts):
                    rows.append(
                        "<Row T='%s' IX='%d'>%s%s</Row>" % (
                            "RelMoveTo" if j == 0 else "RelLineTo", j + 1,
                            _cell("X", round(rx, 5)),
                            _cell("Y", round(1.0 - ry, 5))))
            out.append("<Section N='Geometry' IX='%d'>" % i
                       + _cell("NoFill", 0 if fill else 1)
                       + _cell("NoLine", 0 if line else 1)
                       + "".join(rows) + "</Section>")
        return "".join(out)

    def svg_shape(self, px_in):
        st = self.st
        X = lambda rx: (self.x + rx * self.w) * px_in
        Y = lambda ry: (self.y + ry * self.h) * px_in
        stroke_w = max(st["line_weight"] * px_in, 0.6)
        dash = " stroke-dasharray='6 4'" if st["dashed"] else ""
        out = []
        for path in self.paths:
            fill = st["fill"] if (path.get("fill", True)
                                  and not st["no_fill"]) else "none"
            stroke = st["line"] if (path.get("line", True)
                                    and not st["no_line"]) else "none"
            if "ellipse" in path:
                cx, cy, rx, ry = path["ellipse"]
                out.append("<ellipse cx='%.2f' cy='%.2f' rx='%.2f' ry='%.2f' "
                           "fill='%s' stroke='%s' stroke-width='%.2f'%s/>"
                           % (X(cx), Y(cy), rx * self.w * px_in,
                              ry * self.h * px_in, fill, stroke, stroke_w, dash))
            else:
                d = " ".join("%s%.2f,%.2f" % ("M" if j == 0 else "L", X(a), Y(b))
                             for j, (a, b) in enumerate(path["pts"]))
                if path.get("close", True):
                    d += " Z"
                out.append("<path d='%s' fill='%s' stroke='%s' "
                           "stroke-width='%.2f' stroke-linejoin='round'%s/>"
                           % (d, fill, stroke, stroke_w, dash))
        return out


# ---------------------------------------------------------------------
# the symbols — path generators in top-down relative coordinates
# ---------------------------------------------------------------------
def _arc(cx, cy, rx, ry, a0, a1, n=24):
    """Points along an ellipse arc, angles in degrees, y measured DOWNWARDS."""
    return [(cx + rx * math.cos(math.radians(a0 + (a1 - a0) * i / n)),
             cy + ry * math.sin(math.radians(a0 + (a1 - a0) * i / n)))
            for i in range(n + 1)]


def rect_paths(**_):
    return [{"pts": [(0, 0), (1, 0), (1, 1), (0, 1)]}]


def hexagon_paths(notch=0.18, **_):
    return [{"pts": [(notch, 0), (1 - notch, 0), (1, 0.5),
                     (1 - notch, 1), (notch, 1), (0, 0.5)]}]


def diamond_paths(**_):
    return [{"pts": [(0.5, 0), (1, 0.5), (0.5, 1), (0, 0.5)]}]


def parallelogram_paths(skew=0.14, **_):
    return [{"pts": [(skew, 0), (1, 0), (1 - skew, 1), (0, 1)]}]


def cylinder_paths(cap=0.16, **_):
    """The classic database symbol: a tube with an elliptical rim."""
    body = ([(0.0, cap / 2)]
            + [(0.0, 1 - cap / 2)]
            + _arc(0.5, 1 - cap / 2, 0.5, cap / 2, 180, 0)     # bottom bulge
            + [(1.0, cap / 2)]
            + _arc(0.5, cap / 2, 0.5, cap / 2, 0, -180))       # top, round the back
    rim = _arc(0.5, cap / 2, 0.5, cap / 2, 180, 0)             # the visible front rim
    return [{"pts": body}, {"pts": rim, "close": False, "fill": False}]


def box3d_paths(depth=0.13, **_):
    """A server: a front face with a lid and a flank."""
    d = depth
    front = [(0, d), (1 - d, d), (1 - d, 1), (0, 1)]
    lid = [(0, d), (d, 0), (1, 0), (1 - d, d)]
    flank = [(1 - d, d), (1, 0), (1, 1 - d), (1 - d, 1)]
    return [{"pts": front}, {"pts": lid}, {"pts": flank}]


def datastore_paths(**_):
    """The data-flow-diagram store: open on the right, no box around it."""
    return [{"pts": [(0, 0), (1, 0), (1, 1), (0, 1)], "line": False},
            {"pts": [(0, 0), (1, 0)], "close": False, "fill": False},
            {"pts": [(0, 1), (1, 1)], "close": False, "fill": False},
            {"pts": [(0, 0), (0, 1)], "close": False, "fill": False}]


def document_paths(**_):
    """A page with the usual rippled foot."""
    n = 24
    wave = [(1 - i / float(n),
             0.90 + 0.055 * math.sin(math.radians(360 * i / float(n))))
            for i in range(n + 1)]
    return [{"pts": [(0, 0), (1, 0)] + wave}]


def ellipse_paths(**_):
    return [{"ellipse": (0.5, 0.5, 0.5, 0.5)}]


def actor_paths(**_):
    """A person. Authored in a square box, because a stick figure in a
    wide one is a stick figure that has been sat on."""
    return [{"ellipse": (0.5, 0.15, 0.13, 0.13)},
            {"pts": [(0.5, 0.29), (0.5, 0.62)], "close": False, "fill": False},
            {"pts": [(0.22, 0.42), (0.78, 0.42)], "close": False, "fill": False},
            {"pts": [(0.5, 0.62), (0.26, 0.97)], "close": False, "fill": False},
            {"pts": [(0.5, 0.62), (0.74, 0.97)], "close": False, "fill": False}]


def cloud_paths(bumps=9, **_):
    """A cloud — for a boundary that is somebody else's.

    Traced as the UNION outline of overlapping bumps, by taking the
    farthest intersection along each ray from the centre. Stitching the
    bumps' own arcs together instead draws their inner halves too, and
    the result is a flower.
    """
    ring, br = 0.30, 0.21
    centres = [(ring * math.cos(2 * math.pi * i / bumps),
                ring * math.sin(2 * math.pi * i / bumps) * 0.78)
               for i in range(bumps)]
    pts = []
    for step in range(120):
        th = math.radians(360.0 * step / 120)
        ux, uy = math.cos(th), math.sin(th)
        best = 0.0
        for cx, cy in centres:
            proj = cx * ux + cy * uy
            disc = br * br - (cx * cx + cy * cy) + proj * proj
            if disc > 0:
                best = max(best, proj + math.sqrt(disc))
        pts.append((0.5 + best * ux, 0.5 + best * uy * 1.25))
    return [{"pts": pts}]


SYMBOLS = {
    "rect": rect_paths, "hexagon": hexagon_paths, "diamond": diamond_paths,
    "parallelogram": parallelogram_paths, "cylinder": cylinder_paths,
    "box3d": box3d_paths, "datastore": datastore_paths,
    "document": document_paths, "ellipse": ellipse_paths,
    "circle": ellipse_paths, "actor": actor_paths, "cloud": cloud_paths,
}


class Arrow:
    kind = "arrow"

    def __init__(self, page, points, color, weight, dashed, end_arrow,
                 begin_arrow, arrow_size):
        self.page, self.id = page, None
        self.points = [(float(a), float(b)) for a, b in points]
        self.color, self.weight, self.dashed = color, weight, dashed
        self.end_arrow, self.begin_arrow = end_arrow, begin_arrow
        self.arrow_size = arrow_size

    def xml(self):
        fp = [(a, self.page.height - b) for a, b in self.points]
        xs, ys = [q[0] for q in fp], [q[1] for q in fp]
        minx, maxx, miny, maxy = min(xs), max(xs), min(ys), max(ys)
        w, h = max(maxx - minx, 0.0001), max(maxy - miny, 0.0001)
        rows = "".join(
            "<Row T='%s' IX='%d'>%s%s</Row>" % (
                "MoveTo" if i == 0 else "LineTo", i + 1,
                _cell("X", round(gx - minx, 5)), _cell("Y", round(gy - miny, 5)))
            for i, (gx, gy) in enumerate(fp))
        return (
            "<Shape ID='%d' NameU='Arrow.%d' Type='Shape'>" % (self.id, self.id)
            + _cell("PinX", round(minx + w / 2.0, 5))
            + _cell("PinY", round(miny + h / 2.0, 5))
            + _cell("Width", round(w, 5)) + _cell("Height", round(h, 5))
            + _cell("LocPinX", round(w / 2.0, 5), "Width*0.5")
            + _cell("LocPinY", round(h / 2.0, 5), "Height*0.5")
            + _cell("Angle", 0) + _cell("FlipX", 0) + _cell("FlipY", 0)
            + _cell("BeginX", round(fp[0][0], 5)) + _cell("BeginY", round(fp[0][1], 5))
            + _cell("EndX", round(fp[-1][0], 5)) + _cell("EndY", round(fp[-1][1], 5))
            + _cell("LineColor", self.color) + _cell("LineWeight", self.weight)
            + _cell("LinePattern", 2 if self.dashed else 1) + _cell("LineCap", 0)
            + _cell("Rounding", 0.04)
            + _cell("BeginArrow", self.begin_arrow) + _cell("EndArrow", self.end_arrow)
            + _cell("BeginArrowSize", self.arrow_size)
            + _cell("EndArrowSize", self.arrow_size)
            + _cell("FillPattern", 0)
            + "<Section N='Geometry' IX='0'>" + _cell("NoFill", 1)
            + _cell("NoLine", 0) + rows + "</Section></Shape>")


_BOX_DEFAULTS = dict(
    fill="#FFFFFF", line="#404040", line_weight=0.0104, dashed=False,
    rounding=0.06, title_size=10.0, body_size=7.5, title_color="#101010",
    body_color="#333333", title_bold=True, valign=0, halign=0, mono=False,
    title_mono=False, no_fill=False, no_line=False, ok_overlap=False,
)


class Page:
    def __init__(self, name, width, height):
        self.name, self.width, self.height = name, float(width), float(height)
        self.shapes = []

    def box(self, x, y, w, h, title="", body="", **kw):
        st = dict(_BOX_DEFAULTS)
        st.update(kw)
        s = Box(self, x, y, w, h, title, body, **st)
        self.shapes.append(s)
        return s

    def fitbox(self, x, y, w, title="", body="", pad=0.12, min_h=0.0, **kw):
        """A box whose height is DERIVED from the text it holds."""
        st = dict(_BOX_DEFAULTS)
        st.update(kw)
        _, _, need = measure(title, body, w, st["title_size"], st["body_size"],
                             st["mono"], st["title_mono"])
        return self.box(x, y, w, max(need + pad, min_h), title, body, **kw)

    def text(self, x, y, w, body, size=8.0, color="#333333", bold=False,
             halign=0, mono=False, fill=None, ok_overlap=True, label=False):
        """A caption: no line, and a fill only where it must mask a line.

        `label=True` marks it as an arrow label, which opts it into
        `label_overlap_report()`: a filled label laid over another one
        silently CLIPS the text underneath, and the general overlap gate
        cannot see it because captions are allowed to sit on boxes.
        """
        s = self.fitbox(
            x, y, w, body, "", pad=0.06,
            title_size=size, title_color=color, title_bold=bold,
            title_mono=mono, fill=fill or "#FFFFFF", no_fill=fill is None,
            no_line=True, halign=halign, rounding=0, ok_overlap=ok_overlap)
        s.is_label = label
        return s

    def symbol(self, kind, x, y, w, h, title="", body="", **kw):
        """One of `SYMBOLS` — cylinder, hexagon, box3d, actor, cloud, … —
        sized and filled like a box but drawn with its own geometry."""
        st = dict(_BOX_DEFAULTS)
        st.update(kw)
        shape_kw = {k: st.pop(k) for k in ("notch", "cap", "depth", "skew",
                                           "bumps") if k in st}
        try:
            paths = SYMBOLS[kind](**shape_kw)
        except KeyError:
            raise ValueError("unknown symbol %r; have %s"
                             % (kind, ", ".join(sorted(SYMBOLS))))
        s = Poly(self, x, y, w, h, paths, title, body, **st)
        self.shapes.append(s)
        return s

    def arrow(self, points, color="#404040", weight=0.0139, dashed=False,
              end_arrow=ARROW_FILLED, begin_arrow=0, arrow_size=2):
        s = Arrow(self, points, color, weight, dashed, end_arrow, begin_arrow,
                  arrow_size)
        self.shapes.append(s)
        return s

    def stack(self, x, y, w, gap=0.2):
        return Stack(self, x, y, w, gap)

    def trim(self, margin=0.45):
        """Shrink the page to what was actually drawn on it.

        Safe to call after layout: y is stored top-down and flipped only
        at save time, so changing the height moves nothing.
        """
        bottom = right = 0.0
        for s in self.shapes:
            if s.kind == "box":
                bottom = max(bottom, s.y + s.h)
                right = max(right, s.x + s.w)
            else:
                bottom = max(bottom, max(b for _, b in s.points))
                right = max(right, max(a for a, _ in s.points))
        self.height = bottom + margin
        self.width = right + margin
        return self

    def contents_xml(self):
        for i, s in enumerate(self.shapes):
            s.id = i + 1
        return ("<?xml version='1.0' encoding='UTF-8' standalone='yes'?>\r\n"
                "<PageContents xmlns='%s' xmlns:r='%s' xml:space='preserve'>"
                "<Shapes>%s</Shapes></PageContents>"
                % (NS, RNS, "".join(s.xml() for s in self.shapes)))


class Stack:
    """A column that places each box directly under the last, at the
    height its own text needs. Nothing in a stack overflows, and nothing
    in it carries dead space."""

    def __init__(self, page, x, y, w, gap=0.2):
        self.page, self.x, self.w, self.gap = page, x, w, gap
        self.y0 = self.y = y

    def box(self, title="", body="", pad=0.12, min_h=0.0, gap=None, **kw):
        s = self.page.fitbox(self.x, self.y, self.w, title, body,
                             pad=pad, min_h=min_h, **kw)
        self.y += s.h + (self.gap if gap is None else gap)
        return s

    def text(self, body, size=8.0, gap=None, **kw):
        s = self.page.text(self.x, self.y, self.w, body, size=size, **kw)
        self.y += s.h + (self.gap if gap is None else gap)
        return s

    def skip(self, dy):
        self.y += dy

    @property
    def bottom(self):
        """Where the last box ended, without the trailing gap."""
        return self.y - self.gap


# ---------------------------------------------------------------------
# document
# ---------------------------------------------------------------------
class Document:
    def __init__(self, title="", creator="", description=""):
        self.pages = []
        self.title, self.creator, self.description = title, creator, description

    def page(self, name, width=24.0, height=15.0):
        p = Page(name, width, height)
        self.pages.append(p)
        return p

    # -- gates ---------------------------------------------------------
    def check(self):
        """Overflowing text and off-page shapes: both invisible in Visio."""
        out = []
        for p in self.pages:
            for s in p.shapes:
                if s.kind != "box":
                    continue
                name = (s.title_lines or s.body_lines or ["?"])[0][:38]
                if s.need > s.h + 1e-6:
                    out.append("[%s] OVERFLOW %-38s needs %.3f\" in %.3f\""
                               % (p.name, name, s.need, s.h))
                if (s.x < -1e-6 or s.y < -1e-6
                        or s.x + s.w > p.width + 1e-6
                        or s.y + s.h > p.height + 1e-6):
                    out.append("[%s] OFFPAGE  %-38s (%.2f,%.2f %.2fx%.2f) on %.1fx%.1f"
                               % (p.name, name, s.x, s.y, s.w, s.h, p.width, p.height))
        return out

    def slack_report(self, floor=0.55):
        out = []
        for p in self.pages:
            for s in p.shapes:
                if s.kind != "box" or not (s.title_lines or s.body_lines):
                    continue
                if s.h - s.need > floor:
                    out.append("[%s] SLACK %5.2f\" %-38s box %.2f\" needs %.2f\""
                               % (p.name, s.h - s.need,
                                  (s.title_lines or s.body_lines)[0][:38],
                                  s.h, s.need))
        return out

    def label_on_line_report(self, inset=0.03):
        """Labels an arrow is drawn straight through.

        A label with a paper fill hides the line and punches a hole in
        it; a TRANSPARENT one lets the line strike the text out. Neither
        is acceptable, so a label belongs BESIDE its arrow, and this is
        what says whether it is.
        """
        out = []
        for p in self.pages:
            labels = [s for s in p.shapes if s.kind == "box" and s.is_label]
            segs = []
            for s in p.shapes:
                if s.kind == "arrow":
                    segs += list(zip(s.points, s.points[1:]))
            for lb in labels:
                # inset, so merely touching an edge is not a finding
                x0, y0 = lb.x + inset, lb.y + inset
                x1, y1 = lb.x + lb.w - inset, lb.y + lb.h - inset
                if x1 <= x0 or y1 <= y0:
                    continue
                for (ax, ay), (bx, by) in segs:
                    if _seg_hits_rect(ax, ay, bx, by, x0, y0, x1, y1):
                        out.append("[%s] LABEL ON LINE  %r"
                                   % (p.name, " ".join(lb.title_lines)[:34]))
                        break
        return out

    def label_overlap_report(self, tol=0.01):
        """Arrow labels laid over one another.

        A label carries a paper fill so it breaks the line it names, so
        two of them overlapping does not look wrong — the one drawn
        second just eats the other's last few characters. Nothing else
        catches that.
        """
        out = []
        for p in self.pages:
            ls = [s for s in p.shapes if s.kind == "box" and s.is_label]
            for i in range(len(ls)):
                for j in range(i + 1, len(ls)):
                    a, b = ls[i], ls[j]
                    ox = min(a.x + a.w, b.x + b.w) - max(a.x, b.x)
                    oy = min(a.y + a.h, b.y + b.h) - max(a.y, b.y)
                    if ox > tol and oy > tol:
                        out.append(
                            "[%s] LABELS CLASH %.2f x %.2f\"  %r over %r"
                            % (p.name, ox, oy,
                               " ".join(a.title_lines)[:24],
                               " ".join(b.title_lines)[:24]))
        return out

    def overlap_report(self, tol=0.02):
        """Intersecting boxes that are not nested one inside the other."""
        out = []
        for p in self.pages:
            rs = [s for s in p.shapes if s.kind == "box"]
            for i in range(len(rs)):
                for j in range(i + 1, len(rs)):
                    a, b = rs[i], rs[j]
                    if a.ok_overlap or b.ok_overlap:
                        continue
                    ox = min(a.x + a.w, b.x + b.w) - max(a.x, b.x)
                    oy = min(a.y + a.h, b.y + b.h) - max(a.y, b.y)
                    if ox <= tol or oy <= tol:
                        continue
                    nested = (
                        (a.x >= b.x - tol and a.y >= b.y - tol
                         and a.x + a.w <= b.x + b.w + tol
                         and a.y + a.h <= b.y + b.h + tol)
                        or (b.x >= a.x - tol and b.y >= a.y - tol
                            and b.x + b.w <= a.x + a.w + tol
                            and b.y + b.h <= a.y + a.h + tol))
                    if nested:
                        continue
                    out.append("[%s] OVERLAP %.2f x %.2f\" %-32s × %-32s"
                               % (p.name, ox, oy,
                                  (a.title_lines or a.body_lines or ["?"])[0][:32],
                                  (b.title_lines or b.body_lines or ["?"])[0][:32]))
        return out

    # -- package parts -------------------------------------------------
    def _content_types(self):
        ov = [("/docProps/app.xml",
               "application/vnd.openxmlformats-officedocument.extended-properties+xml"),
              ("/docProps/core.xml",
               "application/vnd.openxmlformats-package.core-properties+xml"),
              ("/visio/document.xml", "application/vnd.ms-visio.drawing.main+xml"),
              ("/visio/pages/pages.xml", "application/vnd.ms-visio.pages+xml"),
              ("/visio/windows.xml", "application/vnd.ms-visio.windows+xml")]
        ov += [("/visio/pages/page%d.xml" % (i + 1), "application/vnd.ms-visio.page+xml")
               for i in range(len(self.pages))]
        return ("<?xml version='1.0' encoding='UTF-8' standalone='yes'?>\r\n"
                "<Types xmlns='http://schemas.openxmlformats.org/package/2006/"
                "content-types'><Default Extension='rels' ContentType='application/"
                "vnd.openxmlformats-package.relationships+xml'/>"
                "<Default Extension='xml' ContentType='application/xml'/>%s</Types>"
                % "".join("<Override PartName='%s' ContentType='%s'/>" % t for t in ov))

    def _root_rels(self):
        return ("<?xml version='1.0' encoding='UTF-8' standalone='yes'?>\r\n"
                "<Relationships xmlns='http://schemas.openxmlformats.org/package/"
                "2006/relationships'>"
                "<Relationship Id='rId1' Type='%s/document' Target='visio/document.xml'/>"
                "<Relationship Id='rId2' Type='http://schemas.openxmlformats.org/"
                "package/2006/relationships/metadata/core-properties' "
                "Target='docProps/core.xml'/>"
                "<Relationship Id='rId3' Type='http://schemas.openxmlformats.org/"
                "officeDocument/2006/relationships/extended-properties' "
                "Target='docProps/app.xml'/></Relationships>" % VREL)

    def _document_rels(self):
        return ("<?xml version='1.0' encoding='UTF-8' standalone='yes'?>\r\n"
                "<Relationships xmlns='http://schemas.openxmlformats.org/package/"
                "2006/relationships'>"
                "<Relationship Id='rId1' Type='%s/pages' Target='pages/pages.xml'/>"
                "<Relationship Id='rId2' Type='%s/windows' Target='windows.xml'/>"
                "</Relationships>" % (VREL, VREL))

    def _document(self):
        face = ("<FaceName NameU='Segoe UI' UnicodeRanges='-1 -369098753 63 0' "
                "CharSets='1073742335 -65536' Panos='2 11 5 2 4 2 4 2 2 3' "
                "Flags='325'/><FaceName NameU='Consolas' "
                "UnicodeRanges='-1 -251658241 15 0' CharSets='-1342176593 1' "
                "Panos='2 11 6 9 2 2 4 3 2 4' Flags='325'/>")
        return ("<?xml version='1.0' encoding='UTF-8' standalone='yes'?>\r\n"
                "<VisioDocument xmlns='%s' xmlns:r='%s' xml:space='preserve'>"
                "<DocumentSettings TopPage='0' DefaultTextStyle='0' "
                "DefaultLineStyle='0' DefaultFillStyle='0' DefaultGuideStyle='0'>"
                "<GlueSettings>9</GlueSettings><SnapSettings>65847</SnapSettings>"
                "<SnapExtensions>34</SnapExtensions><SnapAngles/>"
                "<DynamicGridEnabled>1</DynamicGridEnabled>"
                "<ProtectStyles>0</ProtectStyles><ProtectShapes>0</ProtectShapes>"
                "<ProtectMasters>0</ProtectMasters><ProtectBkgnds>0</ProtectBkgnds>"
                "</DocumentSettings><Colors/><FaceNames>%s</FaceNames>"
                "<StyleSheets><StyleSheet ID='0' NameU='No Style' Name='No Style'>"
                "%s%s%s%s%s%s</StyleSheet></StyleSheets></VisioDocument>"
                % (NS, RNS, face,
                   _cell("LineWeight", 0.01), _cell("LineColor", "#000000"),
                   _cell("LinePattern", 1), _cell("FillForegnd", "#FFFFFF"),
                   _cell("FillPattern", 1), _cell("CharSize", pt(8))))

    def _pages(self):
        out = []
        for i, p in enumerate(self.pages):
            out.append(
                "<Page ID='%d' NameU='%s' Name='%s' ViewScale='-1' "
                "ViewCenterX='%.4f' ViewCenterY='%.4f'>"
                "<PageSheet LineStyle='0' FillStyle='0' TextStyle='0'>%s%s%s%s%s%s%s%s"
                "</PageSheet><Rel r:id='rId%d'/></Page>" % (
                    i, escape(p.name, {"'": "&apos;"}), escape(p.name, {"'": "&apos;"}),
                    p.width / 2.0, p.height / 2.0,
                    _cell("PageWidth", p.width), _cell("PageHeight", p.height),
                    _cell("ShdwOffsetX", 0.1181), _cell("ShdwOffsetY", -0.1181),
                    _cell("PageScale", 1, "1 in"), _cell("DrawingScale", 1, "1 in"),
                    _cell("DrawingSizeType", 3), _cell("DrawingScaleType", 0),
                    i + 1))
        return ("<?xml version='1.0' encoding='UTF-8' standalone='yes'?>\r\n"
                "<Pages xmlns='%s' xmlns:r='%s' xml:space='preserve'>%s</Pages>"
                % (NS, RNS, "".join(out)))

    def _pages_rels(self):
        return ("<?xml version='1.0' encoding='UTF-8' standalone='yes'?>\r\n"
                "<Relationships xmlns='http://schemas.openxmlformats.org/package/"
                "2006/relationships'>%s</Relationships>"
                % "".join("<Relationship Id='rId%d' Type='%s/page' "
                          "Target='page%d.xml'/>" % (i + 1, VREL, i + 1)
                          for i in range(len(self.pages))))

    def _windows(self):
        return ("<?xml version='1.0' encoding='UTF-8' standalone='yes'?>\r\n"
                "<Windows xmlns='%s' xmlns:r='%s' ClientWidth='1600' "
                "ClientHeight='900'><Window ID='0' WindowType='Drawing' "
                "WindowState='1073741824' Document='visio/document.xml' Page='0' "
                "ViewScale='-1' ViewCenterX='%.4f' ViewCenterY='%.4f'/></Windows>"
                % (NS, RNS, self.pages[0].width / 2.0, self.pages[0].height / 2.0))

    def _core(self):
        return ("<?xml version='1.0' encoding='UTF-8' standalone='yes'?>\r\n"
                "<cp:coreProperties xmlns:cp='http://schemas.openxmlformats.org/"
                "package/2006/metadata/core-properties' "
                "xmlns:dc='http://purl.org/dc/elements/1.1/' "
                "xmlns:dcterms='http://purl.org/dc/terms/' "
                "xmlns:xsi='http://www.w3.org/2001/XMLSchema-instance'>"
                "<dc:title>%s</dc:title><dc:creator>%s</dc:creator>"
                "<dc:description>%s</dc:description>"
                "<cp:lastModifiedBy>%s</cp:lastModifiedBy></cp:coreProperties>"
                % (escape(self.title), escape(self.creator),
                   escape(self.description), escape(self.creator)))

    def _app(self):
        return ("<?xml version='1.0' encoding='UTF-8' standalone='yes'?>\r\n"
                "<Properties xmlns='http://schemas.openxmlformats.org/"
                "officeDocument/2006/extended-properties' xmlns:vt='http://"
                "schemas.openxmlformats.org/officeDocument/2006/docPropsVTypes'>"
                "<Application>Microsoft Visio</Application>"
                "<AppVersion>15.0000</AppVersion><TitlesOfParts>"
                "<vt:vector size='%d' baseType='lpstr'>%s</vt:vector>"
                "</TitlesOfParts></Properties>"
                % (len(self.pages),
                   "".join("<vt:lpstr>%s</vt:lpstr>" % escape(p.name)
                           for p in self.pages)))

    def save(self, path):
        parts = {
            "_rels/.rels": self._root_rels(),
            "docProps/app.xml": self._app(),
            "docProps/core.xml": self._core(),
            "visio/document.xml": self._document(),
            "visio/_rels/document.xml.rels": self._document_rels(),
            "visio/windows.xml": self._windows(),
            "visio/pages/pages.xml": self._pages(),
            "visio/pages/_rels/pages.xml.rels": self._pages_rels(),
        }
        for i, p in enumerate(self.pages):
            parts["visio/pages/page%d.xml" % (i + 1)] = p.contents_xml()
        with zipfile.ZipFile(path, "w", zipfile.ZIP_DEFLATED) as z:
            # [Content_Types].xml first, as the OPC convention expects.
            z.writestr("[Content_Types].xml", self._content_types())
            for name in sorted(parts):
                z.writestr(name, parts[name])
        return path

    def save_svg(self, prefix, px_in=96.0):
        out = []
        for i, p in enumerate(self.pages):
            path = "%s-%d.svg" % (prefix, i + 1)
            with open(path, "w") as fh:
                fh.write(_page_svg(p, px_in))
            out.append(path)
        return out


# ---------------------------------------------------------------------
# SVG preview — the same shape list, so the layout can be looked at
# ---------------------------------------------------------------------
def text_runs(s):
    """Where each line of a box's text sits, in inches.

    THE one place text is laid out. Both back ends — the SVG preview and
    the EMF export — consume this, so the picture you look at and the
    picture you hand to Office cannot drift apart.

    Returns dicts of anchor x, BASELINE y, the string, its point size
    and colour, whether it is bold or monospaced, and which end of the
    text the anchor x refers to.
    """
    st = s.st
    th = len(s.title_lines) * (st["title_size"] / 72.0) * _LINE_SPACING
    bh = len(s.body_lines) * (st["body_size"] / 72.0) * _LINE_SPACING
    total = th + bh
    if st["valign"] == 0:
        top = s.y + _V_MARGIN
    elif st["valign"] == 2:
        top = s.y + s.h - _V_MARGIN - total
    else:
        top = s.y + (s.h - total) / 2.0
    if st["halign"] == 0:
        tx, anchor = s.x + _H_MARGIN, "start"
    elif st["halign"] == 2:
        tx, anchor = s.x + s.w - _H_MARGIN, "end"
    else:
        tx, anchor = s.x + s.w / 2.0, "middle"

    out, cur = [], top
    for lines, size, color, bold, mono in (
            (s.title_lines, st["title_size"], st["title_color"],
             st["title_bold"], st["title_mono"]),
            (s.body_lines, st["body_size"], st["body_color"], False,
             st["mono"])):
        lh = (size / 72.0) * _LINE_SPACING
        for ln in lines:
            if ln:
                out.append(dict(x=tx, y=cur + lh * 0.80, text=ln, size=size,
                                color=color, bold=bold, mono=mono,
                                anchor=anchor))
            cur += lh
    return out


def _svg_text(s, px_in):
    # xml:space is required, or SVG collapses the leading spaces that
    # align every key map on these pages — and the preview would libel a
    # file that is actually fine.
    return ["<text xml:space='preserve' x='%.2f' y='%.2f' "
            "font-family=\"%s\" font-size='%.2f' fill='%s' "
            "text-anchor='%s'%s>%s</text>"
            % (r["x"] * px_in, r["y"] * px_in,
               "Consolas, Menlo, monospace" if r["mono"]
               else "Segoe UI, Helvetica, Arial, sans-serif",
               r["size"] * px_in / 72.0, r["color"], r["anchor"],
               " font-weight='600'" if r["bold"] else "", escape(r["text"]))
            for r in text_runs(s)]


def _page_svg(page, px_in=96.0):
    body = ["<rect x='0' y='0' width='%.1f' height='%.1f' fill='#FFFFFF'/>"
            % (page.width * px_in, page.height * px_in)]
    for s in page.shapes:
        if s.kind == "box":
            body.extend(s.svg_shape(px_in))
            body.extend(_svg_text(s, px_in))
        else:
            body.append(
                "<polyline points='%s' fill='none' stroke='%s' stroke-width='%.2f'%s "
                "%s%s/>" % (
                    " ".join("%.2f,%.2f" % (a * px_in, b * px_in) for a, b in s.points),
                    s.color, max(s.weight * px_in, 1.0),
                    " stroke-dasharray='6 4'" if s.dashed else "",
                    "marker-end='url(#ah-%s)' " % s.color.lstrip("#")
                    if s.end_arrow else "",
                    "marker-start='url(#ahs-%s)'" % s.color.lstrip("#")
                    if s.begin_arrow else ""))

    defs = []
    for c in {s.color for s in page.shapes if s.kind == "arrow"}:
        for pfx, path, refx in (("ah", "M0,0 L8,3 L0,6 z", "7.5"),
                                ("ahs", "M8,0 L0,3 L8,6 z", "0.5")):
            defs.append("<marker id='%s-%s' markerWidth='8' markerHeight='6' "
                        "refX='%s' refY='3' orient='auto' markerUnits='strokeWidth'>"
                        "<path d='%s' fill='%s'/></marker>"
                        % (pfx, c.lstrip("#"), refx, path, c))

    return ("<svg xmlns='http://www.w3.org/2000/svg' width='%.0f' height='%.0f' "
            "viewBox='0 0 %.0f %.0f'><defs>%s</defs>%s</svg>"
            % (page.width * px_in, page.height * px_in,
               page.width * px_in, page.height * px_in,
               "".join(defs), "".join(body)))
