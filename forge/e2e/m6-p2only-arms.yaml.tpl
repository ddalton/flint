# M6's P2-ISOLATED arms — the CONTROL for the shared-repository result.
#
# SUPERSEDES this file's first header, which pre-registered the wrong
# mechanism. That header said the after-arm's ~385 MiB on tiny pushes was
# DEFERRED P9 WORK forced out by the 64-pack cap. Pair 3 refuted it: the
# before-arm sat on the same 1152 MiB repository and uploaded 1.4 MiB. It
# owed nothing. There is no debt, so the rebuild is not a debt being paid.
#
# What the three pairs actually show:
#
#   P2 uploaded      after (floor 256)      before (floor 0)
#   pair 1            385.4 MiB  6 folds       3.0 MiB  45 folds
#   pair 2            769.8 MiB  7 folds     770.2 MiB  29 folds
#   pair 3           1153.9 MiB  5 folds       1.4 MiB  32 folds
#   cumulative       24 folds / 4 base       187 folds / 2 base
#
# after's bytes are 1x, 2x, 3x of the 384 MiB each P9 leg deposits: it
# rebuilds the WHOLE repository on every P2 leg. The mechanism is the
# floor's own rule — tiny packs can never reach 256 MiB, so nothing folds
# until the pack cap trips, and then the only fold that satisfies the
# floor is one that pulls in everything. The floor converts many cheap
# folds into one full base rebuild per cap trip.
#
# THE CONFOUND THAT REMAINS. The floor causes the EVENT; P9's content
# sets its SIZE. These repositories never see P9, so there is no large
# content to re-upload and the discriminating signal is NOT bytes — it is
# the event count. Hence `baseRebuilds` is now recorded per leg; the
# shared run only recorded foldsCommitted, which is why the expensive
# event was invisible in it.
#
# PRE-REGISTERED, before this runs:
#   H1 (the floor is the trigger): m6p2-after records a base rebuild in
#      >= 2 of 3 legs, m6p2-before in <= 1 of 3; after's folds stay single
#      digit while before's run to tens.
#   H0 (P9's content was required): after's base rebuilds per leg are
#      <= before's. Then the floor is NOT the trigger and the shared
#      result has a cause I have not found — M6 reports no P2 verdict.
#   Bytes here are expected SMALL in absolute terms on both arms (a few
#   MiB; the repo only ever holds tiny pushes). If after still uploads
#   far more PER PUSH than before on a repository this small, the floor
#   costs bytes on tiny pushes outright, independent of any big content.
#
# NOTE the direction. M6's original P2 pre-registration expected
# before/after >= 1.30x — the floor SAVING bytes, as foldsim predicted
# (5.65x -> 3.08x). Every clean leg on the wire ran the other way. This
# control is not run to rescue that prediction; it is run to find out
# whether the floor's cost on tiny pushes is real, and the answer is
# allowed to be that the shipped rule is wrong for this shape.
#
# Two arms: ONE image, ONE cluster, ONE knob.
#
# `m6p2-after` runs the shipped tiers rule; `m6p2-before` sets the ladder
# floor to 0, which `foldsim.py` shows reproduces the whole pre-6bc67980
# rule exactly. Everything else is byte-identical between the arms: same
# bucket, same image, same node, same branch policy, same consumers.
# `syncerEnv` is the only line that differs, and the drill reads it back
# off each pod before it scores anything, because an arm assignment that
# is assumed rather than observed is how two arms silently become one.
---
apiVersion: chert.us/v1alpha1
kind: FlintRepo
metadata: { name: m6p2-after, namespace: agents }
spec:
  projectId: m6p2-after
  bucket: $BUCKET
  keyPrefix: $PREFIX/m6p2-after/
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
metadata: { name: m6p2-before, namespace: agents }
spec:
  projectId: m6p2-before
  bucket: $BUCKET
  keyPrefix: $PREFIX/m6p2-before/
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
