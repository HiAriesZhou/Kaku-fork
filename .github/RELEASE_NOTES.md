# V0.27.1 Guard

<div align="center">
  <img src="https://raw.githubusercontent.com/tw93/Kaku/main/assets/logo.png" alt="Kaku Logo" width="120" height="120" />
  <h1 style="margin: 12px 0 6px;">Kaku V0.27.1</h1>
  <p><em>A fast, out-of-the-box terminal built for AI coding.</em></p>
</div>

### Changelog
1. **Crash Fix**: Kaku no longer quits with every tab open while drawing certain lines that contain links.
2. **Remote Pairing**: A new browser pairs only after both it and `kaku remote` show the same six-digit code and you press Enter in the terminal, so someone who gets hold of the link cannot get in alone.
3. **Remote Direct**: Remote terminals now connect directly even with a TUN-mode proxy such as Stash or Clash running, and a stranger can no longer cut that direct connection.
4. **Custom Themes**: Settings lists the color schemes in `~/.config/kaku/colors` and applies them just like the built-in ones.
5. **Yazi and Lazygit**: Cmd+Shift+Y and Cmd+Shift+G open the tool in a new tab when Codex, Claude Code or vim holds the pane, instead of typing the command into it.
6. **Tab Bar**: With the tab bar at the bottom, clicking the last terminal row no longer switches tabs.
7. **Kaku Assistant**: The chat no longer redraws the whole screen while it waits for a reply, and saving its settings writes the file in one step, so an interrupted save no longer leaves it half written.

### 更新日志
1. **崩溃修复**：绘制某些带链接的行时，Kaku 不会再连同所有标签一起退出。
2. **远程配对**：新浏览器要等它和 `kaku remote` 显示同一个六位码、并在终端按下回车后才会配对，别人拿到链接也进不来。
3. **远程直连**：开着 Stash、Clash 这类 TUN 模式代理时，远程终端也能直接连接，外人也无法再掐断这条直连。
4. **自定义主题**：Settings 会列出 `~/.config/kaku/colors` 里的配色方案，可以像内置主题一样直接应用。
5. **Yazi 和 Lazygit**：Codex、Claude Code 或 vim 占着当前分屏时，Cmd+Shift+Y 和 Cmd+Shift+G 会在新标签页打开工具，不再把命令打进去。
6. **标签栏**：标签栏放在底部时，点击终端最后一行不会再切换标签。
7. **Kaku 助手**：等待回复时不再反复重绘整个界面，保存设置时一次写完整个文件，中途打断也不会留下写了一半的配置。

Special thanks to @Konata9, @sunbird89629 and @darion-yaphet for their contributions to this release.

> https://github.com/tw93/Kaku
