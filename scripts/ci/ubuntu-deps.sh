#!/usr/bin/env bash
# Build dependencies on Ubuntu (CI runners and the local docker check), plus
# any extra packages given as arguments (CI adds setbfree for the LV2 tests).
# usage: scripts/ci/ubuntu-deps.sh [extra-package...]
set -euo pipefail
sudo=""
[ "$(id -u)" -eq 0 ] || sudo=sudo
# The man-db trigger rebuilds the manual index after every install; on the
# x86_64 runners that alone took most of the 45-125 s this step used to need.
$sudo rm -f /var/lib/man-db/auto-update
$sudo apt-get update -q
$sudo apt-get install -y -q --no-install-recommends \
  build-essential pkg-config ca-certificates curl git file \
  liblilv-dev libasound2-dev libdbus-1-dev "$@"
