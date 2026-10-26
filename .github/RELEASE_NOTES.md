# V0.27.0 Reach

<div align="center">
  <img src="https://raw.githubusercontent.com/tw93/Kaku/main/assets/logo.png" alt="Kaku Logo" width="120" height="120" />
  <h1 style="margin: 12px 0 6px;">Kaku V0.27.0</h1>
  <p><em>A fast, out-of-the-box terminal built for AI coding.</em></p>
</div>

### Changelog
1. **Wrapped Links**: A file path or URL that Claude Code and other TUIs break across two lines now opens in full with Cmd+Click, from either half.
2. **Voice Input**: WeType and other input tools that look for the focused text field now find Kaku's cursor reliably, also after switching tabs or panes.
3. **Remote Speed**: Remote terminals connect directly when the network allows, so typing no longer detours through the relay, which stays as the fallback.
4. **Remote Page**: The page installs as its own app on Android, says when the Mac behind the desktop view is locked, keeps tab titles readable, and no longer flashes at the wrong size after a reload or stays shrunk after leaving the desktop view.
5. **Remote Command**: `kaku remote` keeps working when another copy of Kaku starts with remote access off, and every `kaku` command now says why it failed instead of exiting silently.
6. **Kaku Assistant**: Answer `a` in `k` to approve the rest of a session's requests at once, and Cmd+Shift+E no longer runs a fix that quits or kills apps, it pastes it for review instead.
7. **macOS**: Kaku no longer blocks log out, restart or shut down, closing from the right-click menu asks only once, the hotkey drawer keeps its height across displays, and pi sessions show their agent mark and topic in the tab.

### 更新日志
1. **折行链接**：Claude Code 等 TUI 折成两行的文件路径和网址，现在 Cmd+点击任意一半都能完整打开。
2. **语音输入**：微信输入法等依赖焦点文本框的输入工具，现在能稳定找到 Kaku 的光标，切换标签或分屏后也一样。
3. **远程速度**：网络允许时远程终端会直接连接，打字不再绕道中继，中继保留为备用路线。
4. **远程网页**：网页在 Android 上可以安装成独立应用，桌面视图背后的 Mac 锁屏时会提示，标签标题保持可读，刷新后不再闪现错误尺寸，离开桌面视图后也不再缩着。
5. **远程命令**：另一个关闭了远程访问的 Kaku 启动后，`kaku remote` 照常可用，所有 `kaku` 命令失败时都会说明原因，不再静默退出。
6. **Kaku 助手**：在 `k` 里回答 `a` 可一次同意本次会话后续的所有请求，Cmd+Shift+E 不再直接执行会退出或结束应用的修复命令，而是粘贴出来让你确认。
7. **macOS**：Kaku 不再阻止注销、重启和关机，右键菜单关闭只确认一次，快捷键抽屉在不同显示器间保持高度，pi 会话的标签会显示 Agent 标记和主题。

Special thanks to @sunbird89629 and @vw2x for their contributions to this release.

> https://github.com/tw93/Kaku
