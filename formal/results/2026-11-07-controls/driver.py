#!/usr/bin/env python3
# driver.py -- gate item 2 of the flint catch-up gate: the TLAPS controls for the membership-stamp proof
# (FlintCatchupSProof.tla 9ed5580a), run on the on-prem cluster vc0101..vc0113 (2026-11-07).
#
# ONE script, run from the workstation (needs ssh to the nodes; nodes cannot reach each other):
#   driver.py all [--tlaps-tgz FILE] [--only NAME,NAME] [--no-gate]
#       1. prep:   check the baseline md5s; build every control's input files (a COPY of the proof with exactly one
#                  flipped ASSUME Full token, or a COPY of the model with one rule removed), verify each by diff;
#                  push the tlapm install tarball + inputs + job.py to every node and verify the tarball's sha256 there;
#       2. run:    a memory-aware scheduler: each job = (control, line range fbN) = one `tlapm --toolbox B E --cleanfp
#                  --stretch 5 --threads 5` in its own directory (own fresh .tlacache), started with nohup on a node
#                  whose free memory allows it; the scheduler is resumable (it re-reads every node's state on start);
#                  the validation control BASE (the unflipped proof) goes first;
#       3. report: fetch the logs, tally per obligation id with formal/sproof/tlapm-tally.py, write
#                  controls-table.txt and tables.json.
#   driver.py report      only step 3.   driver.py prep   only step 1.   driver.py stop   kill our jobs on all nodes.
# job.py (below, JOB_PY) is shipped to the nodes by step 1; it ASSERTS the md5 of the proof, the Ind and the model
# before tlapm runs and prints them.  Logs: logs/ (generated; to be gitignored).
import os, re, sys, json, time, glob, gzip, hashlib, shutil, subprocess, threading, difflib, datetime, tempfile

HERE = os.path.dirname(os.path.abspath(__file__))
FORMAL = os.path.abspath(os.path.join(HERE, '..', '..'))
LOGS = os.path.join(HERE, 'logs')
INP = os.path.join(LOGS, 'inputs')
TALLY = os.path.join(FORMAL, 'sproof', 'tlapm-tally.py')
USER = 'ddalton'
HOSTS = ['vc01%02d.halxg.cloudera.com' % i for i in range(1, 14)]      # vc0101..vc0113 only
NB = '/home/ddalton/flintcg/run'                                        # node base dir
THREADS = 5
MAXJOBS_PER_NODE = 6
HEADROOM_GB = 12.0       # MemAvailable must stay this far above everything our running jobs may still grow to
SAFETY = 1.0             # x the record's peak RSS for a range
JOB_CAP_S = 4 * 3600

BASELINE = {'FlintCatchupSProof.tla': '9ed5580a462718e4f737f2cbab79499c',
            'FlintCatchupSInd.tla': 'c70f9660e53176e7290cae84a746f063',
            'FlintCatchupS.tla': '61ae6829c17e74477400f270734af5e7'}
PINNED_MODEL = os.path.join(FORMAL, 'sproof', 'model', 'FlintCatchupS-r17.tla')

# the sixteen line ranges of the c31 record (RESULTS.txt of 2026-10-31-c31-ec2), cut at lemma boundaries:
# name, first line, last line, obligations, wall s and peak RSS GB there
RANGES = [('fb1', 1, 1098, 1196, 407, 5.6), ('fb2', 1099, 1939, 1146, 453, 6.4), ('fb3', 1940, 2881, 1541, 863, 10.0),
          ('fb4', 2882, 3306, 818, 455, 4.7), ('fb5', 3307, 3996, 1403, 789, 7.7), ('fb6', 3997, 4953, 1675, 1117, 10.6),
          ('fb7', 4954, 5279, 603, 469, 4.3), ('fb8', 5280, 6130, 1503, 1195, 10.4), ('fb9', 6131, 6764, 935, 865, 7.8),
          ('fb10', 6765, 7318, 1013, 943, 6.5), ('fb11', 7319, 7979, 1156, 1116, 9.0), ('fb12', 7980, 8872, 1599, 1896, 13.6),
          ('fb13', 8873, 9239, 590, 703, 5.6), ('fb14', 9240, 9875, 1357, 1451, 9.7), ('fb15', 9876, 10511, 1090, 1287, 9.1),
          ('fb16', 10512, 11061, 1391, 1550, 9.5)]

