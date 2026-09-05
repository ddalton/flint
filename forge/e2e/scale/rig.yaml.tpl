# The SCALE rig: two repositories on one bucket, and one agent whose
# working space is an emptyDir large enough to build a multi-GiB
# repository in.
#
# `big` is the repository under test. `small` is the takeover leg's
# CONTROL — the same pods, the same challenger, a restore that finishes
# in seconds — and the durability leg's subject, because a kill there
# costs a ten-second restore rather than a three-minute one.
#
# Neither carries export, bundles, LFS or the idle rung. This rig
# measures the push and the restore; an export of an 8 GiB `main` every
# 30 s would be the loudest thing on the node and would confound both.
#
# $BUCKET, $PREFIX and $TAG are substituted by deploy.sh. The audience
# on the agent's token MUST equal the chart's door.audience; a token for
# any other audience is perfectly valid and refused by every request.
---
apiVersion: v1
kind: ServiceAccount
metadata: { name: scale-agent, namespace: agents }
---
apiVersion: chert.us/v1alpha1
kind: FlintRepo
metadata: { name: big, namespace: agents }
spec:
  projectId: big
  bucket: $BUCKET
  keyPrefix: $PREFIX/big/
  credentialsSecretRef: forge-creds
  defaultBranch: main
  consumers:
    serviceAccounts: [scale-agent]
  branches:
    protected: [main]
    mergeInto:
      main: ["system:serviceaccount:agents:scale-agent"]
    agentPattern: "agent/*"
---
apiVersion: chert.us/v1alpha1
kind: FlintRepo
metadata: { name: small, namespace: agents }
spec:
  projectId: small
  bucket: $BUCKET
  keyPrefix: $PREFIX/small/
  credentialsSecretRef: forge-creds
  defaultBranch: main
  consumers:
    serviceAccounts: [scale-agent]
  branches:
    protected: [main]
    mergeInto:
      main: ["system:serviceaccount:agents:scale-agent"]
    agentPattern: "agent/*"
---
# The agent: one projected token, and /work on an emptyDir. /tmp would
# be the container's writable layer, which lives on the node's ROOT
# disk — 8 GiB on a trove instance — and the repositories this drill
# builds do not fit there. The emptyDir lands wherever the kubelet's
# pod directory does, which prep-nodes.sh puts on the local NVMe.
apiVersion: v1
kind: Pod
metadata: { name: agent1, namespace: agents, labels: { role: forge-agent } }
spec:
  serviceAccountName: scale-agent
  restartPolicy: Never
  containers:
    - name: agent
      image: dilipdalton/flint-forge-git:$TAG
      command: ["sleep", "infinity"]
      volumeMounts:
        - { name: forge-token, mountPath: /var/run/secrets/forge, readOnly: true }
        - { name: work, mountPath: /work }
  volumes:
    - name: work
      emptyDir: {}
    - name: forge-token
      projected:
        sources:
          - serviceAccountToken:
              path: token
              audience: forge.chert.us
              expirationSeconds: 3600
