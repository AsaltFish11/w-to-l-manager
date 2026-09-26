//! 程序日志：写进文件（同时镜像到 stderr），方便在 GUI 模式下排查问题。
//!
//! 日志文件位置由 [`crate::paths::log_file_candidates`] 决定，也可以用
//! `W2L_LOG_FILE` 环境变量指定。级别用 `W2L_LOG` 控制：
//! `off` / `error` / `warn` / `info`（默认）/ `debug` / `trace`。
//!
//! 设为 `debug` 时会连每条命令的原始输出一起记下来，排查安装失败最有用。

use log::{Level, LevelFilter, Log, Metadata, Record};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

/// 单个日志文件的上限，超过就在启动时轮转一次。
const MAX_LOG_BYTES: u64 = 4 * 1024 * 1024;

struct FileLogger {
    path: PathBuf,
    file: Mutex<Option<File>>,
    level: LevelFilter,
    mirror_stderr: bool,
}

static LOGGER: OnceLock<&'static FileLogger> = OnceLock::new();

/// 初始化日志。返回实际使用的日志文件路径。
pub fn init(path: PathBuf) -> PathBuf {
    let level = level_from_env();
    let logger: &'static FileLogger = Box::leak(Box::new(FileLogger {
        path: path.clone(),
        file: Mutex::new(open_log(&path)),
        level,
        mirror_stderr: true,
    }));
    let _ = LOGGER.set(logger);
    // 已经有别的 logger 时忽略错误：日志失败绝不该让程序起不来。
    let _ = log::set_logger(logger);
    log::set_max_level(level);
    path
}

/// 尝试初始化日志：按候选路径挑第一个能写的。
pub fn init_first_writable(candidates: &[PathBuf]) -> Option<PathBuf> {
    for path in candidates {
        if let Some(dir) = path.parent()
            && fs::create_dir_all(dir).is_err()
        {
            continue;
        }
        if open_log(path).is_some() {
            return Some(init(path.clone()));
        }
    }
    // 一个都写不了也要初始化，至少还能镜像到 stderr。
    candidates.first().map(|path| init(path.clone()))
}

/// 当前日志级别（供界面显示）。
pub fn current_level() -> LevelFilter {
    LOGGER.get().map(|l| l.level).unwrap_or(LevelFilter::Info)
}

/// 当前使用的日志文件路径（供界面显示）。
pub fn current_path() -> Option<PathBuf> {
    LOGGER.get().map(|l| l.path.clone())
}

fn level_from_env() -> LevelFilter {
    match std::env::var("W2L_LOG")
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "off" | "none" => LevelFilter::Off,
        "error" => LevelFilter::Error,
        "warn" | "warning" => LevelFilter::Warn,
        "debug" => LevelFilter::Debug,
        "trace" => LevelFilter::Trace,
        "" => LevelFilter::Info,
        // 认不出来的值按 info 处理，并在下面记一笔
        other => {
            eprintln!("W2L_LOG={other} 不是有效级别，按 info 处理");
            LevelFilter::Info
        }
    }
}

fn open_log(path: &Path) -> Option<File> {
    if let Some(dir) = path.parent() {
        let _ = fs::create_dir_all(dir);
    }

    // 文件太大就先轮转一次，避免无限增长。
    if let Ok(meta) = fs::metadata(path)
        && meta.len() > MAX_LOG_BYTES
    {
        let _ = fs::rename(path, path.with_extension("log.1"));
    }

    OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .ok()
}

impl Log for FileLogger {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        metadata.level() <= self.level
    }

    fn log(&self, record: &Record<'_>) {
        if !self.enabled(record.metadata()) {
            return;
        }

        let line = format!(
            "{} [{:<5}] {} {}\n",
            timestamp(),
            record.level().as_str(),
            record.target(),
            record.args()
        );

        if self.mirror_stderr {
            eprint!("{line}");
        }
        if let Ok(mut guard) = self.file.lock()
            && let Some(file) = guard.as_mut()
        {
            let _ = file.write_all(line.as_bytes());
            // 立刻刷盘：崩溃时也能看到最后几行。
            let _ = file.flush();
        }
    }

    fn flush(&self) {
        if let Ok(mut guard) = self.file.lock()
            && let Some(file) = guard.as_mut()
        {
            let _ = file.flush();
        }
    }
}

