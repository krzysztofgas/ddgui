#!/bin/sh
set -eu

cd "$(dirname "$0")"

NAME=ddgui
APP="target/$NAME.app"

rustup target add x86_64-apple-darwin aarch64-apple-darwin >/dev/null

MACOSX_DEPLOYMENT_TARGET=10.12 cargo build --release --target x86_64-apple-darwin
MACOSX_DEPLOYMENT_TARGET=11.0 cargo build --release --target aarch64-apple-darwin

rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS"
lipo -create \
    "target/x86_64-apple-darwin/release/$NAME" \
    "target/aarch64-apple-darwin/release/$NAME" \
    -output "$APP/Contents/MacOS/$NAME"

VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n 1)

cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key>
    <string>$NAME</string>
    <key>CFBundleDisplayName</key>
    <string>$NAME</string>
    <key>CFBundleExecutable</key>
    <string>$NAME</string>
    <key>CFBundleIdentifier</key>
    <string>local.$NAME</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>CFBundleShortVersionString</key>
    <string>$VERSION</string>
    <key>CFBundleVersion</key>
    <string>$VERSION</string>
    <key>LSMinimumSystemVersion</key>
    <string>10.12</string>
    <key>LSApplicationCategoryType</key>
    <string>public.app-category.utilities</string>
    <key>NSHighResolutionCapable</key>
    <true/>
    <key>NSSupportsAutomaticGraphicsSwitching</key>
    <true/>
</dict>
</plist>
PLIST

codesign --force --sign - "$APP"
echo "$APP"
