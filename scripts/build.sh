#!/usr/bin/env bash
set -euo pipefail

if [[ "${OSTYPE:-}" != darwin* ]]; then
	echo "This script is macOS-only." >&2
	exit 1
fi

# Keep vendored native deps on the same minimum macOS target as Rust.
export MACOSX_DEPLOYMENT_TARGET="${MACOSX_DEPLOYMENT_TARGET:-11.0}"
export CMAKE_OSX_DEPLOYMENT_TARGET="${CMAKE_OSX_DEPLOYMENT_TARGET:-11.0}"

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO_ROOT"

APP_NAME="Kaku"
TARGET_DIR="${TARGET_DIR:-target}"
PROFILE="${PROFILE:-release}"
OUT_DIR="${OUT_DIR:-dist}"
OPEN_APP="${OPEN_APP:-0}"
APP_ONLY="${APP_ONLY:-0}"
BUILD_ARCH="${BUILD_ARCH:-}"
KAKU_REQUIRE_SIGNED_RELEASE="${KAKU_REQUIRE_SIGNED_RELEASE:-0}"

if [[ -z "$BUILD_ARCH" ]]; then
	if [[ "$PROFILE" == "release" || "$PROFILE" == "release-opt" ]]; then
		BUILD_ARCH="universal"
	else
		BUILD_ARCH="native"
	fi
fi

resolve_native_target() {
	case "$(uname -m)" in
	arm64 | aarch64)
		echo "aarch64-apple-darwin"
		;;
	x86_64)
		echo "x86_64-apple-darwin"
		;;
	*)
		echo "Unsupported macOS architecture: $(uname -m)" >&2
		exit 1
		;;
	esac
}

resolve_build_targets() {
	case "$BUILD_ARCH" in
	universal)
		echo "aarch64-apple-darwin x86_64-apple-darwin"
		;;
	native)
		resolve_native_target
		;;
	arm64)
		echo "aarch64-apple-darwin"
		;;
	x86_64)
		echo "x86_64-apple-darwin"
		;;
	*)
		echo "Unsupported BUILD_ARCH=$BUILD_ARCH (expected: universal, native, arm64, x86_64)" >&2
		exit 1
		;;
	esac
}

require_command() {
	if ! command -v "$1" >/dev/null 2>&1; then
		echo "Missing required command: $1" >&2
		echo "Install the Rust toolchain bootstrap first, then retry." >&2
		echo "See CONTRIBUTING.md for setup instructions." >&2
		exit 1
	fi
}

is_developer_id_application_identity() {
	[[ "$1" == Developer\ ID\ Application:* ]]
}

detect_signing_identity() {
	local identities count

	if [[ -n "${KAKU_SIGNING_IDENTITY:-}" ]]; then
		if ! is_developer_id_application_identity "$KAKU_SIGNING_IDENTITY"; then
			echo "Warning: KAKU_SIGNING_IDENTITY is not a Developer ID Application certificate: $KAKU_SIGNING_IDENTITY" >&2
			echo "Notarization requires Developer ID Application signing." >&2
			return 1
		fi
		return 0
	fi

	identities=$(security find-identity -v -p codesigning 2>/dev/null | awk -F '"' '/Developer ID Application/{print $2}' || true)
	count=$(printf '%s\n' "$identities" | grep -c '^Developer ID Application:' || true)

	if [[ "$count" -ge 1 ]]; then
		KAKU_SIGNING_IDENTITY=$(printf '%s\n' "$identities" | head -n1)
		export KAKU_SIGNING_IDENTITY
		if [[ "$count" -gt 1 ]]; then
			echo "Release build: found multiple Developer ID Application certificates, auto-selecting: $KAKU_SIGNING_IDENTITY"
		else
			echo "Release build: auto-detected signing identity: $KAKU_SIGNING_IDENTITY"
		fi
		return 0
	fi

	echo "Warning: no Developer ID Application certificate found in Keychain." >&2
	return 1
}

# Apple's timestamp service fails in two wordings: "timestamp service is not
# available" and "A timestamp was expected but was not found." Both mean the
# TSA round trip failed, not that the certificate is wrong, so both retry.
CODESIGN_TIMESTAMP_ERROR_PATTERN="timestamp service is not available|A timestamp was expected but was not found"

