import os, re
D = os.path.expanduser('~/github/flint-rebuild-proof/formal')
RULE = '\\* The design: spdk-csi-driver/docs/incremental-replica-rebuild.md rev 8, §12 "Record-before-ack: the membership stamp".'
BASE = dict(Reps='{1, 2, 3}', Blocks='{b1}', MaxVer='1', MaxEpoch='2', MaxFails='2', Retain='1',
            Backoff='TRUE', BaseInclusive='TRUE', PinRetention='TRUE', CoverageWalk='TRUE', Policy='"trust"',
            ZombieWhere='"colocated"', StageOracle='FALSE', Capture='FALSE', Crash='FALSE', ZombieMoves='FALSE',
            MarkGate='TRUE', RaidFailsAtDrop='TRUE', ReadFails='FALSE', LocalLegGuard='TRUE', FenceMark='FALSE',
            CatchupWaitsCut='FALSE', AssemblyMark='TRUE', StageDefer='TRUE', WriteQuorum='1', AssemblyQuorum='1',
            AckAfterMark='FALSE', Stamp='TRUE', MaxAsm='3', RecordUp='TRUE', MajorityRead='"count"', Decree='TRUE', DeferExpires='FALSE', MonitorSkipsHold='FALSE', DecreeGated='TRUE', DecreeSparesLast='TRUE', RelView='TRUE', EvenVouch='"off"', TornExcluded='FALSE', TornNeverSole='FALSE', ZombiePreStampLegal='FALSE',
            CutStampsAll='FALSE')
NNL3 = ['Inv_NoNewLoss', 'Inv_MembersHold', 'Inv_RaidHolds']
SC = dict(Crash='TRUE'); SZ = dict(ZombieMoves='TRUE'); SL = {}
def cfg(name, head, over={}, invs=NNL3, props=(), spec=''):
    k = dict(BASE); k.update(over)
    nrep = {2: 'two', 3: 'three', 4: 'four'}[k['Reps'].count(',') + 1]
    bound = f"bound {'D' if k['MaxFails']=='2' else 'D, MaxFails ' + k['MaxFails']} ({nrep} replicas, one write, two epochs, one retained)"
    lines = [f'\\* FlintCatchupS, {bound}.',
             '\\* Code-faithful otherwise (no wait for a cut, the assembly mark, the FlintLocalLeg guard, F36c defer, NO capture),',
             '\\* trust, the mark-race fix (CutStampsAll FALSE).'] + [f'\\* {h}' for h in head] + [RULE, 'SPECIFICATION Spec', 'CONSTANTS']
    lines += [f'  {a} = {b}' for a, b in k.items()]
    lines += [f'INVARIANT {i}' for i in invs] + [f'PROPERTY {p}' for p in props] + ['CHECK_DEADLOCK FALSE']
    if spec: lines[0] = f'\\* {spec}; ' + lines[0][3:]
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

# the vacuity probes (FlintCatchupSReach.tla): base SZ for the zombie's, SZRisk for Risk, else SL
for pr, base in (('Hold', SL), ('MajRel', SL), ('Sole', SL), ('ZStamp', SZ), ('Excluded', SL), ('Seq3', SL),
                 ('ZombieAck', SZ), ('ServeOnZ', SZ), ('Risk', dict(SZ, DeferExpires='TRUE'))):
    bn = {id(SL): 'SL', id(SZ): 'SZ'}.get(id(base), 'SZRisk')
    cfg(f'SReach{pr}', [], base, [f'Reach_{pr}'],
        spec=f'FlintCatchupSReach (vacuity probe Reach_{pr}: must be VIOLATED) on the cfg of {bn}')