# ---- the controls -----------------------------------------------------------------------------------------------
# ('proof', old, new): replace one token of the ASSUME Full block of the proof copy (exactly once).
# ('model', old, new): replace one exact text in the model copy (exactly once); new '' removes the text.
FLAG = lambda n, v: ('proof', ' %s = %s' % (n, 'TRUE' if v else 'FALSE'), ' %s = %s' % (n, 'FALSE' if v else 'TRUE'))
CONTROLS = [
    ('BASE', 'validation: the unflipped proof, model and Ind', None),
    # the stamp's rules (11)
    ('K-Stamp', 'Stamp = TRUE -> FALSE', FLAG('Stamp', True)),
    ('K-RecordUp', 'RecordUp = TRUE -> FALSE', FLAG('RecordUp', True)),
    ('K-MajorityRead', 'MajorityRead = "count" -> "off"', ('proof', 'MajorityRead = "count"', 'MajorityRead = "off"')),
    ('K-Decree', 'Decree = TRUE -> FALSE', FLAG('Decree', True)),
    ('K-DecreeGated', 'DecreeGated = TRUE -> FALSE', FLAG('DecreeGated', True)),
    ('K-DecreeSparesLast', 'DecreeSparesLast = TRUE -> FALSE', FLAG('DecreeSparesLast', True)),
    ('K-RelView', 'RelView = TRUE -> FALSE', FLAG('RelView', True)),
    ('K-EvenVouch', 'EvenVouch = "decree" -> "off"', ('proof', 'EvenVouch = "decree"', 'EvenVouch = "off"')),
    ('K-TornExcluded', 'TornExcluded = TRUE -> FALSE', FLAG('TornExcluded', True)),
    ('K-TornNeverSole', 'TornNeverSole = TRUE -> FALSE', FLAG('TornNeverSole', True)),
    ('K-ZombiePreStampLegal', 'ZombiePreStampLegal = TRUE -> FALSE', FLAG('ZombiePreStampLegal', True)),
    # the user's 10-31 rules (5)
    ('K-WholeMark', 'WholeMark = TRUE -> FALSE', FLAG('WholeMark', True)),
    ('K-InWriteGuard', 'InWriteGuard = TRUE -> FALSE', FLAG('InWriteGuard', True)),
    ('K-MonitorGuard', 'MonitorGuard = TRUE -> FALSE', FLAG('MonitorGuard', True)),
    ('K-DrainStamps', 'DrainStamps = TRUE -> FALSE', FLAG('DrainStamps', True)),
    ('K-ZombieTriple', 'ZombieTriple = TRUE -> FALSE', FLAG('ZombieTriple', True)),
    # T's premises carried into S (6)
    ('K-CutStampsAll', 'CutStampsAll = FALSE -> TRUE', FLAG('CutStampsAll', False)),
    ('K-RaidFailsAtDrop', 'RaidFailsAtDrop = TRUE -> FALSE', FLAG('RaidFailsAtDrop', True)),
    ('K-BaseInclusive', 'BaseInclusive = TRUE -> FALSE', FLAG('BaseInclusive', True)),
    ('K-PinRetention', 'PinRetention = TRUE -> FALSE', FLAG('PinRetention', True)),
    ('K-StageOracle', 'StageOracle = FALSE -> TRUE', FLAG('StageOracle', False)),
    ('K-LocalLegGuard', 'LocalLegGuard = TRUE -> FALSE', FLAG('LocalLegGuard', True)),
    # T's model rules (5): one rule removed from a COPY of FlintCatchupS.tla (rev 17)
    ('M-SourceTrust', 'model: Covers drops "Policy = trust => t \\in tr[s]"',
     ('model', '\n  /\\ Policy = "trust" => t \\in tr[s]', '')),
    ('M-CutGone', 'model: CutOne drops "r \\notin cutGone"', ('model', '\n  /\\ r \\notin cutGone', '')),
    ('M-BaseBelowTgt', 'model: BaseCandidates drops "e <= Tgt"',
     ('model', 'e \\in tr[r] /\\ e <= Tgt ', 'e \\in tr[r]  ')),
    ('M-BulkBelowTgt', 'model: BulkCopy drops "Policy = trust => sb[r] <= Tgt"',
     ('model', '\n  /\\ Policy = "trust" => sb[r] <= Tgt', '')),
    ('M-DemoteReverts', 'model: Demote keeps reverted (no reset to -1)',
     ('model', 'reverted\' = IF Policy = "trust" THEN [reverted EXCEPT ![r] = -1] ELSE reverted', 'reverted\' = reverted')),
]

# ---- helpers ----------------------------------------------------------------------------------------------------
def md5f(p): return hashlib.md5(open(p, 'rb').read()).hexdigest()
def sha256f(p):
    h = hashlib.sha256()
    with open(p, 'rb') as f:
        for b in iter(lambda: f.read(1 << 20), b''): h.update(b)
    return h.hexdigest()
def now(): return datetime.datetime.now(datetime.timezone.utc).strftime('%Y-%m-%dT%H:%M:%SZ')
LOGF = None
def log(*a):
    s = '%s %s' % (now(), ' '.join(str(x) for x in a))
    print(s, flush=True)
    if LOGF:
        LOGF.write(s + '\n'); LOGF.flush()

SSH = ['ssh', '-n', '-o', 'BatchMode=yes', '-o', 'ConnectTimeout=20', '-o', 'ServerAliveInterval=20',
       '-o', 'ServerAliveCountMax=3', '-o', 'ControlMaster=auto', '-o', 'ControlPath=/tmp/cg-%C', '-o', 'ControlPersist=15m']
