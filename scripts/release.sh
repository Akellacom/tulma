#!/usr/bin/env bash
# Builds a Developer ID signed, notarized Tulma DMG for distribution outside the App Store.
#
#   scripts/release.sh [VERSION]      VERSION like 0.2.0; defaults to MARKETING_VERSION in project.yml
#
# Signing uses the "Developer ID Application" identity in the keychain (override: SIGN_IDENTITY).
# Notarization credentials, one of:
#   NOTARY_KEY + NOTARY_KEY_ID + NOTARY_ISSUER   App Store Connect API key (.p8 path); used in CI
#   NOTARY_PROFILE (default: tulma-notary)       saved once with `xcrun notarytool store-credentials`
# NOTARIZE=0 signs without notarizing: fine for checking the build, not for publishing.
#
# Output: build/release/Tulma-<version>-arm64.dmg and Tulma-<version>-arm64.dmg.sha256

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

log() { printf '\n==> %s\n' "$*"; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }

VERSION="${1:-$(sed -n 's/^ *MARKETING_VERSION: *"\{0,1\}\([0-9.]*\)"\{0,1\} *$/\1/p' project.yml | head -1)}"
VERSION="${VERSION#v}"
# CFBundleShortVersionString allows only up to three dot-separated integers.
[[ "$VERSION" =~ ^[0-9]+(\.[0-9]+){1,2}$ ]] || die "version must look like 1.2.3, got '$VERSION'"
BUILD_NUMBER="${BUILD_NUMBER:-$(git rev-list --count HEAD 2>/dev/null || echo 1)}"
NOTARIZE="${NOTARIZE:-1}"

OUT="$ROOT/build/release"
DERIVED="$ROOT/build/release-derived"
APP="$OUT/Tulma.app"
DMG="$OUT/Tulma-$VERSION-arm64.dmg"

SIGN_IDENTITY="${SIGN_IDENTITY:-$(security find-identity -v -p codesigning \
    | sed -n 's/.*"\(Developer ID Application: .*\)"$/\1/p' | head -1)}"
[[ -n "$SIGN_IDENTITY" ]] || die "no 'Developer ID Application' certificate found in the keychain"
TEAM_ID="${TEAM_ID:-$(sed -n 's/.*(\([A-Z0-9]\{10\}\))$/\1/p' <<<"$SIGN_IDENTITY")}"
[[ -n "$TEAM_ID" ]] || die "cannot read the team ID from '$SIGN_IDENTITY'; set TEAM_ID"

if [[ -n "${NOTARY_KEY:-}" ]]; then
    NOTARY=(--key "$NOTARY_KEY" --key-id "${NOTARY_KEY_ID:?set NOTARY_KEY_ID}" --issuer "${NOTARY_ISSUER:?set NOTARY_ISSUER}")
else
    NOTARY=(--keychain-profile "${NOTARY_PROFILE:-tulma-notary}")
fi

command -v xcodegen >/dev/null || die "xcodegen not found: brew install xcodegen"
command -v cargo >/dev/null || die "cargo not found: install Rust with rustup"

CARGO_VERSION="$(sed -n 's/^version = "\(.*\)"$/\1/p' core/Cargo.toml | head -1)"
if [[ "$CARGO_VERSION" != "$VERSION" ]]; then
    printf 'warning: core/Cargo.toml says %s, so `tulma --version` will not report %s\n' "$CARGO_VERSION" "$VERSION" >&2
fi

if [[ "$NOTARIZE" == 1 ]]; then
    # Fail on bad credentials now rather than after a ten-minute build.
    xcrun notarytool history "${NOTARY[@]}" >/dev/null \
        || die "notarization credentials do not work; see docs/releasing.md"
fi

