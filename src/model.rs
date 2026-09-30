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
    /// 特殊配置：这条命令返回非 0 也继续往下跑（其余命令非 0 就中止）。
    ///
    /// 主要给卸载用：同一个项目可能装在用户级或系统级，`make uninstall`、
    /// `systemctl --user disable` 这类命令“没东西可卸”时返回非 0 是正常的。
    #[serde(
        rename = "ignore-error",
        alias = "continue-on-error",
        alias = "allow-failure",
        default
    )]
    pub ignore_error: bool,
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
            ignore_error: false,
        })
    }

    /// 这条命令失败（返回非 0）时是否继续执行后面的命令。
    pub fn ignore_error(&self) -> bool {
        match self {
            CommandSpec::Plain(_) => false,
            CommandSpec::Detailed(detail) => detail.ignore_error,
        }
    }

    /// 标记成「返回非 0 也继续」。
    pub fn ignoring_error(self) -> Self {
        match self {
            CommandSpec::Plain(command) => CommandSpec::Detailed(DetailedCommand {
                permission: Permission::Normal,
                command,
                ignore_error: true,
            }),
            CommandSpec::Detailed(mut detail) => {
                detail.ignore_error = true;
                CommandSpec::Detailed(detail)
            }
        }
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

    /// 换一条命令，保留原有权限与「忽略错误」设置。
    pub fn with_command(&self, command: impl Into<String>) -> Self {
        let mut spec = CommandSpec::new(command, self.permission());
        if self.ignore_error() {
            spec = spec.ignoring_error();
        }
        spec
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
                    "命令字符串，或 {\"permission\": \"normal\"|\"root\", \
                     \"command\": \"...\", \"ignore-error\": true}",
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
    /// 适用平台，比如 `["all-linux"]`；空数组表示没写。
    #[serde(default)]
    pub platform: Vec<String>,
    /// 许可证标识，一般是 SPDX 写法（`MIT`、`GPL-3.0-only`…）。
    #[serde(default)]
    pub license: Option<String>,
    /// 额外提示（可多行），显示在说明下方。
    #[serde(default)]
    pub tips: String,
    /// `button-N` 形式的操作按钮，按 N 从小到大排好序。
    ///
    /// 字段名带数字，没法用 serde 直接映射，由 [`parse`] 从原始 JSON 里挑出来。
    #[serde(skip)]
    pub buttons: Vec<ActionButton>,
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

    /// 适用平台（去掉空白项）。
    pub fn platforms(&self) -> Vec<&str> {
        self.platform
            .iter()
            .map(|p| p.trim())
            .filter(|p| !p.is_empty())
            .collect()
    }

    /// 许可证标识（去掉首尾空白后非空才算）。
    pub fn license(&self) -> Option<&str> {
        non_empty(self.license.as_deref())
    }

    /// 额外提示（去掉首尾空白后非空才算）。
    pub fn tips(&self) -> Option<&str> {
        let tips = self.tips.trim();
        if tips.is_empty() { None } else { Some(tips) }
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

/// 从条目的原始 JSON 里挑出 `button-<数字>` 形式的按钮，按数字从小到大排序。
///
/// 数字位数不限：`button-1`、`button-07`、`button-123` 都认。
fn extract_buttons(value: &serde_json::Value, warnings: &mut Vec<String>) -> Vec<ActionButton> {
    let Some(map) = value.as_object() else {
        return Vec::new();
    };

    let mut found: Vec<(String, ActionButton)> = Vec::new();
    for (key, raw) in map {
        let Some(number) = button_number(key) else {
            continue;
        };
        match serde_json::from_value::<ActionButton>(raw.clone()) {
            Ok(button) => {
                if button.commands.is_empty() {
                    warnings.push(format!("`{key}` 没有任何命令，点了不会有反应"));
                }
                found.push((number, button));
            }
            Err(err) => warnings.push(format!("`{key}` 结构不对：{err}")),
        }
    }

    // 按数值大小排（先比位数再比字典序，多少位都不会溢出）
    found.sort_by(|a, b| a.0.len().cmp(&b.0.len()).then_with(|| a.0.cmp(&b.0)));
    found.into_iter().map(|(_, button)| button).collect()
}

/// `button-` 后面跟任意位数、任意大小的数字都算按钮，返回规范化后的数字串。
fn button_number(key: &str) -> Option<String> {
    let rest = key.strip_prefix("button-")?;
    if rest.is_empty() || !rest.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    // 去掉前导零（`button-07` 就是第 7 个）
    let normalized = rest.trim_start_matches('0');
    Some(if normalized.is_empty() {
        "0".to_string()
    } else {
        normalized.to_string()
    })
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

    // 先取出「原始条目数组」：button-N 这种带数字的字段名没法直接用 serde
    // 映射，所以要在反序列化之前从原始 JSON 里挑出来。
    let raw_list: Vec<serde_json::Value> = match value {
        serde_json::Value::Object(map) => map
            .get("lists")
            .ok_or_else(|| "结构不匹配：顶层对象里缺少 `lists` 字段".to_string())?
            .as_array()
            .ok_or_else(|| "结构不匹配：`lists` 必须是数组".to_string())?
            .clone(),
        serde_json::Value::Array(items) => items,
        _ => return Err("顶层必须是对象 {\"lists\": [...]} 或数组 [...]".to_string()),
    };

    let mut warnings = Vec::new();
    let mut entries = Vec::with_capacity(raw_list.len());
    for (index, raw) in raw_list.into_iter().enumerate() {
        let buttons = extract_buttons(&raw, &mut warnings);
        let mut entry: ProjectEntry = serde_json::from_value(raw)
            .map_err(|e| format!("第 {} 个条目结构不匹配：{e}", index + 1))?;
        entry.buttons = buttons;
        entries.push(entry);
    }

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

/// 条目里的一个操作按钮（JSON 里写成 `button-1`、`button-2` …）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionButton {
    /// 按钮上显示的文字。
    #[serde(default)]
    pub name: String,
    /// 鼠标悬停时的说明。
    #[serde(default)]
    pub describe: String,
    /// 点击后依次执行的命令。
    #[serde(default)]
    pub commands: Vec<CommandSpec>,
    /// 是否放到后台跑：不占用任务位、不阻塞界面，只在开始/结束时提示一句。
    #[serde(default)]
    pub background: bool,
}

impl ActionButton {
    /// 按钮文字；没写 name 时退回一个默认值。
    pub fn label(&self) -> &str {
        let name = self.name.trim();
        if name.is_empty() { "执行" } else { name }
    }
}

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
    fn buttons_are_collected_and_sorted_by_number() {
        let loaded = parse(
            r#"{"lists":[{
                "id": "a",
                "github-url": "https://example.com",
                "install-commands": ["true"],
                "button-10": {"name": "十", "commands": ["true"]},
                "button-2":  {"name": "二", "commands": ["true"]},
                "button-1":  {"name": "一", "describe": "第一个", "commands": ["echo hi"]}
            }]}"#,
        )
        .unwrap();
        let entry = &loaded.entries[0];
        let names: Vec<&str> = entry.buttons.iter().map(|b| b.label()).collect();
        assert_eq!(names, vec!["一", "二", "十"], "要按数字排，不是按字符串");
        assert_eq!(entry.buttons[0].describe, "第一个");
        assert_eq!(entry.buttons[0].commands[0].command(), "echo hi");
        assert!(loaded.warnings.is_empty(), "{:?}", loaded.warnings);
    }

    /// button- 后面跟任意数字都认；不规范的键名一律忽略。
    #[test]
    fn any_number_of_digits_is_recognised() {
        let loaded = parse(
            r#"{"lists":[{
                "id": "a",
                "button-1": {"name": "一", "commands": ["true"]},
                "button-007": {"name": "七", "commands": ["true"]},
                "button-0": {"name": "零", "commands": ["true"]},
                "button-123456789012345678901234567890": {"name": "巨", "commands": ["true"]},
                "button-": {"name": "空数字", "commands": ["true"]},
                "button-x": {"name": "非数字", "commands": ["true"]},
                "button": {"name": "没有横杠", "commands": ["true"]},
                "buttons-1": {"name": "复数", "commands": ["true"]},
                "button-1x": {"name": "数字加字母", "commands": ["true"]}
            }]}"#,
        )
        .unwrap();
        let names: Vec<&str> = loaded.entries[0].buttons.iter().map(|b| b.label()).collect();
        assert_eq!(
            names,
            vec!["零", "一", "七", "巨"],
            "只有 button-<纯数字> 才算按钮，且按数值排序"
        );
    }

    #[test]
    fn buttons_without_commands_are_warned_about() {
        let loaded = parse(
            r#"{"lists":[{"id": "a", "button-1": {"name": "空"}}]}"#,
        )
        .unwrap();
        assert_eq!(loaded.entries[0].buttons.len(), 1, "按钮本身还是要留着");
        assert!(
            loaded.warnings.iter().any(|w| w.contains("button-1")),
            "{:?}",
            loaded.warnings
        );
    }

    #[test]
    fn buttons_can_ask_to_run_in_the_background() {
        let loaded = parse(
            r#"{"lists":[{"id": "a",
                "button-1": {"name": "启动", "background": true, "commands": ["true"]},
                "button-2": {"name": "普通", "commands": ["true"]}
            }]}"#,
        )
        .unwrap();
        let buttons = &loaded.entries[0].buttons;
        assert!(buttons[0].background, "写了 background: true 就该是后台按钮");
        assert!(!buttons[1].background, "没写就默认前台");
    }

    #[test]
    fn license_is_optional_and_trimmed() {
        let loaded = parse(r#"{"lists":[{"id": "a", "license": " GPL-3.0-only "}]}"#).unwrap();
        assert_eq!(loaded.entries[0].license(), Some("GPL-3.0-only"));

        let loaded = parse(r#"{"lists":[{"id": "a", "license": "   "}]}"#).unwrap();
        assert_eq!(loaded.entries[0].license(), None);

        let loaded = parse(r#"{"lists":[{"id": "a"}]}"#).unwrap();
        assert_eq!(loaded.entries[0].license(), None);
    }

    #[test]
    fn platforms_and_tips_are_optional() {
        let loaded = parse(
            r#"{"lists":[{
                "id": "a",
                "platform": ["all-linux", "x86_64"],
                "tips": "注意安全\n第二行"
            }]}"#,
        )
        .unwrap();
        let entry = &loaded.entries[0];
        assert_eq!(entry.platforms(), vec!["all-linux", "x86_64"]);
        assert_eq!(entry.tips(), Some("注意安全\n第二行"));

        // 没写 / 空串 / 空白项都要当“没有”
        let loaded = parse(r#"{"lists":[{"id": "a"}]}"#).unwrap();
        assert!(loaded.entries[0].platforms().is_empty());
        assert_eq!(loaded.entries[0].tips(), None);

        let loaded = parse(
            r#"{"lists":[{"id": "a", "platform": ["", "  "], "tips": "   "}]}"#,
        )
        .unwrap();
        assert!(loaded.entries[0].platforms().is_empty());
        assert_eq!(loaded.entries[0].tips(), None);
    }

    #[test]
    fn ignore_error_is_read_from_json() {
        let loaded = parse(
            r#"{"lists":[{"id": "a", "uninstall-commands": [
                "plain",
                {"permission": "root", "command": "rm -f /x", "ignore-error": true},
                {"command": "alias", "continue-on-error": true},
                {"command": "normal", "ignore-error": false}
            ]}]}"#,
        )
        .unwrap();
        let commands = &loaded.entries[0].uninstall_commands;
        assert!(!commands[0].ignore_error(), "默认是失败即停");
        assert!(commands[1].ignore_error());
        assert!(commands[1].permission().needs_root(), "权限声明不受影响");
        assert!(commands[2].ignore_error(), "别名 continue-on-error 也要认");
        assert!(!commands[3].ignore_error());
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