def ssh(host, cmd, timeout=120, check=False):
    try:
        r = subprocess.run(SSH + ['%s@%s' % (USER, host), cmd], capture_output=True, text=True, timeout=timeout)
    except subprocess.TimeoutExpired:
        return 255, '', 'timeout'
    if check and r.returncode != 0:
        raise RuntimeError('ssh %s %r: rc=%d %s' % (host, cmd[:80], r.returncode, r.stderr[-300:]))
    return r.returncode, r.stdout, r.stderr
def scp_to(host, local, remote, timeout=900):
    r = subprocess.run(['scp', '-q', '-o', 'BatchMode=yes', '-o', 'ControlMaster=auto', '-o', 'ControlPath=/tmp/cg-%C',
                        '-o', 'ControlPersist=15m', local, '%s@%s:%s' % (USER, host, remote)],
                       capture_output=True, text=True, timeout=timeout)
    if r.returncode: raise RuntimeError('scp %s -> %s: %s' % (local, host, r.stderr[-300:]))
def scp_from(host, remote, local, timeout=300):
    r = subprocess.run(['scp', '-q', '-o', 'BatchMode=yes', '-o', 'ControlMaster=auto', '-o', 'ControlPath=/tmp/cg-%C',
                        '-o', 'ControlPersist=15m', '%s@%s:%s' % (USER, host, remote), local],
                       capture_output=True, text=True, timeout=timeout)
    return r.returncode == 0

# ---- step 1: the inputs -----------------------------------------------------------------------------------------
def full_block_span(proof):
    a = proof.index('ASSUME Full ==')
    b = proof.index('\n\n', a)
    return a, b

def build_inputs():
    os.makedirs(INP, exist_ok=True)
    src = {n: os.path.join(FORMAL, n) for n in BASELINE}
    for n, m in BASELINE.items():
        assert md5f(src[n]) == m, 'baseline md5 of %s: %s != %s' % (n, md5f(src[n]), m)
    assert md5f(PINNED_MODEL) == BASELINE['FlintCatchupS.tla'], 'pinned model r17 md5'
    proof = open(src['FlintCatchupSProof.tla']).read()
    model = open(src['FlintCatchupS.tla']).read()
    ind = open(src['FlintCatchupSInd.tla']).read()
    a, b = full_block_span(proof)
    out = {}
    for name, desc, edit in CONTROLS:
        d = os.path.join(INP, name); os.makedirs(d, exist_ok=True)
        p2, m2 = proof, model
        if edit is None:
            pass
        elif edit[0] == 'proof':
            blk = proof[a:b]
            assert blk.count(edit[1]) == 1, (name, 'token occurs %d times in ASSUME Full' % blk.count(edit[1]))
            p2 = proof[:a] + blk.replace(edit[1], edit[2]) + proof[b:]
        else:
            assert model.count(edit[1]) == 1, (name, 'model text occurs %d times' % model.count(edit[1]))
            m2 = model.replace(edit[1], edit[2])
        assert len(p2.split('\n')) == len(proof.split('\n')) or edit and edit[0] == 'model', name
        open(os.path.join(d, 'FlintCatchupSProof.tla'), 'w').write(p2)
        open(os.path.join(d, 'FlintCatchupS.tla'), 'w').write(m2)
        open(os.path.join(d, 'FlintCatchupSInd.tla'), 'w').write(ind)
        # the diff against the pinned baseline: exactly one changed hunk in exactly one file
        dp = list(difflib.unified_diff(proof.split('\n'), p2.split('\n'), 'baseline/FlintCatchupSProof.tla',
                                        name + '/FlintCatchupSProof.tla', lineterm='', n=0))
        dm = list(difflib.unified_diff(model.split('\n'), m2.split('\n'), 'baseline/FlintCatchupS.tla',
                                        name + '/FlintCatchupS.tla', lineterm='', n=0))
        if edit is None: assert not dp and not dm
        elif edit[0] == 'proof':
            assert dp and not dm and sum(1 for l in dp if l.startswith('@@')) == 1, name
            assert sum(1 for l in dp if l.startswith('-') and not l.startswith('---')) == 1, name
        else:
            assert dm and not dp and sum(1 for l in dm if l.startswith('@@')) == 1, name
        open(os.path.join(HERE, 'diffs', name + '.diff'), 'w').write('\n'.join(dp + dm) + '\n') if edit else None
        out[name] = {'proof': md5f(os.path.join(d, 'FlintCatchupSProof.tla')),
                     'ind': md5f(os.path.join(d, 'FlintCatchupSInd.tla')),
                     'model': md5f(os.path.join(d, 'FlintCatchupS.tla'))}
    json.dump(out, open(os.path.join(LOGS, 'input-md5.json'), 'w'), indent=1)
    return out

