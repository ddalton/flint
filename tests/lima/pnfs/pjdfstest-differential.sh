#!/usr/bin/env bash
# Leg A0 — pjdfstest against flint, WITH a knfsd control arm.
#
# pjdfstest is the industry-accepted POSIX filesystem conformance suite
# (~8,800 assertions; what CephFS, GlusterFS and OpenZFS use). Crucially
# it is MULTI-UID by design: it runs as root and switches credentials.
# Every suite this repo already runs — pynfs, nfstest_posix — runs as a
# single uid and is therefore structurally blind to authorization.
#
# WHY THE CONTROL ARM IS NOT OPTIONAL. pjdfstest is written against local
# filesystem semantics, and NFS legitimately differs in places (AUTH_SYS
# carries at most 16 supplementary gids, no atomic O_EXCL over some
# paths, etc). Run against knfsd, the reference implementation, it still
# fails ~149 assertions. Scoring flint's raw failure count against zero
# would therefore charge it for NFS being NFS. Only the per-test
# DIFFERENTIAL — failing here and NOT on knfsd — is evidence.
#
# ⚠ A HIGHER PASS COUNT IS NOT AUTOMATICALLY BETTER. flint currently
# "passes" 131 assertions knfsd fails, and not one of them is a win: they
# are tests that expect an operation to SUCCEED, and flint passes them
# because it permits everything. A suite where "allow everything" scores
# points is exactly why this reads per-test rather than by score.
set -uo pipefail

VM=${LIMA_VM:-flint-nfs-client}
PORT=${NFS_PORT:-20490}
BASELINE=${BASELINE:-tests/lima/pjdfstest-baseline.json}

vm() { limactl shell "$VM" -- sudo bash -lc "$1"; }

echo "── building pjdfstest if absent ──"
vm 'test -x /opt/pjdfstest/pjdfstest || {
      rm -rf /opt/pjdfstest
      git clone --depth 1 https://github.com/pjd/pjdfstest.git /opt/pjdfstest >/dev/null 2>&1
      cd /opt/pjdfstest && autoreconf -ifs >/dev/null 2>&1 && ./configure >/dev/null 2>&1 && make pjdfstest >/dev/null 2>&1
    }
    test -x /opt/pjdfstest/pjdfstest' || { echo "FAIL: pjdfstest did not build"; exit 1; }

echo "── arm A: flint ──"
vm "systemctl stop flint-pjd 2>/dev/null||true; systemctl reset-failed flint-pjd 2>/dev/null||true
    umount -f /mnt/pjd 2>/dev/null||true
    rm -rf /srv/flint-mds-export /srv/flint-mds-state
    mkdir -p /srv/flint-mds-export/tmp /srv/flint-mds-state /mnt/pjd
    chmod 0777 /srv/flint-mds-export/tmp
    systemd-run --unit=flint-pjd --collect --setenv=RUST_LOG=warn \
      /tmp/flint-pnfs-mds-vm --config /tmp/lite-pynfs.yaml >/dev/null 2>&1
    sleep 4
    mount -t nfs -o vers=4.1,port=$PORT,nolock 127.0.0.1:/ /mnt/pjd
    mkdir -p /mnt/pjd/tmp/pjd
    cd /mnt/pjd/tmp/pjd && timeout 2400 prove -r -f /opt/pjdfstest/tests > /tmp/pjd-flint.txt 2>&1
    echo done" >/dev/null 2>&1

echo "── arm B (control): knfsd ──"
vm "umount -f /mnt/knfsd 2>/dev/null||true
    rm -rf /srv/knfsd-export; mkdir -p /srv/knfsd-export /mnt/knfsd; chmod 0777 /srv/knfsd-export
    grep -q /srv/knfsd-export /etc/exports 2>/dev/null || \
      echo '/srv/knfsd-export 127.0.0.1/32(rw,sync,no_subtree_check,no_root_squash,fsid=7)' >> /etc/exports
    exportfs -ra; systemctl restart nfs-kernel-server; sleep 3
    mount -t nfs -o vers=4.1,nolock 127.0.0.1:/srv/knfsd-export /mnt/knfsd
    mkdir -p /mnt/knfsd/pjd
    cd /mnt/knfsd/pjd && timeout 2400 prove -r -f /opt/pjdfstest/tests > /tmp/pjd-knfsd.txt 2>&1
    echo done" >/dev/null 2>&1

