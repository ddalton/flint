# The store half of the s3.csi.chert.us rig against a REAL bucket
# (run-s3csi.sh STORE=s3 renders it): no MinIO, the same namespaces, the
# same seed, the same durable mc pod, and the broker's static Secret —
# holding a key scoped to ONE bucket, minted for the drill and deleted
# with it. Placeholders: __ENDPOINT__ __BUCKET__ __REGION__ __KEY__
# __SECRET__. The seed WIPES the bucket first (every version): the
# legs count objects under prefixes, and a versioned bucket keeps what
# the previous run left.
apiVersion: v1
kind: Namespace
metadata:
  name: flint-system
---
apiVersion: v1
kind: Namespace
metadata:
  name: s3-tenants
  labels:
    pod-security.kubernetes.io/enforce: restricted
    pod-security.kubernetes.io/enforce-version: latest
---
apiVersion: batch/v1
kind: Job
metadata:
  name: seed-bucket
  namespace: flint-system
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
              until mc alias set m __ENDPOINT__ __KEY__ __SECRET__; do sleep 2; done
              mc ls m/__BUCKET__ >/dev/null || { echo "bucket __BUCKET__ is not reachable with this key"; exit 1; }
              mc rm --recursive --force --versions m/__BUCKET__/ >/dev/null 2>&1 || true
              for i in 00 01 02 03 04 05 06 07 08 09; do
                echo "seeded-object-$i" | mc pipe "m/__BUCKET__/datasets/imagenet/shard-$i.txt"
              done
              echo "deep-seeded" | mc pipe m/__BUCKET__/datasets/imagenet/sub/deep.txt
              echo "elsewhere-only" | mc pipe m/__BUCKET__/elsewhere/only.txt
              echo "must-not-be-visible" | mc pipe m/__BUCKET__/private/secret.txt
---
apiVersion: v1
kind: Pod
metadata:
  name: mc-s3
  namespace: flint-system
spec:
  # PINNED TO THE CONTROL PLANE. This pod is the drill's only window on
  # the bucket, and a window that dies with the thing it watches reports
  # "nothing" — which reads as "the object is not there". It sat on the
  # worker a node-loss leg terminated, and six assertions failed on an
  # empty answer while the product had done exactly the right thing
  # (2026-09-04). The control plane is the one node these legs never
  # reboot, fill, or destroy.
  nodeSelector: { node-role.kubernetes.io/control-plane: "" }
  tolerations:
    - { key: node-role.kubernetes.io/control-plane, operator: Exists, effect: NoSchedule }
    - { key: node-role.kubernetes.io/master, operator: Exists, effect: NoSchedule }
  containers:
    - name: mc
      image: minio/mc
      command: ["/bin/sh", "-c"]
      args:
        - |
          trap 'exit 0' TERM INT
          until mc alias set m __ENDPOINT__ __KEY__ __SECRET__; do sleep 2; done
          sleep 86400 & wait
---
apiVersion: v1
kind: Secret
metadata:
  name: s3-broker-static
  namespace: flint-system
type: Opaque
stringData:
  AWS_ACCESS_KEY_ID: __KEY__
  AWS_SECRET_ACCESS_KEY: __SECRET__
---
# The interim STATIC arm's Secret (leg S5c), same name as on the MinIO
# rig because the fixture names it.
apiVersion: v1
kind: Secret
metadata:
  name: minio-creds
  namespace: s3-tenants
type: Opaque
stringData:
  AWS_ACCESS_KEY_ID: __KEY__
  AWS_SECRET_ACCESS_KEY: __SECRET__
  AWS_REGION: __REGION__