log "Building Tulma $VERSION ($BUILD_NUMBER), signed by $SIGN_IDENTITY"
rm -rf "$OUT" "$DERIVED"
mkdir -p "$OUT"
# Build the daemon before generating the project: xcodegen drops the "copy into
# Contents/Helpers" phase when core/target/release/tulma does not exist yet.
(cd core && cargo build --release --locked -p tulma)
xcodegen generate --quiet
xcodebuild -project Tulma.xcodeproj -scheme Tulma -configuration Release \
    -derivedDataPath "$DERIVED" -destination 'platform=macOS,arch=arm64' \
    MARKETING_VERSION="$VERSION" CURRENT_PROJECT_VERSION="$BUILD_NUMBER" \
    CODE_SIGN_STYLE=Manual CODE_SIGN_IDENTITY="$SIGN_IDENTITY" DEVELOPMENT_TEAM="$TEAM_ID" \
    OTHER_CODE_SIGN_FLAGS="--timestamp" CODE_SIGN_INJECT_BASE_ENTITLEMENTS=NO \
    build -quiet
ditto "$DERIVED/Build/Products/Release/Tulma.app" "$APP"
[[ -x "$APP/Contents/Helpers/tulma" ]] || die "the daemon is missing from Tulma.app/Contents/Helpers"

log "Signing"
# Notarization needs every executable signed with the hardened runtime and a secure timestamp;
# re-sign both explicitly rather than trusting Xcode's signing. Tulma needs no entitlements, and
# re-signing without any also drops get-task-allow, which notarization rejects.
codesign --force --timestamp --options runtime --sign "$SIGN_IDENTITY" "$APP/Contents/Helpers/tulma"
codesign --force --timestamp --options runtime --sign "$SIGN_IDENTITY" "$APP"
codesign --verify --strict --deep --verbose=2 "$APP"
for binary in "$APP/Contents/MacOS/Tulma" "$APP/Contents/Helpers/tulma"; do
    details="$(codesign --display --verbose=4 "$binary" 2>&1)"
    [[ "$details" == *"(runtime)"* ]] || die "$binary is not signed with the hardened runtime"
    [[ "$details" == *"Timestamp="* ]] || die "$binary has no secure timestamp"
    entitlements="$(codesign --display --entitlements - --xml "$binary" 2>/dev/null || true)"
    [[ "$entitlements" != *"get-task-allow"* ]] || die "$binary has the get-task-allow entitlement"
done

notarize() {
    local file="$1" result status id
    log "Notarizing $(basename "$file") (usually a few minutes)"
    result="$(xcrun notarytool submit "$file" "${NOTARY[@]}" --wait --output-format json)" || true
    status="$(plutil -extract status raw -o - - <<<"$result" 2>/dev/null || echo unknown)"
    id="$(plutil -extract id raw -o - - <<<"$result" 2>/dev/null || true)"
    if [[ "$status" != "Accepted" ]]; then
        printf '%s\n' "$result" >&2
        [[ -n "$id" ]] && xcrun notarytool log "$id" "${NOTARY[@]}" >&2 || true
        die "notarization of $(basename "$file") ended with status '$status'"
    fi
}

if [[ "$NOTARIZE" == 1 ]]; then
    ditto -c -k --keepParent "$APP" "$OUT/Tulma.zip"
    notarize "$OUT/Tulma.zip"
    rm "$OUT/Tulma.zip"
    xcrun stapler staple "$APP"
fi

log "Packaging $(basename "$DMG")"
STAGE="$OUT/dmg"
mkdir -p "$STAGE"
ditto "$APP" "$STAGE/Tulma.app"
ln -s /Applications "$STAGE/Applications"
hdiutil create -volname "Tulma" -srcfolder "$STAGE" -fs HFS+ -format UDZO -ov -quiet "$DMG"
rm -rf "$STAGE"
codesign --force --timestamp --sign "$SIGN_IDENTITY" "$DMG"

if [[ "$NOTARIZE" == 1 ]]; then
    notarize "$DMG"
    xcrun stapler staple "$DMG"
    log "Checking Gatekeeper"
    spctl --assess --type execute --verbose=2 "$APP"
    spctl --assess --type open --context context:primary-signature --verbose=2 "$DMG"
else
    printf '\nwarning: NOTARIZE=0, so Gatekeeper will block this DMG on other Macs\n' >&2
fi

(cd "$OUT" && shasum -a 256 "$(basename "$DMG")" > "$(basename "$DMG").sha256")
log "Done: $DMG"
cat "$DMG.sha256"
