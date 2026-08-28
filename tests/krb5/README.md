# The Kerberos rig

Everything needed to exercise flint's RPCSEC_GSS against a **real MIT
KDC and a real Linux client**, plus the drills that do it.

These scripts spent their first day in a throwaway VM's `/tmp`. They are
here because the numbers in `docs/` cite them, and because the radar
generator already taught this repo what happens to evidence that lives
only in a scratchpad.

## Why it exists

flint shipped a pure-Rust RPCSEC_GSS implementation that had **never
spoken to a KDC**. `c733cf74` rebuilt the crypto to spec and pinned it to
published vectors; that found four defects. A real KDC and a real mount
then found five more, in a strict ladder — each visible only once the one
above it was fixed. Vectors prove you computed the right bytes; only a
peer proves you exchange them correctly.

## Order

```sh
./setup-kdc.sh        # MIT KDC, realm FLINT.TEST, principals + keytabs
./setup-mount.sh      # /etc/hosts, service principal, client keytab
./setup-pynfs.sh      # pynfs that can actually speak GSS (see traps)
```

Then any drill. Each takes `BIN=` (default `/tmp/flint-nfs-server`);
cross-build with
`cargo zigbuild --bin flint-nfs-server --target aarch64-unknown-linux-musl`.

| drill | what it proves | last run |
|---|---|---|
| `run-krb5p.sh` | `mount -o sec=krb5p`, readdir, read | 6 ok, 0 bad |
| `run-krb5i.sh` | `sec=krb5i` end to end, incl. write-then-read-back (MICs the reply, not just the call) | 8 ok, 0 bad |
| `run-secfloor.sh` | `FLINT_NFS_MIN_SEC` actually refuses | 12 ok, 0 bad |
| `run-pynfs-gss.sh` | the conformance suite over GSS; takes test names or `all` | see below |

## Traps, each of which cost a run

- **`rpc.gssd` does a REVERSE lookup** on the server address to build the
  service principal. `127.0.0.1` must map back to the server name, not to
  `localhost`, or the client asks the KDC for the wrong principal.
- **`xdrlib` was removed from the Python stdlib in 3.13** (PEP 594).
  pynfs imports it directly, so on a modern distro `import rpc.security`
  dies long before any flavor question. `xdrlib3` + a shim module.
- **There is no `nfs4.1/Makefile`.** The repo Makefile's
  `cd nfs4.1 && make` was always a no-op under `|| true`. The build is
  `setup.py build` at the root — and it drives sub-builds through
  `os.system`, so their failures are invisible. Run a subdir build
  directly to see an error.
- **A mount that succeeds may not have negotiated what you asked for.**
  Assert `/proc/mounts` really says `sec=krb5i`; the drills do.
- **A refusal leg passes just as well against a dead server.** Every
  refusal in `run-secfloor.sh` is paired with a mount of an ADMITTED
  flavor on the SAME process. That guard is not decoration — it is what
  caught the krb5p-floor design error below.

## The krb5p floor, and why it is refused

`FLINT_NFS_MIN_SEC=krb5p` **refuses to start**, deliberately. Measured
here, three runs, one variable:

| floor | mount | result | services on the wire |
|---|---|---|---|
| krb5p | sec=krb5p | **refused** | Integrity, None — no Privacy ever |
| krb5i | sec=krb5p | mounted | Integrity, None, **Privacy** |
| none | sec=krb5p | mounted | Integrity, None, Privacy |

A Linux `sec=krb5p` mount runs its NFSv4 **state management over krb5i**
— EXCHANGE_ID, CREATE_SESSION, the machine credential — and only the
filesystem operations over krb5p. An RPC-layer krb5p floor refuses those
krb5i calls, so the mount dies before one private byte moves: the
strongest posture in the tree, delivered as an unmountable export.

Doing it properly is what knfsd does — security as a property of the
EXPORT, enforced as `NFS4ERR_WRONGSEC` (10016) on the
filehandle-establishing operations so the client re-negotiates via
SECINFO. That is per-operation enforcement inside the COMPOUND
dispatcher, with SEQUENCE still processed first on 4.1. Until it exists,
krb5i is the strongest usable floor and clients may still choose
`sec=krb5p` for data.

## What pynfs over GSS found on its first run

An RPC **NULL over RPCSEC_GSS** — legal, and how a client probes a
context — came back `GARBAGE_ARGS`. The GSS `DATA` arm called
`handle_compound` unconditionally and never looked at `call.procedure`,
so a NULL met the COMPOUND decoder with an empty body. The non-GSS path
had always dispatched NULL correctly; only the GSS path did not, which is
exactly why `sec=sys` testing could never see it.

**The realm is disposable.** `FLINT.TEST` keys authenticate nothing; the
committed `src/nfs/krb/testdata/interop.keytab` belongs to a KDC that no
longer exists. Rebuild with `setup-kdc.sh`, never reuse.
