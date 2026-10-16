# pNFS Kerberos Phase 3 in the VM rig (2026-10-16)

`tests/krb5/pnfs-phase3.sh` on the branch's binaries (d7926239: the shared
GSS front end, Kerberos on the data servers, the DS union gate), Ubuntu
24.04 VM with a real KDC and `rpc.gssd`. MDS floor krb5i and
`FLINT_PNFS_LAYOUTS_FOR_GSS=true` in every arm; 64 MiB written, read back
after a cache drop. Raw output: `results.txt`. Flavors per port are from a
pcap (20490 MDS, 20491/20492 the data servers).

| Arm | Data servers | Result |
|---|---|---|
| E | keys, krb5i floor; `sec=krb5i` | 32 MiB on each DS, MDS 16 KiB; DS compounds 180/175, all RPCSEC_GSS; read-back intact |
| F | the same; `sec=krb5p` | the same, sealed (176/175 GSS compounds); intact |
| G | krb5i floor, principals NOT in the KDC | the client tried AUTH_SYS at a DS twice, was refused (`server 127.0.0.2 requires stronger authentication`), and fell back to the MDS: 284 GSS compounds there, none more at the DSes; intact |
| H | no keytab, no floor (a misconfiguration the chart cannot render) | every RPCSEC_GSS call refused (1432/1464), and the client DOWNGRADED to AUTH_SYS, which the floorless DSes served (858/876 sys compounds): Phase 0's E1 again |

Findings:

- **E1 against a refusing DS (G): the client falls back to the MDS**, it
  does not fail and does not send data unauthenticated. The bytes still
  reach the DSes, but through the MDS's F66 relay over the MDS-DS control
  path, which has no TLS yet (Phase 5).
- **The DS floor is what protects (H).** A DS with Kerberos but without the
  floor would take the client's AUTH_SYS fallback. The chart renders the
  floor, the keytab and the MDS's layout grant from one switch
  (`pnfs.kerberos.dataServers`), so it cannot produce H. Follow-up: the DS
  could report its floor at registration and the MDS grant Kerberos
  callers layouts only for DSes that report one.
- The krb5i/krb5p arms read 64 MiB in about 5 s through two DSes with
  splice off (debug build): no throughput claim from this rig.
- The "KDC asked for" and "gssd upcalls" lines are empty: the rig's log
  scraping does not match this KDC's log format. Not used.
