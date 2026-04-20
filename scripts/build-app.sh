#!/usr/bin/env bash
# Build ClaudeMeter.app from the release binary.
# Usage: scripts/build-app.sh [out-dir]
#   Default out-dir is ./dist

set -euo pipefail

out_dir="${1:-dist}"
app_name="ClaudeMeter"
bundle_id="com.m13v.claude-meter"
version="0.1.0"

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"

echo "building icons + release binaries..." >&2
bash "$root/scripts/build-icons.sh" >&2
cargo build --release --bin claude-meter --bin claude-meter-menubar >&2

app_path="$out_dir/$app_name.app"
rm -rf "$app_path"
mkdir -p "$app_path/Contents/MacOS"
mkdir -p "$app_path/Contents/Resources"

cp "target/release/claude-meter-menubar" "$app_path/Contents/MacOS/$app_name"
# Also ship the CLI next to it for users who want both.
cp "target/release/claude-meter" "$app_path/Contents/MacOS/claude-meter"
cp "$root/assets/AppIcon.icns" "$app_path/Contents/Resources/AppIcon.icns"

cat > "$app_path/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key>
    <string>$app_name</string>
    <key>CFBundleDisplayName</key>
    <string>$app_name</string>
    <key>CFBundleIdentifier</key>
    <string>$bundle_id</string>
    <key>CFBundleVersion</key>
    <string>$version</string>
    <key>CFBundleShortVersionString</key>
    <string>$version</string>
    <key>CFBundleExecutable</key>
    <string>$app_name</string>
    <key>CFBundleIconFile</key>
    <string>AppIcon</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>LSMinimumSystemVersion</key>
    <string>11.0</string>
    <key>LSUIElement</key>
    <true/>
    <key>NSHighResolutionCapable</key>
    <true/>
</dict>
</plist>
PLIST

echo "$app_path" >&2
echo "drop it into /Applications, or run:  open \"$app_path\"" >&2
