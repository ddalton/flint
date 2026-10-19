#!/usr/bin/env bash
# FALSIFIER 9 — the export, in both shapes (be41f1e8).
#
#   BUCKET=... PREFIX=... TAG=... ./forge/e2e/f9-export.sh
#   (after forge/e2e/export/run.sh has deployed the chart and export/rig.yaml.tpl)
#
# PLAIN (repo exp): commits/<sha>/<path> + manifests/<sha>.json + CURRENT,
#   which anything that reads S3 can use:
#   P1 CURRENT names main            P2 every file byte-identical to git show
#   P3 nothing the tree lacks        P4 the manifest lists exactly the tree
#   P5 a second push moves CURRENT; the new prefix lacks the deleted file;
#      the superseded prefix is still whole inside graceSecs
#   P6 past the grace (keep 1), a third push collects the oldest prefix and
#      its manifest; the superseded-but-young one and CURRENT's survive
#
# LEAN (repo exl): since 1.57.0 the objects are files/<path>@<flush> and only
#   the manifest names the tree, so this drill NO LONGER reads files/<path>
#   with a bare GET (the old (a) check, which could only fail). It reads the
#   export the way a consumer must — `flint-sync checkout`, read-only:
#   L1 the export record has published main
#   L2 a read-only checkout is byte-identical to the tree, and holds nothing more
#   L3 the bucket holds handles (`files/<path>@<flush>`), not bare paths
#   L4 a three-file push writes about three new file objects (O(changed)),
#      and a fresh checkout reads the new bytes
#
# Every bucket read runs in the cluster (lib.sh: s3cli). PASS/FAIL/PENDING;
# exit 0 / 1 / 2.
set -uo pipefail
here=$(cd "$(dirname "$0")" && pwd)
. "$here/export/lib.sh"

sha_git(){ gitq "$1" show "$2:$3" | sha256sum | cut -d' ' -f1; }
sha_s3(){ s3cat "$1" | sha256sum | cut -d' ' -f1; }
current(){ s3cat "$PREFIX/exp-export/CURRENT" | tr -d '\n'; }
export_record(){ kubectl exec -n "$NS" deploy/forge-exl -c syncer -- \
  sh -c "cat /repo/$NS/exl.git/flint-forge/export/tree.record.json 2>/dev/null" | jq -r '.commit // "none"' 2>/dev/null; }

echo "══ F9: the export, plain and lean ══"
agent_pod seeder runc forge || { bad "the seeder pod never became Ready"; summary; exit $?; }

