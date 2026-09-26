//! 把「装依赖 → 准备源码 → 项目命令」组装成一次任务要执行的步骤序列。

use std::path::{Path, PathBuf};

use crate::deps::PackageManagerKind;
use crate::exec::JobStep;
use crate::model::{self, CommandSpec, ProjectEntry};
use crate::paths;
use crate::source::{self, CloneState};

/// 组装好的任务。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedJob {
    pub steps: Vec<JobStep>,
    /// 该项目源码所在目录；`None` 表示不需要克隆。
    pub source_base: Option<PathBuf>,
}

impl PlannedJob {
    /// 需要 root 的步骤数。
    pub fn root_steps(&self) -> usize {
        self.steps.iter().filter(|s| s.needs_root).count()
    }

    /// 会把源码放到哪里。
    pub fn source_dir(&self) -> Option<&Path> {
        self.source_base.as_deref()
    }
}

/// 安装：先补齐缺失依赖，再把源码克隆到独立目录，最后执行安装命令。
pub fn install_steps(
    project_root: &Path,
    entry: &ProjectEntry,
    auto_packages: &[String],
    manager: Option<PackageManagerKind>,
    clone_state: CloneState,
) -> PlannedJob {
    let mut steps = Vec::new();

    // 1) 依赖（需要 root，失败则整个安装中止）
    if let Some(kind) = manager {
        for command in kind.install_commands(auto_packages) {
            steps.push(JobStep::root(command));
        }
    }

    // 2) 源码：每个项目一个独立目录
    let source_base = entry
        .needs_clone()
        .then(|| paths::project_source_base(project_root, &entry.id));
    if let (Some(base), Some(clone_command)) = (&source_base, &entry.clone_command) {
        steps.extend(source::clone_steps(
            project_root,
            base,
            clone_command,
            clone_state,
        ));
    }

    // 3) 项目自己的安装命令，权限按 JSON 的声明
    for spec in &entry.install_commands {
        steps.push(project_step(spec, source_base.is_some()));
    }

    PlannedJob {
        steps,
        source_base,
    }
}

/// 卸载：先跑卸载命令，再按需清理依赖与源码目录。
pub fn uninstall_steps(
    project_root: &Path,
    entry: &ProjectEntry,
    commands: &[CommandSpec],
    remove_packages: &[String],
    manager: Option<PackageManagerKind>,
    remove_sources: bool,
) -> PlannedJob {
    let source_base = entry
        .needs_clone()
        .then(|| paths::project_source_base(project_root, &entry.id));

    let mut steps: Vec<JobStep> = commands
        .iter()
        .map(|spec| project_step(spec, source_base.is_some()))
        .collect();

    // 包可能已经被手动删掉了，这一步失败不该让整个卸载判定为失败
    if let Some(kind) = manager {
        for command in kind.remove_commands(remove_packages) {
            steps.push(JobStep::root(command).optional());
        }
    }

    // 源码目录是本管理器自己建的，卸载时可以顺手删掉
    if remove_sources && let Some(base) = &source_base {
        steps.push(
            JobStep::user(format!(
                "rm -rf {}",
                model::shell_quote(&base.display().to_string())
            ))
            .in_dir(project_root.to_path_buf())
            .optional(),
        );
    }

    PlannedJob {
        steps,
        source_base,
    }
}

