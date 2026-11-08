#!/usr/bin/env python3
"""Write the FlintCatchupS cfgs of the 2026-11-07 cluster batch.

FULL is the proof's ASSUME Full (FlintCatchupSProof.tla, 9ed5580a): rev 17,
the final setting.  REV12 turns rev 13-17's knobs off, which gives rev 12's
behaviour (each knob FALSE = the revision before it).  An arm is a base, the
fault shape, the sizes and its overrides; each cfg says so in its header.

  python3 mkcfg.py            # writes cfg/*.cfg next to this script
"""
import os

FULL = dict(
    Policy='"trust"', Capture=False, Crash=True, ZombieMoves=True,
    ZombieWhere='"colocated"', RaidFailsAtDrop=True, StageOracle=False,
    WriteQuorum=1, AssemblyQuorum=1, BaseInclusive=True,
    CutStampsAll=False, PinRetention=True,
    Backoff=True, CoverageWalk=True, MarkGate=True, ReadFails=False, LocalLegGuard=True,
    FenceMark=False, CatchupWaitsCut=False, AssemblyMark=True, StageDefer=True, AckAfterMark=False,
    Stamp=True, RecordUp=True, MajorityRead='"count"', Decree=True, DeferExpires=False,
    MonitorSkipsHold=False, DecreeGated=True, DecreeSparesLast=True, RelView=True,
    EvenVouch='"decree"', TornExcluded=True, TornNeverSole=True, ZombiePreStampLegal=True,
    LastFails=True, WholeMark=True, InWriteGuard=True, MonitorGuard=True,
    OperatorMark=True, DrainStamps=True, ZombieTriple=True,
)
REV12 = dict(FULL, LastFails=False, WholeMark=False, InWriteGuard=False, MonitorGuard=False,
             OperatorMark=False, DrainStamps=False, ZombieTriple=False)

# the fault shapes: one fault kind per arm, as the §12 rows name them
SHAPES = {
    'lost': dict(Crash=False, ZombieMoves=False),   # the lost mark (no capture)
    'crash': dict(Crash=True, ZombieMoves=False),
    'zombie': dict(Crash=False, ZombieMoves=True),
}

def sizes(n, fails, maxver=2, maxepoch=2, retain=1, maxasm=3, ids='nat'):
    reps = ', '.join(str(i) for i in range(1, n + 1)) if ids == 'nat' else \
           ', '.join(f'r{i}' for i in range(1, n + 1))
    return dict(Reps='{%s}' % reps, Blocks='{b1}', MaxVer=maxver, MaxEpoch=maxepoch,
                MaxFails=fails, Retain=retain, MaxAsm=maxasm)

def val(v):
    return ('TRUE' if v else 'FALSE') if isinstance(v, bool) else str(v)

def cfg(name, what, expect, base, shape, sz, inv, over=None, spec='FlintCatchupS.tla',
        extra=''):
    c = dict(base)
    c.update(SHAPES[shape])
    c.update(over or {})
    lines = [f'\\* {what}', f'\\* expect: {expect}', f'\\* spec: {spec}',
             'SPECIFICATION Spec', 'CONSTANTS']
    lines += [f'  {k} = {val(v)}' for k, v in sz.items()]
    lines += [f'  {k} = {val(v)}' for k, v in sorted(c.items())]
    lines += [f'INVARIANT {" ".join(inv)}', 'CHECK_DEADLOCK FALSE']
    if extra:
        lines.append(extra)
    return name, '\n'.join(lines) + '\n'

SAFE = ['Inv_NoNewLoss', 'Inv_MembersHold', 'Inv_RaidHolds', 'Inv_AckStamped']

ARMS = []
def arm(*a, **k):
    ARMS.append(cfg(*a, **k))

# calibration: rev 12's two-replica spaces (§12 c28: lost 3,627,109; crash
# 5,326,386; zombie 29,140,655), to find the sizes the batches used
for mv in (1, 2):
    for me in (1, 2):
        for ma in (2, 3):
            for sh in ('lost', 'crash', 'zombie'):
                arm(f'Cal_{sh}_v{mv}e{me}a{ma}', f'calibration: rev 12 knobs, {sh}, 2 replicas, 1 failure, MaxVer {mv} MaxEpoch {me} MaxAsm {ma}',
                    'pass', REV12, sh, sizes(2, 1, mv, me, 1, ma), SAFE)

# round 2: MaxVer 1, MaxEpoch 2, more assemblies, one or two failures
for mf in (1, 2):
    for ma in (4, 5, 6):
        for sh in ('lost', 'crash'):
            arm(f'Cal2_{sh}_f{mf}a{ma}', f'calibration: rev 12 knobs, {sh}, 2 replicas, {mf} failures, MaxVer 1 MaxEpoch 2 MaxAsm {ma}',
                'pass', REV12, sh, sizes(2, mf, 1, 2, 1, ma), SAFE)

