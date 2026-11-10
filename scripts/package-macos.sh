#!/bin/bash
set -euo pipefail
cd "$(dirname "$0")/.."

# universal (default, used by CI) | aarch64 (Apple Silicon only, faster local builds)
ARCH=${1:-universal}

BINARY=target/packaging/flowsurface
case "$ARCH" in
  universal) REQUIRED_ARCHS=(arm64 x86_64); DMG_SUFFIX=universal ;;
  aarch64) REQUIRED_ARCHS=(arm64); DMG_SUFFIX=aarch64 ;;
  *) echo "Unsupported macOS architecture: $ARCH (expected universal or aarch64)" >&2; exit 1 ;;
esac

if ! lipo "$BINARY" -verify_arch "${REQUIRED_ARCHS[@]}"; then
    echo "macOS $ARCH packaging requires a ${REQUIRED_ARCHS[*]} executable: $BINARY" >&2
    echo "Build it first with: bash scripts/build-macos.sh $ARCH" >&2
    exit 1
fi
VERSION=$(cargo metadata --no-deps --format-version 1 | python3 -c 'import json,sys; m=json.load(sys.stdin); print(next(p["version"] for p in m["packages"] if p["name"] == "flowsurface"))')
APP_DIR="target/packaging/macos"
APP="$APP_DIR/Flowsurface.app"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "$BINARY" "$APP/Contents/MacOS/flowsurface"
chmod +x "$APP/Contents/MacOS/flowsurface"
cp assets/app-icon/flowsurface.icns "$APP/Contents/Resources/flowsurface.icns"
sed "s/@VERSION@/$VERSION/g" scripts/packaging/macos/Info.plist > "$APP/Contents/Info.plist"
plutil -lint "$APP/Contents/Info.plist"

codesign --force --sign - "$APP"
codesign --verify --strict "$APP"
touch "$APP"

ln -sfn /Applications "$APP_DIR/Applications"
DMG="target/packaging/flowsurface-${DMG_SUFFIX}-macos.dmg"
hdiutil create -volname Flowsurface -fs HFS+ -srcfolder "$APP_DIR" -ov -format UDZO "$DMG"
echo "Created $DMG"
