# Mux Agent Guide

The `mux` crate owns tab, pane, domain, and mux state management.

## Scope

`mux` is the central abstraction for:
- tabs and panes
- windows and workspaces
- local and remote domains
- client/server mux coordination

## Where to Look

- `mux/src/tab.rs`: tab tree and split behavior
- `mux/src/pane.rs`: pane state and interfaces
- `mux/src/window.rs`: window-level mux state
- `mux/src/domain.rs`: domain abstractions
- `mux/src/client.rs` and `crates/wezterm-mux-server-impl/src/` (`sessionhandler.rs`, `dispatch.rs`, `local.rs`): protocol flow

## Practical Rules

- Already encoded input uses `send_text`, without paste framing or newline conversion. Local panes share the terminal's existing `ThreadedWriter` with keys and paste, preserving order without blocking on a full PTY.
- A foreground process group can outlive its leader (`seq | less -X`). When the leader has no executable path, identify a live member of that same group from the cached shell tree. A foreground-group change, exec or leader exit invalidates the leader immediately and the fallback tree must be sampled after that leader update; checking only PGID misses a producer that exits after being cached. Do not wait for the normal TTL, substitute an unrelated background job, or scan all processes on every frame. Release the tree cache guard before calling Lua hooks that can query the pane again.
- Keep alert propagation reliable, including `Alert::SetUserVar`, because GUI reload and user-var hooks depend on this path.
- Preserve pane and overlay invalidation signals consumed by GUI AI/chat overlays when panes split, move, close, or resize.

## Known Pitfalls

- **Alert propagation**: `Alert::SetUserVar` must reach the GUI side reliably; broken propagation silently breaks config reload and user-var event hooks.
- **Pane ID stability**: Pane IDs must remain stable across mux round-trips; don't reassign or reuse IDs in ways that confuse client/server sync.
- **Domain abstraction leakage**: GUI-specific logic (rendering, window handles) must not appear in `mux`; only mux-level state and events.
- **Split tree correctness**: Tab split tree mutations in `tab.rs` are tricky; always verify with a pane layout sanity check after modifications.
- **Overlay resize regressions**: GUI overlays derive geometry from pane state. Mux changes that skip resize or removal notifications can leave AI/chat overlays stale.

## Cross-References

- [`kaku-gui/AGENTS.md`](../kaku-gui/AGENTS.md) - GUI rendering for mux state.
- [`lua-api-crates/AGENTS.md`](../lua-api-crates/AGENTS.md) - Lua APIs for mux control.
- [`term/AGENTS.md`](../term/AGENTS.md) - Terminal instances owned by panes.
