#!/usr/bin/env bash

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../../.." && pwd)"
source "$SCRIPT_DIR/common.sh"

# kaku.zsh rebuilds the completion dump only when it is older than a day and
# loads it with `compinit -C` otherwise. The age test is a `(#q...)` glob
# qualifier, which zsh only honors with EXTENDED_GLOB; without it the test was
# always true and every shell paid for a full compinit.

tmp_dir="$(mktemp -d "${TMPDIR:-/tmp}/kaku-compinit-cache.XXXXXX")"
cleanup() {
  rm -rf "$tmp_dir"
}
trap cleanup EXIT

HOME="$tmp_dir/home"
ZDOTDIR="$HOME"
mkdir -p "$HOME"

vendor_dir="$tmp_dir/vendor"
create_stub_vendor_dir "$vendor_dir"

setup_out=""
setup_status=0
setup_out="$(
  HOME="$HOME" \
  ZDOTDIR="$ZDOTDIR" \
  KAKU_INIT_INTERNAL=1 \
  KAKU_SKIP_TOOL_BOOTSTRAP=1 \
  KAKU_SKIP_TERMINFO_BOOTSTRAP=1 \
  KAKU_VENDOR_DIR="$vendor_dir" \
  bash "$REPO_ROOT/assets/shell-integration/setup_zsh.sh" --update-only 2>&1
)" || setup_status=$?
if [[ "$setup_status" -ne 0 ]]; then
  echo "compinit_cache: setup_zsh.sh failed (exit $setup_status):" >&2
  echo "$setup_out" >&2
  exit 1
fi

# Print the arguments kaku.zsh passes to compinit, and whether it leaked
# EXTENDED_GLOB into the user's shell.
compinit_call() {
  TERM=xterm-256color HOME="$HOME" ZDOTDIR="$ZDOTDIR" zsh -f -c '
compinit() { print -r -- "__KAKU_COMPINIT__:[$*]" }
source "$HOME/.config/kaku/zsh/kaku.zsh"
[[ -o extendedglob ]] && print -r -- "__KAKU_EXTGLOB__:on" || print -r -- "__KAKU_EXTGLOB__:off"
' 2>&1
}

expect() {
  local label="$1" out="$2" want="$3"
  case "$out" in
    *"$want"*) ;;
    *)
      echo "compinit_cache: $label: expected $want in:" >&2
      echo "$out" >&2
      exit 1
      ;;
  esac
}

dump="$ZDOTDIR/.zcompdump"

# A fresh dump is loaded from cache.
touch "$dump"
out="$(compinit_call)"
expect "fresh dump" "$out" "__KAKU_COMPINIT__:[-C]"
expect "fresh dump" "$out" "__KAKU_EXTGLOB__:off"

# A dump older than a day is rebuilt.
touch -t 202001010000 "$dump"
out="$(compinit_call)"
expect "stale dump" "$out" "__KAKU_COMPINIT__:[]"

# No dump yet: compinit -C writes one, the same as the original intent.
rm -f "$dump"
out="$(compinit_call)"
expect "missing dump" "$out" "__KAKU_COMPINIT__:[-C]"

echo "compinit_cache smoke test passed"
