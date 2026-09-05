# An agent, as the design describes one: ONE projected token and
# nothing else. No S3 credential, no sidecar, no bucket name, no key.
# The whole of its authority to reach the repository is this token, and
# the door is the only thing that can turn it into an identity.
apiVersion: v1
kind: Pod
metadata:
  name: $AGENT
  namespace: agents
  labels: { role: forge-agent }
spec:
  serviceAccountName: agent-runner
  restartPolicy: Never
  containers:
    - name: agent
      image: dilipdalton/flint-forge-git:$TAG
      command: ["sleep", "infinity"]
      volumeMounts:
        - name: forge-token
          mountPath: /var/run/secrets/forge
          readOnly: true
  volumes:
    - name: forge-token
      projected:
        sources:
          - serviceAccountToken:
              path: token
              # MUST equal the door's --git-audience. A token minted for
              # the apiserver's own audience is refused, and that is
              # exactly what stops any pod token here being a
              # forge credential.
              audience: forge.chert.us
              expirationSeconds: 3600
