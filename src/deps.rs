//! 依赖管理：识别系统包管理器（pacman / apt）、把命令名映射成包名、
//! 生成依赖的安装 / 卸载命令，并组装最终要执行的步骤序列。
//!
//! 设计原则：
//! * **已经存在的依赖绝不动**：安装前就在 `PATH` 里的依赖只记录、不安装，
//!   卸载时也不会被删除。
//! * **只卸载自己装的东西**：卸载项目时只询问是否移除由本管理器自动安装的依赖。
//! * **装不了就明说**：识别不出包管理器或找不到对应包时，给出具体原因让用户手动处理。

use std::path::PathBuf;
use std::process::Command;

use crate::paths;

/// 支持的系统包管理器。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackageManagerKind {
    /// Arch Linux 系。
    Pacman,
    /// Debian / Ubuntu 系。
    Apt,
}

impl PackageManagerKind {
    /// 用于查询仓库、安装、卸载的可执行文件名。
    pub fn binary(self) -> &'static str {
        match self {
            PackageManagerKind::Pacman => "pacman",
            PackageManagerKind::Apt => "apt-get",
        }
    }

    /// 展示给用户的名字。
    pub fn name(self) -> &'static str {
        match self {
            PackageManagerKind::Pacman => "pacman",
            PackageManagerKind::Apt => "apt",
        }
    }

    /// 从状态文件里记录的名字还原包管理器。
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "pacman" => Some(PackageManagerKind::Pacman),
            "apt" => Some(PackageManagerKind::Apt),
            _ => None,
        }
    }

    /// 安装一组包的命令（需要 root）。
    pub fn install_commands(self, packages: &[String]) -> Vec<String> {
        if packages.is_empty() {
            return Vec::new();
        }
        let list = packages.join(" ");
        match self {
            PackageManagerKind::Pacman => {
                // --needed：已经装了就跳过；--noconfirm：不交互。
                vec![format!("pacman -S --needed --noconfirm {list}")]
            }
            PackageManagerKind::Apt => vec![format!(
                "DEBIAN_FRONTEND=noninteractive apt-get install -y {list}"
            )],
        }
    }

    /// 卸载一组包的命令（需要 root）。
    pub fn remove_commands(self, packages: &[String]) -> Vec<String> {
        if packages.is_empty() {
            return Vec::new();
        }
        let list = packages.join(" ");
        match self {
            PackageManagerKind::Pacman => {
                // -Rns：连同不再被需要的依赖一起清理。
                vec![format!("pacman -Rns --noconfirm {list}")]
            }
            PackageManagerKind::Apt => vec![format!(
                "DEBIAN_FRONTEND=noninteractive apt-get remove -y {list}"
            )],
        }
    }

    /// 查询某个包是否存在于软件源里（不需要 root）。
    pub fn package_query_args(self, package: &str) -> Vec<String> {
        match self {
            PackageManagerKind::Pacman => vec!["-Si".to_string(), package.to_string()],
            PackageManagerKind::Apt => vec!["show".to_string(), package.to_string()],
        }
    }

    /// 查询包是否存在时应该调用的可执行文件。
    fn query_binary(self) -> &'static str {
        match self {
            PackageManagerKind::Pacman => "pacman",
            PackageManagerKind::Apt => "apt-cache",
        }
    }

    /// 给“手动安装”用的提示命令。
    pub fn manual_hint(self, packages: &[String]) -> String {
        if packages.is_empty() {
            return String::new();
        }
        let list = packages.join(" ");
        match self {
            PackageManagerKind::Pacman => format!("sudo pacman -S {list}"),
            PackageManagerKind::Apt => format!("sudo apt install {list}"),
        }
    }
}

/// 某些发行版里命令名和包名不一致，这里做一层常见映射。
const PACMAN_ALIASES: &[(&str, &str)] = &[
    ("pip", "python-pip"),
    ("pip3", "python-pip"),
    ("python3", "python"),
    ("qmake", "qt5-base"),
    ("qmake6", "qt6-base"),
    ("cc", "gcc"),
    ("g++", "gcc"),
    ("pkg-config", "pkgconf"),
    ("ninja-build", "ninja"),
];