codesign_with_retry() {
	local max_attempts=2
	local delay_seconds=15
	local attempt=1
	local output rc

	while (( attempt <= max_attempts )); do
		# Capture the status on the command itself. Reading $? after a failed
		# `if` yields 0, which made this helper return success after a failed
		# signing and let the build package an unsigned app.
		rc=0
		output=$(codesign "$@" 2>&1) || rc=$?
		if (( rc == 0 )); then
			if [[ -n "$output" ]]; then
				printf '%s\n' "$output"
			fi
			return 0
		fi

		printf '%s\n' "$output" >&2

		if ! grep -Eq "$CODESIGN_TIMESTAMP_ERROR_PATTERN" <<<"$output"; then
			return "$rc"
		fi

		if (( attempt == max_attempts )); then
			echo "codesign failed after $max_attempts attempts because the Apple timestamp service remained unavailable." >&2
			return "$rc"
		fi

		echo "codesign timestamp service unavailable, retrying in ${delay_seconds}s (attempt ${attempt}/${max_attempts})..." >&2
		sleep "$delay_seconds"
		attempt=$((attempt + 1))
	done
}

ensure_rust_targets() {
	local installed
	local missing=()

	installed="$(rustup target list --installed)"
	for target in "$@"; do
		if ! grep -Fxq "$target" <<<"$installed"; then
			missing+=("$target")
		fi
	done

	if [[ ${#missing[@]} -gt 0 ]]; then
		echo "Installing missing Rust targets: ${missing[*]}"
		rustup target add "${missing[@]}"
	fi
}

for arg in "$@"; do
	case "$arg" in
	--open) OPEN_APP=1 ;;
	--app-only) APP_ONLY=1 ;;
	--native-arch) BUILD_ARCH="native" ;;
	esac
done

require_command cargo
require_command rustup

APP_BUNDLE_SRC="assets/macos/Kaku.app"
APP_BUNDLE_OUT="$OUT_DIR/$APP_NAME.app"

echo "[1/7] Building binaries ($PROFILE, $BUILD_ARCH)..."
PROFILE_DIR="debug"
CARGO_PROFILE_ARGS=()
if [[ "$PROFILE" == "release" ]]; then
	CARGO_PROFILE_ARGS=(--release)
	PROFILE_DIR="release"
elif [[ "$PROFILE" == "release-opt" ]]; then
	CARGO_PROFILE_ARGS=(--profile release-opt)
	PROFILE_DIR="release-opt"
fi

# Optional cargo features, comma-separated, e.g. `CARGO_FEATURES=kaku-gui/desktop`.
CARGO_FEATURE_ARGS=()
if [[ -n "${CARGO_FEATURES:-}" ]]; then
	CARGO_FEATURE_ARGS=(--features "$CARGO_FEATURES")
	echo "  Extra cargo features: $CARGO_FEATURES"
fi

if ! BUILD_TARGETS_STR="$(resolve_build_targets)"; then
	exit 1
fi

BUILD_TARGETS=()
IFS=' ' read -r -a BUILD_TARGETS <<<"$BUILD_TARGETS_STR"
if [[ ${#BUILD_TARGETS[@]} -eq 0 ]]; then
	echo "No build targets resolved for BUILD_ARCH=$BUILD_ARCH" >&2
	exit 1
fi

# A private desktop build can carry its helper instead of depending on a
# separately installed copy. Public builds never copy it implicitly.
if [[ -n "${KAKU_DESKTOP_HELPER:-}" ]]; then
	helper_features="${CARGO_FEATURES:-}"
	case ",${helper_features// /,}," in
	*,desktop,* | *,kaku-gui/desktop,*) ;;
	*) echo "KAKU_DESKTOP_HELPER requires CARGO_FEATURES=kaku-gui/desktop" >&2; exit 1 ;;
	esac
	if [[ ! -f "$KAKU_DESKTOP_HELPER" || ! -x "$KAKU_DESKTOP_HELPER" ]]; then
		echo "KAKU_DESKTOP_HELPER must name an executable helper" >&2
		exit 1
	fi
	for target in "${BUILD_TARGETS[@]}"; do
		case "$target" in
		aarch64-apple-darwin) helper_arch=arm64 ;;
		x86_64-apple-darwin) helper_arch=x86_64 ;;
		esac
		if ! lipo -verify_arch "$helper_arch" "$KAKU_DESKTOP_HELPER"; then
			echo "Desktop helper does not contain requested architecture: $helper_arch" >&2
			exit 1
		fi
	done
fi

ensure_rust_targets "${BUILD_TARGETS[@]}"

