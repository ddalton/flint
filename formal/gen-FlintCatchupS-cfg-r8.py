import os, re
D = os.path.expanduser('~/github/flint-rebuild-proof/formal')
RULE = '\\* The design: spdk-csi-driver/docs/incremental-replica-rebuild.md rev 8, §12 "Record-before-ack: the membership stamp".'
BASE = dict(Reps='{1, 2, 3}', Blocks='{b1}', MaxVer='1', MaxEpoch='2', MaxFails='2', Retain='1',
            Backoff='TRUE', BaseInclusive='TRUE', PinRetention='TRUE', CoverageWalk='TRUE', Policy='"trust"',
            ZombieWhere='"colocated"', StageOracle='FALSE', Capture='FALSE', Crash='FALSE', ZombieMoves='FALSE',
            MarkGate='TRUE', RaidFailsAtDrop='TRUE', ReadFails='FALSE', LocalLegGuard='TRUE', FenceMark='FALSE',
            CatchupWaitsCut='FALSE', AssemblyMark='TRUE', StageDefer='TRUE', WriteQuorum='1', AssemblyQuorum='1',
            AckAfterMark='FALSE', Stamp='TRUE', MaxAsm='3', RecordUp='TRUE', MajorityRead='"count"', Decree='TRUE', DeferExpires='FALSE', MonitorSkipsHold='FALSE', DecreeGated='TRUE', DecreeSparesLast='TRUE', RelView='TRUE',
            CutStampsAll='FALSE')
NNL3 = ['Inv_NoNewLoss', 'Inv_MembersHold', 'Inv_RaidHolds']
SC = dict(Crash='TRUE'); SZ = dict(ZombieMoves='TRUE'); SL = {}
def cfg(name, head, over={}, invs=NNL3, props=()):
    k = dict(BASE); k.update(over)
    bound = f"bound {'D' if k['MaxFails']=='2' else 'D, MaxFails ' + k['MaxFails']} (three replicas, one write, two epochs, one retained)"
    lines = [f'\\* FlintCatchupS, {bound}.',
             '\\* Code-faithful otherwise (no wait for a cut, the assembly mark, the FlintLocalLeg guard, F36c defer, NO capture),',
             '\\* trust, the mark-race fix (CutStampsAll FALSE).'] + [f'\\* {h}' for h in head] + [RULE, 'SPECIFICATION Spec', 'CONSTANTS']
    lines += [f'  {a} = {b}' for a, b in k.items()]
    lines += [f'INVARIANT {i}' for i in invs] + [f'PROPERTY {p}' for p in props] + ['CHECK_DEADLOCK FALSE']
    open(f'{D}/FlintCatchup{name}.cfg', 'w').write('\n'.join(lines) + '\n'); print(name)
for S, ov, what in (('Z', SZ, 'a zombie move (gated marks)'), ('C', SC, 'a consumer crash'), ('L', SL, 'the lost mark (no crash, no zombie)')):
    cfg(f'S{S}', [f'Arm 1: the stamp (majority release, minority record gate, MajorityRead, RelView, Decree), {what}:',
                  'Inv_NoNewLoss, Inv_MembersHold, Inv_RaidHolds.  Expect HOLD (flint-cb, §12).'], ov)
    cfg(f'S{S}NoRV', [f'Control for S{S}: RelView FALSE (a stamp carries only its own generation; design rev 8 as written),',
                      f'{what}.  Expect VIOL (an unconfirmed minority stamp hides the last released membership).'],
        dict(ov, RelView='FALSE'))
    cfg(f'S{S}0', [f'Control for S{S}: Stamp = FALSE (FlintCatchupT, no option (b)), {what}: the d9 residual.  Expect VIOL.'],
        dict(ov, Stamp='FALSE'))
    cfg(f'S{S}Mem', [f'Arm 1 with the stricter read (MajorityRead "member": attached legs the view names), {what}.'],
        dict(ov, MajorityRead='"member"'))
    cfg(f'S{S}F3', [f'Gap: MaxFails 3, the stamp with MajorityRead, {what}.  Expect HOLD.'], dict(ov, MaxFails='3'))
    cfg(f'S{S}F3NoMR', [f'Gap control: MaxFails 3, the stamp WITHOUT MajorityRead (design rev 8 step 2 as written), {what}.',
                        'Expect VIOL (the stage serves a leg whose stamp predates an unreached majority stamp).'],
        dict(ov, MaxFails='3', MajorityRead='"off"'))
cfg('SZAck', ['Arm 2: the zombie never acknowledges (Inv_ZombieNoAck).  A confirmed sole continuation on its own leg',
              'is the one case the design lets it ack: a violation is checked against that case.'], SZ, ['Inv_ZombieNoAck'])
