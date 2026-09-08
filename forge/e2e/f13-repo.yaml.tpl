# The repository for F13: one repo, both doors, reached only through the
# gateway.
#
# NO `fileApi.tokenSecret`, and its absence is load-bearing rather than
# an omission. That field is a shared bearer the SYNCER requires and the
# door structurally cannot present — reading it would mean `get secrets`
# in every tenant namespace, which the gateway has never had. Behind the
# door the boundary is the NetworkPolicy admitting its pods to the file
# port, exactly as it already is for `X-Remote-User`. Setting both would
# 401 every request through the door; the door detects that case and
# says so by name.
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
    # `agent-runner` only — NOT `*`. P0's non-consumer leg needs a real
    # ServiceAccount that is genuinely absent from this list, and `*`
    # would admit it and make that leg vacuous.
    serviceAccounts: ["agent-runner"]
  branches:
    protected: ["main"]
  fileApi:
    enabled: true
    branch: agents
    maxMb: 2