# ---- the node-side job ------------------------------------------------------------------------------------------
JOB_PY = r'''#!/usr/bin/env python3
# job.py CTL FB BEGIN END THREADS WANT_PROOF WANT_IND WANT_MODEL   (shipped by driver.py; runs on a node)
# One range of one control: copy the three input files into a fresh job dir, ASSERT their md5s, run
#   tlapm --toolbox BEGIN END --cleanfp --stretch 5 --threads N FlintCatchupSProof.tla
# in that dir (a fresh cache), sampling the RSS of tlapm's whole process tree; write done.json.
import os, sys, json, time, shutil, hashlib, subprocess, signal, gzip, datetime, socket
ctl, fb, B, E, TH, wp, wi, wm = sys.argv[1:9]
NB = os.path.expanduser('~/flintcg/run')
J = '%s/jobs/%s__%s' % (NB, ctl, fb)
TL = NB + '/tlaps/tlapm/bin/tlapm'
def md5(p): return hashlib.md5(open(p, 'rb').read()).hexdigest()
def utc(t=None): return datetime.datetime.fromtimestamp(t or time.time(), datetime.timezone.utc).strftime('%Y-%m-%dT%H:%M:%SZ')
os.makedirs(J, exist_ok=True)
for f in os.listdir(J):
    p = os.path.join(J, f)
    shutil.rmtree(p) if os.path.isdir(p) else os.remove(p)
S = open(J + '/summary.txt', 'w')
def say(s): S.write(s + '\n'); S.flush()
t0 = time.time(); done = {'ctl': ctl, 'fb': fb, 'range': [int(B), int(E)], 'host': socket.gethostname(), 'start': utc(t0)}
def finish(rc, why=''):
    done.update(rc=rc, why=why, end=utc(), wall=round(time.time() - t0, 1))
    try:
        if os.path.exists(J + '/out.txt'):
            with open(J + '/out.txt', 'rb') as a, gzip.open(J + '/out.txt.gz', 'wb', 6) as b: shutil.copyfileobj(a, b)
            os.remove(J + '/out.txt')
        shutil.rmtree(J + '/.tlacache', ignore_errors=True)
    except Exception as e: done['gzerr'] = str(e)
    json.dump(done, open(J + '/done.json.tmp', 'w')); os.rename(J + '/done.json.tmp', J + '/done.json')
    sys.exit(0)
try:
    want = {'FlintCatchupSProof.tla': wp, 'FlintCatchupSInd.tla': wi, 'FlintCatchupS.tla': wm}
    got = {}
    for n in want:
        shutil.copy('%s/inputs/%s/%s' % (NB, ctl, n), J + '/' + n)
        got[n] = md5(J + '/' + n)
    say('job %s %s lines %s-%s start %s host %s' % (ctl, fb, B, E, utc(t0), socket.gethostname()))
    say('md5 proof %s ind %s model %s' % (got['FlintCatchupSProof.tla'][:8], got['FlintCatchupSInd.tla'][:8], got['FlintCatchupS.tla'][:8]))
    say('want proof %s ind %s model %s' % (wp[:8], wi[:8], wm[:8]))
    done['md5'] = got
    for n in want:
        if got[n] != want[n]:
            say('MD5 MISMATCH ' + n); finish(97, 'md5 mismatch ' + n)
    say('md5 ASSERTED ok')
    say('tlapm ' + subprocess.run([TL, '--version'], capture_output=True, text=True).stdout.strip().split('\n')[0])
    out = open(J + '/out.txt', 'wb')
    env = dict(os.environ); env.setdefault('HOME', os.path.expanduser('~'))
    p = subprocess.Popen(['nice', '-n', '5', TL, '--toolbox', B, E, '--cleanfp', '--stretch', '5', '--threads', TH,
                          'FlintCatchupSProof.tla'], cwd=J, stdout=out, stderr=subprocess.STDOUT, env=env,
                         start_new_session=True)
    peak = 0; peakmax1 = 0
    def tree_rss(root):
        try:
            ps = subprocess.run(['ps', '-e', '-o', 'pid=,ppid=,rss='], capture_output=True, text=True).stdout.split('\n')
        except Exception: return 0
        kids = {}; rss = {}
        for l in ps:
            f = l.split()
            if len(f) == 3: kids.setdefault(int(f[1]), []).append(int(f[0])); rss[int(f[0])] = int(f[2])
        tot = 0; st = [root]
        while st:
            x = st.pop(); tot += rss.get(x, 0); st.extend(kids.get(x, []))
        return tot
    while p.poll() is None:
        r = tree_rss(p.pid); peak = max(peak, r)
        open(J + '/rss.tmp', 'w').write('%d %d %d\n' % (r, peak, int(time.time()))); os.rename(J + '/rss.tmp', J + '/rss.txt')
        if time.time() - t0 > @CAP@:
            os.killpg(p.pid, signal.SIGKILL); break
        time.sleep(3)
    p.wait()
    try: os.killpg(p.pid, signal.SIGKILL)
    except Exception: pass
    done['peak_kb'] = peak
    done['tlapm_rc'] = p.returncode
    finish(0 if p.returncode == 0 else 1, 'capped' if time.time() - t0 > @CAP@ else '')
except SystemExit: raise
except BaseException as e:
    finish(98, 'exception %r' % (e,))
'''.replace('@CAP@', str(JOB_CAP_S))

