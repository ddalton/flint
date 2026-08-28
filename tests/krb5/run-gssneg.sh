#!/bin/bash
# GSS NEGATIVE LEGS: does the server REFUSE bad Kerberos, or only accept
# good Kerberos?
#
# Every drill before this one asked whether a correct client works. The
# radar deck said so in its own foot -- "no negative legs yet" -- and that
# gap hid three defects, all found by writing these:
#
#   1. the sequence number that RESET the replay window could be replayed
#      exactly once, including the first call on a fresh context
#   2. RPCSEC_GSS_MAXSEQ (RFC 2203 §5.3.3.1) was not enforced at all
#   3. the ticket's own endtime was PARSED AND NEVER READ -- an expired
#      ticket authenticated forever, so revoking a principal at the KDC
#      did not stop anyone already holding one
#
# 1-3 are pinned by unit and interop tests (they need a clock or a crafted
# ticket, which the wire cannot supply). What runs HERE is what only a
# real client and a real KDC can show.
#
# House rules, inherited from the lean drills:
#   * every leg observes its own PRECONDITION or FAILS
#   * every refusal is paired with an ACCEPTED CONTROL on the SAME server
#     process -- a refusal leg passes just as well against a corpse
#   * no leg may pass by not looking
#
# Ports are private to this drill (2056x): two sessions share this VM, and
# a drill that kills servers by name takes the other session's with it.
set -uo pipefail
SRV=flintsrv.flint.test
EXPORT=/srv/flintexport-neg
VOL=gssneg-drill
MNT=/mnt/gssneg
BIN=${BIN:-/tmp/flint-nfs-server-neg}
PROBE="$(dirname "$0")/gssprobe.py"
ok=0; bad=0
leg() { if [ "$1" = 0 ]; then echo "  ok   $2"; ok=$((ok+1)); else echo "  BAD  $2"; bad=$((bad+1)); fi; }

SRV_PIDS=""
CAP_PID=""
cleanup() {
  sudo umount -f "$MNT" 2>/dev/null
  for p in $SRV_PIDS; do sudo kill "$p" 2>/dev/null; done
  [ -n "$CAP_PID" ] && kill "$CAP_PID" 2>/dev/null
  # NOT `pkill -f rpc.gssd`. It is a SHARED daemon: reaping it here takes
  # out any other drill on this VM, and this drill's own overlapping runs
  # did exactly that -- N1 mounted, then N3 could not, and mount.nfs4
  # reports a missing rpc.gssd as "an incorrect mount option was
  # specified", which sends you looking at the options. The same
  # kill-by-name lesson as run-pynfs-gss.sh, one daemon further out.
  return 0
}

# Self-healing precondition: a mount that needs GSS needs this alive.
ensure_gssd() {
  [ "$(pgrep -c -f rpc.gssd 2>/dev/null || echo 0)" -ge 1 ] && return 0
  sudo sh -c 'rpc.gssd -f -vvv -rrr >> /tmp/gssd-neg.log 2>&1 &'
  sleep 2
  [ "$(pgrep -c -f rpc.gssd 2>/dev/null || echo 0)" -ge 1 ]
}
trap cleanup EXIT

# Sets SRV_PID. NOT `p=$(start ...)`: command substitution runs the
# function in a subshell, so the PID would never reach the cleanup trap
# and this drill would leak servers onto a VM another session is using.
SRV_PID=""
start() { # $1=keytab $2=port $3=log
  sudo env KRB5_KTNAME="$1" RUST_LOG=info "$BIN" \
    --export-path "$EXPORT" --volume-id "$VOL" --bind-addr 0.0.0.0 --port "$2" \
    > "$3" 2>&1 &
  SRV_PID=$!
  SRV_PIDS="$SRV_PIDS $SRV_PID"
  sleep 4
}
# Liveness by PORT, not just by process: `kill -0` on the sudo wrapper
# says the wrapper lives, which is not the claim these controls make.
listening() { ss -ltn 2>/dev/null | grep -qE "[:.]$1[[:space:]]"; }
# Returns 0 only if $MNT is mounted AND on the port asked for.
#
# `mountpoint -q` alone is not that claim. When the previous leg's umount
# lost a race the old mount survived, the new mount never happened, and
# `mountpoint -q` cheerfully reported success -- so N3 "mounted through
# the recording proxy" while every byte went straight to the server, and
# the capture came back empty. Checking the port in /proc/mounts kills
# that whole class in one place.
try_mount() { # $1=flavor $2=port
  ensure_gssd || { echo "    (rpc.gssd would not start)"; return 1; }
  sudo umount -f "$MNT" 2>/dev/null
  mountpoint -q "$MNT" && sudo umount -l "$MNT" 2>/dev/null
  sleep 1
  if mountpoint -q "$MNT"; then
    echo "    (a stale mount on $MNT would not clear)"
    return 1
  fi
  sudo timeout 40 mount -t nfs4 -o vers=4.1,sec="$1",port="$2",soft,timeo=25,retrans=1 \
    "$SRV:/" "$MNT" >/dev/null 2>&1
  mountpoint -q "$MNT" && [ "$(grep -c "port=$2" /proc/mounts)" -ge 1 ]
}

