#!/usr/bin/env bash
# Leg C10 — xfstests (generic) against flint, WITH a knfsd control arm.
#
# WHAT THIS ADDS THAT THE OTHER SUITES DO NOT. pynfs asks protocol
# questions (does COMPOUND behave), nfstest asks client-visible POSIX
# questions, pjdfstest asks authorization questions. None of them
# exercise the suite the Linux filesystem community actually gates on.
# xfstests is that suite: it is what every in-tree Linux filesystem must
# survive, it runs fsstress/fsx/dbench workloads, and its `generic/`
# group is explicitly written to be filesystem-agnostic — including NFS,
# which upstream runs in its own nightly.
#
# WHY THE CONTROL ARM IS NOT OPTIONAL — the same reason as pjdfstest,
# only more so. A large fraction of generic/ is inapplicable to NFS and
# will _notrun; another fraction fails on ANY NFS server because the
# protocol cannot express what the test asserts (O_DIRECT alignment,
# seek-hole precision, xattr namespaces, i_version semantics). knfsd is
# the reference implementation. A raw flint failure count is therefore
# not a score; only tests that fail on flint and PASS on knfsd are
# evidence about flint.
#
# ⚠ _notrun IS NOT A PASS. The parse below counts notrun separately and
# the differential ignores it in BOTH directions — a test that runs on
# knfsd but is skipped on flint is reported as a COVERAGE LOSS, not a
# win. Without that column a server could score perfectly by declining
# to support anything the suite probes for.
#
# ⚠ PRIVATE PORT BY DEFAULT. Two Claude sessions share this Lima VM;
# 20490 belongs to the pynfs rig. Overriding NFS_PORT to 20490 will
# fight whatever else is running.
set -uo pipefail

VM=${LIMA_VM:-flint-nfs-client}
PORT=${NFS_PORT:-24490}
HEALTH_PORT=${NFS_HEALTH_PORT:-24491}
GROUP=${XFS_GROUP:-quick}
TIMEOUT=${XFS_TIMEOUT:-5400}
BASELINE=${BASELINE:-tests/lima/xfstests-baseline.json}
SRC_CONFIG=${SRC_CONFIG:-tests/lima/pnfs/lite-pynfs.yaml}

vm() { limactl shell "$VM" -- sudo bash -lc "$1"; }

fail() { echo "FAIL: $*" >&2; exit 1; }

echo "── preflight ──"
# xfstests is multi-uid like pjdfstest: it su's to fsgqa. Missing users
# do not make tests fail, they make them silently _notrun — which is the
# vacuity this whole file exists to refuse.
vm 'for u in fsgqa fsgqa2 123456-fsgqa; do id "$u" >/dev/null 2>&1 || exit 1; done' \
  || fail "fsgqa / fsgqa2 / 123456-fsgqa must all exist, or xfstests silently skips its multi-uid tests"
vm 'test -x /opt/xfstests/check && test -x /opt/xfstests/ltp/fsstress && test -x /opt/xfstests/src/godown' \
  || fail "/opt/xfstests is not built (need check, ltp/fsstress, src/godown)"

# ── df -a, and why the rig has to be patched to get here ─────────────
# xfstests resolves a filesystem's type with `_fs_type`, which greps the
# DF_PROG output for the device string. GNU df OMITS any filesystem
# reporting zero total blocks unless -a is given, and flint answers
# FATTR4_SPACE_TOTAL/FILES_TOTAL with nothing at all when the tier is
# off (fileops.rs: the SPACE_*/FILES_* arms are guarded on
# `tier::space::view().is_some()`, and `tier.enabled` defaults to false
# in flint-lite-chart/values.yaml). The mount is therefore invisible to
# plain df, _fs_type returns empty, and ./check refuses to start with
# "TEST_DEV=... is mounted but not a type nfs filesystem".
#
# The patch is applied ONCE, to the shared xfstests tree, so BOTH arms
# run under it — the control arm stays a control. It is a workaround for
# the rig, NOT for flint: every test that then fails because the server
# claims zero free space still fails, and shows up in the differential
# where it belongs.
vm "grep -q 'df -a -T -P' /opt/xfstests/common/config || \
    sed -i 's|DF_PROG -T -P|DF_PROG -a -T -P|' /opt/xfstests/common/config
    grep -q 'df -a -T -P\|DF_PROG -a -T -P' /opt/xfstests/common/config" \
  || fail "could not patch DF_PROG to df -a"

