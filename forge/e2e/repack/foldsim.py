#!/usr/bin/env python3
"""The fold planner over a clock — the simulator behind the tiers design's
§13 (docs/plans/forge-compaction-tiers-design.md).

`fold::plan` re-implemented in Python with a fold IN FLIGHT blocking the
next plan (85 MB/s for a tier fold, 40 MB/s for a base rebuild — the
rates the runca run showed), so pushes that land during a fold
accumulate as they did on the wire. The `runca` sequence is the
re-match's own push sequence, every push and its second from the
bucket listing and the legs' windows; `shapes` are the design's §3.6
shapes. Candidates are dicts of the planner's knobs.

    python3 foldsim.py runca
    python3 foldsim.py shapes

Bytes uploaded ÷ bytes pushed is the score; "maxpacks" is the most the
snapshot ever named, the price of a floor and of big packs that wait.
"""
import sys
MiB = 1 << 20
GiB = 1 << 30

def split(tiers, factor):
    n = len(tiers)
    if n < 2:
        return 0
    i = n - 1
    while i > 0:
        if tiers[i][1] < factor * tiers[i - 1][1]:
            break
        i -= 1
    s = i + 1 if i > 0 else 0
    total = sum(b for _, b in tiers[:s])
    while s < n and tiers[s][1] < factor * total:
        total += tiers[s][1]
        s += 1
    return s

class Sim:
    def __init__(self, factor=2, floor=0, cap=64, cap_mode='all', big=None,
                 cadence=3600, persist=True, waive=False, waive_x=1.0, percent=50, base_min=64*MiB,
                 tier_rate=85e6, base_rate=40e6, tick=5.0):
        self.__dict__.update(locals()); del self.__dict__['self']
        self.tiers = []   # (id, bytes)
        self.base = 0
        self.nid = 0
        self.up = 0; self.pushed = 0; self.largest = 0
        self.folds = 0; self.rebuilds = 0; self.maxpacks = 0
        self.inflight = None  # (done_time, kind, inputs_ids, bytes)
        self.last_base = None
        self.now = 0.0
        self.fold_bytes = 0; self.base_bytes_up = 0
        self.events = []
        self.worst_tier = 0.0

    def packs(self):
        return len(self.tiers) + (1 if self.base else 0)

    def base_allowed(self):
        if self.last_base is None:
            return True
        if self.waive and self.base > 0 and sum(b for _, b in self.tiers) >= self.waive_x * self.base:
            return True
        return self.now - self.last_base >= self.cadence

    def plan(self):
        tiers = sorted(self.tiers, key=lambda t: (t[1], t[0]))
        tb = sum(b for _, b in tiers)
        if self.base_allowed():
            if self.base == 0 and tb >= self.base_min and tiers:
                return ('base', [i for i, _ in tiers])
            if self.base > 0 and tiers and tb * 100 >= self.base * self.percent:
                return ('base', [i for i, _ in tiers])
        if self.big is not None:
            ref = self.base if self.base > 0 else self.base_min
            tiers = [t for t in tiers if t[1] < self.big * ref]
        n = len(tiers)
        if n < 2:
            return None
        s = split(tiers, self.factor)
        forced = n >= max(self.cap, 2)
        if s < 2:
            if forced:
                if self.cap_mode == 'all':
                    return ('fold', [i for i, _ in tiers])
                h = (n + 1) // 2
                return ('fold', [i for i, _ in tiers[:h]])
            return None
        total = sum(b for _, b in tiers[:s])
        if total < self.floor and not forced:
            return None
        return ('fold', [i for i, _ in tiers[:s]])

    def start(self, plan):
        kind, ids = plan
        if kind == 'base':
            nbytes = self.base + sum(b for i, b in self.tiers if i in ids)
            dur = nbytes / self.base_rate
        else:
            nbytes = sum(b for i, b in self.tiers if i in ids)
            dur = nbytes / self.tier_rate
        self.inflight = (self.now + dur, kind, set(ids), nbytes)

    def commit(self):
        done, kind, ids, nbytes = self.inflight
        self.inflight = None
        self.up += nbytes; self.largest = max(self.largest, nbytes)
        if kind == 'base':
            self.base = nbytes; self.tiers = [t for t in self.tiers if t[0] not in ids]
            self.rebuilds += 1; self.last_base = self.now; self.base_bytes_up += nbytes
            self.events.append((self.now, 'base', nbytes))
        else:
            self.tiers = [t for t in self.tiers if t[0] not in ids]
            self.nid += 1; self.tiers.append((self.nid, nbytes))
            self.folds += 1; self.fold_bytes += nbytes
            self.events.append((self.now, 'fold', nbytes))

    def maybe_plan(self):
        if self.inflight is not None:
            return
        p = self.plan()
        if p:
            self.start(p)

    def advance(self, t):
        # ticks and in-flight completion up to time t
        while True:
            nxt = self.now + self.tick
            if self.inflight and self.inflight[0] <= min(nxt, t):
                self.now = self.inflight[0]
                self.commit()
                self.maybe_plan()
                continue
            if nxt <= t:
                self.now = nxt
                self.maybe_plan()
                continue
            break
        self.now = t

    def push(self, t, size):
        self.advance(t)
        self.nid += 1; self.tiers.append((self.nid, size))
        self.pushed += size; self.up += size
        self.maxpacks = max(self.maxpacks, self.packs())
        tb = sum(b for _, b in self.tiers)
        self.worst_tier = max(self.worst_tier, tb / self.base if self.base else 0)
        self.maybe_plan()

    def restart(self, t):
        self.advance(t)
        if self.inflight:
            self.inflight = None  # the task died with the pod; nothing named
        if not self.persist:
            self.last_base = None
        self.maybe_plan()

