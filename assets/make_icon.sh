#!/bin/sh
set -eu

cd "$(dirname "$0")"

WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

swift icon.swift "$WORK/icon.png"

SET="$WORK/ddgui.iconset"
mkdir "$SET"
for size in 16 32 128 256 512; do
    sips -z $size $size "$WORK/icon.png" --out "$SET/icon_${size}x${size}.png" >/dev/null
    double=$((size * 2))
    sips -z $double $double "$WORK/icon.png" --out "$SET/icon_${size}x${size}@2x.png" >/dev/null
done

iconutil -c icns "$SET" -o ddgui.icns
cp "$WORK/icon.png" icon.png
echo "assets/ddgui.icns"
