"""An EMF (Enhanced Metafile) back end for `vsdxkit`.

Office and Visio embed EMF as true vector art — it scales, and its text
stays text — so this is the format to hand them when a `.vsdx` is not
what is wanted. There is no converter and no library here, so the file
is written record by record against MS-EMF, the same way `vsdxkit`
writes the OPC package.

It consumes the SAME shape list and the SAME `vsdxkit.text_runs()` as
the SVG preview, so what you looked at is what Office gets.

Two deliberate choices worth knowing:

* Logical units are 0.01 mm (2540 per inch), which makes `rclBounds`
  and `rclFrame` agree numerically and keeps font heights exact — at
  96 units per inch an 8 pt font rounds to 11 units and the page drifts.
* Text alignment is left to GDI (`TA_CENTER` / `TA_RIGHT`) rather than
  computed here. This kit only ESTIMATES text widths, and estimating a
  centre would mis-place every centred label by a little; naming the
  anchor and the alignment lets the consumer use the real font metrics.

EMF has no arrowhead primitive, so each one is emitted as a filled
triangle over the end of its line.
"""

import struct

import vsdxkit as k

UPI = 2540                      # logical units per inch == 0.01 mm

# record types, from MS-EMF
EMR_HEADER = 1
EMR_POLYGON = 3
EMR_POLYLINE = 4
EMR_SETWINDOWEXTEX = 9
EMR_SETWINDOWORGEX = 10
EMR_SETVIEWPORTEXTEX = 11
EMR_SETVIEWPORTORGEX = 12
EMR_EOF = 14
EMR_SETMAPMODE = 17
EMR_SETBKMODE = 18
EMR_SETPOLYFILLMODE = 19
EMR_SETTEXTALIGN = 22
EMR_SETTEXTCOLOR = 24
EMR_SELECTOBJECT = 37
EMR_CREATEBRUSHINDIRECT = 39
EMR_ELLIPSE = 42
EMR_RECTANGLE = 43
EMR_ROUNDRECT = 44
EMR_EXTCREATEFONTINDIRECTW = 82
EMR_EXTTEXTOUTW = 84
EMR_EXTCREATEPEN = 95

STOCK_NULL_BRUSH = 0x80000005
STOCK_NULL_PEN = 0x80000008

MM_ANISOTROPIC = 8
TRANSPARENT = 1
WINDING = 2
GM_COMPATIBLE = 1

PS_GEOMETRIC = 0x00010000
PS_SOLID = 0x00000000
PS_USERSTYLE = 0x00000007
PS_ENDCAP_ROUND = 0x00000000
PS_JOIN_ROUND = 0x00000000
BS_SOLID = 0
BS_NULL = 1

TA_BASELINE = 24
TA_CENTER = 6
TA_RIGHT = 2
_ANCHOR_ALIGN = {"start": TA_BASELINE, "middle": TA_BASELINE | TA_CENTER,
                 "end": TA_BASELINE | TA_RIGHT}


def _colorref(hexstr):
    """0x00BBGGRR, which is not the order anyone expects."""
    h = hexstr.lstrip("#")
    return int(h[0:2], 16) | (int(h[2:4], 16) << 8) | (int(h[4:6], 16) << 16)


def _rec(itype, payload):
    """One record, padded to the 4-byte multiple nSize must be."""
    if len(payload) % 4:
        payload += b"\0" * (4 - len(payload) % 4)
    return struct.pack("<II", itype, 8 + len(payload)) + payload


def _rectl(x0, y0, x1, y1):
    return struct.pack("<4l", int(round(x0)), int(round(y0)),
                       int(round(x1)), int(round(y1)))


def _bounds(pts):
    xs = [q[0] for q in pts]
    ys = [q[1] for q in pts]
    return _rectl(min(xs), min(ys), max(xs), max(ys))


