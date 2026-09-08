# The repository for F16 — the lease.
#
# NO protected branch and NO fileApi: F16 is about which SERVER is
# allowed to write, not about which client is. A policy refusal or a
# second door would answer a push before the lease was ever consulted,
# and a failure could then be either.
apiVersion: chert.us/v1alpha1
kind: FlintRepo
metadata:
  name: __REPO__
  namespace: __NS__
spec:
  projectId: __REPO__
  bucket: __BUCKET__
  keyPrefix: __PREFIX__/__REPO__/
  # HEAD points where the work is — the same trap F14 hit: without it
  # every clone checks out nothing and the first push fails with
  # `src refspec ... does not match any`, which reads as a door problem.
  defaultBranch: agents
  credentialsSecretRef: forge-creds
  consumers:
    serviceAccounts: ["agent-runner"]
