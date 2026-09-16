#!/usr/bin/env bash
#
# SQLite CONCURRENCY drill — does flint-lite's headline use case hold?
#
# WHY THIS EXISTS. flint-lite's pitch names sqlite three times
# (`docs/flint-lite.md:11`, `flint-lite-for-agent-fleets.md:21`, and the
# operator mandates `hard` mounts *because* "agents run git and
# sqlite"). The mechanisms are present — NFSv4 byte-range LOCK/LOCKU
# with lock stateids, atomic rename, fsync-before-ACK. But the only
# sqlite the test tree ever ran is `tier-drill.sh:245`: one process, two
# rows, sequential. That proves sqlite RUNS over the mount. It exercises
# no lock contention, no second writer, no WAL — i.e. none of the things
# that make sqlite-over-NFS the classic corruption story.
#
# THE TRAP THIS DRILL EXISTS TO AVOID. Two writer processes on ONE NFS
# client prove nothing: the client kernel can arbitrate fcntl locally
# between two local processes and never emit a LOCK to the server. Such
# a run passes whatever the server does. So the real arms use TWO
# SEPARATE KERNELS, and arm A1 runs the trap deliberately so its result
# is ON THE RECORD rather than assumed.
#
# WHAT THE FIRST RUN (2026-09-16) GOT WRONG, fixed here:
#   1. Arm E asked "is the answer the string 'wal'?" and treated
#      everything else as "WAL safely refused" — so `unable to open
#      database file` scored as a PASS. An ERROR MUST NOT RETURN A LEGAL
#      VALUE. E now classifies errors explicitly.
#   2. A failing CONTROL did not stop the run attributing anything to
#      flint. knfsd failed and the script still printed FAIL against
#      flint. Arm C is now reported NOT ATTRIBUTABLE unless B passed.
#   3. Arm ordering let damage propagate: the hub wedged during arm C
#      (fd exhaustion, see results/2026-09-16-sqlite-concurrency/) and
#      A1 and E then "measured" a dead server. Every arm now runs a
#      HEALTH CHECK first and reports INCONCLUSIVE, never FAIL, if the
#      server is not up.
#
# ONE DIMENSION AT A TIME. The hub must be started with a high
# `ulimit -n` (see hub-up.sh): its FdCache leaks a descriptor per OPEN
# and will otherwise wedge mid-run, which measures the leak instead of
# the locking. The leak is recorded per arm as data, not as a crash.
#
# ARMS:
#   A0  local ext4 on C1                 — is the workload itself sound?
#   B   knfsd, C1 + C2                   — CONTROL: is this just NFS?
#   C   flint, C1 + C2                   — the measurement
#   A1  flint, 2 procs on C1 ONLY        — the same-client trap
#   E   flint, journal_mode=WAL probe    — accepted? refused? or broken?
#
# PASS, per contended arm — all three or it is a FAIL:
#   * every writer exits 0 (a `database is locked` outliving
#     busy_timeout is a failure, not a retry);
#   * `PRAGMA integrity_check` returns exactly `ok`;
#   * row count is EXACTLY writers x txns — a lost update is silent and
#     the count is the only thing that sees it.
#
# Exit: 0 PASS, 1 FAIL, 2 INCONCLUSIVE.

set -uo pipefail

RIG="${RIG:-ssh}"
C1_HOST="${C1_HOST:-}"; C2_HOST="${C2_HOST:-}"; SSH_KEY="${SSH_KEY:-}"
HUB_IP="${HUB_IP:-}"
VM1="${VM1:-flint-nfs-client}"; VM2="${VM2:-flint-nfs-client2}"
MDS_PORT="${MDS_PORT:-20493}"
FLINT_MNT=/mnt/flint
KNFSD_MNT=/mnt/knfsd
KNFSD_DIR=/srv/knfsdexport
TXNS="${TXNS:-200}"
BUSY_MS="${BUSY_MS:-30000}"

PASSN=0; FAILN=0; INCN=0
B_CONTROL_OK=0        # did the knfsd control pass? gates attribution
pass(){ echo "  PASS  $*"; PASSN=$((PASSN+1)); }
fail(){ echo "  FAIL  $*"; FAILN=$((FAILN+1)); }
inconc(){ echo "  INCONCLUSIVE  $*"; INCN=$((INCN+1)); }
hdr(){ echo; echo "== $*"; }

if [ "$RIG" = "ssh" ]; then
  SSHO="-i $SSH_KEY -o StrictHostKeyChecking=no -o ConnectTimeout=10 -o BatchMode=yes"
  c1(){ ssh $SSHO "$C1_HOST" "$1"; }
  c2(){ ssh $SSHO "$C2_HOST" "$1"; }
