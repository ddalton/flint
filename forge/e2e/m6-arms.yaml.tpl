# M6's two arms: ONE image, ONE cluster, ONE knob.
#
# `m6-after` runs the shipped tiers rule; `m6-before` sets the ladder
# floor to 0, which `foldsim.py` shows reproduces the whole pre-6bc67980
# rule exactly (P9 3.56x/23 folds, P2 5.65x/465 folds on both). The
# commit's other two rules — the base-percent exemption and the cap's
# half-fold — contribute nothing at these shapes, so this is the
# dimension and not a stand-in for it.
#
# Everything else is byte-identical between the arms, deliberately: same
# bucket, same image, same node, same branch policy, same consumers.
# `syncerEnv` is the only line that differs, and the drill reads it back
# off each pod before it scores anything, because an arm assignment that
# is assumed rather than observed is how two arms silently become one.
---
apiVersion: chert.us/v1alpha1
kind: FlintRepo
metadata: { name: m6-after, namespace: agents }
spec:
  projectId: m6-after
  bucket: $BUCKET
  keyPrefix: $PREFIX/m6-after/
  credentialsSecretRef: forge-creds
  defaultBranch: main
  syncerEnv:
    FLINT_FORGE_FOLD_MIN_MIB: "256"
  consumers:
    serviceAccounts: [scale-agent]
  branches:
    protected: [main]
    mergeInto:
      main: ["system:serviceaccount:agents:scale-agent"]
    agentPattern: "agent/*"
---
apiVersion: chert.us/v1alpha1
kind: FlintRepo
metadata: { name: m6-before, namespace: agents }
spec:
  projectId: m6-before
  bucket: $BUCKET
  keyPrefix: $PREFIX/m6-before/
  credentialsSecretRef: forge-creds
  defaultBranch: main
  syncerEnv:
    FLINT_FORGE_FOLD_MIN_MIB: "0"
  consumers:
    serviceAccounts: [scale-agent]
  branches:
    protected: [main]
    mergeInto:
      main: ["system:serviceaccount:agents:scale-agent"]
    agentPattern: "agent/*"
