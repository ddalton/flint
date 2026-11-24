#!/bin/bash
# sjob5.sh (= sjob4.sh through c31's inbox, s3://flint-c31-20261031: 192 cores; rev 17: MODEL defaults to sproof/model/FlintCatchupS-r17.tla, md5 61ae6829 asserted; STRETCH 4) <name> <threads> [<begin-line> <end-line>]: one tlapm job on c33 through its inbox.  The job carries
# FlintCatchupS.tla rev 12 (8e54566d, as-run/ms12), formal/FlintCatchupSInd.tla and formal/FlintCatchupSProof.tla as
# they stand (md5s in its summary); tlapm --toolbox <bl> <el> (0 0 = the whole file), stretch 3, no --cleanfp;
# SEED=<dir on the instance> copies that dir's .tlacache in first (e.g. /work/run-t1); CAP=<s> (default 3600);
# PROOF=<file> sends that file as FlintCatchupSProof.tla (default formal/FlintCatchupSProof.tla).
# STRETCH=<n> (default 3); MODEL=<file> sends that FlintCatchupS.tla (default rev 12, as-run/ms12; its md5 is printed,
# not asserted); CLEANFP=1 passes --cleanfp.
# Waits for the job's DONE line, pulls out/jobs/<name>/ into results/.../jobs/<name>/ and prints the summary.
set -euo pipefail
export AWS_PROFILE=trove-admin
n=$1; th=$2; bl=${3:-0}; el=${4:-0}; BK=flint-c31-20261031; F=$HOME/github/flint-rebuild-proof/formal
P=$F/results/2026-10-31-c31-ec2; J=$P/jobsrc/$n
[ -e $J ] && { echo "job $n exists locally"; exit 1; }
mkdir -p $J
cp $F/results/2026-10-29-flintcatchupt/as-run/ms12/FlintCatchupS.tla $F/FlintCatchupSInd.tla $J/
cp ${PROOF:-$F/FlintCatchupSProof.tla} $J/FlintCatchupSProof.tla
cp ${MODEL:-$F/sproof/model/FlintCatchupS-r17.tla} $J/FlintCatchupS.tla; [ -n "${MODEL:-}" ] || [ "$(md5 -q $J/FlintCatchupS.tla | cut -c1-8)" = 61ae6829 ] || { echo "model md5"; exit 1; }
echo ${CAP:-3600} > $J/cap
cat > $J/job.sh <<JOB
md5() { md5sum < \$1 | cut -c1-8; }
SEED=${SEED:-}
[ -n "\$SEED" ] && [ -d "\$SEED/.tlacache" ] && cp -r \$SEED/.tlacache .
echo "S \$(md5 FlintCatchupS.tla) ind \$(md5 FlintCatchupSInd.tla) proof \$(md5 FlintCatchupSProof.tla) threads $th stretch ${STRETCH:-4} range $bl $el seed [\$SEED] start \$(date -u +%TZ)" > \$OUT/summary.txt
t0=\$(date +%s)
/usr/bin/time -v -o \$OUT/time.txt \$TL --toolbox $bl $el $( [ -n "${CLEANFP:-}" ] && echo --cleanfp ) --stretch ${STRETCH:-4} --threads $th FlintCatchupSProof.tla > \$OUT/tlapm.out 2>&1
rc=\$?
python3 \$SUMM \$OUT/tlapm.out >> \$OUT/summary.txt
echo "DONE rc=\$rc secs=\$(( \$(date +%s) - t0 )) maxrss=\$(grep -oP "Maximum resident set size \(kbytes\): \K\d+" \$OUT/time.txt)KB \$(date -u +%TZ)" >> \$OUT/summary.txt
JOB
(cd $J && COPYFILE_DISABLE=1 tar --no-xattrs -czf ../$n.tgz .) && shasum -a 256 $P/jobsrc/$n.tgz | cut -d' ' -f1 > $P/jobsrc/$n.sha256
aws s3 cp $P/jobsrc/$n.sha256 s3://$BK/inbox/$n.sha256 --region us-west-1 --quiet
aws s3 cp $P/jobsrc/$n.tgz s3://$BK/inbox/$n.tgz --region us-west-1 --quiet
echo "submitted $n (proof $(md5 -q $J/FlintCatchupSProof.tla | cut -c1-8), threads $th, range $bl $el, seed [${SEED:-}]) $(date -u +%TZ)"
until aws s3 cp s3://$BK/out/jobs.txt - --region us-west-1 2>/dev/null | grep -qE "^JOB $n (DONE|SHA MISMATCH|SKIPPED)"; do sleep 15; done
mkdir -p $P/jobs/$n && aws s3 sync s3://$BK/out/jobs/$n $P/jobs/$n --region us-west-1 --quiet
aws s3 cp s3://$BK/out/jobs.txt - --region us-west-1 | grep "^JOB $n "
cat $P/jobs/$n/summary.txt 2>/dev/null
