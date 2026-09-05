#!/bin/sh
# An RPO on the agent's WORKING TREE, which git does not offer.
#
# git's contract is that uncommitted work is not durable, and forge
# keeps that contract: a push is durable when it returns, and nothing
# else is. For a harness that would rather not lose an hour of an
# agent's edits to a spot reclamation, this pushes the working tree to
# `refs/wip/<pod>` on a timer.
#
# Run it in the AGENT's pod — a sidecar container sharing the workspace
# volume, or a background loop in the agent's own entrypoint. Forge does
# not inject it: forge owns repository servers, not agent pods, and a
# CRD field asking for it would be a field the operator ignores.
#
#   WIP_REPO      the working tree (default: the current directory)
#   WIP_NAME      the ref suffix (default: $POD_NAME, then the hostname)
#   WIP_EVERY     seconds between snapshots (default 60); 0 = once, exit
#
# EVERYTHING HERE IS PLUMBING, and that is not stylistic. `git commit`
# against a throwaway index still moves HEAD, so a "snapshot" written
# with it would silently rewrite the agent's own branch under it —
# which is what an earlier draft of the design described. `write-tree`
# and `commit-tree` touch no ref at all; the only ref that moves is the
# one this script names on the remote.
set -eu

repo=${WIP_REPO:-.}
name=${WIP_NAME:-${POD_NAME:-$(hostname)}}
every=${WIP_EVERY:-60}
index=$(mktemp -t flint-forge-wip.XXXXXX)
trap 'rm -f "$index"' EXIT

snapshot() {
    cd "$repo"
    # Start from the real index so staging is a diff, not a full scan,
    # then add everything the working tree has — including files the
    # agent has not staged, which is the whole point.
    cp -f .git/index "$index" 2>/dev/null || rm -f "$index"
    GIT_INDEX_FILE="$index" git add -A
    tree=$(GIT_INDEX_FILE="$index" git write-tree)
    parent=$(git rev-parse --verify --quiet HEAD || true)
    if [ -n "$parent" ]; then
        commit=$(git commit-tree "$tree" -p "$parent" -m "wip: $name")
    else
        commit=$(git commit-tree "$tree" -m "wip: $name")
    fi
    # `--force`: a wip ref is a moving snapshot, not history.
    git push --quiet --force origin "$commit:refs/wip/$name"
}

if [ "$every" -eq 0 ]; then
    snapshot
    exit 0
fi

while true; do
    snapshot || echo "flint-forge: wip snapshot failed; retrying in ${every}s" >&2
    sleep "$every"
done
