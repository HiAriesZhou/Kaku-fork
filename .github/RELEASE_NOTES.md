# V0.23.1 <emoji TBD>

<div align="center">
  <img src="https://raw.githubusercontent.com/tw93/Kaku/main/assets/logo.png" alt="Kaku Logo" width="120" height="120" />
  <h1 style="margin: 12px 0 6px;">Kaku V0.23.1</h1>
  <p><em>A fast, out-of-the-box terminal built for AI coding.</em></p>
</div>

### Changelog

<!--
Auto-generated commit summary below. Rewrite as numbered prose items
(each one starts with **Topic Name**: short description). Then duplicate the
prose to "### 更新日志" in Chinese and delete this comment + the bulleted
commit list before publishing.
-->

**Features (9)**
- feat(remote): turn remote access on and off from kaku remote (aaec823d)
- feat(remote): serve the web terminal page from its own origin (e64dfd4b)
- feat(remote): gate relay registration on an access code, limit per address (f726dcd4)
- feat(relay): check signed registration tokens, off until required (bcab7ea6)
- feat(relay): serve the web terminal page from the relay's own copy (9cfa7425)
- feat(remote): notify the Mac when a paired browser connects after a while away (3ec4c5eb)
- feat(remote): Cmd+click a path in the web terminal to download that file (16f3c33f)
- feat(remote): report the relay link and paired browsers in kaku doctor (b04056d3)
- feat(remote): pair each browser instead of sharing one long-lived link key (1a3f7f7e)

**Fixes (40)**
- fix(session): #579 preserve checkpoints through restore and TERM (d409aa52)
- fix(tabs): #584 cancel stale drag state before a new gesture (a1c374b1)
- fix(render): #583 skip process trees for non-Kaku foreground apps (c13f5101)
- fix(shell): #582 preserve linked config on write failure (76a0d0a9)
- fix(remote): rebuild the WebGL glyph atlas on tab switches and returns (eba54f87)
- fix(remote): draw Claude Code's status symbols as text, not emoji (f0bcee2f)
- fix(remote): draw dim text the way the Mac window does (5a1f3ab3)
- fix(remote): click like a hand does for programs that take the mouse (7fb42f84)
- fix(remote): recover optimistic tab closes after reconnect (6178bf27)
- fix(scripts): reopen Kaku without a process or session snapshot (ced87edf)
- fix(relay): enforce required tokens and atomic host limits (1acb0e46)
- fix(config): protect every remote config override (b86183e1)
- fix(doctor): redact committed input method text (48420347)
- fix(relay): let sockets the Mac never admitted go first when a host is full (0b81fa4d)
- fix(remote): judge access codes by the attempt that used them, confirm off (28ee8613)
- fix(remote): keep a revoked device revoked while it keeps signing in (fc5ad123)
- fix(relay): cap frame size, keep redirects on site, tell old pages to reload (2e1f38bc)
- fix(remote): recognise a paired device by its whole key, not its short id (438a2c69)
- fix(doctor): keep typed keys out of the diagnostics bundle (a2ae9ad3)
- fix(deps): bump rustls to 0.23.46 for today's advisories (2cec35c1)
- fix(shell): show the cursor again at the prompt after a TUI hides it (e293bcb8)
- fix(remote): end a clicked path at Chinese sentence punctuation (986eec3f)
- fix(cli): keep symlinked config files linked when Kaku writes them (d5b2b6a3)
- fix(update): save the session before an in-app update replaces Kaku (abf6e0a8)
- fix(shell): write through symlinked dotfiles instead of replacing them (ba091e74)
- fix(doctor): redact URL credentials and fine-grained GitHub tokens (18c7de0f)
- fix(ai): keep Gemini's other model settings when saving the model (d886bdeb)
- fix(ai): refuse to save over an assistant.toml that does not parse (75d0a1f0)
- fix(init): run only the bundled setup script in release builds (4a7b8234)
- fix(ai): bypass the system proxy for private IPv6 model hosts (838ec787)
- fix(session): keep a window from Restore Previous Window saveable (7f79c1d8)
- fix(session): keep plain scrollback under the sidecar size cap (25abf8b6)
- fix(remote): hand the web grid to the DOM at once when WebGL loses its context (ba4e9af6)
- fix: stop release network failures after one retry (076e491d)
- fix(relay): pass the expired pairing link close code to the page (1ea97a43)
- fix(scripts): read startup times from hyperfine 2's json (a4009eb3)
- fix(relay): bound queued bytes per browser and relay-wide, cap at 100 sockets (30bc31ed)
- fix(scripts): restart every running kaku and let only the snapshot owner save (ca1d1141)
- fix(macos): hotkey drawer appears at once past a stacked display instead of fading (a949d98a)
- fix(macos): hotkey drawer fades past a stacked display and keeps its dragged height (2daf1d97)

**Performance (6)**
- perf(remote): close a tab in the page on the click (da3fdd2e)
- perf(remote): send a browser-opened tab's first frame with the tab list (28331bc0)
- perf(remote): show a cached tab in the next frame, without a blank one (3c943b4e)
- perf(remote): send the tab list when tabs change, not on the next idle tick (6f42ac41)
- perf(remote): send a frame as soon as output follows a quiet spell (4aeef2d7)
- perf(remote): draw the web terminal with webgl and blink the cursor only when idle (162313d6)

### 更新日志

<!-- TODO: translate the prose Changelog above into Chinese, same numbered structure. -->

Special thanks to <contributors> for their contributions to this release.

> https://github.com/tw93/Kaku
