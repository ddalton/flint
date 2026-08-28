#!/usr/bin/env python3
"""Copy a keytab, flipping the KEY BYTES of every entry.

Same principal names, same enctypes, same kvnos -- only the key material
differs. That is precisely the "wrong service key" case: the server finds
a key for the principal the ticket names and simply cannot decrypt it.
Rotating the principal at the KDC would do the same thing, but it would
also invalidate the keytab every OTHER drill on this VM depends on.

keytab v2: u16 0x0502, then repeated { s32 size; entry[size] }, entry =
  u16 n_components, counted realm, n counted components, u32 name_type,
  u32 timestamp, u8 vno8, u16 enctype, counted key, [u32 vno32]
"""
import struct, sys

def counted(b, i):
    n = struct.unpack_from(">H", b, i)[0]
    return b[i + 2:i + 2 + n], i + 2 + n

src, dst = sys.argv[1], sys.argv[2]
b = bytearray(open(src, "rb").read())
assert b[:2] == b"\x05\x02", "not a keytab v2: %r" % bytes(b[:2])
i, flipped = 2, 0
while i + 4 <= len(b):
    size = struct.unpack_from(">i", b, i)[0]
    i += 4
    if size <= 0:                       # a hole
        i += -size
        continue
    j = i
    ncomp = struct.unpack_from(">H", b, j)[0]; j += 2
    _realm, j = counted(b, j)
    for _ in range(ncomp):
        _c, j = counted(b, j)
    j += 4 + 4 + 1                      # name_type, timestamp, vno8
    j += 2                              # enctype
    klen = struct.unpack_from(">H", b, j)[0]; j += 2
    for k in range(klen):               # flip every byte of the key
        b[j + k] ^= 0xFF
    flipped += 1
    i += size
open(dst, "wb").write(bytes(b))
print("flipped the key of %d entr%s" % (flipped, "y" if flipped == 1 else "ies"))
