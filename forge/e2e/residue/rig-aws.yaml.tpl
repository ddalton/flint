# THE RESIDUE RIG, on a REAL CLUSTER against REAL S3.
#
# Same two arms as `rig.yaml.tpl` and the same one-field difference; what
# changes is the store. No MinIO, no endpoint override, and the
# credentials Secret is written by the runner from the KEYFILE rather
# than living here — a bucket and a key do not belong in the tree.
#
# `__BUCKET__` and `__TAG__` are substituted by run-residue.sh.
---
apiVersion: v1
kind: Namespace
metadata:
  name: forge-system
---
apiVersion: v1
kind: Namespace
metadata:
  name: agents
---
apiVersion: v1
kind: ServiceAccount
metadata:
  name: forge-writer
  namespace: agents
---
apiVersion: chert.us/v1alpha1
kind: FlintRepo
metadata:
  name: ctl
  namespace: agents
spec:
  projectId: ctl
  bucket: __BUCKET__
  keyPrefix: residue/ctl/
  credentialsSecretRef: forge-creds
  defaultBranch: main
  consumers:
    serviceAccounts:
    - forge-writer
  branches:
    protected:
    - main
    mergeInto:
      main:
      - system:serviceaccount:agents:forge-writer
    agentPattern: agent/*
  # EXPLICITLY OFF. Both rules DEFAULT ON since 2026-09-09, so an
  # ABSENT block would give the control the rules too and the two arms
  # would be identical — the drill would compare a repository against
  # itself and report a green that means nothing, which is exactly the
  # failure its first run already made once. The control has to say NO
  # out loud, and being able to say it is why the operator renders
  # `false` as an explicit 0 rather than as silence.
  packs:
    nameAcceptedSet: false
    reclaimAtRest: false
__KNOBS__
---
apiVersion: chert.us/v1alpha1
kind: FlintRepo
metadata:
  name: treated
  namespace: agents
spec:
  projectId: treated
  bucket: __BUCKET__
  keyPrefix: residue/treated/
  credentialsSecretRef: forge-creds
  defaultBranch: main
  consumers:
    serviceAccounts:
    - forge-writer
  branches:
    protected:
    - main
    mergeInto:
      main:
      - system:serviceaccount:agents:forge-writer
    agentPattern: agent/*
  packs:
    nameAcceptedSet: true
    reclaimAtRest: true
__KNOBS__
---
apiVersion: v1
kind: Pod
metadata:
  name: writer
  namespace: agents
  labels:
    role: forge-agent
spec:
  serviceAccountName: forge-writer
  restartPolicy: Never
  containers:
  - name: agent
    image: dilipdalton/flint-forge-git:__TAG__
    imagePullPolicy: IfNotPresent
    command:
    - sleep
    - infinity
    volumeMounts:
    - name: forge-token
      mountPath: /var/run/secrets/forge
      readOnly: true
  volumes:
  - name: forge-token
    projected:
      sources:
      - serviceAccountToken:
          path: token
          audience: forge.chert.us
          expirationSeconds: 3600
