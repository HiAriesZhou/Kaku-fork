#!/usr/bin/env bash
set -euo pipefail

# Notarization script for Kaku macOS app
# Usage: ./scripts/notarize.sh [--staple-only]
#
# Prerequisites:
# 1. App must be signed with Developer ID
# 2. Preferred: App Store Connect API Key (rcodesign, avoids notarytool SIGBUS on macOS 26):
#    - Store the JSON key path in Keychain: security add-generic-password -s "kaku-asc-api-key-path" -a "kaku" -w "/path/to/asc_api_key.json"
#    - Generate with: rcodesign encode-app-store-connect-api-key -o asc_api_key.json <issuer-id> <key-id> AuthKey_*.p8
# 3. Fallback: notarytool Keychain profile:
#    - xcrun notarytool store-credentials kaku-notarytool --apple-id <apple-id> --team-id <team-id>
#    - Store the profile name in Keychain: security add-generic-password -s "kaku-notarytool-profile" -a "kaku" -w "kaku-notarytool"

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO_ROOT"

APP_NAME="Kaku"
OUT_DIR="${OUT_DIR:-dist}"
APP_BUNDLE="${OUT_DIR}/${APP_NAME}.app"
DMG_PATH="${OUT_DIR}/${APP_NAME}.dmg"
NOTARY_SUBMIT_MAX_ATTEMPTS="${NOTARY_SUBMIT_MAX_ATTEMPTS:-2}"
NOTARY_SUBMIT_RETRY_DELAY="${NOTARY_SUBMIT_RETRY_DELAY:-20}"

if ! [[ "$NOTARY_SUBMIT_MAX_ATTEMPTS" =~ ^[1-9][0-9]*$ ]]; then
	echo "Error: NOTARY_SUBMIT_MAX_ATTEMPTS must be a positive integer." >&2
	exit 1
fi
# Network failures allow one retry, even when an older environment requests more.
if (( NOTARY_SUBMIT_MAX_ATTEMPTS > 2 )); then
	NOTARY_SUBMIT_MAX_ATTEMPTS=2
fi
if ! [[ "$NOTARY_SUBMIT_RETRY_DELAY" =~ ^[0-9]+$ ]]; then
	echo "Error: NOTARY_SUBMIT_RETRY_DELAY must be a non-negative integer." >&2
	exit 1
fi

STAPLE_ONLY=0
for arg in "$@"; do
	case "$arg" in
	--staple-only) STAPLE_ONLY=1 ;;
	esac
done

is_valid_team_id() {
	[[ "$1" =~ ^[A-Z0-9]{10}$ ]]
}

require_developer_id_signature() {
	local metadata
	local signed_team_id

	metadata=$(codesign -dvvvv "$APP_BUNDLE" 2>&1) || {
		echo "Error: failed to inspect app signature." >&2
		return 1
	}

	if ! grep -q "^Authority=Developer ID Application:" <<<"$metadata"; then
		echo "Error: App must be signed with a Developer ID Application certificate before notarization." >&2
		echo "Rebuild with ./scripts/build.sh after installing a single Developer ID Application certificate, or set KAKU_SIGNING_IDENTITY explicitly." >&2
		echo "$metadata" | grep -E "^(Authority=|TeamIdentifier=|Signature=)" >&2 || true
		return 1
	fi

	signed_team_id=$(echo "$metadata" | awk -F= '/^TeamIdentifier=/{print $2; exit}')
	if ! is_valid_team_id "$signed_team_id"; then
		echo "Error: App signature does not contain a valid TeamIdentifier." >&2
		echo "$metadata" | grep -E "^(Authority=|TeamIdentifier=|Signature=)" >&2 || true
		return 1
	fi
}

# Check if app exists
if [[ ! -d "$APP_BUNDLE" ]]; then
	echo "Error: $APP_BUNDLE not found. Run ./scripts/build.sh first."
	exit 1
fi

# Verify signing
if ! codesign -v "$APP_BUNDLE" 2>/dev/null; then
	echo "Error: App is not signed. Re-run ./scripts/build.sh with a Developer ID Application certificate available."
	exit 1
fi

require_developer_id_signature || exit 1

echo "App: $APP_BUNDLE"
echo "DMG: $DMG_PATH"

# Resolve submission path
if [[ -f "$DMG_PATH" ]]; then
	SUBMISSION_PATH="$DMG_PATH"
else
	SUBMISSION_PATH="$APP_BUNDLE"
fi