def runca_sequence():
    """Times in seconds from 01:04:00 UTC, from the bucket listing and the legs."""
    ev = []
    t = 30
    for _ in range(5): ev.append((t, 1024)); t += 1            # killed attempt P1 tiny
    t = 60
    for _ in range(5): ev.append((t, 64*MiB)); t += 4          # killed attempt P1 64 MiB
    for m in ('01:06:08', '01:07:46', '01:08:25', '01:10:17', '01:11:04'):
        ev.append((hms(m), GiB))                                 # killed attempt P1 1 GiB
    ev.append((hms('01:11:13'), 1024))                           # run A P0
    t = hms('01:11:20')
    for _ in range(5): ev.append((t, 1024)); t += 3
    t = hms('01:11:45')
    for _ in range(5): ev.append((t, 64*MiB)); t += 5
    for m in ('01:12:34', '01:14:54', '01:15:34', '01:17:30', '01:18:12'):
        ev.append((hms(m), GiB))                                 # run A P1 1 GiB
    ev.append((hms('01:18:50'), 1024)); ev.append((hms('01:18:51'), 1024))   # P4
    t = hms('01:19:05')
    for _ in range(48): ev.append((t, 8*MiB)); t += 0.75        # P9
    t = hms('01:20:12')
    for _ in range(848): ev.append((t, 341)); t += 60/848       # P2
    ev.append((hms('01:22:50'), GiB))                            # P7 branch
    ev.append((hms('01:27:30'), 'restart'))                      # P5
    ev.append((hms('01:36:00'), 1024)); ev.append((hms('01:36:10'), 1024))  # P11
    ev.append((hms('01:40:30'), 1024))                           # P10 push after the cut
    t = hms('01:41:38')
    for _ in range(300): ev.append((t, 8*MiB)); t += 0.95       # B1 P9-300
    ev.append((hms('01:52:22'), GiB))                            # B2 P7 branch
    ev.append((hms('01:58:00'), 'end'))
    return sorted(ev, key=lambda e: e[0])

def hms(s):
    h, m, sec = s.split(':')
    return (int(h) - 1) * 3600 + int(m) * 60 + int(sec) - 4 * 60

def run(seq, **kw):
    sim = Sim(**kw)
    for t, x in seq:
        if x == 'restart': sim.restart(t)
        elif x == 'end': sim.advance(t)
        else: sim.push(t, x)
    return sim

