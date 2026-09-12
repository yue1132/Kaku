//! 中文界面（本仓库 `zh-ui` 分支的本地功能，不属于上游）。
//!
//! 上游 Kaku 明确不做 UI i18n（见根 `AGENTS.md`），所以这里刻意做成最小侵入：
//! 一个模块 + 若干 `tr()` 调用点 + 一份数据表。表编译进二进制
//! （`assets/i18n/<locale>.toml`），并可在运行时被
//! `~/.config/kaku/i18n/<locale>.toml` 覆盖或补充——**加词不需要改代码**，
//! 上游合并时冲突只会落在这几处 `tr()` 上。
//!
//! 未设置 `config.language` 时一切保持英文原文（上游行为不变）。

use std::collections::HashMap;
use std::sync::OnceLock;

/// 仓库内置的翻译表（按需增加 include_str!，例如将来加 ja-JP）。
fn bundled(locale: &str) -> Option<&'static str> {
    match locale {
        "zh-CN" | "zh" | "zh-Hans" => Some(include_str!("../../assets/i18n/zh-CN.toml")),
        _ => None,
    }
}

/// 用户覆盖表的位置：`~/.config/kaku/i18n/<locale>.toml`。
fn user_table_path(locale: &str) -> Option<std::path::PathBuf> {
    let home = dirs_next::home_dir()?;
    Some(
        home.join(".config/kaku/i18n")
            .join(format!("{locale}.toml")),
    )
}

/// 载入「内置表 + 用户覆盖」，后者优先。纯函数式，方便测试。
pub fn load_table(locale: &str, user_path: Option<&std::path::Path>) -> HashMap<String, String> {
    let mut table = HashMap::new();
    let locale = locale.trim();
    if locale.is_empty() {
        return table;
    }
    if let Some(text) = bundled(locale) {
        merge(&mut table, text);
    }
    if let Some(path) = user_path {
        if let Ok(text) = std::fs::read_to_string(path) {
            merge(&mut table, &text);
        }
    }
    table
}

fn merge(table: &mut HashMap<String, String>, text: &str) {
    match text.parse::<toml::Value>() {
        Ok(toml::Value::Table(entries)) => {
            for (source, translated) in entries {
                if let Some(translated) = translated.as_str() {
                    if !source.trim().is_empty() && !translated.trim().is_empty() {
                        table.insert(source, translated.to_string());
                    }
                }
            }
        }
        Ok(_) => log::warn!("i18n: translation file is not a table"),
        Err(err) => log::warn!("i18n: cannot parse translations: {err}"),
    }
}

static TABLE: OnceLock<HashMap<String, String>> = OnceLock::new();

/// 启动时调用一次。空字符串表示不翻译（出厂状态）。
///
/// `config.language` 优先；为空时看 `KAKU_UI_LANG`，方便临时试用中文而
/// 不改配置文件。
pub fn init(locale: &str) {
    let locale = if locale.trim().is_empty() {
        std::env::var("KAKU_UI_LANG").unwrap_or_default()
    } else {
        locale.to_string()
    };
    let locale = locale.as_str();
    let user = user_table_path(locale.trim());
    let table = load_table(locale, user.as_deref());
    if !table.is_empty() {
        log::info!(
            "i18n: {} entries loaded for locale {:?}",
            table.len(),
            locale.trim()
        );
    }
    let _ = TABLE.set(table);
}

fn table() -> &'static HashMap<String, String> {
    TABLE.get_or_init(HashMap::new)
}

/// 当前是否真的启用了翻译（目前只有测试与日志用得到）。
#[allow(dead_code)]
pub fn enabled() -> bool {
    !table().is_empty()
}

/// 把界面英文原文换成当前语言；没有译文时原样返回英文。
pub fn tr(source: &str) -> String {
    if table().is_empty() {
        return source.to_string();
    }
    match table().get(source) {
        Some(translated) => translated.clone(),
        None => source.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_user_table(entries: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("zh-CN.toml");
        std::fs::write(&path, entries).unwrap();
        (dir, path)
    }

    #[test]
    fn empty_locale_disables_translation() {
        let table = load_table("", None);
        assert!(table.is_empty());
    }

    #[test]
    fn bundled_table_has_chinese_for_core_menus() {
        let table = load_table("zh-CN", None);
        assert_eq!(table.get("Shell").map(String::as_str), Some("终端"));
        assert!(table.len() > 30, "table looks empty: {}", table.len());
    }

    #[test]
    fn unknown_locale_falls_back_to_english() {
        let table = load_table("xx-YY", None);
        assert!(table.is_empty());
    }

    /// 用户表既能把内置译文改掉，也能补充仓库里没有的词条。
    #[test]
    fn user_table_overrides_and_extends() {
        let (dir, path) = write_user_table(
            r#"
"Shell" = "我的终端"
"Some New Label" = "新词条"
"#,
        );
        let table = load_table("zh-CN", Some(&path));
        assert_eq!(table.get("Shell").map(String::as_str), Some("我的终端"));
        assert_eq!(
            table.get("Some New Label").map(String::as_str),
            Some("新词条")
        );
        // 内置的其他词条仍在
        assert!(table.contains_key("Edit"));
        drop(dir);
    }

    /// 审计：列出还没翻译的命令标题（上游新增命令后跑一下）。
    /// 默认忽略，用 `--ignored --nocapture` 运行。
    #[test]
    #[ignore]
    fn audit_untranslated_commands() {
        let config = config::ConfigHandle::default_config();
        let table = load_table("zh-CN", None);
        let mut missing = Vec::new();
        for cmd in crate::commands::CommandDef::expanded_commands(&config) {
            if !table.contains_key(cmd.brief.as_ref()) {
                missing.push(cmd.brief.to_string());
            }
        }
        missing.sort();
        missing.dedup();
        println!("未翻译的命令标题（{} 条）：", missing.len());
        for title in &missing {
            println!("  \"{title}\" = \"\"");
        }
        assert!(table.len() > 100, "翻译表看起来是空的：{}", table.len());
    }

    #[test]
    fn broken_user_table_is_ignored_not_fatal() {
        let (dir, path) = write_user_table("this is not toml = = =");
        let table = load_table("zh-CN", Some(&path));
        assert!(table.contains_key("Shell"), "builtin table was lost");
        drop(dir);
    }
}
