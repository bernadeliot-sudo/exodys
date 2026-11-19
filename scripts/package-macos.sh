#!/bin/sh
set -eu

if [ "$(uname -s)" != Darwin ]; then
    printf '%s\n' 'Relay Desk must be packaged on macOS.' >&2
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
install -m 755 "$project/target/release/relay-desk" "$contents/MacOS/relay-desk"
install -m 644 LICENSE "$contents/Resources/LICENSE"

plist="$contents/Info.plist"
/usr/bin/plutil -create xml1 "$plist"
/usr/bin/plutil -insert CFBundleName -string "$name" "$plist"
/usr/bin/plutil -insert CFBundleDisplayName -string "$name" "$plist"
/usr/bin/plutil -insert CFBundleIdentifier -string "$identifier" "$plist"
/usr/bin/plutil -insert CFBundleVersion -string "$version" "$plist"
/usr/bin/plutil -insert CFBundleShortVersionString -string "$version" "$plist"
/usr/bin/plutil -insert CFBundleExecutable -string relay-desk "$plist"
/usr/bin/plutil -insert CFBundlePackageType -string APPL "$plist"
/usr/bin/plutil -insert NSHighResolutionCapable -bool YES "$plist"
/usr/bin/codesign --sign - "$app"
/usr/bin/codesign --verify --strict "$app"

archive="$output/Relay-Desk-$version-macos-$(uname -m).zip"
/usr/bin/ditto -c -k --keepParent "$app" "$archive"
printf '%s\n' "$app" "$archive"
