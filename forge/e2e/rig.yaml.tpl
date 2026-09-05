# The forge drill rig: one repository, one agent identity, one bucket.
#
# `$BUCKET` and `$PREFIX` are substituted by deploy.sh — the literals
# are never committed, so the same file drives any bucket.
#
# THE AUDIENCE IS THE WHOLE POINT of the agent's token. A token minted
# for the apiserver's own audience is refused by the door, which is what
# stops every pod token in the cluster from being a forge credential.
# `audience: forge.chert.us` here must equal `door.audience` in the
# chart, and getting it wrong produces a token that is perfectly valid,
# perfectly wrong, and rejected by every request.
---
apiVersion: v1
kind: ServiceAccount
metadata:
  name: agent-runner
  namespace: agents
---
apiVersion: chert.us/v1alpha1
kind: FlintRepo
metadata:
  name: proj
  namespace: agents
spec:
  projectId: proj
  bucket: $BUCKET
  keyPrefix: $PREFIX/git/
  credentialsSecretRef: forge-creds
  defaultBranch: main
  consumers:
    serviceAccounts: [agent-runner]
  branches:
    # main is PROPOSED to, never pushed to. An agent's direct push is
    # refused by pre-receive naming the rule; its merge request goes to
    # refs/for/main.
    protected: [main]
    mergeInto:
      main: ["system:serviceaccount:agents:agent-runner"]
    agentPattern: "agent/*"
  # F7's lever. Short enough that a drill need not wait, long enough
  # that a clone does not race the suspend.
  idle:
    suspendAfterSecs: 120
  # F9: the legible export, a lean workspace of its own.
  export:
    prefix: $PREFIX/export/
    refs: [refs/heads/main]
    everySecs: 30
  # F8's lever, and the ONLY one that is not enough on its own: a
  # client that has not opted in ignores the advertisement entirely.
  fleet:
    bundles:
      enabled: true
      everySecs: 120
      urlTtlSecs: 3600
  lfs:
    enabled: true
