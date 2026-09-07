#!/usr/bin/env python3
"""Small files for the C7 rig, so the repository has OBJECTS and not
only bytes: the restore's transfer is proportional to the bytes and its
proof is proportional to the objects, and a rig with five huge blobs
measures the first while reporting the second as free."""
import os
import random
import sys

d, n = sys.argv[1], int(sys.argv[2])
os.makedirs(d, exist_ok=True)
r = random.Random(hash(d) & 0xFFFF ^ n)
for k in range(n):
    with open(f"{d}/f{k:05d}.txt", "w") as f:
        f.write("%032x\n" % r.getrandbits(128))
