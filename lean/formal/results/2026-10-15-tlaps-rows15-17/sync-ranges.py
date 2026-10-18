#!/usr/bin/env python3
"""sync-ranges.py DIR -- the line range of every proof step that reads the sync's
parts (SyncW, RPullSyncW, SyncBl, SyncOwed, SyncTaken, SyncConv, SyncAll) or
expands `Sync`/`RPullSync` (a DEF list naming one) in each proof module of DIR:
the LEMMAs named after the two steps, and any other top-level LEMMA/THEOREM
whose text does.  (A lemma that only CITES Sync_X by name is not listed: it
expands nothing that changed.)  One "name first last MODULE"
line per range (final/lem.sh's ranges.txt form)."""
import os, re, sys
d = sys.argv[1]
PART = re.compile(r'\b(RPullSyncW|Sync(W|Bl|Owed|Taken|Conv|All))\b|DEF[^\n]*\b(Sync|RPullSync)\b')
HEAD = re.compile(r'^(LEMMA|THEOREM) (\w+)')
for f in sorted(os.listdir(d)):
    if not (f.startswith('LeanP1Proof') and f.endswith('.tla')):
        continue
    mod = f[:-4]
    L = open(os.path.join(d, f)).read().split('\n')
    heads = [(i, HEAD.match(l).group(2)) for i, l in enumerate(L) if HEAD.match(l)]
    for k, (i, name) in enumerate(heads):
        j = heads[k + 1][0] if k + 1 < len(heads) else len(L)
        # a range ends at the last non-blank line before the next head
        while j > i + 1 and not L[j - 1].strip():
            j -= 1
        body = '\n'.join(L[i:j])
        if name.startswith(('Sync_', 'RPullSync_')) or PART.search(body):
            print(f'{mod[11:]}-{name} {i + 1} {j} {mod}')
