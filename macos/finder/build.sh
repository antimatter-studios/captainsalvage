#!/bin/bash
# Builds "Captain Salvage Finder.app", the helper that shows canvases in Finder, and signs it.
#
# A File Provider extension only runs when it is signed by a team with a provisioning profile
# for its identifier and the app group, so this needs the team's development key and both
# profiles. None of them belong in the repo; pass their paths:
#
#   CS_SIGN_KEY      development private key, PEM
#   CS_SIGN_CERT     development certificate, PEM
#   CS_APP_PROFILE   profile for com.antimatterstudios.captainsalvage
#   CS_EXT_PROFILE   profile for com.antimatterstudios.captainsalvage.fileprovider
#
# Usage: macos/finder/build.sh [--install]
#   --install  copies the app to ~/Applications and registers its extension with macOS.
set -euo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
OUT="$HERE/build"
NAME="Captain Salvage Finder"
APP="$OUT/$NAME.app"
EXT="$APP/Contents/PlugIns/Captain Salvage.appex"
TEAM=43UMKXZ8P4
ID=com.antimatterstudios.captainsalvage
GROUP=group.com.antimatterstudios.captainsalvage
VERSION=$(python3 "$HERE/../../scripts/release.py" version 2>/dev/null || echo 0.0.0)
LSREGISTER=/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister

for name in CS_SIGN_KEY CS_SIGN_CERT CS_APP_PROFILE CS_EXT_PROFILE; do
    if [ ! -f "${!name:-}" ]; then
        echo "Set $name to the path of its file." >&2
        exit 1
    fi
done

rm -rf "$OUT"
mkdir -p "$APP/Contents/MacOS" "$EXT/Contents/MacOS"

swiftc -O -parse-as-library -module-name CaptainSalvageFinderExtension -application-extension \
    -Xlinker -e -Xlinker _NSExtensionMain -framework FileProvider -framework Foundation \
    -o "$EXT/Contents/MacOS/captain-salvage-extension" "$HERE/Extension.swift"
swiftc -O -framework FileProvider -framework Foundation \
    -o "$APP/Contents/MacOS/captain-salvage-finder" "$HERE/Helper.swift"

plist() {
    cat <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>$1</dict></plist>
EOF
}
plist "
<key>CFBundleIdentifier</key><string>$ID</string>
<key>CFBundleName</key><string>$NAME</string>
<key>CFBundleExecutable</key><string>captain-salvage-finder</string>
<key>CFBundlePackageType</key><string>APPL</string>
<key>CFBundleShortVersionString</key><string>$VERSION</string>
<key>CFBundleVersion</key><string>$VERSION</string>
<key>LSMinimumSystemVersion</key><string>13.0</string>
<key>LSUIElement</key><true/>" > "$APP/Contents/Info.plist"
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
<key>com.apple.security.application-groups</key><array><string>$GROUP</string></array>" > "$OUT/extension.entitlements"
plist "
<key>com.apple.application-identifier</key><string>$TEAM.$ID</string>
<key>com.apple.developer.team-identifier</key><string>$TEAM</string>
<key>com.apple.security.application-groups</key><array><string>$GROUP</string></array>" > "$OUT/helper.entitlements"
cp "$CS_APP_PROFILE" "$APP/Contents/embedded.provisionprofile"
cp "$CS_EXT_PROFILE" "$EXT/Contents/embedded.provisionprofile"

# A keychain of its own, so the key is never added to the login keychain, and is gone after.
KEYCHAIN="$OUT/signing.keychain-db"
PASSWORD=$(openssl rand -hex 24)
ORIGINAL=$(security list-keychains -d user | tr -d '"')
restore() {
    # shellcheck disable=SC2086
    security list-keychains -d user -s $ORIGINAL >/dev/null 2>&1 || true
    security delete-keychain "$KEYCHAIN" >/dev/null 2>&1 || true
    rm -f "$KEYCHAIN"
}
trap restore EXIT
security create-keychain -p "$PASSWORD" "$KEYCHAIN"
security set-keychain-settings "$KEYCHAIN"
security unlock-keychain -p "$PASSWORD" "$KEYCHAIN"
security import "$CS_SIGN_KEY" -k "$KEYCHAIN" -T /usr/bin/codesign >/dev/null
security import "$CS_SIGN_CERT" -k "$KEYCHAIN" >/dev/null
INTERMEDIATE="$OUT/AppleWWDRCAG3.cer"
curl -fsS --max-time 30 -o "$INTERMEDIATE" https://www.apple.com/certificateauthority/AppleWWDRCAG3.cer
security import "$INTERMEDIATE" -k "$KEYCHAIN" >/dev/null
security set-key-partition-list -S apple-tool:,apple:,codesign: -s -k "$PASSWORD" "$KEYCHAIN" >/dev/null
# shellcheck disable=SC2086
security list-keychains -d user -s "$KEYCHAIN" $ORIGINAL
IDENTITY=$(security find-identity -v -p codesigning "$KEYCHAIN" | grep -oE '[0-9A-F]{40}' | head -1)
if [ -z "$IDENTITY" ]; then
    echo "The key and certificate do not make a signing identity." >&2
    exit 1
fi

codesign --force --timestamp=none --keychain "$KEYCHAIN" -s "$IDENTITY" \
    --entitlements "$OUT/extension.entitlements" "$EXT"
codesign --force --timestamp=none --keychain "$KEYCHAIN" -s "$IDENTITY" \
    --entitlements "$OUT/helper.entitlements" "$APP"
codesign --verify --deep --strict "$APP"
echo "Built $APP"

if [ "${1:-}" = "--install" ]; then
    TARGET="$HOME/Applications/$NAME.app"
    mkdir -p "$HOME/Applications"
    rm -rf "$TARGET"
    cp -R "$APP" "$TARGET"
    "$LSREGISTER" -f -R -trusted "$TARGET"
    pluginkit -a "$TARGET/Contents/PlugIns/Captain Salvage.appex"
    echo "Installed $TARGET"
fi
