#!/bin/bash
set -euo pipefail
REALM=FLINT.TEST
SRV=flintsrv.flint.test
CLI=lima-flint-drill.flint.test

# Both names must resolve, and rpc.gssd does a REVERSE lookup on the
# server address to build the service principal — so 127.0.0.1 must map
# back to the server name, not to localhost.
grep -q "$SRV" /etc/hosts || echo "127.0.0.1 $SRV flintsrv $CLI" | sudo tee -a /etc/hosts >/dev/null

# Server principal (what rpc.gssd will ask the KDC for) ...
sudo kadmin.local -q "addprinc -randkey nfs/$SRV@$REALM" >/dev/null 2>&1 || true
sudo rm -f /tmp/flint-nfs.keytab
sudo kadmin.local -q "ktadd -k /tmp/flint-nfs.keytab nfs/$SRV@$REALM" >/dev/null 2>&1
sudo chmod 644 /tmp/flint-nfs.keytab

# ... and the CLIENT machine credential rpc.gssd uses for the mount
# itself. Without this the mount fails before a single NFS op is sent.
sudo kadmin.local -q "addprinc -randkey host/$CLI@$REALM" >/dev/null 2>&1 || true
sudo rm -f /etc/krb5.keytab
sudo kadmin.local -q "ktadd -k /etc/krb5.keytab host/$CLI@$REALM" >/dev/null 2>&1

sudo mkdir -p /srv/flintexport && sudo chmod 777 /srv/flintexport
echo hello-from-krb5p | sudo tee /srv/flintexport/probe.txt >/dev/null

echo "--- server keytab ---"; sudo klist -k /tmp/flint-nfs.keytab
echo "--- client keytab ---"; sudo klist -k /etc/krb5.keytab
echo "--- hosts ---"; grep flint /etc/hosts
