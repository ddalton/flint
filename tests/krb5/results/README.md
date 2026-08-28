# pynfs over RPCSEC_GSS — 2026-08-27

Verbatim `--json` output from `run-pynfs-gss.sh all`, one clean serial
run per flavor, against a real MIT KDC and the flint NFSv4.1 server.

| flavor | tests | errors | failures | skipped | passed |
|---|---|---|---|---|---|
| `sys` | 266 | 0 | 23 | 68 | 175 |
| `krb5` | 266 | 0 | 23 | 68 | 175 |
| `krb5i` | 266 | 0 | 23 | 68 | 175 |
| `krb5p` | 266 | 0 | 23 | 68 | 175 |

**All four flavors are identical — and not just in count.** The set of
failing test NAMES is the same in every run (verified equal),
so the set difference is empty in both directions. **Kerberos introduces
zero conformance regression across svc_none, svc_integrity and
svc_privacy.**

The 23 are pre-existing feature gaps, failing the same way under
`sec=sys`: 11 xattr (RFC 8276 — flint answers `NFS4ERR_OP_ILLEGAL`),
10 delegation, 2 `FATTR4_OPEN_ARGUMENTS`.

## Read these before quoting a number

- **The old `171/0/91` figure is retired.** That was an older pynfs. A
  fresh clone has 266 tests, including xattr tests flint does not
  implement, so the honest baseline moved — not the server.
- **Run these SERIALLY.** A krb5i run once showed 25 failures instead of
  23. It did not reproduce. That run overlapped another full 266-test
  suite on a 4-CPU VM, and the two extra failures were the two most
  timing-sensitive tests in the suite: EID9 `testLeasePeriod`, the only
  test that sleeps out a full 90s lease and then asserts on expiry
  timing, and EID4 `testBadFlags`, which does not fail in its own body at
  all — it fails in `environment.startUp()`, inheriting EID9's wedged
  connection. One failure and one victim.
- Before calling a batch serial, check nothing else is still running.
  Fixing `pkill`-by-name made concurrent runs stop killing each other; it
  did not make them stop *slowing* each other.
