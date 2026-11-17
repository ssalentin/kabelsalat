#!/usr/bin/env bash
# Decides whether a PR may merge without a human (AGENTS.md "Merging").
# Usage: human-gate.sh <labels> < changed-files
#   labels: comma-separated PR labels; changed files: one path per line.
# Exits 0 when the PR may auto-merge, 1 when it waits for `human-approved`.
set -euo pipefail
labels=",${1:-},"

# Paths CI cannot judge: what users see, what they hear (real-time audio,
# real plugins, MIDI hardware), and what ships or governs the repo.
human_paths='^(src/ui(\.rs|/)|src/theme\.rs|assets/|docs/images\.sha256'\
'|src/engine(\.rs|/)|src/audio[^/]*\.rs|src/dsp\.rs|src/host\.rs|src/lv2\.rs'\
'|src/juce\.rs|src/organ\.rs|src/midi\.rs|src/record\.rs|src/player\.rs'\
'|src/capture\.rs|src/meter\.rs|src/transport\.rs'\
'|Cargo\.toml|AGENTS\.md|\.github/|\.githooks/|packaging/|scripts/ci/|scripts/check-images\.sh)'

hits=$(grep -E "$human_paths" || true)
reason=""
[[ -n $hits ]] && reason="touches paths CI cannot judge:"$'\n'"$hits"
[[ $labels == *,needs-human,* ]] && reason="labelled needs-human${reason:+; }$reason"

if [[ -z $reason ]]; then
  echo "auto-merge allowed: no human-review paths changed"
  exit 0
fi
if [[ $labels == *,human-approved,* ]]; then
  echo "human-approved: $reason"
  exit 0
fi
echo "::error::waiting for a human: $reason"
echo "A maintainer reviews the PR and adds the 'human-approved' label."
exit 1
