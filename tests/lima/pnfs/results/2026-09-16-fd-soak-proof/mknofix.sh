#!/usr/bin/env bash
# Build the POSITIVE CONTROL: the same tree with the FIX REMOVED.
#
# A flat descriptor line only means something if this build's line climbs.
# So mutate exactly the lines the fix added — the laundromat that expires
# clients on a timer, the RAII lease that makes the state own its
# descriptor, and the two backstop release hooks — and nothing else.
set -eu
source "$HOME/.cargo/env"
cd /home/ubuntu/flintsrc/spdk-csi-driver
cp src/pnfs/mds/server.rs /tmp/server.rs.bak
cp src/nfs/v4/operations/ioops.rs /tmp/ioops.rs.bak

python3 - <<'PYEOF'
import re
p='src/pnfs/mds/server.rs'
s=open(p).read()
old_start = s.index('            let d = Arc::clone(&base_dispatcher);')
old_end   = s.index('"🧺 laundromat armed (traffic-independent lease expiry)");', old_start)
old_end   = s.index('\n', old_end)+1
block = s[old_start:old_end]
assert 'courtesy_release_expired' in block, "did not find the laundromat"
s = s[:old_start] + '            let _ = &base_dispatcher; // NOFIX: laundromat removed\n' + s[old_end:]
open(p,'w').write(s)
print("laundromat removed")

p='src/nfs/v4/operations/ioops.rs'
s=open(p).read()
for marker, name in [('fd_cache.install_lease_attach(', 'RAII lease attach'),
                     ('.install_fd_release(Arc::new(move |sid: &StateId| {', 'delegation release hook'),
                     ('state_mgr.stateids.install_fd_release(Arc::new(move |other: &[u8; 12]| {', 'stateid sweep hook')]:
    i = s.index(marker)
    # walk back to the opening brace of the enclosing block statement
    j = s.rindex('{\n', 0, s.rindex('\n', 0, i))
    # find the matching close of the block that starts at j
    depth=0; k=j
    while True:
        if s[k]=='{': depth+=1
        elif s[k]=='}': 
            depth-=1
            if depth==0: break
        k+=1
    s = s[:j] + '{ /* NOFIX: ' + name + ' removed */ }' + s[k+1:]
    print(f"{name} removed")
# set_self_ref only arms the Weak back-pointer the lease needs; harmless, keep.
open(p,'w').write(s)
PYEOF

grep -c 'NOFIX' src/pnfs/mds/server.rs src/nfs/v4/operations/ioops.rs
cargo build --release --bin flint-pnfs-mds 2>&1 | tail -20
rc=${PIPESTATUS[0]}
echo "NOFIX_BUILD_RC=$rc"
cp target/release/flint-pnfs-mds /home/ubuntu/flint-pnfs-mds-nofix
# restore the real tree immediately so nothing runs the mutant by accident
cp /tmp/server.rs.bak src/pnfs/mds/server.rs
cp /tmp/ioops.rs.bak src/nfs/v4/operations/ioops.rs
echo NOFIX_DONE > /home/ubuntu/NOFIX_DONE
