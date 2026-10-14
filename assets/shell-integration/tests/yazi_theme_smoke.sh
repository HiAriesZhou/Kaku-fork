#!/usr/bin/env bash

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../../.." && pwd)"
source "$SCRIPT_DIR/common.sh"

# Run inside Kaku, these point at the real config, whose theme then decides
# the flavor instead of the test fixtures.
unset KAKU_CONFIG_FILE KAKU_CONFIG_DIR

fail() {
	echo "yazi_theme: $*" >&2
	exit 1
}

assert_file_contains() {
	local needle="$1"
	local file="$2"
	if ! grep -Fq "$needle" "$file"; then
		echo "yazi_theme: expected $file to contain:" >&2
		echo "  $needle" >&2
		echo "yazi_theme: actual file content:" >&2
		sed -n '1,160p' "$file" >&2
		fail "missing expected content"
	fi
}

assert_file_not_contains() {
	local needle="$1"
	local file="$2"
	if grep -Fq "$needle" "$file"; then
		echo "yazi_theme: expected $file to omit:" >&2
		echo "  $needle" >&2
		echo "yazi_theme: actual file content:" >&2
		sed -n '1,160p' "$file" >&2
		fail "unexpected content"
	fi
}

tmp_dir="$(mktemp -d "${TMPDIR:-/tmp}/kaku-yazi-theme.XXXXXX")"
cleanup() {
	rm -rf "$tmp_dir"
}
trap cleanup EXIT

create_stub_vendor_dir "$tmp_dir/vendor"

run_setup() {
	local home_dir="$1"
	HOME="$home_dir" \
	ZDOTDIR="$home_dir" \
	KAKU_INIT_INTERNAL=1 \
	KAKU_SKIP_TOOL_BOOTSTRAP=1 \
	KAKU_SKIP_TERMINFO_BOOTSTRAP=1 \
	KAKU_VENDOR_DIR="$tmp_dir/vendor" \
	bash "$REPO_ROOT/assets/shell-integration/setup_zsh.sh" --update-only >/dev/null
}

expected_auto_flavor() {
	if command -v defaults >/dev/null 2>&1; then
		local appearance
		appearance="$(defaults read -g AppleInterfaceStyle 2>/dev/null || true)"
		if [[ "$appearance" != "Dark" ]]; then
			printf '%s\n' "kaku-light"
			return
		fi
	fi

	printf '%s\n' "kaku-dark"
}

expected_flavor="$(expected_auto_flavor)"

home_new="$tmp_dir/home-new"
mkdir -p "$home_new"
run_setup "$home_new"

theme_new="$home_new/.config/yazi/theme.toml"
dark_new="$home_new/.config/yazi/flavors/kaku-dark.yazi/flavor.toml"
light_new="$home_new/.config/yazi/flavors/kaku-light.yazi/flavor.toml"
wrapper_new="$home_new/.config/kaku/zsh/bin/yazi"

[[ -f "$theme_new" ]]
[[ -f "$dark_new" ]]
[[ -f "$light_new" ]]
[[ -x "$wrapper_new" ]]
assert_file_contains '[flavor]' "$theme_new"
assert_file_contains "dark = \"$expected_flavor\"" "$theme_new"
assert_file_contains "light = \"$expected_flavor\"" "$theme_new"
assert_file_contains '{ url = "*/", fg = "#90c9e6" }' "$dark_new"
assert_file_contains '{ url = "*/", fg = "#205ea6" }' "$light_new"

home_legacy="$tmp_dir/home-legacy"
mkdir -p "$home_legacy/.config/yazi"
cat <<'EOF' >"$home_legacy/.config/yazi/theme.toml"
[mgr]
border_symbol = "│"
border_style = { fg = "#555555" }

[indicator]
padding = { open = "", close = "" }
EOF

run_setup "$home_legacy"

theme_legacy="$home_legacy/.config/yazi/theme.toml"
assert_file_contains '[flavor]' "$theme_legacy"
assert_file_not_contains 'border_style = { fg = "#555555" }' "$theme_legacy"

home_static="$tmp_dir/home-static"
mkdir -p "$home_static/.config/yazi"
cat <<'EOF' >"$home_static/.config/yazi/theme.toml"
# Kaku-aligned theme for Yazi 26.x

[app]
overall = { bg = "#15141b" }

[mgr]
cwd = { fg = "#a277ff", bold = true }
border_symbol = "│"
border_style = { fg = "#2b2838" }

[indicator]
current = { fg = "#a277ff", bg = "#2a233f", bold = true }
padding = { open = "", close = "" }

[mode]
normal_main = { fg = "#a277ff", bg = "#2a233f", bold = true }
EOF

run_setup "$home_static"

theme_static="$home_static/.config/yazi/theme.toml"
assert_file_contains '[flavor]' "$theme_static"
assert_file_not_contains '# Kaku-aligned theme for Yazi 26.x' "$theme_static"
assert_file_not_contains 'overall = { bg = "#15141b" }' "$theme_static"
assert_file_contains "dark = \"$expected_flavor\"" "$theme_static"
assert_file_contains "light = \"$expected_flavor\"" "$theme_static"