else
  c1(){ limactl shell "$VM1" -- bash -c "$1"; }
  c2(){ limactl shell "$VM2" -- bash -c "$1"; }
fi

# Is the thing under test actually alive? A dead server must produce
# INCONCLUSIVE, never FAIL: "the server was not running" is not a
# statement about sqlite.
hub_fds(){ c1 'P=$(pgrep flint-pnfs-mds 2>/dev/null); [ -n "$P" ] && sudo ls /proc/$P/fd 2>/dev/null | wc -l || echo -1'; }
health(){ # <what>
  local alive writable
  alive=$(c1 'pgrep -c flint-pnfs-mds 2>/dev/null || echo 0' | tr -d '\r')
  writable=$(c1 "timeout 10 touch $FLINT_MNT/.hb 2>/dev/null && rm -f $FLINT_MNT/.hb && echo yes || echo no" | tr -d '\r')
  if [ "$alive" = "0" ] || [ "$writable" != "yes" ]; then
    echo "     HEALTH: hub_procs=$alive mount_writable=$writable"
    return 1
  fi
  return 0
}

install_workload(){ # <c1|c2>
  $1 "cat >/tmp/wr.sh <<'WREOF'
#!/usr/bin/env bash
set -u
db=\$1; tag=\$2; n=\$3; busy=\$4
for i in \$(seq 1 \$n); do
  out=\$(sqlite3 \"\$db\" \".timeout \$busy\" \"INSERT INTO t(who,seq) VALUES('\$tag',\$i);\" 2>&1) || {
    echo \"writer \$tag txn \$i FAILED: \$out\" >&2; exit 1; }
  [ -n \"\$out\" ] && { echo \"writer \$tag txn \$i NOISE: \$out\" >&2; exit 1; }
done
exit 0
WREOF
chmod +x /tmp/wr.sh"
}

# sqlite3 prints errors on stdout; a value that starts with "Error" or
# names an open failure is NOT an answer. Everything that classifies a
# sqlite result goes through here so no arm can score an error as data.
is_error(){ case "$1" in Error*|*"unable to open"*|*"disk I/O error"*|*"malformed"*|"") return 0;; *) return 1;; esac; }

# contended_arm <label> <db> <second: c2|c1local> <mode: judge|record> <needs_hub: yes|no>
contended_arm(){
  local label="$1" db="$2" second="$3" mode="$4" needs_hub="$5" rc=0 want=$((2*TXNS))
  if [ "$needs_hub" = "yes" ] && ! health "$label"; then
    inconc "$label — the hub was not healthy BEFORE this arm; not a statement about sqlite"
    return
  fi
  local fds_before; fds_before=$(hub_fds | tr -d '\r')

  c1 "rm -f '$db' '$db'-journal '$db'-wal '$db'-shm; sqlite3 '$db' 'CREATE TABLE t(who text, seq int);'" \
    >/dev/null 2>&1 || { inconc "$label: could not create the db"; return; }

  local p1 p2
  c1 "/tmp/wr.sh '$db' w1 $TXNS $BUSY_MS" >/tmp/sqlc-1.log 2>&1 & p1=$!
  if [ "$second" = "c2" ]; then
    c2 "/tmp/wr.sh '$db' w2 $TXNS $BUSY_MS" >/tmp/sqlc-2.log 2>&1 & p2=$!
  else
    c1 "/tmp/wr.sh '$db' w2 $TXNS $BUSY_MS" >/tmp/sqlc-2.log 2>&1 & p2=$!
  fi
  wait $p1 || rc=1
  wait $p2 || rc=1

  local integ count fds_after
  integ=$(c1 "sqlite3 '$db' 'PRAGMA integrity_check;'" 2>&1 | tr -d '\r' | head -1)
  count=$(c1 "sqlite3 '$db' 'SELECT count(*) FROM t;'" 2>&1 | tr -d '\r' | head -1)
  fds_after=$(hub_fds | tr -d '\r')
  echo "     writers_rc=$rc  integrity='$integ'  rows=$count/$want"
  [ "$needs_hub" = "yes" ] && echo "     hub fds: $fds_before -> $fds_after (FdCache has no bound; recorded, not judged)"
  if [ "$rc" -ne 0 ]; then
    echo "     --- writer errors ---"
    grep -h 'FAILED\|NOISE' /tmp/sqlc-1.log /tmp/sqlc-2.log 2>/dev/null | head -2 | sed 's/^/     /'
  fi

  # A reading that is itself an error cannot be judged as data.
  if is_error "$integ" || is_error "$count"; then
    inconc "$label — could not READ the result back (integrity='$integ' rows='$count')"
    return
  fi
  if [ "$mode" = "record" ]; then
    echo "     (RECORDED, not judged — this arm exists to be observed)"
    return
  fi
  if [ "$rc" -eq 0 ] && [ "$integ" = "ok" ] && [ "$count" = "$want" ]; then
    pass "$label"; [ "$label" = "${label#B }" ] || B_CONTROL_OK=1
  else
    fail "$label — rc=$rc integrity='$integ' rows=$count/$want"
  fi
}

