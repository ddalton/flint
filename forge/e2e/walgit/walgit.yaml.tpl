# The walgit arm of the comparison (docs/plans/flint-forge-simplification-2026-09-05.md §9).
#
# One walgit instance, token auth, the S3 backend on the campaign's
# bucket under its own prefix, the cache on an emptyDir that
# prep-nodes.sh has put on the node's NVMe — the same disk forge's
# repository cache lives on. Pinned to the node the image was imported
# on (imagePullPolicy Never: the image is built on the cluster and never
# pushed to a registry). The agent reaches it at
# http://walgit.agents.svc:8080/acme/<repo>.git with a bearer token.
#
# $BUCKET $WPREFIX $WALGIT_IMAGE $WALGIT_NODE $WALGIT_TOKEN are substituted by deploy-walgit.sh.
---
apiVersion: v1
kind: Secret
metadata: { name: walgit-token, namespace: agents }
type: Opaque
stringData:
  WALGIT_TOKEN_AGENT: "$WALGIT_TOKEN"
---
apiVersion: v1
kind: ConfigMap
metadata: { name: walgit-config, namespace: agents }
data:
  walgit.toml: |
    [server]
    listen = "0.0.0.0:8080"
    public_url = "http://walgit.agents.svc:8080"
    auto_create_on_push = true
    roles = []
    [server.tls]
    mode = "off"
    [server.auth]
    mode = "token"
    anonymous_read = false
    tokens = [ { principal = "agent", token_env = "WALGIT_TOKEN_AGENT", write = true, admin = true } ]
    [store]
    backend = "s3"
    bucket = "$BUCKET"
    prefix = "$WPREFIX"
    [store.s3]
    endpoint = "https://s3.us-west-1.amazonaws.com"
    region = "us-west-1"
    access_key_env = "AWS_ACCESS_KEY_ID"
    secret_key_env = "AWS_SECRET_ACCESS_KEY"
    force_path_style = false
    [cache]
    dir = "/var/lib/walgit"
    mode = "disk"
    [wal]
    batch_window = "400ms"
    [maintenance]
    interval = "15s"
    disk = "ssd"
    [[bundles.strategy]]
    name = "weekly"
    kind = "full"
    schedule = "0 0 23 * * Sun"
    keep = 1
    backfill_max = 1
    [[bundles.strategy]]
    name = "daily"
    kind = "incremental"
    base = "weekly"
    schedule = "0 0 23 * * *"
    chain = true
    [bundles]
    require = []
    [telemetry]
    log_format = "pretty"
---
apiVersion: apps/v1
kind: Deployment
metadata: { name: walgit, namespace: agents, labels: { app: walgit } }
spec:
  replicas: 1
  strategy: { type: Recreate }
  selector: { matchLabels: { app: walgit } }
  template:
    metadata: { labels: { app: walgit } }
    spec:
      nodeSelector: { kubernetes.io/hostname: "$WALGIT_NODE" }
      terminationGracePeriodSeconds: 30
      containers:
        - name: walgit
          image: "$WALGIT_IMAGE"
          imagePullPolicy: Never
          args: ["serve"]
          ports: [ { containerPort: 8080, name: http } ]
          env:
            - { name: WALGIT_CONFIG, value: /etc/walgit/walgit.toml }
            - { name: WALGIT__CACHE__DIR, value: /var/lib/walgit }
            - { name: RUST_LOG, value: "info,walgit=info" }
          envFrom:
            - secretRef: { name: walgit-token }
            - secretRef: { name: forge-creds }
          volumeMounts:
            - { name: config, mountPath: /etc/walgit, readOnly: true }
            - { name: cache, mountPath: /var/lib/walgit }
          readinessProbe:
            httpGet: { path: /readyz, port: 8080 }
            periodSeconds: 2
          resources:
            requests: { cpu: 50m, memory: 64Mi }
      volumes:
        - name: config
          configMap: { name: walgit-config }
        - name: cache
          emptyDir: {}
---
apiVersion: v1
kind: Service
metadata: { name: walgit, namespace: agents }
spec:
  selector: { app: walgit }
  ports: [ { port: 8080, targetPort: 8080, name: http } ]
