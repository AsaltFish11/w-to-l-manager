//! `project_list.json` 的数据模型、解析，以及卸载命令推导。
//!
//! 顶层结构：
//! ```json
//! {
//!     "lists": [
//!         {
//!             "id": "ChenPi11_cmd",
//!             "describe": "...",
//!             "github-url": "https://github.com/ChenPi11/cmd",
//!             "clone-command": "git clone https://github.com/ChenPi11/cmd.git",
//!             "dependency": ["make"],
//!             "install-commands": [
//!                 { "permission": "normal", "command": "make" }
//!             ],
//!             "uninstall-commands": [
//!                 { "permission": "root", "command": "rm -f /usr/local/bin/cmd" }
//!             ]
//!         }
//!     ]
//! }
//! ```
//!
//! 命令既可以写成 `{"permission": "normal"|"root", "command": "..."}`，
//! 也可以直接写成字符串（等价于 `permission: "normal"`），后者兼容旧格式。
//!
//! `uninstall-commands` 是可选字段：若作者未提供，则尝试从 `install-commands`
//! 推导（例如 `make install PREFIX=/usr/local` → `make uninstall PREFIX=/usr/local`）。

use serde::de::{self, MapAccess, Visitor};
use serde::ser::Serializer;
use serde::{Deserialize, Deserializer, Serialize};
use std::collections::HashSet;
use std::fmt;

/// 一条命令需要的权限。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Permission {
    /// 用当前用户身份执行。
    #[default]
    Normal,
    /// 需要 root 权限，执行到这一步时才会向用户索要密码。
    Root,
}

impl Permission {
    pub fn label(self) -> &'static str {
        match self {
            Permission::Normal => "普通权限",
            Permission::Root => "需要 root",
        }
    }

    pub fn needs_root(self) -> bool {
        matches!(self, Permission::Root)
    }
}

/// 带权限声明的命令。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DetailedCommand {
    /// 缺省为 `normal`。
    #[serde(default)]
    pub permission: Permission,
    pub command: String,
}

/// 一条命令：支持字符串简写与带权限的完整写法。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandSpec {
    /// 旧格式：直接一个字符串，视为普通权限。
    Plain(String),
    /// `{"permission": ..., "command": ...}`
    Detailed(DetailedCommand),
}

impl CommandSpec {
    pub fn new(command: impl Into<String>, permission: Permission) -> Self {
        CommandSpec::Detailed(DetailedCommand {
            permission,
            command: command.into(),
        })
    }

    /// 普通权限的简写命令。
    pub fn normal(command: impl Into<String>) -> Self {
        CommandSpec::new(command, Permission::Normal)
    }

    pub fn command(&self) -> &str {
        match self {
            CommandSpec::Plain(command) => command,
            CommandSpec::Detailed(detail) => &detail.command,
        }
    }

    pub fn permission(&self) -> Permission {
        match self {
            CommandSpec::Plain(_) => Permission::Normal,
            CommandSpec::Detailed(detail) => detail.permission,
        }
    }

    /// 换一条命令，保留原有权限。
    pub fn with_command(&self, command: impl Into<String>) -> Self {
        CommandSpec::new(command, self.permission())
    }
}

impl fmt::Display for CommandSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.command())
    }
}

impl<'de> Deserialize<'de> for CommandSpec {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct SpecVisitor;

        impl<'de> Visitor<'de> for SpecVisitor {
            type Value = CommandSpec;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(
                    "命令字符串，或 {\"permission\": \"normal\"|\"root\", \"command\": \"...\"}",
                )
            }

            fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
                Ok(CommandSpec::Plain(value.to_string()))
            }

            fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<Self::Value, A::Error> {
                let detail = DetailedCommand::deserialize(de::value::MapAccessDeserializer::new(map))?;
                if detail.command.trim().is_empty() {
                    return Err(de::Error::custom("command 不能为空"));
                }
                Ok(CommandSpec::Detailed(detail))
            }
        }

        deserializer.deserialize_any(SpecVisitor)
    }
}