# One cargo invocation for every target, so one architecture keeps compiling
# while the other sits in its single-threaded fat-LTO link.
CARGO_TARGET_ARGS=()
for target in "${BUILD_TARGETS[@]}"; do
	CARGO_TARGET_ARGS+=(--target "$target")
done
echo "Building targets: ${BUILD_TARGETS[*]}"
./scripts/prune_target.sh "$TARGET_DIR"
CARGO_TERM_PROGRESS_WHEN=auto cargo build --locked ${CARGO_PROFILE_ARGS[@]+"${CARGO_PROFILE_ARGS[@]}"} ${CARGO_FEATURE_ARGS[@]+"${CARGO_FEATURE_ARGS[@]}"} "${CARGO_TARGET_ARGS[@]}" --target-dir "$TARGET_DIR" -p kaku-gui -p kaku

if [[ "$BUILD_ARCH" == "universal" ]]; then
	BIN_DIR="$TARGET_DIR/universal/$PROFILE_DIR"
	mkdir -p "$BIN_DIR"
	for bin in kaku kaku-gui k; do
		lipo -create \
			-output "$BIN_DIR/$bin" \
			"$TARGET_DIR/aarch64-apple-darwin/$PROFILE_DIR/$bin" \
			"$TARGET_DIR/x86_64-apple-darwin/$PROFILE_DIR/$bin"
		chmod +x "$BIN_DIR/$bin"
	done
else
	BIN_DIR="$TARGET_DIR/${BUILD_TARGETS[0]}/$PROFILE_DIR"
fi

for bin in kaku kaku-gui k; do
	echo -n "Built $bin: "
	lipo -info "$BIN_DIR/$bin"
done

echo "[2/7] Preparing app bundle..."
rm -rf "$APP_BUNDLE_OUT"
mkdir -p "$OUT_DIR"
cp -R "$APP_BUNDLE_SRC" "$APP_BUNDLE_OUT"

mkdir -p "$APP_BUNDLE_OUT/Contents/MacOS"
mkdir -p "$APP_BUNDLE_OUT/Contents/Resources"

echo "[2.5/7] Syncing version from Cargo.toml..."
# Extract version from kaku/Cargo.toml (assuming it's the source of truth)
VERSION=$(grep '^version =' kaku/Cargo.toml | head -n 1 | cut -d '"' -f2)
if [[ -n "$VERSION" ]]; then
	echo "Stamping version $VERSION into Info.plist"
	/usr/libexec/PlistBuddy -c "Set :CFBundleShortVersionString $VERSION" "$APP_BUNDLE_OUT/Contents/Info.plist"
	/usr/libexec/PlistBuddy -c "Set :CFBundleVersion $VERSION" "$APP_BUNDLE_OUT/Contents/Info.plist"
else
	echo "Warning: Could not detect version from kaku/Cargo.toml"
fi

echo "[3/7] Downloading vendor plugins..."
./scripts/download_vendor.sh

