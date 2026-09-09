# THE RESIDUE RIG: two repositories on one cluster, identical in every
# way except the two pack rules.
#
# `ctl` asks for neither and is the control. `treated` asks for both.
# Same bucket, same MinIO, same images, same agent image, same workload
# driven by the same script — so a difference in what the snapshot names
# is the rules and nothing else. `__TAG__` is substituted by the runner.
---
apiVersion: v1
kind: Namespace
metadata: { name: forge-system }
---
apiVersion: v1
kind: Namespace
metadata: { name: agents }
---
apiVersion: apps/v1
kind: Deployment
metadata: { name: minio, namespace: forge-system }
spec:
  replicas: 1
  selector: { matchLabels: { app: minio } }
  template:
    metadata: { labels: { app: minio } }
    spec:
      containers:
        - name: minio
          image: quay.io/minio/minio:latest
          args: ["server", "/data", "--console-address", ":9001"]
          env:
            - { name: MINIO_ROOT_USER, value: drill }
            - { name: MINIO_ROOT_PASSWORD, value: drillsecret }
          ports: [{ containerPort: 9000 }]
---
apiVersion: v1
kind: Service
metadata: { name: minio, namespace: forge-system }
spec:
  selector: { app: minio }
  ports: [{ port: 9000, targetPort: 9000 }]
---
apiVersion: batch/v1
kind: Job
metadata: { name: seed-bucket, namespace: forge-system }
spec:
  template:
    spec:
      restartPolicy: OnFailure
      containers:
        - name: mc
          image: quay.io/minio/mc:latest
          command: ["sh", "-c"]
          args:
            - |
              until mc alias set m http://minio.forge-system.svc:9000 drill drillsecret; do sleep 2; done
              mc mb --ignore-existing m/s3bucket
---
apiVersion: v1
kind: Pod
metadata: { name: mc-s3, namespace: forge-system }
spec:
  restartPolicy: Never
  containers:
    - name: mc
      image: quay.io/minio/mc:latest
      command: ["sh", "-c"]
      args:
        - |
          until mc alias set m http://minio.forge-system.svc:9000 drill drillsecret; do sleep 2; done
          sleep infinity
---
apiVersion: v1
kind: Secret
metadata: { name: forge-creds, namespace: agents }
type: Opaque
stringData:
  AWS_ACCESS_KEY_ID: drill
  AWS_SECRET_ACCESS_KEY: drillsecret
  AWS_REGION: us-east-1
---
apiVersion: v1
kind: ServiceAccount
metadata: { name: forge-writer, namespace: agents }
---
# ── THE CONTROL: neither rule ────────────────────────────────────────
apiVersion: chert.us/v1alpha1
kind: FlintRepo
metadata: { name: ctl, namespace: agents }
spec:
  projectId: ctl
  bucket: s3bucket
  keyPrefix: residue/ctl/
  endpoint: http://minio.forge-system.svc:9000
  credentialsSecretRef: forge-creds
  defaultBranch: main
  consumers:
    serviceAccounts: [forge-writer]
  branches:
    protected: [main]
    mergeInto:
      main: ["system:serviceaccount:agents:forge-writer"]
    agentPattern: "agent/*"
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
# ── THE TREATED ARM: both rules, and NOTHING else differs ────────────
apiVersion: chert.us/v1alpha1
kind: FlintRepo
metadata: { name: treated, namespace: agents }
spec:
  projectId: treated
  bucket: s3bucket
  keyPrefix: residue/treated/
  endpoint: http://minio.forge-system.svc:9000
  credentialsSecretRef: forge-creds
  defaultBranch: main
  consumers:
    serviceAccounts: [forge-writer]
  branches:
    protected: [main]
    mergeInto:
      main: ["system:serviceaccount:agents:forge-writer"]
    agentPattern: "agent/*"
__KNOBS__
  packs:
    nameAcceptedSet: true
    reclaimAtRest: true
---
apiVersion: v1
kind: Pod
metadata: { name: writer, namespace: agents, labels: { role: forge-agent } }
spec:
  serviceAccountName: forge-writer
  restartPolicy: Never
  containers:
    - name: agent
      image: dilipdalton/flint-forge-git:__TAG__
      # IfNotPresent, not Always: these images are `kind load`ed and
      # exist on no registry, so Always would fail to pull.
      imagePullPolicy: IfNotPresent
      command: ["sleep", "infinity"]
      volumeMounts:
        - { name: forge-token, mountPath: /var/run/secrets/forge, readOnly: true }
  volumes:
    - name: forge-token
      projected:
        sources:
          - serviceAccountToken:
              path: token
              audience: forge.chert.us
              expirationSeconds: 3600
