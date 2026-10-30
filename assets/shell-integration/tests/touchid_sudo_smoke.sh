#!/usr/bin/env bash
# Smoke tests for configure_touchid in setup_zsh.sh, run against the real
# function with /etc/pam.d redirected to a temporary directory and sudo
# stubbed. When macOS refuses the write (the user clicks Don't Allow on the
# "administer your computer" prompt), setup must say Touch ID was not enabled
# instead of reporting success.

set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
SETUP="$ROOT_DIR/assets/shell-integration/setup_zsh.sh"

fail() {
  echo "FAIL: $*" >&2
  exit 1
}

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
PAM_DIR="$TMP/pam.d"

# Load the shipped function with its /etc/pam.d paths pointed at PAM_DIR.
func="$(sed -n '/^configure_touchid() {$/,/^}$/p' "$SETUP")"
[[ -n "$func" ]] || fail "configure_touchid not found in setup_zsh.sh"
eval "${func//\/etc\/pam.d/$PAM_DIR}"

BOLD='' GREEN='' YELLOW='' NC=''
bioutil() { :; }

# A sudo that succeeds, dropping the root ownership a test user cannot set.
sudo_ok() {
  case "$1" in
  chown) return 0 ;;
  install)
    shift
    local args=()
    while (($#)); do
      case "$1" in
      -o | -g) shift 2 ;;
      *)
        args+=("$1")
        shift
        ;;
      esac
    done
    command install "${args[@]}"
    ;;
  *) "$@" ;;
  esac
}

reset_pam() {
  rm -rf "$PAM_DIR"
  mkdir -p "$PAM_DIR"
  printf '%s\n' "# sudo: auth account password session" "$1" "auth       required       pam_opendirectory.so" >"$PAM_DIR/sudo"
}

# 1. macOS refuses the write: no success line, no file.
reset_pam "auth       include        sudo_local"
sudo() { return 1; }
out="$(printf '\n' | configure_touchid 2>&1)"
grep -q "Touch ID not enabled" <<<"$out" || fail "refused write did not report failure: $out"
grep -q "Enabled via" <<<"$out" && fail "refused write still reported success: $out"
[[ ! -e "$PAM_DIR/sudo_local" ]] || fail "refused write left a sudo_local behind"

# 2. Sonoma and later: sudo_local gets the pam_tid line.
reset_pam "auth       include        sudo_local"
sudo() { sudo_ok "$@"; }
out="$(printf '\n' | configure_touchid 2>&1)"
grep -q "Enabled via $PAM_DIR/sudo_local" <<<"$out" || fail "sudo_local path did not report success: $out"
grep -qx "auth       sufficient     pam_tid.so" "$PAM_DIR/sudo_local" || fail "sudo_local is missing pam_tid"

# 3. Before Sonoma: pam_tid goes into line 2 of the sudo file itself.
reset_pam "auth       sufficient     pam_smartcard.so"
out="$(printf '\n' | configure_touchid 2>&1)"
grep -q "Enabled via $PAM_DIR/sudo" <<<"$out" || fail "legacy path did not report success: $out"
[[ "$(sed -n 2p "$PAM_DIR/sudo")" == "auth       sufficient     pam_tid.so" ]] || fail "legacy path did not insert pam_tid on line 2"

# 4. Answering n leaves everything untouched.
reset_pam "auth       include        sudo_local"
out="$(printf 'n' | configure_touchid 2>&1)"
[[ ! -e "$PAM_DIR/sudo_local" ]] || fail "declining still wrote sudo_local"

echo "touchid_sudo_smoke: ok"
