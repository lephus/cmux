#!/usr/bin/env bash
# Build, sign, notarize and staple ShellX.app under Apple team W4RF3W4VTZ.
#
# Usage:
#   scripts/shellx/sign-and-notarize.sh --build [derived-data-dir]
#       Release build with ShellX ids and a local GhosttyKit; prints the .app path.
#       This is the one sanctioned non-tagged build: it uses ShellX ids, so it
#       cannot collide with a cmux install. Run it with CMUX_GHOSTTYKIT_NO_PREBUILT=1.
#   scripts/shellx/sign-and-notarize.sh <app-path>
#       Sign inside-out (scripts/sign-cmux-bundle.sh), notarize with the keychain
#       profile shellx-notary, staple, then verify with stapler and spctl.
#
# One-time setup, done by hand (this script never handles credentials):
#   xcrun notarytool store-credentials shellx-notary \
#     --apple-id <apple-id> --team-id W4RF3W4VTZ
#
# Optional env:
#   SHELLX_SIGNING_IDENTITY  default "Developer ID Application: Phu Le huu (W4RF3W4VTZ)"
#   SHELLX_NOTARY_PROFILE    default shellx-notary

set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

IDENTITY="${SHELLX_SIGNING_IDENTITY:-Developer ID Application: Phu Le huu (W4RF3W4VTZ)}"
NOTARY_PROFILE="${SHELLX_NOTARY_PROFILE:-shellx-notary}"
BUNDLE_ID="pro.shellx.desktop"
PRODUCT_NAME="ShellX"
ENTITLEMENTS="$REPO_ROOT/shellx.release.entitlements"

usage() {
  echo "usage: $0 --build [derived-data-dir] | $0 <app-path>" >&2
  exit 2
}

build_app() {
  local derived="${1:-$REPO_ROOT/build-shellx}"
  cd "$REPO_ROOT"
  "$REPO_ROOT/scripts/ensure-ghosttykit.sh" >&2
  # No Go toolchain is needed: the tunnel extension is dropped at signing time,
  # so its wireguard-go archive may be the stub (Release otherwise requires Go).
  # SWIFT_OPTIMIZATION_LEVEL=-Onone: with Xcode 26.2 (Swift 6.2.3) an -O build dies
  # in the SIL linker ("SILFunction type mismatch for ghostty_surface_clear_selection"):
  # CmuxTerminalCore declares it via @_silgen_name while the app imports it from the
  # GhosttyKit header. W0 only needs a signable bundle; W1 must fix the duplicate
  # declaration (or pin Xcode 26.3+) before shipping an optimized build.
  # ENABLE_DEBUG_DYLIB=NO: -Onone otherwise splits the app into cmux.debug.dylib
  # and __preview.dylib, which the inside-out signer never signs (notary rejects).
  CMUX_WIREGUARD_GO_REQUIRE=0 xcodebuild \
    -project cmux.xcodeproj \
    -scheme cmux \
    -configuration Release \
    -destination 'platform=macOS' \
    -derivedDataPath "$derived" \
    PRODUCT_BUNDLE_IDENTIFIER="$BUNDLE_ID" \
    INFOPLIST_KEY_CFBundleName="$PRODUCT_NAME" \
    INFOPLIST_KEY_CFBundleDisplayName="$PRODUCT_NAME" \
    SWIFT_OPTIMIZATION_LEVEL=-Onone \
    ENABLE_DEBUG_DYLIB=NO \
    ARCHS=arm64 \
    ONLY_ACTIVE_ARCH=YES \
    CODE_SIGNING_ALLOWED=NO \
    CODE_SIGN_ENTITLEMENTS= \
    build >&2
  # PRODUCT_NAME is a project-wide override, so setting it also renames every
  # Swift package product and collides ("Multiple commands produce ShellX.bundle").
  # The executable stays "cmux" until W1 renames the app target in the project;
  # only the bundle directory and display names are ShellX here.
  # The Cloud tunnel system extension is embedded by the build; the signing step
  # removes it because shellx.release.entitlements does not request the tunnel.
  local products="$derived/Build/Products/Release"
  rm -rf "$products/$PRODUCT_NAME.app"
  mv "$products/cmux.app" "$products/$PRODUCT_NAME.app"
  echo "$products/$PRODUCT_NAME.app"
}

sign_and_notarize() {
  local app="$1"
  if [[ ! -d "$app" ]]; then
    echo "error: app bundle not found at $app" >&2
    exit 1
  fi
  app="$(cd "$app" && pwd)"
  cd "$REPO_ROOT"

  echo "==> signing $app"
  "$REPO_ROOT/scripts/sign-cmux-bundle.sh" "$app" "$ENTITLEMENTS" "$IDENTITY"

  local work zip
  work="$(mktemp -d "${TMPDIR:-/tmp}/shellx-notarize.XXXXXX")"
  # Expand now: the EXIT trap runs after this function's locals are gone (set -u).
  trap "rm -rf '$work'" EXIT
  zip="$work/ShellX.zip"
  /usr/bin/ditto -c -k --keepParent "$app" "$zip"

  echo "==> submitting to the Apple notary service (profile $NOTARY_PROFILE)"
  local result
  result="$(xcrun notarytool submit "$zip" --keychain-profile "$NOTARY_PROFILE" --wait --output-format json)"
  echo "$result"
  local status
  status="$(python3 -c 'import json,sys; print(json.load(sys.stdin).get("status", ""))' <<<"$result")"
  if [[ "$status" != "Accepted" ]]; then
    local id
    id="$(python3 -c 'import json,sys; print(json.load(sys.stdin).get("id", ""))' <<<"$result")"
    echo "error: notarization finished with status '$status' (submission $id)" >&2
    [[ -n "$id" ]] && xcrun notarytool log "$id" --keychain-profile "$NOTARY_PROFILE" >&2 || true
    exit 1
  fi

  echo "==> stapling"
  xcrun stapler staple "$app"
  xcrun stapler validate "$app"

  echo "==> Gatekeeper assessment"
  /usr/bin/codesign --verify --deep --strict --verbose=2 "$app"
  spctl -a -vv -t exec "$app" 2>&1 | tee "$work/spctl.txt"
  grep -q 'source=Notarized Developer ID' "$work/spctl.txt" || {
    echo "error: Gatekeeper did not report source=Notarized Developer ID" >&2
    exit 1
  }
  echo "==> signed, notarized and stapled: $app"
}

case "${1:-}" in
  --build) build_app "${2:-}" ;;
  ""|-h|--help|--*) usage ;;
  *) [[ $# -eq 1 ]] || usage; sign_and_notarize "$1" ;;
esac
