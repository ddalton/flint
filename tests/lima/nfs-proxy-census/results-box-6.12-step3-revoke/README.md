# step 3 `revoke` — a hub loses its state; does the other workspace notice?

Box, Linux 6.12.0, 2026-09-30. `step3-drills.sh revoke`; design §4
(Leases, hub-loses-state row) and §8 step 3.

One process holds an open and a write lock on `ws-a/f` and `ws-b/f`
through the proxy (one client, one session). Hub A is stopped, its
`locks`/`stateids`/`sessions`/`clients` tables are emptied (server id and
filehandles kept), and it is restarted. The process then writes through
both fds, ws-a first. A contender on each hub (a direct mount, a
different client) then tries the lock.

| Arm (file) | ws-a write | ws-b write | kernel | contender A / B | result |
|---|---|---|---|---|---|
| design: loss, no flag (`design.txt`, fixed build `design-fixed.txt`) | EIO | ok | lost 1 locks | ACQUIRED / REFUSED | 7/7 |
| control: nothing lost (`control-noloss.txt`) | ok | ok | — | REFUSED / REFUSED | 6/6 |
| known-bad 0x20 ADMIN (`knownbad-0x20.txt`) | EIO | **EIO** | lost 2 locks | ACQUIRED / REFUSED | 6/8 |
| known-bad 0x10 EXPIRED_SOME (`knownbad-0x10.txt`) | EIO | **EIO** | lost 2 locks | ACQUIRED / REFUSED | 6/8 |
| known-bad 0x08 EXPIRED_ALL (`knownbad-0x08.txt`) | EIO | **EIO** | lost 2 locks | ACQUIRED / REFUSED | 6/8 |
| known-bad 0x40 RECALLABLE (`knownbad-0x40.txt`) | EIO | ok | lost 1 locks | ACQUIRED / REFUSED | 8/8 |

Known-bad arms run a proxy built with `../revoke-inject.patch`, which ORs
the flag into the first forwarded downstream `SEQUENCE` after the loss
(its log line `KNOWN-BAD: injected sr_status_flags 0x..` was checked in
each arm). Each failing arm fails
exactly the two ws-b checks. In those arms hub B still holds ws-b's lock
(contender B refused): the client gave up a lock the server never took.

What it settled:
- Per-op stateid errors alone keep the recovery per-state (§9 answered).
- 0x08 and 0x10 are as client-wide as 0x20. The proxy passed both; now it
  passes 0x40 only (`HUB_FLAGS_PASSED`, test
  `only_a_per_state_revocation_flag_reaches_the_client`: 0x58 reached the
  client from a hub raising 0x78 before the fix, 0x40 after).

Files: `<arm>.txt` drill output, `<arm>.dmesg.delta` kernel lines during
the arm. The proxy and hub logs are `*.log` (ignored by the repo); they
stay on the box in `~/nfs-proxy-revoke-results/`.