# ── the flint config, derived rather than duplicated ──────────────────
# The posture that matters (sqlite state backend, health listener) lives
# in lite-pynfs.yaml and is documented there. Copying it here would let
# the two drift and leave this leg measuring a posture flint-lite does
# not ship. Only the port and the export path are overridden.
TMPCFG=$(mktemp)
sed -e "s|port: 20490|port: $PORT|" \
    -e "s|port: 20491|port: $HEALTH_PORT|" \
    -e "s|/srv/flint-mds-export|/srv/flint-xfs-export|g" \
    -e "s|/srv/flint-mds-state|/srv/flint-xfs-state|g" \
    "$SRC_CONFIG" > "$TMPCFG"
grep -q "port: $PORT" "$TMPCFG" || fail "config rewrite did not take — refusing to run on the pynfs rig's port"
limactl copy "$TMPCFG" "$VM":/tmp/xfstests-flint.yaml >/dev/null || fail "could not copy config into the VM"

# ── size-capped backing store, and why the run needs one ─────────────
# With the tier off there is no admission gate, so an ENOSPC test writes
# until the VM's real disk fills. That disk is SHARED: another session's
# flint server keeps its export and sqlite state on it, and a 100%-full
# root would take that down as collateral. Each arm therefore gets its
# own loopback ext4 of BACKING_MB, torn down before the next arm, so at
# most one cap-sized image exists at a time and a fill is contained.
#
# Both arms get the SAME size for the same reason they run the same
# tests: a control arm on a differently-sized filesystem is not a control
# for any test that turns on capacity.
BACKING_MB=${BACKING_MB:-512}

setup_backing() {   # mountpoint, image
  vm "umount -f -l $1 2>/dev/null || true
      rm -f $2
      mkdir -p $1
      truncate -s ${BACKING_MB}M $2
      mkfs.ext4 -q -F $2 >/dev/null 2>&1
      mount -o loop $2 $1
      mountpoint -q $1" \
    || fail "could not mount the ${BACKING_MB}M backing store at $1"
}

teardown_backing() {   # mountpoint, image
  vm "umount -f -l $1 2>/dev/null || true; rm -f $2" >/dev/null 2>&1
}

write_config() {   # arm, test_dev, scratch_dev, mount_opts
  vm "cat > /opt/xfstests/local.config <<'EOF'
export FSTYP=nfs
export TEST_DEV=$2
export TEST_DIR=/mnt/xfs-test
export SCRATCH_DEV=$3
export SCRATCH_MNT=/mnt/xfs-scratch
export NFS_MOUNT_OPTIONS=\"$4\"
export TEST_FS_MOUNT_OPTS=\"$4\"
export MOUNT_OPTIONS=\"$4\"
export RESULT_BASE=/tmp/xfsresults-$1
export LOAD_FACTOR=1
export TIME_FACTOR=1
EOF
mkdir -p /mnt/xfs-test /mnt/xfs-scratch /tmp/xfsresults-$1"
}

run_arm() {        # arm, label
  local arm=$1
  echo "   running xfstests -g $GROUP  (arm: $arm)"
  # Delete the previous transcript FIRST. Without this, an arm whose
  # ./check dies on startup leaves the last run's file in place — and
  # that file has a "Ran:" line, so the guard below would pass, the
  # parser would read hours-old results, and the differential would be
  # computed against a run that never happened. Same shape as the
  # journalctl trap: the instrument reporting on stale state.
  vm "rm -f /tmp/xfs-$arm.txt" >/dev/null 2>&1
  vm "cd /opt/xfstests && timeout $TIMEOUT ./check -g $GROUP > /tmp/xfs-$arm.txt 2>&1; echo \"CHECK_EXIT=\$?\" >> /tmp/xfs-$arm.txt" >/dev/null 2>&1
  # ./check exits non-zero whenever ANY test fails, which is the expected
  # state for both arms. The exit status is therefore recorded, not
  # gated on; what gates is the parse below finding a result section at
  # all. A run that died before testing anything must not read as "0
  # failures".
  vm "grep -qE '^(Ran:|Passed all)' /tmp/xfs-$arm.txt" \
    || { echo "FAIL: arm $arm produced no xfstests result section — the run died, it did not pass"; \
         vm "grep -c CHECK_EXIT=124 /tmp/xfs-$arm.txt" >/dev/null 2>&1 && \
           echo "      (CHECK_EXIT=124: the \$TIMEOUT of ${TIMEOUT}s truncated it — raise XFS_TIMEOUT)"; \
         vm "tail -30 /tmp/xfs-$arm.txt"; exit 1; }
}

# ── arm A: flint ──────────────────────────────────────────────────────
echo "── arm A: flint (port $PORT) ──"
vm "umount -f -l /mnt/xfs-test 2>/dev/null||true; umount -f -l /mnt/xfs-scratch 2>/dev/null||true
    systemctl stop flint-xfs 2>/dev/null||true; systemctl reset-failed flint-xfs 2>/dev/null||true
    rm -rf /srv/flint-xfs-state /tmp/xfsresults-flint
    mkdir -p /srv/flint-xfs-state /mnt/xfs-test /mnt/xfs-scratch" >/dev/null 2>&1
