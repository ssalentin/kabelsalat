#!/usr/bin/env bash
# Print the CHANGELOG.md block of one version (the lines under "## [X.Y.Z]"
# up to the next "## [" heading); fail when the block is missing or empty.
# With <dist> <tag> <owner/repo>, append a Download section that links every
# package in <dist> by its release URL, AppImages first.
# usage: scripts/ci/release-notes.sh X.Y.Z [<dist> <tag> <owner/repo>]
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
[ $# -ge 4 ] || exit 0

dist=$2 tag=$3 repo=$4
base="https://github.com/$repo/releases/download/$tag"
link() { printf -- '- [%s](%s/%s)\n' "$1" "$base" "$1"; }
shopt -s nullglob
appimages=("$dist"/kabelsalat-*-x86_64.AppImage "$dist"/kabelsalat-*-aarch64.AppImage)
if [ ${#appimages[@]} -eq 0 ]; then
  echo "no AppImage in $dist" >&2
  exit 1
fi

printf '\n## Download\n\n'
printf '**Recommended: the AppImage.** Download the one for your machine, `chmod +x` it and run it. '
printf 'It finds your installed CLAP/LV2 plugins and gets realtime audio priority.\n\n'
for f in "${appimages[@]}"; do
  name=$(basename "$f")
  arch=${name%.AppImage}; arch=${arch##*-}
  printf -- '- **%s:** [%s](%s/%s)\n' "$arch" "$name" "$base" "$name"
done
printf '\nOther packages (limits in [docs/PACKAGING.md](https://github.com/%s/blob/main/docs/PACKAGING.md)):\n\n' "$repo"
for f in "$dist"/*; do
  name=$(basename "$f")
  case $name in *.AppImage | SHA256SUMS) continue ;; esac
  link "$name"
done
[ -e "$dist/SHA256SUMS" ] && printf '\nChecksums: [SHA256SUMS](%s/SHA256SUMS)\n' "$base"
exit 0
