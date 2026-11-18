#!/usr/bin/env bash
# Decides whether a PR may merge without a human (AGENTS.md "Merging").
# Usage: human-gate.sh <labels> < changed-files
#   labels: comma-separated PR labels; changed files: one path per line.
# Exits 0 when the PR may auto-merge, 1 when it waits for `human-approved`.
set -euo pipefail
labels=",${1:-},"

# Paths that set the rules agents work under, or ship to users. Everything
# else merges on green CI; hold a single risky PR with `needs-human`.
human_paths='^(AGENTS\.md|\.github/|\.githooks/|scripts/ci/|scripts/check-images\.sh'\
'|Cargo\.toml|packaging/)'

hits=$(grep -E "$human_paths" || true)
reason=""
[[ -n $hits ]] && reason="touches governance or release paths:"$'\n'"$hits"
[[ $labels == *,needs-human,* ]] && reason="labelled needs-human${reason:+; }$reason"

if [[ -z $reason ]]; then
  echo "auto-merge allowed: no governance or release paths changed"
  exit 0
fi
if [[ $labels == *,human-approved,* ]]; then
  echo "human-approved: $reason"
  exit 0
fi
echo "::error::waiting for a human: $reason"
echo "A maintainer reviews the PR and adds the 'human-approved' label."
exit 1
