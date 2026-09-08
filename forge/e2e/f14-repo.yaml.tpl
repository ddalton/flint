# The repository for F14 — collaborative editing by N git clients.
#
# NO protected branch on `agents`, deliberately: F14 is about what the
# non-fast-forward rule does under contention, and a protected-branch
# refusal would answer every push before that rule was ever reached.
# `main` stays protected so the repository is not a special shape.
#
# NO fileApi: this drill is git clients only. The one-door claim is
# F13's; mixing them here would mean a failure could be either.
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
    serviceAccounts: ["agent-runner"]
  branches:
    protected: ["main"]
