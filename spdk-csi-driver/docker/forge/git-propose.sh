#!/bin/sh
# `git propose` — move a protected branch the only way forge allows.
#
# Installed on PATH as `git-propose`, so git's own dispatch makes it a
# subcommand: `git propose` needs no alias, no `include.path`, and no
# per-repository config. It works in any clone, including one made
# before this image existed.
#
#   git propose                  # propose HEAD into the default branch
#   git propose release/2.1      # …into a named branch
#   git propose -o strategy=ours # pass push options through
#   git propose main -o strategy=theirs
#
# WHY THIS SCRIPT EXISTS. Forge has no merge API: a protected branch
# moves by pushing to `refs/for/<target>`, which the server merges with
# `merge-tree` (design §6). That is Gerrit's plumbing, adopted by Gitea
# and Gitee as the AGit flow, and it is not something anyone — a person
# or a model writing git commands — emits from memory. The server's
# refusal already names the remedy ("push to refs/for/main to propose a
# merge"), but it arrives as `! [remote rejected]` on a non-zero exit,
# and a harness that treats non-zero as fatal never shows it to the
# agent that needed to read it. So the remedy is a verb here instead.
#
# It is a CONVENIENCE, never an authority: this runs entirely on the
# client and pushes the same refspec by hand. Whether the merge is
# allowed is decided twice on the server, by `pre-receive` and by the
# syncer, from the policy the operator rendered.
set -eu

usage() {
    cat >&2 <<'EOF'
usage: git propose [<target-branch>] [<git-push option>...]

Proposes HEAD as a merge into <target-branch> on the forge server,
which performs the merge and reports which ref actually moved. With no
<target-branch>, the remote's default branch is used.

  -o strategy=ours|theirs   how the server should resolve a conflict
EOF
}

case "${1-}" in
    -h|--help) usage; exit 0 ;;
esac

git rev-parse --git-dir >/dev/null 2>&1 || {
    echo "git propose: not inside a git repository" >&2; exit 128
}

remote=${GIT_PROPOSE_REMOTE:-origin}

# A leading argument is the target only if it is not an option. This is
# what lets `git propose -o strategy=ours` mean the default branch
# rather than a branch named "-o".
target=""
case "${1-}" in
    ""|-*) ;;
    *) target=$1; shift ;;
esac

# The default branch, asked of the remote rather than assumed. `origin/HEAD`
# is set by `git clone` but NOT by `git remote add` + `git fetch`, so fall
# back to asking the remote directly before falling back to a guess.
if [ -z "$target" ]; then
    target=$(git symbolic-ref --quiet --short "refs/remotes/$remote/HEAD" 2>/dev/null | sed "s#^$remote/##") || true
fi
if [ -z "$target" ]; then
    target=$(git ls-remote --symref "$remote" HEAD 2>/dev/null \
             | sed -n 's#^ref: refs/heads/\([^[:space:]]*\)[[:space:]]*HEAD$#\1#p' | head -1) || true
fi
if [ -z "$target" ]; then
    echo "git propose: could not determine $remote's default branch; name it: git propose <branch>" >&2
    exit 2
fi

echo "git propose: proposing HEAD as a merge into $target on $remote" >&2
# `HEAD:` and not a branch name: an agent proposing from a detached
# checkout is ordinary, and the server is being told a commit, not a
# branch it should create.
exec git push "$@" "$remote" "HEAD:refs/for/$target"
