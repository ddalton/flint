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
def cfg(name, head, over={}, invs=NNL3, props=(), spec='', sym=False, specname='Spec'):
    k = dict(BASE); k.update(over)
    nrep = {2: 'two', 3: 'three', 4: 'four'}[k['Reps'].count(',') + 1]
    bound = f"bound {'D' if k['MaxFails']=='2' else 'D, MaxFails ' + k['MaxFails']} ({nrep} replicas, one write, two epochs, one retained)"
    lines = [f'\\* FlintCatchupS, {bound}.',
             '\\* Code-faithful otherwise (no wait for a cut, the assembly mark, the FlintLocalLeg guard, F36c defer, NO capture),',
             '\\* trust, the mark-race fix (CutStampsAll FALSE).'] + [f'\\* {h}' for h in head] + [RULE, f'SPECIFICATION {specname}', 'CONSTANTS']
    lines += [f'  {a} = {b}' for a, b in k.items()]
    lines += [f'INVARIANT {i}' for i in invs] + [f'PROPERTY {p}' for p in props] + (['SYMMETRY Sym'] if sym else []) + ['CHECK_DEADLOCK FALSE']
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
# --- c27 (EC2 m7i.48xlarge, tla-mc 0.2.1): rev 12's full rule set at three replicas, to the full space if it fits ---------
FULL = NNL3 + ['Inv_NoLateJunk2'] + J
cfg('XZTNJ', ['c27 (EC2): THREE replicas, a zombie move, the full rule set (the vouch "decree", TornExcluded,',
              'ZombiePreStampLegal, TornNeverSole): NNL3, NoLateJunk2, TornJunkMarked.  Expect HOLD (full space if it fits).'],
    dict(SZ, **VD, **TN), FULL)
cfg('XLTNJ', ['c27 (EC2): three replicas, the lost mark, the full rule set: NNL3, NoLateJunk2, TornJunkMarked.  Expect HOLD.'],
    dict(SL, **VD, **TN), FULL)
cfg('XCTNJ', ['c27 (EC2): three replicas, a consumer crash, the full rule set: NNL3, NoLateJunk2, TornJunkMarked.  Expect HOLD.'],
    dict(SC, **VD, **TN), FULL)
cfg('XZTNJRisk', ['c27 (EC2): three replicas, a zombie move, the full rule set WITH the expiry: TornJunkMarked and LossMarked',
                  '(the expiry may lose a write held only on an unreachable leg, never unmarked).  Expect HOLD.'],
    dict(SZ, **VD, **TN, **RK), J + ['Inv_LossMarked'])
cfg('XZSole', ['c27 (EC2): three replicas, a zombie move, the full rule set: Inv_SoleOnly.  Expect HOLD.'], dict(SZ, **VD, **TN), ['Inv_SoleOnly'])
cfg('XLSole', ['c27 (EC2): three replicas, the lost mark, the full rule set: Inv_SoleOnly.  Expect HOLD.'], dict(SL, **VD, **TN), ['Inv_SoleOnly'])
cfg('XAck', ['c27 (EC2): three replicas, the lost mark, the full rule set: Inv_AckStamped (the barrier).  Expect HOLD.'],
    dict(SL, **VD, **TN), ['Inv_AckStamped'])
cfg('X4ZTNJ', ['c27 (EC2): FOUR replicas, a zombie move, the full rule set: NNL3, TornJunkMarked.  Expect HOLD (bounded likely).'],
    dict(SZ, **S4, **VD, **TN), NNL3 + J)
# --- c28 (box, tla-mc 0.2.1): the gate's liveness item (flint-cb, 10-31): Prop_BarrierEnds at TWO replicas on rev 12's
# full rule set, each fault, full pass; each with its record-down control (a minority barrier never ends: a LASSO) ------
for S3, ov3, what3 in (('L', SL, 'the lost mark'), ('C', SC, 'a consumer crash'), ('Z', SZ, 'a zombie move')):
    cfg(f'S2Live{S3}', [f'c28: two replicas, {what3}, the full rule set (the vouch "decree", the three torn knobs): a barrier',
                        'ends (Prop_BarrierEnds) under the weak fairness of Spec, RecordUp.  Expect HOLD (full pass).'],
        dict(ov3, **S2, **VD, **TN), ['TypeOK'], ['Prop_BarrierEnds'])
    cfg(f'S2Live{S3}NoRec', [f'c28 control for S2Live{S3}: RecordUp FALSE, a minority barrier never ends.  Expect a LASSO.'],
        dict(ov3, **S2, **VD, **TN, RecordUp='FALSE'), ['TypeOK'], ['Prop_BarrierEnds'])
