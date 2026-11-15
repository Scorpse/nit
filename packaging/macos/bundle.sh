#!/usr/bin/env bash
# Builds the macOS app bundle and its downloads from the release binary
# (cargo build --release --locked -p app; MACOSX_DEPLOYMENT_TARGET=11.0 as in Info.plist):
#   bundle.sh app <version> <out-dir>          -> <out-dir>/nit.app (ad-hoc signed)
#   bundle.sh dmg <version> <arch> <out-dir>   -> <out-dir>/nit-<version>-macos-<arch>.{dmg,tar.gz}
# A Developer ID signature, when there is one, goes on nit.app between the two steps.
set -euo pipefail

usage="usage: bundle.sh app <version> <out-dir> | bundle.sh dmg <version> <arch> <out-dir>"
root=$(cd "$(dirname "$0")/../.." && pwd)

case "${1:-}" in
app)
  version=${2:?$usage}
  out=${3:?$usage}
  bin=${CARGO_TARGET_DIR:-$root/target}/release/nit
  if [ "$("$bin" --version)" != "nit $version" ]; then
    echo "$bin is not nit $version" >&2
    exit 1
  fi
  app=$out/nit.app
  rm -rf "$app"
  mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
  # bundle versions are numbers only: 1.2.0-rc.1 -> 1.2.0
  sed "s/@VERSION@/${version%%-*}/g" "$root/packaging/macos/Info.plist" >"$app/Contents/Info.plist"
  plutil -lint "$app/Contents/Info.plist"
  printf 'APPL????' >"$app/Contents/PkgInfo"
  install -m 755 "$bin" "$app/Contents/MacOS/nit"
  cp "$root/packaging/icons/nit.icns" "$app/Contents/Resources/nit.icns"
  cp "$root/LICENSE-MIT" "$root/LICENSE-APACHE" "$root/THIRD-PARTY-NOTICES.md" "$app/Contents/Resources/"
  # ad-hoc signature: Apple silicon runs nothing unsigned, and a sealed bundle is reported as
  # "unidentified developer" rather than "damaged"
  codesign --force --deep --sign - "$app"
  ;;
dmg)
  version=${2:?$usage}
  arch=${3:?$usage}
  out=${4:?$usage}
  name=nit-$version-macos-$arch
  stage=$(mktemp -d)
  trap 'rm -rf "$stage"' EXIT
  ditto "$out/nit.app" "$stage/nit.app"
  ln -s /Applications "$stage/Applications"
  hdiutil create -volname nit -srcfolder "$stage" -fs HFS+ -format UDZO -ov "$out/$name.dmg"
  # no AppleDouble (._*) files in the archive
  COPYFILE_DISABLE=1 tar -C "$out" -czf "$out/$name.tar.gz" nit.app
  ;;
*)
  echo "$usage" >&2
  exit 2
  ;;
esac
