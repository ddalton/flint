# One tenant pod (aws-passthrough.sh renders __NAME__ __CR__ __NODE__ and
# an optional __EXTRA__ line), restricted-admissible, uid 1001, the same
# shape as tenants.yaml's reader.
apiVersion: v1
kind: Pod
metadata:
  name: __NAME__
  namespace: s3-tenants
  labels: { suite: pt }
spec:
  serviceAccountName: trainer
  nodeName: __NODE__
  securityContext:
    runAsNonRoot: true
    runAsUser: 1001
    seccompProfile: { type: RuntimeDefault }
  volumes:
    - name: data
      csi:
        driver: s3.csi.chert.us
        volumeAttributes:
          chert.us/mount: __CR__
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