impl Serialize for CommandSpec {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            CommandSpec::Plain(command) => serializer.serialize_str(command),
            CommandSpec::Detailed(detail) => detail.serialize(serializer),
        }
    }
}

/// `project_list.json` 中的单个软件条目。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectEntry {
    /// 唯一标识，同时作为界面上的标题，也用作源码目录名。
    pub id: String,
    /// 作者 / 维护者，显示在标题旁边。
    #[serde(default)]
    pub author: Option<String>,
    /// 说明文字，允许包含换行。
    #[serde(default)]
    pub describe: String,
    /// 项目主页 / GitHub 链接，界面上会显示成可点击的链接。
    #[serde(rename = "github-url", default)]
    pub github_url: Option<String>,
    /// B 站视频链接（介绍 / 演示），界面上显示成可点击的超链接。
    #[serde(rename = "bilibili-url", default)]
    pub bilibili_url: Option<String>,
    /// 把源码克隆下来的命令；缺省表示不需要克隆，直接用「工作目录」。
    #[serde(rename = "clone-command", default)]
    pub clone_command: Option<String>,
    /// 依赖的可执行程序名（如 `make`、`cmake`）。
    #[serde(default)]
    pub dependency: Vec<String>,
    /// 安装命令，按顺序依次执行。
    #[serde(rename = "install-commands", default)]
    pub install_commands: Vec<CommandSpec>,
    /// 卸载命令；可选，缺省时由 `install-commands` 推导。
    #[serde(rename = "uninstall-commands", default)]
    pub uninstall_commands: Vec<CommandSpec>,
}

impl ProjectEntry {
    /// 是否需要先把源码克隆下来。
    pub fn needs_clone(&self) -> bool {
        self.clone_command
            .as_ref()
            .is_some_and(|command| !command.trim().is_empty())
    }

    /// 作者（去掉首尾空白后非空才算）。
    pub fn author(&self) -> Option<&str> {
        self.author
            .as_deref()
            .map(str::trim)
            .filter(|name| !name.is_empty())
    }

    /// 项目相关的外部链接：`(显示名, URL)`。
    ///
    /// 顺序固定，方便界面按统一顺序渲染。
    pub fn links(&self) -> Vec<(&'static str, &str)> {
        let mut out = Vec::new();
        if let Some(url) = non_empty(self.github_url.as_deref()) {
            out.push(("🔗", url));
        }
        if let Some(url) = non_empty(self.bilibili_url.as_deref()) {
            out.push(("📺", url));
        }
        out
    }
}

/// 去掉首尾空白，空串按 `None` 处理。
fn non_empty(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|v| !v.is_empty())
}

/// `project_list.json` 的顶层对象。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProjectList {
    #[serde(default)]
    pub lists: Vec<ProjectEntry>,
}

/// 解析结果：条目 + 非致命警告。
#[derive(Debug, Clone, Default)]
pub struct LoadedList {
    pub entries: Vec<ProjectEntry>,
    pub warnings: Vec<String>,
}

/// 解析 `project_list.json` 文本。
pub fn parse(text: &str) -> Result<LoadedList, String> {
    let value: serde_json::Value =
        serde_json::from_str(text).map_err(|e| format!("JSON 语法错误：{e}"))?;

    let entries = match value {
        serde_json::Value::Object(_) => {
            let list: ProjectList =
                serde_json::from_value(value).map_err(|e| format!("结构不匹配：{e}"))?;
            list.lists
        }
        serde_json::Value::Array(_) => serde_json::from_value::<Vec<ProjectEntry>>(value)
            .map_err(|e| format!("结构不匹配：{e}"))?,
        _ => return Err("顶层必须是对象 {\"lists\": [...]} 或数组 [...]".to_string()),
    };

    let mut warnings = Vec::new();
    let mut seen: HashSet<&str> = HashSet::new();
    for entry in &entries {
        if entry.id.trim().is_empty() {
            warnings.push("存在 id 为空的条目".to_string());
        } else if !seen.insert(entry.id.as_str()) {
            warnings.push(format!("id 重复：{}", entry.id));
        }
        if entry.id.contains('/') || entry.id.contains("..") {
            warnings.push(format!("id 里不应包含路径分隔符：{}", entry.id));
        }
        if entry.install_commands.is_empty() {
            warnings.push(format!("`{}` 未提供 install-commands，无法安装", entry.id));
        }
        if entry.github_url.is_none() {
            warnings.push(format!("`{}` 没有 github-url，界面上不会显示项目链接", entry.id));
        }
    }

    Ok(LoadedList { entries, warnings })
}