# ---- node I/O ---------------------------------------------------------------------------------------------------
POLL = ("grep MemAvailable /proc/meminfo | awk '{print \"MEM\", $2}'; echo LOAD $(cut -d' ' -f1 /proc/loadavg); "
        "for d in %(NB)s/jobs/*/; do n=$(basename $d); [ -d $d ] || continue; "
        "if [ -f $d/done.json ]; then echo DONE $n; "
        "elif [ -f $d/rss.txt ]; then echo RUN $n $(cat $d/rss.txt) $(date +%%s); "
        "else echo INIT $n $(( $(date +%%s) - $(stat -c %%Y $d) )); fi; done" % {'NB': NB})

def poll(host):
    rc, o, e = ssh(host, POLL, timeout=60)
    if rc != 0: return None
    st = {'mem_kb': 0, 'load': 0.0, 'jobs': {}}
    for l in o.split('\n'):
        f = l.split()
        if not f: continue
        if f[0] == 'MEM': st['mem_kb'] = int(f[1])
        elif f[0] == 'LOAD': st['load'] = float(f[1])
        elif f[0] == 'DONE': st['jobs'][f[1]] = ('DONE',)
        elif f[0] == 'RUN':
            cur, peak, ts, nowt = int(f[2]), int(f[3]), int(f[4]), int(f[5])
            st['jobs'][f[1]] = ('RUN', cur, peak, nowt - ts)
        elif f[0] == 'INIT': st['jobs'][f[1]] = ('INIT', int(f[2]))
    return st

def prep_nodes(tgz, tgz_sha, inputs):
    inp_tar = os.path.join(LOGS, 'inputs.tgz')
    subprocess.run(['tar', '-C', INP, '-czf', inp_tar, '.'], check=True, env=dict(os.environ, COPYFILE_DISABLE='1'))
    jp = os.path.join(LOGS, 'job.py'); open(jp, 'w').write(JOB_PY)
    errs = []
    def one(h):
        try:
            ssh(h, 'mkdir -p %s/jobs %s/inputs' % (NB, NB), check=True)
            rc, o, _ = ssh(h, 'cat %s/tlaps.sha256 2>/dev/null; test -x %s/tlaps/tlapm/bin/tlapm && echo OK' % (NB, NB))
            if tgz_sha not in o or 'OK' not in o:
                scp_to(h, tgz, NB + '/tlaps.tgz')
                rc, o, _ = ssh(h, 'cd %s && sha256sum tlaps.tgz' % NB, timeout=300, check=True)
                got = o.split()[0]
                assert got == tgz_sha, 'sha256 on %s: %s != %s' % (h, got, tgz_sha)
                ssh(h, 'cd %s && rm -rf tlaps && mkdir tlaps && tar -xzf tlaps.tgz -C tlaps && echo %s > tlaps.sha256 && '
                       'ls tlaps' % (NB, tgz_sha), timeout=600, check=True)
            # the Isabelle heaps in the install hard-code the BUILD tree's absolute paths (heaps' parents, src/*/ROOT):
            # the install is not relocatable, so every node gets that tree (minus contrib/) at the same absolute path.
            isa = os.path.join(LOGS, 'isa-tree.tgz'); isa_sha = sha256f(isa)
            rc, o, _ = ssh(h, 'cat %s/isa-tree.sha256 2>/dev/null' % NB)
            if isa_sha not in o:
                scp_to(h, isa, NB + '/isa-tree.tgz', timeout=1800)
                rc, o, _ = ssh(h, 'cd %s && sha256sum isa-tree.tgz' % NB, timeout=300, check=True)
                assert o.split()[0] == isa_sha, 'isa-tree sha256 on %s: %s' % (h, o)
                D = '/home/ddalton/flintcg/build/tlapm/_build/default/deps/isabelle'
                ssh(h, 'rm -rf %s/Isabelle; mkdir -p %s && tar -xzf %s/isa-tree.tgz -C %s && echo %s > %s/isa-tree.sha256' % (D, D, NB, D, isa_sha, NB), timeout=900, check=True)
            scp_to(h, inp_tar, NB + '/inputs.tgz'); scp_to(h, jp, NB + '/job.py')
            ssh(h, 'cd %s && tar -xzf inputs.tgz -C inputs' % NB, timeout=120, check=True)
            rc, o, _ = ssh(h, 'cd %s && sha256sum tlaps.tgz && cat tlaps.sha256 && %s/tlaps/tlapm/bin/tlapm --version | head -1' % (NB, NB), check=True)
            log('prep', h, 'ok', o.replace('\n', ' | ')[:200])
        except Exception as e:
            errs.append((h, repr(e))); log('prep FAILED', h, repr(e))
    ts = [threading.Thread(target=one, args=(h,)) for h in HOSTS]
    [t.start() for t in ts]; [t.join() for t in ts]
    if errs: raise SystemExit('prep failed on %s' % errs)

# ---- step 2: the scheduler --------------------------------------------------------------------------------------
class Job:
    def __init__(s, ctl, fb, b, e, est_s, est_gb):
        s.ctl, s.fb, s.b, s.e, s.est_s, s.est_gb = ctl, fb, b, e, est_s, est_gb
        s.key = '%s__%s' % (ctl, fb); s.state = 'pending'; s.host = None; s.tries = 0; s.t_start = None
        s.cur_kb = 0; s.fetched = False

