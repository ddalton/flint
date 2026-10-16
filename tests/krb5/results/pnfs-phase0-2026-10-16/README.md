# pNFS Kerberos, phase 0 (E1, E1b, E2), 2026-10-16

`tests/krb5/pnfs-e1.sh` (with `tests/krb5/rpcflav.py`) inside the krb5 rig
VM: KVM Ubuntu 24.04 on the build box, kernel 6.8.0-142, nfs-utils 2.6.4,
MIT KDC 1.20, after `setup-kdc.sh` and `setup-mount.sh`. Binaries:
`flint-pnfs-mds` / `flint-pnfs-ds` built from main at `1b581372`+#15/#16.

- `abc/results.txt`: arms A (`sec=sys`, gssd on), B (`sec=sys`, gssd off),
  C (`sec=krb5i`, no DS principals in the KDC).
- `d/results.txt`: arm D (`sec=krb5i`, `nfs/ds1`, `nfs/ds2` in the KDC),
  run with `ARMS=D`.
- `*/gssd-*-summary.txt`: counted `rpc.gssd -vvv` lines (the raw logs
  were 30k lines each).
- `*/mds-*.log`, `*/ds1-*.log`: the servers' first 200 lines.

"flavors by port" counts ONC RPC calls per destination port: `20490` the
MDS, `20491`/`20492` the DSes; `sys`/`gss`/`none` the credential flavor;
`null` the NULL procedure (GSS context set-up and pings), `compound` real
NFS work. The interpretation is in `docs/plans/pnfs-kerberos-plan.md`,
"Phase 0 results".