/// 把 JSON 里的命令声明变成一步任务：权限照搬，有源码目录时在源码根目录里执行。
fn project_step(spec: &CommandSpec, in_source: bool) -> JobStep {
    let step = if spec.permission().needs_root() {
        JobStep::root(spec.command())
    } else {
        JobStep::user(spec.command())
    };
    if in_source {
        step.in_project_source()
    } else {
        step
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::WorkDir;
    use crate::model::Permission;

    fn entry(with_clone: bool) -> ProjectEntry {
        ProjectEntry {
            id: "demo".to_string(),
            clone_command: with_clone.then(|| "git clone https://example.com/demo.git".to_string()),
            install_commands: vec![
                CommandSpec::normal("make"),
                CommandSpec::new("make install PREFIX=/usr/local", Permission::Root),
            ],
            uninstall_commands: vec![CommandSpec::new("rm -f /usr/local/bin/demo", Permission::Root)],
            ..Default::default()
        }
    }

    #[test]
    fn install_clones_into_a_per_project_dir_then_builds_there() {
        let plan = install_steps(
            Path::new("/tmp/proj"),
            &entry(true),
            &[],
            None,
            CloneState::Fresh,
        );

        assert_eq!(
            plan.source_dir(),
            Some(Path::new("/tmp/proj/sources/demo"))
        );

        let commands: Vec<&str> = plan.steps.iter().map(|s| s.command.as_str()).collect();
        assert!(commands[0].starts_with("mkdir -p /tmp/proj/sources/demo"), "{commands:?}");
        assert_eq!(commands[1], "git clone https://example.com/demo.git");
        // 克隆成功后才写“已完成”标记，下次安装才敢跳过克隆
        assert!(commands[2].contains("w2l-clone-ok"), "{commands:?}");
        assert_eq!(commands[3], "make");
        assert_eq!(commands[4], "make install PREFIX=/usr/local");

        // 克隆（含写标记）都在源码目录里执行
        let source_dir = Some(WorkDir::Fixed(PathBuf::from("/tmp/proj/sources/demo")));
        assert_eq!(plan.steps[1].work_dir, source_dir);
        assert_eq!(plan.steps[2].work_dir, source_dir);
        // 项目命令在（克隆后解析出来的）源码根目录里执行
        assert_eq!(plan.steps[3].work_dir, Some(WorkDir::ProjectSource));
        assert_eq!(plan.steps[4].work_dir, Some(WorkDir::ProjectSource));

        // 权限来自 JSON
        assert!(!plan.steps[3].needs_root);
        assert!(plan.steps[4].needs_root);
        assert_eq!(plan.root_steps(), 1);
    }

    #[test]
    fn install_without_clone_uses_the_working_directory() {
        let plan = install_steps(
            Path::new("/tmp/proj"),
            &entry(false),
            &[],
            None,
            CloneState::Fresh,
        );
        assert!(plan.source_dir().is_none());
        assert_eq!(plan.steps.len(), 2);
        assert!(plan.steps.iter().all(|s| s.work_dir.is_none()));
    }

    #[test]
    fn dependencies_are_installed_before_cloning() {
        let plan = install_steps(
            Path::new("/tmp/proj"),
            &entry(true),
            &["cmake".to_string()],
            Some(PackageManagerKind::Pacman),
            CloneState::Fresh,
        );
        assert!(plan.steps[0].needs_root, "先装依赖");
        assert!(plan.steps[0].command.starts_with("pacman -S"));
        assert!(
            plan.steps[1].command.starts_with("mkdir -p "),
            "再准备源码：{:?}",
            plan.steps[1]
        );
    }

    #[test]
    fn uninstall_runs_in_source_then_cleans_up() {
        let e = entry(true);
        let plan = uninstall_steps(
            Path::new("/tmp/proj"),
            &e,
            &e.uninstall_commands,
            &["cmake".to_string()],
            Some(PackageManagerKind::Apt),
            true,
        );

        let commands: Vec<&str> = plan.steps.iter().map(|s| s.command.as_str()).collect();
        assert_eq!(commands[0], "rm -f /usr/local/bin/demo");
        assert!(commands[1].contains("apt-get remove"), "{commands:?}");
        assert!(commands[2].starts_with("rm -rf /tmp/proj/sources/demo"), "{commands:?}");

        // 卸载命令在源码目录里跑（源码被删掉时执行器会退回默认目录）
        assert_eq!(plan.steps[0].work_dir, Some(WorkDir::ProjectSource));
        assert!(plan.steps[0].needs_root);
        // 清理步骤失败不应中断
        assert!(plan.steps[1].optional);
        assert!(plan.steps[2].optional);
        assert!(!plan.steps[2].needs_root);
    }

    #[test]
    fn uninstall_can_keep_the_sources() {
        let e = entry(true);
        let plan = uninstall_steps(
            Path::new("/tmp/proj"),
            &e,
            &e.uninstall_commands,
            &[],
            None,
            false,
        );
        assert_eq!(plan.steps.len(), 1);
        assert!(!plan.steps.iter().any(|s| s.command.starts_with("rm -rf")));
    }
}
