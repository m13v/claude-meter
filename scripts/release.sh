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

echo "[1/8] building release binaries..."
bash scripts/build-icons.sh
cargo build --release --bin claude-meter --bin claude-meter-menubar

echo "[2/8] assembling $APP_BUNDLE..."
rm -rf "$APP_BUNDLE"
mkdir -p "$APP_BUNDLE/Contents/MacOS"
mkdir -p "$APP_BUNDLE/Contents/Resources"
cp "target/release/claude-meter-menubar" "$APP_BUNDLE/Contents/MacOS/$APP_NAME"
cp "target/release/claude-meter" "$APP_BUNDLE/Contents/MacOS/claude-meter"
cp "assets/AppIcon.icns" "$APP_BUNDLE/Contents/Resources/AppIcon.icns"

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
    <key>CFBundleIconFile</key><string>AppIcon</string>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>LSMinimumSystemVersion</key><string>11.0</string>
    <key>LSUIElement</key><true/>
    <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
PLIST

echo "[3/8] codesigning (hardened runtime, Developer ID)..."
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

echo "[4/8] submitting for notarization (this takes ~1-3 min)..."
ditto -c -k --keepParent "$APP_BUNDLE" "$OUT_DIR/notarize.zip"
xcrun notarytool submit "$OUT_DIR/notarize.zip" \
    --keychain-profile "$NOTARY_PROFILE" \
    --wait
rm -f "$OUT_DIR/notarize.zip"

echo "[5/8] stapling notarization ticket..."
xcrun stapler staple "$APP_BUNDLE"
xcrun stapler validate "$APP_BUNDLE"
spctl --assess --type execute --verbose "$APP_BUNDLE" || true

echo "[6/8] producing a zipped .app for direct download..."
rm -f "$ZIP_PATH"
ditto -c -k --keepParent "$APP_BUNDLE" "$ZIP_PATH"

echo "[7/8] building + signing DMG (notarization optional via SKIP_DMG_NOTARIZE=1)..."
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

echo "[8/8] publishing GitHub release v$VERSION (assets: DMG + zip)..."
# Without this step the website's /api/download endpoint keeps serving the
# previous release's DMG (it polls api.github.com/.../releases/latest), so
# users get the old version after a tag-only push. Idempotent: if the release
# already exists, just upload/clobber the assets.
if [ "${SKIP_GH_RELEASE:-0}" = "1" ]; then
    echo "   skipping GitHub release publish (SKIP_GH_RELEASE=1)."
else
    GH_BIN="$(command -v gh || echo /opt/homebrew/bin/gh)"
    if [ ! -x "$GH_BIN" ]; then
        echo "   gh CLI not found; install with: brew install gh" >&2
        echo "   skipping; upload manually at https://github.com/m13v/claude-meter/releases/new?tag=v$VERSION"
    else
        REPO="m13v/claude-meter"
        # Ensure the tag exists locally and on origin.
        if ! git rev-parse "v$VERSION" >/dev/null 2>&1; then
            echo "   tag v$VERSION not found locally; create + push it before running release." >&2
            exit 1
        fi
        git push origin "v$VERSION" 2>/dev/null || true
        if "$GH_BIN" release view "v$VERSION" --repo "$REPO" >/dev/null 2>&1; then
            echo "   release v$VERSION exists; uploading + clobbering assets..."
            "$GH_BIN" release upload "v$VERSION" --repo "$REPO" --clobber \
                "$DMG_PATH" "$ZIP_PATH"
        else
            echo "   creating release v$VERSION + uploading assets..."
            NOTES="${RELEASE_NOTES:-Release v$VERSION. See commits since previous tag for details.}"
            "$GH_BIN" release create "v$VERSION" --repo "$REPO" \
                --title "v$VERSION" \
                --notes "$NOTES" \
                "$DMG_PATH" "$ZIP_PATH"
        fi
        echo "   published: https://github.com/$REPO/releases/tag/v$VERSION"
    fi
fi

echo
echo "done:"
echo "  $APP_BUNDLE   (signed, notarized, stapled)"
echo "  $ZIP_PATH   (ready to ship)"
echo "  $DMG_PATH   $([ "${SKIP_DMG_NOTARIZE:-0}" = "1" ] && echo "(signed only, not notarized)" || echo "(signed, notarized, stapled)")"
