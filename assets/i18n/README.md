# 中文界面（`zh-ui` 分支本地功能）

上游 Kaku 不做 UI i18n（见根 `AGENTS.md`，PR #362 的中文 UI 已在 `b4d779a` 回滚）。
这套东西只活在**独立分支**上，`main` 仍与上游同步，因此 **`git pull` 上游不受影响**。

## 怎么用

```lua
-- ~/.config/kaku/kaku.lua
config.language = "zh-CN"
```

或临时试用（不改配置）：

```bash
KAKU_UI_LANG=zh-CN open -a Kaku
```

不设置时一切保持英文原文，与上游行为完全一致。

## 怎么加/改词

翻译表有两层，后者覆盖前者：

1. 仓库内置：`assets/i18n/<locale>.toml`（编译进二进制，`include_str!`）
2. 用户覆盖：`~/.config/kaku/i18n/<locale>.toml`（**加词不需要改代码**）

键是界面上的英文原文，值是译文；缺词条时原样显示英文，所以永远可以增量翻译。

```toml
# ~/.config/kaku/i18n/zh-CN.toml
"Shell" = "我的终端"
"Some New Label" = "新词条"
```

注意 TOML 不允许重复键——重复会让整个文件解析失败（此时会退回英文并写日志）。
改完用 `cargo test -p kaku-gui --bin kaku-gui zh_table` 验证表能被解析。

## 与上游同步

```bash
git switch main && git pull            # 上游照常拉
git switch zh-ui && git rebase main    # 把中文功能重放到新上游之上
```

冲突只会出现在很少的几处 `tr()` 调用点（菜单/面板/命令面板），翻译表本身几乎不会冲突。
想审计有没有漏译（上游新增命令后）：

```bash
cargo test -p kaku-gui --bin kaku-gui i18n -- --ignored --nocapture
```

## 已覆盖 / 未覆盖

- 已覆盖：菜单栏（顶层菜单 + 菜单项）、命令面板与启动器标题、SFTP 面板的提示与消息。
- 未覆盖：配置 TUI（`kaku` crate，约 77+ 条）、若干确认对话框与 toast、CLI 子命令输出。
