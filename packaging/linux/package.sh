#!/usr/bin/env bash
# Packs the release binary (cargo build --release --locked -p app) into
#   <out-dir>/nit-<version>-linux-<arch>.{deb,rpm,AppImage,tar.gz}
# Needs cargo-deb, cargo-generate-rpm and appimagetool ($APPIMAGETOOL, default appimagetool;
# $APPIMAGE_RUNTIME names its runtime file, otherwise appimagetool downloads one).
set -euo pipefail

usage="usage: package.sh <version> <out-dir>"
version=${1:?$usage}
out=${2:?$usage}
root=$(cd "$(dirname "$0")/../.." && pwd)
target=${CARGO_TARGET_DIR:-$root/target}
bin=$target/release/nit
arch=$(uname -m) # x86_64 or aarch64
name=nit-$version-linux-$arch
mkdir -p "$out"
out=$(cd "$out" && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

if [ "$("$bin" --version)" != "nit $version" ]; then
  echo "$bin is not nit $version" >&2
  exit 1
fi
cd "$root"

# .deb: Debian versions sort 1.0.0~rc.1 before 1.0.0, cargo-deb converts the "-"
cargo deb -p app --no-build --output "$out/$name.deb"

# .rpm: RPM versions cannot contain "-"; "~" sorts before the release like in Debian
cargo generate-rpm -p crates/app --target-dir "$target" --output "$out/$name.rpm" \
  --set-metadata "version = \"${version//-/\~}\""

# .tar.gz: the binary with its desktop file, icon and licences
tree=$work/$name
mkdir -p "$tree"
install -m 755 "$bin" "$tree/nit"
install -m 644 packaging/linux/nit.desktop "$tree/nit.desktop"
install -m 644 packaging/icons/nit-256.png "$tree/nit.png"
install -m 644 README.md LICENSE-MIT LICENSE-APACHE THIRD-PARTY-NOTICES.md "$tree/"
tar -C "$work" --owner=0 --group=0 -czf "$out/$name.tar.gz" "$name"

# .AppImage: the binary and its desktop integration only. Everything else comes from the
# system, as for the .deb: glibc, libdbus, libxkbcommon, the X11 / Wayland client libraries,
# the Vulkan / OpenGL drivers, git and the Secret Service.
app=$work/nit.AppDir
install -D -m 755 "$bin" "$app/usr/bin/nit"
install -D -m 644 packaging/linux/nit.desktop "$app/usr/share/applications/nit.desktop"
install -D -m 644 packaging/linux/io.github.scorpse.nit.metainfo.xml \
  "$app/usr/share/metainfo/io.github.scorpse.nit.metainfo.xml"
for size in 16 22 24 32 48 64 128 256 512; do
  install -D -m 644 "packaging/icons/nit-$size.png" \
    "$app/usr/share/icons/hicolor/${size}x$size/apps/nit.png"
done
install -m 644 packaging/linux/nit.desktop "$app/nit.desktop"
install -m 644 packaging/icons/nit-256.png "$app/nit.png"
ln -s nit.png "$app/.DirIcon"
ln -s usr/bin/nit "$app/AppRun"
runtime=()
if [ -n "${APPIMAGE_RUNTIME:-}" ]; then
  runtime=(--runtime-file "$APPIMAGE_RUNTIME")
fi
ARCH=$arch "${APPIMAGETOOL:-appimagetool}" "${runtime[@]}" "$app" "$out/$name.AppImage"

ls -l "$out"