/// `YYYY-MM-DD HH:MM:SS.mmm`（本地时区）。
fn timestamp() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    format!(
        "{}.{:03}",
        crate::state::format_time(now.as_secs()),
        now.subsec_millis()
    )
}

/// 日志级别名字，界面展示用。
pub fn level_name(level: LevelFilter) -> &'static str {
    match level {
        LevelFilter::Off => "off",
        LevelFilter::Error => "error",
        LevelFilter::Warn => "warn",
        LevelFilter::Info => "info",
        LevelFilter::Debug => "debug",
        LevelFilter::Trace => "trace",
    }
}

/// 一个等级对应的简短标记（测试里用得到）。
pub fn level_tag(level: Level) -> &'static str {
    level.as_str()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    fn temp_path(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "w2l-log-{tag}-{}-{}.log",
            std::process::id(),
            crate::state::now_secs()
        ))
    }

    #[test]
    fn open_log_creates_parent_directories() {
        let dir = std::env::temp_dir().join(format!("w2l-logdir-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let path = dir.join("nested").join("app.log");

        let file = open_log(&path);
        assert!(file.is_some(), "应能创建目录并打开日志文件");
        assert!(path.is_file());

        drop(file);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn rotates_when_the_file_gets_too_big() {
        let path = temp_path("rotate");
        let _ = fs::remove_file(&path);
        let rotated = path.with_extension("log.1");
        let _ = fs::remove_file(&rotated);

        // 造一个超过上限的文件
        let big = vec![b'x'; (MAX_LOG_BYTES + 16) as usize];
        fs::write(&path, &big).unwrap();

        let file = open_log(&path);
        assert!(file.is_some());
        assert!(rotated.is_file(), "旧日志应被轮转成 .log.1");
        assert!(fs::metadata(&path).unwrap().len() < MAX_LOG_BYTES);

        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(&rotated);
    }

    #[test]
    fn writes_lines_at_or_below_the_configured_level() {
        // 直接测底层写入，不动全局 logger（全局 logger 只能设置一次）。
        let path = temp_path("write");
        let _ = fs::remove_file(&path);
        let logger = FileLogger {
            path: path.clone(),
            file: Mutex::new(open_log(&path)),
            level: LevelFilter::Info,
            mirror_stderr: false,
        };

        assert!(logger.enabled(&metadata(Level::Error)));
        assert!(logger.enabled(&metadata(Level::Info)));
        assert!(!logger.enabled(&metadata(Level::Debug)));

        logger.log(
            &Record::builder()
                .args(format_args!("安装开始"))
                .level(Level::Info)
                .target("test")
                .build(),
        );

        let mut text = String::new();
        File::open(&path).unwrap().read_to_string(&mut text).unwrap();
        assert!(text.contains("安装开始"), "{text}");
        assert!(text.contains("[INFO ]"), "{text}");
        assert!(text.contains("test"), "{text}");

        let _ = fs::remove_file(&path);
    }

    fn metadata(level: Level) -> Metadata<'static> {
        Metadata::builder().level(level).target("test").build()
    }

    /// 验证 `main` 用的初始化路径真的会把日志写进文件。
    /// 全局 logger 一个进程只能设置一次，所以这里只调用一次 init。
    #[test]
    fn init_writes_startup_lines_to_the_file() {
        let path = temp_path("init");
        let _ = fs::remove_file(&path);

        let chosen = init_first_writable(std::slice::from_ref(&path));
        assert_eq!(chosen.as_ref(), Some(&path));

        log::info!("启动自检信息 {}", 42);
        log::debug!("这条在 info 级别下不应出现");

        log::logger().flush();
        let mut text = String::new();
        File::open(&path).unwrap().read_to_string(&mut text).unwrap();
        assert!(text.contains("启动自检信息 42"), "{text}");
        assert!(!text.contains("不应出现"), "{text}");
        assert!(text.contains("[INFO ]"), "{text}");

        let _ = fs::remove_file(&path);
    }

    #[test]
    fn level_names_are_stable() {
        assert_eq!(level_name(LevelFilter::Info), "info");
        assert_eq!(level_tag(Level::Debug), "DEBUG");
    }
}
