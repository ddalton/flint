#!/usr/bin/env python3
"""fill.py <doc.md> <tables-output.txt> : replace each TABLE-<NAME> placeholder line in the doc with the block that
tables.py printed under <<TABLE-NAME>>. Refuses if a placeholder has no block or a block has no placeholder."""
import re, sys
doc, out = sys.argv[1], sys.argv[2]
s = open(doc).read(); t = open(out).read()
blocks = {}
for m in re.finditer(r'^<<(TABLE-[A-Z]+)>>\n(.*?)(?=^<<TABLE-|\Z)', t, re.S | re.M):
    blocks[m.group(1)] = m.group(2).strip('\n')
holes = set(re.findall(r'^(TABLE-[A-Z]+)$', s, re.M))
missing = holes - blocks.keys(); extra = blocks.keys() - holes
if missing or extra:
    sys.exit(f"placeholders without a block: {sorted(missing)}; blocks without a placeholder: {sorted(extra)}")
for k, v in blocks.items():
    s = re.sub(r'^' + k + r'$', lambda m: v, s, count=1, flags=re.M)
open(doc, 'w').write(s)
print(f"filled {sorted(blocks)}")