# --- c29 (box, tla-mc 0.2.1): the gate's item 3, SYMMETRY over the replica ids (rev 13 41fa0652 + FlintCatchupSSym.tla).
# M* = rev 13 with model-value ids and NO symmetry: each must count EXACTLY as its rev 12 row (integer ids) and holds
# Inv_NewestUnique (NewestS's CHOOSE is id-free); Y* = the same cfg + SYMMETRY Sym: the same verdict (a control at the
# same depth), fewer states.  MZTNJU/YZTNJ: the three-replica zombie, bounded, to size the reduction level by level.
MV2 = dict(Reps='{r1, r2}'); MV3 = dict(Reps='{r1, r2, r3}'); U = ['Inv_NewestUnique']
for P, sym in (('M', False), ('Y', True)):
    tag = 'with SYMMETRY Sym' if sym else 'model-value ids, no symmetry'
    cfg(f'{P}Id12', [f'c29 ({tag}): SId12 (rev 12 7,286,102 / d36, HOLD).'], dict(MaxFails='1', **MV3), NNL3 + U, sym=sym)
    cfg(f'{P}2ZTNJ', [f'c29 ({tag}): S2ZTNJ (rev 12 29,140,655 / d39, HOLD).'], dict(SZ, **VD, **TN, **MV2),
        NNL3 + ['Inv_NoLateJunk2'] + J + U, sym=sym)
    cfg(f'{P}2LTN', [f'c29 ({tag}): S2LTN (rev 11/12 3,627,109 / d35, HOLD).'], dict(SL, **VD, **TN, **MV2), NNL3 + U, sym=sym)
    cfg(f'{P}2CTN', [f'c29 ({tag}): S2CTN (rev 11/12 5,326,386 / d35, HOLD).'], dict(SC, **VD, **TN, **MV2), NNL3 + U, sym=sym)
    cfg(f'{P}2ZTHJ', [f'c29 ({tag}): control S2ZTHJ (rev 12 VIOL TornJunkMarked d7).'], dict(SZ, **VD, **TH, **MV2), J, sym=sym)
    cfg(f'{P}2ZHJ', [f'c29 ({tag}): control S2ZHJ (rev 12 VIOL TornJunkMarked d5).'], dict(SZ, **VD, **PL, **MV2), J, sym=sym)
    cfg(f'{P}ZDecreeUngated', [f'c29 ({tag}): control SZDecreeUngated (rev 7 VIOL NoNewLoss d13).'],
        dict(SZ, DecreeGated='FALSE', **MV3), sym=sym)
cfg('MZTNJU', ['c29 (model-value ids, no symmetry): the three-replica zombie, the full rule set (as XZTNJ), Inv_NewestUnique;',
               'bounded: sizes the reduction level by level against YZTNJ.'], dict(SZ, **VD, **TN, **MV3), U)
cfg('YZTNJ', ['c29 (with SYMMETRY Sym): the three-replica zombie, the full rule set (as XZTNJ), NNL3 and the junk invariants;',
              'bounded: sizes the reduction against MZTNJU.'], dict(SZ, **VD, **TN, **MV3), NNL3 + ['Inv_NoLateJunk2'] + J + U, sym=True)
# --- c30 (box, tla-mc 0.2.1, ms12 rev 12 + FlintCatchupSLiveW.tla): flint-cb's open question (10-31): does a barrier end
# WITHOUT a failed leg's path coming back?  The c28 arms with SpecNoReturn (Spec's fairness less WF_vars(Return(r))).
for S3, ov3, what3 in (('L', SL, 'the lost mark'), ('C', SC, 'a consumer crash'), ('Z', SZ, 'a zombie move')):
    cfg(f'S2Live{S3}NoRet', [f'c30: S2Live{S3} ({what3}) under SpecNoReturn: Spec\'s fairness without WF_vars(Return(r)).',
                             'Does a barrier end without a failed path coming back?  Record (no expectation).'],
        dict(ov3, **S2, **VD, **TN), ['TypeOK'], ['Prop_BarrierEnds'], specname='SpecNoReturn')
