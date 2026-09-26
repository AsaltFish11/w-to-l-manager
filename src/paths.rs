//! 路径定位：项目根目录、`project_list.json`、状态文件，以及 PATH 探测。

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

/// `project_list.json` 的固定文件名。
pub const PROJECT_LIST_FILE: &str = "project_list.json";

/// 定位 `project_list.json`。
///
/// 顺序：`W2L_PROJECT_LIST` 环境变量 → 当前目录向上查找 → 可执行文件所在目录
/// 向上查找 → 编译期项目目录 → 当前目录下的默认文件名。
pub fn locate_project_list() -> PathBuf {
    if let Some(raw) = env::var_os("W2L_PROJECT_LIST") {
        let candidate = PathBuf::from(raw);
        if candidate.is_file() {
            return candidate;
        }
    }

    if let Ok(cwd) = env::current_dir()
        && let Some(found) = search_upward(&cwd)
    {
        return found;
    }

    if let Ok(exe) = env::current_exe()
        && let Some(dir) = exe.parent()
        && let Some(found) = search_upward(dir)
    {
        return found;
    }

    let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join(PROJECT_LIST_FILE);
    if manifest.is_file() {
        return manifest;
    }

    env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join(PROJECT_LIST_FILE)
}

fn search_upward(start: &Path) -> Option<PathBuf> {
    start
        .ancestors()
        .map(|dir| dir.join(PROJECT_LIST_FILE))
        .find(|candidate| candidate.is_file())
}

/// 把用户输入的路径展开为绝对路径（支持 `~` 与相对路径）。
pub fn resolve_input_path(input: &str) -> PathBuf {
    let trimmed = input.trim();
    let expanded = if trimmed == "~" {
        home_dir().unwrap_or_else(|| PathBuf::from(trimmed))
    } else if let Some(rest) = trimmed.strip_prefix("~/") {
        match home_dir() {
            Some(home) => home.join(rest),
            None => PathBuf::from(trimmed),
        }
    } else {
        PathBuf::from(trimmed)
    };

    if expanded.is_absolute() {
        expanded
    } else {
        env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(expanded)
    }
}

fn home_dir() -> Option<PathBuf> {
    env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
}

/// 日志文件名。
pub const LOG_FILE_NAME: &str = "w-to-l-manager.log";