// ---------------------------------------------------------------------------
// 卸载计划
// ---------------------------------------------------------------------------

/// 卸载计划：直接采用作者在 `uninstall-commands` 里写的命令。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UninstallPlan {
    pub commands: Vec<CommandSpec>,
}

/// 某个条目能否卸载。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UninstallSupport {
    Available(UninstallPlan),
    Unavailable { reason: String },
}

/// 某条目是否可以卸载。
///
/// 只认作者显式写在 `uninstall-commands` 里的命令：从 `install-commands`
/// 反推卸载命令并不可靠（`make install` 未必有对应的 `make uninstall`，
/// 改写出来的命令有可能删错东西），所以不做任何推导。
pub fn uninstall_plan(entry: &ProjectEntry) -> UninstallSupport {
    if entry.uninstall_commands.is_empty() {
        return UninstallSupport::Unavailable {
            reason: "该条目没有提供 uninstall-commands，请在 project_list.json 里补充".to_string(),
        };
    }
    UninstallSupport::Available(UninstallPlan {
        commands: entry.uninstall_commands.clone(),
    })
}

// ---------------------------------------------------------------------------
// shell 引用
// ---------------------------------------------------------------------------

/// 把一个字符串安全地放进 shell 命令行（需要时加单引号）。
pub fn shell_quote(word: &str) -> String {
    let safe = !word.is_empty()
        && word.chars().all(|c| {
            c.is_ascii_alphanumeric()
                || matches!(
                    c,
                    '_' | '@' | '%' | '+' | '=' | ':' | ',' | '.' | '/' | '-'
                )
        });
    if safe {
        word.to_string()
    } else {
        format!("'{}'", word.replace('\'', r"'\''"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, install: &[CommandSpec], uninstall: &[CommandSpec]) -> ProjectEntry {
        ProjectEntry {
            id: id.to_string(),
            install_commands: install.to_vec(),
            uninstall_commands: uninstall.to_vec(),
            ..Default::default()
        }
    }

    #[test]
    fn parses_the_new_document_format() {
        let text = r#"{
            "lists" : [
                {
                    "id": "ChenPi11_cmd",
                    "author": "ChenPi11",
                    "describe": "Windows cmd.exe 解释器",
                    "github-url": "https://github.com/ChenPi11/cmd",
                    "bilibili-url": "https://www.bilibili.com/video/BV1wkuH64EE8",
                    "clone-command": "git clone https://github.com/ChenPi11/cmd.git",
                    "dependency": ["make"],
                    "install-commands": [
                        { "permission": "normal", "command": "make" },
                        { "permission": "normal", "command": "make install PREFIX=/usr/local" }
                    ],
                    "uninstall-commands": [
                        { "permission": "root", "command": "rm -f /usr/local/bin/cmd" }
                    ]
                }
            ]
        }"#;
        let loaded = parse(text).expect("应能解析");
        assert_eq!(loaded.entries.len(), 1);
        let e = &loaded.entries[0];
        assert_eq!(e.id, "ChenPi11_cmd");
        assert_eq!(e.author(), Some("ChenPi11"));
        assert_eq!(e.github_url.as_deref(), Some("https://github.com/ChenPi11/cmd"));
        assert_eq!(
            e.bilibili_url.as_deref(),
            Some("https://www.bilibili.com/video/BV1wkuH64EE8")
        );
        assert_eq!(
            e.clone_command.as_deref(),
            Some("git clone https://github.com/ChenPi11/cmd.git")
        );
        assert!(e.needs_clone());
        assert_eq!(e.dependency, vec!["make"]);

        assert_eq!(e.install_commands.len(), 2);
        assert_eq!(e.install_commands[0].command(), "make");
        assert_eq!(e.install_commands[0].permission(), Permission::Normal);

        assert_eq!(e.uninstall_commands.len(), 1);
        assert_eq!(
            e.uninstall_commands[0].command(),
            "rm -f /usr/local/bin/cmd"
        );
        assert_eq!(e.uninstall_commands[0].permission(), Permission::Root);
        assert!(loaded.warnings.is_empty(), "{:?}", loaded.warnings);
    }

    #[test]
    fn author_and_links_are_optional_and_trimmed() {
        // 都没有时：author() 是 None，links() 是空的
        let loaded = parse(r#"[{"id":"a","install-commands":["true"]}]"#).unwrap();
        let entry = &loaded.entries[0];
        assert_eq!(entry.author(), None);
        assert!(entry.links().is_empty());

        // 空串 / 空白串按“没写”处理，不会渲染出空链接
        let loaded = parse(
            r#"[{"id":"a","author":"  ","github-url":"","bilibili-url":"   ",
                 "install-commands":["true"]}]"#,
        )
        .unwrap();
        let entry = &loaded.entries[0];
        assert_eq!(entry.author(), None);
        assert!(entry.links().is_empty());

        // 有值时按固定顺序给出（GitHub 在前，B 站在后），并去掉首尾空白
        let loaded = parse(
            r#"[{"id":"a","author":" ChenPi11 ","bilibili-url":" https://b.com/v ",
                 "github-url":"https://g.com/x","install-commands":["true"]}]"#,
        )
        .unwrap();
        let entry = &loaded.entries[0];
        assert_eq!(entry.author(), Some("ChenPi11"));
        assert_eq!(
            entry.links(),
            vec![("🔗", "https://g.com/x"), ("📺", "https://b.com/v")]
        );
    }

    #[test]
    fn still_accepts_plain_string_commands() {
        let text = r#"[{"id":"old","install-commands":["make install"],
                        "uninstall-commands":["make uninstall"]}]"#;
        let loaded = parse(text).expect("旧格式仍应可用");
        let e = &loaded.entries[0];
        assert_eq!(e.install_commands[0], CommandSpec::Plain("make install".into()));
        assert_eq!(e.install_commands[0].permission(), Permission::Normal);
        assert!(!e.needs_clone());
    }

    #[test]
    fn permission_defaults_to_normal_and_rejects_typos() {
        let loaded = parse(r#"[{"id":"a","install-commands":[{"command":"make install"}]}]"#)
            .expect("缺少 permission 时应默认为 normal");
        assert_eq!(
            loaded.entries[0].install_commands[0].permission(),
            Permission::Normal
        );

        let err = parse(r#"[{"id":"a","install-commands":[{"permission":"rooty","command":"x"}]}]"#)
            .unwrap_err();
        assert!(err.contains("结构不匹配"), "{err}");
    }

    #[test]
    fn rejects_empty_command() {
        let err = parse(r#"[{"id":"a","install-commands":[{"command":"   "}]}]"#).unwrap_err();
        assert!(err.contains("结构不匹配"), "{err}");
    }

    #[test]
    fn parses_bare_array_and_explicit_uninstall() {
        let text = r#"[{"id":"a","install-commands":["make install"],"uninstall-commands":["rm -rf /opt/a"]}]"#;
        let loaded = parse(text).expect("should parse");
        assert_eq!(loaded.entries[0].uninstall_commands[0].command(), "rm -rf /opt/a");
    }

    #[test]
    fn reports_bad_json_and_bad_shape() {
        assert!(parse("{ not json").unwrap_err().contains("JSON 语法错误"));
        assert!(parse("\"just a string\"").unwrap_err().contains("顶层"));
        assert!(parse("{\"lists\":\"nope\"}").unwrap_err().contains("结构不匹配"));
    }

    #[test]
    fn warns_about_duplicate_and_empty_ids() {
        let text = r#"[{"id":"dup","install-commands":["make install"]},{"id":"dup","install-commands":["make install"]},{"id":"","install-commands":["make install"]}]"#;
        let loaded = parse(text).expect("should parse");
        assert!(
            loaded.warnings.iter().any(|w| w == "id 重复：dup"),
            "{:?}",
            loaded.warnings
        );
        assert!(
            loaded.warnings.iter().any(|w| w == "存在 id 为空的条目"),
            "{:?}",
            loaded.warnings
        );
        // 缺 github-url 也应有提示
        assert!(
            loaded.warnings.iter().any(|w| w.contains("github-url")),
            "{:?}",
            loaded.warnings
        );
    }

    #[test]
    fn warns_when_install_commands_are_missing() {
        let loaded = parse(r#"[{"id":"a"}]"#).expect("should parse");
        assert!(loaded.warnings.iter().any(|w| w.contains("install-commands")));
        assert!(loaded.warnings.iter().any(|w| w.contains("github-url")));
    }

    #[test]
    fn warns_about_ids_that_look_like_paths() {
        let loaded =
            parse(r#"[{"id":"../evil","install-commands":["true"]}]"#).expect("should parse");
        assert!(
            loaded.warnings.iter().any(|w| w.contains("路径分隔符")),
            "{:?}",
            loaded.warnings
        );
    }




    #[test]
    fn plan_uses_the_author_provided_commands() {
        let e = entry(
            "a",
            &[CommandSpec::normal("make install")],
            &[CommandSpec::new("make uninstall", Permission::Root)],
        );
        match uninstall_plan(&e) {
            UninstallSupport::Available(p) => {
                assert_eq!(p.commands.len(), 1);
                assert_eq!(p.commands[0].command(), "make uninstall");
                // 权限声明跟着作者走
                assert_eq!(p.commands[0].permission(), Permission::Root);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    /// 没有写 uninstall-commands 就不能卸载 —— 不再从 install-commands 反推。
    #[test]
    fn plan_refuses_to_derive_uninstall_commands() {
        // 哪怕安装命令看起来“能推导”（make install），也不再猜
        let e = entry("a", &[CommandSpec::normal("make install")], &[]);
        match uninstall_plan(&e) {
            UninstallSupport::Unavailable { reason } => {
                assert!(reason.contains("uninstall-commands"), "{reason}")
            }
            other => panic!("不该推导出卸载命令，实际是 {other:?}"),
        }

        // 完全没有命令的条目同样不可卸载
        assert!(matches!(
            uninstall_plan(&entry("b", &[], &[])),
            UninstallSupport::Unavailable { .. }
        ));
    }


    #[test]
    fn quotes_paths_with_spaces() {
        assert_eq!(shell_quote("/tmp/a b"), "'/tmp/a b'");
        assert_eq!(shell_quote("/tmp/a"), "/tmp/a");
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
        assert_eq!(shell_quote(""), "''");
    }

    #[test]
    fn command_spec_round_trips_through_json() {
        let specs = vec![
            CommandSpec::normal("make"),
            CommandSpec::new("rm -f /usr/local/bin/cmd", Permission::Root),
        ];
        let json = serde_json::to_string(&specs).unwrap();
        let back: Vec<CommandSpec> = serde_json::from_str(&json).unwrap();
        assert_eq!(back, specs);
        // 带权限的写法序列化成对象
        assert!(json.contains(r#""permission":"normal""#), "{json}");
        assert!(json.contains(r#""permission":"root""#), "{json}");

        // 旧的字符串写法仍然可以读，且序列化回去还是字符串
        let plain: Vec<CommandSpec> = serde_json::from_str(r#"["make install"]"#).unwrap();
        assert_eq!(plain[0].permission(), Permission::Normal);
        assert_eq!(serde_json::to_string(&plain).unwrap(), r#"["make install"]"#);
    }
}