cfg('SZFree', ['Arm 2: a leg hygiene frees during the defer is stale (Inv_FreedStale), the decree on.'], SZ, ['Inv_FreedStale'])
cfg('SZClean', ['Arm 2: cleanliness, no member holds a zombie\'s unacknowledged write (Inv_MembersClean), the decree on.'], SZ, ['Inv_MembersClean'])
cfg('SZLate', ['Arm 2, flint-cb\'s line: once an assembly after the move stamped WITHOUT z, no member ever holds the',
               'zombie\'s unacknowledged writes (Inv_NoLateJunk).  Junk on z at the FIRST assembly is the write-hole class.'],
    SZ, ['Inv_NoLateJunk'])
cfg('SZLate2', ['Arm 2, flint-cb\'s line (rev 4): no member ever holds a zombie write the first assembly after the move',
                'did not take in (Inv_NoLateJunk2): junk entering later, or z rejoining dirty after it was excluded.'],
    SZ, ['Inv_NoLateJunk2'])
cfg('SZDecreeAll', ['Control for SZ: step 4 as written (DecreeGated FALSE, DecreeSparesLast FALSE): the decree may mark',
                    'the last in_sync leg stale.  Expect VIOL (Inv_NoNewLoss, d8: no in_sync leg left).'],
    dict(SZ, DecreeGated='FALSE', DecreeSparesLast='FALSE'))
cfg('SZDecreeUngated', ['Control for SZ: DecreeGated FALSE (the decree runs at any stage attempt; a stale-by-decree z counts',
                        'for the record\'s vouch).  Expect VIOL (Inv_NoNewLoss, d13: the stage serves a leg z was newer than).'],
    dict(SZ, DecreeGated='FALSE'))
cfg('SZDecreeSkip', ['flint-cb\'s alternative root fix: the decree ungated (DecreeGated FALSE) but the monitor marks nothing',
                     'while a membership change is pending (MonitorSkipsHold TRUE).  Record only.'],
    dict(SZ, DecreeGated='FALSE', MonitorSkipsHold='TRUE'))
cfg('SZSoleOnly', ['flint-cb: a confirmed sole continuation on q leaves the record with q alone in_sync (Inv_SoleOnly).'],
    SZ, ['Inv_SoleOnly'])
cfg('SLSoleOnly', ['flint-cb: a confirmed sole continuation on q leaves the record with q alone in_sync (Inv_SoleOnly).'],
    SL, ['Inv_SoleOnly'])
for S2, ov2, what2 in (('Z', SZ, 'a zombie move'), ('L', SL, 'the lost mark'), ('C', SC, 'a consumer crash')):
    cfg(f'S{S2}Risk', [f'The defer\'s bound EXPIRES (DeferExpires: ServeWithRisk on an inconclusive read), {what2}:',
                       'Inv_NoNewLoss, the measurement the user asked for (expected to fail: the loss the bound allows).'],
        dict(ov2, DeferExpires='TRUE'), ['Inv_NoNewLoss'])
    cfg(f'S{S2}RiskMarked', [f'The defer\'s bound expires, {what2}: Inv_LossMarked, no acked write absent from every in_sync leg',
                             'while AckedTailRisk is unset (the bound never loses silently).  Expect HOLD.'],
        dict(ov2, DeferExpires='TRUE'), ['Inv_LossMarked'])
cfg('SZClean0', ['Control for SZClean: the decree OFF.'], dict(SZ, Decree='FALSE'), ['Inv_MembersClean'])
cfg('S31', ['Arm 3: three to one with the record UNWRITABLE (RecordUp FALSE), no crash, no zombie: writes fail, nothing lost.',
            'Inv_NoNewLoss, Inv_MembersHold, Inv_RaidHolds, Inv_AckStamped.  Expect HOLD.'], dict(RecordUp='FALSE'),
    NNL3 + ['Inv_AckStamped'])
cfg('SAck', ['Every ack with every member stamped (Inv_AckStamped), the lost-mark arm.  Expect HOLD.'], {}, ['Inv_AckStamped'])
cfg('SLive', ['Arm 4: a barrier ends (Prop_BarrierEnds), RecordUp: expect HOLD.'], {}, ['TypeOK'], ['Prop_BarrierEnds'])
cfg('SLiveNoRec', ['Arm 4 control: RecordUp FALSE, a minority barrier never ends: expect a LASSO.'], dict(RecordUp='FALSE'),
    ['TypeOK'], ['Prop_BarrierEnds'])
