#!/bin/bash
# Puts the Finder extension inside a built "Captain Salvage.app", and signs the whole app with
# the team's key. The app adds and removes its Finder locations itself, which only the app
# holding the extension, signed into its app group, may do.
#
# A File Provider extension only runs when the app holding it is signed by a team, with a
# provisioning profile for its identifier and the app group. None of that belongs in the
# repo; pass the paths:
#
#   CS_SIGN_KEY      development private key, PEM
#   CS_SIGN_CERT     development certificate, PEM
#   CS_APP_PROFILE   profile for com.antimatterstudios.captainsalvage
#   CS_EXT_PROFILE   profile for com.antimatterstudios.captainsalvage.fileprovider
#
# Without them the app is left as it was built: it runs, and Show in Finder is not offered.
# A signature that stays the same from build to build also keeps network filters' rules for
# the app working.
#
# Usage: macos/finder/package.sh "path/to/Captain Salvage.app"
set -euo pipefail

APP=${1:?give the path of the built app}
HERE=$(cd "$(dirname "$0")" && pwd)
TEAM=43UMKXZ8P4
ID=com.antimatterstudios.captainsalvage
GROUP=group.com.antimatterstudios.captainsalvage
EXT="$APP/Contents/PlugIns/Captain Salvage.appex"

for name in CS_SIGN_KEY CS_SIGN_CERT CS_APP_PROFILE CS_EXT_PROFILE; do
    if [ ! -f "${!name:-}" ]; then
        echo "No signing material ($name is not set), so the app has no Finder extension."
        exit 0
    fi
done

WORK=$(mktemp -d)
VERSION=$(/usr/libexec/PlistBuddy -c "Print :CFBundleShortVersionString" "$APP/Contents/Info.plist")
rm -rf "$EXT"
mkdir -p "$EXT/Contents/MacOS"

swiftc -O -parse-as-library -module-name CaptainSalvageFinderExtension -application-extension \
    -Xlinker -e -Xlinker _NSExtensionMain -framework FileProvider -framework Foundation \
    -o "$EXT/Contents/MacOS/captain-salvage-extension" "$HERE/Extension.swift"

plist() {
    cat <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>$1</dict></plist>
PLIST
}
plist "
<key>CFBundleIdentifier</key><string>$ID.fileprovider</string>
<key>CFBundleName</key><string>Captain Salvage</string>
<key>CFBundleDisplayName</key><string>Captain Salvage</string>
<key>CFBundleExecutable</key><string>captain-salvage-extension</string>
<key>CFBundlePackageType</key><string>XPC!</string>
<key>CFBundleShortVersionString</key><string>$VERSION</string>
<key>CFBundleVersion</key><string>$VERSION</string>
<key>LSMinimumSystemVersion</key><string>13.0</string>
<key>NSExtension</key><dict>
  <key>NSExtensionPointIdentifier</key><string>com.apple.fileprovider-nonui</string>
  <key>NSExtensionPrincipalClass</key><string>FileProviderExtension</string>
  <key>NSExtensionFileProviderDocumentGroup</key><string>$GROUP</string>
  <key>NSExtensionFileProviderSupportsEnumeration</key><true/>
</dict>" > "$EXT/Contents/Info.plist"

plist "
<key>com.apple.application-identifier</key><string>$TEAM.$ID.fileprovider</string>
<key>com.apple.developer.team-identifier</key><string>$TEAM</string>
<key>com.apple.security.app-sandbox</key><true/>
<key>com.apple.security.network.client</key><true/>
<key>com.apple.security.application-groups</key><array><string>$GROUP</string></array>" > "$WORK/extension.entitlements"
plist "
<key>com.apple.application-identifier</key><string>$TEAM.$ID</string>
<key>com.apple.developer.team-identifier</key><string>$TEAM</string>
<key>com.apple.security.application-groups</key><array><string>$GROUP</string></array>" > "$WORK/app.entitlements"
cp "$CS_APP_PROFILE" "$APP/Contents/embedded.provisionprofile"
cp "$CS_EXT_PROFILE" "$EXT/Contents/embedded.provisionprofile"

# A keychain of its own, so the key is never added to the login keychain, and is gone after.
KEYCHAIN="$WORK/signing.keychain-db"
PASSWORD=$(openssl rand -hex 24)
ORIGINAL=$(security list-keychains -d user | tr -d '"')
restore() {
    # shellcheck disable=SC2086
    security list-keychains -d user -s $ORIGINAL >/dev/null 2>&1 || true
    security delete-keychain "$KEYCHAIN" >/dev/null 2>&1 || true
    rm -rf "$WORK"
}
trap restore EXIT
security create-keychain -p "$PASSWORD" "$KEYCHAIN"
security set-keychain-settings "$KEYCHAIN"
security unlock-keychain -p "$PASSWORD" "$KEYCHAIN"
security import "$CS_SIGN_KEY" -k "$KEYCHAIN" -T /usr/bin/codesign >/dev/null
security import "$CS_SIGN_CERT" -k "$KEYCHAIN" >/dev/null
curl -fsS --max-time 30 -o "$WORK/AppleWWDRCAG3.cer" https://www.apple.com/certificateauthority/AppleWWDRCAG3.cer
security import "$WORK/AppleWWDRCAG3.cer" -k "$KEYCHAIN" >/dev/null
security set-key-partition-list -S apple-tool:,apple:,codesign: -s -k "$PASSWORD" "$KEYCHAIN" >/dev/null
# shellcheck disable=SC2086
security list-keychains -d user -s "$KEYCHAIN" $ORIGINAL
IDENTITY=$(security find-identity -v -p codesigning "$KEYCHAIN" | grep -oE '[0-9A-F]{40}' | head -1)
if [ -z "$IDENTITY" ]; then
    echo "The key and certificate do not make a signing identity." >&2
    exit 1
fi

sign() {
    codesign --force --timestamp=none --keychain "$KEYCHAIN" -s "$IDENTITY" "$@"
}
# Inside out: the extension first, then the app around it.
sign --entitlements "$WORK/extension.entitlements" "$EXT"
sign --entitlements "$WORK/app.entitlements" "$APP"
codesign --verify --deep --strict "$APP"
echo "Signed $APP, with its Finder extension"