# Seed both repositories with one commit on main.
seed(){ asg seeder "set -e; rm -rf /work/$1 && git clone -q $DOOR/git/$NS/$1.git /work/$1 2>/dev/null || { mkdir -p /work/$1 && cd /work/$1 && git init -q -b main && git remote add origin $DOOR/git/$NS/$1.git; }
  cd /work/$1; git checkout -q -B main
  mkdir -p dir data; echo alpha > a.txt; echo beta > b.txt; echo gamma > dir/c.txt
  for i in 0 1 2 3 4 5; do head -c 4096 /dev/urandom > data/blob-\$i.bin; done
  git add -A && git commit -qm seed && git push -q origin HEAD:refs/heads/main && git rev-parse HEAD"; }
C1P=$(seed exp | tail -1); C1L=$(seed exl | tail -1)
echo "seeded exp=$C1P exl=$C1L"
[ "$(gitq exp rev-parse refs/heads/main)" = "$C1P" ] || bad "exp main is not the seed ($C1P)"

# ── PLAIN ──────────────────────────────────────────────────────────────
echo "── plain (exp) ──"
if wait_for "$C1P" 180 current; then ok "P1 CURRENT names main ($C1P)"; else bad "P1 CURRENT is '$(current)', not $C1P"; fi
TREE=$(gitq exp ls-tree -r --name-only "$C1P" | sort)
N=0; D=0
while read -r p; do [ -z "$p" ] && continue; N=$((N+1))
  [ "$(sha_git exp "$C1P" "$p")" = "$(sha_s3 "$PREFIX/exp-export/commits/$C1P/$p")" ] || { D=$((D+1)); echo "      DIFFERS: $p"; }
done <<< "$TREE"
[ "$N" -gt 0 ] && [ "$D" = 0 ] && ok "P2 all $N files byte-identical to git show" || bad "P2 $D of $N files differ"
KEYS=$(s3keys "$PREFIX/exp-export/commits/$C1P/" | sed "s|^$PREFIX/exp-export/commits/$C1P/||" | sort)
[ "$KEYS" = "$TREE" ] && ok "P3 commits/<sha>/ holds exactly the tree ($(echo "$KEYS" | wc -l | tr -d ' ') keys)" \
  || bad "P3 key set differs: $(comm -3 <(echo "$TREE") <(echo "$KEYS") | tr '\n' ' ')"
MF=$(s3cat "$PREFIX/exp-export/manifests/$C1P.json" | jq -r '.files[].path' | sort)
[ "$MF" = "$TREE" ] && ok "P4 manifests/$C1P.json lists exactly the tree" || bad "P4 manifest differs from the tree"

C2P=$(asg seeder "set -e; cd /work/exp; echo ALPHA2 > a.txt; git rm -q b.txt; git commit -qam two; git push -q origin HEAD:refs/heads/main; git rev-parse HEAD" | tail -1)
if wait_for "$C2P" 180 current; then
  T2=$(date +%s)
  NOB=$(s3keys "$PREFIX/exp-export/commits/$C2P/b.txt" | wc -l | tr -d ' ')
  OLDB=$(s3keys "$PREFIX/exp-export/commits/$C1P/" | wc -l | tr -d ' ')
  A2=$(s3cat "$PREFIX/exp-export/commits/$C2P/a.txt")
  [ "$NOB" = 0 ] && [ "$A2" = ALPHA2 ] && [ "$OLDB" = "$N" ] \
    && ok "P5 CURRENT moved to $C2P; b.txt absent from it; a.txt new; $C1P still whole ($OLDB keys) inside the grace" \
    || bad "P5 new-prefix b.txt=$NOB a.txt='$A2' old-prefix keys=$OLDB (want 0, ALPHA2, $N)"
else bad "P5 CURRENT never moved to $C2P (is '$(current)')"; T2=$(date +%s); fi

# Past c1's grace (90 s from its supersession), then a third commit.
sleep $(( 100 - ($(date +%s) - T2) > 0 ? 100 - ($(date +%s) - T2) : 0 ))
C3P=$(asg seeder "set -e; cd /work/exp; echo three > dir/c.txt; git commit -qam three; git push -q origin HEAD:refs/heads/main; git rev-parse HEAD" | tail -1)
if wait_for "$C3P" 180 current; then
  sleep 5
  G1=$(s3keys "$PREFIX/exp-export/commits/$C1P/" | wc -l | tr -d ' ')
  M1=$(s3keys "$PREFIX/exp-export/manifests/$C1P.json" | wc -l | tr -d ' ')
  G2=$(s3keys "$PREFIX/exp-export/commits/$C2P/" | wc -l | tr -d ' ')
  M2=$(s3keys "$PREFIX/exp-export/manifests/$C2P.json" | wc -l | tr -d ' ')
  G3=$(s3keys "$PREFIX/exp-export/commits/$C3P/" | wc -l | tr -d ' ')
  [ "$G1" = 0 ] && [ "$M1" = 0 ] && [ "$G2" -gt 0 ] && [ "$M2" = 1 ] && [ "$G3" -gt 0 ] \
    && ok "P6 $C1P collected (0 keys, no manifest); $C2P kept inside its grace ($G2 keys); CURRENT's $C3P present ($G3)" \
    || bad "P6 c1 keys=$G1 manifest=$M1, c2 keys=$G2 manifest=$M2, c3 keys=$G3 (want 0,0,>0,1,>0)"
else bad "P6 CURRENT never moved to $C3P"; fi

# ── LEAN ───────────────────────────────────────────────────────────────
echo "── lean (exl) ──"
if wait_for "$C1L" 180 export_record; then ok "L1 the export record has published main ($C1L)"; else bad "L1 export record is '$(export_record)', not $C1L"; fi
# checkout_cmp <dir> <commit>: a read-only flint-sync checkout, compared to the tree.
checkout_cmp(){ local dir=$1 c=$2 tree got n=0 d=0
  kubectl exec -n "$NS" leanreader -- sh -c "rm -rf $dir && mkdir -p $dir && FLINT_SYNC_ROOT=$dir flint-sync checkout >/tmp/co.log 2>&1" \
    || { echo "      checkout failed: $(kubectl exec -n "$NS" leanreader -- tail -3 /tmp/co.log | tr '\n' ' ')"; return 1; }
  tree=$(gitq exl ls-tree -r --name-only "$c" | sort)
  got=$(kubectl exec -n "$NS" leanreader -- sh -c "cd $dir && find . -type f -not -path './.flint*' | sed 's|^\./||'" | sort)
  [ "$got" = "$tree" ] || { echo "      tree mismatch: $(comm -3 <(echo "$tree") <(echo "$got") | tr '\n' ' ')"; return 1; }
  while read -r p; do n=$((n+1))
    [ "$(sha_git exl "$c" "$p")" = "$(kubectl exec -n "$NS" leanreader -- sha256sum "$dir/$p" | cut -d' ' -f1)" ] || { d=$((d+1)); echo "      DIFFERS: $p"; }
  done <<< "$tree"
  [ "$d" = 0 ] && echo "$n"; }
R=$(checkout_cmp /ws/c1 "$C1L"); case "$R" in ''|*[!0-9]*) bad "L2 the read-only checkout does not match the tree $R";; *) ok "L2 a read-only flint-sync checkout is the tree: $R files byte-identical, nothing more";; esac
H=$(s3keys "$PREFIX/exl-export/files/" | grep -c '@')
BARE=$(s3keys "$PREFIX/exl-export/files/a.txt" | grep -cx "$PREFIX/exl-export/files/a.txt")
[ "$H" -gt 0 ] && [ "$BARE" = 0 ] && ok "L3 the bucket holds $H handle objects (files/<path>@<flush>) and no bare files/a.txt" \
  || bad "L3 handles=$H bare-a.txt=$BARE (a lean export is not a plain tree)"
BEFORE=$(s3keys "$PREFIX/exl-export/files/" | sort)
C2L=$(asg seeder "set -e; cd /work/exl; for i in 0 1 2; do echo changed-\$i > data/blob-\$i.bin; done; git commit -qam three-files; git push -q origin HEAD:refs/heads/main; git rev-parse HEAD" | tail -1)
if wait_for "$C2L" 180 export_record; then
  NEW=$(comm -13 <(echo "$BEFORE") <(s3keys "$PREFIX/exl-export/files/" | sort) | wc -l | tr -d ' ')
  R=$(checkout_cmp /ws/c2 "$C2L")
  if [ "$NEW" -gt 0 ] && [ "$NEW" -le 6 ] && [ -n "$R" ] && [ "${R//[0-9]/}" = "" ]; then
    ok "L4 a three-file push wrote $NEW new file objects, and a fresh checkout reads the new bytes ($R files)"
  else bad "L4 new file objects=$NEW (want 1..6), checkout: $R"; fi
else bad "L4 the export never followed $C2L"; fi

kubectl -n "$NS" delete pod seeder --wait=false >/dev/null 2>&1
summary