sudo mkdir -p "$EXPORT/.flint-nfs" "$MNT"
echo -n "$VOL" | sudo tee "$EXPORT/.flint-nfs/volume-id" >/dev/null
echo gssneg-probe | sudo tee "$EXPORT/probe.txt" >/dev/null
ensure_gssd; leg $? "PRECONDITION: rpc.gssd is running"
echo flintflint | kinit testuser@FLINT.TEST >/dev/null 2>&1
klist >/dev/null 2>&1; leg $? "PRECONDITION: testuser holds a TGT"

# ── N1: a service key the KDC never issued against ───────────────────
# The KDC mints a ticket for nfs/flintsrv encrypted to the REAL service
# key. Hand the server a keytab for the same principal with a DIFFERENT
# key and the ticket must not decrypt.
echo "--- N1: wrong service key ---"
# The keytab is COPIED with its key bytes flipped -- same principal, same
# enctype, same kvno, wrong key. Rotating the principal at the KDC would
# test the same thing and would also invalidate the keytab every other
# drill on this shared VM depends on.
sudo rm -f /tmp/wrong.keytab
# sudo: a keytab is not world-readable, and without it this step fails
# silently into "server started with NO keytab" -- which refuses the mount
# too, for entirely the wrong reason. N1a would have been green over
# nothing; only this precondition caught it.
sudo python3 "$(dirname "$0")/ktcorrupt.py" /tmp/flint-nfs.keytab /tmp/wrong.keytab 2>&1 | sed 's/^/    /'
[ -s /tmp/wrong.keytab ]; leg $? "N1 PRECONDITION: built a same-name wrong-key keytab"
# ...and it must name the SAME principal, or the server fails at lookup
# rather than at decryption, which is a different claim entirely.
REALP=$(sudo klist -k /tmp/flint-nfs.keytab 2>/dev/null | awk "NR>3{print \$2}" | sort -u | head -1)
WRONGP=$(sudo klist -k /tmp/wrong.keytab 2>/dev/null | awk "NR>3{print \$2}" | sort -u | head -1)
[ -n "$REALP" ] && [ "$REALP" = "$WRONGP" ]
leg $? "N1 PRECONDITION: it names the SAME principal ($WRONGP) — so N1a tests the KEY, not the lookup"
start /tmp/wrong.keytab 20560 /tmp/n1-wrong.log
listening 20560; leg $? "N1 PRECONDITION: server with the wrong-key keytab is LISTENING"
kdestroy >/dev/null 2>&1; echo flintflint | kinit testuser@FLINT.TEST >/dev/null 2>&1
try_mount krb5i 20560; [ $? -ne 0 ]; leg $? "N1a: sec=krb5i REFUSED when the service key does not match"
listening 20560; leg $? "N1b: and the server SURVIVED it (a crash would also refuse)"
sudo umount -f "$MNT" 2>/dev/null

# ANTI-VACUITY: same binary, same KDC, the UNMODIFIED keytab -> must mount.
start /tmp/flint-nfs.keytab 20561 /tmp/n1-right.log
kdestroy >/dev/null 2>&1; echo flintflint | kinit testuser@FLINT.TEST >/dev/null 2>&1
try_mount krb5i 20561; leg $? "N1c: ACCEPTED CONTROL — the CURRENT key mounts (so N1a was the key, not the rig)"
got=$(sudo timeout 20 cat "$MNT/probe.txt" 2>&1); [ "$got" = "gssneg-probe" ]
leg $? "N1d: and it serves data (got: $got)"

