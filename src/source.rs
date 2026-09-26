//! 源码目录：每个项目都克隆到 `<项目根>/sources/<id>` 下，互不干扰。
//!
//! 这里最要紧的一点是**不要相信「目录存在」就等于「克隆好了」**：
//! `git clone` 会先建出目标目录再往里签出文件，所以中途被取消或失败时，
//! 会留下一个存在但内容不全的目录。如果只按「目录非空」就跳过克隆，
//! 后续 `make` 就会在半个仓库里跑，报出“找不到 makefile”这种莫名其妙的错误。

use std::path::Path;
use std::process::Command;

use crate::exec::JobStep;
use crate::model::shell_quote;
use crate::paths;

/// 克隆成功后由我们自己写下的标记文件。
///
/// 有它就说明“这个目录是本管理器完整克隆过的”，可以直接复用。
pub const CLONE_MARKER: &str = ".w2l-clone-ok";

/// 已有源码时怎么处理。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloneState {
    /// 还没有源码，需要克隆。
    Fresh,
    /// 已经有完整源码，跳过克隆。
    Existing,
    /// 目录存在但没有可用的源码（上次克隆失败 / 被取消），先删掉再重新克隆。
    ReClone,
}

impl CloneState {
    /// 根据目录现状与用户选择决定克隆方式。
    pub fn decide(base: &Path, re_clone: bool) -> Self {
        if re_clone {
            return CloneState::ReClone;
        }
        if !base.exists() {
            return CloneState::Fresh;
        }
        if has_complete_sources(base) {
            CloneState::Existing
        } else {
            // 目录在，但源码不完整 —— 必须重新克隆，否则后面一定构建失败。
            CloneState::ReClone
        }
    }
}

/// 这个源码目录里是否已经有**完整**的源码可以拿来构建。
///
/// 判断顺序：
/// 1. 有我们自己写的标记文件 → 完整；
/// 2. 是个 git 仓库 → 看签出是否完整（索引里的文件是否都在工作区）；
/// 3. 其它情况 → 认为不完整（宁可重新克隆，也不要在半成品上构建）。
pub fn has_complete_sources(base: &Path) -> bool {
    if !base.is_dir() {
        return false;
    }
    if base.join(CLONE_MARKER).is_file() {
        return true;
    }

    let root = paths::resolve_source_root(base);
    if !root.join(".git").exists() {
        return false;
    }
    is_git_checkout_complete(&root)
}

/// git 仓库的签出是否完整。
///
/// 只看「索引里有、工作区缺失」的文件（`git status --porcelain` 里第二列为 `D`），
/// 因为那正是克隆没做完的特征；用户自己改过的文件（`M`）不算数，免得把改动删掉。
pub fn is_git_checkout_complete(root: &Path) -> bool {
    if paths::which("git").is_none() {
        return false;
    }

    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["status", "--porcelain", "--untracked-files=no"])
        .output();

    match output {
        Ok(output) if output.status.success() => {
            let text = String::from_utf8_lossy(&output.stdout);
            !text.lines().any(|line| {
                let mut chars = line.chars();
                let _staged = chars.next();
                matches!(chars.next(), Some('D'))
            })
        }
        _ => false,
    }
}

