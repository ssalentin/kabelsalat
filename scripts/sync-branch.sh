#!/usr/bin/env bash
# Rebase the current feature branch onto the newest origin/main, and push it
# with --push (AGENTS.md, "Rebase before every push").
#
#   scripts/sync-branch.sh          fetch + rebase onto origin/main
#   scripts/sync-branch.sh --push   ... then git push --force-with-lease
#
# Conflicts that are only insertions on both sides of a Markdown file (rules
# appended to AGENTS.md by parallel PRs) are resolved by keeping both
# (scripts/ci/resolve_appends.py, the same script resolve-conflicts.yml uses).
# Any other conflict stops the rebase where it is and exits 2, naming the
# files: resolve them, `git add` them, `git rebase --continue`, run the gate
# tests again, then run this script again.
#
# With --push, when the rebase brought in changes from main to files this
# branch changes too (Markdown aside), it does not push but exits 3 and names them: run their
# test modules (rule 155), then run it again (the branch is up to date now,
# so the second run pushes).
#
# A merge commit the resolve-conflicts bot pushed to the branch is dropped by
# the rebase (its content is already in main), so --force-with-lease after a
# fetch replaces it on purpose.
set -euo pipefail

push=0
case "${1:-}" in
  --push) push=1 ;;
  "") ;;
  -h|--help) sed -n '2,23p' "$0"; exit 0 ;;
  *) echo "usage: $0 [--push]" >&2; exit 64 ;;
esac

top=$(git rev-parse --show-toplevel)
cd "$top"
branch=$(git branch --show-current)
if [ -z "$branch" ]; then
  echo "sync-branch: detached HEAD; check out your feature branch first" >&2
  exit 1
fi
if [ "$branch" = main ]; then
  echo "sync-branch: on main; work on a feature branch in its own worktree (AGENTS.md, Ground rules)" >&2
  exit 1
fi
if [ -d "$(git rev-parse --git-path rebase-merge)" ] || [ -d "$(git rev-parse --git-path rebase-apply)" ]; then
  echo "sync-branch: a rebase is already in progress; finish it (git rebase --continue) or abort it" >&2
  exit 1
fi
if ! git diff --quiet || ! git diff --cached --quiet; then
  echo "sync-branch: uncommitted changes; commit them first (never a bare git stash: it is shared across worktrees)" >&2
  git status --short >&2
  exit 1
fi

git fetch -q origin main
git fetch -q origin "$branch" 2>/dev/null || true # the branch may not be pushed yet

before=$(git rev-parse HEAD)
oldbase=$(git merge-base HEAD origin/main)
if git merge-base --is-ancestor origin/main HEAD; then
  echo "sync-branch: $branch already contains origin/main ($(git rev-parse --short origin/main))"
else
  echo "sync-branch: rebasing $branch onto origin/main ($(git rev-parse --short origin/main))"
  if ! GIT_EDITOR=true git rebase origin/main; then
    while :; do
      if ! python3 "$top/scripts/ci/resolve_appends.py"; then
        echo >&2
        echo "sync-branch: the rebase stopped on a conflict that is not an append to a Markdown file." >&2
        echo "  Resolve the files named above, git add them, then: git rebase --continue" >&2
        echo "  Run your gate tests again (rule 50), then run this script again." >&2
        exit 2
      fi
      if GIT_EDITOR=true git rebase --continue; then
        break
      fi
      # --continue stops again on the next commit's conflict: loop.
      if [ ! -d "$(git rev-parse --git-path rebase-merge)" ] && [ ! -d "$(git rev-parse --git-path rebase-apply)" ]; then
        echo "sync-branch: git rebase --continue failed outside a conflict" >&2
        exit 1
      fi
    done
  fi
  echo "sync-branch: rebased $(git rev-list --count origin/main..HEAD) commit(s) onto origin/main"
fi

if [ "$push" -eq 1 ]; then
  if [ "$(git rev-parse HEAD)" != "$before" ]; then
    both=$(comm -12 <(git diff --name-only "$oldbase" origin/main | sort) \
                    <(git diff --name-only origin/main HEAD | sort) | grep -v '\.md$' || true)
    if [ -n "$both" ]; then
      echo "sync-branch: main changed files this branch changes too; run their test modules again" >&2
      echo "  before you push (rule 155), then run this script again:" >&2
      echo "$both" | sed 's/^/    /' >&2
      exit 3
    fi
  fi
  git push --force-with-lease -u origin "HEAD:$branch"
fi
