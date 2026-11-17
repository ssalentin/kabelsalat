#!/usr/bin/env bash
# Every image in the git index must be listed in docs/images.sha256 with its
# exact content hash (AGENTS.md rule 108). A screenshot with other windows in it
# reached the public repo twice; listing a hash is the explicit "I looked at
# this image and it shows only kabelsalat" step.
# Runs in the pre-commit hook (.githooks) and in CI, both on the index.
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"
MANIFEST=docs/images.sha256
fail=0
while read -r _mode blob _stage path; do
  sum=$(git cat-file blob "$blob" | sha256sum | cut -d' ' -f1)
  if ! git show ":$MANIFEST" 2>/dev/null | grep -qxF "$sum  $path"; then
    echo "unreviewed image: $path ($sum)" >&2
    fail=1
  fi
done < <(git ls-files -s | grep -iE '\.(png|jpe?g|webp|gif|bmp|avif|tiff?|ppm|qoi)$')
if [ "$fail" -ne 0 ]; then
  cat >&2 <<EOF

Open each image above and confirm it shows ONLY kabelsalat: no other windows,
browser tabs, notifications, bars, file names or account names. Take app
screenshots with scripts/shot.sh (window buffer only), never grim -o.
Then add its line to $MANIFEST:  sha256sum <path> >> $MANIFEST
EOF
  exit 1
fi
