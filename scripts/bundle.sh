#!/bin/sh
# Build "PDF Review.app" into ~/Applications.
#   APP_DIR=dir      put the .app somewhere else
#   BIN=path         use an already built binary (CI passes a universal one) instead of building
#   SIGN_IDENTITY=-  ad-hoc signature (CI has no certificate)
set -eu
cd "$(dirname "$0")/.."
if [ -z "${BIN:-}" ]; then
  cargo build --release
  BIN=target/release/pdf-review
fi
APP="${APP_DIR:-$HOME/Applications}/PDF Review.app"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "$BIN" "$APP/Contents/MacOS/pdf-review"
cp assets/AppIcon.icns "$APP/Contents/Resources/AppIcon.icns"
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>PDF Review</string>
  <key>CFBundleDisplayName</key><string>PDF Review</string>
  <key>CFBundleIdentifier</key><string>se.moriya.pdf-review</string>
  <key>CFBundleExecutable</key><string>pdf-review</string>
  <key>CFBundleIconFile</key><string>AppIcon</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>$VERSION</string>
  <key>CFBundleVersion</key><string>$VERSION</string>
  <key>LSMinimumSystemVersion</key><string>12.0</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>CFBundleDocumentTypes</key>
  <array>
    <dict>
      <key>CFBundleTypeName</key><string>PDF Document</string>
      <key>CFBundleTypeRole</key><string>Editor</string>
      <key>LSHandlerRank</key><string>Alternate</string>
      <key>LSItemContentTypes</key><array><string>com.adobe.pdf</string></array>
    </dict>
  </array>
</dict>
</plist>
PLIST
# Sign with a stable identity so macOS privacy grants (e.g. Downloads access) survive
# rebuilds. An ad-hoc signature is tied to the binary's hash, so every build would ask again.
IDENTITY="${SIGN_IDENTITY:-$(security find-identity -v -p codesigning 2>/dev/null | sed -n 's/.*"\(Apple Development:[^"]*\)".*/\1/p' | head -1)}"
if [ -n "$IDENTITY" ]; then
  codesign --force --sign "$IDENTITY" --identifier se.moriya.pdf-review "$APP"
  echo "Signed with: $IDENTITY"
else
  codesign --force --sign - "$APP" >/dev/null 2>&1 || true
  echo "No signing identity found; ad-hoc signed (macOS will re-ask for folder access after each rebuild)"
fi
/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister -f "$APP" >/dev/null 2>&1 || true
echo "Built $APP"
