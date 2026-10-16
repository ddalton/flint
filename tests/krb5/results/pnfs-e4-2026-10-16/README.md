# E4: which DS error for a refused filehandle (2026-10-16)

The pNFS Kerberos plan's E4, before Phase 4 (a MAC'd DS filehandle): when a
data server refuses a filehandle, which NFS4 error does the Linux client
recover from cleanly, and which livelocks? Scratch build (branch
`e4-scratch`, 8f176f78, not merged): the DS refuses EVERY PUTFH with
`FLINT_E4_REFUSE`'s status. VM rig (Ubuntu 24.04 kernel client),
`sec=sys`, 16 MiB written with `conv=fsync` and read back after a cache
drop; the rig is `tests/krb5/pnfs-e4.sh` on that branch. Raw output:
`results.txt`.

| DS answers PUTFH with | Completes, intact | DS refusals (ds1/ds2) | MDS LAYOUTGETs | What the client did |
|---|---|---|---|---|
| (nothing refused, control) | yes, 0.3 s | 0 | 2 | striped as usual |
| NFS4ERR_BADHANDLE | yes, 0.2 s | 48/49 | 43 | new LAYOUTGETs, then I/O through the MDS |
| NFS4ERR_STALE | yes | 49/48 | 45 | the same |
| NFS4ERR_ACCESS | yes | 49/48 | 43 | the same |
| NFS4ERR_FHEXPIRED | yes | 49/48 | 47 | the same |
| NFS4ERR_PERM | yes | 49/48 | 2 | through the MDS, no new layouts |
| NFS4ERR_IO | yes | 8/9 | 2 | gave up on the DSes at once |

- **No error livelocked**: every arm finished in well under a second, and
  the refusal counters stopped once the I/O did.
- The DSes hold 8 MiB each in every arm: that is the MDS's F66 relay of the
  fallback I/O over the control path, so where the bytes are does not tell
  the arms apart; the LAYOUTGET count does.
- **Phase 4 answers NFS4ERR_BADHANDLE**, as the plan expected: a refused
  handle is, for a legitimate client, one minted under a key since rotated
  away, and BADHANDLE (like STALE) makes the client fetch a fresh layout,
  which the MDS mints under the current key. A forged handle keeps failing,
  a bounded number of times, and its I/O ends on the MDS. PERM and IO skip
  the fresh layout, so a rotation would push every client to the MDS until
  its layouts were returned.