# --- rev 9: step 2's even-size vouch (flint-cb, PR #43) ---------------------------------
S2 = dict(Reps='{1, 2}'); S4 = dict(Reps='{1, 2, 3, 4}')
VD = dict(EvenVouch='"decree"'); VR = dict(EvenVouch='"read"')
for S, ov, what in (('Z', SZ, 'a zombie move (gated marks)'), ('C', SC, 'a consumer crash'), ('L', SL, 'the lost mark (no crash, no zombie)')):
    cfg(f'S2{S}V', [f'rev 9: TWO replicas, step 2\'s even-size vouch ("decree": the vouch read also writes step 4\'s decree), {what}:',
                    'Inv_NoNewLoss, Inv_MembersHold, Inv_RaidHolds.  Expect HOLD (flint-cb, PR #43).'], dict(ov, **S2, **VD))
    cfg(f'S2{S}VR', [f'rev 9: TWO replicas, the vouch "read" (concludes the read only; the decree as rev 8), {what}:',
                     'Inv_NoNewLoss, Inv_MembersHold, Inv_RaidHolds.  Expect HOLD.'], dict(ov, **S2, **VR))
    cfg(f'S2{S}', [f'rev 9 control: TWO replicas, the vouch OFF (rev 8 step 2: the stage defers for the absent leg), {what}:',
                   'Inv_NoNewLoss, Inv_MembersHold, Inv_RaidHolds.  Expect HOLD.'], dict(ov, **S2))
    cfg(f'S{S}V', [f'rev 9: three replicas, the vouch "decree" (it adds to a size-2 stamp only), {what}:',
                   'Inv_NoNewLoss, Inv_MembersHold, Inv_RaidHolds.  Expect HOLD (bounded; compare S{S}).'], dict(ov, **VD))
    cfg(f'S4{S}V', [f'rev 9: FOUR replicas, the vouch "decree" (two of four), {what}:',
                    'Inv_NoNewLoss, Inv_MembersHold, Inv_RaidHolds.  Expect HOLD (bounded).'], dict(ov, **S4, **VD))
cfg('S2ZVSoleOnly', ['rev 9: two replicas, the vouch "decree", a zombie move: Inv_SoleOnly.'], dict(SZ, **S2, **VD), ['Inv_SoleOnly'])
cfg('S2LVSoleOnly', ['rev 9: two replicas, the vouch "decree", the lost mark: Inv_SoleOnly.'], dict(SL, **S2, **VD), ['Inv_SoleOnly'])
cfg('S2ZVLate2', ['rev 9: two replicas, the vouch "decree", a zombie move: Inv_NoLateJunk2.'], dict(SZ, **S2, **VD), ['Inv_NoLateJunk2'])
cfg('S2LVAck', ['rev 9: two replicas, the vouch "decree", the lost mark: Inv_AckStamped.'], dict(SL, **S2, **VD), ['Inv_AckStamped'])
cfg('S2ZVAck', ['rev 9: two replicas, the vouch "decree", a zombie move: Inv_ZombieNoAck (record only: the permitted',
                'sole-continuation ack, as SZAck).'], dict(SZ, **S2, **VD), ['Inv_ZombieNoAck'])
cfg('SId9', ['IDENTITY check (full space, MaxFails 1): rev 9 with EvenVouch "off" must count exactly as rev 8 (7,286,102).'],
    dict(MaxFails='1'))
cfg('SId9V', ['rev 9, three replicas, MaxFails 1, the vouch "decree": the full space, record (compare SId9).'],
    dict(MaxFails='1', **VD))
# the probes: the vouch serves a one-of-two stage with the absent leg record-in_sync (Reach_Vouch, VIOL with
# the vouch, HOLD without), and the situation arises either way (Reach_VouchSpot, VIOL both)
for S, ov in (('L', SL), ('Z', SZ), ('C', SC)):
    for nm, kn in (('', VD), ('Off', {})):
        for pr in ('Vouch', 'VouchSpot'):
            cfg(f'S2Reach{pr}{S}{nm}', [], dict(ov, **S2, **kn), [f'Reach_{pr}'],
                spec=f'FlintCatchupSReach (probe Reach_{pr}: VIOLATED = reachable) on the cfg of S2{S}{"V" if kn else ""}')
# the zombie arms' completeness set: no acknowledged write lost (Inv_AckedHeld, Inv_MembersHold, which covers the
# admission's own acked-loss disjunct, Inv_RaidHolds), without Inv_AdmitNoLoss's cleanliness disjunct, which the
# first post-move assembly's junk (SZFree's write hole) fails once a later catch-up readmits z from its own epoch
ACK4 = ['TypeOK', 'Inv_AckedHeld', 'Inv_MembersHold', 'Inv_RaidHolds']
for nm, ov, kn in (('S2ZVA', dict(SZ, **S2), VD), ('S2ZVRA', dict(SZ, **S2), VR), ('S2ZA', dict(SZ, **S2), {}),
                   ('SZVA', SZ, VD), ('S4ZVA', dict(SZ, **S4), VD)):
    cfg(nm, [f'rev 9: {nm[:2] if nm[1] in "24" else "S3"} zombie arm, the vouch {kn.get("EvenVouch", "off")}, the completeness set (no acked write lost):',
             'TypeOK, Inv_AckedHeld, Inv_MembersHold, Inv_RaidHolds.  Expect HOLD.'], dict(ov, **kn), ACK4)

