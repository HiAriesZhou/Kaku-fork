# Terminal Agent Guide

## Scope

`term/` contains terminal emulation core behavior, including screen state, escape sequence handling, selection, and PTY-facing terminal semantics.

## Where to Look

- `term/src/terminal.rs` - main terminal state and event handling
- `term/src/terminalstate/` - detailed state transitions
- `term/src/screen.rs` - screen buffer behavior
- `term/src/test/` - terminal-core regression tests (`selection.rs`, `csi.rs`, `c0.rs`, `c1.rs`, `core_emulation.rs`)
- `crates/wezterm-escape-parser/` - escape sequence parsing (CSI/OSC/ESC/APC); `term` consumes it, it does not live under `term/src/`
- `kaku-gui/src/selection.rs` and `kaku-gui/src/termwindow/selection.rs` - selection and copy behavior (GUI side, not `term`)

## Practical Rules

- Keep GUI assumptions out of terminal core.
- Color scheme reporting uses pane-local DEC mode 2031 and DSR 996/997. Notify subscribers after effective configuration palette changes, including same-mode palette changes, and answer subsequent OSC color queries from the new palette. Reloads with an unchanged effective palette and application-issued OSC color writes do not notify; cancellation, RIS and DECSTR clear the subscription. `color::is_light_color` is shared with config/window chrome so the reported scheme stays consistent with Kaku's colors.
- Keep normal-screen and alternate-screen behavior distinct; wheel scrolling and inline AI status must not corrupt terminal state.
- Preserve selection and cursor invariants that GUI overlays and inline assistant status depend on.
- Only the terminal wrap attribute proves that physical rows form one logical line. Full-width output followed by text in column zero can still be independent commands. Keep both indented and unindented hard-newline regressions, alongside soft-wrap coverage, when changing hyperlink grouping (#547).
- For TUI re-render corruption, reduce the report to an ANSI transcript and cover cursor motion plus erasure in terminal-core tests before changing GUI code. Full-line erase of a physical row must also leave wrap state consistent, especially for styled prompts that previously wrapped.

## Verification

- Compile checks: run `make check`.
- Terminal logic changes: run `make test` or the narrow cargo nextest filter for the affected behavior.
- Cross-crate behavior: also inspect `mux/AGENTS.md` and `kaku-gui/AGENTS.md`.

## Cross-References

- `../mux/AGENTS.md` - owns panes and terminal instances.
- `../kaku-gui/AGENTS.md` - renders terminal surfaces.
- `../termwiz/AGENTS.md` - terminal UI primitives.
