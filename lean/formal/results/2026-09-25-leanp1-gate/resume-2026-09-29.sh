#!/bin/bash
# Resume after the 2026-09-29 00:30 UTC pause. On the box: ~/resume-2026-09-29.sh
# Every runner skips what is already decided and resumes a world that has a
# checkpoint (-recover; each was test-recovered on a copy before the pause).
#  1. gate d: LeanP1DeleteOverrideOff (checkpoint), then 19 decisive worlds,
#     then the 2 RECORD worlds (1 h cap). LiveHoldsSmall is already decided,
#     so gate d is done at SMALLDONE: this writes LEANP1DONE after it.
#  2. the deep LeanP1Holds (tlc-rs 551c2ffe) waits for LEANP1DONE.
#  3. Forge rewind: KeepsNamedRetentionLoss (checkpoint), then Holds.
#  4. the gate-shape sizing: MCLeanP1GateSeq3Copies0 (checkpoint), then the
#     two larger bounds. It was paused to give gate d the cores: start it
#     only if the box has room (RESUME_SHAPE=1).
set -u
T=/home/ddalton
cd $T/lean-leanp1-2026-09-25d && (setsid nohup bash -c "./run-leanp1-small-tlcrs.sh $T/tlc-rs-92c7a987/formal/tlc-rs/target/release/tlc-rs 92c7a987; grep -q SMALLDONE RESULTS.txt && echo LEANP1DONE >> RESULTS.txt" > runner-small-tlcrs.log 2>&1 < /dev/null &)
cd $T/lean-leanp1-deep-2026-09-27 && (setsid nohup ./run-deep-tlcrs.sh $T/tlc-rs-551c2ffe/formal/tlc-rs/target/release/tlc-rs 551c2ffe > run-tlcrs.log 2>&1 < /dev/null &)
cd $T/forge-needed-2026-09-26 && (setsid nohup ./run-rewind-tlcrs.sh $T/tlc-rs-badaa979/formal/tlc-rs/target/release/tlc-rs badaa979 5ee1b260cba85f0cbfe45829e2493a77 > run-rewind-tlcrs.log 2>&1 < /dev/null &)
if [ "${RESUME_SHAPE:-0}" = 1 ]; then
  cd $T/lean-leanp1-gate-shape-2026-09-27 && (setsid nohup ./run-shape-tlcrs.sh $T/tlc-rs-badaa979/formal/tlc-rs/target/release/tlc-rs badaa979 > run-tlcrs.log 2>&1 < /dev/null &)
fi
sleep 5; pgrep -af "run-leanp1-small|run-deep-tlcrs|run-rewind-tlcrs|run-shape-tlcrs|^$T/tlc-rs" | grep -v pgrep | cut -c1-150
