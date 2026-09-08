# The repository for F12: both doors open on one repo.
#
# `fileApi.branch` is NOT main, and that is the point rather than an
# omission. `policy.judge` applies to a file-API write exactly as it
# does to a push, so a UI aimed at a protected branch is a 403 machine
# — P5 proves the protection is real by writing at main and being
# refused.
apiVersion: chert.us/v1alpha1
kind: FlintRepo
metadata:
  name: __REPO__
  namespace: __NS__
spec:
  projectId: __REPO__
  bucket: __BUCKET__
  keyPrefix: __PREFIX__/__REPO__/
  credentialsSecretRef: forge-creds
  consumers:
    serviceAccounts: ["*"]
  branches:
    # Bare names, not `refs/heads/...` — the schema's own spelling.
    protected: ["main"]
  fileApi:
    enabled: true
    branch: agents
    maxMb: 2
    tokenSecret: __REPO__-file-token
