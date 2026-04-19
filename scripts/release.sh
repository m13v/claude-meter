#!/usr/bin/env bash
# Signed + notarized release pipeline for ClaudeMeter.app.
# Usage: scripts/release.sh [version]
#   Defaults to the version in Cargo.toml.

set -euo pipefail

VERSION="${1:-}"
APP_NAME="ClaudeMeter"
BUNDLE_ID="com.m13v.claude-meter"
SIGN_IDENTITY="Developer ID Application: Matthew Diakonov (S6DP5HF77G)"
NOTARY_PROFILE="claude-meter-notary"

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"

if [ -z "$VERSION" ]; then
    VERSION="$(grep '^version' Cargo.toml | head -1 | sed -E 's/.*"(.*)".*/\1/')"
fi
echo "version: $VERSION"

OUT_DIR="dist"
APP_BUNDLE="$OUT_DIR/$APP_NAME.app"
DMG_PATH="$OUT_DIR/$APP_NAME-$VERSION.dmg"
ZIP_PATH="$OUT_DIR/$APP_NAME-$VERSION.zip"
ENTITLEMENTS="scripts/entitlements.plist"

echo "[1/7] building release binaries..."
cargo build --release --bin claude-meter --bin claude-meter-menubar

echo "[2/7] assembling $APP_BUNDLE..."
rm -rf "$APP_BUNDLE"
mkdir -p "$APP_BUNDLE/Contents/MacOS"
mkdir -p "$APP_BUNDLE/Contents/Resources"
cp "target/release/claude-meter-menubar" "$APP_BUNDLE/Contents/MacOS/$APP_NAME"
cp "target/release/claude-meter" "$APP_BUNDLE/Contents/MacOS/claude-meter"

cat > "$APP_BUNDLE/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key><string>$APP_NAME</string>
    <key>CFBundleDisplayName</key><string>$APP_NAME</string>
    <key>CFBundleIdentifier</key><string>$BUNDLE_ID</string>
    <key>CFBundleVersion</key><string>$VERSION</string>
    <key>CFBundleShortVersionString</key><string>$VERSION</string>
    <key>CFBundleExecutable</key><string>$APP_NAME</string>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>LSMinimumSystemVersion</key><string>11.0</string>
    <key>LSUIElement</key><true/>
    <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
PLIST

echo "[3/7] codesigning (hardened runtime, Developer ID)..."
# Sign the inner CLI binary first, then the bundle (which picks up the main executable).
codesign --force --options runtime --timestamp \
    --entitlements "$ENTITLEMENTS" \
    --sign "$SIGN_IDENTITY" \
    "$APP_BUNDLE/Contents/MacOS/claude-meter"
codesign --force --options runtime --timestamp \
    --entitlements "$ENTITLEMENTS" \
    --sign "$SIGN_IDENTITY" \
    "$APP_BUNDLE"
codesign --verify --deep --strict --verbose=2 "$APP_BUNDLE"

echo "[4/7] submitting for notarization (this takes ~1-3 min)..."
ditto -c -k --keepParent "$APP_BUNDLE" "$OUT_DIR/notarize.zip"
xcrun notarytool submit "$OUT_DIR/notarize.zip" \
    --keychain-profile "$NOTARY_PROFILE" \
    --wait
rm -f "$OUT_DIR/notarize.zip"

echo "[5/7] stapling notarization ticket..."
xcrun stapler staple "$APP_BUNDLE"
xcrun stapler validate "$APP_BUNDLE"
spctl --assess --type execute --verbose "$APP_BUNDLE" || true

echo "[6/7] producing a zipped .app for direct download..."
rm -f "$ZIP_PATH"
ditto -c -k --keepParent "$APP_BUNDLE" "$ZIP_PATH"

echo "[7/7] building + signing DMG (notarization optional via SKIP_DMG_NOTARIZE=1)..."
rm -f "$DMG_PATH"
STAGING="$OUT_DIR/dmg-staging-$$"
rm -rf "$STAGING"
mkdir -p "$STAGING"
ditto "$APP_BUNDLE" "$STAGING/$APP_NAME.app"
ln -s /Applications "$STAGING/Applications"
hdiutil create -volname "$APP_NAME" -srcfolder "$STAGING" -ov -format UDZO "$DMG_PATH" >/dev/null
rm -rf "$STAGING"
codesign --force --sign "$SIGN_IDENTITY" "$DMG_PATH"
if [ "${SKIP_DMG_NOTARIZE:-0}" = "1" ]; then
    echo "   skipping DMG notarization (SKIP_DMG_NOTARIZE=1); zip is already notarized+stapled."
else
    xcrun notarytool submit "$DMG_PATH" --keychain-profile "$NOTARY_PROFILE" --wait
    xcrun stapler staple "$DMG_PATH"
fi

echo
echo "done:"
echo "  $APP_BUNDLE   (signed, notarized, stapled)"
echo "  $ZIP_PATH   (ready to ship)"
echo "  $DMG_PATH   $([ "${SKIP_DMG_NOTARIZE:-0}" = "1" ] && echo "(signed only, not notarized)" || echo "(signed, notarized, stapled)")"
