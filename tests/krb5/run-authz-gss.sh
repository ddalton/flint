#!/bin/bash
# Does a Kerberos identity carry any RIGHTS? The permission-enforcement
# drill over RPCSEC_GSS.
#
# The claim under test (deck security plate row 7, radar Security/Rights,
# scored from code 2026-09-05 with no verifier): a `sec=krb5p` COMPOUND
# carries no unix credential (`compound.rs` `unix_cred: None` under
# AUTH_NONE/GSS), so `authz::check` returns Ok having evaluated nothing —
# even with FLINT_NFS_ENFORCE_PERMISSIONS=enforce. krb5p proves who you
# are and derives no rights from it.
#
# The shape: one server, ENFORCE mode, one uid, one set of files; two
# mounts that differ ONLY in `sec=`. Under sec=sys the uid is a claim and
# the mode bits are evaluated against it — that arm is the CONTROL and
# must be DENIED, or Enforce is not live and the krb5p arm proves
# nothing. Under sec=krb5p the same uid, holding a real TGT, makes the
# same requests.
#
# Verdict logic (printed at the end):
#   CONFIRMED    control denied; krb5p reads the 0600 file and/or its
#                write lands, with NO "DENIED" line from the server
#   REFUTED      control denied; krb5p is denied AND the server logged
#                the denial (it evaluated a credential under GSS)
#   INCONCLUSIVE anything else — most likely the krb5p arm's own control
#                (a world-readable read) failed, i.e. no GSS context for
#                the uid, which is a rig fault and not a server answer
#
# Run INSIDE the Lima VM, as the ordinary (non-root) user, after
# setup-kdc.sh and setup-mount.sh:
#   BIN=/path/to/flint-nfs-server bash run-authz-gss.sh
set -uo pipefail
BIN=${BIN:-/tmp/flint-nfs-server}
PORT=${PORT:-20497}
SRV=flintsrv.flint.test
EXPORT=/srv/flintauthz
VOL=authz-drill
LOG=/tmp/flint-authz.log
GSSLOG=/tmp/gssd-authz.log
ME=$(id -u)
OTHER=1001           # owns every file; must not be $ME and must not be 0
ok=0; bad=0
leg() { if [ "$1" = 0 ]; then echo "  ok   $2"; ok=$((ok+1)); else echo "  BAD  $2"; bad=$((bad+1)); fi; }
note() { echo "  ..   $*"; }
t() { timeout 20 "$@"; }

[ "$ME" != 0 ] || { echo "run as a non-root user: uid 0 is never squashed, so the control arm cannot deny"; exit 2; }
[ "$ME" != "$OTHER" ] || { echo "uid $ME owns the files; pick another OTHER"; exit 2; }
[ -x "$BIN" ] || { echo "no server binary at $BIN"; exit 2; }
echo "server: $BIN"
echo "caller: uid $ME ($(id -un)); file owner: uid $OTHER; port $PORT"

