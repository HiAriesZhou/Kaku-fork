# V0.23.0 Drawer

<div align="center">
  <img src="https://raw.githubusercontent.com/tw93/Kaku/main/assets/logo.png" alt="Kaku Logo" width="120" height="120" />
  <h1 style="margin: 12px 0 6px;">Kaku V0.23.0</h1>
  <p><em>A fast, out-of-the-box terminal built for AI coding.</em></p>
</div>

### Changelog

1. **Hotkey Drawer**: Cmd+Opt+Ctrl+K now drops a terminal from the top of the screen over any app, and `config.macos_global_hotkey_mode = 'Ordinary'` brings back the old show and hide.
2. **Remote Desktop**: Hold for a second to open the right-click menu anywhere, and Window > Touch Mode adds a close button to every tab.
3. **Rename Tabs**: Cmd+Shift+I renames the current tab, and the editor shows its selection and stays centered when the tab bar is hidden.
4. **Fonts and Zoom**: Zooming is smoother, bitmap fallback fonts pick the right size at every step, and font, size and line height changes reach open windows on reload.
5. **Prompt Icons**: The Starship branch and git status icons are back, including for installs that kept the blank ones from earlier versions.
6. **AI Sessions**: Claude Code and Codex keep their last screen when Kaku restarts.
7. **Search and Prompts**: Cmd+F matches case only when you type a capital letter, and Ctrl-U deletes to the start of the line in Kaku's prompts.
8. **Notifications**: Each notification shows once, semicolons in its text are kept, and Kaku points to System Settings when macOS blocks them.
9. **Terminal Fixes**: Apps hear when the color palette changes, OSC 8 links keep their semicolons, tmux -CC input works again, and Kaku no longer answers screen checksum queries.
10. **Shortcuts**: Ctrl chords now reach the shell, so Ctrl+Shift+C and V no longer copy and paste, and Quick Select, Copy Mode and the pane picker lose their default keys; all can still be bound in kaku.lua.

### 更新日志

1. **快捷键抽屉**：Cmd+Opt+Ctrl+K 改为从屏幕顶部拉出终端，盖在当前应用之上，设置 `config.macos_global_hotkey_mode = 'Ordinary'` 可恢复原来的显示隐藏。
2. **远程桌面**：在任意位置按住一秒即可打开右键菜单，开启 Window > Touch Mode 后每个标签都有关闭按钮。
3. **重命名标签**：Cmd+Shift+I 可直接重命名当前标签，编辑框会显示选中的文字，隐藏标签栏时也保持居中。
4. **字体与缩放**：缩放更顺滑，位图回退字体在每一级都选对尺寸，修改字体、字号和行高后已打开的窗口会同步更新。
5. **提示符图标**：Starship 的分支和 git 状态图标恢复显示，之前版本留下空白图标的安装也会一并修好。
6. **AI 会话**：重启 Kaku 后，Claude Code 和 Codex 会保留上一次的画面。
7. **搜索与输入框**：Cmd+F 只在输入大写字母时区分大小写，Kaku 自带的输入框里 Ctrl-U 可删除到行首。
8. **通知**：每条通知只弹一次，正文里的分号不再丢失，macOS 拦截通知时 Kaku 会提示去系统设置开启。
9. **终端修复**：配色变化时会通知程序，OSC 8 链接保留分号，tmux -CC 输入恢复正常，也不再响应读取屏幕校验和的请求。
10. **快捷键**：Ctrl 组合键直接交给 shell，Ctrl+Shift+C 和 V 不再复制粘贴，Quick Select、Copy Mode 和分屏选择器不再默认绑定，都可在 kaku.lua 里自行绑定。

> https://github.com/tw93/Kaku