def uniform(base, n, size, gap, start=0):
    seq = [(start + i * gap, size) for i in range(n)]
    seq.append((start + n * gap + 3600, 'end'))
    return seq, base

def report(name, sim):
    print(f"{name:34s} up={sim.up/1e9:6.1f} GB pushed={sim.pushed/1e9:5.1f} ratio={sim.up/sim.pushed:5.2f}x "
          f"folds={sim.folds:4d} ({sim.fold_bytes/1e9:5.1f} GB) rebuilds={sim.rebuilds} ({sim.base_bytes_up/1e9:5.1f} GB) "
          f"largest={sim.largest/1e9:5.1f} GB maxpacks={sim.maxpacks:4d} tiers_end={len(sim.tiers):3d} base_end={sim.base/1e9:5.1f} GB worst_tier/base={sim.worst_tier:5.1f}")

CANDS = [
    ("today, cadence lost at P5",        dict(persist=False)),
    ("1: cadence persisted",             dict(persist=True)),
    ("A: big .5 + floor 256M",           dict(persist=True, cap_mode='half', big=0.5, floor=256*MiB)),
    ("B: A + waive at 2x",               dict(persist=True, cap_mode='half', big=0.5, floor=256*MiB, waive=True, waive_x=2.0)),
    ("C: A + waive at 3x",               dict(persist=True, cap_mode='half', big=0.5, floor=256*MiB, waive=True, waive_x=3.0)),
    ("D: A + waive at 1x",               dict(persist=True, cap_mode='half', big=0.5, floor=256*MiB, waive=True, waive_x=1.0)),
    ("E: pct 100, no cadence, big .5, floor", dict(persist=True, cap_mode='half', big=0.5, floor=256*MiB, cadence=0, percent=100)),
    ("F: A with floor 128M",             dict(persist=True, cap_mode='half', big=0.5, floor=128*MiB, waive=True, waive_x=2.0)),
    ("G: A with floor 512M",             dict(persist=True, cap_mode='half', big=0.5, floor=512*MiB, waive=True, waive_x=2.0)),
    ("H: B with cap 32",                 dict(persist=True, cap_mode='half', big=0.5, floor=256*MiB, waive=True, waive_x=2.0, cap=32)),
]

if __name__ == '__main__':
    which = sys.argv[1] if len(sys.argv) > 1 else 'runca'
    if which == 'runca':
        print("=== runca: run A + B1 + B2, real times (fold 85 MB/s, base 40 MB/s) — measured 47.8 GB run A + ~14 GB B ===")
        seq = runca_sequence()
        for name, kw in CANDS:
            report(name, run(seq, **kw))
    else:
        shapes = {
            'P9-800 on 6 GiB, 1/s':        (uniform(6*GiB, 800, 8*MiB, 1.0)),
            'P9-2000 on 6 GiB, 1/s':       (uniform(6*GiB, 2000, 8*MiB, 1.0)),
            'fleet 10000x32KiB on 1 GiB, 10/s': (uniform(GiB, 10000, 32*1024, 0.1)),
            'RUN1 0 -> 875x8MiB, 1/s':     (uniform(0, 875, 8*MiB, 1.0)),
            '20x1GiB on 12 GiB, 1/min':    (uniform(12*GiB, 20, GiB, 60.0)),
            '20x1GiB on 0.2 GiB, 1/min':   (uniform(200*MiB, 20, GiB, 60.0)),
            'blob rig 512+100x2MiB, 1/s':  (uniform(512*MiB, 100, 2*MiB, 1.0)),
        }
        for sname, (seq, base) in shapes.items():
            print(f"=== {sname} ===")
            for name, kw in CANDS:
                if 'lost' in name: continue
                sim = Sim(**kw)
                sim.base = base
                if base: sim.last_base = -100000  # an old base
                for t, x in seq:
                    if x == 'end': sim.advance(t)
                    else: sim.push(t, x)
                report(name, sim)