# --- rev 10: the torn leg (flint-cb) and the zombie's pre-stamp write as the write hole ------------------
TE = dict(TornExcluded='TRUE'); PL = dict(ZombiePreStampLegal='TRUE'); TH = dict(TE, **PL)
Z2 = dict(SZ, **S2)
cfg('S2ZT', ['rev 10: two replicas, a zombie move, the vouch "decree", TornExcluded (flint-cb\'s rule) alone:',
             'Inv_NoNewLoss, Inv_MembersHold, Inv_RaidHolds.  flint-cb expects HOLD; a zombie write that lands before',
             'the zombie\'s stamp (rev 9 S2Z\'s own trace) leaves junk without a torn stamp.'], dict(Z2, **VD, **TE))
cfg('S2ZTH', ['rev 10: two replicas, a zombie move, the vouch "decree", TornExcluded, and the pre-stamp write as',
              'the write hole (ZombiePreStampLegal): Inv_NoNewLoss, Inv_MembersHold, Inv_RaidHolds.  Expect HOLD.'],
    dict(Z2, **VD, **TH))
cfg('S2ZH', ['rev 10 control: two replicas, a zombie move, the vouch "decree", the pre-stamp write legal but',
             'TornExcluded OFF: Inv_NoNewLoss, ...  Expect VIOL (a torn z with post-stamp junk is assembled).'],
    dict(Z2, **VD, **PL))
cfg('S2ZTHOff', ['rev 10: S2ZTH with the vouch OFF.'], dict(Z2, **TH))
cfg('S2ZTHLate2', ['rev 10: S2ZTH, Inv_NoLateJunk2.  Expect HOLD.'], dict(Z2, **VD, **TH), ['Inv_NoLateJunk2'])
cfg('S2ZTLate2', ['rev 10: S2ZT (TornExcluded alone), Inv_NoLateJunk2.'], dict(Z2, **VD, **TE), ['Inv_NoLateJunk2'])
cfg('S2LTH', ['rev 10: two replicas, the lost mark, the vouch "decree", TornExcluded + ZombiePreStampLegal.  Expect HOLD.'],
    dict(SL, **S2, **VD, **TH))
cfg('S2CTH', ['rev 10: two replicas, a consumer crash (the crash-held torn leg), the vouch "decree", TornExcluded +',
              'ZombiePreStampLegal.  Expect HOLD.'], dict(SC, **S2, **VD, **TH))
cfg('SZTH', ['rev 10: three replicas, a zombie move, the vouch "decree", TornExcluded + ZombiePreStampLegal.',
             'Expect HOLD (bounded).'], dict(SZ, **VD, **TH))
cfg('SZH', ['rev 10 control: three replicas, a zombie move, the vouch "decree", the pre-stamp write legal, TornExcluded OFF.',
            'Expect VIOL (bounded).'], dict(SZ, **VD, **PL))
cfg('SId10', ['IDENTITY check (full space, MaxFails 1): rev 10 with every rev 9/10 knob off must count exactly as rev 8',
              '(7,286,102).'], dict(MaxFails='1'))
for pr, nm, ov in (('TornMarked', 'S2ReachTornMarkedZ', Z2), ('TornServed', 'S2ReachTornServedZ', Z2),
                   ('JunkNoStamp', 'S2ReachJunkNoStampZ', Z2), ('TornMarked', 'S2ReachTornMarkedC', dict(SC, **S2)),
                   ('TornMarked', 'SReachTornMarkedZ', SZ)):
    cfg(nm, [], dict(ov, **VD, **TH), [f'Reach_{pr}'],
        spec=f'FlintCatchupSReach (probe Reach_{pr}: VIOLATED = reachable), rev 10, TornExcluded + ZombiePreStampLegal, vouch "decree"')

# --- rev 11: TornNeverSole (flint-cb): a torn leg serves alone only through the expiry, marked ---------------
TN = dict(TH, TornNeverSole='TRUE'); RK = dict(DeferExpires='TRUE')
cfg('S2ZTN', ['rev 11: two replicas, a zombie move, the vouch "decree", TornExcluded + ZombiePreStampLegal + TornNeverSole:',
              'Inv_NoNewLoss, Inv_MembersHold, Inv_RaidHolds.  Expect HOLD.'], dict(Z2, **VD, **TN))
cfg('S2ZTNLate2', ['rev 11: S2ZTN, Inv_NoLateJunk2.  Expect HOLD.'], dict(Z2, **VD, **TN), ['Inv_NoLateJunk2'])
cfg('S2ZTNMarked', ['rev 11: S2ZTN, Inv_TornMarked (no ordinary stage serves torn legs alone).  Expect HOLD.'],
    dict(Z2, **VD, **TN), ['Inv_TornMarked'])
