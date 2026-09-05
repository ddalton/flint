# The published-artifact rig: a store, a repository and one agent, and
# NOTHING built from this checkout.
#
# `__TAG__` is substituted with the tag the CHART names, read out of
# `flint-forge-chart/values.yaml` by the drill — not passed in. That is
# the point of the whole leg: the agent runs the same published image
# the chart would give a user, so if the chart names a tag that does
# not exist or does not work, this rig cannot stand up.
apiVersion: v1
kind: Namespace
metadata: { name: forge-system }
---
apiVersion: v1
kind: Namespace
metadata: { name: agents }
---
# The store. Ephemeral on purpose: this drill never asserts anything
# that survives MinIO being rescheduled, and the seed Job is recreated
# by the drill on every run rather than trusted to be Complete.
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
          image: minio/minio
          args: ["server", "/data", "--address", ":9000"]
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
  backoffLimit: 20
  template:
    spec:
      restartPolicy: OnFailure
      containers:
        - name: mc
          image: minio/mc
          command:
            - /bin/sh
            - -c
            - |
              until mc alias set m http://minio.forge-system.svc:9000 drill drillsecret; do sleep 2; done
              mc mb --ignore-existing m/s3bucket
---
# A long-lived mc, so the drill can read the bucket without a client on
# the Mac and without a credential leaving the cluster.
apiVersion: v1
kind: Pod
metadata: { name: mc-s3, namespace: forge-system }
spec:
  containers:
    - name: mc
      image: minio/mc
      command: ["/bin/sh", "-c", "until mc alias set m http://minio.forge-system.svc:9000 drill drillsecret; do sleep 2; done; sleep infinity"]
---
# The syncer reads these through envFrom, so THE KEYS ARE THE ENV VAR
# NAMES and must be AWS_* verbatim: a renamed key is a credential the
# SDK never looks for, and the failure is a timeout rather than a 403.
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
# One repository, with main PROTECTED — so the drill exercises the
# refs/for path a user actually meets, not only the permissive default.
apiVersion: chert.us/v1alpha1
kind: FlintRepo
metadata: { name: proj, namespace: agents }
spec:
  projectId: proj
  bucket: s3bucket
  keyPrefix: published/proj/
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
---
# The agent. One projected token and nothing else — no bucket name, no
# key, no sidecar. The image is the PUBLISHED forge-git image at the
# chart's own tag, which is what makes this pod evidence about a
# release rather than about this working tree.
apiVersion: v1
kind: Pod
metadata: { name: writer, namespace: agents, labels: { role: forge-agent } }
spec:
  serviceAccountName: forge-writer
  restartPolicy: Never
  containers:
    - name: agent
      image: dilipdalton/flint-forge-git:__TAG__
      # Always, not IfNotPresent: a stale layer from an earlier drill
      # would make this pod evidence about the wrong artifact.
      imagePullPolicy: Always
      command: ["sleep", "infinity"]
      volumeMounts:
        - { name: forge-token, mountPath: /var/run/secrets/forge, readOnly: true }
  volumes:
    - name: forge-token
      projected:
        sources:
          - serviceAccountToken:
              path: token
              # MUST equal the door's --git-audience.
              audience: forge.chert.us
              expirationSeconds: 3600