RESERVED = ['vc01%02d.halxg.cloudera.com' % i for i in (3, 7, 8, 9, 12, 13)]
KILL_SH = ("pids=$(pgrep -u ddalton -f '^python3 job.py %s %s '); all=\"$pids\"; cur=\"$pids\"; "
           "while [ -n \"$cur\" ]; do nxt=''; for p in $cur; do nxt=\"$nxt $(pgrep -P $p)\"; done; cur=$(echo $nxt); all=\"$all $cur\"; done; "
           "kill -9 $all 2>/dev/null; true")

def schedule(inputs, only, gate):
    jobs = []
    names = [c[0] for c in CONTROLS if only is None or c[0] in only or c[0] == 'BASE']
    for n in names:
        for fb, b, e, nob, ws, gb in sorted(RANGES, key=lambda r: -r[4]):
            jobs.append(Job(n, fb, b, e, ws, gb))
    # the record's peaks (EC2) were too low on this cluster (BASE fb1 13.0 vs 5.6 GB; fb12 73.6 vs 13.6 GB, z3 blow-ups of
    # 28 GB): use the BASE peaks observed here (x1.1), fb12 capped at 45 GB for admission (watchdog below covers the rest)
    for j in jobs:
        f = os.path.join(LOGS, 'jobs', 'BASE', j.fb, 'done.json')
        if os.path.exists(f):
            j.est_gb = max(j.est_gb, 1.1 * json.load(open(f)).get('peak_kb', 0) / 1e6)
        if j.fb == 'fb12': j.est_gb = 60.0
    by = {j.key: j for j in jobs}
    node = {h: {'jobs': set(), 'mem_kb': 0, 'load': 0, 'ok': False, 'bad': 0} for h in HOSTS}
    # adopt: whatever is on the nodes already
    for h in HOSTS:
        st = poll(h)
        if st is None: continue
        node[h].update(ok=True, mem_kb=st['mem_kb'], load=st['load'])
        for k, v in st['jobs'].items():
            if k in by:
                j = by[k]
                if v[0] == 'DONE': j.state, j.host = 'done', h
                elif v[0] == 'RUN' and v[3] < 120: j.state, j.host, j.cur_kb = 'running', h, v[1]; node[h]['jobs'].add(k)
                elif v[0] == 'INIT' and v[1] < 120: j.state, j.host = 'running', h; node[h]['jobs'].add(k)
    adopted = sum(1 for j in jobs if j.state != 'pending')
    log('scheduler: %d jobs (%d controls); adopted %d already on the nodes' % (len(jobs), len(names), adopted))
    base_keys = [j.key for j in jobs if j.ctl == 'BASE']
    released = gate is False
    t_last = 0
    def fetch(j):
        d = os.path.join(LOGS, 'jobs', j.ctl, j.fb); os.makedirs(d, exist_ok=True)
        ok = all(scp_from(j.host, '%s/jobs/%s/%s' % (NB, j.key, f), d) for f in ('done.json', 'summary.txt', 'out.txt.gz'))
        if ok: j.fetched = True
        return ok
    while True:
        # poll every node
        for h in HOSTS:
            st = poll(h)
            if st is None:
                node[h]['bad'] += 1; node[h]['ok'] = node[h]['bad'] < 3 and node[h]['ok']
                continue
            node[h].update(ok=True, bad=0, mem_kb=st['mem_kb'], load=st['load'])
            for k in list(node[h]['jobs']):
                j = by[k]; v = st['jobs'].get(k)
                if v is None or (v[0] == 'RUN' and v[3] > 180):       # lost: the job died without a done.json
                    j.tries += 1; log('LOST', k, h, v); ssh(h, 'rm -rf %s/jobs/%s' % (NB, k))
                    node[h]['jobs'].discard(k); j.state, j.host = ('pending' if j.tries < 12 else 'lost'), None
                elif v[0] == 'RUN': j.cur_kb = v[1]
                elif v[0] == 'DONE':
                    node[h]['jobs'].discard(k); j.state = 'done'
        # watchdog: a node near out-of-memory loses its biggest job of ours (the job is re-queued, from scratch)
        for h in HOSTS:
            n = node[h]
            if n['ok'] and n['mem_kb'] / 1e6 < 6.0 and n['jobs']:
                k = max(n['jobs'], key=lambda q: by[q].cur_kb); ctl, fb = k.split('__')
                log('WATCHDOG', h, 'MemAvailable %.1f GB: killing %s (cur %.1f GB)' % (n['mem_kb'] / 1e6, k, by[k].cur_kb / 1e6))
                ssh(h, KILL_SH % (ctl, fb))
                n['mem_kb'] += by[k].cur_kb
        for j in jobs:
            if j.state == 'done' and not j.fetched:
                if not fetch(j): log('fetch retry', j.key)
                else:
                    dj = json.load(open(os.path.join(LOGS, 'jobs', j.ctl, j.fb, 'done.json')))
                    log('DONE %-24s %-5s %s rc=%s wall=%ss peak=%.1fGB %s' % (j.ctl, j.fb, j.host[:6], dj.get('rc'),
                        dj.get('wall'), dj.get('peak_kb', 0) / 1e6, dj.get('why', '')))
        # release the controls once the validation is under way and healthy
        if not released:
            bd = [by[k] for k in base_keys if by[k].state == 'done']
            bad = [j for j in bd if json.load(open(os.path.join(LOGS, 'jobs', j.ctl, j.fb, 'done.json'))).get('rc') != 0]
            if len(bd) >= 6 and not bad and all(by[k].state != 'pending' for k in base_keys):
                released = True; log('gate: BASE has %d ranges done, none bad: the controls are released' % len(bd))
            elif bad: log('gate: BASE range(s) bad: %s -- controls held' % [j.key for j in bad])
        # dispatch
        pend = [j for j in jobs if j.state == 'pending' and (j.ctl == 'BASE' or released)]
        fb12_wait = any(j.fb == 'fb12' for j in pend)
        small_wait = any(j.fb != 'fb12' for j in pend)
        for j in pend:
            best = None
            for h in HOSTS:
                n = node[h]
                if not n['ok'] or len(n['jobs']) >= MAXJOBS_PER_NODE or n['load'] > 34: continue
                # fb12 (z3 blow-ups, ~75 GB peak here) runs alone on a node; while other work is waiting, six nodes are
                # kept for it (they drain) and the rest run the small ranges
                has12 = any(by[q].fb == 'fb12' for q in n['jobs'])
                if j.fb == 'fb12':
                    if n['jobs']: continue
                    if small_wait and h not in RESERVED: continue
                else:
                    if has12 or (fb12_wait and h in RESERVED): continue
                grow = sum(max(0.0, by[k].est_gb * SAFETY - by[k].cur_kb / 1e6) for k in n['jobs'])
                free = n['mem_kb'] / 1e6 - grow - (j.est_gb * SAFETY) - HEADROOM_GB
                if free > 0 and (best is None or free > best[0]): best = (free, h)
            if best is None: continue
            h = best[1]
            want = inputs[j.ctl]
            cmd = ('cd %s && (setsid nohup python3 job.py %s %s %d %d %d %s %s %s </dev/null >/dev/null 2>&1 &)' %
                   (NB, j.ctl, j.fb, j.b, j.e, THREADS, want['proof'], want['ind'], want['model']))
            rc, _, err = ssh(h, cmd, timeout=60)
            if rc != 0: log('dispatch failed', j.key, h, err[-100:]); continue
            j.state, j.host, j.t_start, j.cur_kb = 'running', h, time.time(), 0
            node[h]['jobs'].add(j.key)
            node[h]['mem_kb'] -= 0        # the next poll shows the real number; `grow` covers the gap
            log('START %-24s %-5s on %s (node jobs %d, free %.0f GB)' % (j.ctl, j.fb, h[:6], len(node[h]['jobs']), node[h]['mem_kb'] / 1e6))
        cnt = {s: sum(1 for j in jobs if j.state == s) for s in ('pending', 'running', 'done', 'lost')}
        if time.time() - t_last > 300:
            t_last = time.time()
            log('status', cnt, ' '.join('%s:%d/%.0fG' % (h[4:6], len(node[h]['jobs']), node[h]['mem_kb'] / 1e6) for h in HOSTS))
        if cnt['pending'] == 0 and cnt['running'] == 0 and all(j.fetched for j in jobs if j.state == 'done'):
            break
        if cnt['lost'] and cnt['pending'] == 0 and cnt['running'] == 0: break
        time.sleep(25)
    log('scheduler finished', cnt)

