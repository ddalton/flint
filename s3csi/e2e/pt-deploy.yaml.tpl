# The node-loss tenant: a Deployment (so it reschedules) pinned by
# affinity to __NODE__ until that node is gone.
apiVersion: apps/v1
kind: Deployment
metadata: { name: pt-deploy, namespace: s3-tenants }
spec:
  replicas: 1
  selector: { matchLabels: { app: pt-deploy } }
  template:
    metadata: { labels: { app: pt-deploy, suite: pt } }
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
        seccompProfile: { type: RuntimeDefault }
      volumes:
        - name: data
          csi:
            driver: s3.csi.chert.us
            volumeAttributes:
              chert.us/mount: datasets
      containers:
        - name: agent
          image: busybox:1.36
          command: ["/bin/sh", "-c"]
          args: ["trap 'exit 0' TERM INT; sleep 86400 & wait"]
          securityContext:
            allowPrivilegeEscalation: false
            capabilities: { drop: [ALL] }
          volumeMounts:
            - { name: data, mountPath: /mnt/s3 }
