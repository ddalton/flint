# A storm client.
#
# WHY tmpfs AND NOT THE CONTAINER FILESYSTEM. The first calibration run
# wrote its clones to the container's writable layer, which is the
# node's ROOT volume — 8 GiB on these instances. Fifty concurrent 40 MiB
# clones filled it, kubelet set DiskPressure and evicted pods, and one
# of the pods it evicted was the forge SERVER: its replacement reset the
# NIC counter, so the run reported NEGATIVE egress. A storm client must
# not be able to evict the thing it is measuring.
#
# The taint toleration and the nodeSelector are the other half of that:
# the server is kept off these nodes entirely.
apiVersion: v1
kind: Pod
metadata:
  name: $AGENT
  namespace: agents
  labels: { role: forge-stormer }
spec:
  serviceAccountName: agent-runner
  restartPolicy: Never
  nodeSelector:
    kubernetes.io/hostname: $NODE
  tolerations:
    - key: storm
      operator: Equal
      value: client
      effect: NoSchedule
  containers:
    - name: agent
      image: dilipdalton/flint-forge-git:$TAG
      command: ["sleep", "infinity"]
      resources:
        requests: { cpu: "2", memory: 8Gi }
        limits:   { memory: 40Gi }
      volumeMounts:
        - name: scratch
          mountPath: /storm
        - name: forge-token
          mountPath: /var/run/secrets/forge
          readOnly: true
  volumes:
    - name: scratch
      emptyDir:
        medium: Memory
        sizeLimit: 24Gi
    # WITHOUT THIS EVERY CLONE FAILS AUTH, and a storm of failed clones
    # moves no bytes — so the treatment arm reports 0 MiB of server
    # egress and looks like a triumph. The first calibration run did
    # exactly that. Hence the ok-count assertion in the harness.
    - name: forge-token
      projected:
        sources:
          - serviceAccountToken:
              path: token
              audience: forge.chert.us
              expirationSeconds: 3600
