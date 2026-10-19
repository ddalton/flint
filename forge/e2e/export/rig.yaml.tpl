# The export drill's rig (f9-export.sh, g-agent-gvisor.sh). Two
# repositories in one bucket, one per export shape, plus the in-cluster
# readers every bucket read goes through (NO EGRESS: nothing is read
# from outside the region). $BUCKET, $PREFIX and $TAG come from
# forge/e2e/export/run.sh.
---
apiVersion: v1
kind: ServiceAccount
metadata: { name: agent-runner, namespace: agents }
---
# lean shape (the default)
apiVersion: chert.us/v1alpha1
kind: FlintRepo
metadata: { name: exl, namespace: agents }
spec:
  projectId: exl
  bucket: $BUCKET
  keyPrefix: $PREFIX/exl/
  credentialsSecretRef: forge-creds
  defaultBranch: main
  consumers: { serviceAccounts: [agent-runner] }
  export: { prefix: $PREFIX/exl-export/, refs: [refs/heads/main], everySecs: 10 }
---
# plain shape. main is protected: it moves by a direct push only for the
# seed (pushers) and otherwise by `git propose` (mergeInto), which is
# what GA2 exercises. keep 1 and a 90 s grace so collection is observable
# inside one drill.
apiVersion: chert.us/v1alpha1
kind: FlintRepo
metadata: { name: exp, namespace: agents }
spec:
  projectId: exp
  bucket: $BUCKET
  keyPrefix: $PREFIX/exp/
  credentialsSecretRef: forge-creds
  defaultBranch: main
  consumers: { serviceAccounts: [agent-runner] }
  branches:
    protected: [main]
    pushers:   { main: ["system:serviceaccount:agents:agent-runner"] }
    mergeInto: { main: ["system:serviceaccount:agents:agent-runner"] }
    agentPattern: "agent/*"
  export: { prefix: $PREFIX/exp-export/, refs: [refs/heads/main], everySecs: 10, format: plain, keep: 1, graceSecs: 90 }
---
# Every bucket read in the drill: the AWS CLI, in the cluster's region.
apiVersion: v1
kind: Pod
metadata: { name: s3cli, namespace: agents }
spec:
  restartPolicy: Never
  containers:
    - name: c
      image: public.ecr.aws/aws-cli/aws-cli:2.27.0
      command: ["sleep", "infinity"]
      envFrom: [{ secretRef: { name: forge-creds } }]
---
# The lean shape's honest reader: flint-sync itself, read-only, checking
# the export out into an emptyDir through the manifest.
apiVersion: v1
kind: Pod
metadata: { name: leanreader, namespace: agents }
spec:
  restartPolicy: Never
  containers:
    - name: c
      image: dilipdalton/flint-forge-syncer:$TAG
      command: ["sleep", "infinity"]
      envFrom: [{ secretRef: { name: forge-creds } }]
      env:
        - { name: FLINT_SYNC_BUCKET, value: $BUCKET }
        - { name: FLINT_SYNC_PREFIX, value: $PREFIX/exl-export }
        - { name: FLINT_SYNC_ROOT, value: /ws }
        - { name: FLINT_SYNC_ACCESS, value: read }
      volumeMounts: [{ name: ws, mountPath: /ws }]
  volumes: [{ name: ws, emptyDir: {} }]
