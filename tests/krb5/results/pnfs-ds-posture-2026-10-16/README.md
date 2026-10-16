# The data servers report their posture (2026-10-16)

`ARMS=EH tests/krb5/pnfs-phase3.sh` on 6fcbe890: each DS reports a keytab
and its floor at registration, and the MDS grants Kerberos callers files
layouts only while every registered DS has both (`DeviceInfo::gss_ready`).
Raw output: `results.txt`. Compare with arm H of
`../pnfs-phase3-2026-10-16/`, before the change.

| Arm | Data servers | Before | After |
|---|---|---|---|
| E | keys, krb5i floor | 32 MiB per DS, all GSS | the same (174/178 GSS compounds); intact |
| H | no keytab, no floor | client downgraded to AUTH_SYS at the DSes (858/876 sys compounds) | no layout: all 64 MiB on the MDS under krb5i (282 GSS compounds), not one call at a DS; intact |
