# V0.23.1 Steady

<div align="center">
  <img src="https://raw.githubusercontent.com/tw93/Kaku/main/assets/logo.png" alt="Kaku Logo" width="120" height="120" />
  <h1 style="margin: 12px 0 6px;">Kaku V0.23.1</h1>
  <p><em>A fast, out-of-the-box terminal built for AI coding.</em></p>
</div>

### Changelog

1. **Performance**: Kaku no longer scans every process on the Mac while it draws, so AI coding tools and a blinking cursor cost far less CPU.
2. **Tabs**: Clicking tabs in fullscreen no longer reorders them, and Settings adds a Tab Numbers switch.
3. **Sessions**: Tabs and their history come back after a restore, a quit or an in-app update, and windows reopened with Restore Previous Window keep saving.
4. **Shell**: The cursor comes back at the prompt after a program hides it and exits, and dotfiles kept as symlinks stay linked when Kaku updates them.
5. **Kaku AI**: Saving a Gemini model keeps its other settings, a broken assistant.toml is no longer overwritten, and model servers on private IPv6 addresses skip the system proxy.
6. **Diagnostics**: The kaku doctor bundle leaves out typed keys, input method text, passwords in URLs and GitHub tokens.
7. **Hotkey Drawer**: With a display stacked above, the drawer appears at once instead of sliding across it, and it keeps the height you drag it to.

### 更新日志

1. **性能**：绘制时不再扫描整台 Mac 的全部进程，跑 AI 编程工具和光标闪烁时 CPU 占用明显下降。
2. **标签**：全屏下点击标签不再打乱顺序，设置里新增了标签编号开关。
3. **会话**：恢复会话、退出和应用内更新之后，标签和历史内容都能找回，用 Restore Previous Window 重新打开的窗口也会继续保存。
4. **Shell**：程序隐藏光标后退出，回到提示符时光标会自动恢复，用符号链接管理的配置文件在 Kaku 更新时保持链接不变。
5. **Kaku AI**：保存 Gemini 模型时保留其他设置，解析失败的 assistant.toml 不再被覆盖，私有 IPv6 地址上的模型服务不再走系统代理。
6. **诊断包**：kaku doctor 生成的诊断包不再包含输入过的按键、输入法文字、链接里的密码和 GitHub 令牌。
7. **快捷键抽屉**：上方叠着另一块显示器时，抽屉直接出现，不再从那块屏幕上滑过，拖动调整过的高度也会保留。

> https://github.com/tw93/Kaku