# c30b: the positive control that the SPECIFICATION line switches the fairness (Prop_PathReturns, FlintCatchupSLiveW.tla)
cfg('S2PathSpec', ['c30b: two replicas, the lost mark, the full rule set, Spec: Prop_PathReturns.  Expect HOLD (WF Return).'],
    dict(SL, **S2, **VD, **TN), ['TypeOK'], ['Prop_PathReturns'])
cfg('S2PathNoRet', ['c30b: as S2PathSpec under SpecNoReturn.  Expect a LASSO (a path stays out).'],
    dict(SL, **S2, **VD, **TN), ['TypeOK'], ['Prop_PathReturns'], specname='SpecNoReturn')
# --- c31 (EC2, tla-mc 0.2.2 compiled; the gate's items 5+6, the user's go 10-31): the crash twin of XAck
cfg('XCAck', ['c31 (EC2): three replicas, a consumer crash, the full rule set: Inv_AckStamped (the barrier).  Expect HOLD,',
              'the full space of XCTNJ (2,325,892,812 / d46: the same constants, one invariant).'], dict(SC, **VD, **TN), ['Inv_AckStamped'])
# --- c32 (box): c29's three-replica zombie pair again with the uniqueness of the read fields (Inv_NewestCoreUnique)
cfg('MZTNJC', ['c32 (model-value ids, no symmetry): the three-replica zombie, the full rule set (as XZTNJ), Inv_NewestCoreUnique;',
               'bounded.'], dict(SZ, **VD, **TN, **MV3), ['Inv_NewestCoreUnique'])
cfg('YZTNJC', ['c32 (with SYMMETRY Sym): the three-replica zombie, the full rule set (as XZTNJ), NNL3, the junk invariants and',
               'Inv_NewestCoreUnique; bounded: sizes the reduction against MZTNJC.'], dict(SZ, **VD, **TN, **MV3),
    NNL3 + ['Inv_NoLateJunk2'] + J + ['Inv_NewestCoreUnique'], sym=True)
# --- proof (gate item 2): candidate inductive-invariant conjuncts (FlintCatchupSInd.tla), each an INVARIANT, on the
# c27 rule set: P0<F> two replicas (full), P0<F>3 three replicas (bounded)
J0 = ['J_DueHold', 'J_HoldAtt', 'J_Rel', 'J_Cur', 'J_ZxOut', 'J_Aseq', 'J_SeqSync', 'J_MemRel', 'J_RelHold', 'J_NoAckBad']
for F, ovF in (('Z', SZ), ('C', SC), ('L', SL)):
    cfg(f'P0{F}', [f'proof v0: two replicas, {F}, the full rule set; the candidate conjuncts J0.'], dict(ovF, **S2, **VD, **TN), J0)
    cfg(f'P0{F}3', [f'proof v0: three replicas, {F}, the full rule set; the candidate conjuncts J0 (bounded).'], dict(ovF, **VD, **TN), J0)
J1 = J0 + ['J_M0', 'J_RelValid', 'J_MinorRel', 'J_RelInSync', 'J_ViewIn', 'J_ViewOut', 'J_SoleSeq']
for F, ovF in (('Z', SZ), ('C', SC), ('L', SL)):
    cfg(f'P1{F}', [f'proof v1: two replicas, {F}, the full rule set; the candidate conjuncts J1.'], dict(ovF, **S2, **VD, **TN), J1)
    cfg(f'P1{F}3', [f'proof v1: three replicas, {F}, the full rule set; the candidate conjuncts J1 (bounded).'], dict(ovF, **VD, **TN), J1)
