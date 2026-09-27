//! 源码目录：每个项目都克隆到 `<项目根>/sources/<id>` 下，互不干扰。
//!
//! 这里最要紧的一点是**不要相信「目录存在」就等于「克隆好了」**：
//! `git clone` 会先建出目标目录再往里签出文件，所以中途被取消或失败时，
//! 会留下一个存在但内容不全的目录。如果只按「目录非空」就跳过克隆，
//! 后续 `make` 就会在半个仓库里跑，报出“找不到 makefile”这种莫名其妙的错误。

use std::path::Path;

use crate::exec::JobStep;
use crate::model::shell_quote;

/// 克隆成功后由我们自己写下的「完整性标记」文件。
///
/// **它是否存在，是判断源码能不能复用的唯一依据**：
/// 在 → 这个目录是一次完整的克隆，可以直接构建；
/// 不在 → 上次克隆没有跑完（失败 / 被取消 / 被删掉），目录里的东西不可信，
/// 必须先删掉整个目录再重新克隆。
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

/// 这个源码目录里是不是已经有一次**完整**的克隆。
///
/// 判据只有一个：克隆成功之后写下的 [`CLONE_MARKER`] 在不在。
/// 不看目录是否非空 —— `git clone` 是先建目录再签出文件的，
/// 只签出了一半的目录同样“非空”，拿它去构建只会得到莫名其妙的错误。
pub fn has_complete_sources(base: &Path) -> bool {
    base.join(CLONE_MARKER).is_file()
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

    /// 标记必须排在克隆步骤**之后**：克隆失败或被取消时任务会中断，
    /// 标记就不会被写下来，下次安装才知道要重来。
    #[test]
    fn the_marker_is_written_only_after_the_clone() {
        for state in [CloneState::Fresh, CloneState::ReClone] {
            let steps = clone_steps(Path::new("/tmp/proj"), &base(), "git clone x", state);
            let clone_at = steps
                .iter()
                .position(|s| s.command == "git clone x")
                .expect("应该有克隆步骤");
            let marker_at = steps
                .iter()
                .position(|s| s.command.contains(CLONE_MARKER))
                .unwrap_or_else(|| panic!("{state:?} 应该写标记"));
            assert!(clone_at < marker_at, "{state:?}：标记必须在克隆之后");
            assert_eq!(marker_at, steps.len() - 1, "标记应该是最后一步");
        }

        // 复用已有源码时不会碰标记
        let steps = clone_steps(
            Path::new("/tmp/proj"),
            &base(),
            "git clone x",
            CloneState::Existing,
        );
        assert!(!steps.iter().any(|s| s.command.contains(CLONE_MARKER)));
    }

    /// 核心规则：**只有标记文件**能证明这是一次完整的克隆。
    #[test]
    fn only_the_marker_proves_the_clone_is_complete() {
        let root = temp_dir("marker");

        // 目录不存在 → 全新克隆
        let missing = root.join("missing");
        assert!(!has_complete_sources(&missing));
        assert_eq!(CloneState::decide(&missing, false), CloneState::Fresh);

        // 目录在但没标记（上次克隆只建出目录就被取消了）→ 重来
        let empty = root.join("empty");
        fs::create_dir_all(&empty).unwrap();
        assert!(!has_complete_sources(&empty));
        assert_eq!(CloneState::decide(&empty, false), CloneState::ReClone);

        // 只签出了一半 → 重来
        let half = root.join("half");
        fs::create_dir_all(half.join("cmd")).unwrap();
        assert!(!has_complete_sources(&half));
        assert_eq!(CloneState::decide(&half, false), CloneState::ReClone);

        // 看起来“像”个完整仓库，但只要没有标记，一样重来
        let looks_done = root.join("looks-done");
        fs::create_dir_all(looks_done.join("cmd").join(".git")).unwrap();
        fs::write(looks_done.join("cmd").join("Makefile"), "all:\n").unwrap();
        assert!(
            !has_complete_sources(&looks_done),
            "没有标记就不算完整，哪怕里面已经有 Makefile"
        );
        assert_eq!(CloneState::decide(&looks_done, false), CloneState::ReClone);

        // 有标记 → 直接复用
        let done = root.join("done");
        fs::create_dir_all(&done).unwrap();
        fs::write(done.join(CLONE_MARKER), "").unwrap();
        assert!(has_complete_sources(&done));
        assert_eq!(CloneState::decide(&done, false), CloneState::Existing);

        // 用户勾了「重新克隆」时优先级最高
        assert_eq!(CloneState::decide(&done, true), CloneState::ReClone);

        let _ = fs::remove_dir_all(&root);
    }
}
