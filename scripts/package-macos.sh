#!/bin/sh
set -eu

if [ "$(uname -s)" != Darwin ]; then
    printf '%s\n' 'Exodys must be packaged on macOS.' >&2
    exit 1
fi

project=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
cd "$project"

name=$(/usr/bin/plutil -extract name raw -o - nsh.json)
identifier=$(/usr/bin/plutil -extract id raw -o - nsh.json)
version=$(/usr/bin/plutil -extract version raw -o - nsh.json)
cargo build --locked --release --target-dir "$project/target"

mkdir -p "$project/.nsh/out"
output=$(mktemp -d "$project/.nsh/out/package.XXXXXX")
app="$output/$name.app"
contents="$app/Contents"
mkdir -p "$contents/MacOS" "$contents/Resources"
install -m 755 "$project/target/release/exodys" "$contents/MacOS/exodys"
install -m 644 LICENSE "$contents/Resources/LICENSE"

icon_work=$(mktemp -d "${TMPDIR:-/tmp}/exodys-icon.XXXXXX")
trap 'rm -rf -- "$icon_work"' EXIT
iconset="$icon_work/Exodys.iconset"
mkdir "$iconset"
for size in 16 32 128 256 512; do
    retina=$((size * 2))
    /usr/bin/sips -z "$size" "$size" assets/Exodys.png \
        --out "$iconset/icon_${size}x${size}.png" >/dev/null
    /usr/bin/sips -z "$retina" "$retina" assets/Exodys.png \
        --out "$iconset/icon_${size}x${size}@2x.png" >/dev/null
done
/usr/bin/iconutil -c icns "$iconset" -o "$contents/Resources/Exodys.icns"

plist="$contents/Info.plist"
/usr/bin/plutil -create xml1 "$plist"
/usr/bin/plutil -insert CFBundleName -string "$name" "$plist"
/usr/bin/plutil -insert CFBundleDisplayName -string "$name" "$plist"
/usr/bin/plutil -insert CFBundleIdentifier -string "$identifier" "$plist"
/usr/bin/plutil -insert CFBundleVersion -string "$version" "$plist"
/usr/bin/plutil -insert CFBundleShortVersionString -string "$version" "$plist"
/usr/bin/plutil -insert CFBundleExecutable -string exodys "$plist"
/usr/bin/plutil -insert CFBundlePackageType -string APPL "$plist"
/usr/bin/plutil -insert CFBundleIconFile -string Exodys.icns "$plist"
/usr/bin/plutil -insert NSHighResolutionCapable -bool YES "$plist"
/usr/bin/codesign --sign - "$app"
/usr/bin/codesign --verify --strict "$app"

archive="$output/Exodys-$version-macos-$(uname -m).zip"
/usr/bin/ditto -c -k --keepParent "$app" "$archive"
printf '%s\n' "$app" "$archive"