/// 日志文件的候选路径，按优先级排列。
///
/// 优先放在用户状态目录（`$XDG_STATE_HOME` / `~/.local/state`），
/// 写不了再退回项目目录；两者都可以用 `W2L_LOG_FILE` 覆盖。
pub fn log_file_candidates(project_root: &Path) -> Vec<PathBuf> {
    if let Some(raw) = env::var_os("W2L_LOG_FILE")
        && !raw.is_empty()
    {
        return vec![PathBuf::from(raw)];
    }

    let mut out = Vec::new();
    let state_home = env::var_os("XDG_STATE_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| home_dir().map(|h| h.join(".local").join("state")));

    if let Some(dir) = state_home {
        out.push(dir.join("w-to-l-manager").join(LOG_FILE_NAME));
    }
    out.push(project_root.join(format!(".{LOG_FILE_NAME}")));
    out
}

/// 状态文件的候选路径，按优先级排列：主路径写失败时回退到项目目录。
pub fn state_file_candidates(project_root: &Path) -> Vec<PathBuf> {
    if let Some(raw) = env::var_os("W2L_STATE_FILE")
        && !raw.is_empty()
    {
        return vec![PathBuf::from(raw)];
    }

    let mut out = Vec::new();
    let data_home = env::var_os("XDG_DATA_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| home_dir().map(|h| h.join(".local").join("share")));

    if let Some(dir) = data_home {
        out.push(
            dir.join("w-to-l-manager")
                .join(crate::state::STATE_FILE_NAME),
        );
    }
    out.push(project_root.join(".w2l-manager-state.json"));
    out
}

/// 在 `PATH` 中查找可执行文件。
pub fn which(program: &str) -> Option<PathBuf> {
    let program = program.trim();
    if program.is_empty() {
        return None;
    }

    let as_path = Path::new(program);
    if as_path.components().count() > 1 {
        return is_executable(as_path).then(|| as_path.to_path_buf());
    }

    let path_var = env::var_os("PATH")?;
    env::split_paths(&path_var)
        .map(|dir| dir.join(program))
        .find(|candidate| is_executable(candidate))
}

fn is_executable(path: &Path) -> bool {
    match fs::metadata(path) {
        Ok(meta) => {
            if !meta.is_file() {
                return false;
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                meta.permissions().mode() & 0o111 != 0
            }
            #[cfg(not(unix))]
            {
                true
            }
        }
        Err(_) => false,
    }
}

/// 存放各项目源码的目录名（位于项目根目录下）。
pub const SOURCES_DIR_NAME: &str = "sources";

/// 某个项目的源码基目录：`<项目根>/sources/<id>`。
///
/// 每个项目一个独立目录，互不干扰。
pub fn project_source_base(project_root: &Path, project_id: &str) -> PathBuf {
    project_root
        .join(SOURCES_DIR_NAME)
        .join(safe_dir_name(project_id))
}

/// 把 id 里可能造成目录穿越的字符替换掉。
pub fn safe_dir_name(project_id: &str) -> String {
    let cleaned: String = project_id
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let trimmed = cleaned.trim_matches('.').to_string();
    if trimmed.is_empty() {
        "project".to_string()
    } else {
        trimmed
    }
}

/// 基目录里是否已经有内容（用来判断要不要再克隆一次）。
pub fn has_sources(base: &Path) -> bool {
    match fs::read_dir(base) {
        Ok(mut entries) => entries.next().is_some(),
        Err(_) => false,
    }
}

/// 克隆结束后判断真正的源码根目录。
///
/// `git clone <url>` 会在基目录里再建一层以仓库名命名的目录，
/// 而 `git clone <url> .` 则会把文件直接铺在基目录里。这里的规则是：
/// 基目录下**只有一个目录**（忽略隐藏项）时用那个目录，否则就用基目录本身。
pub fn resolve_source_root(base: &Path) -> PathBuf {
    let Ok(entries) = fs::read_dir(base) else {
        return base.to_path_buf();
    };

    let mut only_dir: Option<PathBuf> = None;
    let mut has_loose_file = false;

    for entry in entries.flatten() {
        let name = entry.file_name();
        if name.to_string_lossy().starts_with('.') {
            continue; // 跳过 .git 之类的隐藏项
        }
        let Ok(kind) = entry.file_type() else { continue };
        if kind.is_dir() {
            if only_dir.is_some() {
                return base.to_path_buf(); // 多个目录，无法判断，就用基目录
            }
            only_dir = Some(entry.path());
        } else {
            has_loose_file = true;
        }
    }

    match (only_dir, has_loose_file) {
        (Some(dir), false) => dir,
        _ => base.to_path_buf(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locates_the_real_project_list() {
        let found = locate_project_list();
        assert!(
            found.is_file(),
            "应能在项目目录中找到 project_list.json，实际得到 {found:?}"
        );
        assert_eq!(
            found.file_name().and_then(|n| n.to_str()),
            Some(PROJECT_LIST_FILE)
        );
    }

    #[test]
    fn resolves_relative_and_home_paths() {
        assert!(resolve_input_path("project_list.json").is_absolute());
        if env::var_os("HOME").is_some() {
            assert!(resolve_input_path("~/x.json").is_absolute());
        }
    }

    #[test]
    fn which_finds_basic_tools() {
        assert!(which("sh").is_some());
        assert!(which("definitely-not-a-real-binary-xyz").is_none());
        assert!(which("   ").is_none());
    }

    #[test]
    fn source_base_is_per_project_and_cannot_escape() {
        let root = Path::new("/tmp/proj");
        assert_eq!(
            project_source_base(root, "ChenPi11_cmd"),
            PathBuf::from("/tmp/proj/sources/ChenPi11_cmd")
        );
        // 不同项目互不干扰
        assert_ne!(
            project_source_base(root, "a"),
            project_source_base(root, "b")
        );
        // 目录穿越被挡住
        let escaped = project_source_base(root, "../../etc");
        assert!(escaped.starts_with("/tmp/proj/sources"), "{escaped:?}");
        // 不能出现向上跳的路径分量
        assert!(
            !escaped
                .components()
                .any(|c| c.as_os_str() == std::ffi::OsStr::new("..")),
            "{escaped:?}"
        );
        assert_eq!(
            escaped.file_name().and_then(|n| n.to_str()),
            Some("_.._etc"),
            "{escaped:?}"
        );
        assert_eq!(safe_dir_name(".."), "project");
        assert_eq!(safe_dir_name(""), "project");
        assert_eq!(safe_dir_name("a/b"), "a_b");
    }

    #[test]
    fn resolves_the_real_source_root_after_cloning() {
        let base = std::env::temp_dir().join(format!("w2l-resolve-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).unwrap();

        // 空目录 → 基目录本身
        assert_eq!(resolve_source_root(&base), base);
        assert!(!has_sources(&base));

        // 只有一个仓库目录（外加隐藏的 .git）→ 那个目录
        fs::create_dir_all(base.join(".git")).unwrap();
        fs::create_dir_all(base.join("cmd")).unwrap();
        assert_eq!(resolve_source_root(&base), base.join("cmd"));
        assert!(has_sources(&base));

        // 直接铺在基目录里的文件 → 基目录本身
        let flat = base.join("flat");
        fs::create_dir_all(&flat).unwrap();
        fs::write(flat.join("Makefile"), "all:").unwrap();
        assert_eq!(resolve_source_root(&flat), flat);

        // 多个目录 → 基目录本身
        let many = base.join("many");
        fs::create_dir_all(many.join("x")).unwrap();
        fs::create_dir_all(many.join("y")).unwrap();
        assert_eq!(resolve_source_root(&many), many);

        fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn state_candidates_are_non_empty() {
        let list = state_file_candidates(Path::new("/tmp/proj"));
        assert!(!list.is_empty());
        assert!(list.iter().all(|p| p.is_absolute()));
    }
}
