# A lean tenant pod for aws-hardening.sh (__NAME__ __CR__ __NODE__
# __FILES__ __NONCE__). restartPolicy Always: after a node reboot kubelet
# restarts the container, and the command seeds only a tree that is not
# yet seeded, so a restart re-declares nothing. It logs timestamps the
# legs read: SEED START / SEED WRITTEN / SEED PUBLISHED / SEED PRESENT.
apiVersion: v1
kind: Pod
metadata:
  name: __NAME__
  namespace: s3-tenants
  labels: { suite: hd }
spec:
  serviceAccountName: trainer
  nodeName: __NODE__
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
          chert.us/workspace: __CR__
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
            i=0; d=""
            while [ $i -lt __FILES__ ]; do
              nd=/workspace/src/d$((i / 1000)); [ "$nd" = "$d" ] || { d=$nd; mkdir -p $d || exit 1; }
              printf 'unit %06d of the seeded project\n' $i > $d/f$i.txt || exit 1
              i=$((i + 1))
            done
            echo seeded > /workspace/src/seeded
            echo "SEED WRITTEN $(date +%s)"
            mkdir -p /workspace/.flint
            printf '{"nonce":"__NONCE__"}' > /workspace/.flint/publish.tmp
            mv /workspace/.flint/publish.tmp /workspace/.flint/publish
            n=0; while [ $n -lt 2400 ]; do grep -q __NONCE__ /workspace/.flint/publish.ack 2>/dev/null && break; n=$((n + 1)); sleep 1; done
            grep -q __NONCE__ /workspace/.flint/publish.ack 2>/dev/null && echo "SEED PUBLISHED $(date +%s)" || echo "SEED NEVER ACKED $(date +%s)"
          else
            echo "SEED PRESENT $(date +%s)"
          fi
          trap 'exit 0' TERM INT; sleep 86400 & wait
