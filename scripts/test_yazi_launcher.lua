-- Run with: luajit scripts/test_yazi_launcher.lua [bundled-config-path]
-- The Rust test `bundled_tool_launchers_type_only_into_shells` runs it in CI.
-- Execute the actual launchers with fake panes/windows; never type into a real
-- terminal or run the theme wrapper against the user's configuration.
local path = arg and arg[1] or 'assets/macos/Kaku.app/Contents/Resources/kaku.lua'
assert(loadfile(path)) -- Also guard the bundled chunk's local-variable limit.
local file = assert(io.open(path))
local source = file:read('*a')
file:close()
local first = source:find('-- Cmd+Shift+G and Cmd+Shift+Y type', 1, true)
  or assert(source:find('local function launch_lazygit(window, pane)', 1, true))
local last = assert(source:find('\nlocal function evict_stale_cache', first, true))
local chunk = source:sub(first, last - 1)

local function fixture()
  local state = { launches = {}, sent = {}, toasts = {}, themes = 0, activations = {}, layouts = 0 }
  local env = setmetatable({}, { __index = _G })
  env.basename = function(p) return p:match('([^/]+)$') end
  env.resolve_active_pane = function(_, pane) return pane end
  env.resolve_yazi_command = function() return state.yazi end
  env.resolve_lazygit_command = function() return state.lazygit end
  env.resolve_remote_target_from_pane = function(pane) return pane.remote end
  env.pane_cwd = function(pane) return pane.cwd or '' end
  env.pane_foreground_process_name = function(pane) return pane.fg or '' end
  env.is_shell_foreground = function(pane)
    local name = (env.basename((pane.fg or ''):lower()) or ''):gsub('^%-', '')
    return name == 'zsh' or name == 'fish' or name == 'bash'
  end
  env.sync_managed_yazi_theme = function() state.themes = state.themes + 1 end
  env.update_window_config = function() state.layouts = state.layouts + 1 end
  env.show_yazi_toast = function(_, _, event) table.insert(state.toasts, event) end
  env.show_lazygit_toast = function(_, _, event) table.insert(state.toasts, event) end
  env.set_yazi_mode_hint = function() end -- Lets the pre-#602 code run to its dispatch.
  env.wezterm = { action = {
    SendString = function(text) return { send = text } end,
  } }
  state.yazi = '/local/home/.config/kaku/zsh/bin/yazi'
  state.lazygit = '/opt/homebrew/bin/lazygit'
  local loaded = assert(load(chunk .. '\nreturn launch_yazi, launch_lazygit, tool_launcher',
    'tool launchers', 't', env))
  local launch_yazi, launch_lazygit, launcher = loaded()
  local function tab(id)
    return {
      tab_id = function() return id end,
      activate = function() table.insert(state.activations, id) end,
    }
  end
  local function window(id, source_id)
    local origin = tab(source_id)
    local mux = { live = { origin } }
    function mux:window_id() return id end
    function mux:tabs() return self.live end
    function mux:spawn_tab(spec)
      if state.fail_spawn then error('spawn failed') end
      table.insert(state.launches, spec)
      local child = tab(100 + #state.launches)
      table.insert(self.live, child)
      return child
    end
    return {
      mux_window = function() return mux end,
      active_tab = function() return origin end,
      get_dimensions = function() return { is_full_screen = false } end,
      perform_action = function(_, action, pane)
        assert(action.send, 'only typed input is dispatched as an action')
        table.insert(state.sent, { text = action.send, pane = pane })
      end,
    }, mux, origin
  end
  return state, launch_yazi, launch_lazygit, launcher, window
end

-- A shell prompt keeps the typed `y` wrapper, so quitting Yazi syncs the cwd.
do
  local state, launch_yazi, launch_lazygit, _, window = fixture()
  local win = window(1, 10)
  local pane = { fg = '/bin/zsh', cwd = '/tmp/project' }
  launch_yazi(win, pane)
  assert(#state.launches == 0, 'a shell pane must not open a tab')
  assert(state.sent[1].text == '\x15y 2>/dev/null || ' .. state.yazi .. '\r')
  assert(state.sent[1].pane == pane and state.themes == 1 and state.layouts == 1)
  launch_lazygit(win, { fg = '-fish' })
  assert(state.sent[2].text == '\x15' .. state.lazygit .. '\r')
  assert(#state.launches == 0 and #state.toasts == 0)
end

-- ssh, tmux and Herdr hand typed input on to the shell they hold.
do
  local state, launch_yazi, launch_lazygit, _, window = fixture()
  local win = window(1, 10)
  launch_yazi(win, { fg = 'ssh', remote = 'user@server', cwd = '/remote' })
  assert(state.sent[1].text == '\x15y 2>/dev/null || yazi\r', 'no local path for a remote shell')
  assert(state.themes == 0)
  for _, fg in ipairs({ '/opt/homebrew/bin/tmux', 'herdr' }) do
    launch_yazi(win, { fg = fg, cwd = '/tmp/project' })
    launch_lazygit(win, { fg = fg, cwd = '/tmp/project' })
  end
  assert(#state.sent == 5 and #state.launches == 0)
end

-- Codex (or any other program) owns its input: open a tab, type nothing.
do
  local state, launch_yazi, _, launcher, window = fixture()
  local win, mux, origin = window(1, 10)
  local pane = { fg = 'codex', cwd = '/tmp/project with spaces/中文' }
  launch_yazi(win, pane)
  assert(#state.sent == 0, 'must not type into Codex (#602)')
  local spec = assert(state.launches[1], 'must open Yazi in a new tab from a Codex pane')
  assert(spec.cwd == pane.cwd and spec.domain == 'DefaultDomain')
  assert(#spec.args == 1 and spec.args[1] == state.yazi)
  assert(state.themes == 1 and state.layouts == 0 and #state.toasts == 0)
  assert(state.activations[1] == 101)
  launcher.restore(win)
  assert(#state.activations == 1, 'do not return while Yazi is still open')
  mux.live = { origin }
  launcher.restore(win)
  assert(state.activations[2] == 10)
  assert(next(launcher.tabs) == nil)
  launcher.restore(win)
  assert(#state.activations == 2, 'return only once')
end

do
  local state, _, launch_lazygit, _, window = fixture()
  local win = window(1, 10)
  launch_lazygit(win, { fg = 'nvim', cwd = '/tmp/repo' })
  assert(#state.sent == 0, 'must not type into vim (#602)')
  local spec = assert(state.launches[1])
  assert(spec.cwd == '/tmp/repo' and spec.args[1] == state.lazygit)
  launch_lazygit(win, {}) -- Unknown foreground and cwd: a tab without a cwd.
  assert(state.launches[2].cwd == nil and #state.sent == 0)
end

-- Another window must not discard the return target; a closed source is skipped.
do
  local state, launch_yazi, _, launcher, window = fixture()
  local win, mux, origin = window(1, 10)
  local other = window(2, 20)
  launch_yazi(win, { fg = 'claude', cwd = '/tmp/project' })
  launcher.restore(other)
  assert(next(launcher.tabs), 'another window must not discard the return target')
  mux.live = { origin }
  launcher.restore(win)
  assert(state.activations[2] == 10)
  launch_yazi(win, { fg = 'claude', cwd = '/tmp/project' })
  mux.live = {}
  launcher.restore(win)
  assert(#state.activations == 3 and next(launcher.tabs) == nil)
end

do
  local state, launch_yazi, launch_lazygit, launcher, window = fixture()
  local win = window(1, 10)
  launch_yazi(win, nil)
  assert(state.toasts[1] == 'kaku-toast-yazi-no-pane')
  state.yazi = nil
  launch_yazi(win, { fg = 'codex' })
  assert(state.toasts[2] == 'kaku-toast-yazi-missing')
  state.yazi = '/missing/yazi'
  state.lazygit = '/missing/lazygit'
  state.fail_spawn = true
  launch_yazi(win, { fg = 'codex' })
  assert(state.toasts[3] == 'kaku-toast-yazi-dispatch-failed')
  launch_lazygit(win, { fg = 'codex' })
  assert(state.toasts[4] == 'kaku-toast-lazygit-dispatch-failed')
  assert(next(launcher.tabs) == nil and #state.sent == 0)
end

print('Tool launcher regression tests passed')