refresh_update_archive() {
	local update_zip_name="kaku_for_update.zip"
	local update_zip_path="$OUT_DIR/$update_zip_name"
	local update_sha_path="$OUT_DIR/${update_zip_name}.sha256"

	echo ""
	echo "Refreshing auto-update archive with stapled app..."
	rm -f "$update_zip_path" "$update_sha_path"
	/usr/bin/ditto -c -k --sequesterRsrc --keepParent "$APP_BUNDLE" "$update_zip_path"
	(
		cd "$OUT_DIR"
		/usr/bin/shasum -a 256 "$update_zip_name" >"$(basename "$update_sha_path")"
	)
	echo "Update archive refreshed: $update_zip_path"
	echo "Update checksum refreshed: $update_sha_path"
}

if [[ "$STAPLE_ONLY" == "1" ]]; then
	echo "Stapling existing notarization ticket..."
	xcrun stapler staple "$APP_BUNDLE"
	[[ -f "$DMG_PATH" ]] && xcrun stapler staple "$DMG_PATH"
	refresh_update_archive
	echo "✅ Staple complete!"
	echo ""
	echo "Verifying notarization:"
	spctl -a -vv "$APP_BUNDLE" 2>&1 || true
	exit 0
fi

staple_and_verify() {
	xcrun stapler staple "$APP_BUNDLE"
	[[ -f "$DMG_PATH" ]] && xcrun stapler staple "$DMG_PATH"
	refresh_update_archive
	echo ""
	echo "✅ Done! App is notarized and ready for distribution."
	echo ""
	echo "Verifying notarization:"
	spctl -a -vv "$APP_BUNDLE" 2>&1 || true
}

# Preferred: rcodesign with App Store Connect API Key (avoids notarytool SIGBUS on macOS 26)
ASC_API_KEY_PATH="${KAKU_ASC_API_KEY_PATH:-}"
if [[ -z "$ASC_API_KEY_PATH" ]]; then
	ASC_API_KEY_PATH=$(security find-generic-password -s "kaku-asc-api-key-path" -w 2>/dev/null || true)
fi

if [[ -n "$ASC_API_KEY_PATH" && -f "$ASC_API_KEY_PATH" ]] && command -v rcodesign >/dev/null 2>&1; then
	echo "Submitting via rcodesign (App Store Connect API Key)..."
	echo "  Key: $ASC_API_KEY_PATH"
	echo "  File: $SUBMISSION_PATH"
	echo ""
	# Wait for Apple's verdict but let xcrun stapler do the stapling below.
	# rcodesign's own --staple refuses a DMG without an embedded signature,
	# which is what build.sh produces, so with --staple an accepted submission
	# still returned failure here and fell through to the notarytool path.
	#
	# The S3 upload stalls intermittently on proxied networks (the AWS SDK
	# ignores *_proxy and gives up at 0 B/s). Retry once on the same backend,
	# then stop for network diagnosis. Non-network failures may still use
	# the compatibility fallback.
	RCODESIGN_UPLOAD_ERROR_PATTERN="ThroughputBelowMinimum|connect timeout|dispatch failure|timed out"
	rcodesign_ok=0
	attempt=1
	while (( attempt <= NOTARY_SUBMIT_MAX_ATTEMPTS )); do
		echo "rcodesign attempt ${attempt}/${NOTARY_SUBMIT_MAX_ATTEMPTS}..."
		if RCODESIGN_OUTPUT=$(rcodesign notary-submit \
			--api-key-path "$ASC_API_KEY_PATH" \
			--wait \
			"$SUBMISSION_PATH" 2>&1); then
			echo "$RCODESIGN_OUTPUT"
			rcodesign_ok=1
			break
		fi
		echo "$RCODESIGN_OUTPUT"
		if ! grep -Eq "$RCODESIGN_UPLOAD_ERROR_PATTERN" <<<"$RCODESIGN_OUTPUT"; then
			break
		fi
		if (( attempt == NOTARY_SUBMIT_MAX_ATTEMPTS )); then
			echo "Error: rcodesign network failure persisted; stop and resolve the network before resuming." >&2
			exit 1
		fi
		if (( attempt < NOTARY_SUBMIT_MAX_ATTEMPTS )); then
			echo "rcodesign upload stalled. Retrying in ${NOTARY_SUBMIT_RETRY_DELAY}s..."
			sleep "$NOTARY_SUBMIT_RETRY_DELAY"
		fi
		attempt=$((attempt + 1))
	done
	if (( rcodesign_ok )); then
		echo ""
		echo "✅ Notarization accepted! Stapling ticket..."
		staple_and_verify
		exit 0
	fi
	echo "❌ rcodesign notarization failed. Falling back to notarytool if Apple ID credentials are available."