# ---- step 3: the tally ------------------------------------------------------------------------------------------
def lemma_step(plines, line):
    lem = '?'; step = '?'
    for i in range(line - 1, -1, -1):
        m = re.match(r'\s*(<\d+>[^\s.]*)\.', plines[i])
        if step == '?' and m: step = m.group(1)
        m = re.match(r'(LEMMA|THEOREM)\s+(\w+)', plines[i])
        if m: lem = m.group(2); break
    return lem, step

def tally_control(name):
    base = os.path.join(LOGS, 'jobs', name)
    tmp = tempfile.mkdtemp(prefix='tal-')
    files = []; walls = []; peaks = []; hosts = set(); starts = []; ends = []; rcs = []; md5s = set()
    for fb, b, e, nob, ws, gb in RANGES:
        d = os.path.join(base, fb)
        dj = json.load(open(os.path.join(d, 'done.json')))
        walls.append(dj['wall']); peaks.append(dj.get('peak_kb', 0) / 1e6); hosts.add(dj['host'].split('.')[0])
        starts.append(dj['start']); ends.append(dj['end']); rcs.append((dj['rc'], dj.get('tlapm_rc')))
        md5s.add(json.dumps(dj['md5'], sort_keys=True))
        f = os.path.join(tmp, fb + '.out')
        with gzip.open(os.path.join(d, 'out.txt.gz'), 'rb') as a, open(f, 'wb') as b2: shutil.copyfileobj(a, b2)
        files.append(f)
    r = subprocess.run([sys.executable, TALLY] + files, capture_output=True, text=True)
    shutil.rmtree(tmp, ignore_errors=True)
    t = r.stdout.split('\n')
    m = re.match(r'TALLY (\{.*\}) total (\d+)', t[0])
    counts = eval(m.group(1)); total = int(m.group(2))
    fails = []
    for l in t[1:]:
        m = re.match(r'NOT PROVED (\d+):(\d+):(\d+):(\d+) (\S+) (\S+)', l)
        if m: fails.append((int(m.group(1)), int(m.group(2)), int(m.group(3)), int(m.group(4)), m.group(5), m.group(6)))
    return dict(counts=counts, total=total, fails=fails, walls=walls, peaks=peaks, hosts=sorted(hosts),
                start=min(starts), end=max(ends), rcs=rcs, md5=[json.loads(x) for x in md5s])