home_auto="$tmp_dir/home-auto"
mkdir -p "$home_auto/.config/kaku"
cat <<'EOF' >"$home_auto/.config/kaku/kaku.lua"
local wezterm = require 'wezterm'
local config = wezterm.config_builder()
config.color_scheme = 'Auto'
return config
EOF

run_setup "$home_auto"

theme_auto="$home_auto/.config/yazi/theme.toml"
auto_flavor="$(expected_auto_flavor)"
assert_file_contains "dark = \"$auto_flavor\"" "$theme_auto"
assert_file_contains "light = \"$auto_flavor\"" "$theme_auto"

home_light="$tmp_dir/home-light"
mkdir -p "$home_light/.config/kaku"
cat <<'EOF' >"$home_light/.config/kaku/kaku.lua"
local wezterm = require 'wezterm'
local config = wezterm.config_builder()
config.color_scheme = 'Kaku Light'
return config
EOF

run_setup "$home_light"

mkdir -p "$tmp_dir/realbin"
cat <<'EOF' >"$tmp_dir/realbin/yazi"
#!/usr/bin/env bash
echo "real-yazi $*"
EOF
chmod +x "$tmp_dir/realbin/yazi"

wrapper_light="$home_light/.config/kaku/zsh/bin/yazi"
theme_light="$home_light/.config/yazi/theme.toml"
output="$(
  HOME="$home_light" \
  PATH="$home_light/.config/kaku/zsh/bin:$tmp_dir/realbin:$PATH" \
  "$wrapper_light" --version
)"
[[ "$output" == "real-yazi --version" ]]
assert_file_contains 'dark = "kaku-light"' "$theme_light"
assert_file_contains 'light = "kaku-light"' "$theme_light"

# Fresh configs must use the #:schema comment yazi supports, never a $schema key
# (yazi >= 26.5.6 rejects the key and fails to start, see issue #457).
keymap_new="$home_new/.config/yazi/keymap.toml"
assert_file_contains '#:schema https://yazi-rs.github.io/schemas/theme.json' "$theme_new"
assert_file_not_contains '$schema' "$theme_new"
assert_file_contains '#:schema https://yazi-rs.github.io/schemas/keymap.json' "$keymap_new"
assert_file_not_contains '$schema' "$keymap_new"

# Setup migrates $schema keys written by older Kaku versions, quoted or bare,
# and keeps the user's own content.
home_schema="$tmp_dir/home-schema"
mkdir -p "$home_schema/.config/yazi"
cat <<'EOF' >"$home_schema/.config/yazi/theme.toml"
"$schema" = "https://yazi-rs.github.io/schemas/theme.json"

[mode]
normal_main = { fg = "#a277ff" }
EOF
cat <<'EOF' >"$home_schema/.config/yazi/keymap.toml"
$schema = "https://yazi-rs.github.io/schemas/keymap.json"

[mgr]
prepend_keymap = []
EOF

run_setup "$home_schema"

theme_schema="$home_schema/.config/yazi/theme.toml"
keymap_schema="$home_schema/.config/yazi/keymap.toml"
assert_file_contains '#:schema https://yazi-rs.github.io/schemas/theme.json' "$theme_schema"
assert_file_not_contains '$schema' "$theme_schema"
assert_file_contains 'normal_main = { fg = "#a277ff" }' "$theme_schema"
assert_file_contains '[flavor]' "$theme_schema"
assert_file_contains '#:schema https://yazi-rs.github.io/schemas/keymap.json' "$keymap_schema"
assert_file_not_contains '$schema' "$keymap_schema"
assert_file_contains 'prepend_keymap = []' "$keymap_schema"

# The installed wrapper self-heals $schema keys before every yazi launch.
cat <<'EOF' >"$home_light/.config/yazi/keymap.toml"
"$schema" = "https://yazi-rs.github.io/schemas/keymap.json"

[mgr]
prepend_keymap = []
EOF
output="$(
  HOME="$home_light" \
  PATH="$home_light/.config/kaku/zsh/bin:$tmp_dir/realbin:$PATH" \
  "$wrapper_light" --version
)"
[[ "$output" == "real-yazi --version" ]]
assert_file_contains '#:schema https://yazi-rs.github.io/schemas/keymap.json' "$home_light/.config/yazi/keymap.toml"
assert_file_not_contains '$schema' "$home_light/.config/yazi/keymap.toml"

# When a real yazi is installed, gate the generated configs through its parser.
real_yazi=""
for candidate in /opt/homebrew/bin/yazi /usr/local/bin/yazi /opt/local/bin/yazi; do
	if [[ -x "$candidate" ]]; then
		real_yazi="$candidate"
		break
	fi
done
if [[ -n "$real_yazi" ]]; then
	YAZI_CONFIG_HOME="$home_new/.config/yazi" "$real_yazi" --version >/dev/null 2>&1 </dev/null \
		|| fail "generated yazi config rejected by $real_yazi"
	YAZI_CONFIG_HOME="$home_schema/.config/yazi" "$real_yazi" --version >/dev/null 2>&1 </dev/null \
		|| fail "migrated yazi config rejected by $real_yazi"
fi

echo "yazi_theme smoke test passed"