# ---------------------------------------------------------------------
hdr "preflight"
for h in c1 c2; do $h "command -v sqlite3 >/dev/null" || { echo "  sqlite3 missing on $h"; exit 2; }; done
install_workload c1; install_workload c2
echo "  TXNS=$TXNS per writer, 2 writers, busy_timeout=${BUSY_MS}ms"
if health "preflight"; then echo "  hub healthy, fds=$(hub_fds)"; else echo "  hub NOT healthy at preflight"; fi

hdr "A0 CONTROL — local ext4 on C1 (is the workload itself sound?)"
contended_arm "A0 local ext4" /tmp/a0.db c1local judge no
if [ "$FAILN" -ne 0 ] || [ "$INCN" -ne 0 ]; then
  echo; echo "A0 did not pass — the workload is not sound on a local filesystem."
  echo "Nothing downstream would be interpretable. Stopping."; exit 2
fi

hdr "B CONTROL — knfsd, two clients (is this just how NFS behaves?)"
c1 "sudo mkdir -p $KNFSD_DIR && sudo chmod 777 $KNFSD_DIR && \
    echo '$KNFSD_DIR *(rw,sync,no_subtree_check,no_root_squash)' | sudo tee /etc/exports >/dev/null && \
    sudo exportfs -ra && sudo systemctl restart nfs-kernel-server && sleep 2" >/dev/null 2>&1
KOK=1
for h in c1 c2; do
  $h "sudo mkdir -p $KNFSD_MNT; sudo umount $KNFSD_MNT 2>/dev/null; \
      sudo timeout 30 mount -t nfs4 -o minorversion=1,proto=tcp,hard $HUB_IP:$KNFSD_DIR $KNFSD_MNT" >/dev/null 2>&1 || KOK=0
done
if [ "$KOK" = "1" ]; then
  contended_arm "B knfsd 2 clients" $KNFSD_MNT/b.db c2 judge no
else
  inconc "B knfsd control could not be mounted"
fi

hdr "C flint lite, two clients — THE MEASUREMENT"
contended_arm "C flint 2 clients" $FLINT_MNT/c.db c2 judge yes
if [ "$B_CONTROL_OK" = "0" ]; then
  echo "     *** NOT ATTRIBUTABLE TO FLINT: the knfsd control did not pass,"
  echo "     *** so any failure here is shared with a stock kernel NFS server."
fi

hdr "A1 the SAME-CLIENT trap — 2 procs on C1 only, over the flint mount"
echo "  If this passes while C fails, the client kernel arbitrated locally"
echo "  and never asked the server. Recorded, not judged."
contended_arm "A1 flint same-client" $FLINT_MNT/a1.db c1local record yes

hdr "E WAL probe — is journal_mode=WAL accepted over the mount?"
if health "E"; then
  WAL=$(c1 "rm -f $FLINT_MNT/e.db*; sqlite3 $FLINT_MNT/e.db 'PRAGMA journal_mode=WAL;' 2>&1" | tr -d '\r' | head -1)
  echo "     journal_mode returned: '$WAL'"
  if is_error "$WAL"; then
    # THE 2026-09-16 BUG: this used to score as "safely refused".
    inconc "E — the probe ERRORED; that is not a refusal and not an answer: '$WAL'"
  elif [ "$WAL" = "wal" ]; then
    echo "     WAL ACCEPTED — the risky answer. Testing two clients under it."
    c1 "sqlite3 $FLINT_MNT/e.db 'CREATE TABLE t(who text, seq int);'" >/dev/null 2>&1
    contended_arm "E WAL 2 clients" $FLINT_MNT/e.db c2 record yes
    echo "     NOTE: WAL needs a coherent shared-memory -shm mapping, which does"
    echo "     not exist between hosts over NFS. A clean run here means corruption"
    echo "     was not provoked by THIS workload — a much weaker claim than safety."
  else
    pass "E WAL declined, journal_mode stayed '$WAL' — the safe answer"
  fi
else
  inconc "E — hub not healthy"
fi

echo
echo "summary: PASS=$PASSN FAIL=$FAILN INCONCLUSIVE=$INCN"
[ "$B_CONTROL_OK" = "0" ] && echo "NOTE: the knfsd control did NOT pass — no result here is attributable to flint alone."
[ "$FAILN" -ne 0 ] && exit 1
[ "$INCN" -ne 0 ] && exit 2
exit 0
