# uid 1000 on NODE, which holds alice@FLINT.TEST's ticket for that uid.
apiVersion: v1
kind: Pod
metadata:
  name: alice-pod
spec:
  nodeName: NODE
  terminationGracePeriodSeconds: 1
  securityContext:
    runAsUser: 1000
    runAsGroup: 1000
  containers:
  - name: c
    image: busybox
    command: [sh, -c, "sleep 3600"]
    volumeMounts:
    - name: data
      mountPath: /data
  volumes:
  - name: data
    persistentVolumeClaim:
      claimName: krb5-pvc
