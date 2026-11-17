#!/usr/bin/env bash
# Build dependencies on Ubuntu (CI runners and the local docker check).
set -euo pipefail
sudo=""
[ "$(id -u)" -eq 0 ] || sudo=sudo
$sudo apt-get update -q
$sudo apt-get install -y -q --no-install-recommends \
  build-essential pkg-config ca-certificates curl git file \
  liblilv-dev libasound2-dev libdbus-1-dev
