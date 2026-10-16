# pNFS Kerberos Phase 1 in the VM, 2026-10-16

`tests/krb5/pnfs-e1.sh` (arms A, B, C) with the Phase 1 binaries
(`flint-pnfs-mds` / `flint-pnfs-ds` from this branch at `c8867794`), on the
same KVM Ubuntu 24.04 VM as phase 0 (kernel 6.8, nfs-utils 2.6.4, MIT KDC
1.20). The MDS has `KRB5_KTNAME` in every arm, so it withholds layouts from
RPCSEC_GSS callers.

- **C (`sec=krb5i`): no RPC reached either DS** (no 20491/20492 entries
  in "flavors by port"); the 64 MiB went to the MDS export; every call was
  RPCSEC_GSS; read-back intact; read 0.3 s. Before Phase 1 the same arm sent
  the data to the DSes as AUTH_SYS after failed GSS attempts
  (`../pnfs-phase0-2026-10-16/`).
- **A, B (`sec=sys`)**: unchanged — 32 MiB on each DS, AUTH_SYS, intact.
- `mds-C.log`: the MDS's start-up lines, which say the layouts are withheld.