def report():
    inputs = json.load(open(os.path.join(LOGS, 'input-md5.json')))
    proof = open(os.path.join(FORMAL, 'FlintCatchupSProof.tla')).read().split('\n')
    res = {}
    for name, desc, edit in CONTROLS:
        if not os.path.exists(os.path.join(LOGS, 'jobs', name, RANGES[-1][0], 'done.json')) and \
           not all(os.path.exists(os.path.join(LOGS, 'jobs', name, r[0], 'done.json')) for r in RANGES):
            continue
        try: res[name] = tally_control(name)
        except Exception as ex: print('tally failed', name, ex)
    lines = []
    ISA = {(131, 5), (1262, 3)}
    for name, desc, edit in CONTROLS:
        if name not in res: continue
        t = res[name]
        cp = os.path.join(INP, name, 'FlintCatchupSProof.tla')
        pl = open(cp).read().split('\n')
        fl = []
        for (l1, c1, l2, c2, st, fn) in t['fails']:
            lem, step = lemma_step(pl, l1)
            fl.append(((l1, c1), '%d:%d %s %s [%s]' % (l1, c1, lem, step, st)))
        isa = [x for x in fl if x[0] in ISA]
        other = [x for x in fl if x[0] not in ISA]
        t['isa'] = [x[1] for x in isa]; t['other'] = [x[1] for x in other]
        c = t['counts']
        if name == 'BASE': verdict = 'VALIDATION'
        elif t['other']: verdict = 'KILLED'
        else: verdict = 'NOT KILLED'
        t['verdict'] = verdict
        lines.append('%s | %s | %s' % (name, desc, verdict))
        lines.append('   counts %s total %d | failed outside 131:5/1262:3: %d (%s) | 131:5/1262:3 failed: %d' %
                     (json.dumps(c, sort_keys=True), t['total'], len(t['other']), '; '.join(t['other']) or '-', len(t['isa'])))
        lines.append('   wall: slowest range %.0f s, sum of ranges %.0f s | peak RSS: largest range %.1f GB, sum of range peaks %.1f GB | window %s - %s | hosts %s' %
                     (max(t['walls']), sum(t['walls']), max(t['peaks']), sum(t['peaks']), t['start'], t['end'], ','.join(t['hosts'])))
    txt = '\n'.join(lines) + '\n'
    open(os.path.join(HERE, 'controls-table.txt'), 'w').write(txt)
    json.dump(res, open(os.path.join(HERE, 'tables.json'), 'w'), indent=1, default=str)
    print(txt)
    print('table md5', hashlib.md5(txt.encode()).hexdigest())

def stop_all():
    for h in HOSTS:
        ssh(h, "pkill -u %s -f 'run/[t]laps' ; pkill -u %s -f '[j]ob.py'; true" % (USER, USER))
        log('stopped', h)

def main():
    global LOGF
    os.makedirs(LOGS, exist_ok=True); os.makedirs(os.path.join(HERE, 'diffs'), exist_ok=True)
    LOGF = open(os.path.join(LOGS, 'driver.log'), 'a')
    cmd = sys.argv[1] if len(sys.argv) > 1 else 'all'
    args = sys.argv[2:]
    tgz = args[args.index('--tlaps-tgz') + 1] if '--tlaps-tgz' in args else os.path.join(LOGS, 'tlaps.tgz')
    only = set(args[args.index('--only') + 1].split(',')) if '--only' in args else None
    if cmd == 'report': return report()
    if cmd == 'stop': return stop_all()
    inputs = build_inputs()
    log('inputs built: %d controls; md5 baseline proof %s' % (len(inputs), inputs['BASE']['proof']))
    sha = sha256f(tgz); log('tlapm tarball', tgz, 'sha256', sha)
    prep_nodes(tgz, sha, inputs)
    if cmd == 'prep': return
    log('RUN START')
    schedule(inputs, only, gate=not ('--no-gate' in args))
    log('RUN END')
    report()

if __name__ == '__main__':
    main()
