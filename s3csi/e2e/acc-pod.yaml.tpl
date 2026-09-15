# One access-drill tenant (aws-access.sh). Rendered: __NAME__ __SA__
# __NODE__ __SELKEY__ (workspace | mount) __CR__ __RO__ (true | false), and
# the __OBO__ line, which becomes `chert.us/on-behalf-of: <user>` or is
# deleted. The agent does nothing on its own: every write, publish and read
# is a leg's exec.
apiVersion: v1
kind: Pod
metadata:
  name: __NAME__
  namespace: s3-tenants
  labels: { suite: acc }
spec:
  serviceAccountName: __SA__
  nodeName: __NODE__
  automountServiceAccountToken: false
  securityContext:
    runAsNonRoot: true
    runAsUser: 1001
    runAsGroup: 1001
    seccompProfile: { type: RuntimeDefault }
  volumes:
    - name: ws
      csi:
        driver: s3.csi.chert.us
        readOnly: __RO__
        volumeAttributes:
          chert.us/__SELKEY__: __CR__
          __OBO__
  containers:
    - name: agent
      image: busybox:1.36
      command: ["/bin/sh", "-c"]
      args: ["trap 'exit 0' TERM INT; sleep 86400 & wait"]
      securityContext:
        allowPrivilegeEscalation: false
        capabilities: { drop: [ALL] }
      volumeMounts:
        - { name: ws, mountPath: /workspace }
