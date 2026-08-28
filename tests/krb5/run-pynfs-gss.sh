#!/bin/bash
# pynfs over RPCSEC_GSS — the suite the repo installed and then ran only
# with sec=sys, because `pip install gssapi || true` let the bindings fail
# silently.
set -uo pipefail
PORT=${PORT:-20520}
SRV=flintsrv.flint.test
EXPORT=/srv/pynfs-export
VOL=pynfs-gss
SEC=${SEC:-krb5}
BIN=${BIN:-/tmp/flint-nfs-server}

# Kill only OUR server, by pid, never `pkill -f flint-nfs-server`.
#
# The name-wide pkill made two of these unable to run at the same time:
# a second run's startup (and its teardown) killed the FIRST run's
# server, and the first run's client then sat in futex_do_wait forever
# against a socket nobody was serving. It looks exactly like a slow test.
SRV_PID=""
cleanup() { [ -n "$SRV_PID" ] && sudo kill "$SRV_PID" 2>/dev/null; }
trap cleanup EXIT

sudo rm -rf "$EXPORT"; sudo mkdir -p "$EXPORT/.flint-nfs"
echo -n "$VOL" | sudo tee "$EXPORT/.flint-nfs/volume-id" >/dev/null
sudo mkdir -p "$EXPORT/tmp"; sudo chmod 0777 "$EXPORT/tmp"

sudo env KRB5_KTNAME=/tmp/flint-nfs.keytab FLINT_NFS_GRACE_SECS=900 RUST_LOG=info \
  "$BIN" --export-path "$EXPORT" --volume-id "$VOL" \
  --bind-addr 0.0.0.0 --port "$PORT" > "/tmp/pynfs-flint-$SEC.log" 2>&1 &
SRV_PID=$!
sleep 4
# Check OUR pid, not any process with the name -- and give each flavor
# its own log, so a concurrent run cannot overwrite the evidence either.
sudo kill -0 "$SRV_PID" 2>/dev/null || { echo "SERVER DID NOT START"; tail -5 "/tmp/pynfs-flint-$SEC.log"; exit 1; }
echo "server up on $PORT (pid $SRV_PID)"

echo flintflint | kinit testuser@FLINT.TEST 2>&1 | tail -1
klist 2>&1 | head -3

cd /opt/pynfs/nfs4.1
echo "=== pynfs --security=$SEC ==="
PYTHONPATH=/opt/pynfs /opt/pynfs/.venv/bin/python ./testserver.py \
  "$SRV:$PORT/tmp" --security="$SEC" --maketree --nocleanup \
  --json=/tmp/pynfs-$SEC.json "$@" 2>&1 | tail -40
echo "PYNFS_RC=$?"
cleanup