fi

# Fallback: notarytool with Keychain profile
NOTARYTOOL_PROFILE="${KAKU_NOTARYTOOL_PROFILE:-}"

if [[ -z "$NOTARYTOOL_PROFILE" ]]; then
	NOTARYTOOL_PROFILE=$(security find-generic-password -s "kaku-notarytool-profile" -w 2>/dev/null || true)
fi

if [[ -z "$NOTARYTOOL_PROFILE" ]]; then
	echo ""
	echo "Error: rcodesign notarization failed and no notarytool Keychain profile was found."
	echo ""
	echo "Preferred (rcodesign, avoids notarytool SIGBUS on macOS 26):"
	echo "  1. Create an API key at https://appstoreconnect.apple.com/access/integrations/api"
	echo "  2. rcodesign encode-app-store-connect-api-key -o asc_api_key.json <issuer-id> <key-id> AuthKey_*.p8"
	echo "  3. security add-generic-password -s 'kaku-asc-api-key-path' -a 'kaku' -w '/path/to/asc_api_key.json'"
	echo ""
	echo "Fallback (notarytool Keychain profile, with secure password prompt):"
	echo "  xcrun notarytool store-credentials kaku-notarytool --apple-id <apple-id> --team-id <team-id>"
	echo "  security add-generic-password -s 'kaku-notarytool-profile' -a 'kaku' -w 'kaku-notarytool'"
	exit 1
fi

NOTARYTOOL_AUTH_ARGS=(--keychain-profile "$NOTARYTOOL_PROFILE")

echo "Submitting via notarytool..."
echo "  Profile: $NOTARYTOOL_PROFILE"
echo "  File: $SUBMISSION_PATH"
echo ""
echo "Uploading to Apple notarization service (this may take a few minutes)..."

NOTARYTOOL_ARGS=(
	"$SUBMISSION_PATH"
	"${NOTARYTOOL_AUTH_ARGS[@]}"
	--wait
)

# Transport failures retry on the same backend and route, then stop.
NOTARYTOOL_UPLOAD_ERROR_PATTERN="abortedUpload|deadlineExceeded|ThroughputBelowMinimum|connectTimeout"
SUBMIT_OUTPUT=""
attempt=1
while (( attempt <= NOTARY_SUBMIT_MAX_ATTEMPTS )); do
	SUBMIT_ARGS=("${NOTARYTOOL_ARGS[@]}")
	echo "notarytool attempt ${attempt}/${NOTARY_SUBMIT_MAX_ATTEMPTS}..."

	if SUBMIT_OUTPUT=$(xcrun notarytool submit "${SUBMIT_ARGS[@]}" 2>&1); then
		break
	fi

	if ! grep -Eq "$NOTARYTOOL_UPLOAD_ERROR_PATTERN" <<<"$SUBMIT_OUTPUT"; then
		echo "Notarization submission failed:"
		echo "$SUBMIT_OUTPUT"
		exit 1
	fi

	if (( attempt == NOTARY_SUBMIT_MAX_ATTEMPTS )); then
		echo "Notarization network failure persisted after ${NOTARY_SUBMIT_MAX_ATTEMPTS} attempts; resolve the network before resuming:"
		echo "$SUBMIT_OUTPUT"
		exit 1
	fi

	echo "notarytool upload failed. Retrying in ${NOTARY_SUBMIT_RETRY_DELAY}s..."
	sleep "$NOTARY_SUBMIT_RETRY_DELAY"
	attempt=$((attempt + 1))
done

echo "$SUBMIT_OUTPUT"

if echo "$SUBMIT_OUTPUT" | grep -q "Accepted"; then
	echo ""
	echo "✅ Notarization accepted! Stapling ticket..."
	staple_and_verify
else
	echo ""
	echo "❌ Notarization failed."
	SUBMISSION_ID=$(echo "$SUBMIT_OUTPUT" | grep "id:" | head -1 | awk '{print $2}')
	if [[ -n "$SUBMISSION_ID" ]]; then
		echo "Fetching detailed log..."
		xcrun notarytool log "$SUBMISSION_ID" \
			"${NOTARYTOOL_AUTH_ARGS[@]}" 2>&1 || true
	fi
	exit 1
fi
