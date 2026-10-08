#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
if [[ "$(uname -s)" != Darwin ]]; then
  echo 'Requires macOS with Xcode Command Line Tools (xcode-select --install).' >&2
  exit 1
fi
TARGET=aarch64-apple-darwin
rustup target add "$TARGET"
cargo build --release --locked --target "$TARGET"
OUT="dist/macos-arm64"
APP="$OUT/ftpseg.app"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "target/$TARGET/release/ftpseg-gui" "$APP/Contents/MacOS/ftpseg-gui"
cp "target/$TARGET/release/ftpseg" "$OUT/ftpseg"
chmod +x "$APP/Contents/MacOS/ftpseg-gui" "$OUT/ftpseg"
cat > "$APP/Contents/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>CFBundleExecutable</key><string>ftpseg-gui</string>
<key>CFBundleIdentifier</key><string>local.ftpseg.gui</string>
<key>CFBundleName</key><string>ftpseg</string>
<key>CFBundlePackageType</key><string>APPL</string>
<key>CFBundleShortVersionString</key><string>0.1.0</string>
<key>CFBundleVersion</key><string>1</string>
<key>CFBundleIconFile</key><string>ftpseg.icns</string>
<key>LSMinimumSystemVersion</key><string>11.0</string>
<key>NSHighResolutionCapable</key><true/>
</dict></plist>
PLIST
ICONSET="$OUT/ftpseg.iconset"
mkdir -p "$ICONSET"
for SIZE in 16 32 128 256 512; do
  sips -z "$SIZE" "$SIZE" assets/ftpseg.png --out "$ICONSET/icon_${SIZE}x${SIZE}.png" >/dev/null
  DOUBLE=$((SIZE * 2))
  sips -z "$DOUBLE" "$DOUBLE" assets/ftpseg.png --out "$ICONSET/icon_${SIZE}x${SIZE}@2x.png" >/dev/null
done
iconutil -c icns "$ICONSET" -o "$APP/Contents/Resources/ftpseg.icns"
# Ad-hoc signing is not Developer ID signing or Apple notarization.
codesign --force --sign - "$APP"
codesign --verify --deep --strict "$APP"
lipo "$APP/Contents/MacOS/ftpseg-gui" -verify_arch arm64
lipo "$OUT/ftpseg" -verify_arch arm64
ditto -c -k --sequesterRsrc --keepParent "$APP" "$OUT/ftpseg-macos-arm64.zip"
echo "Built $OUT/ftpseg-macos-arm64.zip and $OUT/ftpseg"