class _Emf:
    def __init__(self, w_in, h_in):
        self.w = int(round(w_in * UPI))
        self.h = int(round(h_in * UPI))
        self.recs = []
        self.objects = {}           # key -> handle
        self.next_handle = 1
        self.selected = {}          # slot -> handle, to skip redundant selects
        self.text_align = None
        self.text_color = None

    # -- units ---------------------------------------------------------
    def u(self, inches):
        return int(round(inches * UPI))

    # -- objects -------------------------------------------------------
    def _handle(self, key, build):
        if key in self.objects:
            return self.objects[key]
        h = self.next_handle
        self.next_handle += 1
        self.objects[key] = h
        self.recs.append(build(h))
        return h

    def pen(self, color, weight_in, dashed):
        width = max(self.u(weight_in), 1)
        key = ("pen", color, width, dashed)

        def build(h):
            if dashed:
                style = (PS_GEOMETRIC | PS_USERSTYLE | PS_ENDCAP_ROUND
                         | PS_JOIN_ROUND)
                entries = [self.u(6 / 96.0), self.u(4 / 96.0)]
            else:
                style = (PS_GEOMETRIC | PS_SOLID | PS_ENDCAP_ROUND
                         | PS_JOIN_ROUND)
                entries = []
            payload = struct.pack(
                "<IIIIIIIIIII", h, 0, 0, 0, 0, style, width, BS_SOLID,
                _colorref(color), 0, len(entries))
            payload += struct.pack("<%dI" % len(entries), *entries)
            return _rec(EMR_EXTCREATEPEN, payload)
        return self._handle(key, build)

    def brush(self, color):
        key = ("brush", color)

        def build(h):
            return _rec(EMR_CREATEBRUSHINDIRECT,
                        struct.pack("<IIII", h, BS_SOLID, _colorref(color), 0))
        return self._handle(key, build)

    def font(self, size_pt, bold, mono):
        key = ("font", round(size_pt, 2), bold, mono)

        def build(h):
            face = "Consolas" if mono else "Segoe UI"
            name = face.encode("utf-16-le")[:62].ljust(64, b"\0")
            lf = struct.pack(
                "<5l8B", -self.u(size_pt / 72.0), 0, 0, 0,
                700 if bold else 400, 0, 0, 0, 1, 0, 0, 0, 0) + name
            return _rec(EMR_EXTCREATEFONTINDIRECTW,
                        struct.pack("<I", h) + lf)
        return self._handle(key, build)

    def select(self, slot, handle):
        if self.selected.get(slot) == handle:
            return
        self.selected[slot] = handle
        self.recs.append(_rec(EMR_SELECTOBJECT, struct.pack("<I", handle)))

    # -- state ---------------------------------------------------------
    def set_text(self, color, align):
        if self.text_color != color:
            self.text_color = color
            self.recs.append(_rec(EMR_SETTEXTCOLOR,
                                  struct.pack("<I", _colorref(color))))
        if self.text_align != align:
            self.text_align = align
            self.recs.append(_rec(EMR_SETTEXTALIGN, struct.pack("<I", align)))

    # -- drawing -------------------------------------------------------
    def rect(self, x, y, w, h, rounding):
        box = _rectl(self.u(x), self.u(y), self.u(x + w), self.u(y + h))
        if rounding > 0:
            self.recs.append(_rec(EMR_ROUNDRECT, box + struct.pack(
                "<2l", self.u(2 * rounding), self.u(2 * rounding))))
        else:
            self.recs.append(_rec(EMR_RECTANGLE, box))

    def ellipse(self, cx, cy, rx, ry):
        self.recs.append(_rec(EMR_ELLIPSE, _rectl(
            self.u(cx - rx), self.u(cy - ry),
            self.u(cx + rx), self.u(cy + ry))))

    def poly(self, pts_in, closed):
        pts = [(self.u(a), self.u(b)) for a, b in pts_in]
        payload = (_bounds(pts) + struct.pack("<I", len(pts))
                   + b"".join(struct.pack("<2l", a, b) for a, b in pts))
        self.recs.append(_rec(EMR_POLYGON if closed else EMR_POLYLINE,
                              payload))

    def text(self, x, y, s, align):
        enc = s.encode("utf-16-le")
        ref = struct.pack("<2l", self.u(x), self.u(y))
        # every offset in this record counts from the record's own start
        off_string = 76
        emrtext = (ref + struct.pack("<III", len(s), off_string, 0)
                   + _rectl(0, 0, 0, 0) + struct.pack("<I", 0))
        payload = (_rectl(0, 0, -1, -1)                 # bounds: unused
                   + struct.pack("<I", GM_COMPATIBLE)
                   + struct.pack("<2f", 1.0, 1.0)
                   + emrtext + enc)
        self.recs.append(_rec(EMR_EXTTEXTOUTW, payload))

    # -- serialise -----------------------------------------------------
    def bytes(self):
        body = b"".join(self.recs)
        n_records = len(self.recs) + 2                  # + header + EOF
        eof = _rec(EMR_EOF, struct.pack("<III", 0, 16, 20))
        header_len = 88
        total = header_len + len(body) + len(eof)
        header = struct.pack("<II", EMR_HEADER, header_len)
        header += _rectl(0, 0, self.w - 1, self.h - 1)  # rclBounds, device
        header += _rectl(0, 0, self.w, self.h)          # rclFrame, .01 mm
        header += struct.pack("<II", 0x464D4520, 0x00010000)   # ' EMF', v1
        header += struct.pack("<II", total, n_records)
        header += struct.pack("<HH", self.next_handle, 0)
        header += struct.pack("<III", 0, 0, 0)          # no description
        header += struct.pack("<2l", self.w, self.h)    # szlDevice
        header += struct.pack("<2l", self.w // 100, self.h // 100)
        assert len(header) == header_len, len(header)
        return header + body + eof


def _arrow_head(a, b, size):
    """A filled triangle at b, pointing away from a. EMF has no arrows."""
    dx, dy = b[0] - a[0], b[1] - a[1]
    length = (dx * dx + dy * dy) ** 0.5
    if length == 0:
        return None
    ux, uy = dx / length, dy / length
    bx, by = b[0] - ux * size, b[1] - uy * size
    hw = size * 0.38
    return [b, (bx - uy * hw, by + ux * hw), (bx + uy * hw, by - ux * hw)]


def page_emf(page):
    """One page's shape list as EMF bytes."""
    e = _Emf(page.width, page.height)
    e.recs.append(_rec(EMR_SETMAPMODE, struct.pack("<I", MM_ANISOTROPIC)))
    e.recs.append(_rec(EMR_SETWINDOWORGEX, struct.pack("<2l", 0, 0)))
    e.recs.append(_rec(EMR_SETWINDOWEXTEX, struct.pack("<2l", e.w, e.h)))
    e.recs.append(_rec(EMR_SETVIEWPORTORGEX, struct.pack("<2l", 0, 0)))
    e.recs.append(_rec(EMR_SETVIEWPORTEXTEX, struct.pack("<2l", e.w, e.h)))
    e.recs.append(_rec(EMR_SETBKMODE, struct.pack("<I", TRANSPARENT)))
    e.recs.append(_rec(EMR_SETPOLYFILLMODE, struct.pack("<I", WINDING)))

    for s in page.shapes:
        if s.kind == "arrow":
            e.select("pen", e.pen(s.color, s.weight, s.dashed))
            e.select("brush", STOCK_NULL_BRUSH)
            e.poly(s.points, closed=False)
            size = max(s.weight * 8.0, 0.07)
            heads = []
            if s.end_arrow:
                heads.append(_arrow_head(s.points[-2], s.points[-1], size))
            if s.begin_arrow:
                heads.append(_arrow_head(s.points[1], s.points[0], size))
            for head in heads:
                if head:
                    # a solid head: the line's colour as both pen and fill
                    e.select("brush", e.brush(s.color))
                    e.poly(head, closed=True)
            continue

        st = s.st
        pen = (STOCK_NULL_PEN if st["no_line"]
               else e.pen(st["line"], st["line_weight"], st["dashed"]))
        brush = STOCK_NULL_BRUSH if st["no_fill"] else e.brush(st["fill"])

        paths = getattr(s, "paths", None)
        if paths is None:
            if not (st["no_fill"] and st["no_line"]):
                e.select("pen", pen)
                e.select("brush", brush)
                e.rect(s.x, s.y, s.w, s.h, st["rounding"])
        else:
            for path in paths:
                p_pen = pen if path.get("line", True) else STOCK_NULL_PEN
                p_brush = brush if path.get("fill", True) else STOCK_NULL_BRUSH
                e.select("pen", p_pen)
                e.select("brush", p_brush)
                if "ellipse" in path:
                    cx, cy, rx, ry = path["ellipse"]
                    e.ellipse(s.x + cx * s.w, s.y + cy * s.h,
                              rx * s.w, ry * s.h)
                else:
                    pts = [(s.x + a * s.w, s.y + b * s.h)
                           for a, b in path["pts"]]
                    e.poly(pts, closed=path.get("close", True))

        for r in k.text_runs(s):
            e.select("font", e.font(r["size"], r["bold"], r["mono"]))
            e.set_text(r["color"], _ANCHOR_ALIGN[r["anchor"]])
            e.text(r["x"], r["y"], r["text"], _ANCHOR_ALIGN[r["anchor"]])

    return e.bytes()


def save_emf(doc, prefix):
    """One .emf per page. A metafile holds a single picture, so a
    multi-page document becomes several files."""
    out = []
    for i, p in enumerate(doc.pages):
        path = ("%s.emf" % prefix if len(doc.pages) == 1
                else "%s-%d.emf" % (prefix, i + 1))
        with open(path, "wb") as fh:
            fh.write(page_emf(p))
        out.append(path)
    return out


# ---------------------------------------------------------------------
# validation — the file cannot be opened here, so it is parsed back
# ---------------------------------------------------------------------
def validate_emf(path):
    """Walk every record. Returns a list of problems; empty is good."""
    data = open(path, "rb").read()
    bad = []
    if len(data) < 88:
        return ["shorter than an EMF header"]

    itype, nsize = struct.unpack_from("<II", data, 0)
    if itype != EMR_HEADER:
        bad.append("first record is type %d, not EMR_HEADER" % itype)
    sig, ver = struct.unpack_from("<II", data, 40)
    if sig != 0x464D4520:
        bad.append("signature is 0x%08X, not ' EMF'" % sig)
    if ver != 0x00010000:
        bad.append("version is 0x%08X" % ver)
    nbytes, nrecords = struct.unpack_from("<II", data, 48)
    nhandles, = struct.unpack_from("<H", data, 56)
    if nbytes != len(data):
        bad.append("header says %d bytes, file is %d" % (nbytes, len(data)))

    # walk
    off, count, seen_eof, max_handle = 0, 0, False, 0
    while off < len(data):
        if off + 8 > len(data):
            bad.append("truncated record header at %d" % off)
            break
        t, size = struct.unpack_from("<II", data, off)
        if size < 8 or size % 4:
            bad.append("record %d at %d has nSize %d" % (count, off, size))
            break
        if off + size > len(data):
            bad.append("record %d at %d runs past the end" % (count, off))
            break
        if t in (EMR_EXTCREATEPEN, EMR_CREATEBRUSHINDIRECT,
                 EMR_EXTCREATEFONTINDIRECTW):
            h, = struct.unpack_from("<I", data, off + 8)
            max_handle = max(max_handle, h)
        if t == EMR_SELECTOBJECT:
            h, = struct.unpack_from("<I", data, off + 8)
            if not (h & 0x80000000) and h > max_handle:
                bad.append("record %d selects handle %d before it exists"
                           % (count, h))
        if t == EMR_EOF:
            seen_eof = True
            _, _, last = struct.unpack_from("<III", data, off + 8)
            if last != size:
                bad.append("EOF nSizeLast %d, record is %d" % (last, size))
            if off + size != len(data):
                bad.append("EOF is not the last record")
        count += 1
        off += size

    if count != nrecords:
        bad.append("header says %d records, walked %d" % (nrecords, count))
    if not seen_eof:
        bad.append("no EMR_EOF record")
    if nhandles <= max_handle:
        bad.append("nHandles %d does not exceed the top handle %d"
                   % (nhandles, max_handle))
    return bad


def describe_emf(path):
    """A one-line summary, and the record histogram behind it."""
    data = open(path, "rb").read()
    nbytes, nrecords = struct.unpack_from("<II", data, 48)
    fx0, fy0, fx1, fy1 = struct.unpack_from("<4l", data, 24)
    hist, off = {}, 0
    while off < len(data):
        t, size = struct.unpack_from("<II", data, off)
        if size < 8:
            break
        hist[t] = hist.get(t, 0) + 1
        off += size
    return dict(bytes=nbytes, records=nrecords,
                inches=((fx1 - fx0) / float(UPI), (fy1 - fy0) / float(UPI)),
                hist=hist)
