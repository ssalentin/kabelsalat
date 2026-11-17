#!/usr/bin/env bash
# Print the CHANGELOG.md block of one version (the lines under "## [X.Y.Z]"
# up to the next "## [" heading); fail when the block is missing or empty.
# usage: scripts/ci/release-notes.sh X.Y.Z
set -euo pipefail
version=$1
notes=$(awk -v v="$version" '
  /^## \[/ { if (on) exit; if (index($0, "## [" v "]") == 1) { on = 1; next } }
  on' "$(dirname "$0")/../../CHANGELOG.md" | sed -E -e '/^\[[^]]+\]: https?:/d' -e '/./,$!d')
if [ -z "${notes//[$'\n\t ']/}" ]; then
  echo "CHANGELOG.md has no notes under ## [$version]" >&2
  exit 1
fi
printf '%s\n' "$notes"