const APT_ALIASES: &[(&str, &str)] = &[
    ("pip", "python3-pip"),
    ("pip3", "python3-pip"),
    ("python", "python3"),
    ("ninja", "ninja-build"),
    ("qmake", "qt5-qmake"),
    ("qmake6", "qt6-base-dev"),
    ("cc", "gcc"),
    ("g++", "g++"),
];

/// 该命令名对应的候选包名（优先别名，其次命令名本身）。
pub fn candidate_packages(kind: PackageManagerKind, command: &str) -> Vec<String> {
    let table = match kind {
        PackageManagerKind::Pacman => PACMAN_ALIASES,
        PackageManagerKind::Apt => APT_ALIASES,
    };

    let mut out: Vec<String> = Vec::new();
    if let Some((_, package)) = table.iter().find(|(cmd, _)| *cmd == command) {
        out.push((*package).to_string());
    }
    if !out.iter().any(|p| p == command) {
        out.push(command.to_string());
    }
    out
}

/// 识别当前系统使用的包管理器。
///
/// 优先看 `/etc/os-release` 的 ID，再看可执行文件是否存在，
/// 这样在同时装了多个包管理器的机器上也能选对。
pub fn detect() -> Option<PackageManagerKind> {
    let os_id = std::fs::read_to_string("/etc/os-release")
        .ok()
        .and_then(|text| {
            text.lines()
                .find_map(|line| line.strip_prefix("ID="))
                .map(|id| id.trim().trim_matches('"').to_ascii_lowercase())
        });

    let detected = detect_from(
        os_id.as_deref(),
        paths::which("pacman").is_some(),
        paths::which("apt-get").is_some(),
    );
    log::debug!(
        "包管理器识别：os-release ID={:?}，结果={:?}",
        os_id,
        detected.map(|k| k.name())
    );
    detected
}

/// [`detect`] 的纯函数版本，便于测试。
pub fn detect_from(
    os_id: Option<&str>,
    has_pacman: bool,
    has_apt: bool,
) -> Option<PackageManagerKind> {
    let prefer_pacman = matches!(
        os_id,
        Some("arch") | Some("manjaro") | Some("endeavouros") | Some("cachyos") | Some("garuda")
    );
    let prefer_apt = matches!(
        os_id,
        Some("debian") | Some("ubuntu") | Some("linuxmint") | Some("pop") | Some("kali")
    );

    if prefer_pacman && has_pacman {
        return Some(PackageManagerKind::Pacman);
    }
    if prefer_apt && has_apt {
        return Some(PackageManagerKind::Apt);
    }
    if has_pacman {
        return Some(PackageManagerKind::Pacman);
    }
    if has_apt {
        return Some(PackageManagerKind::Apt);
    }
    None
}

/// 查询某个包在软件源里是否存在。
pub fn package_exists(kind: PackageManagerKind, package: &str) -> bool {
    let args = kind.package_query_args(package);
    log::debug!("查询软件源：{} {}", kind.query_binary(), args.join(" "));
    Command::new(kind.query_binary())
        .args(&args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

/// 单个缺失依赖的处理方案。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MissingDep {
    pub command: String,
    /// 将要安装的包名；`None` 表示无法自动安装。
    pub package: Option<String>,
    /// 无法自动安装的原因。
    pub reason: Option<String>,
}

/// 一个项目的依赖检查结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DepPlan {
    /// 安装前就已经存在、卸载时绝不能动的依赖。
    pub present: Vec<String>,
    pub missing: Vec<MissingDep>,
    pub manager: Option<PackageManagerKind>,
}

impl DepPlan {
    /// 是否所有缺失依赖都能自动安装。
    pub fn auto_installable(&self) -> bool {
        !self.missing.is_empty()
            && self.manager.is_some()
            && self.missing.iter().all(|m| m.package.is_some())
    }

    /// 需要自动安装的包名（去重、保序）。
    pub fn auto_packages(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for dep in &self.missing {
            if let Some(package) = &dep.package
                && !out.contains(package)
            {
                out.push(package.clone());
            }
        }
        out
    }

    /// 无法自动安装的依赖。
    pub fn blocked(&self) -> Vec<&MissingDep> {
        self.missing
            .iter()
            .filter(|m| m.package.is_none())
            .collect()
    }

