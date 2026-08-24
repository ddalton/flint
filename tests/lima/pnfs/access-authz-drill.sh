#!/usr/bin/env bash
# Leg A1 — the cross-uid authorization gate.
#
# pjdfstest (leg A0) is the industry oracle and it is thorough, but it
# takes ~3.5 minutes over 8,798 tests and needs a build. This is the
# fast version: seconds, no build, and it fails loudly on the exact
# defect §2A describes — ACCESS is a mirror and the data ops are checked
# against the server's identity rather than the caller's.
#
# USAGE:  access-authz-drill.sh <mountpoint> [label]
#
# MUST BE RUN AS ROOT: it creates files owned by one uid and then acts
# as another. That is the whole point — every suite that runs as a
# single uid (pynfs, nfstest_posix) is structurally blind to this.
#
# ANTI-VACUITY: run it against knfsd. Every assertion here must PASS
# there. An assertion that knfsd also fails is this drill being wrong
# about POSIX, not the server under test being wrong — and it must be
# fixed here, not excused there.
set -uo pipefail

MNT=${1:?usage: access-authz-drill.sh <mountpoint> [label]}
LABEL=${2:-$MNT}
OWNER=65534          # nobody — owns the fixtures
OTHER=65533          # a DIFFERENT unprivileged uid — acts on them
PASS=0; FAIL=0

as_other() { setpriv --reuid=$OTHER --regid=$OTHER --clear-groups "$@" 2>/dev/null; }

# expect_deny <what> <cmd...>  — the command MUST fail.
expect_deny() {
  local what=$1; shift
  if as_other "$@"; then
    echo "  DENY-EXPECTED but ALLOWED : $what"
    FAIL=$((FAIL+1))
  else
    echo "  ok (denied)               : $what"
    PASS=$((PASS+1))
  fi
}

# expect_allow <what> <cmd...> — the command MUST succeed. These keep the
# drill honest: a server that denied EVERYTHING would pass every
# expect_deny above and be just as broken.
expect_allow() {
  local what=$1; shift
  if as_other "$@"; then
    echo "  ok (allowed)              : $what"
    PASS=$((PASS+1))
  else
    echo "  ALLOW-EXPECTED but DENIED : $what"
    FAIL=$((FAIL+1))
  fi
}

D="$MNT/authz-drill.$$"
rm -rf "$D" 2>/dev/null
mkdir -p "$D" || { echo "FAIL: cannot create $D"; exit 1; }

# ── fixtures, owned by OWNER, unreadable by OTHER ──
echo secret       > "$D/private.txt";  chmod 0600 "$D/private.txt";  chown $OWNER:$OWNER "$D/private.txt"
mkdir -p            "$D/privatedir";   chmod 0700 "$D/privatedir";   chown $OWNER:$OWNER "$D/privatedir"
echo x            > "$D/privatedir/inside.txt"; chown $OWNER:$OWNER "$D/privatedir/inside.txt"
echo public       > "$D/world.txt";    chmod 0644 "$D/world.txt";    chown $OWNER:$OWNER "$D/world.txt"
mkdir -p            "$D/opendir";      chmod 0777 "$D/opendir";      chown $OWNER:$OWNER "$D/opendir"

echo "── $LABEL ──"
# The core of §2A: a 0600 file must not be readable by another uid.
expect_deny "read 0600 file owned by another uid"        cat  "$D/private.txt"
expect_deny "write 0600 file owned by another uid"       dd if=/dev/zero of="$D/private.txt" bs=1 count=1 conv=notrunc
expect_deny "list 0700 dir owned by another uid"         ls   "$D/privatedir"
expect_deny "traverse into 0700 dir owned by another"    cat  "$D/privatedir/inside.txt"
# chown by a non-owner is EPERM even for the file's own group — this is a
# privilege-escalation primitive if it is allowed.
expect_deny "chown a file owned by another uid"          chown $OTHER "$D/private.txt"
expect_deny "chmod a file owned by another uid"          chmod 0777 "$D/private.txt"
expect_deny "unlink from a 0700 dir owned by another"    rm -f "$D/privatedir/inside.txt"
# ...and the other direction, so "deny everything" cannot pass.
expect_allow "read a 0644 file"                          cat  "$D/world.txt"
expect_allow "create a file in a 0777 dir"               touch "$D/opendir/mine.txt"

rm -rf "$D" 2>/dev/null
echo "  → $LABEL: $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