# --- rev 14 probe (box; flint-cb 10-31): the premise the stamp proof inherits.  LastFails lets the last healthy member
# fail; WholeMark marks every in_sync leg the live raid lacks in one write (R1).  Invariants: the stage's business
# (MembersHold, RaidHolds, ~badLoss); Inv_AckedHeld is left out because with LastFails every leg may fail and be
# marked stale (a dead volume, nothing served), which violates it with or without WholeMark.
R14 = dict(LastFails='FALSE', WholeMark='FALSE'); SG = ['Inv_MembersHold', 'Inv_RaidHolds', 'Inv_AdmitNoLoss']
cfg('Q14Id', ['rev 14 identity: both knobs off, SId12 (must count 7,286,102 / d36).'], dict(MaxFails='1', **R14))
cfg('Q14LF', ['rev 14: three replicas, the lost mark, the full rule set, LastFails: expect VIOL (the stage serves a leg lost',
              'before the last release, whose members were all marked stale).'], dict(SL, **VD, **TN, LastFails='TRUE', WholeMark='FALSE'), SG)
cfg('Q14LFW', ['rev 14: as Q14LF with WholeMark (R1): expect no violation (bounded).'], dict(SL, **VD, **TN, LastFails='TRUE', WholeMark='TRUE'), SG)
cfg('Q14W', ['rev 14: WholeMark alone (rev 13 failure model): expect no violation (bounded).'], dict(SL, **VD, **TN, LastFails='FALSE', WholeMark='TRUE'), SG)
cfg('Q14LF2', ['rev 14: TWO replicas, the lost mark, LastFails: expect HOLD (a lost leg of two is a minority: the record marks it).'],
    dict(SL, **S2, **VD, **TN, LastFails='TRUE', WholeMark='FALSE'), SG)
# q14b: the probe at MaxFails 3 (q14's c27 bound has MaxFails 2: the gap trace needs three failures, c then a then b,
# so q14's LastFails arm was in the WRONG CONFIGURATION and ruled out nothing)
F3 = dict(MaxFails='3')
cfg('Q14LF3', ['rev 14, MaxFails 3: three replicas, the lost mark, the full rule set, LastFails: expect VIOL (the stage serves',
               'a leg lost before the last release, whose members were all marked stale).'], dict(SL, **VD, **TN, **F3, LastFails='TRUE', WholeMark='FALSE'), SG)
cfg('Q14LFW3', ['rev 14, MaxFails 3: as Q14LF3 with WholeMark (R1): expect no violation (bounded).'], dict(SL, **VD, **TN, **F3, LastFails='TRUE', WholeMark='TRUE'), SG)
cfg('Q14N3', ['rev 14, MaxFails 3: both knobs off (rev 13: the last healthy member never fails): expect no violation (bounded).'],
    dict(SL, **VD, **TN, **F3, LastFails='FALSE', WholeMark='FALSE'), SG)
# --- rev 15 probe (box; flint-cb 10-31): the drain window.  WholeMark TRUE (the code's monitor), LastFails FALSE (the
# rev 13 failure model), MaxFails 3; G = InWriteGuard, S = DrainStamps (the hook on the shared removal path)
R15 = dict(LastFails='FALSE', WholeMark='TRUE', OperatorMark='TRUE')
cfg('D15Id', ['rev 15 identity: every knob off, SId12 (must count 7,286,102 / d36).'],
    dict(MaxFails='1', LastFails='FALSE', WholeMark='FALSE', OperatorMark='FALSE', InWriteGuard='FALSE', DrainStamps='FALSE'))
for G in ('N', 'G'):
    for S_ in ('N', 'S'):
        kn = dict(R15, InWriteGuard='TRUE' if G == 'G' else 'FALSE', DrainStamps='TRUE' if S_ == 'S' else 'FALSE')
        what = ('the in-write refusal' if G == 'G' else 'no in-write refusal') + ', ' + ('the planned removal stamps' if S_ == 'S' else 'no stamp on the planned removal (§12 today)')
        cfg(f'D15{G}{S_}', [f'rev 15, MaxFails 3: three replicas, the lost mark, the full rule set, the drain: {what}.',
                           'MembersHold, RaidHolds, ~badLoss.'], dict(SL, **VD, **TN, **F3, **kn), SG)
        if (G, S_) in (('N', 'N'), ('G', 'N'), ('G', 'S')):
            cfg(f'D15{G}{S_}A', [f'rev 15, MaxFails 3: as D15{G}{S_}: Inv_AckStamped.'], dict(SL, **VD, **TN, **F3, **kn), ['Inv_AckStamped'])
