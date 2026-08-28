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
| `run-pynfs-gss.sh` | the conformance suite over GSS; takes test codes (EID9, CSID1) or `all` | 175/23/68 on every flavor — `results/` |
| `run-gssneg.sh` | **the negative legs** — wrong service key, unknown context handle, wire replay, stale MIC, and whether a keyless peer can move the replay window | 27 ok, 0 bad — `results/gssneg-2026-08-28.log` |

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

## The negative legs, and the four defects they found

Every drill above asks whether a CORRECT client works. `run-gssneg.sh`
asks whether an incorrect one is refused, which is a different question
and had never been put. It needs two things the kernel client will not
do: `gssprobe.py` records one real RPCSEC_GSS DATA record off the wire
through a forwarding proxy and replays it (a replay attacker holds no key
either, which is the point), and `ktcorrupt.py` copies a keytab with its
key bytes flipped — same principal, same enctype, wrong key.

Four defects, in the order they surfaced:

1. **The sequence number that RESET the replay window could be replayed
   once.** `verify_sequence` marked the new high-water number as seen only
   on the sliding path; on the reset path it cleared the bitmap and left
   that number unmarked. The same hole applied to the first call on a
   fresh context. The existing `test_gss_sequence_large_gap` passed
   throughout — it replayed the OLD number after the reset, never the one
   that caused it.
2. **RPCSEC_GSS_MAXSEQ was not enforced** (RFC 2203 §5.3.3.1). A peer could
   park `last_seq_num` at the top of the range, after which every honest
   number is "old".
3. **The ticket's own validity period was never checked.** `EncTicketPart`
   parsed `starttime` and `endtime` and carried `#[allow(dead_code)]`; the
   only clock check on the accept path was the AUTHENTICATOR's skew, and
   the authenticator is minted fresh by whoever holds the session key. An
   expired ticket authenticated forever, so revoking a principal at the
   KDC did not stop anyone already holding one. Pinned by a real MIT
   fixture under an injected clock, since a fixture recorded today is
   valid today and expired tomorrow.
4. **A keyless peer could move an established context's replay window.**
   Found by leg N5. `validate_data` ran `verify_sequence` — which MUTATES
   `last_seq_num` — BEFORE the call verifier was checked, the reverse of
   RFC 2203 §5.3.3.1. So: take a captured record, rewrite its seq_num to
   500001, send it. The MIC check duly rejects it, after the window has
   been parked there. The honest client's next call is then refused as
   "outside window". Sequence acceptance is now `accept_sequence`, called
   only once the MIC has proved the caller holds the key.
   `results/gssneg-prefix-2026-08-28.log` is the red run: `Replay
   detected: seq_num 3 is outside window (last: 500001, window: 128)`.

1-3 are pinned by unit and interop tests, because they need a clock or a
crafted ticket that the wire cannot supply. 4 is pinned both ways.

### Three oracles that were wrong first

Worth keeping, because each PASSED against a server that had the bug:

- **"does the mount still serve?"** It does. CREDPROBLEM tells the client
  to re-init and the kernel does exactly that, so the read succeeds
  through a fresh context and the damage never appears.
- **"did the kernel client hit it?"** Only if its next call happens to use
  the poisoned context. A mount holds one context for the machine
  credential and one per user; which one the capture belongs to is luck.
  Twice it was not. N5 now drives both sides itself.
- **a bare grep of the server log.** It races tracing's buffered writer and
  reads clean off a log that ends up holding the line. Stop the server
  first, then count.

And two rig failures that made legs pass by not looking: `mountpoint -q`
reported success on a STALE mount when the umount lost a race, so "mounted
through the recording proxy" was true while every byte went straight to
the server (`try_mount` now asserts `port=` in `/proc/mounts`); and a
proxy leaked by an aborted run kept 20563 bound, so ours died with
EADDRINUSE while the stale one forwarded and saved nothing.

**`pkill -f rpc.gssd` in a cleanup trap takes out every other drill on the
VM.** Two overlapping runs of this drill did it to each other — N1 mounted,
N3 could not, and `mount.nfs4` reports a missing rpc.gssd as *"an
incorrect mount option was specified"*, which sends you looking at the
options. Same kill-by-name lesson as `run-pynfs-gss.sh`, one daemon
further out. The drill no longer reaps it and re-starts it on demand.

## Results

`results/` holds one clean serial run per flavor with its own README.
The headline: **sys, krb5, krb5i and krb5p are identical — same counts
AND the same 23 failing test names**, so Kerberos costs nothing in
conformance. Run them SERIALLY; two suites on this VM is enough to make
the lease-timing tests fail.

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
