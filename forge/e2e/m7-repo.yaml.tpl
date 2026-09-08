# M7 — the idle rung's durability. A repository is reaped for inactivity
# (`replicas: 0`, the pod destroyed, the cache an emptyDir that dies with
# it), a git request wakes a FRESH process, and the question is whether
# everything acknowledged before the reap is still there.
#
# This is forge's central claim — "acknowledged means durable" — put
# through a process death rather than argued from the code. The syncer
# holds unfolded packs, a snapshot cell and fold state in memory; the
# reap discards all of it and the successor rebuilds from the bucket.
#
# `suspendAfterSecs: 60` is the shortest rung that still leaves room for
# the push leg to finish without the ladder firing underneath it.
---
apiVersion: chert.us/v1alpha1
kind: FlintRepo
metadata: { name: m7, namespace: agents }
spec:
  projectId: m7
  bucket: $BUCKET
  keyPrefix: $PREFIX/m7/
  credentialsSecretRef: forge-creds
  defaultBranch: main
  idle:
    suspendAfterSecs: 60
  consumers:
    serviceAccounts: [scale-agent]
  branches:
    protected: [main]
    mergeInto:
      main: ["system:serviceaccount:agents:scale-agent"]
    agentPattern: "agent/*"