# ── N2: a GSS context handle the server never issued ─────────────────
echo "--- N2: unknown GSS context handle ---"
# Recorded from the live mount above, then replayed on a fresh connection
# after the context is destroyed -- see N3. First the cheap one: bytes
# that claim a handle out of thin air.
python3 - "$MNT" <<'PY' > /tmp/n2.out 2>&1
import socket, struct, sys
# RPC CALL, NFS prog 100003 v4 proc 0 (NULL), cred = RPCSEC_GSS v1 DATA
# with a handle the server has never seen.
handle = b"\xde\xad\xbe\xef" * 4
cred = struct.pack(">IIII", 1, 0, 1, 1) + struct.pack(">I", len(handle)) + handle
body = struct.pack(">IIIIII", 0x4e454741, 0, 2, 100003, 4, 0)
body += struct.pack(">II", 6, len(cred)) + cred
body += struct.pack(">II", 0, 0)          # empty verifier
rec = struct.pack(">I", 0x80000000 | len(body)) + body
s = socket.create_connection(("127.0.0.1", 20561), timeout=8)
s.sendall(rec)
try:
    head = s.recv(4)
except socket.timeout:
    print("NO_REPLY"); sys.exit()
if len(head) < 4:
    print("CLOSED"); sys.exit()
n = struct.unpack(">I", head)[0] & 0x7fffffff
b = b""
while len(b) < n:
    c = s.recv(n - len(b))
    if not c: break
    b += c
xid, mt, stat = struct.unpack(">III", b[:12])
if stat == 1 and struct.unpack(">I", b[12:16])[0] == 1:
    print("DENIED auth_stat=%d" % struct.unpack(">I", b[16:20])[0])
else:
    print("NOT_DENIED stat=%d" % stat)
PY
cat /tmp/n2.out | sed 's/^/    /'
grep -q "DENIED auth_stat=" /tmp/n2.out; leg $? "N2a: an unknown context handle is DENIED at the auth layer"
# RPCSEC_GSS_CREDPROBLEM=13, RPCSEC_GSS_CTXPROBLEM=14 (RFC 2203 §5.3.3.3)
grep -qE "auth_stat=(13|14)" /tmp/n2.out; leg $? "N2b: and with a GSS auth_stat that tells the client to re-init"
listening 20561; leg $? "N2c: server survived the bogus handle"
mountpoint -q "$MNT"; leg $? "N2d: ACCEPTED CONTROL — the real mount is still serving on the same process"

# ── N3: a REPLAY of a real, correctly-signed RPC ─────────────────────
# Nothing here forges crypto: the record is captured verbatim from the
# kernel client, which is exactly what an attacker on the wire has.
echo "--- N3: wire replay of a captured GSS RPC ---"
sudo umount -f "$MNT" 2>/dev/null
start /tmp/flint-nfs.keytab 20562 /tmp/n3.log
P3=$SRV_PID
rm -f /tmp/gssrec.bin /tmp/cap.log
# A proxy leaked by an aborted run keeps 20563 bound. Ours then dies with
# EADDRINUSE while the STALE one forwards the traffic -- and, having
# already captured on its own earlier run, saves nothing. "Is something
# listening on 20563" cannot tell the two apart, so it reported ready and
# the capture came back empty. Reap first, then prove it is OURS.
pkill -f "gssprobe.py capture" 2>/dev/null; sleep 1
python3 "$PROBE" capture 20563 20562 /tmp/gssrec.bin > /tmp/cap.log 2>&1 &
CAP_PID=$!
for _ in $(seq 1 20); do listening 20563 && break; sleep 0.5; done
kill -0 "$CAP_PID" 2>/dev/null && [ "$(grep -c Traceback /tmp/cap.log 2>/dev/null)" -eq 0 ] && listening 20563
leg $? "N3 PRECONDITION: OUR recording proxy owns 20563 (pid $CAP_PID, no bind error)"
kdestroy >/dev/null 2>&1; echo flintflint | kinit testuser@FLINT.TEST >/dev/null 2>&1
try_mount krb5i 20563; leg $? "N3 PRECONDITION: mounted THROUGH the recording proxy"
sudo timeout 20 ls "$MNT" >/dev/null 2>&1
sudo timeout 20 cat "$MNT/probe.txt" >/dev/null 2>&1
sleep 1
[ -s /tmp/gssrec.bin ]; leg $? "N3a: captured a real RPCSEC_GSS DATA record ($(stat -c%s /tmp/gssrec.bin 2>/dev/null || echo 0) bytes)"
grep -o "seq=[0-9]*" /tmp/cap.log | head -1 | sed 's/^/    captured /'

# The captured call already reached the server once, through the proxy.
# Sending the identical bytes again re-uses its seq_num on a context that
# has seen it.
python3 "$PROBE" send 20562 /tmp/gssrec.bin > /tmp/n3-replay.out 2>&1
cat /tmp/n3-replay.out | sed 's/^/    replay: /'
grep -q "DENIED" /tmp/n3-replay.out; leg $? "N3b: the replayed record is DENIED"
grep -qE "auth_stat=(13|14)" /tmp/n3-replay.out; leg $? "N3c: and denied as a GSS credential/context problem"

