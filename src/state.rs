//! 安装状态的持久化：记录哪些条目已安装、何时安装、用什么命令安装。

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub const STATE_FILE_NAME: &str = "state.json";

/// 由本管理器自动安装的一个依赖。
///
/// 只有这里记录下来的依赖，卸载项目时才会询问是否一并移除；
/// 安装前就已存在的依赖永远不会被本管理器删除。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AutoDep {
    /// `project_list.json` 里写的依赖命令名。
    pub command: String,
    /// 实际安装的包名。
    pub package: String,
    /// 使用的包管理器（`pacman` / `apt`）。
    pub manager: String,
    pub installed_at: u64,
}

/// 一个项目的依赖记录。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DepRecord {
    /// 安装前就已经存在（或根本不需要装）的依赖，卸载时不动。
    #[serde(default)]
    pub pre_existing: Vec<String>,
    /// 由本管理器自动安装的依赖。
    #[serde(default)]
    pub auto_installed: Vec<AutoDep>,
}

impl DepRecord {
    pub fn is_empty(&self) -> bool {
        self.pre_existing.is_empty() && self.auto_installed.is_empty()
    }

    /// 自动安装过的包名（去重、保序）。
    pub fn auto_packages(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for dep in &self.auto_installed {
            if !out.contains(&dep.package) {
                out.push(dep.package.clone());
            }
        }
        out
    }

    /// 自动安装依赖时用的包管理器（取第一条记录）。
    pub fn manager_name(&self) -> Option<&str> {
        self.auto_installed.first().map(|d| d.manager.as_str())
    }
}

/// 单个条目的安装记录。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstallRecord {
    /// 安装完成时间（Unix 秒）。
    #[serde(default)]
    pub installed_at: u64,
    /// 实际执行成功的安装命令。
    #[serde(default)]
    pub commands: Vec<String>,
    /// 源码被克隆到哪里（`<项目根>/sources/<id>`）；不克隆的项目为 `None`。
    #[serde(default)]
    pub source_dir: Option<String>,
    /// 依赖记录。老版本状态文件没有这个字段，缺省为空。
    #[serde(default)]
    pub dependencies: DepRecord,
}

/// 管理器状态文件的内容。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManagerState {
    /// id → 安装记录。存在即视为“已安装”。
    #[serde(default)]
    pub installed: BTreeMap<String, InstallRecord>,
    /// 状态文件的格式版本，便于以后迁移。
    #[serde(default = "default_version")]
    pub version: u32,
}

fn default_version() -> u32 {
    2
}

impl ManagerState {
    pub fn is_installed(&self, id: &str) -> bool {
        self.installed.contains_key(id)
    }

    pub fn mark_installed(
        &mut self,
        id: &str,
        commands: Vec<String>,
        source_dir: Option<String>,
        dependencies: DepRecord,
    ) {
        self.installed.insert(
            id.to_string(),
            InstallRecord {
                installed_at: now_secs(),
                commands,
                source_dir,
                dependencies,
            },
        );
    }

    pub fn mark_uninstalled(&mut self, id: &str) {
        self.installed.remove(id);
    }

    /// 载入状态；文件不存在时返回空状态。第二个返回值是警告信息。
    pub fn load(path: &Path) -> (Self, Option<String>) {
        match fs::read_to_string(path) {
            Ok(text) => match serde_json::from_str::<ManagerState>(&text) {
                Ok(state) => (state, None),
                Err(err) => (
                    ManagerState::default(),
                    Some(format!("状态文件 {} 解析失败：{err}", path.display())),
                ),
            },
            Err(err) if err.kind() == io::ErrorKind::NotFound => (ManagerState::default(), None),
            Err(err) => (
                ManagerState::default(),
                Some(format!("状态文件 {} 读取失败：{err}", path.display())),
            ),
        }
    }

    /// 依次尝试候选路径写入，返回实际写入的路径。
    pub fn save(&self, candidates: &[PathBuf]) -> Result<PathBuf, String> {
        let text = serde_json::to_string_pretty(self)
            .map_err(|e| format!("状态序列化失败：{e}"))?;

        let mut last_err = String::from("没有可用的状态文件路径");
        for path in candidates {
            match write_atomic(path, &text) {
                Ok(()) => return Ok(path.clone()),
                Err(err) => last_err = format!("{} 写入失败：{err}", path.display()),
            }
        }
        Err(last_err)
    }
}

/// 先写临时文件再重命名，避免中断导致状态文件损坏。
fn write_atomic(path: &Path, text: &str) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, text)?;
    match fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(err) => {
            let _ = fs::remove_file(&tmp);
            Err(err)
        }
    }
}

pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 把 Unix 秒格式化成 `YYYY-MM-DD HH:MM:SS`（本地时区，取不到时退化为 UTC）。
pub fn format_time(secs: u64) -> String {
    if secs == 0 {
        return "未知".to_string();
    }

    #[cfg(unix)]
    {
        // SAFETY: `localtime_r` 只写入我们提供的 `tm`，参数均为有效指针。
        unsafe {
            let time = secs as libc::time_t;
            let mut tm: libc::tm = std::mem::zeroed();
            if !libc::localtime_r(&time, &mut tm).is_null() {
                return format!(
                    "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
                    tm.tm_year + 1900,
                    tm.tm_mon + 1,
                    tm.tm_mday,
                    tm.tm_hour,
                    tm.tm_min,
                    tm.tm_sec
                );
            }
        }
    }

    let (y, m, d, hh, mm, ss) = civil_from_unix(secs);
    format!("{y:04}-{m:02}-{d:02} {hh:02}:{mm:02}:{ss:02} UTC")
}

