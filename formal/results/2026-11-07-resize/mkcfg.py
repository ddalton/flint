#!/usr/bin/env python3
"""Write the FlintCatchupSR cfgs of the 2026-11-07 resize batch (gate item 7).

FULL is the proof's ASSUME Full (rev 17, the final setting).  SR adds the
resize knobs; RES_OFF is Resize = FALSE (rev 17 exactly), RES_ON the doc.

  python3 mkcfg.py            # writes cfg/*.cfg next to this script
Identity cfgs ("Id_*") are written for FlintCatchupS.tla and FlintCatchupSR.tla
(the S copy omits the rev 18 constants), so both modules check the same space.
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
RES_ON = dict(Resize=True, SizeGuard=True, Reserve=True, FinishAtCreate=True,
              FsAfterCtl=True, MoveBeforeAck=True, RetryRuns=True, MoveCrash=True)
RES_OFF = dict(RES_ON, Resize=False)
SHAPES = {
    'lost': dict(Crash=False, ZombieMoves=False),
    'crash': dict(Crash=True, ZombieMoves=False),
    'zombie': dict(Crash=False, ZombieMoves=True),
}
SAFE = ['Inv_NoNewLoss', 'Inv_MembersHold', 'Inv_RaidHolds', 'Inv_AckStamped']
RINV = ['Inv_RTypes', 'Inv_ArrayLeBases', 'Inv_FsFits', 'Inv_FsRangeClean', 'Inv_VacatedZero',
        'Inv_ShortNeverServes', 'Inv_MoveFinished', 'Inv_NoWipe', 'Inv_FsLegsFit']

def sizes(n, fails, maxver=1, maxepoch=2, retain=1, maxasm=3):
    reps = ', '.join(str(i) for i in range(1, n + 1))
    return dict(Reps='{%s}' % reps, Blocks='{b1}', MaxVer=maxver, MaxEpoch=maxepoch,
                MaxFails=fails, Retain=retain, MaxAsm=maxasm)

def val(v):
    return ('TRUE' if v else 'FALSE') if isinstance(v, bool) else str(v)

ARMS = []
def arm(name, what, expect, shape, sz, inv, res=RES_ON, over=None, spec='FlintCatchupSR.tla', extra=''):
    c = dict(FULL); c.update(SHAPES[shape])
    if spec == 'FlintCatchupSR.tla':
        c.update(res)
    c.update(over or {})
    lines = [f'\\* {what}', f'\\* expect: {expect}', f'\\* spec: {spec}', 'SPECIFICATION Spec', 'CONSTANTS']
    lines += [f'  {k} = {val(v)}' for k, v in sz.items()]
    lines += [f'  {k} = {val(v)}' for k, v in sorted(c.items())]
    lines += [f'INVARIANT {" ".join(inv)}', 'CHECK_DEADLOCK FALSE']
    if extra: lines.append(extra)
    ARMS.append((name, '\n'.join(lines) + '\n'))

# the rev 17 identity: the same cfg on S and on SR with Resize = FALSE
for sh in ('lost', 'crash', 'zombie'):
    for (mv, me, ma) in ((1, 2, 3), (2, 2, 3)):
        sz = sizes(2, 1, mv, me, 1, ma)
        arm(f'IdS_{sh}_v{mv}e{me}a{ma}', f'identity: rev 17 on S, {sh}, 2 replicas', 'pass', sh, sz, SAFE, spec='FlintCatchupS.tla')
        arm(f'IdSR_{sh}_v{mv}e{me}a{ma}', f'identity: SR with Resize = FALSE, {sh}, 2 replicas', 'pass', sh, sz, SAFE, res=RES_OFF)

# Resize on: the safety invariants and the resize ones
for n, fails, ma in ((2, 1, 3), (2, 1, 4)):
    for sh in ('lost', 'crash', 'zombie'):
        arm(f'Res_{sh}_r{n}f{fails}a{ma}', f'Resize on, {sh}, {n} replicas, {fails} failures, MaxVer 1 MaxEpoch 2 MaxAsm {ma}',
            'pass', sh, sizes(n, fails, 1, 2, 1, ma), SAFE + RINV)
for sh in ('lost', 'crash', 'zombie'):
    arm(f'Res3_{sh}_r3f1a3', f'Resize on, {sh}, 3 replicas, 1 failure, MaxVer 1 MaxEpoch 2 MaxAsm 3 (bounded)',
        'pass', sh, sizes(3, 1, 1, 2, 1, 3), SAFE + RINV)

# planted bugs: each must fail its named invariant
BUGS = {
    'NoReserve': (dict(Reserve=False), 'Inv_ArrayLeBases', 'raid1_resize does not take the reserve off'),
    'NoFinish': (dict(FinishAtCreate=False), 'Inv_MoveFinished', 'bdev_raid_create does not finish a move with prev set'),
    'ShortAdmitted': (dict(SizeGuard=False), 'Inv_ShortNeverServes', 'no leg-size guard: a short leg is admitted'),
    'FsEarly': (dict(FsAfterCtl=False), 'Inv_FsRangeClean or Inv_FsLegsFit', 'the fs is grown before every leg grew'),
}
for k, (over, inv, what) in BUGS.items():
    arm(f'Bug_{k}', f'planted bug: {what}', inv, 'lost', sizes(2, 1, 1, 2, 1, 3), SAFE + RINV, over=over)

# design arms: the ordering the doc says the safety rests on
arm('Design_NoRetryRun', 'design: the retry of a crashed grow is a no-op success (same-size resize)', 'Inv_VacatedZero or Inv_FsRangeClean or pass',
    'lost', sizes(2, 1, 1, 2, 1, 3), SAFE + RINV, over=dict(RetryRuns=False))
arm('Design_AckAfterGrow', 'design: a leg counts as grown at the lvol resize, before its move', 'Inv_NoWipe or Inv_FsRangeClean or pass',
    'lost', sizes(2, 1, 1, 2, 1, 3), SAFE + RINV, over=dict(MoveBeforeAck=False))

if __name__ == '__main__':
    out = os.path.join(os.path.dirname(os.path.abspath(__file__)), 'cfg')
    os.makedirs(out, exist_ok=True)
    for name, text in ARMS:
        open(os.path.join(out, name + '.cfg'), 'w').write(text)
    print(len(ARMS), 'cfgs in', out)
