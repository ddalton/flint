# The repository for F17: the idle ladder, armed short.
#
# `suspendAfterSecs` is the whole point, so it is in the template
# rather than patched in after the fact — a patch would roll the
# Deployment mid-drill and the pod UID assertions could not tell that
# apart from a park and a restore.
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
  defaultBranch: main
  consumers:
    serviceAccounts: ["agent-runner"]
  # No branch protection on purpose: the ladder is what is under test,
  # and a protected main would put a policy hop in front of the seed
  # push whose failure would look like a ladder failure.
  idle:
    suspendAfterSecs: __AFTER__