/// Howard Hinnant 的 civil_from_days 算法（UTC，非 unix 平台或无本地时间时使用）。
fn civil_from_unix(secs: u64) -> (i64, u32, u32, u32, u32, u32) {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if m <= 2 { y + 1 } else { y };
    (
        y,
        m,
        d,
        (rem / 3600) as u32,
        ((rem % 3600) / 60) as u32,
        (rem % 60) as u32,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "w2l-state-test-{tag}-{}-{}",
            std::process::id(),
            now_secs()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn round_trips_through_disk() {
        let dir = temp_dir("roundtrip");
        let path = dir.join(STATE_FILE_NAME);

        let mut state = ManagerState::default();
        state.mark_installed(
            "ChenPi11_cmd",
            vec!["make install".into()],
            Some("/tmp/proj/sources/ChenPi11_cmd".to_string()),
            DepRecord {
                pre_existing: vec!["make".into()],
                auto_installed: vec![AutoDep {
                    command: "cmake".into(),
                    package: "cmake".into(),
                    manager: "pacman".into(),
                    installed_at: now_secs(),
                }],
            },
        );
        assert!(state.is_installed("ChenPi11_cmd"));

        let written = state.save(std::slice::from_ref(&path)).expect("save should succeed");
        assert_eq!(written, path);

        let (loaded, warning) = ManagerState::load(&path);
        assert!(warning.is_none());
        assert_eq!(loaded, state);
        assert!(loaded.installed["ChenPi11_cmd"].installed_at > 0);
        assert_eq!(
            loaded.installed["ChenPi11_cmd"].source_dir.as_deref(),
            Some("/tmp/proj/sources/ChenPi11_cmd")
        );

        loaded.save(std::slice::from_ref(&path)).unwrap();
        let (mut reloaded, _) = ManagerState::load(&path);
        reloaded.mark_uninstalled("ChenPi11_cmd");
        assert!(!reloaded.is_installed("ChenPi11_cmd"));

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_file_is_not_an_error() {
        let dir = temp_dir("missing");
        let (state, warning) = ManagerState::load(&dir.join("nope.json"));
        assert_eq!(state, ManagerState::default());
        assert!(warning.is_none());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn loads_old_state_files_without_dependency_info() {
        // 旧版本写出的状态文件里没有 dependencies 字段，必须还能读出来。
        let dir = temp_dir("legacy");
        let path = dir.join(STATE_FILE_NAME);
        fs::write(
            &path,
            r#"{"version":1,"installed":{"old":{"installed_at":42,"commands":["make install"]}}}"#,
        )
        .unwrap();

        let (state, warning) = ManagerState::load(&path);
        assert!(warning.is_none(), "{warning:?}");
        assert!(state.is_installed("old"));
        let record = &state.installed["old"];
        assert_eq!(record.commands, vec!["make install"]);
        assert!(record.dependencies.is_empty());
        assert!(record.dependencies.auto_packages().is_empty());
        assert_eq!(record.dependencies.manager_name(), None);
        assert_eq!(record.source_dir, None, "旧文件没有 source_dir");

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn dependency_record_keeps_only_what_we_installed() {
        let record = DepRecord {
            pre_existing: vec!["make".into(), "gcc".into()],
            auto_installed: vec![
                AutoDep {
                    command: "cmake".into(),
                    package: "cmake".into(),
                    manager: "pacman".into(),
                    installed_at: 1,
                },
                AutoDep {
                    command: "ninja".into(),
                    package: "ninja".into(),
                    manager: "pacman".into(),
                    installed_at: 2,
                },
                // 同一个包重复时不应重复列出
                AutoDep {
                    command: "ninja-build".into(),
                    package: "ninja".into(),
                    manager: "pacman".into(),
                    installed_at: 3,
                },
            ],
        };

        assert!(!record.is_empty());
        assert_eq!(record.auto_packages(), vec!["cmake", "ninja"]);
        assert_eq!(record.manager_name(), Some("pacman"));
        // 预先存在的依赖只被记录，不在“要卸载的包”里
        assert!(!record.auto_packages().contains(&"make".to_string()));
    }

    #[test]
    fn corrupt_file_yields_warning_not_panic() {
        let dir = temp_dir("corrupt");
        let path = dir.join(STATE_FILE_NAME);
        fs::write(&path, "{ not json").unwrap();
        let (state, warning) = ManagerState::load(&path);
        assert_eq!(state, ManagerState::default());
        assert!(warning.expect("应给出警告").contains("解析失败"));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn save_falls_back_to_second_candidate() {
        let dir = temp_dir("fallback");
        let bad = PathBuf::from("/proc/definitely/not/writable/state.json");
        let good = dir.join("state.json");
        let path = ManagerState::default()
            .save(&[bad, good.clone()])
            .expect("应回退到第二个候选路径");
        assert_eq!(path, good);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn formats_timestamps() {
        assert_eq!(format_time(0), "未知");
        let text = format_time(1_700_000_000);
        assert_eq!(text.len(), 19, "{text}");
        assert!(text.starts_with("2023-11-1"), "{text}");
    }
}
