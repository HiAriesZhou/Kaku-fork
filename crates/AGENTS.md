# Crates Agent Guide

`crates/` contains shared utility crates used across the workspace. Inspect the target crate's `Cargo.toml` before editing because Kaku keeps some WezTerm-derived crates and adds its own helpers over time.

## Scope

Key groups:

| Group | Crates |
|-------|--------|
| Terminal core | `codec`, `color-types`, `vtparse`, `wezterm-cell`, `wezterm-escape-parser`, `wezterm-surface` |
| GUI and input | `wezterm-font`, `wezterm-input-types`, `wezterm-gui-subcommands`, `wezterm-open-url`, `wezterm-toast-notification` |
| Mux, client, and remote | `wezterm-client`, `wezterm-mux-server-impl`, `wezterm-ssh`, `wezterm-uds`, `kaku-relay`, `kaku-remote` |
| Platform and process | `pty`, `filedescriptor`, `procinfo`, `env-bootstrap`, `umask`, `async_ossl` |
| Lua and config support | `luahelper`, `wezterm-dynamic` |
| Shared utilities | `promise`, `base91`, `bidi`, `bintree`, `frecency`, `lfucache`, `rangeset`, `ratelim`, `tabout`, `wezterm-blob-leases`, `wezterm-char-props`, `wezterm-version` |
| Kaku-specific utilities | `kaku-ai-utils`, `kaku-unicode-names` (generated Unicode name table lifted out of `kaku-gui/src`; regenerate with `ucd-generate`, do not hand-edit) |

## Practical Rules

- For macOS notification actions that launch Kaku commands, prefer resolving bundled executables relative to the running `kaku-gui` binary.
- Keep `kaku-ai-utils` provider-agnostic; provider policy belongs in GUI/CLI call sites and config, not in low-level helpers.
- When shared cache helpers affect startup, verify the GUI consumer and avoid invalidating hot-path caches unnecessarily.

## Web Terminal and Relay

`kaku-remote` serves a browser terminal (`src/web.rs`, page in `assets/web/`) and `kaku-relay` lets it be reached from outside the LAN (`src/share.rs` on the Mac side). Both ship only with the `remote` cargo feature. Invariants:

