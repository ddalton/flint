# The repository for F15 — a PERSON is the principal.
#
# `agent-runner` is deliberately ABSENT from consumers. Every other
# forge drill authenticates with the pod's own ServiceAccount token, so
# if that worked here too, every push F15 makes could be succeeding as
# the ServiceAccount and the JWT would be decoration. P2 asserts the pod
# token is refused, and this absence is what makes that true.
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
    serviceAccounts:
__EXTRA__    - "jwt:user:__ALICE__"
  branches:
    protected: ["main"]
    # PER-PERSON branch rights — the limitation the architecture
    # document records as unfixable while every principal is a
    # ServiceAccount many pods share. P4 pushes both patterns.
    pushers:
      "agent/alice/*": ["__ALICE__"]
      "agent/bob/*": ["bob@example.com"]