cfg('S2ZTHMarked', ['rev 11 control: TornNeverSole OFF (rev 10\'s S2ZTH), Inv_TornMarked.  Expect VIOL (the d5 shape:',
                    'a freed torn z, the clean leg down, the vouch serves z alone).'], dict(Z2, **VD, **TH), ['Inv_TornMarked'])
cfg('S2ZTNRiskMarked', ['rev 11: S2ZTN with the expiry (DeferExpires: node-gone, the operator\'s cut): Inv_TornMarked and',
                        'Inv_LossMarked (a torn leg serves alone, or a write is lost, only with AckedTailRisk).  Expect HOLD.'],
    dict(Z2, **VD, **TN, **RK), ['Inv_TornMarked', 'Inv_LossMarked'])
cfg('S2ZTNRisk', ['rev 11: S2ZTN with the expiry: Inv_NoNewLoss, record (the expiry\'s measured cost).'],
    dict(Z2, **VD, **TN, **RK), ['Inv_NoNewLoss'])
cfg('S2LTN', ['rev 11: two replicas, the lost mark, the vouch "decree", the three torn knobs.  Expect HOLD.'], dict(SL, **S2, **VD, **TN))
cfg('S2CTN', ['rev 11: two replicas, a consumer crash, the vouch "decree", the three torn knobs.  Expect HOLD.'], dict(SC, **S2, **VD, **TN))
cfg('S2ZTNOff', ['rev 11: S2ZTN with the vouch OFF.  Expect HOLD.'], dict(Z2, **TN))
cfg('SZTN', ['rev 11: three replicas, a zombie move, the vouch "decree", the three torn knobs.  Expect HOLD (bounded).'],
    dict(SZ, **VD, **TN))
cfg('SId11', ['IDENTITY check (full space, MaxFails 1): rev 11 with every rev 9-11 knob off must count exactly as rev 8',
              '(7,286,102).'], dict(MaxFails='1'))
for nm, kn, what in (('S2ReachTornServedZN', {}, 'without the expiry: expect HOLD (no ordinary stage serves a torn leg)'),
                     ('S2ReachTornServedZNRisk', RK, 'with the expiry: expect VIOL (only StageRisk serves a torn leg)')):
    cfg(nm, [f'rev 11, the three torn knobs, vouch "decree", {what}.'], dict(Z2, **VD, **TN, **kn), ['Reach_TornServed'],
        spec='FlintCatchupSReach (probe Reach_TornServed: VIOLATED = reachable)')

# --- rev 12: Inv_TornJunkMarked, the torn junk itself (flint-cb) ---------------------------------------------
J = ['Inv_TornJunkMarked']
cfg('S2ZTNJ', ['rev 12: two replicas, a zombie move, the vouch "decree", the three torn knobs: Inv_NoNewLoss,',
               'Inv_MembersHold, Inv_RaidHolds, Inv_NoLateJunk2 and Inv_TornJunkMarked (no member holds a write the',
               'fenced writer made after its stamp while AckedTailRisk is unset).  Expect HOLD.'],
    dict(Z2, **VD, **TN), NNL3 + ['Inv_NoLateJunk2'] + J)
cfg('S2ZTHJ', ['rev 12 control: TornNeverSole OFF (TornExcluded + ZombiePreStampLegal), Inv_TornJunkMarked.  Expect VIOL',
               '(the torn z serves alone on the vouch with its post-stamp junk, unmarked).'], dict(Z2, **VD, **TH), J)
cfg('S2ZHJ', ['rev 12 control: TornExcluded OFF (ZombiePreStampLegal only), Inv_TornJunkMarked.  Expect VIOL (a torn z',
              'is assembled beside a clean leg).'], dict(Z2, **VD, **PL), J)
cfg('S2ZTNJRisk', ['rev 12: S2ZTNJ with the expiry (DeferExpires): Inv_TornJunkMarked (a torn leg serves alone only',
                   'through StageRisk, which sets AckedTailRisk).  Expect HOLD.'], dict(Z2, **VD, **TN, **RK), J)
cfg('SZTNJ', ['rev 12: three replicas, a zombie move, the vouch "decree", the three torn knobs, NNL3 and',
              'Inv_TornJunkMarked.  Expect HOLD (bounded).'], dict(SZ, **VD, **TN), NNL3 + J)
cfg('SId12', ['IDENTITY check (full space, MaxFails 1): rev 12 with every rev 9-11 knob off must count exactly as rev 8',
              '(7,286,102).'], dict(MaxFails='1'))