# ── N4: a FRESH sequence number with a stale checksum ────────────────
# N3 alone cannot show the MIC is checked: a captured record's seq_num is
# already spent, so the replay window explains the refusal on its own.
# Move the seq_num forward to a number the context has never seen -- the
# window accepts it -- and leave the call verifier untouched. The verifier
# is a MIC over the seq_num (RFC 2203 §5.3.1), so it no longer matches,
# and nothing but checksum verification can refuse this.
echo "--- N4: fresh seq_num, stale verifier ---"
CAPSEQ=$(python3 "$PROBE" seqof /tmp/gssrec.bin 2>/dev/null || echo 0)
[ "$CAPSEQ" -gt 0 ]; leg $? "N4 PRECONDITION: read the captured seq_num ($CAPSEQ)"
FRESH=$((CAPSEQ + 1000))
python3 "$PROBE" send 20562 /tmp/gssrec.bin --seq "$FRESH" > /tmp/n4.out 2>&1
cat /tmp/n4.out | sed "s/^/    seq $FRESH: /"
grep -qE "DENIED|NO_REPLY|CLOSED" /tmp/n4.out; leg $? "N4a: an unseen seq_num with a stale MIC is refused (replay cannot explain it)"
listening 20562; leg $? "N4b: server survived it"

# ── N5: can a keyless peer MOVE the replay window? ───────────────────
# The consequence leg for N4, and the reason the ordering bug was found.
#
# `verify_sequence` ADVANCES last_seq_num. Run it before the checksum is
# verified and anyone holding a captured record -- no key material at all
# -- can park the window wherever they like, simply by rewriting the
# seq_num and accepting that the MIC check will reject the call. Genuine
# traffic on that context then falls outside the window.
#
# TWO EARLIER ORACLES FOR THIS WERE WRONG, both of which passed against a
# server that HAD the bug:
#   * "does the mount still serve?" -- it does. CREDPROBLEM tells the
#     client to re-init and the kernel does, so the read succeeds through
#     a fresh context and the damage never shows.
#   * "did the kernel client hit it?" -- only if its next call happens to
#     use the poisoned context. A mount holds one for the machine
#     credential and one per user; which one the capture belongs to, and
#     whether it is used again, is luck. Twice it was not.
# So BOTH sides are driven from here. Two probes on one context, and the
# question is only what the server says about the second.
echo "--- N5: does a refused injection move the window? ---"
sudo timeout 20 cat "$MNT/probe.txt" >/dev/null 2>&1
leg $? "N5 PRECONDITION: the mount still serves before the probes"

# Probe A: a wild seq_num with a stale MIC. Refused either way -- the
# question is what it leaves behind.
python3 "$PROBE" send 20562 /tmp/gssrec.bin --seq $((CAPSEQ + 500000)) > /tmp/n5a.out 2>&1
[ "$(grep -c DENIED /tmp/n5a.out)" -ge 1 ]
leg $? "N5a: probe A (seq +500000, stale MIC) is refused — it holds no key"

# Probe B: a seq_num a few above the capture. It is inside any sane
# window -- UNLESS probe A moved the window to +500000, in which case it
# is 499,999 short of it.
python3 "$PROBE" send 20562 /tmp/gssrec.bin --seq $((CAPSEQ + 2)) > /tmp/n5b.out 2>&1
cat /tmp/n5b.out | sed 's/^/    probe B: /'
[ "$(grep -c DENIED /tmp/n5b.out)" -ge 1 ]
leg $? "N5b ANTI-VACUITY: probe B reached the server and was answered"

sudo timeout 25 cat "$MNT/probe.txt" >/dev/null 2>&1
leg $? "N5c: the honest mount still serves after both probes"
listening 20562; leg $? "N5d: server survived"

# The log is the oracle, and tracing buffers it: counting while the
# server runs raced the flush and read clean on a run whose log did hold
# the line. Stop it first.
sudo kill "$P3" 2>/dev/null; sleep 3
OUTSIDE=$(grep -c "outside window" /tmp/n3.log 2>/dev/null || true); OUTSIDE=${OUTSIDE:-0}
echo "    server refused $OUTSIDE call(s) as 'outside window'"
grep -m2 -oE "Replay detected: .*" /tmp/n3.log | sed 's/^/    /'
[ "$OUTSIDE" -eq 0 ]
leg $? "N5e: probe B was refused for its MIC, NOT for a moved window ($OUTSIDE outside-window refusals) — a keyless peer cannot move the replay window"

echo
echo "=== $ok ok, $bad bad ==="
[ "$bad" -eq 0 ]