echo "[4/7] Copying resources and binaries..."
cp -R assets/shell-integration/* "$APP_BUNDLE_OUT/Contents/Resources/"
cp -R assets/shell-completion "$APP_BUNDLE_OUT/Contents/Resources/"
cp -R assets/fonts "$APP_BUNDLE_OUT/Contents/Resources/"
cp -R assets/prompts "$APP_BUNDLE_OUT/Contents/Resources/"
mkdir -p "$APP_BUNDLE_OUT/Contents/Resources/vendor"
for vendor_item in starship.toml fast-syntax-highlighting zsh-autosuggestions zsh-completions zsh-z; do
	src_path="assets/vendor/$vendor_item"
	if [[ -e "$src_path" ]]; then
		cp -R "$src_path" "$APP_BUNDLE_OUT/Contents/Resources/vendor/"
	else
		echo "Warning: missing vendor item: $src_path"
	fi
done
cp assets/shell-integration/first_run.sh "$APP_BUNDLE_OUT/Contents/Resources/"
chmod +x "$APP_BUNDLE_OUT/Contents/Resources/first_run.sh"

# Explicitly use the logo.icns from assets if available
if [[ -f "assets/logo.icns" ]]; then
	cp "assets/logo.icns" "$APP_BUNDLE_OUT/Contents/Resources/terminal.icns"
fi

if ! tic -xe kaku -o "$APP_BUNDLE_OUT/Contents/Resources/terminfo" termwiz/data/kaku.terminfo; then
	echo "Warning: 'tic -xe' failed (some ncurses/tic variants). Falling back to full compile mode."
	tic -x -o "$APP_BUNDLE_OUT/Contents/Resources/terminfo" termwiz/data/kaku.terminfo
fi

for bin in kaku kaku-gui k; do
	cp "$BIN_DIR/$bin" "$APP_BUNDLE_OUT/Contents/MacOS/$bin"
	chmod +x "$APP_BUNDLE_OUT/Contents/MacOS/$bin"
done

if [[ -n "${KAKU_DESKTOP_HELPER:-}" ]]; then
	cp "$KAKU_DESKTOP_HELPER" "$APP_BUNDLE_OUT/Contents/MacOS/kaku-desktop"
	chmod +x "$APP_BUNDLE_OUT/Contents/MacOS/kaku-desktop"
fi

# Clean up xattrs to prevent icon caching issues or quarantine
xattr -cr "$APP_BUNDLE_OUT"

echo "[5/7] Signing app bundle..."
# Signing strategy:
# - Dev builds (PROFILE=dev): Always use ad-hoc signing (-) for speed
# - Release builds (PROFILE=release/release-opt): Use KAKU_SIGNING_IDENTITY or auto-detect a Developer ID Application certificate
# Usage with developer certificate:
#   KAKU_SIGNING_IDENTITY="Developer ID Application: Your Name (TEAMID)" ./scripts/build.sh
if [[ "$KAKU_REQUIRE_SIGNED_RELEASE" == "1" && ( "$PROFILE" == "dev" || "$PROFILE" == "debug" ) ]]; then
	echo "Error: signed release requires PROFILE=release or PROFILE=release-opt, got PROFILE=$PROFILE" >&2
	exit 1
fi

if [[ "$PROFILE" == "dev" || "$PROFILE" == "debug" ]]; then
	SIGNING_IDENTITY="-"
	echo "Dev build: using ad-hoc signing"
else
	if detect_signing_identity; then
		SIGNING_IDENTITY="$KAKU_SIGNING_IDENTITY"
		echo "Release build: signing with developer certificate"
	else
		if [[ "$KAKU_REQUIRE_SIGNED_RELEASE" == "1" ]]; then
			echo "Error: release build requires a Developer ID Application certificate. Set KAKU_SIGNING_IDENTITY or install one in Keychain." >&2
			exit 1
		fi
		SIGNING_IDENTITY="-"
		echo "Release build: using ad-hoc signing (set KAKU_SIGNING_IDENTITY or install a single Developer ID Application certificate for notarization)"
	fi
fi

BASE_SIGN_ARGS=(
	--force
	--sign "$SIGNING_IDENTITY"
)

RUNTIME_SIGN_ARGS=(
	"${BASE_SIGN_ARGS[@]}"
	--options runtime
	--entitlements assets/macos/Kaku.entitlements
)

if [[ "$SIGNING_IDENTITY" == "-" ]]; then
	BUNDLE_ID="$(/usr/libexec/PlistBuddy -c "Print :CFBundleIdentifier" "$APP_BUNDLE_OUT/Contents/Info.plist" 2>/dev/null || true)"
	if [[ -n "$BUNDLE_ID" ]]; then
		# Keep designated requirement stable across local ad-hoc builds so macOS TCC
		# does not treat each rebuilt app as a brand-new identity.
		echo "Ad-hoc signing with stable designated requirement: $BUNDLE_ID"
	else
		echo "Warning: CFBundleIdentifier not found. Falling back to default ad-hoc requirement."
	fi
fi

touch "$APP_BUNDLE_OUT/Contents/Resources/terminal.icns"
touch "$APP_BUNDLE_OUT/Contents/Info.plist"
touch "$APP_BUNDLE_OUT"

# kaku-gui is CFBundleExecutable. When codesign signs the main executable it
# validates the other executables in Contents/MacOS as nested code and refuses
# while any of them is still unsigned ("code object is not signed at all,
# In subcomponent: .../k"), so the helpers must be signed before it.
SIGN_BINARIES=(kaku k)
if [[ -n "${KAKU_DESKTOP_HELPER:-}" ]]; then
	SIGN_BINARIES+=(kaku-desktop)
fi
SIGN_BINARIES+=(kaku-gui)
for bin in "${SIGN_BINARIES[@]}"; do
	codesign_with_retry "${RUNTIME_SIGN_ARGS[@]}" "$APP_BUNDLE_OUT/Contents/MacOS/$bin"
done

APP_SIGN_ARGS=("${RUNTIME_SIGN_ARGS[@]}")
if [[ "$SIGNING_IDENTITY" == "-" && -n "${BUNDLE_ID:-}" ]]; then
	APP_SIGN_ARGS+=("-r=designated => identifier \"$BUNDLE_ID\"")
fi

codesign_with_retry "${APP_SIGN_ARGS[@]}" "$APP_BUNDLE_OUT"

if [[ "$APP_ONLY" == "1" ]]; then
	echo "App bundle ready: $APP_BUNDLE_OUT"
	if [[ "$OPEN_APP" == "1" ]]; then open "$APP_BUNDLE_OUT"; fi
	exit 0
fi

UPDATE_ZIP_NAME="kaku_for_update.zip"
UPDATE_ZIP_PATH="$OUT_DIR/$UPDATE_ZIP_NAME"
UPDATE_SHA_PATH="$OUT_DIR/${UPDATE_ZIP_NAME}.sha256"

echo "[6/7] Creating auto-update archive..."
rm -f "$UPDATE_ZIP_PATH" "$UPDATE_SHA_PATH"
/usr/bin/ditto -c -k --sequesterRsrc --keepParent "$APP_BUNDLE_OUT" "$UPDATE_ZIP_PATH"
(
	cd "$OUT_DIR"
	/usr/bin/shasum -a 256 "$UPDATE_ZIP_NAME" >"$(basename "$UPDATE_SHA_PATH")"
)
echo "Update archive created: $UPDATE_ZIP_PATH"
echo "Update checksum created: $UPDATE_SHA_PATH"

echo "[7/7] Creating DMG..."
DMG_NAME="$APP_NAME.dmg"
DMG_PATH="$OUT_DIR/$DMG_NAME"
DMG_BASE_PATH="$OUT_DIR/$APP_NAME"
TEMP_DMG_PATH="$OUT_DIR/${APP_NAME}-temp.dmg"
STAGING_DIR="$OUT_DIR/dmg_staging"
BACKGROUND_IMAGE_SOURCE="assets/macos/dmg/background.png"
BACKGROUND_IMAGE_NAME="background.png"

hdiutil_cmd() {
	LC_ALL=C LANG=en_US.UTF-8 hdiutil "$@"
}

cleanup_volumes() {
	local vol_pattern="/Volumes/$APP_NAME"
	local max_attempts=15
	local attempt=1

	while [ $attempt -le $max_attempts ]; do
		if hdiutil_cmd info | grep -q "$vol_pattern"; then
			echo "Detaching existing volumes (Attempt $attempt/$max_attempts)..."
			hdiutil_cmd info | grep "$vol_pattern" | awk '{print $1}' | while read -r dev; do
				echo "Force detaching $dev..."
				hdiutil_cmd detach "$dev" -force || true
			done
			sleep 1
		else
			if [ -d "$vol_pattern" ]; then
				echo "Removing stale mount point directory $vol_pattern..."
				rmdir "$vol_pattern" || true
			fi
			return 0
		fi
		attempt=$((attempt + 1))
	done
	echo "Warning: Failed to fully detach volumes after $max_attempts attempts."
}

configure_dmg_layout() {
	local disk_name="$1"
	local app_name="$2"
	local background_name="$3"

	# Finder learns about a freshly attached volume a moment after hdiutil
	# returns; asking for the disk too early fails with -1728 and left a
	# Nightly DMG without its window layout. Wait for Finder to see it.
	osascript >/dev/null <<EOF
tell application "Finder"
	repeat 40 times
		if exists disk "${disk_name}" then exit repeat
		delay 0.25
	end repeat
	tell disk "${disk_name}"
		open
		set current view of container window to icon view
		set toolbar visible of container window to false
		set statusbar visible of container window to false
		set the bounds of container window to {100, 100, 780, 520}
		set viewOptions to the icon view options of container window
		set arrangement of viewOptions to not arranged
		set icon size of viewOptions to 120
		set text size of viewOptions to 14
		try
			set background picture of viewOptions to file ".background:${background_name}"
		end try
		set position of item "${app_name}.app" of container window to {190, 245}
		set position of item "Applications" of container window to {500, 245}
		close
		open
		update without registering applications
		delay 1
	end tell
end tell
EOF
}

cleanup_volumes

/bin/sync

rm -rf "$DMG_PATH" "$TEMP_DMG_PATH" "$STAGING_DIR" "$DMG_BASE_PATH.dmg"
mkdir -p "$STAGING_DIR"

cp -R "$APP_BUNDLE_OUT" "$STAGING_DIR/"
ln -s /Applications "$STAGING_DIR/Applications"

if [[ -f "$BACKGROUND_IMAGE_SOURCE" ]]; then
	mkdir -p "$STAGING_DIR/.background"
	cp "$BACKGROUND_IMAGE_SOURCE" "$STAGING_DIR/.background/$BACKGROUND_IMAGE_NAME"
else
	echo "Warning: DMG background image not found at $BACKGROUND_IMAGE_SOURCE; using default Finder background."
fi

mdutil -i off "$STAGING_DIR" >/dev/null 2>&1 || true

echo "Creating DMG..."
MAX_RETRIES=3
RETRY_COUNT=0

while [ $RETRY_COUNT -lt $MAX_RETRIES ]; do
	if ! hdiutil_cmd create -quiet -volname "$APP_NAME" \
		-srcfolder "$STAGING_DIR" \
		-ov -format UDRW \
		"$TEMP_DMG_PATH"; then
		echo "hdiutil create failed. Retrying in 2 seconds... ($((RETRY_COUNT + 1))/$MAX_RETRIES)"
		cleanup_volumes
		sleep 2
		RETRY_COUNT=$((RETRY_COUNT + 1))
		continue
	fi

	ATTACH_OUTPUT=$(hdiutil_cmd attach -readwrite -noverify -noautoopen "$TEMP_DMG_PATH" 2>/dev/null || true)
	DEVICE=$(echo "$ATTACH_OUTPUT" | awk '/\/dev\// {print $1; exit}')
	MOUNT_POINT=$(echo "$ATTACH_OUTPUT" | awk -F'\t' '/\/Volumes\// {print $NF; exit}')
	MOUNT_NAME=$(basename "$MOUNT_POINT")

	if [[ -z "$DEVICE" || -z "$MOUNT_POINT" ]]; then
		echo "Failed to attach temporary DMG. Retrying..."
		if [[ -n "${DEVICE:-}" ]]; then
			hdiutil_cmd detach "$DEVICE" -force >/dev/null 2>&1 || true
		fi
		cleanup_volumes
		sleep 2
		RETRY_COUNT=$((RETRY_COUNT + 1))
		continue
	fi

	LAYOUT_OK=0
	if configure_dmg_layout "$MOUNT_NAME" "$APP_NAME" "$BACKGROUND_IMAGE_NAME"; then
		# Finder writes .DS_Store a moment after the script returns.
		for _ in $(seq 1 20); do
			[[ -f "$MOUNT_POINT/.DS_Store" ]] && LAYOUT_OK=1 && break
			sleep 0.25
		done
	fi
	if [[ "$LAYOUT_OK" != "1" ]]; then
		# A published DMG without its layout opens as a bare file list, so a
		# release-grade build stops here instead of shipping one.
		if [[ "$KAKU_REQUIRE_SIGNED_RELEASE" == "1" ]]; then
			hdiutil_cmd detach "$DEVICE" -force >/dev/null 2>&1 || true
			echo "Error: Failed to configure Finder layout for DMG."
			exit 1
		fi
		echo "Warning: Failed to configure Finder layout for DMG."
	fi

		/bin/sync
	hdiutil_cmd detach "$DEVICE" -force >/dev/null 2>&1 || true

	if hdiutil_cmd convert -quiet "$TEMP_DMG_PATH" \
		-format UDZO \
		-imagekey zlib-level=9 \
		-ov -o "$DMG_BASE_PATH"; then
		break
	else
		echo "hdiutil convert failed. Retrying in 2 seconds... ($((RETRY_COUNT + 1))/$MAX_RETRIES)"
		cleanup_volumes
		sleep 2
		RETRY_COUNT=$((RETRY_COUNT + 1))
	fi
done

if [ ! -f "$DMG_PATH" ]; then
	echo "Error: Failed to create DMG after retries."
	exit 1
fi

rm -rf "$STAGING_DIR" "$TEMP_DMG_PATH"

echo "DMG created: $DMG_PATH"

echo "Done: $APP_BUNDLE_OUT"
if [[ "$OPEN_APP" == "1" ]]; then
	echo "Opening app..."
	open "$APP_BUNDLE_OUT"
fi
