# The STS stand-in (sts-shim.py) and the drill's AWS CLI pod, both pinned
# to the control plane (a leg never disturbs it). Rendered: __REGION__
# __ROLE_ARN__ __STS_KEY__ __STS_SECRET__ __CP__. The script arrives as a
# ConfigMap built from the file, so what runs is what is committed.
apiVersion: v1
kind: Secret
metadata: { name: sts-shim-keys, namespace: flint-system }
stringData:
  AWS_ACCESS_KEY_ID: __STS_KEY__
  AWS_SECRET_ACCESS_KEY: __STS_SECRET__
---
apiVersion: apps/v1
kind: Deployment
metadata: { name: sts-shim, namespace: flint-system }
spec:
  replicas: 1
  selector: { matchLabels: { app: sts-shim } }
  template:
    metadata: { labels: { app: sts-shim } }
    spec:
      nodeName: __CP__
      tolerations: [{ operator: Exists }]
      containers:
        - name: shim
          image: python:3.12-alpine
          command: ["python3", "-u", "/shim/sts-shim.py"]
          env:
            - { name: SHIM_UPSTREAM, value: "https://sts.__REGION__.amazonaws.com" }
            - { name: SHIM_REGION, value: "__REGION__" }
            - { name: SHIM_ROLE_ARN, value: "__ROLE_ARN__" }
          envFrom: [{ secretRef: { name: sts-shim-keys } }]
          ports: [{ containerPort: 8080 }]
          readinessProbe: { httpGet: { path: /healthz, port: 8080 } }
          volumeMounts: [{ name: shim, mountPath: /shim }]
      volumes:
        - name: shim
          configMap: { name: sts-shim-script }
---
apiVersion: v1
kind: Service
metadata: { name: sts-shim, namespace: flint-system }
spec:
  selector: { app: sts-shim }
  ports: [{ port: 8080, targetPort: 8080 }]
---
apiVersion: v1
kind: Pod
metadata: { name: awscli, namespace: flint-system }
spec:
  nodeName: __CP__
  tolerations: [{ operator: Exists }]
  containers:
    - name: aws
      image: amazon/aws-cli:2.36.39
      command: ["sh", "-c", "trap 'exit 0' TERM INT; sleep 86400 & wait"]
      env: [{ name: HOME, value: /tmp }]
