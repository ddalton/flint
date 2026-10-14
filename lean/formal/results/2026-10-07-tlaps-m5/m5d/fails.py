#!/usr/bin/env python3
"""Summarise a tlapm --toolbox run: failed / omitted obligations by line."""
import sys, re
txt = open(sys.argv[1]).read()
blocks = txt.split('@!!BEGIN')
fail = {}; omit = {}
for b in blocks:
    m = re.search(r'@!!status:(\w+)', b); l = re.search(r'@!!loc:(\d+):', b)
    if m and l:
        if m.group(1) == 'failed': fail[int(l.group(1))] = 1
        if m.group(1) == 'omitted': omit[int(l.group(1))] = 1
for line in txt.splitlines():
    if re.search(r'\[(ERROR|INFO)\]: .*obligations|^rc=|level|Error|error', line) and '@!!' not in line:
        print(line[:200])
print('failed lines:', sorted(fail)); print('omitted lines:', sorted(omit))