    /// 给用户看的一句话总结。
    pub fn summary(&self) -> String {
        if self.missing.is_empty() {
            return "依赖齐全".to_string();
        }
        let names: Vec<&str> = self.missing.iter().map(|m| m.command.as_str()).collect();
        format!("缺少 {} 个依赖：{}", names.len(), names.join("、"))
    }
}

/// 检查一组依赖命令。
///
/// `package_exists` 用来判断某个包名是否在软件源里，注入它是为了便于测试。
pub fn plan_with<F>(commands: &[String], manager: Option<PackageManagerKind>, package_exists: F) -> DepPlan
where
    F: Fn(PackageManagerKind, &str) -> bool,
{
    let mut present = Vec::new();
    let mut missing = Vec::new();

    for command in commands {
        // 已经在 PATH 里 —— 属于“已经存在的依赖”，不安装也不删除。
        if paths::which(command).is_some() {
            present.push(command.clone());
            continue;
        }

        let package = manager.and_then(|kind| {
            candidate_packages(kind, command)
                .into_iter()
                .find(|candidate| package_exists(kind, candidate))
        });

        let reason = match (&package, manager) {
            (Some(_), _) => None,
            (None, None) => Some(
                "没有检测到受支持的包管理器（目前支持 pacman 与 apt），请手动安装".to_string(),
            ),
            (None, Some(kind)) => Some(format!(
                "在 {} 的软件源里没有找到与 `{command}` 对应的包，请手动安装",
                kind.name()
            )),
        };

        missing.push(MissingDep {
            command: command.clone(),
            package,
            reason,
        });
    }

    DepPlan {
        present,
        missing,
        manager,
    }
}

/// [`plan_with`] 的便捷版本，直接查询真实软件源。
pub fn plan(commands: &[String], manager: Option<PackageManagerKind>) -> DepPlan {
    plan_with(commands, manager, package_exists)
}

