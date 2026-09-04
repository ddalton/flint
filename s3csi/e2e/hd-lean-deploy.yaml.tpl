# A Deployment-managed lean tenant (__NODE__ __FILES__ __NONCE__): prefers
# __NODE__ while it exists, follows the node-loss tolerations (30 s), and
# is otherwise the pod of hd-lean-pod.yaml.tpl. Its CR is proj-loss.
apiVersion: apps/v1
kind: Deployment
metadata: { name: lean-loss, namespace: s3-tenants }
spec:
  replicas: 1
  selector: { matchLabels: { app: lean-loss } }
  template:
    metadata: { labels: { app: lean-loss, suite: hd } }
    spec:
      serviceAccountName: trainer
      affinity:
        nodeAffinity:
          preferredDuringSchedulingIgnoredDuringExecution:
            - weight: 100
              preference: { matchExpressions: [{ key: kubernetes.io/hostname, operator: In, values: [__NODE__] }] }
      tolerations:
        - { key: node.kubernetes.io/not-ready, operator: Exists, effect: NoExecute, tolerationSeconds: 30 }
        - { key: node.kubernetes.io/unreachable, operator: Exists, effect: NoExecute, tolerationSeconds: 30 }
      securityContext:
        runAsNonRoot: true
        runAsUser: 1001
        runAsGroup: 1001
        seccompProfile: { type: RuntimeDefault }
      volumes:
        - name: ws
          csi:
            driver: s3.csi.chert.us
            volumeAttributes:
              chert.us/workspace: proj-loss
      containers:
        - name: agent
          image: busybox:1.36
          securityContext:
            allowPrivilegeEscalation: false
            capabilities: { drop: [ALL] }
          volumeMounts:
            - { name: ws, mountPath: /workspace }
          command:
            - /bin/sh
            - -c
            - |
              test -f /workspace/.flint-sync/checkout-complete || { echo "GATE BROKEN"; exit 1; }
              if [ ! -f /workspace/src/seeded ]; then
                echo "SEED START $(date +%s)"
                i=0; mkdir -p /workspace/src/d0 || exit 1
                while [ $i -lt __FILES__ ]; do printf 'unit %06d of the seeded project\n' $i > /workspace/src/d0/f$i.txt || exit 1; i=$((i + 1)); done
                echo seeded > /workspace/src/seeded
                mkdir -p /workspace/.flint
                printf '{"nonce":"__NONCE__"}' > /workspace/.flint/publish.tmp
                mv /workspace/.flint/publish.tmp /workspace/.flint/publish
                n=0; while [ $n -lt 600 ]; do grep -q __NONCE__ /workspace/.flint/publish.ack 2>/dev/null && break; n=$((n + 1)); sleep 1; done
                grep -q __NONCE__ /workspace/.flint/publish.ack 2>/dev/null && echo "SEED PUBLISHED $(date +%s)" || echo "SEED NEVER ACKED $(date +%s)"
              else
                echo "SEED PRESENT $(date +%s)"
              fi
              trap 'exit 0' TERM INT; sleep 86400 & wait