TMP=$(mktemp -d)
limactl shell "$VM" -- sudo cat /tmp/pjd-flint.txt > "$TMP/flint.txt" 2>/dev/null
limactl shell "$VM" -- sudo cat /tmp/pjd-knfsd.txt > "$TMP/knfsd.txt" 2>/dev/null

python3 - "$TMP/flint.txt" "$TMP/knfsd.txt" "$BASELINE" <<'PYEOF'
import re, sys, json, os, collections
def parse(p):
    out, cur, started = {}, None, False
    for line in open(p, errors="replace").read().splitlines():
        if "Test Summary Report" in line: started = True; continue
        if not started: continue
        m = re.match(r"^(/opt/pjdfstest/tests/\S+\.t)\s+\(Wstat", line)
        if m: cur = m.group(1); out.setdefault(cur, set()); continue
        m = re.search(r"Failed tests?:\s+(.*)$", line)
        if m and cur: out[cur] |= exp(m.group(1)); continue
        if cur and re.match(r"^\s+[\d,\s-]+$", line): out[cur] |= exp(line)
    return out
def exp(s):
    g = set()
    for p in s.replace(" ", "").split(","):
        if not p: continue
        if "-" in p:
            a, _, b = p.partition("-")
            if a.isdigit() and b.isdigit(): g |= set(range(int(a), int(b)+1))
        elif p.isdigit(): g.add(int(p))
    return g

f, k, base = parse(sys.argv[1]), parse(sys.argv[2]), sys.argv[3]
nf = sum(len(v) for v in f.values()); nk = sum(len(v) for v in k.values())

# ANTI-VACUITY. A run that produced nothing parses as zero failures and
# would look like a clean sweep. knfsd is known to fail ~149; if either
# arm reports implausibly few, the run did not happen.
if nf == 0 and nk == 0:
    print("VOID: both arms reported zero failures — the suite did not run"); sys.exit(1)
if nk < 50:
    print(f"VOID: the knfsd control arm reported only {nk} failures (expected ~149).")
    print("      Either it did not run or the mount is wrong. Without a trustworthy")
    print("      control, flint's number cannot be attributed to flint.")
    sys.exit(1)

only_f = {t: f[t] - k.get(t, set()) for t in f if f[t] - k.get(t, set())}
n_only = sum(len(v) for v in only_f.values())
both = sum(len(f[t] & k.get(t, set())) for t in f)
only_k = sum(len(k[t] - f.get(t, set())) for t in k)

print(f"flint failures            : {nf}")
print(f"knfsd failures (control)  : {nk}")
print(f"failing in BOTH (NFS-generic, NOT flint's bug) : {both}")
print(f"knfsd-only (flint MORE permissive — not a win) : {only_k}")
print(f"FLINT-ONLY (the differential)                  : {n_only} across {len(only_f)} files")
area = collections.Counter()
for t, v in only_f.items(): area[t.split('/')[-2]] += len(v)
print("  " + ", ".join(f"{a}:{c}" for a, c in area.most_common(8)))

floor = None
if os.path.exists(base):
    floor = json.load(open(base)).get("flint_only_max")
if floor is None:
    print(f"\nFAIL: no baseline recorded at {base}. Record one you have INSPECTED:")
    print(f'        echo \'{{"flint_only_max": {n_only}}}\' > {base}')
    sys.exit(1)
if n_only > floor:
    print(f"\nFAIL: {n_only} flint-only failures exceeds the recorded ceiling of {floor}.")
    sys.exit(1)
print(f"\nPJDFSTEST GATE PASSED ({n_only} flint-only <= ceiling {floor})")
if n_only < floor:
    print(f"NOTE: {floor - n_only} FEWER than the ceiling — lower it in {base}.")
PYEOF
rc=$?
rm -rf "$TMP"
exit $rc