/// 把依赖名解析成可执行文件路径（界面展示用）。
pub fn resolve_command(command: &str) -> Option<PathBuf> {
    paths::which(command)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn always_exists(_kind: PackageManagerKind, _package: &str) -> bool {
        true
    }

    fn never_exists(_kind: PackageManagerKind, _package: &str) -> bool {
        false
    }

    fn only(package: &'static str) -> impl Fn(PackageManagerKind, &str) -> bool {
        move |_kind, candidate| candidate == package
    }

    #[test]
    fn detects_package_manager_from_os_release_first() {
        // 两个包管理器都在时，按发行版选择
        assert_eq!(
            detect_from(Some("arch"), true, true),
            Some(PackageManagerKind::Pacman)
        );
        assert_eq!(
            detect_from(Some("ubuntu"), true, true),
            Some(PackageManagerKind::Apt)
        );
        // 发行版不认识时，退回到“谁存在就用谁”
        assert_eq!(
            detect_from(None, false, true),
            Some(PackageManagerKind::Apt)
        );
        assert_eq!(
            detect_from(Some("fedora"), true, false),
            Some(PackageManagerKind::Pacman)
        );
        // 都没有
        assert_eq!(detect_from(Some("debian"), false, false), None);
        assert_eq!(detect_from(None, false, false), None);
    }

    #[test]
    fn generates_install_and_remove_commands() {
        let packages = vec!["cmake".to_string(), "ninja".to_string()];

        assert_eq!(
            PackageManagerKind::Pacman.install_commands(&packages),
            vec!["pacman -S --needed --noconfirm cmake ninja"]
        );
        assert_eq!(
            PackageManagerKind::Apt.install_commands(&packages),
            vec!["DEBIAN_FRONTEND=noninteractive apt-get install -y cmake ninja"]
        );
        assert_eq!(
            PackageManagerKind::Pacman.remove_commands(&packages),
            vec!["pacman -Rns --noconfirm cmake ninja"]
        );
        assert_eq!(
            PackageManagerKind::Apt.remove_commands(&packages),
            vec!["DEBIAN_FRONTEND=noninteractive apt-get remove -y cmake ninja"]
        );

        // 空列表不产生命令
        assert!(PackageManagerKind::Pacman.install_commands(&[]).is_empty());
        assert!(PackageManagerKind::Apt.remove_commands(&[]).is_empty());
    }

    #[test]
    fn maps_command_names_to_package_names() {
        // 别名优先，命令名本身作为兜底候选
        assert_eq!(
            candidate_packages(PackageManagerKind::Pacman, "pip3"),
            vec!["python-pip", "pip3"]
        );
        assert_eq!(
            candidate_packages(PackageManagerKind::Apt, "ninja"),
            vec!["ninja-build", "ninja"]
        );
        // 没有别名时只退回命令名本身
        assert_eq!(
            candidate_packages(PackageManagerKind::Apt, "cmake"),
            vec!["cmake"]
        );
        assert_eq!(
            candidate_packages(PackageManagerKind::Pacman, "make"),
            vec!["make"]
        );
        // 别名解析：第一个存在的候选被选中
        let plan = plan_with(
            &["pip3".to_string()],
            Some(PackageManagerKind::Pacman),
            only("python-pip"),
        );
        if !plan.missing.is_empty() {
            assert_eq!(plan.missing[0].package.as_deref(), Some("python-pip"));
        }
    }

    #[test]
    fn manual_hint_is_actionable() {
        let packages = vec!["cmake".to_string()];
        assert_eq!(
            PackageManagerKind::Pacman.manual_hint(&packages),
            "sudo pacman -S cmake"
        );
        assert_eq!(
            PackageManagerKind::Apt.manual_hint(&packages),
            "sudo apt install cmake"
        );
        assert!(PackageManagerKind::Apt.manual_hint(&[]).is_empty());
    }

    #[test]
    fn plan_separates_present_from_missing() {
        // sh 一定存在；另一个一定不存在
        let commands = vec!["sh".to_string(), "definitely-not-a-real-tool".to_string()];
        let plan = plan_with(&commands, Some(PackageManagerKind::Apt), always_exists);

        assert_eq!(plan.present, vec!["sh"]);
        assert_eq!(plan.missing.len(), 1);
        assert_eq!(plan.missing[0].command, "definitely-not-a-real-tool");
        assert_eq!(
            plan.missing[0].package.as_deref(),
            Some("definitely-not-a-real-tool")
        );
        assert!(plan.missing[0].reason.is_none());
        assert!(plan.auto_installable());
        assert_eq!(plan.auto_packages(), vec!["definitely-not-a-real-tool"]);
    }

    #[test]
    fn plan_explains_when_package_is_unknown() {
        let commands = vec!["definitely-not-a-real-tool".to_string()];
        let plan = plan_with(&commands, Some(PackageManagerKind::Pacman), never_exists);

        assert!(!plan.auto_installable());
        let reason = plan.missing[0].reason.as_deref().unwrap_or_default();
        assert!(reason.contains("pacman"), "{reason}");
        assert!(reason.contains("没有找到"), "{reason}");
        assert_eq!(plan.blocked().len(), 1);
    }

    #[test]
    fn plan_explains_when_no_package_manager() {
        let commands = vec!["definitely-not-a-real-tool".to_string()];
        let plan = plan_with(&commands, None, always_exists);

        assert!(!plan.auto_installable());
        let reason = plan.missing[0].reason.as_deref().unwrap_or_default();
        assert!(reason.contains("包管理器"), "{reason}");
        assert!(reason.contains("手动安装"), "{reason}");
    }

    #[test]
    fn plan_prefers_the_alias_when_it_exists() {
        let commands = vec!["definitely-not-a-real-tool".to_string()];
        // 只认别名对应的包（这里用 ninja -> ninja-build 验证优先级）
        let plan = plan_with(
            &["ninja".to_string()],
            Some(PackageManagerKind::Apt),
            only("ninja-build"),
        );
        // ninja 可能真的存在于系统里，那就无所谓了；只要解析到别名即可
        if plan.missing.is_empty() {
            return;
        }
        assert_eq!(plan.missing[0].package.as_deref(), Some("ninja-build"));
        let _ = commands;
    }

    #[test]
    fn summary_counts_missing_dependencies() {
        let commands = vec!["definitely-not-a-real-tool".to_string(), "sh".to_string()];
        let plan = plan_with(&commands, None, always_exists);
        assert!(plan.summary().contains("缺少 1 个依赖"), "{}", plan.summary());

        let clean = plan_with(&["sh".to_string()], None, always_exists);
        assert_eq!(clean.summary(), "依赖齐全");
    }

}
