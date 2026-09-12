# V0.20.0 Steady

<div align="center">
  <img src="https://raw.githubusercontent.com/tw93/Kaku/main/assets/logo.png" alt="Kaku Logo" width="120" height="120" />
  <h1 style="margin: 12px 0 6px;">Kaku V0.20.0</h1>
  <p><em>A fast, out-of-the-box terminal built for AI coding.</em></p>
</div>

### Changelog

1. **Window Behavior**: Native macOS dragging and edge tiling work again, closing windows no longer triggers a delayed repaint crash, and new windows skip repeated waits after graphics initialization fails.
2. **Shell Integration**: Returning to the prompt clears stray mouse reports after a TUI exits unexpectedly, tmux keeps its prompt and Smart Tab, and fish grep completion expands correctly.
3. **AI Model Selection**: Switching providers no longer reuses the previous provider's model list, all returned models are shown, and manually configured models remain available.
4. **AI Chat**: Chats include local project type information without probing local projects during remote sessions, and cancellation interrupts retry waits promptly.
5. **Context Menu**: Right-click to paste, search, open AI chat, or manage panes, with an option in Settings to show the new-tab button.
6. **Theme Settings**: Standalone configurations can use the built-in light and dark themes, Settings saves to the custom configuration used at launch, manual color overrides are shown explicitly, and Fancy tabs use the selected theme.
7. **Link Detection**: Unrelated text after a hard newline stays out of link targets while automatically wrapped URLs keep their complete addresses.
8. **Version Reporting**: The GUI executable reports its package version without initializing a window.

### 更新日志

1. **窗口操作**：恢复 macOS 原生拖动和边缘平铺，修复关闭窗口后可能出现的崩溃，图形初始化失败后新窗口不再重复等待。
2. **Shell 集成**：修复 TUI 异常退出后鼠标移动产生乱码的问题，保留 tmux 中的提示符和智能补全，并修正 fish 的 grep 补全展开。
3. **AI 模型选择**：切换服务后不再沿用旧服务的模型列表，完整展示服务返回的模型，并保留手动配置的模型。
4. **AI 对话**：补充当前项目类型信息，远程会话不读取本机项目，取消请求时不再等待重试倒计时结束。
5. **右键菜单**：支持粘贴、搜索、AI 对话和分屏操作，也可以在设置中开启新建标签页按钮。
6. **主题设置**：独立配置也能使用内置深浅主题，通过自定义配置启动时设置会保存到对应文件，手动配色覆盖主题时会显示说明，Fancy 标签栏也会跟随主题配色。
7. **链接识别**：避免把换行后的无关输出拼进链接，保留自动折行网址的完整地址。
8. **版本查询**：图形程序的版本查询无需初始化窗口即可正确返回。

Special thanks to @elonnzhang, @trxuan, and @yansigit for their contributions to this release.

> https://github.com/tw93/Kaku