/// 生成「准备源码」的步骤。
///
/// 所有命令都使用绝对路径，`mkdir` / `rm` 在项目根目录里执行，
/// 克隆命令则在**该项目的源码目录**里执行，这样仓库既不会污染项目根目录，
/// 不同项目之间也不会互相覆盖。克隆成功后写下 [`CLONE_MARKER`]。
pub fn clone_steps(
    project_root: &Path,
    base: &Path,
    clone_command: &str,
    state: CloneState,
) -> Vec<JobStep> {
    let base_arg = shell_quote(&base.display().to_string());
    let marker_arg = shell_quote(&base.join(CLONE_MARKER).display().to_string());

    match state {
        CloneState::Existing => vec![
            JobStep::user(format!("echo 源码已完整，跳过克隆：{base_arg}"))
                .in_dir(project_root.to_path_buf()),
        ],
        CloneState::Fresh => vec![
            JobStep::user(format!("mkdir -p {base_arg}")).in_dir(project_root.to_path_buf()),
            JobStep::user(clone_command.to_string()).in_dir(base.to_path_buf()),
            JobStep::user(format!("touch {marker_arg}")).in_dir(base.to_path_buf()),
        ],
        CloneState::ReClone => vec![
            JobStep::user(format!("rm -rf {base_arg}")).in_dir(project_root.to_path_buf()),
            JobStep::user(format!("mkdir -p {base_arg}")).in_dir(project_root.to_path_buf()),
            JobStep::user(clone_command.to_string()).in_dir(base.to_path_buf()),
            JobStep::user(format!("touch {marker_arg}")).in_dir(base.to_path_buf()),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::WorkDir;
    use std::fs;
    use std::path::PathBuf;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "w2l-src-{tag}-{}-{}",
            std::process::id(),
            crate::state::now_secs()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn base() -> PathBuf {
        PathBuf::from("/tmp/proj/sources/demo")
    }

    fn git_available() -> bool {
        paths::which("git").is_some()
    }

    /// 造一个真实的 git 仓库（带一次提交）。
    fn init_repo(dir: &Path) {
        fs::create_dir_all(dir).unwrap();
        let run = |args: &[&str]| {
            let status = Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(args)
                .output()
                .expect("git 应该能运行");
            assert!(
                status.status.success(),
                "git {args:?} 失败：{}",
                String::from_utf8_lossy(&status.stderr)
            );
        };
        run(&["init", "-q"]);
        fs::write(dir.join("Makefile"), "all:\n\t@echo built\n").unwrap();
        fs::write(dir.join("main.c"), "int main(void){return 0;}\n").unwrap();
        run(&["add", "."]);
        run(&[
            "-c",
            "user.email=test@example.com",
            "-c",
            "user.name=test",
            "commit",
            "-qm",
            "init",
        ]);
    }

    #[test]
    fn fresh_clone_creates_the_dir_then_clones_inside_it() {
        let steps = clone_steps(
            Path::new("/tmp/proj"),
            &base(),
            "git clone https://example.com/demo.git",
            CloneState::Fresh,
        );
        assert_eq!(steps.len(), 3);

        assert!(steps[0].command.starts_with("mkdir -p "));
        assert_eq!(
            steps[0].work_dir,
            Some(WorkDir::Fixed(PathBuf::from("/tmp/proj")))
        );

        assert_eq!(steps[1].command, "git clone https://example.com/demo.git");
        assert_eq!(steps[1].work_dir, Some(WorkDir::Fixed(base())));
        assert!(!steps[1].needs_root);

        // 克隆成功后才写标记
        assert!(steps[2].command.starts_with("touch "));
        assert!(
            steps[2].command.contains(CLONE_MARKER),
            "{:?}",
            steps[2].command
        );
    }

    #[test]
    fn existing_sources_skip_cloning() {
        let steps = clone_steps(
            Path::new("/tmp/proj"),
            &base(),
            "git clone x",
            CloneState::Existing,
        );
        assert_eq!(steps.len(), 1);
        assert!(steps[0].command.starts_with("echo "));
        assert!(!steps[0].command.contains("git clone"));
    }

    #[test]
    fn reclone_removes_then_clones() {
        let steps = clone_steps(
            Path::new("/tmp/proj"),
            &base(),
            "git clone x",
            CloneState::ReClone,
        );
        assert_eq!(steps.len(), 4);
        assert!(steps[0].command.starts_with("rm -rf "));
        assert!(steps[1].command.starts_with("mkdir -p "));
        assert_eq!(steps[2].command, "git clone x");
        assert!(steps[3].command.starts_with("touch "));
    }

    #[test]
    fn paths_with_spaces_are_quoted() {
        let weird = PathBuf::from("/tmp/my proj/sources/demo");
        let steps = clone_steps(
            Path::new("/tmp/my proj"),
            &weird,
            "git clone x",
            CloneState::Fresh,
        );
        assert!(
            steps[0].command.contains("'/tmp/my proj/sources/demo'"),
            "{:?}",
            steps[0]
        );
    }

    /// 回归测试：目录存在但只签出了一半时，绝不能当成“已克隆好”。
    /// 用户遇到的就是这个：`git clone` 先建目录、再慢慢签出文件，
    /// 中途被取消后 `make` 在半个仓库里跑，报“找不到 makefile”。
    #[test]
    fn partially_checked_out_repo_is_not_treated_as_complete() {
        if !git_available() {
            eprintln!("跳过：没有 git");
            return;
        }

        let root = temp_dir("partial");
        let base = root.join("sources").join("demo");
        let repo = base.join("cmd");
        init_repo(&repo);

        // 完整签出 → 可以复用
        assert!(has_complete_sources(&base), "完整仓库应被认作可用");
        assert_eq!(CloneState::decide(&base, false), CloneState::Existing);

        // 模拟“克隆只做了一半”：删掉工作区里的文件（索引里还在）
        fs::remove_file(repo.join("Makefile")).unwrap();
        assert!(!has_complete_sources(&base), "签出不完整时不该被认作可用");
        assert_eq!(
            CloneState::decide(&base, false),
            CloneState::ReClone,
            "不完整时必须重新克隆"
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn empty_or_foreign_directories_are_recloned() {
        let root = temp_dir("empty");

        // 目录不存在 → Fresh
        let missing = root.join("missing");
        assert_eq!(CloneState::decide(&missing, false), CloneState::Fresh);

        // 空目录（mkdir 建出来但克隆没跑）→ 重新克隆
        let empty = root.join("empty");
        fs::create_dir_all(&empty).unwrap();
        assert!(!has_complete_sources(&empty));
        assert_eq!(CloneState::decide(&empty, false), CloneState::ReClone);

        // 只有一层空子目录（克隆刚开始）→ 重新克隆
        let half = root.join("half");
        fs::create_dir_all(half.join("cmd")).unwrap();
        assert!(!has_complete_sources(&half));
        assert_eq!(CloneState::decide(&half, false), CloneState::ReClone);

        // 不是 git 仓库、也没写过标记 → 重新克隆
        let plain = root.join("plain");
        fs::create_dir_all(&plain).unwrap();
        fs::write(plain.join("Makefile"), "all:\n").unwrap();
        assert!(!has_complete_sources(&plain));
        assert_eq!(CloneState::decide(&plain, false), CloneState::ReClone);

        // 有标记文件 → 直接复用
        let marked = root.join("marked");
        fs::create_dir_all(&marked).unwrap();
        fs::write(marked.join(CLONE_MARKER), "").unwrap();
        assert!(has_complete_sources(&marked));
        assert_eq!(CloneState::decide(&marked, false), CloneState::Existing);

        // 用户要求重新克隆时优先级最高
        assert_eq!(CloneState::decide(&marked, true), CloneState::ReClone);

        let _ = fs::remove_dir_all(&root);
    }

    /// 用户自己改过文件不该被当成“没克隆好”，否则重新克隆会删掉他的改动。
    #[test]
    fn local_modifications_do_not_look_like_an_incomplete_checkout() {
        if !git_available() {
            eprintln!("跳过：没有 git");
            return;
        }

        let root = temp_dir("modified");
        let repo = root.join("repo");
        init_repo(&repo);

        fs::write(repo.join("Makefile"), "all:\n\t@echo changed\n").unwrap();
        assert!(
            has_complete_sources(&root),
            "只是改了文件，仓库仍然是完整的"
        );

        let _ = fs::remove_dir_all(&root);
    }
}