# item 8, the monitor-skips question: the final setting with the ungated decree
# and the monitor skipping held legs, against the same with the monitor
# marking (the control: the third gap's strand) and the final setting itself.
# The pre-stamp write is legal in all three (rev 10 on, Full).
SHAPES['full'] = dict(Crash=True, ZombieMoves=True)
M8 = {
    'Skip': ('the ungated decree, the monitor skipping held legs', dict(DecreeGated=False, MonitorSkipsHold=True)),
    'Ctl': ('control: the ungated decree, the monitor marking', dict(DecreeGated=False, MonitorSkipsHold=False)),
    'Both': ('the gated decree and the monitor skipping held legs', dict(DecreeGated=True, MonitorSkipsHold=True)),
    'Final': ('the final setting (the gated decree, the monitor marking)', {}),
    # without the drain (rev 15's OperatorMark): in both depth-11 traces of
    # the two arms above, the drain takes the raid's second leg
    'SkipNoDrain': ('the ungated decree, the monitor skipping held legs, no drain', dict(DecreeGated=False, MonitorSkipsHold=True, OperatorMark=False, DrainStamps=False)),
    'CtlNoDrain': ('control: the ungated decree, the monitor marking, no drain', dict(DecreeGated=False, MonitorSkipsHold=False, OperatorMark=False, DrainStamps=False)),
}
for n, fails in ((2, 1), (3, 2)):
    for ma in (3, 4):
        for sh in ('zombie', 'full'):
            for k, (what, over) in M8.items():
                arm(f'M8_{k}_{sh}_r{n}f{fails}a{ma}', f'item 8, {what}; rev 17 Full, {sh} shape, {n} replicas, {fails} failures, MaxVer 1 MaxEpoch 2 MaxAsm {ma}',
                    'pass', FULL, sh, sizes(n, fails, 1, 2, 1, ma), SAFE, over=over)

# item 3, symmetry over the replica ids (FlintCatchupSSym.tla): model-value
# ids without SYMMETRY must reproduce the natural-id space exactly; with it,
# each arm keeps its verdict; Inv_ReadUnique says the read is blind to ids
SYM_INV = SAFE + ['Inv_ReadUnique']
for n, fails in ((2, 1), (3, 2)):
    for sh in ('lost', 'crash', 'zombie', 'full'):
        for sym in (False, True):
            tag = 'Sym' if sym else 'Ids'
            arm(f'S3_{tag}_{sh}_r{n}f{fails}a3',
                f'item 3, {"SYMMETRY Perms" if sym else "model-value ids, no symmetry"}; rev 17 Full, {sh} shape, {n} replicas, {fails} failures, MaxVer 1 MaxEpoch 2 MaxAsm 3',
                'pass', FULL, sh, sizes(n, fails, 1, 2, 1, 3, ids='mv'), SYM_INV,
                spec='FlintCatchupSSym.tla', extra='SYMMETRY Perms' if sym else '')
    # c29's first form, too strict: expected to fail where two maximal views
    # differ only in the released view
    arm(f'S3_ViewUnique_zombie_r{n}f{fails}a3', f'item 3, c29\'s first form (the newest view unique); rev 17 Full, zombie, {n} replicas',
        'Inv_ViewUnique or pass', FULL, 'zombie', sizes(n, fails, 1, 2, 1, 3, ids='mv'), ['Inv_ViewUnique'],
        spec='FlintCatchupSSym.tla')

# the VIEW without the write-only ghosts (FlintCatchupSSymV.tla), measured at
# two replicas against the S3_Sym / S3_Ids counts: the verdicts must agree
for sh in ('lost', 'crash', 'zombie', 'full'):
    for sym in (False, True):
        tag = 'SymV' if sym else 'IdsV'
        arm(f'S3_{tag}_{sh}_r2f1a3',
            f'item 3, VIEW ViewVars{" and SYMMETRY Perms" if sym else ""}; rev 17 Full, {sh} shape, 2 replicas, 1 failure, MaxVer 1 MaxEpoch 2 MaxAsm 3',
            'pass', FULL, sh, sizes(2, 1, 1, 2, 1, 3, ids='mv'), SYM_INV,
            spec='FlintCatchupSSymV.tla', extra=('SYMMETRY Perms\n' if sym else '') + 'VIEW ViewVars')

# the zombie-shape runs at three and four replicas take the VIEW too
arm('S3_SymV_zombie_r3f2a3', 'item 3, VIEW ViewVars and SYMMETRY Perms; rev 17 Full, zombie shape, 3 replicas, 2 failures, MaxVer 1 MaxEpoch 2 MaxAsm 3',
    'pass', FULL, 'zombie', sizes(3, 2, 1, 2, 1, 3, ids='mv'), SYM_INV,
    spec='FlintCatchupSSymV.tla', extra='SYMMETRY Perms\nVIEW ViewVars')

# bounded three- and four-replica safety, deeper than c31/c33 reached on EC2:
# the expiry form (the code's 180 s bound: loss allowed only when marked),
# and four replicas at three failures under SYMMETRY
arm('B_Expiry_zombie_r3f2a3', 'bounded: the expiry form (DeferExpires); rev 17 Full, zombie, 3 replicas, 2 failures, SYMMETRY and VIEW',
    'pass', FULL, 'zombie', sizes(3, 2, 1, 2, 1, 3, ids='mv'),
    ['Inv_LossMarked', 'Inv_TornJunkMarked', 'Inv_AckStamped', 'Inv_ReadUnique'], over=dict(DeferExpires=True),
    spec='FlintCatchupSSymV.tla', extra='SYMMETRY Perms\nVIEW ViewVars')
arm('B_Four_zombie_r4f3a3', 'bounded: four replicas, three failures; rev 17 Full, zombie, SYMMETRY and VIEW',
    'pass', FULL, 'zombie', sizes(4, 3, 1, 2, 1, 3, ids='mv'), SYM_INV,
    spec='FlintCatchupSSymV.tla', extra='SYMMETRY Perms\nVIEW ViewVars')

if __name__ == '__main__':
    out = os.path.join(os.path.dirname(os.path.abspath(__file__)), 'cfg')
    os.makedirs(out, exist_ok=True)
    for name, text in ARMS:
        with open(os.path.join(out, name + '.cfg'), 'w') as f:
            f.write(text)
    print(len(ARMS), 'cfgs in', out)