# --- proof (gate item 2) on rev 15 (the user via flint-cb, 10-31: "adopt the in-write rule and the whole mark"):
# the rule's knobs on (LastFails, WholeMark, InWriteGuard), the drain off (OperatorMark FALSE until DrainStamps is
# decided); bases for the v5 peel (FlintCatchupSInd: v4 + J_GenBound, J_ZGen): two replicas full (L, C, Z, and CZ:
# the proof's Full has the crash and the zombie together), three replicas bounded; MaxFails = the replica count, so
# every leg can fail (LastFails)
RP = dict(LastFails='TRUE', WholeMark='TRUE', OperatorMark='FALSE', InWriteGuard='TRUE', DrainStamps='FALSE')
for F, ovF in (('Z', SZ), ('C', SC), ('L', SL), ('CZ', dict(SC, **SZ))):
    cfg(f'P15{F}', [f'proof on rev 15: two replicas, {F}, the full rule set and the in-write rule (drain off); the peel base.'],
        dict(ovF, **S2, **VD, **TN, **RP, MaxFails='2'), J1)
for F, ovF in (('Z', SZ), ('C', SC), ('L', SL)):
    cfg(f'P15{F}3', [f'proof on rev 15: three replicas, {F}, the full rule set and the in-write rule (drain off); the peel base (bounded).'],
        dict(ovF, **VD, **TN, **RP, **F3), J1)
# --- rev 16 (10-31): MonitorGuard, the in-write rule on the monitor's write.  Id16: every knob off (must count
# 7,286,102 / d36).  G16<F>: the rule's model check, three replicas, MaxFails 3, LastFails + WholeMark + InWriteGuard +
# MonitorGuard, the drain off: the goal set (NNL3 = NoNewLoss incl. AckedHeld, MembersHold, RaidHolds) and AckStamped,
# bounded.  P16<F>: the v6 peel bases (as P15<F>, MonitorGuard on).
R16 = dict(LastFails='TRUE', WholeMark='TRUE', OperatorMark='FALSE', InWriteGuard='TRUE', DrainStamps='FALSE', MonitorGuard='TRUE')
cfg('Id16', ['rev 16 identity: every knob off, SId12 (must count 7,286,102 / d36).'],
    dict(MaxFails='1', LastFails='FALSE', WholeMark='FALSE', OperatorMark='FALSE', InWriteGuard='FALSE', DrainStamps='FALSE', MonitorGuard='FALSE'))
for F, ovF in (('L', SL), ('C', SC), ('Z', SZ)):
    cfg(f'G16{F}', [f'rev 16, MaxFails 3: three replicas, {F}, the full rule set and the in-write rule (drain off): the goal set',
                    'and AckStamped (bounded).'], dict(ovF, **VD, **TN, **F3, **R16), NNL3 + ['Inv_AckStamped'])
for F, ovF in (('Z', SZ), ('C', SC), ('L', SL), ('CZ', dict(SC, **SZ))):
    cfg(f'P16{F}', [f'proof on rev 16: two replicas, {F}, the full rule set and the in-write rule (drain off); the peel base.'],
        dict(ovF, **S2, **VD, **TN, **R16, MaxFails='2'), J1)
for F, ovF in (('Z', SZ), ('C', SC), ('L', SL)):
    cfg(f'P16{F}3', [f'proof on rev 16: three replicas, {F}, the full rule set and the in-write rule (drain off); the peel base (bounded).'],
        dict(ovF, **VD, **TN, **R16, **F3), J1)
# --- inv-zombie's trace (10-31): four replicas, the zombie, three failures: the zombie moves during a pending loss and
# acks as the gen-2 confirmed continuation of {a, z} (ZombieReleases reads SolePairs, the pair without the members).
# Expect VIOL (J_RelHold, then the goal set); MaxAsm 2, the rest as the rev 16 rule set.
cfg('Z4Ack', ['inv-zombie trace probe: four replicas, the zombie, MaxFails 3, MaxAsm 2, the rev 16 rule set: the goal set',
              'and AckStamped (expect VIOL if the zombie can ack as a confirmed continuation it is not alone in).'],
    dict(SZ, **S4, **VD, **TN, **F3, **R16, MaxAsm='2'), NNL3 + ['Inv_AckStamped'])
