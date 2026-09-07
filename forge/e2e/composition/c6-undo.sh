#!/usr/bin/env bash
# C6 — X15: is a force-pushed state still recoverable from the bucket?
#
# The memory double proves the shape; this proves it against a real S3
# API, with the real binary, its real sweep and its real CLI. The
# question the walgit comparison's P11 leg asked and forge could not
# answer: after a branch is force-pushed back, can the previous tip be
# recovered from the bucket alone?
#
# ANTI-VACUITY. Three traps this drill is built to avoid:
#   1. A pack that survives because NOTHING has swept is no evidence.
#      The drill makes the snapshot stop naming the pack and then runs
#      the sweep with the grace at 0, so the sweep really does consider
#      it and really does leave it alone.
#   2. "Recovered" must mean the objects, not the name. The tip is
#      recovered into a FRESH repository built from the point's packs,
#      and `fsck` has to pass there.
#   3. The control is the same run with the window at 0 — the code
#      before X15 — which must LOSE the pack. Without that leg a green
#      run could mean "nothing ever deletes anything".
#
#   bash forge/e2e/composition/c6-undo.sh
set -uo pipefail
cd "$(dirname "$0")/../../.."
export WORK=${WORK:-/tmp/fc-c6}
rm -rf "$WORK"; mkdir -p "$WORK"
source forge/e2e/composition/rig.sh
trap rig_clean EXIT
rig_init || { echo "rig_init failed"; exit 1; }

