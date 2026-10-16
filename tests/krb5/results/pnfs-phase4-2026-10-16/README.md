# Signed data server filehandles in the VM rig (2026-10-16)

`tests/krb5/pnfs-phase4.sh` on 96ea6f19 (Phase 4, `pnfs::ds_fh`), the krb5
rig VM, `sec=krb5i`, Kerberos data servers with the krb5i floor, every DS
with `FLINT_PNFS_DS_FH_AUTH=enforce`; 64 MiB written and read back. Raw
output: `results.txt`.

| Arm | MDS signs with | DSes verify with | Result |
|---|---|---|---|
| K | key 1 | key 1 | striped under GSS (174/178 compounds), 0 handles refused; intact |
| X | key 1 | key 2 | every handle refused, "a signed handle that does not verify for this session's node" (193/192); the client fell back to the MDS (743 GSS compounds there); intact |
| U | nothing (v2 handles) | key 1 | every handle refused, "an unsigned handle" (192/193); the same fallback; intact |

- X stands for a forged handle, another node's handle, or one minted under
  a key since rotated away; U for an MDS without the key, or an older one.
  In both the refusals are bounded (about 6 per stripe, as E4's BADHANDLE
  arm) and the client recovers through the MDS. The bytes still reach the
  DSes in X and U, through the MDS's F66 relay over the control path
  (Phase 5).
- Node binding is not exercised here (one node); the dispatcher's unit
  test checks that a handle minted for w1 does not verify for w2, and
  `DsPolicy::admit`'s that a DS refuses it on another node's session.
