#!/usr/bin/env bash
# AppImage built with linuxdeploy: kabelsalat-<version>-<arch>.AppImage in <outdir>.
# Bundles lilv and its libraries; libasound, glibc and the display stack come
# from the host (linuxdeploy's excludelist), so pipewire-alsa keeps working.
# usage: packaging/appimage.sh <binary> <version> <outdir>
set -euo pipefail
bin=$1 version=$2 out=$3
root=$(cd "$(dirname "$0")/.." && pwd)
arch=$(uname -m)
tag=1-alpha-20251107-1
case $arch in
  x86_64) sum=c20cd71e3a4e3b80c3483cef793cda3f4e990aca14014d23c544ca3ce1270b4d ;;
  aarch64) sum=620095110d693282b8ebeb244a95b5e911cf8f65f76c88b4b47d16ae6346fcff ;;
  *) echo "no linuxdeploy build for $arch" >&2; exit 1 ;;
esac
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
tool="$work/linuxdeploy-$arch.AppImage"
curl -sSfL -o "$tool" "https://github.com/linuxdeploy/linuxdeploy/releases/download/$tag/linuxdeploy-$arch.AppImage"
echo "$sum  $tool" | sha256sum -c -
chmod +x "$tool"
cp "$root/assets/logo.svg" "$work/kabelsalat.svg"
mkdir -p "$out"
(
  cd "$work"
  APPIMAGE_EXTRACT_AND_RUN=1 LINUXDEPLOY_OUTPUT_VERSION="$version" \
    LDAI_OUTPUT="kabelsalat-$version-$arch.AppImage" \
    "$tool" --appdir AppDir --executable "$bin" \
      --desktop-file "$root/packaging/kabelsalat.desktop" \
      --icon-file kabelsalat.svg --output appimage
)
mv "$work/kabelsalat-$version-$arch.AppImage" "$out/"
echo "$out/kabelsalat-$version-$arch.AppImage"
