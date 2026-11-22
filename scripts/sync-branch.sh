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
# Non-Markdown overlaps introduced by a rebase are recorded in
# `$(git rev-parse --git-path sync-branch-overlap)` so that even when you
# rebase first with plain `scripts/sync-branch.sh` and push later with
# `--push`, `--push` still stops once with exit 3 naming those files. Run
# their test modules (rule 155), then run `--push` again to push cleanly.
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

overlap_file=$(git rev-parse --git-path sync-branch-overlap)

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

if [ "$(git rev-parse HEAD)" != "$before" ]; then
  both=$(LC_ALL=C comm -12 \
    <(git diff --name-only "$oldbase" origin/main | LC_ALL=C sort -u) \
    <(git diff --name-only origin/main HEAD | LC_ALL=C sort -u) \
    | grep -v '\.md$' || true)
  if [ -n "$both" ]; then
    tmp_overlap=$(mktemp "${overlap_file}.XXXXXX")
    if [ -f "$overlap_file" ]; then
      { cat "$overlap_file"; printf '%s\n' "$both"; } | LC_ALL=C sort -u > "$tmp_overlap"
    else
      printf '%s\n' "$both" | LC_ALL=C sort -u > "$tmp_overlap"
    fi
    mv "$tmp_overlap" "$overlap_file"
    if [ "$push" -eq 0 ]; then
      echo "sync-branch: note: main also changed non-Markdown file(s) touched by this branch:" >&2
      echo "$both" | sed 's/^/    /' >&2
      echo "  Run their test modules (rule 155); the next --push will require acknowledgement (exit 3)." >&2
    fi
  fi
fi

if [ "$push" -eq 1 ]; then
  if [ -s "$overlap_file" ]; then
    overlapped=$(cat "$overlap_file")
    rm -f "$overlap_file"
    echo "sync-branch: main changed files this branch changes too; run their test modules again" >&2
    echo "  before you push (rule 155), then run this script again:" >&2
    echo "$overlapped" | sed 's/^/    /' >&2
    exit 3
  fi
  rm -f "$overlap_file"
  git push --force-with-lease -u origin "HEAD:$branch"
fi
