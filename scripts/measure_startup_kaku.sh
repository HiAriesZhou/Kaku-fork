#!/usr/bin/env bash
# Isolated process launches; preparation and cleanup are not timed.
# Cold clears the fixture's app cache, not OS/font caches. Polling is 10ms.
set -euo pipefail
RUNS="${RUNS:-5}"
WARMUP="${WARMUP:-2}"
WAIT_TIMEOUT_SEC="${WAIT_TIMEOUT_SEC:-15}"
APP_PATH="${APP_PATH:-dist/Kaku.app}"
for command in hyperfine jq; do
  command -v "$command" >/dev/null || { echo "Missing $command" >&2; exit 1; }
done
[[ "$RUNS" =~ ^[1-9][0-9]*$ && "$WARMUP" =~ ^[0-9]+$ && "$WAIT_TIMEOUT_SEC" =~ ^[1-9][0-9]*$ ]] || exit 2
APP_PATH="$(cd "$APP_PATH" && pwd)"
GUI="$APP_PATH/Contents/MacOS/kaku-gui"
[[ -x "$GUI" ]] || { echo "Build the app with make app first" >&2; exit 1; }
BENCH_DIR=$(mktemp -d "${TMPDIR:-/tmp}/kaku-startup.XXXXXX")
cleanup_run() {
  if [[ -s "$BENCH_DIR/pid" ]]; then
    local pid
    pid=$(cat "$BENCH_DIR/pid")
    kill -TERM "$pid" 2>/dev/null || true
    for _ in {1..100}; do
      kill -0 "$pid" 2>/dev/null || break
      sleep 0.01
    done
    if kill -0 "$pid" 2>/dev/null; then kill -KILL "$pid" 2>/dev/null || true; fi
    rm -f "$BENCH_DIR/pid"
  fi
}
cleanup_all() { cleanup_run; rm -rf "$BENCH_DIR"; }
trap cleanup_all EXIT
mkdir -p "$BENCH_DIR/config" "$BENCH_DIR/data" "$BENCH_DIR/cache" "$BENCH_DIR/zsh"
cat > "$BENCH_DIR/zsh/.zshrc" <<'ZSH'
PROMPT='benchmark> '
precmd() { printf 'ready\n' > "$KAKU_BENCH_PROMPT"; }
ZSH
cat > "$BENCH_DIR/kaku.lua" <<'LUA'
local config = dofile(os.getenv('KAKU_BENCH_RESOURCES') .. '/kaku.lua')
config.restore_previous_session = false
config.check_for_updates = false
config.quit_when_all_windows_are_closed = true
config.initial_cols = 80
config.initial_rows = 24
config.set_environment_variables = config.set_environment_variables or {}
config.set_environment_variables.ZDOTDIR = os.getenv('KAKU_BENCH_ZDOTDIR')
config.set_environment_variables.KAKU_BENCH_PROMPT = os.getenv('KAKU_BENCH_PROMPT')
return config
LUA
prepare_run() {
  cleanup_run
  rm -f "$BENCH_DIR/prompt" "$BENCH_DIR/trace"
  if [[ "$1" == cold ]]; then
    rm -rf "$BENCH_DIR/cache"
    mkdir -p "$BENCH_DIR/cache"
  fi
}
launch_and_wait() {
  local target="$1" deadline=$((SECONDS + WAIT_TIMEOUT_SEC))
  XDG_CONFIG_HOME="$BENCH_DIR/config" XDG_DATA_HOME="$BENCH_DIR/data" \
    XDG_CACHE_HOME="$BENCH_DIR/cache" KAKU_STARTUP_TRACE=1 \
    KAKU_BENCH_RESOURCES="$APP_PATH/Contents/Resources" \
    KAKU_BENCH_ZDOTDIR="$BENCH_DIR/zsh" KAKU_BENCH_PROMPT="$BENCH_DIR/prompt" \
    "$GUI" --config-file "$BENCH_DIR/kaku.lua" start --always-new-process \
    -- /bin/zsh -d -i > "$BENCH_DIR/trace" 2>&1 &
  local pid=$!
  printf '%s\n' "$pid" > "$BENCH_DIR/pid"
  while kill -0 "$pid" 2>/dev/null; do
    if [[ "$target" == window ]]; then
      grep -qE '^\[startup\].* window.show\(\) done$' "$BENCH_DIR/trace" && return 0
    elif [[ -s "$BENCH_DIR/prompt" ]] && grep -qE '^\[startup\].* first paint_impl done$' "$BENCH_DIR/trace"; then
      return 0
    fi
    (( SECONDS < deadline )) || break
    sleep 0.01
  done
  echo "Startup did not reach $target within ${WAIT_TIMEOUT_SEC}s" >&2
  return 1
}
export BENCH_DIR GUI APP_PATH WAIT_TIMEOUT_SEC
export -f cleanup_run prepare_run launch_and_wait
for cache in cold warm; do
  for target in window ready; do
    hyperfine --warmup "$WARMUP" --runs "$RUNS" --shell bash \
      --prepare "prepare_run $cache" --cleanup cleanup_run \
      --export-json "$BENCH_DIR/$cache-$target.json" \
      --command-name "$cache-$target" "launch_and_wait $target" >&2
  done
done
jq -n --arg version "$("$GUI" --version)" --argjson runs "$RUNS" \
  --slurpfile cw "$BENCH_DIR/cold-window.json" --slurpfile ww "$BENCH_DIR/warm-window.json" \
  --slurpfile cr "$BENCH_DIR/cold-ready.json" --slurpfile wr "$BENCH_DIR/warm-ready.json" '
  # hyperfine 2 moved wall-clock times under summary and measurements.
  def wall: if .summary then {mean:.summary.time_wall_clock.mean,
    median:.summary.time_wall_clock.median, times:[.measurements[].time_wall_clock.value]}
    else {mean, median, times} end;
  def metric($r): $r.results[0] | wall | {mean_ms:(.mean*1000),
    p50_ms:(.median*1000), p95_ms:(.times | sort | .[(([length*0.95|ceil,1]|max)-1)] * 1000)};
  {schema_version:2, measurement:"launch_to_prompt_and_first_paint", version:$version, runs:$runs,
   cold_start_ms:($cr[0].results[0] | wall | .mean*1000|round),
   warm_start_ms:($wr[0].results[0] | wall | .mean*1000|round),
   cold_window:metric($cw[0]), warm_window:metric($ww[0]),
   cold_ready:metric($cr[0]), warm_ready:metric($wr[0]),
   cache_policy:"cold resets fixture app cache; warm reuses it; OS and font caches are retained",
   observation:"10ms polling; first paint is render completion, not compositor presentation"}'