- Not launched: the maintainer uses it privately until deciding to open it up. Release and Nightly builds leave the `remote` feature off (`scripts/nightly.sh` refuses it), `kaku remote` is hidden from `--help`, and no doc, README, menu, setting, release note or issue reply mentions it. Build it locally with `CARGO_FEATURES=remote ./scripts/build.sh`.
- One session per browser, carried either by the local WebSocket (`#t=<token>` link) or by the relay (`#k=<key>` link). `run_session` only sees two string channels; keep transport logic out of it.
- The page mirrors Kaku's own terminal model as ANSI frames rather than replaying the PTY stream, so terminal queries are answered once, by Kaku. Keys go through `Pane::key_down`, pastes through `send_paste`.
- Anything that reads the Lua config (for example `can_close_without_prompting`) must run on the GUI thread via `promise::spawn::spawn_into_main_thread`; calling it from the server's tokio threads panics.
- Relay ids are the first 128 bits of SHA-256 of a secret that never leaves the Mac, so a link holder cannot register as that Mac. Messages are AES-256-GCM sealed with the key from the link fragment, with direction labels so a frame cannot be reflected. The relay forwards bytes it cannot read and fetches the page's files from the Mac, so the page always matches the Kaku build.
- Each web session holds `awake::Awake`, a latency-critical `NSProcessInfo` activity. Without it, App Nap throttles the backgrounded GUI's main thread, which turns PTY output into the pane change the session renders from, and every echo waited 100-200 ms. Measure remote input latency with the Kaku window hidden, not focused.
- `Shown::update` turns a screen whose rows moved into `CSI n S` / `CSI n T` plus the new rows. That relies on each row starting with its own `CSI row;1H` and on the page never setting a scroll region; keep both, or the browser's copy drifts from Kaku's.
- Clipboard writes from programs (OSC 52, `MuxNotification::AssignClipboard`) also go to browsers viewing that pane (`web::publish_clipboard`, 1 MB cap); the Mac clipboard keeps getting them. Find (`Search`) runs on the Mac over the pane's history, so it reaches past what the page shows. Files a browser drops or pastes arrive in chunks, in a bounded window of eight 64 KB chunks, each acknowledged (`UploadAck`), so its percentage is what reached the Mac; a session that ends mid-file deletes the partial file (`Uploads` drop) and the page resends after reconnecting. Files are written to `~/Downloads/Kaku` under a sanitized, de-duplicated name (`safe_file_name`, `unused_path`, 50 MB cap), and their path is pasted into the original drop pane with `quote_dropped_files`, as a native drop would be.
- Scrolling a plain shell (`local_scroll` in each frame: no alternate screen, no mouse grab) never round-trips: the page fetches history rows with `History` in pages of `HISTORY_PAGE` and scrolls them in `#past`, laid exactly over the terminal with each run placed at Kaku's column. Programs that take the mouse or the alternate screen still get the scroll on the Mac, and Find still pins the Mac-side view (`View::pinned_top`).
- The page names tabs by `tab_id` and reorders them per `window_id`; rename and move go through `TabAction::Rename` / `TabAction::Move` in `kaku-gui/src/remote_tabs.rs`, which keep the Mac's active tab. A split tab lists its `panes`; viewing one sends `Activate`, which also focuses that split on the Mac, and `claim_tab_size` grows the tab by the split's share so the viewed pane gets the browser's grid.
- Web pane menus retain the selected `pane_id` through clipboard permission prompts and close dialogs. Splits call `Mux::split_pane` with that id rather than the deferred active-pane spawn path. Confirmed pane closes remove that exact pane, or record and remove its tab when it is last; `CloseCurrentPane { confirm: false }` is not sufficient under `AlwaysPrompt`. Middle-click tab close shares the existing browser confirmation path. Keep native phone selection menus intact.
- Web URL detection stops at CJK sentence punctuation and unmatched closing wrappers, retains balanced path parentheses, and maps UTF-16 regex offsets back to terminal cells, including surrogate pairs and wide end cells. The browser regression fixture includes the reported GitHub comment link followed by `)，并以`.
- A tab a browser sized keeps that size through relayouts the Mac does on its own (display or scale change, focus, config or theme reload, tab bar visibility): `apply_dimensions` asks `window_relayout_resizes_tab` per tab. Only the Mac's own use takes it back: typing or clicking (`reclaim_active_tab_size`) or a live window drag. Resizing it under the browser made Claude Code and Codex redraw for the Mac's rows and jump up in the page.
- The relay runs in `nrt` (Tokyo, `crates/kaku-relay/fly.toml`), the edge the Mac enters through: from `sin` every keystroke and echo crossed Tokyo to Singapore twice, 375 ms echo against 166 ms now. It keeps hosts in memory, so run one machine; two would split a Mac and its browsers across them.
- Only Mac-confirmed text is rendered. Cursor visibility cannot distinguish normal input from passwords, so never predict typed characters. Pages opt into a four-frame output window with `Hello.frame_ack`; ACK after xterm parses even an ignored pane frame, coalesce later dirty output, and leave legacy hello clients unthrottled.
- Browser selection copying follows the Mac's existing `copy_on_select` value and runs on selection release without a network hop. Preserve manual copy when disabled, reconstruct history line breaks and virtual gaps, and never queue a denied automatic copy for a later unrelated gesture. Browser clipboard permission and real mobile selection remain browser-specific.
- History keeps at most 10,000 row strings but materializes only visible rows and overscan, preserving stable DOM nodes that intersect an active selection. Cached tab attachment must precede resize, and an equal-size claim must still record browser ownership.
- Upload transfer ids bind the file and original pane for ten minutes after completion, across transport reconnects within the same app process. Retry returns the receipt without writing or pasting twice; cancellation and disconnect remove only partial files. Freeze the target once for a multi-file drop.
- Every relay connect is capped by `CONNECT_TIMEOUT`: a local proxy (Stash TUN) can accept TCP and never finish the handshake, which once left the link offline for hours. Bound socket writes and session inbox waits too, but allow short input bursts to wait rather than discard keys. Wrong-key close 4403 is final; idle unauthenticated sessions expire with 4408. A host replacement closes browsers independently of the old data queue, with late input discarded until the close writer finishes.
- The private relay uses `r.kaku.fun` with `/{id}/` links; `/t/{id}/` routes remain valid for already shared links. Never shorten the 128-bit identity or put the encryption key in a URL path/query. After an authenticated host response, the browser remembers a relay key in localStorage and removes the fragment; tab storage carries unverified links and legacy sessions, and LAN tokens stay tab-only. Rejection clears both stores. If storage is blocked, retain the complete link so reloads still work. Explain bookmarking in the CLI, first-connection toast and existing browser help, not public docs.
- One link per Kaku, persisted at `DATA_DIR/remote-share.json` (0600). `kaku remote --reset` rotates it; the running bridge notices within seconds and re-registers, which retires the old link.

## Remote Verification

- For an authorized relay deployment, run `fly deploy --remote-only --depot=false --ha=false --strategy immediate` from `crates/kaku-relay` when Depot stalls at builder authentication. Verify the Fly tunnel with `fly doctor` first; do not add `--wg=false` to a working tunnel, since the legacy builder's HTTPS path can fail with an h2c upgrade HTTP 500. This selects a build transport, not a system proxy setting. Verify the deployed image digest and an isolated encrypted session afterward; a dashboard without new releases does not diagnose a failure before image creation. If CLI DNS checks warn while HTTPS works, compare public DNS before changing anything.
- Run `cargo test --locked -p kaku-remote`, relay tests via its own manifest, and workspace check/tests/clippy with `kaku/remote,kaku-gui/remote` enabled. CI enables the feature only for tests and clippy, never for shipped packages.
- `node --test crates/kaku-remote/tests/web.test.cjs` uses Playwright Chromium to exercise the shipped HTML and xterm, including input privacy, reconnect, upload pacing, history selection, and viewport boundaries. Set `NODE_PATH` when Playwright is installed outside the checkout; `PLAYWRIGHT_CHROMIUM_EXECUTABLE` optionally selects an existing browser.
- After a private `make app` with `CARGO_FEATURES=remote` and a debug relay build, run `node crates/kaku-remote/tests/live.cjs`. It launches only its own app, temporary HOME, and loopback relay, tests encrypted input, uploads across reconnect, frame flow control, host replacement, stable split targets and AlwaysPrompt pane closing, then removes its own fixtures. Never substitute a real share link.

## Cross-References

- [`term/AGENTS.md`](../term/AGENTS.md) - Uses codec, vtparse, and color types.
- [`kaku-gui/AGENTS.md`](../kaku-gui/AGENTS.md) - Uses font, input, and surface crates.
- [`lua-api-crates/AGENTS.md`](../lua-api-crates/AGENTS.md) - Uses Lua helper and dynamic binding crates.