# ── the export: three files owned by someone else, one open directory ──
sudo umount -f /mnt/authz-sys /mnt/authz-krb5p 2>/dev/null
sudo rm -rf "$EXPORT"
sudo mkdir -p "$EXPORT/.flint-nfs" "$EXPORT/open"
echo -n "$VOL" | sudo tee "$EXPORT/.flint-nfs/volume-id" >/dev/null
echo world   | sudo tee "$EXPORT/world.txt"   >/dev/null
echo secret  | sudo tee "$EXPORT/secret.txt"  >/dev/null
echo scratch | sudo tee "$EXPORT/scratch.txt" >/dev/null
sudo chown "$OTHER:$OTHER" "$EXPORT"/*.txt
sudo chmod 0644 "$EXPORT/world.txt" "$EXPORT/scratch.txt"
sudo chmod 0600 "$EXPORT/secret.txt"
sudo chmod 0777 "$EXPORT" "$EXPORT/open"
echo "export:"; sudo ls -ln "$EXPORT" | sed 's/^/    /'

# ── rpc.gssd, verbose and in the foreground so refusals are legible ──
sudo systemctl stop rpc-gssd 2>/dev/null; sudo pkill -f rpc.gssd 2>/dev/null; sleep 1
sudo rm -f "$GSSLOG"
sudo sh -c "rpc.gssd -f -vvv -rrr > $GSSLOG 2>&1 &"
sleep 2
pgrep -f rpc.gssd >/dev/null; leg $? "rpc.gssd is running"

# ── the server, in ENFORCE mode ──
sudo pkill -f "flint-nfs-server.*--port $PORT" 2>/dev/null; sleep 1
sudo rm -f "$LOG"
sudo env KRB5_KTNAME=/tmp/flint-nfs.keytab FLINT_NFS_ENFORCE_PERMISSIONS=enforce RUST_LOG=info \
  "$BIN" --export-path "$EXPORT" --volume-id "$VOL" --bind-addr 0.0.0.0 --port "$PORT" > "$LOG" 2>&1 &
sleep 4
pgrep -f "flint-nfs-server.*--port $PORT" >/dev/null; leg $? "flint-nfs-server is listening on $PORT with FLINT_NFS_ENFORCE_PERMISSIONS=enforce"
note "server on the mode: $(sudo grep -i -m1 -E 'enforc|permission' "$LOG" || echo '(no line names it)')"

# ── the caller's ticket ──
echo flintflint | kinit testuser@FLINT.TEST >/dev/null 2>&1
klist -s; leg $? "uid $ME holds a TGT for testuser in $(klist 2>/dev/null | sed -n 's/^Ticket cache: //p')"

denied_count() { sudo grep -c "DENIED" "$LOG" 2>/dev/null || true; }
classify() {  # stdin: a command's combined output+status via $?; args: rc, output
  local rc=$1 out=$2
  if [ "$rc" = 0 ]; then echo ok
  elif echo "$out" | grep -qi "permission denied"; then echo denied
  else echo "other($rc:$(echo "$out" | tail -1 | cut -c1-60))"; fi
}

declare -A WORLD SECRET WRITE OWNER DENIED
arm() {  # $1 = sys | krb5p
  local sec=$1 mnt=/mnt/authz-$1 d0 d1 out rc
  echo; echo "──── arm sec=$sec: uid $ME against files owned by $OTHER ────"
  sudo mkdir -p "$mnt"
  sudo timeout 60 mount -t nfs4 -o "vers=4.1,sec=$sec,port=$PORT,soft,timeo=50,retrans=1" "$SRV:/" "$mnt" 2>&1 | tail -3
  mountpoint -q "$mnt"; leg $? "[$sec] mounted"
  mountpoint -q "$mnt" || return
  grep " $mnt " /proc/mounts | grep -q "sec=$sec"; leg $? "[$sec] /proc/mounts really says sec=$sec"
  d0=$(denied_count); d0=${d0:-0}

  out=$(t cat "$mnt/world.txt" 2>&1); rc=$?
  WORLD[$sec]=$(classify $rc "$out")
  [ "$out" = world ]; leg $? "[$sec] CONTROL: the 0644 file reads (${WORLD[$sec]}) — the mount and, under GSS, the uid's context work"

  out=$(t cat "$mnt/secret.txt" 2>&1); rc=$?
  SECRET[$sec]=$(classify $rc "$out")
  note "[$sec] read of the 0600 file owned by $OTHER: ${SECRET[$sec]}"

  out=$(t sh -c "echo appended-by-$sec >> $mnt/scratch.txt" 2>&1); rc=$?
  if sudo grep -q "appended-by-$sec" "$EXPORT/scratch.txt"; then WRITE[$sec]=landed
  else WRITE[$sec]=$(classify $rc "$out"); [ "${WRITE[$sec]}" = ok ] && WRITE[$sec]="ok-but-not-on-disk"; fi
  note "[$sec] write to the 0644 file owned by $OTHER: ${WRITE[$sec]}"

  out=$(t sh -c "echo hi > $mnt/open/made-by-$sec" 2>&1); rc=$?
  if [ -e "$EXPORT/open/made-by-$sec" ]; then OWNER[$sec]=$(sudo stat -c %u "$EXPORT/open/made-by-$sec")
  else OWNER[$sec]="(create failed: $(classify $rc "$out"))"; fi
  note "[$sec] a file created in the 0777 directory is owned on the server by uid ${OWNER[$sec]} (caller is $ME)"

  if [ "$sec" = sys ]; then
    # setpriv, not `sudo -u "#$OTHER"`: sudo refuses a numeric uid with
    # no passwd entry ("unknown user #1001"), and this leg was red for
    # exactly that reason on its first run — a rig fault dressed as a
    # server one.
    out=$(sudo setpriv --reuid="$OTHER" --regid="$OTHER" --clear-groups timeout 20 cat "$mnt/secret.txt" 2>&1)
    [ "$out" = secret ]; leg $? "[$sec] CONTROL: the OWNER (uid $OTHER) reads the 0600 file — the denial is about identity, not the file"
  fi

  sleep 1
  d1=$(denied_count); d1=${d1:-0}
  DENIED[$sec]=$((d1 - d0))
  note "[$sec] server DENIED lines during this arm: ${DENIED[$sec]}"
  sudo umount -f "$mnt" 2>/dev/null || sudo umount -l "$mnt" 2>/dev/null
}

arm sys
arm krb5p

# ── the control arm must have denied, or nothing below means anything ──
echo
[ "${SECRET[sys]:-}" = denied ]; leg $? "[sys] CONTROL: uid $ME is DENIED the 0600 file (Enforce is live)"
[ "${WRITE[sys]:-}" = denied ];  leg $? "[sys] CONTROL: uid $ME is DENIED the write to a 0644 file it does not own"
[ "${DENIED[sys]:-0}" -ge 2 ];   leg $? "[sys] CONTROL: the server logged the denials (${DENIED[sys]:-0} lines)"
[ "${OWNER[sys]:-}" = "$ME" ];   leg $? "[sys] CONTROL: a file created under sys is stamped with the caller's uid"

verdict=INCONCLUSIVE
if [ "${SECRET[sys]:-}" = denied ] && [ "${WRITE[sys]:-}" = denied ] && [ "${WORLD[krb5p]:-}" = ok ]; then
  if { [ "${SECRET[krb5p]:-}" = ok ] || [ "${WRITE[krb5p]:-}" = landed ]; } && [ "${DENIED[krb5p]:-0}" = 0 ]; then
    verdict=CONFIRMED
  elif [ "${SECRET[krb5p]:-}" = denied ] && [ "${DENIED[krb5p]:-0}" -ge 1 ]; then
    verdict=REFUTED
  fi
fi

echo
echo "════ observation table (uid $ME; files owned by $OTHER; server ENFORCE) ════"
printf "  %-8s %-10s %-12s %-18s %-14s %s\n" arm "0644 read" "0600 read" "write (not owner)" "created owner" "server DENIED"
for s in sys krb5p; do
  printf "  %-8s %-10s %-12s %-18s %-14s %s\n" "$s" "${WORLD[$s]:-?}" "${SECRET[$s]:-?}" "${WRITE[$s]:-?}" "${OWNER[$s]:-?}" "${DENIED[$s]:-?}"
done
echo
echo "════ VERDICT: $verdict ════"
case $verdict in
  CONFIRMED)    echo "  krb5p authenticated the caller and evaluated no rights: the same uid the sys arm denied reads the 0600 file / writes the file it does not own, and the server logged nothing.";;
  REFUTED)      echo "  the server denied under krb5p and logged it — a credential WAS evaluated under GSS; the deck row and radar score are wrong.";;
  INCONCLUSIVE) echo "  a control failed; see the legs above. Do not read the krb5p cells as an answer.";;
esac

echo; echo "=== server DENIED lines ==="; sudo grep "DENIED" "$LOG" | cut -c1-200 || echo "(none)"
echo "=== server log tail ==="; sudo tail -5 "$LOG" | cut -c1-200
echo "=== rpc.gssd: contexts for uid $ME ==="; sudo grep -E "uid $ME|krb5cc_$ME|testuser" "$GSSLOG" | tail -6 | cut -c1-160

sudo pkill -f "flint-nfs-server.*--port $PORT" 2>/dev/null
sudo pkill -f rpc.gssd 2>/dev/null
echo; echo "=== $ok ok, $bad bad — $verdict ==="
[ "$bad" = 0 ] && [ "$verdict" != INCONCLUSIVE ]
