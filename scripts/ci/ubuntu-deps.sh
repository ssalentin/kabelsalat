#!/usr/bin/env bash
# Build dependencies on Ubuntu (CI runners and the local docker check), plus
# any extra packages given as arguments (CI adds setbfree for the LV2 tests).
# fonts-noto-core: the UI's faces, which `every_ui_symbol_has_a_glyph` checks.
# With APT_DEB_CACHE set, downloaded .debs are kept in that directory: the
# x86_64 runners' Ubuntu mirror served the 4.4 MB of packages at ~54 kB/s
# (80+ s per run), so ci.yml caches them between runs.
# usage: [APT_DEB_CACHE=<dir>] scripts/ci/ubuntu-deps.sh [extra-package...]
set -euo pipefail
sudo=""
[ "$(id -u)" -eq 0 ] || sudo=sudo
opts=()
if [ -n "${APT_DEB_CACHE:-}" ]; then
  mkdir -p "$APT_DEB_CACHE/partial"
  opts=(-o "Dir::Cache::Archives=$APT_DEB_CACHE")
fi
$sudo apt-get update -q
$sudo apt-get install -y -q --no-install-recommends "${opts[@]}" \
  build-essential pkg-config ca-certificates curl git file \
  liblilv-dev libasound2-dev libdbus-1-dev \
  fonts-noto-core "$@"
# apt leaves the cache root-owned; actions/cache must read it to save it.
[ -z "${APT_DEB_CACHE:-}" ] || $sudo chown -R "$(id -u):$(id -g)" "$APT_DEB_CACHE"
