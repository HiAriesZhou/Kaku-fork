# V0.26.0 Bridge

<div align="center">
  <img src="https://raw.githubusercontent.com/tw93/Kaku/main/assets/logo.png" alt="Kaku Logo" width="120" height="120" />
  <h1 style="margin: 12px 0 6px;">Kaku V0.26.0</h1>
  <p><em>A fast, out-of-the-box terminal built for AI coding.</em></p>
</div>

### Changelog

1. **Remote Page**: The web page draws with your Mac's fonts, shows the agent title, previews an image path when you hover it, and picks up a new version on its own after reconnecting.
2. **Remote Typing**: Typing stays ahead of file transfers, emoji take two cells as they do in Kaku, splits keep the Mac's layout, and the page redraws itself if it falls out of step.
3. **Remote Access**: Only one Kaku serves remote access at a time, so a second copy no longer takes the page over, and notices say what to do when the Mac is offline.
4. **Lower CPU**: Programs that keep updating the title, such as Claude Code and Codex, no longer redraw the whole window each time.
5. **Dead Keys**: A dead key such as `~` followed by a key it cannot combine with now types both characters, as Terminal.app does.
6. **Character Picker**: The emoji and Nerd Font picker is back in the command palette, found by searching emoji.
7. **Fixes**: Frame lines are quieter on dark themes, SSH tabs show their number when tab numbers are on, and AI summaries keep file names whole.

### 更新日志

1. **远程网页**：网页使用 Mac 上的字体，显示 Agent 标题，悬停图片路径即可预览，重连后自动换上新版本。
2. **远程输入**：传文件时输入依然优先，emoji 与 Kaku 一样占两格，分屏保持 Mac 上的布局，画面错位时会自动重绘。
3. **远程访问**：同一时间只有一个 Kaku 提供远程访问，另开的副本不再接管网页，Mac 离线时提示会说明该怎么做。
4. **更低的 CPU 占用**：Claude Code、Codex 这类持续更新标题的程序不再每次重绘整个窗口。
5. **死键**：`~` 这类死键后接无法组合的按键时，两个字符都会输入，与 Terminal.app 一致。
6. **字符选择器**：emoji 和 Nerd Font 选择器回到命令面板，搜索 emoji 即可打开。
7. **修复**：深色主题下的边框线更柔和，开启标签编号后 SSH 标签也显示编号，AI 摘要里的文件名保持完整。

Special thanks to @SimonRen for their contribution to this release.

> https://github.com/tw93/Kaku
