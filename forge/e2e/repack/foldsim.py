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
                 base_cap_mode='eligible',
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
        # THE PACK CAP IS PART OF THE BASE GATE in the real planner
        # (`fold.rs`, `k.base_allowed && (k.cadence_open || ...)`), and this
        # model had NO cap term at all: `base_allowed()` was cadence and
        # waiver only. On the state M6 measured — 48 x 8 MiB plus tiny
        # pushes past the cap — the real planner answers ('base', 69) and
        # this model answered ('fold', 68), so the mechanism that produced
        # the wire's 1x/2x/3x re-uploads was invisible here. Every number
        # this file produced about the base rule before 2026-09-07 was
        # produced without it; see the tiers design doc S13.3.
        #
        #   base_cap_mode:
        #     'none'     — this file's old behaviour, kept only to
        #                  reproduce figures published before the fix
        #     'cap'      — forge as SHIPPED before `7202c2b5`: the cap
        #                  admits the base rule outright
        #     'eligible' — forge after `7202c2b5` (the default): the cap
        #                  admits it only when no fold could reduce the
        #                  count, i.e. fewer than two eligible tiers
        pre = sorted(self.tiers, key=lambda t: (t[1], t[0]))
        tb = sum(b for _, b in pre)
        cap_tripped = len(pre) >= max(self.cap, 2)
        ref = self.base if self.base > 0 else self.base_min
        eligible = [t for t in pre if t[1] < self.big * ref] if self.big is not None else list(pre)

        if self.base_cap_mode == 'none':
            gate = self.base_allowed()
        elif self.base_cap_mode == 'cap':
            gate = self.base_allowed() or cap_tripped
        else:
            gate = self.base_allowed() or (cap_tripped and len(eligible) < 2)

        if gate:
            if self.base == 0 and tb >= self.base_min and pre:
                return ('base', [i for i, _ in pre])
            if self.base > 0 and pre and tb * 100 >= self.base * self.percent:
                return ('base', [i for i, _ in pre])

        tiers = eligible
        n = len(tiers)
        if n < 2:
            return None
        s = split(tiers, self.factor)
        # `forced` reads the PRE-exemption count after the fix: without it
        # the cap declines the rebuild, the post-exemption count is under
        # the cap, the floor suppresses the fold, and nothing is planned.
        forced = n >= max(self.cap, 2) or (cap_tripped and self.base_cap_mode == 'eligible')
        if s < 2:
            if forced:
                if self.cap_mode == 'all':
                    return ('fold', [i for i, _ in tiers])
                h = (n + 1) // 2
                if self.base_cap_mode == 'eligible':
                    h = max(h, 2)   # a one-input fold is a no-op that re-plans forever
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
    ("K: A with floor 0 (KNOB-ONLY arm)", dict(persist=True, cap_mode='half', big=0.5, floor=0)),
    ("B: A + waive at 2x",               dict(persist=True, cap_mode='half', big=0.5, floor=256*MiB, waive=True, waive_x=2.0)),
    ("C: A + waive at 3x",               dict(persist=True, cap_mode='half', big=0.5, floor=256*MiB, waive=True, waive_x=3.0)),
    ("D: A + waive at 1x",               dict(persist=True, cap_mode='half', big=0.5, floor=256*MiB, waive=True, waive_x=1.0)),
    ("E: pct 100, no cadence, big .5, floor", dict(persist=True, cap_mode='half', big=0.5, floor=256*MiB, cadence=0, percent=100)),
    ("F: A with floor 128M",             dict(persist=True, cap_mode='half', big=0.5, floor=128*MiB, waive=True, waive_x=2.0)),
    ("G: A with floor 512M",             dict(persist=True, cap_mode='half', big=0.5, floor=512*MiB, waive=True, waive_x=2.0)),
    ("H: B with cap 32",                 dict(persist=True, cap_mode='half', big=0.5, floor=256*MiB, waive=True, waive_x=2.0, cap=32)),
]

def selfcheck():
    """The model's base gate against the two behaviours pinned in Rust.

    This file shipped for weeks with NO cap term in its base gate, so it
    could not express the mechanism that produced M6's 1x/2x/3x
    re-uploads on `runcg`. Worse than silent: with `base_cap_mode='none'`
    it answers `fold` on that state — the same answer the FIXED planner
    gives — so it agreed with a forge that did not exist yet and nothing
    looked wrong. Run `foldsim.py selfcheck` before trusting a base-rule
    number out of this file.
    """
    def state(mode):
        s = Sim(factor=2, floor=256*MiB, cap=64, cap_mode='half', big=0.5,
                base_min=64*MiB, percent=50, base_cap_mode=mode)
        s.tiers = [(i, 8*MiB) for i in range(48)] + [(100+i, 4*1024) for i in range(20)]
        s.base, s.last_base, s.now = 0, 0, 10      # cadence SHUT
        return s
    # `fold.rs`'s own tests pin these: pre-7202c2b5 the cap admitted the
    # base rule over every pack; after it, the cap forces a fold.
    want = {'cap': 'base', 'eligible': 'fold'}
    bad = 0
    for mode, expect in want.items():
        got = state(mode).plan()
        kind = got[0] if got else None
        ok = kind == expect
        bad += 0 if ok else 1
        print(f"  {'ok  ' if ok else 'FAIL'} base_cap_mode={mode:9s} -> {kind}, want {expect}")
    print("  (base_cap_mode='none' is the OLD, WRONG gate; kept only to reproduce"
          " figures published before the fix, e.g. the tiers doc S13.3)")
    return 1 if bad else 0


if __name__ == '__main__':
    which = sys.argv[1] if len(sys.argv) > 1 else 'runca'
    if which == 'selfcheck':
        sys.exit(selfcheck())
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
            # M6 sizing. The walgit rig's P2 leg is 32 pushers x 60 s at
            # ~15.5 pushes/s = ~930 tiny pushes; the question the drill
            # turns on is whether the SHIPPED rule (A) folds at all at
            # that size, or whether the 256 MiB floor swallows the leg
            # and the bytes score the floor rather than the ladder.
            'M6 P9-default 48x8MiB on 0 base':       (uniform(0, 48, 8*MiB, 1.0)),
            'M6 P9-default 48x8MiB on 1 GiB':        (uniform(GiB, 48, 8*MiB, 1.0)),
            'M6 P9-x4 192x8MiB on 1 GiB':            (uniform(GiB, 192, 8*MiB, 1.0)),
            'M6 P2-as-is 930x1KiB on 1 GiB, 15/s':   (uniform(GiB, 930, 1024, 0.065)),
            'M6 P2-10k-x-1KiB on 1 GiB, 15/s':       (uniform(GiB, 10000, 1024, 0.065)),
            'M6 P2-as-is 930x32KiB on 1 GiB, 15/s':  (uniform(GiB, 930, 32*1024, 0.065)),
            'M6 P2-fleet 10000x32KiB on 1 GiB, 15/s': (uniform(GiB, 10000, 32*1024, 0.065)),
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
