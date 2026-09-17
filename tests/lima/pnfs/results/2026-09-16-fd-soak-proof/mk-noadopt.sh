#!/usr/bin/env bash
# The CONTROL for Option A: the same tree with ONLY the two adopt_open_fd
# wirings removed. A flat curve in the patched build means nothing unless
# this build's curve climbs.
set -eu
source "$HOME/.cargo/env"
cd /home/ubuntu/flintsrc/spdk-csi-driver
cp src/nfs/v4/operations/ioops.rs /tmp/ioops.A.bak
python3 - <<'PY'
p='src/nfs/v4/operations/ioops.rs'; s=open(p).read()
import re
n=0
for pat in [
 r"\n *// A miss on THIS stateid does not mean the server has no fd\n(?: *//[^\n]*\n)* *\.or_else\(\|\| self\.adopt_open_fd\(&op\.stateid\.other, &path, false, cacheable\)\);",
 r"\n *// Writable only: a read-only entry cannot serve a WRITE, and\n(?: *//[^\n]*\n)* *\.or_else\(\|\| self\.adopt_open_fd\(&op\.stateid\.other, &path, true, cacheable\)\);",
]:
    s2, k = re.subn(pat, "; // NOADOPT", s)
    assert k==1, f"pattern matched {k} times"
    s=s2; n+=k
open(p,'w').write(s)
print("removed", n, "adopt wirings")
PY
grep -c NOADOPT src/nfs/v4/operations/ioops.rs
cargo build --release --bin flint-pnfs-mds 2>&1 | tail -15
echo "NOADOPT_BUILD_RC=${PIPESTATUS[0]}"
cp target/release/flint-pnfs-mds /home/ubuntu/flint-noadopt
cp /tmp/ioops.A.bak src/nfs/v4/operations/ioops.rs
echo NOADOPT_DONE > /home/ubuntu/NOADOPT_DONE