run_arm() {  # run_arm <tag> <prefix> <window-secs>
  local tag=$1 pfx=$2 window=$3
  head_ "$tag — undo window ${window}s"
  rig_purge "$pfx/"
  new_bare_repo "$WORK/$tag.git"
  forge_up "$tag" "$WORK/$tag.git" "$pfx" \
    "FLINT_FORGE_ALLOW_NON_FF=*" \
    "FLINT_FORGE_UNDO_WINDOW_SECS=$window" \
    "FLINT_FORGE_ORPHAN_GRACE_SECS=0" \
    "FLINT_FORGE_FOLD_FACTOR=0" \
    "FLINT_FORGE_REPACK_THRESHOLD=100000"
  wait_key "$pfx/git/epoch" 30 >/dev/null || { inconc "$tag: the syncer never claimed"; return 1; }
  new_clone "$WORK/$tag.git" "$WORK/$tag-wc"
  local wc="$WORK/$tag-wc"
  printf 'one\n' > "$wc/f"; git_c "$wc" add f; git_c "$wc" commit -qm one
  FORGE_SOCKET=/tmp/fc-$tag.sock push "$wc" HEAD:refs/heads/main > "$WORK/$tag-push1.log" 2>&1
  local t1; t1=$(git_c "$wc" rev-parse HEAD)
  printf 'two\n' >> "$wc/f"; git_c "$wc" add f; git_c "$wc" commit -qm two
  FORGE_SOCKET=/tmp/fc-$tag.sock push "$wc" HEAD:refs/heads/main > "$WORK/$tag-push2.log" 2>&1
  local t2; t2=$(git_c "$wc" rev-parse HEAD)
  sleep 2
  # The pack that carries t2 is the newest one the snapshot names.
  local packs2; packs2=$(s3_ls "$pfx/git/objects/pack/" | grep '\.pack$' | sed "s#.*/##")
  note "$tag: after two pushes the bucket holds $(echo "$packs2" | wc -w | tr -d ' ') pack(s)"

  # The force-push back to t1.
  git_c "$wc" reset -q --hard "$t1"
  FORGE_SOCKET=/tmp/fc-$tag.sock push "$wc" +HEAD:refs/heads/main > "$WORK/$tag-force.log" 2>&1
  sleep 2
  local live; live=$(aws s3api get-object --bucket "$BUCKET" --key "$pfx/git/snapshot" "$WORK/$tag-snap.json" >/dev/null 2>&1 && python3 -c "import json;print(json.load(open('$WORK/$tag-snap.json'))['refs'].get('refs/heads/main',''))")
  [ "$live" = "$t1" ] || {
    inconc "$tag: the force-push did not land (${live:0:8})"
    note "  push 1: $(tail -2 "$WORK/$tag-push1.log" | tr '\n' ' ')"
    note "  force:  $(tail -2 "$WORK/$tag-force.log" | tr '\n' ' ')"
    note "  syncer: $(tail -3 "$WORK/forge-$tag.log" | tr '\n' ' ')"
    forge_down "$tag"; return 1; }
  ok "$tag: the force-push landed — main is ${t1:0:8}, and ${t2:0:8} is reachable from nothing live"

  # What the bucket kept.
  # `s3_ls` prints the AWS CLI's "None" when a listing is empty, and a
  # drill that reads that as a key reports the opposite of the truth.
  local ukey; ukey=$(s3_ls "$pfx/git/undo/" | grep -E '\.json$' | sed 's#.*/##;s#\.json$##' | sort -n | tail -1)
  if [ "$window" = 0 ]; then
    [ -z "$ukey" ] && ok "control: with the window at 0 no undo point is written" \
                   || bad "control: an undo point appeared with the window at 0"
  else
    [ -n "$ukey" ] || { bad "$tag: no undo point after a force-push"; forge_down "$tag"; return 1; }
    aws s3api get-object --bucket "$BUCKET" --key "$pfx/git/undo/$ukey.json" "$WORK/$tag-undo.json" >/dev/null 2>&1
    local uoid; uoid=$(python3 -c "import json;print(json.load(open('$WORK/$tag-undo.json'))['refs'].get('refs/heads/main',''))")
    [ "$uoid" = "$t2" ] && ok "$tag: undo point seq $ukey holds the pre-force tip ${t2:0:8}" \
                        || bad "$tag: undo point holds ${uoid:0:8}, wanted ${t2:0:8}"
    note "$tag: --undo-list says: $(FLINT_FORGE_BUCKET=$BUCKET FLINT_FORGE_PREFIX=$pfx FLINT_FORGE_REPO=$WORK/$tag.git FLINT_FORGE_ENDPOINT=$ENDPOINT "$FORGE_BIN" --undo-list main 2>&1 | head -1)"
  fi

  # Now let a REAL base rebuild stop naming the pack that holds t2:
  # `pack-objects --all` packs by reachability, so the rewound commit
  # is dropped from the new pack and the old ones leave the snapshot.
  # Hand-editing the snapshot was the first shape of this drill and it
  # dropped the wrong pack — the pack list's order is git's, not the
  # push order, and the restored repository then failed fsck on a live
  # ref. Letting the server do it is both faithful and safe.
  local upacks; upacks=$(python3 - "$WORK/$tag-undo.json" <<'PY' 2>/dev/null
import json,sys
try: print(' '.join(json.load(open(sys.argv[1]))['packs']))
except Exception: print('')
PY
)
  local before; before=$(s3_ls "$pfx/git/objects/pack/" | grep '\.pack$' | sed 's#.*/##' | sort | tr '\n' ' ')
  # A DECOY: an object under the pack prefix that no snapshot and no
  # undo point names. The sweep must take it. Without it, "the
  # protected pack survived" cannot be told apart from "the sweep never
  # ran" — which is exactly what the first three shapes of this drill
  # measured, in both arms, for a hundred and fifty seconds.
  local decoy="pack-decafbad00000000000000000000000000$(printf '%05d' $RANDOM).pack"
  printf 'not a pack\n' > "$WORK/$tag-decoy"
  aws s3api put-object --bucket "$BUCKET" --key "$pfx/git/objects/pack/$decoy" --body "$WORK/$tag-decoy" >/dev/null 2>&1
  forge_down "$tag"
  forge_up "$tag-s" "$WORK/$tag.git" "$pfx" \
    "FLINT_FORGE_ALLOW_NON_FF=*" \
    "FLINT_FORGE_UNDO_WINDOW_SECS=$window" \
    "FLINT_FORGE_ORPHAN_GRACE_SECS=0" \
    "FLINT_FORGE_SWEEP_EVERY_SECS=0" \
    "FLINT_FORGE_FOLD_FACTOR=2" \
    "FLINT_FORGE_BASE_MIN_MIB=0" \
    "FLINT_FORGE_BASE_REBUILD_MIN_SECS=0" \
    "FLINT_FORGE_FOLD_MIN_MIB=0"
  local n=0
  while [ $n -lt 60 ]; do
    grep -q 'base rebuild committed\|fold committed' "$WORK/forge-$tag-s.log" 2>/dev/null && break
    sleep 1; n=$((n+1))
  done
  grep -q 'base rebuild committed\|fold committed' "$WORK/forge-$tag-s.log" 2>/dev/null \
    || { inconc "$tag: no rebuild in ${n}s — nothing stopped naming the old packs"; forge_down "$tag-s"; return 1; }
  # The maintenance tick — which runs the ledger sweep and, at
  # SWEEP_EVERY_SECS=0, the full one — fires once a MINUTE. The first
  # shape of this drill waited ten seconds, saw nothing swept in either
  # arm, and would have called the undo arm green on a sweep that never
  # ran. Wait for the sweep to say it did something, or for two ticks.
  # A sweep that ran BEFORE the rebuild says nothing about the packs the
  # rebuild has just unnamed — the control caught exactly that: it took
  # the decoy at start-up, the loop broke at once, and the dropped pack
  # had not yet been offered to any sweep. Count the sweeps the log has
  # now and wait for one MORE.
  local n0; n0=$(grep -cE 'swept [0-9]+ object|ledger sweep deleted' "$WORK/forge-$tag-s.log" 2>/dev/null)
  local w=0
  while [ $w -lt 200 ]; do
    [ "$(grep -cE 'swept [0-9]+ object|ledger sweep deleted' "$WORK/forge-$tag-s.log" 2>/dev/null)" -gt "$n0" ] && break
    sleep 5; w=$((w+5))
  done
  note "$tag: waited ${w}s for a sweep past the rebuild ($n0 -> $(grep -cE 'swept [0-9]+ object|ledger sweep deleted' "$WORK/forge-$tag-s.log" 2>/dev/null) sweep line(s))"
  if s3_has "$pfx/git/objects/pack/$decoy"; then
    inconc "$tag: the decoy orphan is still there — the sweep did not run, so nothing below is evidence"
    forge_down "$tag-s"; return 1
  fi
  ok "$tag: the sweep ran and took the decoy orphan"
  local live_packs; live_packs=$(aws s3api get-object --bucket "$BUCKET" --key "$pfx/git/snapshot" "$WORK/$tag-snap3.json" >/dev/null 2>&1 && python3 -c "import json;print(' '.join(json.load(open('$WORK/$tag-snap3.json'))['packs']))")
  local dropped=""
  for p in $before; do
    case " $live_packs " in *" $p "*) ;; *) dropped="$dropped $p";; esac
  done
  note "$tag: the rebuild left $(echo "$live_packs" | wc -w | tr -d ' ') pack(s) named; $(echo "$dropped" | wc -w | tr -d ' ') dropped"
  [ -n "$(echo "$dropped" | tr -d ' ')" ] || { inconc "$tag: the rebuild dropped nothing — nothing for the sweep to judge"; forge_down "$tag-s"; return 1; }

  local gone=0 kept=0 p
  for p in $dropped; do
    if s3_has "$pfx/git/objects/pack/$p"; then kept=$((kept+1)); else gone=$((gone+1)); fi
  done
  if [ "$window" = 0 ]; then
    [ "$gone" -gt 0 ] && ok "control: with no undo point the sweep took $gone dropped pack(s)" \
                      || bad "control: the sweep took nothing — the drill proves nothing about the other arm"
  else
    # Only the packs the point NAMES have to survive; a dropped pack the
    # point does not name is an ordinary orphan and may go.
    local prot=0 lost=0
    for p in $upacks; do
      case " $dropped " in
        *" $p "*) if s3_has "$pfx/git/objects/pack/$p"; then prot=$((prot+1)); else lost=$((lost+1)); fi;;
      esac
    done
    [ "$lost" -eq 0 ] && [ "$prot" -gt 0 ] \
      && ok "$tag: the sweep dropped $gone pack(s) and left the $prot the undo point names" \
      || bad "$tag: the sweep took $lost pack(s) the undo point names (protected $prot of $(echo "$upacks" | wc -w | tr -d ' '))"
    # And the recovery itself, from the bucket alone.
    local rec="$WORK/$tag-recovered.git"
    rm -rf "$rec"; git init -q --bare -b main "$rec"
    mkdir -p "$rec/objects/pack"
    for p in $upacks; do
      local stem=${p%.pack}
      aws s3api get-object --bucket "$BUCKET" --key "$pfx/git/objects/pack/$stem.pack" "$rec/objects/pack/$stem.pack" >/dev/null 2>&1
      aws s3api get-object --bucket "$BUCKET" --key "$pfx/git/objects/pack/$stem.idx"  "$rec/objects/pack/$stem.idx"  >/dev/null 2>&1
    done
    python3 - "$WORK/$tag-undo.json" > "$WORK/$tag-refs.txt" <<'PY'
import json,sys
for name,oid in json.load(open(sys.argv[1]))['refs'].items():
    print(f"update {name} {oid}")
PY
    git -C "$rec" update-ref --stdin < "$WORK/$tag-refs.txt" >/dev/null 2>&1
    local got; got=$(git -C "$rec" rev-parse refs/heads/main 2>/dev/null)
    if [ "$got" = "$t2" ] && git -C "$rec" fsck --connectivity-only --no-progress >/dev/null 2>&1; then
      ok "$tag: a fresh repository built from the point's packs has ${t2:0:8} and passes fsck"
    else
      bad "$tag: recovery gave ${got:0:8} (wanted ${t2:0:8}) or failed fsck"
    fi
  fi
  forge_down "$tag-s"
}

run_arm undo   c6/undo   604800
run_arm noundo c6/noundo 0
verdict "c6-undo"