setup_backing /srv/flint-xfs-export /var/tmp/flint-xfs.img
vm "mkdir -p /srv/flint-xfs-export/tst /srv/flint-xfs-export/scr
    chmod 0777 /srv/flint-xfs-export /srv/flint-xfs-export/tst /srv/flint-xfs-export/scr
    chmod +x /tmp/flint-pnfs-mds-xfs
    systemd-run --unit=flint-xfs --collect --setenv=RUST_LOG=warn \
      --setenv=FLINT_NFS_GRACE_SECS=5 \
      /tmp/flint-pnfs-mds-xfs --config /tmp/xfstests-flint.yaml" >/dev/null 2>&1
sleep 5
vm "ss -lntp | grep -c ':$PORT ' " >/dev/null 2>&1
vm "ss -lnt | grep -q ':$PORT '" || { vm "journalctl -u flint-xfs --no-pager | tail -30"; fail "flint is not listening on $PORT"; }

# Subdirectory mounts are the whole reason two distinct NFS "devices"
# are available at all. If flint cannot serve them, TEST_DEV and
# SCRATCH_DEV would collapse to the same path and xfstests would happily
# run with a scratch device that IS the test device — silently
# destroying the test fs mid-run and blaming the results on flint.
vm "mount -t nfs -o vers=4.1,port=$PORT,nolock 127.0.0.1:/tst /mnt/xfs-test" \
  || fail "flint refused a subdirectory mount of /tst — cannot form a distinct TEST_DEV"
vm "mountpoint -q /mnt/xfs-test" || fail "/mnt/xfs-test is not a mountpoint"

write_config flint "127.0.0.1:/tst" "127.0.0.1:/scr" "-o vers=4.1,port=$PORT,nolock"
run_arm flint

vm "umount -f -l /mnt/xfs-test 2>/dev/null||true; umount -f -l /mnt/xfs-scratch 2>/dev/null||true
    sleep 1
    systemctl stop flint-xfs 2>/dev/null||true; systemctl reset-failed flint-xfs 2>/dev/null||true" >/dev/null 2>&1
teardown_backing /srv/flint-xfs-export /var/tmp/flint-xfs.img

# ── arm B (control): knfsd ────────────────────────────────────────────
echo "── arm B (control): knfsd ──"
vm "umount -f -l /mnt/xfs-test 2>/dev/null||true; umount -f -l /mnt/xfs-scratch 2>/dev/null||true
    rm -rf /tmp/xfsresults-knfsd
    mkdir -p /mnt/xfs-test /mnt/xfs-scratch" >/dev/null 2>&1
setup_backing /srv/knfsd-xfs /var/tmp/knfsd-xfs.img
vm "mkdir -p /srv/knfsd-xfs/tst /srv/knfsd-xfs/scr
    chmod 0777 /srv/knfsd-xfs /srv/knfsd-xfs/tst /srv/knfsd-xfs/scr
    grep -q '/srv/knfsd-xfs/tst' /etc/exports 2>/dev/null || {
      echo '/srv/knfsd-xfs/tst 127.0.0.1/32(rw,sync,no_subtree_check,no_root_squash,fsid=11)' >> /etc/exports
      echo '/srv/knfsd-xfs/scr 127.0.0.1/32(rw,sync,no_subtree_check,no_root_squash,fsid=12)' >> /etc/exports; }
    exportfs -ra; systemctl restart nfs-kernel-server; sleep 3
    mount -t nfs -o vers=4.1,nolock 127.0.0.1:/srv/knfsd-xfs/tst /mnt/xfs-test" >/dev/null 2>&1
vm "mountpoint -q /mnt/xfs-test" || fail "knfsd control arm did not mount — this is an INFRASTRUCTURE error, not a flint result"

write_config knfsd "127.0.0.1:/srv/knfsd-xfs/tst" "127.0.0.1:/srv/knfsd-xfs/scr" "-o vers=4.1,nolock"
run_arm knfsd

vm "umount -f -l /mnt/xfs-test 2>/dev/null||true; umount -f -l /mnt/xfs-scratch 2>/dev/null||true" >/dev/null 2>&1
teardown_backing /srv/knfsd-xfs /var/tmp/knfsd-xfs.img

# ── differential ──────────────────────────────────────────────────────
TMP=$(mktemp -d)
limactl shell "$VM" -- sudo cat /tmp/xfs-flint.txt > "$TMP/flint.txt" 2>/dev/null
limactl shell "$VM" -- sudo cat /tmp/xfs-knfsd.txt > "$TMP/knfsd.txt" 2>/dev/null

python3 scripts/check-xfstests.py "$TMP/flint.txt" "$TMP/knfsd.txt" "$BASELINE"
