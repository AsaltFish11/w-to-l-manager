//! egui 界面：展示 `project_list.json` 的条目，并提供安装 / 卸载操作。

use eframe::egui::scroll_area::{DragScroll, ScrollSource};
use eframe::egui::{self, Color32, RichText, Ui};
use std::collections::HashMap;
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::deps::{self, DepPlan, PackageManagerKind};
use crate::exec::{Console, Job, JobKind, JobOutcome, JobRequest, LogLine, LogStream, StepStatus};
use crate::fonts::{self, LoadedFont};
use crate::model::{self, ActionButton, CommandSpec, Permission, ProjectEntry, UninstallSupport};
use crate::paths;
use crate::source::{self, CloneState};
use crate::state::{self, AutoDep, DepRecord, ManagerState};
use crate::steps::{self, CONFIG_DIR_NAME, CONFIG_LIST_FILE, CONFIG_REPO_URL, PlannedJob};

/// 窗口标题，同时用作应用名。
pub const APP_TITLE: &str = "Windows 移植 Linux 管理器";

const OK_GREEN: Color32 = Color32::from_rgb(0x35, 0xa1, 0x5a);
const WARN_AMBER: Color32 = Color32::from_rgb(0xd9, 0x8b, 0x1f);
const ERR_RED: Color32 = Color32::from_rgb(0xd4, 0x3b, 0x3b);
const INFO_BLUE: Color32 = Color32::from_rgb(0x5b, 0x9b, 0xd5);
const MUTED: Color32 = Color32::from_rgb(0x8c, 0x8c, 0x8c);
/// 平台上标签的底色。
const PLATFORM_BG: Color32 = Color32::from_rgb(0x2c, 0x3a, 0x48);
/// 提示框的底色。
const TIPS_BG: Color32 = Color32::from_rgb(0x33, 0x2c, 0x1e);

/// 自动重载检查文件变化的间隔。
const AUTO_RELOAD_INTERVAL: Duration = Duration::from_millis(800);
/// 日志面板一次最多渲染的行数（缓冲区里保留得更多）。
const LOG_RENDER_LIMIT: usize = 3_000;

/// 文件内容指纹，用来判断 `project_list.json` 是否被改动。
///
/// 这里直接对**文件内容**做哈希，而不是看 mtime + 大小：
/// 文件可能被别的程序在运行时改写（以后还要从 URL 拉取），
/// 用内容哈希既不会漏掉“同一秒内的多次改动”，也不会有
/// “先读内容、后取 mtime”那种竞态。文件很小，每次读取的代价可以忽略。
type Fingerprint = u64;

fn fingerprint_of(path: &Path) -> Option<Fingerprint> {
    let bytes = fs::read(path).ok()?;
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut hasher);
    Some(hasher.finish())
}

/// 单个依赖的检测结果（仅查 PATH，用于列表展示）。
#[derive(Debug, Clone)]
struct DepStatus {
    name: String,
    resolved: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ToastKind {
    Ok,
    Warn,
    Error,
}

struct Toast {
    text: String,
    kind: ToastKind,
}

/// 缺依赖时的处理方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DepChoice {
    /// 交给系统包管理器安装
    Auto,
    /// 用户自己去装
    Manual,
}

/// 对话框状态。
enum Dialog {
    /// 缺少依赖，询问自动安装还是手动安装。
    MissingDeps {
        project_id: String,
        plan: DepPlan,
        choice: DepChoice,
    },
    /// 确认安装（依赖已齐全，或已决定自动安装）。
    ConfirmInstall {
        project_id: String,
        auto_packages: Vec<String>,
        manager: Option<PackageManagerKind>,
        dep_record: DepRecord,
        /// 源码目录已存在时，是否删掉重新克隆
        re_clone: bool,
    },
    /// 确认执行 `button-N` 里的命令。
    ConfirmAction {
        project_id: String,
        /// 按钮文字，用作标题
        label: String,
        describe: String,
        commands: Vec<CommandSpec>,
        /// 放到后台跑，不占用界面
        background: bool,
    },
    /// 确认卸载。
    ConfirmUninstall {
        project_id: String,
        commands: Vec<CommandSpec>,
        /// 由本管理器安装、可选一并移除的依赖
        removable: Vec<AutoDep>,
        /// 安装前就已存在、绝不会被改动的依赖
        untouched: Vec<String>,
        remove_deps: bool,
        /// 是否同时删除克隆下来的源码目录
        remove_sources: bool,
    },
}

/// 当前任务附带的元信息，用于任务成功后写状态。
struct ActiveMeta {
    project_id: String,
    kind: JobKind,
    dep_record: DepRecord,
    source_dir: Option<String>,
}

/// 一个放进后台跑的任务（`button` 的 `background: true`）。
///
/// 它不占用 `job` 那个唯一的任务位，所以界面不会被卡住，
/// 别的安装/卸载照样能开；只在开始和结束时各提示一句。
struct BackgroundJob {
    job: Job,
    project_id: String,
    label: String,
}

/// 谁在等 sudo 密码。
enum PasswordOwner {
    Main,
    Background(usize),
}

/// 卡片上产生的操作。
///
/// 刻意把条目本身带上，而不是只带 id：`central()` 渲染时会临时把
/// `self.entries` take 走，如果这里再回头查表就会永远查不到。
enum Action {
    Install(Box<ProjectEntry>),
    Uninstall(Box<ProjectEntry>),
    /// 点击条目里的 `button-N`（带下标，避免克隆一份按钮）。
    RunButton(Box<ProjectEntry>, usize),
}

pub struct ManagerApp {
    // ---- 数据源 ----
    list_path_input: String,
    /// 当前读取的列表文件（可能来自配置仓库的克隆结果）。
    list_path: PathBuf,
    /// 项目根目录：`sources/`、状态文件都放在这里。
    ///
    /// 刻意与 `list_path` 分开：列表可以从别处（配置仓库）读，
    /// 但源码目录必须稳定地留在项目根目录下。
    project_root: PathBuf,
    entries: Vec<ProjectEntry>,
    warnings: Vec<String>,
    load_error: Option<String>,
    loaded_at: Option<u64>,
    fingerprint: Option<Fingerprint>,
    auto_reload: bool,
    last_check: Instant,
    dep_status: HashMap<String, Vec<DepStatus>>,

    // ---- 状态 ----
    state: ManagerState,
    state_paths: Vec<PathBuf>,
    state_file: Option<PathBuf>,

    // ---- 设置 ----
    work_dir_input: String,
    is_root: bool,
    manager: Option<PackageManagerKind>,
    /// 用户输入过的 sudo 密码，只存在于内存中，程序退出即消失。
    sudo_password: String,
    show_password: bool,

    // ---- 任务 / 日志 ----
    console: Console,
    job: Option<Job>,
    job_handled: bool,
    active_meta: Option<ActiveMeta>,
    log_follow: bool,

    /// 当前这个更新任务是不是启动时自动发起的（失败时不弹刺眼的错误）。
    update_automatic: bool,
    /// 正在后台跑的任务（不占用主任务位）。
    background: Vec<BackgroundJob>,

    // ---- 密码弹窗 ----
    /// 已经为哪个请求弹过窗（用于自动聚焦输入框）。
    password_prompt: Option<(usize, String)>,
    password_input: String,

    // ---- 弹窗 / 提示 ----
    /// 设置窗口是否打开。
    show_settings: bool,
    dialog: Option<Dialog>,
    toast: Option<Toast>,
    show_help: bool,
    help_sample: String,
    font: Result<LoadedFont, String>,
}

impl ManagerApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let font = fonts::install_cjk_font(&cc.egui_ctx);
        apply_style(&cc.egui_ctx);
        let mut app = Self::build(paths::locate_project_list(), font);

        log::info!(
            "界面初始化：字体={:?}，包管理器={:?}，root={}",
            app.font.as_ref().map(|f| f.display_name()),
            app.manager.map(|m| m.name()),
            app.is_root
        );
        if let Err(err) = &app.font {
            log::warn!("{err}");
        }
        app.reload();
        app.maybe_auto_update();
        app
    }

    /// 启动时按设置决定要不要自动检查更新。
    fn maybe_auto_update(&mut self) {
        if !self.state.auto_update_on_start {
            log::debug!("启动自动更新已关闭，跳过检查");
            return;
        }
        log::info!("启动自动更新已开启，后台检查最新列表");
        self.begin_update(true);
    }

    /// 组装应用状态（不读文件、不画界面），便于测试。
    fn build(list_path: PathBuf, font: Result<LoadedFont, String>) -> Self {
        // 项目根目录取自最初定位到的列表文件，之后不再跟着列表路径变化。
        let project_root = project_root_of(&list_path);

        Self {
            list_path_input: list_path.display().to_string(),
            list_path: list_path.clone(),
            project_root: project_root.clone(),
            entries: Vec::new(),
            warnings: Vec::new(),
            load_error: None,
            loaded_at: None,
            fingerprint: None,
            auto_reload: true,
            last_check: Instant::now(),
            dep_status: HashMap::new(),
            state: ManagerState::default(),
            state_paths: paths::state_file_candidates(&project_root),
            state_file: None,
            work_dir_input: project_root.display().to_string(),
            is_root: running_as_root(),
            manager: deps::detect(),
            sudo_password: String::new(),
            show_password: false,
            // 所有输出同时镜像到命令行，方便从终端启动时直接看
            console: Console::new(true),
            job: None,
            job_handled: true,
            active_meta: None,
            log_follow: true,
            update_automatic: false,
            background: Vec::new(),
            password_prompt: None,
            password_input: String::new(),
            dialog: None,
            toast: None,
            show_settings: false,
            show_help: false,
            help_sample: HELP_SAMPLE.to_string(),
            font,
        }
    }

    // -----------------------------------------------------------------
    // 读取 project_list.json
    // -----------------------------------------------------------------

    /// 重新读取 `project_list.json` 并按需加载状态文件。
    ///
    /// 目前数据源是本机文件；以后如果要改成从 URL 拉取，
    /// 只需在这里换成「下载到内存再解析」，其余流程都不用动。
    fn reload(&mut self) {
        let path = paths::resolve_input_path(&self.list_path_input);
        let changed_path = path != self.list_path;
        self.list_path = path.clone();

        match fs::read_to_string(&path) {
            Ok(text) => match model::parse(&text) {
                Ok(loaded) => {
                    log::info!(
                        "读取 {}：{} 个条目，{} 条警告",
                        path.display(),
                        loaded.entries.len(),
                        loaded.warnings.len()
                    );
                    for warning in &loaded.warnings {
                        log::warn!("配置警告：{warning}");
                    }
                    self.entries = loaded.entries;
                    self.warnings = loaded.warnings;
                    self.load_error = None;
                    self.loaded_at = Some(state::now_secs());
                }
                Err(err) => {
                    log::error!("{} 解析失败：{err}", path.display());
                    self.load_error = Some(format!("{} 解析失败：{err}", path.display()));
                }
            },
            // 还没有列表文件是很正常的情况：更新会把它写到项目根目录。
            // 这里给一句人话提示，而不是甩一个 No such file 的错误。
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                log::warn!("{} 还不存在", path.display());
                let hint = if self.state.auto_update_on_start {
                    "还没有列表文件；正在从配置仓库获取，国内可能比较慢（可看下方日志）"
                } else {
                    "还没有列表文件；可以点「立即更新」从配置仓库获取"
                };
                self.load_error = Some(hint.to_string());
            }
            Err(err) => {
                log::error!("无法读取 {}：{err}", path.display());
                self.load_error = Some(format!("无法读取 {}：{err}", path.display()));
            }
        }

        self.fingerprint = fingerprint_of(&path);
        self.refresh_dependencies();
        self.manager = deps::detect();

        if changed_path {
            log::info!("列表文件切换为 {}", path.display());
        }
        // 状态文件与工作目录都绑定在项目根目录上，不随列表文件位置变化。
        if self.state_file.is_none() {
            self.load_state();
        }
    }

    fn load_state(&mut self) {
        self.state = ManagerState::default();
        for candidate in &self.state_paths {
            if candidate.is_file() {
                let (state, warning) = ManagerState::load(candidate);
                self.state = state;
                self.state_file = Some(candidate.clone());
                if let Some(warning) = warning {
                    self.set_toast(warning, ToastKind::Error);
                }
                return;
            }
        }
        // 还没有状态文件：记住首选路径，第一次保存时创建。
        self.state_file = self.state_paths.first().cloned();
    }

    fn save_state(&mut self) {
        match self.state.save(&self.state_paths) {
            Ok(path) => {
                log::info!(
                    "已保存安装状态到 {}（已安装 {} 项）",
                    path.display(),
                    self.state.installed.len()
                );
                self.state_file = Some(path);
            }
            Err(err) => {
                log::error!("保存安装状态失败：{err}");
                self.set_toast(format!("保存安装状态失败：{err}"), ToastKind::Error);
            }
        }
    }

    /// 依赖检测结果与条目列表同步刷新。
    fn refresh_dependencies(&mut self) {
        let mut map = HashMap::new();
        for entry in &self.entries {
            let statuses = entry
                .dependency
                .iter()
                .map(|name| DepStatus {
                    name: name.clone(),
                    resolved: paths::which(name),
                })
                .collect();
            map.insert(entry.id.clone(), statuses);
        }
        self.dep_status = map;
    }

    /// 每隔一段时间检查文件是否变化，变化就重新读取。
    fn tick_auto_reload(&mut self) {
        if !self.auto_reload || self.last_check.elapsed() < AUTO_RELOAD_INTERVAL {
            return;
        }
        self.last_check = Instant::now();
        self.refresh_if_changed();
    }

    /// 文件内容变了就重新读取，返回是否真的重载了。
    ///
    /// 单独抽出来是为了不受节流影响地测试：文件可能在程序运行时
    /// 被别的程序改写（以后还要从 URL 拉取），必须能稳定地发现。
    fn refresh_if_changed(&mut self) -> bool {
        let current = fingerprint_of(&self.list_path);
        let changed = match current {
            Some(current) => Some(current) != self.fingerprint,
            // 文件被删掉了也算变化，好把错误提示出来
            None => self.fingerprint.is_some(),
        };
        if !changed {
            return false;
        }

        log::info!("{} 发生变化，自动重新读取", self.list_path.display());
        self.reload();
        self.set_toast(
            format!("{} 已变化，列表已自动重新加载", file_label(&self.list_path)),
            ToastKind::Warn,
        );
        true
    }

    // -----------------------------------------------------------------
    // 任务
    // -----------------------------------------------------------------

    /// 检查后台任务是否结束，并把结果写回状态文件。
    fn poll_job(&mut self) {
        if self.job_handled {
            return;
        }
        let Some(job) = &self.job else {
            return;
        };
        if job.is_running() {
            return;
        }

        let (project_id, kind, outcome, message, auth_failed) = {
            let guard = job.lock();
            (
                guard.project_id.clone(),
                guard.kind,
                guard.outcome,
                guard.message.clone(),
                job.auth_failed(),
            )
        };
        self.job_handled = true;
        let meta = self.active_meta.take();
        let source_hint = meta
            .as_ref()
            .and_then(|m| m.source_dir.as_deref())
            .filter(|dir| !source::has_complete_sources(Path::new(dir)))
            .map(|_| "；源码目录看起来不完整，重新安装时会自动重新克隆");

        match outcome {
            JobOutcome::Succeeded => log::info!("任务成功：{} {}", kind.label(), project_id),
            JobOutcome::Failed => log::error!("任务失败：{} {}：{message}", kind.label(), project_id),
            JobOutcome::Cancelled => log::warn!("任务取消：{} {}", kind.label(), project_id),
            JobOutcome::Running => {}
        }

        match outcome {
            JobOutcome::Succeeded => {
                match kind {
                    JobKind::Install => {
                        let commands: Vec<String> = self
                            .entries
                            .iter()
                            .find(|e| e.id == project_id)
                            .map(|e| {
                                e.install_commands
                                    .iter()
                                    .map(|c| c.command().to_string())
                                    .collect()
                            })
                            .unwrap_or_default();
                        let (source_dir, record) = meta
                            .as_ref()
                            .filter(|m| m.kind == JobKind::Install && m.project_id == project_id)
                            .map(|m| (m.source_dir.clone(), m.dep_record.clone()))
                            .unwrap_or_default();
                        self.state
                            .mark_installed(&project_id, commands, source_dir, record);
                        self.save_state();
                        self.refresh_dependencies();
                        self.set_toast(
                            format!("{project_id} {}完成", kind.past()),
                            ToastKind::Ok,
                        );
                    }
                    JobKind::Uninstall => {
                        self.state.mark_uninstalled(&project_id);
                        self.save_state();
                        self.refresh_dependencies();
                        self.set_toast(
                            format!("{project_id} {}完成", kind.past()),
                            ToastKind::Ok,
                        );
                    }
                    JobKind::Update => {
                        self.update_automatic = false;
                        self.apply_update();
                    }
                    // 按钮只是跑命令，不动安装状态
                    JobKind::Action => {
                        self.set_toast(format!("{project_id} 执行完成"), ToastKind::Ok);
                    }
                }
            }
            JobOutcome::Failed if kind == JobKind::Update && self.update_automatic => {
                self.update_automatic = false;
                self.clean_update_tmp();
                log::warn!("启动自动更新失败（继续使用当前列表）：{message}");
                self.set_toast(
                    "启动自动更新失败（可能是网络问题），继续使用当前列表；可点「立即更新」重试"
                        .to_string(),
                    ToastKind::Warn,
                );
            }
            JobOutcome::Failed => {
                if kind == JobKind::Update {
                    self.update_automatic = false;
                    self.clean_update_tmp();
                }
                // sudo 说密码不对 / 需要密码：把内存里的密码丢掉，下次重新问
                let auth_hint = if auth_failed {
                    self.sudo_password.clear();
                    log::warn!("sudo 认证失败，已清除内存中的密码，下次会重新询问");
                    "；sudo 密码无效，已清除，请重新输入后再试"
                } else {
                    ""
                };
                self.set_toast(
                    format!(
                        "{project_id} {}失败：{message}{}{auth_hint}",
                        kind.past(),
                        source_hint.unwrap_or("")
                    ),
                    ToastKind::Error,
                );
            }
            JobOutcome::Cancelled => {
                if kind == JobKind::Update {
                    // 取消 / 失败时不会走到清理步骤，这里补上，别留下临时目录
                    self.update_automatic = false;
                    self.clean_update_tmp();
                }
                self.set_toast(format!("{project_id} 的任务已取消"), ToastKind::Warn);
            }
            JobOutcome::Running => {}
        }
    }

    /// 从配置仓库拉取最新的 `project_list.json`。
    ///
    /// 每次都重新克隆（`--depth=1`，仓库很小）：既保证拿到最新的一份，
    /// 也免去“上次克隆残留”的各种麻烦。
    /// 如果当前占着任务位的只是「启动自动更新」，就让给用户的操作。
    fn yield_automatic_update(&mut self) {
        if !self.busy() || !self.update_automatic {
            return;
        }
        log::info!("用户操作优先：取消正在进行的启动自动更新");
        if let Some(job) = &self.job {
            job.cancel();
        }
        // 丢弃这个任务，后续按用户的操作走
        self.job = None;
        self.job_handled = true;
        self.update_automatic = false;
    }

    fn begin_update(&mut self, automatic: bool) {
        self.yield_automatic_update();
        if self.busy() {
            if !automatic {
                self.set_toast("已有任务正在执行，请先等待或取消".to_string(), ToastKind::Warn);
            }
            return;
        }
        if paths::which("git").is_none() {
            if automatic {
                log::warn!("系统里找不到 git，跳过启动自动更新");
            } else {
                self.set_toast("系统里找不到 git，无法更新列表".to_string(), ToastKind::Error);
            }
            return;
        }

        self.update_automatic = automatic;
        if !automatic {
            self.set_toast(
                "开始拉取最新列表…（国内可能较慢，日志里会显示进度）".to_string(),
                ToastKind::Ok,
            );
        }
        let root = self.project_root.clone();
        let steps = steps::update_steps(&root);

        log::info!(
            "开始{}更新列表：{} -> {}（临时目录 {}）",
            if automatic { "自动" } else { "手动" },
            CONFIG_REPO_URL,
            root.join(paths::PROJECT_LIST_FILE).display(),
            steps::clone_dir(&root).display()
        );

        self.console.push(LogLine {
            job_id: 0,
            project_id: CONFIG_DIR_NAME.to_string(),
            step: 0,
            stream: LogStream::Info,
            text: format!("──────── {} ────────", JobKind::Update.gerund()),
        });

        self.active_meta = Some(ActiveMeta {
            project_id: CONFIG_DIR_NAME.to_string(),
            kind: JobKind::Update,
            dep_record: DepRecord::default(),
            source_dir: None,
        });
        self.job = Some(Job::spawn(JobRequest {
            project_id: CONFIG_DIR_NAME.to_string(),
            kind: JobKind::Update,
            steps,
            work_dir: root,
            sudo_password: None,
            source_base: None,
            env: self.state.usable_env(),
            console: self.console.clone(),
        }));
        self.job_handled = false;
        self.toast = None;
        self.log_follow = true;
    }

    /// 收拾更新用的临时目录：删掉克隆，`tmp/` 空了就一并删掉。
    ///
    /// 正常情况下由任务里的清理步骤完成；任务失败或取消时走不到那一步，
    /// 就在这里兜一下，免得留下半个仓库。
    fn clean_update_tmp(&mut self) {
        let clone = steps::clone_dir(&self.project_root);
        match fs::remove_dir_all(&clone) {
            Ok(()) => log::info!("已清理临时克隆目录 {}", clone.display()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => log::warn!("清理 {} 失败：{err}", clone.display()),
        }

        let tmp = steps::tmp_dir(&self.project_root);
        match fs::remove_dir(&tmp) {
            Ok(()) => log::info!("已删除空的临时目录 {}", tmp.display()),
            // 目录非空（里面还有别人的东西）或不存在：都保持原样
            Err(_) => log::debug!("{} 未删除（不存在或非空）", tmp.display()),
        }
    }

    /// 更新任务成功后：列表已经被换到项目根目录，重新读一遍即可。
    fn apply_update(&mut self) {
        let list = self.project_root.join(paths::PROJECT_LIST_FILE);
        if !list.is_file() {
            let message = format!("更新完成，但没有找到 {}", list.display());
            log::error!("{message}");
            self.set_toast(message, ToastKind::Error);
            return;
        }

        // 列表永远读项目根目录下这一份，所以把路径也拉回来（用户之前可能手动改到别处）
        self.list_path_input = list.display().to_string();
        log::info!("列表已更新：{}", list.display());
        self.reload();

        self.set_toast(
            format!("列表已更新（{} 个条目）", self.entries.len()),
            ToastKind::Ok,
        );
    }

    /// 每帧要做的事（和绘制无关），抽出来是为了能脱离 egui 测。
    fn tick(&mut self) {
        self.tick_auto_reload();
        self.poll_job();
        self.poll_background_jobs();
    }

    /// 当前有没有正在跑的任务。
    fn busy(&self) -> bool {
        self.job.as_ref().is_some_and(|j| j.is_running())
    }

    /// 内存里已经缓存的 sudo 密码（root 身份时不需要）。
    fn job_password(&self) -> Option<String> {
        if self.is_root || self.sudo_password.is_empty() {
            None
        } else {
            Some(self.sudo_password.clone())
        }
    }

    /// 自动安装依赖是否有障碍；`Some` 时说明原因。
    fn auto_install_blocker(&self) -> Option<String> {
        if self.is_root {
            return None;
        }
        if paths::which("sudo").is_none() {
            return Some(
                "自动安装依赖需要 root 权限，但系统里找不到 sudo，请手动安装".to_string(),
            );
        }
        // 有 sudo 就够了：真正执行到那一步时才会弹窗要密码
        None
    }

    fn start_job(
        &mut self,
        project_id: String,
        kind: JobKind,
        plan: PlannedJob,
        dep_record: DepRecord,
    ) {
        if self.busy() {
            self.set_toast("已有任务正在执行，请先等待或取消".to_string(), ToastKind::Warn);
            return;
        }

        let work_dir = paths::resolve_input_path(&self.work_dir_input);
        if !work_dir.is_dir() {
            self.set_toast(
                format!("工作目录不存在：{}", work_dir.display()),
                ToastKind::Error,
            );
            return;
        }
        if plan.steps.is_empty() {
            self.set_toast("没有可执行的命令".to_string(), ToastKind::Warn);
            return;
        }

        let source_dir = plan.source_dir().map(|path| path.display().to_string());

        self.console.push(LogLine {
            job_id: 0,
            project_id: project_id.clone(),
            step: 0,
            stream: LogStream::Info,
            text: format!("──────── {} {} ────────", kind.gerund(), project_id),
        });

        log::info!(
            "开始{} {}：{} 步，工作目录 {}，源码目录 {}",
            kind.past(),
            project_id,
            plan.steps.len(),
            work_dir.display(),
            source_dir.as_deref().unwrap_or("（无，直接在默认目录构建）")
        );
        for (index, step) in plan.steps.iter().enumerate() {
            log::info!(
                "  步骤 {}/{}: [{}] {}",
                index + 1,
                plan.steps.len(),
                if step.needs_root { "root" } else { "user" },
                step.command
            );
        }

        self.active_meta = Some(ActiveMeta {
            project_id: project_id.clone(),
            kind,
            dep_record,
            source_dir,
        });
        self.job = Some(Job::spawn(JobRequest {
            project_id,
            kind,
            steps: plan.steps,
            work_dir,
            sudo_password: self.job_password(),
            source_base: plan.source_base,
            env: self.state.usable_env(),
            console: self.console.clone(),
        }));
        self.job_handled = false;
        self.toast = None;
        self.log_follow = true;
    }

    // -----------------------------------------------------------------
    // 操作入口
    // -----------------------------------------------------------------

    fn handle_action(&mut self, action: Action) {
        // 启动自动更新可能要跑好几分钟（国内克隆很慢），
        // 用户主动点安装/卸载时应该让路，而不是被挡住。
        self.yield_automatic_update();
        if self.busy() {
            self.set_toast("已有任务正在执行，请先等待或取消".to_string(), ToastKind::Warn);
            return;
        }

        match action {
            Action::Install(entry) => self.begin_install(&entry),
            Action::Uninstall(entry) => self.begin_uninstall(&entry),
            Action::RunButton(entry, index) => self.begin_action(&entry, index),
        }
    }

    /// 点击「安装」：先查依赖，缺依赖就先问怎么处理。
    fn begin_install(&mut self, entry: &ProjectEntry) {
        if entry.install_commands.is_empty() {
            self.set_toast(
                format!("{} 没有可执行的 install-commands", entry.id),
                ToastKind::Warn,
            );
            return;
        }

        let plan = deps::plan(&entry.dependency, self.manager);
        log::info!(
            "点击安装：{}，依赖 {} 个（已存在 {}，缺失 {}）",
            entry.id,
            entry.dependency.len(),
            plan.present.len(),
            plan.missing.len()
        );
        for missing in &plan.missing {
            log::warn!(
                "  缺少依赖 {}：{}",
                missing.command,
                missing.reason.as_deref().unwrap_or("可自动安装")
            );
        }
        if plan.missing.is_empty() {
            self.dialog = Some(Dialog::ConfirmInstall {
                project_id: entry.id.clone(),
                auto_packages: Vec::new(),
                manager: self.manager,
                dep_record: DepRecord {
                    pre_existing: plan.present,
                    auto_installed: Vec::new(),
                },
                re_clone: false,
            });
        } else {
            self.dialog = Some(Dialog::MissingDeps {
                project_id: entry.id.clone(),
                plan,
                choice: if self.auto_install_blocker().is_none() {
                    DepChoice::Auto
                } else {
                    DepChoice::Manual
                },
            });
        }
    }

    /// 点击「卸载」：算出卸载命令，并查出可选移除的自动安装依赖。
    /// 点击了条目里的一个操作按钮：先让用户确认要跑的命令。
    fn begin_action(&mut self, entry: &ProjectEntry, index: usize) {
        let Some(button) = entry.buttons.get(index) else {
            log::warn!("{} 上不存在第 {} 个 button", entry.id, index + 1);
            return;
        };
        if button.commands.is_empty() {
            self.set_toast(
                format!("「{}」没有配置任何命令", button.label()),
                ToastKind::Warn,
            );
            return;
        }

        log::info!(
            "点击按钮：{} 的「{}」（{} 条命令）",
            entry.id,
            button.label(),
            button.commands.len()
        );
        self.dialog = Some(Dialog::ConfirmAction {
            project_id: entry.id.clone(),
            label: button.label().to_string(),
            describe: button.describe.trim().to_string(),
            commands: button.commands.clone(),
            background: button.background,
        });
    }

    /// 用户确认后真正开始执行按钮里的命令。
    fn start_action_job(
        &mut self,
        project_id: &str,
        label: &str,
        commands: &[CommandSpec],
        background: bool,
    ) {
        let Some(entry) = self.entries.iter().find(|e| e.id == project_id).cloned() else {
            self.set_toast(
                format!("列表里已经找不到 {project_id} 了，可能刚被更新过"),
                ToastKind::Warn,
            );
            return;
        };
        let button = ActionButton {
            name: label.to_string(),
            describe: String::new(),
            commands: commands.to_vec(),
            background,
        };
        let plan = steps::action_steps(&self.project_root, &entry, &button);
        if plan.steps.is_empty() {
            self.set_toast("这个按钮没有任何命令".to_string(), ToastKind::Warn);
            return;
        }

        // 有源码目录就在里面跑，否则用工作目录
        let work_dir = plan
            .source_dir()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| paths::resolve_input_path(&self.work_dir_input));

        let job = Job::spawn(JobRequest {
            project_id: project_id.to_string(),
            kind: JobKind::Action,
            steps: plan.steps,
            work_dir,
            sudo_password: self.job_password(),
            source_base: plan.source_base,
            env: self.state.usable_env(),
            console: self.console.clone(),
        });

        if background {
            // 后台任务不占用主任务位：界面照常能用，别的安装/卸载也能开
            self.console.push(LogLine {
                job_id: 0,
                project_id: project_id.to_string(),
                step: 0,
                stream: LogStream::Info,
                text: format!(
                    "▶ 「{label}」已在后台启动（{project_id}）—— 结束后会再提示一句"
                ),
            });
            log::info!("后台启动「{label}」（{project_id}）");
            self.background.push(BackgroundJob {
                job,
                project_id: project_id.to_string(),
                label: label.to_string(),
            });
            self.log_follow = true;
            return;
        }

        self.console.push(LogLine {
            job_id: 0,
            project_id: project_id.to_string(),
            step: 0,
            stream: LogStream::Info,
            text: format!("──────── {}「{label}」────────", JobKind::Action.gerund()),
        });
        log::info!("开始执行「{label}」（{project_id}）");

        self.active_meta = Some(ActiveMeta {
            project_id: project_id.to_string(),
            kind: JobKind::Action,
            dep_record: DepRecord::default(),
            source_dir: None,
        });
        self.job = Some(job);
        self.job_handled = false;
        self.toast = None;
        self.log_follow = true;
    }

    /// 检查后台任务有没有跑完；结束了就提示一句并把它移出列表。
    fn poll_background_jobs(&mut self) {
        let mut done: Vec<usize> = Vec::new();

        for (index, task) in self.background.iter().enumerate() {
            if task.job.is_running() {
                continue;
            }

            let (outcome, message, started_at, finished_at) = {
                let guard = task.job.lock();
                (
                    guard.outcome,
                    guard.message.clone(),
                    guard.started_at,
                    guard.finished_at,
                )
            };
            let secs = finished_at.unwrap_or_else(state::now_secs).saturating_sub(started_at);
            // 秒级时间戳，跑得比 1 秒还快就不提耗时了，免得写成“耗时 0 秒”
            let elapsed = if secs >= 1 {
                format!("（耗时 {}）", short_duration(secs))
            } else {
                String::new()
            };
            let text = if outcome == JobOutcome::Succeeded {
                format!("■ 「{}」后台任务已结束{elapsed}", task.label)
            } else if outcome == JobOutcome::Cancelled {
                format!("■ 「{}」后台任务已取消{elapsed}", task.label)
            } else {
                format!(
                    "■ 「{}」后台任务结束，但有命令失败{elapsed}：{message}",
                    task.label
                )
            };

            log::info!(
                "后台任务「{}」（{}）结束：{outcome:?}，耗时 {secs} 秒",
                task.label,
                task.project_id
            );
            self.console.push(LogLine {
                job_id: 0,
                project_id: task.project_id.clone(),
                step: 0,
                stream: LogStream::Info,
                text,
            });
            done.push(index);
        }

        // 从后往前删，下标才不会串
        for index in done.into_iter().rev() {
            let task = self.background.remove(index);
            self.set_toast(
                format!("「{}」后台任务已结束", task.label),
                ToastKind::Ok,
            );
        }
    }

    fn begin_uninstall(&mut self, entry: &ProjectEntry) {
        let plan = match model::uninstall_plan(entry) {
            UninstallSupport::Available(plan) => plan,
            UninstallSupport::Unavailable { reason } => {
                self.set_toast(format!("{} 无法卸载：{reason}", entry.id), ToastKind::Warn);
                return;
            }
        };

        let record = self.state.installed.get(&entry.id).cloned().unwrap_or_default();
        let removable = record.dependencies.auto_installed.clone();
        let untouched = record.dependencies.pre_existing.clone();
        log::info!(
            "点击卸载：{}（{} 条命令，可移除依赖 {} 个）",
            entry.id,
            plan.commands.len(),
            removable.len()
        );

        self.dialog = Some(Dialog::ConfirmUninstall {
            project_id: entry.id.clone(),
            commands: plan.commands,
            removable,
            untouched,
            // 默认不动依赖和源码，避免误删
            remove_deps: false,
            remove_sources: false,
        });
    }

    /// 从依赖计划里取出要自动安装的包与依赖记录。
    fn auto_deps_from_plan(plan: &DepPlan, manager: PackageManagerKind) -> Vec<AutoDep> {
        let now = state::now_secs();
        plan.missing
            .iter()
            .filter_map(|missing| {
                missing.package.as_ref().map(|package| AutoDep {
                    command: missing.command.clone(),
                    package: package.clone(),
                    manager: manager.name().to_string(),
                    installed_at: now,
                })
            })
            .collect()
    }

    fn set_toast(&mut self, text: String, kind: ToastKind) {
        self.toast = Some(Toast { text, kind });
    }

    // -----------------------------------------------------------------
    // 界面
    // -----------------------------------------------------------------

    fn top_bar(&mut self, ui: &mut Ui) {
        egui::Panel::top("w2l_top").show(ui, |ui| {
            ui.add_space(10.0);

            // --- 数据源 ---
            ui.horizontal_wrapped(|ui| {
                ui.label("列表文件：");
                let width = (ui.available_width() - 320.0).clamp(180.0, 560.0);
                ui.add(
                    egui::TextEdit::singleline(&mut self.list_path_input)
                        .desired_width(width)
                        .hint_text("project_list.json 的路径"),
                );
                if ui.button("重新读取").clicked() {
                    self.reload();
                }
                let response = ui
                    .add_enabled(
                        !self.busy(),
                        egui::Button::new(RichText::new("立即更新").strong()),
                    )
                    .on_hover_text(format!(
                        "从配置仓库拉取最新的 {CONFIG_LIST_FILE}\n{CONFIG_REPO_URL}"
                    ))
                    .on_disabled_hover_text("已有任务正在执行");
                if response.clicked() {
                    self.begin_update(false);
                }
                ui.checkbox(&mut self.auto_reload, "自动重载")
                    .on_hover_text("列表文件内容变化时自动重新读取");
                if ui.button("设置").clicked() {
                    self.show_settings = true;
                }
                if ui.button("格式说明").clicked() {
                    self.show_help = true;
                }
            });

            // --- 状态 ---
            ui.add_space(6.0);
            ui.horizontal_wrapped(|ui| {
                match &self.load_error {
                    Some(err) => {
                        ui.label(RichText::new(format!("✖ {err}")).color(ERR_RED));
                    }
                    None => {
                        let when = self
                            .loaded_at
                            .map(state::format_time)
                            .unwrap_or_else(|| "—".to_string());
                        ui.label(
                            RichText::new(format!(
                                "✔ 已读取 {} 个条目 · {when}",
                                self.entries.len()
                            ))
                            .color(OK_GREEN),
                        );
                    }
                }

                status_divider(ui);
                match self.manager {
                    Some(kind) => {
                        ui.label(
                            RichText::new(format!("包管理器：{}", kind.name()))
                                .color(OK_GREEN)
                                .small(),
                        );
                    }
                    None => {
                        ui.label(
                            RichText::new("未检测到 pacman / apt，缺失的依赖只能手动安装")
                                .color(WARN_AMBER)
                                .small(),
                        );
                    }
                }

                // 后台任务不占任务位，但得让人知道还有东西在跑
                if !self.background.is_empty() {
                    status_divider(ui);
                    let labels: Vec<&str> = self
                        .background
                        .iter()
                        .map(|task| task.label.as_str())
                        .collect();
                    ui.label(
                        RichText::new(format!(
                            "后台任务 {} 个：{}",
                            labels.len(),
                            labels.join("、")
                        ))
                        .color(INFO_BLUE)
                        .small(),
                    )
                    .on_hover_text("后台任务不占用任务位，界面可以照常使用；结束后会在日志里提示");
                }

                // 密码只在需要 root 时才问
                status_divider(ui);
                if self.is_root {
                    ui.label(RichText::new("以 root 运行，无需密码").color(OK_GREEN).small());
                } else if self.sudo_password.is_empty() {
                    ui.label(
                        RichText::new("提权密码：未保存")
                            .color(MUTED)
                            .small(),
                    )
                    .on_hover_text("执行到需要 root 的步骤时才会询问，密码只留在内存里");
                } else {
                    ui.label(RichText::new("提权密码：已保存在内存中").color(OK_GREEN).small());
                    if ui.small_button("清除").clicked() {
                        self.sudo_password.clear();
                        self.set_toast("已清除内存中的 sudo 密码".to_string(), ToastKind::Warn);
                    }
                }

                if let Some(warnings) = self.warnings.first() {
                    status_divider(ui);
                    let extra = self.warnings.len().saturating_sub(1);
                    let text = if extra == 0 {
                        format!("⚠ {warnings}")
                    } else {
                        format!("⚠ {warnings}（另有 {extra} 条警告）")
                    };
                    ui.label(RichText::new(text).color(WARN_AMBER))
                        .on_hover_text(self.warnings.join("\n"));
                }
            });

            // --- 文件位置（次要信息，小字放一行）---
            ui.add_space(4.0);
            ui.horizontal_wrapped(|ui| {
                ui.label(
                    RichText::new(format!("项目目录：{}", self.project_root.display()))
                        .color(MUTED)
                        .small(),
                )
                .on_hover_text("源码目录 sources/ 与状态文件都放在这里，不随列表文件位置变化");
                status_divider(ui);
                if let Some(path) = &self.state_file {
                    ui.label(
                        RichText::new(format!("状态文件：{}", path.display()))
                            .color(MUTED)
                            .small(),
                    )
                    .on_hover_text("安装状态保存在这里");
                }
                match crate::logging::current_path() {
                    Some(path) => {
                        status_divider(ui);
                        ui.label(
                            RichText::new(format!(
                                "日志：{}（{} 级）",
                                path.display(),
                                crate::logging::level_name(crate::logging::current_level())
                            ))
                            .color(MUTED)
                            .small(),
                        )
                        .on_hover_text("设置 W2L_LOG=debug 可把命令原始输出也写进日志");
                    }
                    None => {
                        status_divider(ui);
                        ui.label(RichText::new("日志：仅输出到 stderr").color(MUTED).small());
                    }
                }
            });

            if let Err(err) = &self.font {
                ui.add_space(2.0);
                ui.label(RichText::new(format!("⚠ {err}")).color(WARN_AMBER));
            }

            if let Some(toast) = &self.toast {
                let text = toast.text.clone();
                let color = match toast.kind {
                    ToastKind::Ok => OK_GREEN,
                    ToastKind::Warn => WARN_AMBER,
                    ToastKind::Error => ERR_RED,
                };
                let mut dismiss = false;
                ui.add_space(4.0);
                ui.horizontal_wrapped(|ui| {
                    ui.label(RichText::new(&text).color(color).strong());
                    if ui.small_button("知道了").clicked() {
                        dismiss = true;
                    }
                });
                if dismiss {
                    self.toast = None;
                }
            }

            ui.add_space(8.0);
        });
    }

    fn central(&mut self, ui: &mut Ui) {
        // 渲染期间临时把条目取出来，避免遍历时与 &mut self 冲突。
        let entries = std::mem::take(&mut self.entries);
        let mut actions = Vec::new();

        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                if entries.is_empty() {
                    ui.add_space(40.0);
                    ui.vertical_centered(|ui| {
                        ui.label(
                            RichText::new(if self.load_error.is_some() {
                                "暂时没有可显示的列表，请看上方提示"
                            } else {
                                "project_list.json 里没有任何条目"
                            })
                            .color(MUTED),
                        );
                    });
                    ui.add_space(40.0);
                }

                for entry in &entries {
                    if let Some(action) = self.project_card(ui, entry) {
                        actions.push(action);
                    }
                    ui.add_space(8.0);
                }
                ui.add_space(8.0);
            });

        // 必须先把条目放回去，再处理点击：后续流程会读取 self.entries。
        self.entries = entries;
        for action in actions {
            self.handle_action(action);
        }
    }

    fn project_card(&mut self, ui: &mut Ui, entry: &ProjectEntry) -> Option<Action> {
        let id = entry.id.clone();
        let describe = entry.describe.clone();
        let author = entry.author().map(str::to_string);
        let platforms: Vec<String> = entry.platforms().iter().map(|p| p.to_string()).collect();
        let tips = entry.tips().map(str::to_string);
        let buttons: Vec<(String, String)> = entry
            .buttons
            .iter()
            .map(|b| (b.label().to_string(), b.describe.trim().to_string()))
            .collect();
        let links: Vec<(&'static str, String)> = entry
            .links()
            .into_iter()
            .map(|(icon, url)| (icon, url.to_string()))
            .collect();
        let clone_command = entry.clone_command.clone();
        let source_base = entry
            .needs_clone()
            .then(|| paths::project_source_base(&self.project_root, &entry.id));

        let record = self.state.installed.get(&id).cloned();
        let installed = record.is_some();
        let installed_at = record.as_ref().map(|r| r.installed_at).unwrap_or(0);
        let auto_deps = record
            .as_ref()
            .map(|r| r.dependencies.auto_installed.clone())
            .unwrap_or_default();
        let pre_existing = record
            .as_ref()
            .map(|r| r.dependencies.pre_existing.clone())
            .unwrap_or_default();

        let deps = self.dep_status.get(&id).cloned().unwrap_or_default();
        let missing_deps: Vec<String> = deps
            .iter()
            .filter(|d| d.resolved.is_none())
            .map(|d| d.name.clone())
            .collect();

        let install_commands: Vec<String> = entry
            .install_commands
            .iter()
            .map(|c| c.command().to_string())
            .collect();
        let plan = model::uninstall_plan(entry);
        let uninstall_available = matches!(plan, UninstallSupport::Available(_));
        let job_running = self.busy();
        let busy_here = self.job.as_ref().is_some_and(|j| {
            let guard = j.lock();
            guard.is_running() && guard.project_id == id
        });

        let install_tooltip = if install_commands.is_empty() {
            "该条目没有 install-commands".to_string()
        } else if job_running {
            "已有任务正在执行".to_string()
        } else {
            String::new()
        };
        let uninstall_tooltip = if !installed {
            "尚未通过本管理器安装".to_string()
        } else if !uninstall_available {
            match &plan {
                UninstallSupport::Unavailable { reason } => reason.clone(),
                _ => String::new(),
            }
        } else if job_running {
            "已有任务正在执行".to_string()
        } else {
            String::new()
        };

        let uninstall_commands: Vec<CommandSpec> = match &plan {
            UninstallSupport::Available(p) => p.commands.clone(),
            UninstallSupport::Unavailable { .. } => Vec::new(),
        };

        let mut action = None;

        egui::Frame::group(ui.style())
            .inner_margin(egui::Margin::symmetric(14, 12))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());

                // 标题行
                ui.horizontal(|ui| {
                    ui.label(RichText::new(&id).size(18.0).strong());
                    if let Some(author) = &author {
                        ui.label(RichText::new(format!("@{author}")).color(MUTED).small())
                            .on_hover_text("作者 / 维护者");
                    }
                    for platform in &platforms {
                        ui.label(
                            RichText::new(platform)
                                .color(INFO_BLUE)
                                .small()
                                .background_color(PLATFORM_BG),
                        )
                        .on_hover_text("适用平台");
                    }
                    if installed {
                        let text = if installed_at > 0 {
                            format!("● 已安装 · {}", state::format_time(installed_at))
                        } else {
                            "● 已安装".to_string()
                        };
                        ui.label(RichText::new(text).color(OK_GREEN).small());
                    } else {
                        ui.label(RichText::new("○ 未安装").color(MUTED).small());
                    }
                    if busy_here {
                        ui.spinner();
                    }

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let uninstall_enabled = installed && uninstall_available && !job_running;
                        let response =
                            ui.add_enabled(uninstall_enabled, egui::Button::new("卸载"));
                        if response.clicked() {
                            action = Some(Action::Uninstall(Box::new(entry.clone())));
                        }
                        if !uninstall_enabled && !uninstall_tooltip.is_empty() {
                            let _ = response.on_disabled_hover_text(uninstall_tooltip.clone());
                        }

                        let install_enabled = !install_commands.is_empty() && !job_running;
                        let response = ui.add_enabled(
                            install_enabled,
                            egui::Button::new(RichText::new("安装").strong()),
                        );
                        if response.clicked() {
                            action = Some(Action::Install(Box::new(entry.clone())));
                        }
                        if !install_enabled && !install_tooltip.is_empty() {
                            let _ = response.on_disabled_hover_text(install_tooltip.clone());
                        }

                        // 作者在 JSON 里定义的 button-1、button-2 …（倒序添加才是正序显示）
                        for index in (0..buttons.len()).rev() {
                            let (label, describe) = &buttons[index];
                            // 忙的时候谁都别点，免得任务互相打架
                            let enabled = !busy_here && !job_running;
                            let response =
                                ui.add_enabled(enabled, egui::Button::new(label.clone()));
                            let response = if describe.is_empty() {
                                response
                            } else {
                                response.on_hover_text(describe.clone())
                            };
                            if response.clicked() {
                                action = Some(Action::RunButton(Box::new(entry.clone()), index));
                            }
                        }
                        if !buttons.is_empty() {
                            status_divider(ui);
                        }
                    });
                });

                ui.add_space(6.0);

                // 说明
                if describe.trim().is_empty() {
                    ui.label(RichText::new("（该条目没有 describe 说明）").color(MUTED).italics());
                } else {
                    ui.add(
                        egui::Label::new(RichText::new(&describe))
                            .wrap()
                            .selectable(true),
                    );
                }

                // 作者的额外提示（往往是重要的安全说明，别折叠）
                if let Some(tips) = &tips {
                    ui.add_space(6.0);
                    egui::Frame::group(ui.style())
                        .fill(TIPS_BG)
                        .inner_margin(egui::Margin::symmetric(10, 8))
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            ui.label(RichText::new("提示").color(WARN_AMBER).small().strong());
                            ui.add(
                                egui::Label::new(RichText::new(tips).small())
                                    .wrap()
                                    .selectable(true),
                            );
                        });
                }

                // 项目链接（GitHub / B 站等）
                if !links.is_empty() {
                    ui.add_space(4.0);
                    ui.horizontal_wrapped(|ui| {
                        for (index, (icon, url)) in links.iter().enumerate() {
                            if index > 0 {
                                status_divider(ui);
                            }
                            // 自己处理点击：egui 的 hyperlink 依赖 links feature
                            let response = ui
                                .add(
                                    egui::Label::new(
                                        RichText::new(format!("{icon} {}", short_url(url)))
                                            .color(INFO_BLUE)
                                            .underline(),
                                    )
                                    .sense(egui::Sense::click()),
                                )
                                .on_hover_cursor(egui::CursorIcon::PointingHand)
                                .on_hover_text(format!("{url}\n点击用系统默认浏览器打开"));
                            if response.clicked() {
                                match crate::open::open_url(url) {
                                    Ok(()) => self.set_toast(
                                        format!("已在浏览器中打开 {url}"),
                                        ToastKind::Ok,
                                    ),
                                    Err(err) => self.set_toast(err, ToastKind::Error),
                                }
                            }
                        }
                    });
                }

                if !missing_deps.is_empty() {
                    ui.add_space(4.0);
                    let hint = match self.manager {
                        Some(kind) => format!(
                            "⚠ 缺少依赖：{} —— 点击「安装」可选择自动安装（{}）或手动安装",
                            missing_deps.join("、"),
                            kind.name()
                        ),
                        None => format!(
                            "⚠ 缺少依赖：{} —— 未检测到 pacman / apt，请手动安装",
                            missing_deps.join("、")
                        ),
                    };
                    ui.label(RichText::new(hint).color(WARN_AMBER));
                }

                // 详情
                ui.add_space(2.0);
                egui::CollapsingHeader::new("详情（源码 / 依赖 / 安装命令 / 卸载命令）")
                    .id_salt(id.as_str())
                    .default_open(false)
                    .show(ui, |ui| {
                        ui.label(RichText::new("源码").strong());
                        match &clone_command {
                            Some(command) => {
                                ui.monospace(format!("  克隆命令：{command}"));
                                if let Some(base) = &source_base {
                                    ui.monospace(format!("  源码目录：{}", base.display()));
                                }
                                if let Some(dir) =
                                    record.as_ref().and_then(|r| r.source_dir.as_deref())
                                {
                                    ui.label(
                                        RichText::new(format!("  已克隆到：{dir}"))
                                            .color(OK_GREEN)
                                            .small(),
                                    );
                                }
                            }
                            None => {
                                ui.label(
                                    RichText::new("  （没有 clone-command，直接在工作目录里构建）")
                                        .color(MUTED),
                                );
                            }
                        }

                        ui.add_space(4.0);
                        ui.label(RichText::new("依赖").strong());
                        if deps.is_empty() {
                            ui.label(RichText::new("  （无）").color(MUTED));
                        } else {
                            for dep in &deps {
                                match &dep.resolved {
                                    Some(path) => {
                                        let auto = auto_deps
                                            .iter()
                                            .find(|d| d.command == dep.name)
                                            .map(|d| d.package.clone());
                                        let suffix = match auto {
                                            Some(package) => {
                                                format!("本管理器已安装（{package}）")
                                            }
                                            None => {
                                                if pre_existing.contains(&dep.name) {
                                                    "安装前已存在，卸载时不会改动".to_string()
                                                } else {
                                                    "已存在，卸载时不会改动".to_string()
                                                }
                                            }
                                        };
                                        ui.label(
                                            RichText::new(format!(
                                                "  ✔ {} — {}（{}）",
                                                dep.name,
                                                path.display(),
                                                suffix
                                            ))
                                            .color(OK_GREEN),
                                        )
                                    }
                                    None => ui.label(
                                        RichText::new(format!("  ✖ {} — PATH 中未找到", dep.name))
                                            .color(ERR_RED),
                                    ),
                                };
                            }
                        }

                        ui.add_space(4.0);
                        ui.label(RichText::new("安装命令").strong());
                        if entry.install_commands.is_empty() {
                            ui.label(RichText::new("  （无）").color(MUTED));
                        } else {
                            for (i, spec) in entry.install_commands.iter().enumerate() {
                                permission_line(ui, i + 1, spec);
                            }
                        }

                        ui.add_space(4.0);
                        ui.label(RichText::new("卸载命令").strong());
                        if uninstall_commands.is_empty() {
                            ui.label(
                                RichText::new("  （没有，无法卸载；需要在 JSON 里写 uninstall-commands）")
                                    .color(MUTED),
                            );
                        } else {
                            for (i, spec) in uninstall_commands.iter().enumerate() {
                                permission_line(ui, i + 1, spec);
                            }
                        }

                        if !auto_deps.is_empty() {
                            ui.add_space(4.0);
                            ui.label(
                                RichText::new(format!(
                                    "本管理器自动安装的依赖（卸载时会询问是否一并移除）：{}",
                                    auto_deps
                                        .iter()
                                        .map(|d| format!("{}→{}", d.command, d.package))
                                        .collect::<Vec<_>>()
                                        .join("、")
                                ))
                                .color(INFO_BLUE)
                                .small(),
                            );
                        }
                    });
            });

        action
    }

    fn bottom_log(&mut self, ui: &mut Ui) {
        let mut cancel_requested = false;
        let mut clear_requested = false;
        let mut follow = self.log_follow;

        egui::Panel::bottom("w2l_log")
            .resizable(true)
            .default_size(250.0)
            .min_size(90.0)
            .show(ui, |ui| {
                ui.add_space(4.0);

                // 标题栏：当前任务信息 + 控制按钮
                ui.horizontal_wrapped(|ui| {
                    ui.label(RichText::new("运行日志").strong());
                    if !self.background.is_empty() {
                        ui.label(
                            RichText::new(format!("· 后台 {} 个", self.background.len()))
                                .color(INFO_BLUE)
                                .small(),
                        );
                    }
                    match &self.job {
                        Some(job) => {
                            let guard = job.lock();
                            let (done, total) = guard.progress();
                            let outcome = match guard.outcome {
                                JobOutcome::Running => format!("（{}）", guard.kind.gerund()),
                                JobOutcome::Succeeded => "（成功）".to_string(),
                                JobOutcome::Failed => "（失败）".to_string(),
                                JobOutcome::Cancelled => "（已取消）".to_string(),
                            };
                            ui.label(
                                RichText::new(format!(
                                    "#{} {} · {}{}",
                                    guard.id,
                                    guard.kind.label(),
                                    guard.project_id,
                                    outcome
                                ))
                                .color(INFO_BLUE),
                            );
                            ui.add(
                                egui::ProgressBar::new(if total == 0 {
                                    0.0
                                } else {
                                    done as f32 / total as f32
                                })
                                .desired_width(150.0)
                                .text(format!("{done}/{total}")),
                            );
                            // 慢任务（国内克隆仓库）让人一眼看出已经跑了多久
                            if guard.is_running() {
                                let secs = state::now_secs().saturating_sub(guard.started_at);
                                ui.label(
                                    RichText::new(format!("已耗时 {}", short_duration(secs)))
                                        .color(MUTED)
                                        .small(),
                                );
                            }
                            if guard.is_running() && ui.button("取消").clicked() {
                                cancel_requested = true;
                            }
                            drop(guard);
                        }
                        None => {
                            let empty = self.console.lock().is_empty();
                            ui.label(
                                RichText::new(if empty {
                                    "尚未执行任何命令：点击某个条目的「安装」或「卸载」开始"
                                } else {
                                    "当前没有正在运行的任务（下面是本次会话累积的输出）"
                                })
                                .color(MUTED),
                            );
                        }
                    }

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("清空").clicked() {
                            clear_requested = true;
                        }
                        ui.checkbox(&mut follow, "自动滚动到底部");
                    });
                });

                // 步骤状态
                if let Some(job) = &self.job {
                    let guard = job.lock();
                    if !guard.message.is_empty() {
                        ui.label(RichText::new(&guard.message).color(ERR_RED));
                    }
                    ui.horizontal_wrapped(|ui| {
                        for (i, status) in guard.step_status.iter().enumerate() {
                            let color = match status {
                                StepStatus::Pending => MUTED,
                                StepStatus::Running => INFO_BLUE,
                                StepStatus::Ok => OK_GREEN,
                                StepStatus::Failed => ERR_RED,
                            };
                            let text = format!("{} 第 {} 步", status.symbol(), i + 1);
                            let response = ui.label(RichText::new(text).color(color).small());
                            if let Some(step) = guard.steps.get(i) {
                                let hover = if step.needs_root {
                                    format!("{}（需要 root）", step.command)
                                } else {
                                    step.command.clone()
                                };
                                let _ = response.on_hover_text(hover);
                            }
                        }
                    });
                }

                ui.separator();

                // 累积输出：可上下滚动，也支持按住拖动（触摸屏可直接划）
                let guard = self.console.lock();
                let skipped_lines = guard.len().saturating_sub(LOG_RENDER_LIMIT);
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .stick_to_bottom(follow)
                    .scroll_source(ScrollSource {
                        drag: DragScroll::OnTouch,
                        ..Default::default()
                    })
                    .show(ui, |ui| {
                        if guard.dropped() > 0 || skipped_lines > 0 {
                            ui.label(
                                RichText::new(format!(
                                    "… 已省略较早的 {} 行输出",
                                    guard.dropped() as usize + skipped_lines
                                ))
                                .color(MUTED)
                                .small(),
                            );
                        }
                        for line in guard.lines().skip(skipped_lines) {
                            log_line_ui(ui, line);
                        }
                    });
                drop(guard);
            });

        self.log_follow = follow;
        if cancel_requested && let Some(job) = &self.job {
            job.cancel();
        }
        if clear_requested {
            self.console.clear();
        }
    }

    /// 哪个任务在等 sudo 密码（主任务优先）。
    fn password_owner(&self) -> Option<PasswordOwner> {
        if self
            .job
            .as_ref()
            .is_some_and(|job| job.password_request().is_some())
        {
            return Some(PasswordOwner::Main);
        }
        self.background
            .iter()
            .position(|task| task.job.password_request().is_some())
            .map(PasswordOwner::Background)
    }

    /// 按 `PasswordOwner` 取出对应的任务。
    fn job_for(&self, owner: &PasswordOwner) -> Option<&Job> {
        match owner {
            PasswordOwner::Main => self.job.as_ref(),
            PasswordOwner::Background(index) => self.background.get(*index).map(|t| &t.job),
        }
    }

    /// 需要 root 权限时才出现的密码输入框。
    fn password_modal(&mut self, ctx: &egui::Context) {
        let Some(owner) = self.password_owner() else {
            self.password_prompt = None;
            return;
        };
        let request = self
            .job_for(&owner)
            .and_then(|job| job.password_request());
        let Some((step, command)) = request else {
            self.password_prompt = None;
            return;
        };

        // 每次弹出新请求时清空输入并自动聚焦
        let fresh = self.password_prompt.as_ref() != Some(&(step, command.clone()));
        if fresh {
            log::info!("第 {} 步需要 root 权限，等待用户输入密码：{command}", step + 1);
            self.password_input.clear();
            self.password_prompt = Some((step, command.clone()));
        }

        let mut input = std::mem::take(&mut self.password_input);
        let mut show = self.show_password;
        let mut submit = false;
        let mut cancel = false;

        let response = egui::Modal::new(egui::Id::new("w2l_password")).show(ctx, |ui| {
            ui.set_width(540.0);
            ui.heading("这一步需要 root 权限");
            ui.add_space(6.0);
            ui.label(RichText::new(format!("第 {} 步将要执行：", step + 1)).color(MUTED));
            ui.monospace(format!("  {command}"));
            ui.add_space(10.0);

            ui.horizontal(|ui| {
                ui.label("sudo 密码：");
                let field = ui.add(
                    egui::TextEdit::singleline(&mut input)
                        .password(!show)
                        .desired_width(240.0)
                        .hint_text("输入密码后回车"),
                );
                if fresh {
                    field.request_focus();
                }
                ui.checkbox(&mut show, "显示");
                if field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    submit = true;
                }
            });

            ui.add_space(4.0);
            ui.label(
                RichText::new("密码只保存在内存中，不会写入磁盘，程序退出即消失。")
                    .color(MUTED)
                    .small(),
            );

            ui.add_space(12.0);
            ui.horizontal(|ui| {
                if ui.button("取消这一步").clicked() {
                    cancel = true;
                }
                let enabled = !input.is_empty();
                let button = egui::Button::new(RichText::new("确定并继续").strong());
                if ui.add_enabled(enabled, button).clicked() {
                    submit = true;
                }
            });
        });

        if response.should_close() {
            cancel = true;
        }

        self.password_input = input;
        self.show_password = show;

        if submit && !self.password_input.is_empty() {
            // 缓存在内存里，后面的步骤和任务就不用再问了
            log::info!("已提交 sudo 密码（只保存在内存中）");
            self.sudo_password = self.password_input.clone();
            if let Some(job) = self.job_for(&owner) {
                job.supply_password(self.sudo_password.clone());
            }
        } else if cancel && let Some(job) = self.job_for(&owner) {
            log::warn!("用户取消输入 sudo 密码");
            job.decline_password();
        }
    }

    fn dialogs(&mut self, ctx: &egui::Context) {
        let Some(dialog) = self.dialog.take() else {
            return;
        };

        // 每个对话框返回 Some(新的对话框) 表示继续显示，None 表示关闭。
        let next = match dialog {
            Dialog::MissingDeps {
                project_id,
                plan,
                choice,
            } => self.missing_deps_dialog(ctx, project_id, plan, choice),
            Dialog::ConfirmInstall {
                project_id,
                auto_packages,
                manager,
                dep_record,
                re_clone,
            } => self.confirm_install_dialog(
                ctx,
                project_id,
                auto_packages,
                manager,
                dep_record,
                re_clone,
            ),
            Dialog::ConfirmAction {
                project_id,
                label,
                describe,
                commands,
                background,
            } => self.confirm_action_dialog(ctx, project_id, label, describe, commands, background),
            Dialog::ConfirmUninstall {
                project_id,
                commands,
                removable,
                untouched,
                remove_deps,
                remove_sources,
            } => self.confirm_uninstall_dialog(
                ctx,
                project_id,
                commands,
                removable,
                untouched,
                remove_deps,
                remove_sources,
            ),
        };
        self.dialog = next;
    }

    fn missing_deps_dialog(
        &mut self,
        ctx: &egui::Context,
        project_id: String,
        plan: DepPlan,
        mut choice: DepChoice,
    ) -> Option<Dialog> {
        let mut close = false;
        let mut proceed = false;
        let mut recheck = false;

        let manager_name = plan
            .manager
            .map(|m| m.name().to_string())
            .unwrap_or_else(|| "未检测到".to_string());
        let auto_ok = plan.auto_installable();
        let blocker = self.auto_install_blocker();
        let auto_packages = plan.auto_packages();
        let hint = plan
            .manager
            .map(|m| m.manual_hint(&auto_packages))
            .unwrap_or_default();
        let present = plan.present.clone();
        let missing: Vec<(String, Option<String>, Option<String>)> = plan
            .missing
            .iter()
            .map(|m| (m.command.clone(), m.package.clone(), m.reason.clone()))
            .collect();

        let response = egui::Modal::new(egui::Id::new("w2l_missing_deps")).show(ctx, |ui| {
            ui.set_width(640.0);
            ui.heading(format!("缺少依赖：{project_id}"));
            ui.add_space(6.0);
            ui.label(
                RichText::new(format!("检测到的包管理器：{manager_name}")).color(
                    if plan.manager.is_some() {
                        OK_GREEN
                    } else {
                        WARN_AMBER
                    },
                ),
            );
            ui.add_space(6.0);

            ui.label(RichText::new("以下依赖在 PATH 中找不到：").strong());
            for (command, package, reason) in &missing {
                match package {
                    Some(package) => {
                        ui.label(
                            RichText::new(format!("  • {command} → 将安装软件包 {package}"))
                                .color(INFO_BLUE),
                        );
                    }
                    None => {
                        ui.label(
                            RichText::new(format!(
                                "  • {command} —— 无法自动安装：{}",
                                reason.clone().unwrap_or_default()
                            ))
                            .color(ERR_RED),
                        );
                    }
                }
            }

            if !present.is_empty() {
                ui.add_space(4.0);
                ui.label(
                    RichText::new(format!("已经存在的依赖不会被改动：{}", present.join("、")))
                        .color(OK_GREEN)
                        .small(),
                );
            }

            ui.add_space(10.0);
            ui.horizontal(|ui| {
                ui.selectable_value(&mut choice, DepChoice::Auto, "自动安装");
                ui.selectable_value(&mut choice, DepChoice::Manual, "手动安装");
            });

            ui.add_space(6.0);
            match choice {
                DepChoice::Auto => {
                    if auto_ok {
                        if let Some(manager) = plan.manager {
                            for command in manager.install_commands(&auto_packages) {
                                ui.monospace(format!("  $ sudo {command}"));
                            }
                        }
                        if let Some(reason) = &blocker {
                            ui.label(RichText::new(format!("⚠ {reason}")).color(WARN_AMBER));
                        } else {
                            ui.label(
                                RichText::new(
                                    "将先安装上面这些软件包，然后继续安装项目本身；\
                                     真正执行到这一步时才会询问 sudo 密码。",
                                )
                                .color(MUTED),
                            );
                        }
                    } else {
                        ui.label(
                            RichText::new("✖ 无法自动安装这些依赖，请选择「手动安装」")
                                .color(ERR_RED)
                                .strong(),
                        );
                        for (command, _, reason) in &missing {
                            if let Some(reason) = reason {
                                ui.label(
                                    RichText::new(format!("  • {command}：{reason}"))
                                        .color(WARN_AMBER)
                                        .small(),
                                );
                            }
                        }
                    }
                }
                DepChoice::Manual => {
                    ui.label("请自行执行下面的命令（或使用图形化包管理器），装好后点「重新检测」：");
                    if hint.is_empty() {
                        ui.label(
                            RichText::new("（无法给出具体的安装命令，请按发行版文档安装）")
                                .color(MUTED),
                        );
                    } else {
                        ui.monospace(format!("  $ {hint}"));
                    }
                    if ui.button("重新检测").clicked() {
                        recheck = true;
                    }
                }
            }

            ui.add_space(12.0);
            ui.horizontal(|ui| {
                if ui.button("取消").clicked() {
                    close = true;
                }
                match choice {
                    DepChoice::Auto => {
                        let enabled = auto_ok && blocker.is_none();
                        let button = egui::Button::new(RichText::new("安装依赖并继续").strong());
                        if ui.add_enabled(enabled, button).clicked() {
                            proceed = true;
                        }
                    }
                    DepChoice::Manual => {
                        ui.label(
                            RichText::new("装好依赖后点上方的「重新检测」")
                                .color(MUTED)
                                .small(),
                        );
                    }
                }
            });
        });

        if response.should_close() {
            close = true;
        }

        if recheck {
            // 重新检查依赖；都装好了就直接进入安装确认
            let entry = self.entries.iter().find(|e| e.id == project_id).cloned();
            if let Some(entry) = entry {
                let plan = deps::plan(&entry.dependency, self.manager);
                if plan.missing.is_empty() {
                    self.set_toast("依赖已齐全，可以安装了".to_string(), ToastKind::Ok);
                    return Some(Dialog::ConfirmInstall {
                        project_id,
                        auto_packages: Vec::new(),
                        manager: self.manager,
                        dep_record: DepRecord {
                            pre_existing: plan.present,
                            auto_installed: Vec::new(),
                        },
                        re_clone: false,
                    });
                }
                self.refresh_dependencies();
                return Some(Dialog::MissingDeps {
                    project_id,
                    plan,
                    choice,
                });
            }
            return None;
        }

        if proceed {
            if let Some(manager) = plan.manager {
                let auto_deps = Self::auto_deps_from_plan(&plan, manager);
                return Some(Dialog::ConfirmInstall {
                    project_id,
                    auto_packages: plan.auto_packages(),
                    manager: Some(manager),
                    dep_record: DepRecord {
                        pre_existing: plan.present.clone(),
                        auto_installed: auto_deps,
                    },
                    re_clone: false,
                });
            }
            return None;
        }

        if close {
            return None;
        }

        Some(Dialog::MissingDeps {
            project_id,
            plan,
            choice,
        })
    }

    #[allow(clippy::too_many_arguments)]
    /// 确认执行按钮里的命令。
    fn confirm_action_dialog(
        &mut self,
        ctx: &egui::Context,
        project_id: String,
        label: String,
        describe: String,
        commands: Vec<CommandSpec>,
        background: bool,
    ) -> Option<Dialog> {
        let mut close = false;
        let mut confirmed = false;

        let root_needed = commands.iter().any(|c| c.permission() == Permission::Root);

        egui::Modal::new(egui::Id::new("w2l_confirm_action")).show(ctx, |ui| {
            ui.set_width(560.0);
            ui.heading(format!("执行「{label}」"));
            ui.add_space(4.0);
            ui.label(
                RichText::new(format!("项目：{project_id}"))
                    .color(MUTED)
                    .small(),
            );
            if !describe.is_empty() {
                ui.add_space(4.0);
                ui.label(&describe);
            }

            ui.add_space(8.0);
            ui.label(RichText::new("将要依次执行：").strong());
            for (index, spec) in commands.iter().enumerate() {
                permission_line(ui, index + 1, spec);
            }

            if root_needed {
                ui.add_space(6.0);
                ui.label(
                    RichText::new("其中包含需要 root 权限的命令，执行前会先要 sudo 密码。")
                        .color(WARN_AMBER)
                        .small(),
                );
            }
            ui.add_space(4.0);
            if background {
                ui.label(
                    RichText::new(
                        "这个按钮在后台运行：点「执行」后立刻返回，不占用界面，\
                         只在开始和结束时各提示一句。",
                    )
                    .color(INFO_BLUE)
                    .small(),
                );
            }
            ui.label(
                RichText::new("这些命令由 project_list.json 的作者提供，执行前请确认内容。")
                    .color(MUTED)
                    .small(),
            );

            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if ui.button("执行").clicked() {
                    confirmed = true;
                }
                if ui.button("取消").clicked() {
                    close = true;
                }
            });
        });

        if confirmed {
            self.start_action_job(&project_id, &label, &commands, background);
            return None;
        }
        if close {
            return None;
        }
        Some(Dialog::ConfirmAction {
            project_id,
            label,
            describe,
            commands,
            background,
        })
    }

    fn confirm_install_dialog(
        &mut self,
        ctx: &egui::Context,
        project_id: String,
        auto_packages: Vec<String>,
        manager: Option<PackageManagerKind>,
        dep_record: DepRecord,
        mut re_clone: bool,
    ) -> Option<Dialog> {
        let mut close = false;
        let mut confirmed = false;

        let project_root = self.project_root.clone();
        let entry = self.entries.iter().find(|e| e.id == project_id).cloned();
        let needs_clone = entry.as_ref().is_some_and(|e| e.needs_clone());
        let clone_command = entry.as_ref().and_then(|e| e.clone_command.clone());
        let source_base =
            needs_clone.then(|| paths::project_source_base(&project_root, &project_id));
        // 目录存在 ≠ 克隆好了：可能是上次被取消留下的半成品。
        let existing_state = source_base.as_ref().map(|b| CloneState::decide(b, false));
        let sources_complete = matches!(existing_state, Some(CloneState::Existing));
        let sources_incomplete = matches!(existing_state, Some(CloneState::ReClone));

        let install_commands: Vec<CommandSpec> = entry
            .as_ref()
            .map(|e| e.install_commands.clone())
            .unwrap_or_default();
        let dep_commands: Vec<String> = manager
            .map(|m| m.install_commands(&auto_packages))
            .unwrap_or_default();
        let blocker = self.auto_install_blocker();
        let work_dir = paths::resolve_input_path(&self.work_dir_input);
        let root_needed = !dep_commands.is_empty();
        let source_text = source_base.as_ref().map(|b| b.display().to_string());

        let response = egui::Modal::new(egui::Id::new("w2l_confirm_install")).show(ctx, |ui| {
            ui.set_width(660.0);
            ui.heading(format!("确认安装：{project_id}"));
            ui.add_space(6.0);

            if root_needed {
                ui.label(
                    RichText::new("第 1 步：安装缺失的依赖（需要 root，执行到该步时才询问密码）")
                        .strong(),
                );
                for command in &dep_commands {
                    ui.monospace(format!("  $ sudo {command}"));
                }
                if let Some(reason) = &blocker {
                    ui.label(RichText::new(format!("⚠ {reason}")).color(ERR_RED));
                }
                ui.add_space(8.0);
            }

            if let (Some(base), Some(clone)) = (&source_text, &clone_command) {
                ui.label(RichText::new("接下来：把源码克隆到本项目专属的目录").strong());
                ui.monospace(format!("  源码目录：{base}"));
                if sources_incomplete {
                    ui.label(
                        RichText::new(format!(
                            "  该目录里没有完整性标记（{}），说明上次克隆没跑完，\n  \
                             将先删除整个目录再重新克隆 —— 否则构建会在半个仓库里跑。",
                            source::CLONE_MARKER
                        ))
                        .color(WARN_AMBER)
                        .small(),
                    );
                } else if sources_complete {
                    ui.label(
                        RichText::new(format!(
                            "  该目录已有完整源码（存在标记 {}），默认跳过克隆",
                            source::CLONE_MARKER
                        ))
                        .color(OK_GREEN)
                        .small(),
                    );
                    ui.checkbox(&mut re_clone, "重新克隆（先删除上面的目录）");
                }
                ui.monospace(format!("  $ {clone}"));
                ui.add_space(8.0);
            }

            ui.label(RichText::new("最后：执行项目自己的安装命令").strong());
            if install_commands.is_empty() {
                ui.label(RichText::new("  （无）").color(MUTED));
            } else {
                for (i, spec) in install_commands.iter().enumerate() {
                    permission_line(ui, i + 1, spec);
                }
            }

            if !dep_record.pre_existing.is_empty() {
                ui.add_space(6.0);
                ui.label(
                    RichText::new(format!(
                        "已经存在的依赖不会被改动：{}",
                        dep_record.pre_existing.join("、")
                    ))
                    .color(OK_GREEN)
                    .small(),
                );
            }

            if source_text.is_none() {
                ui.add_space(6.0);
                ui.label(
                    RichText::new(format!("工作目录：{}", work_dir.display()))
                        .color(MUTED)
                        .small(),
                );
            }

            ui.add_space(12.0);
            ui.horizontal(|ui| {
                if ui.button("取消").clicked() {
                    close = true;
                }
                let enabled = blocker.is_none();
                let button = egui::Button::new(RichText::new("执行安装").strong());
                if ui.add_enabled(enabled, button).clicked() {
                    confirmed = true;
                }
            });
        });

        if response.should_close() {
            close = true;
        }

        if confirmed {
            if let Some(entry) = &entry {
                let clone_state = source_base
                    .as_ref()
                    .map(|base| CloneState::decide(base, re_clone))
                    .unwrap_or(CloneState::Existing);
                let planned =
                    steps::install_steps(&project_root, entry, &auto_packages, manager, clone_state);
                self.start_job(project_id, JobKind::Install, planned, dep_record);
            }
            return None;
        }
        if close {
            return None;
        }

        Some(Dialog::ConfirmInstall {
            project_id,
            auto_packages,
            manager,
            dep_record,
            re_clone,
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn confirm_uninstall_dialog(
        &mut self,
        ctx: &egui::Context,
        project_id: String,
        commands: Vec<CommandSpec>,
        removable: Vec<AutoDep>,
        untouched: Vec<String>,
        mut remove_deps: bool,
        mut remove_sources: bool,
    ) -> Option<Dialog> {
        let mut close = false;
        let mut confirmed = false;
        let remove_packages: Vec<String> = removable.iter().map(|d| d.package.clone()).collect();
        let remove_manager = removable
            .iter()
            .find_map(|d| PackageManagerKind::from_name(&d.manager));

        let project_root = self.project_root.clone();
        let entry = self.entries.iter().find(|e| e.id == project_id).cloned();
        let source_dir = entry
            .as_ref()
            .filter(|e| e.needs_clone())
            .map(|_| paths::project_source_base(&project_root, &project_id));
        let sources_present = source_dir.as_ref().is_some_and(|b| paths::has_sources(b));
        let source_text = source_dir.as_ref().map(|b| b.display().to_string());
        let recorded_source = self
            .state
            .installed
            .get(&project_id)
            .and_then(|r| r.source_dir.clone());

        let response = egui::Modal::new(egui::Id::new("w2l_confirm_uninstall")).show(ctx, |ui| {
            ui.set_width(660.0);
            ui.heading(format!("确认卸载：{project_id}"));
            ui.add_space(6.0);

            ui.label(
                RichText::new("这些卸载命令来自 project_list.json 的 uninstall-commands。")
                    .color(MUTED),
            );

            ui.add_space(6.0);
            ui.label(RichText::new("将要依次执行：").strong());
            for (i, spec) in commands.iter().enumerate() {
                permission_line(ui, i + 1, spec);
            }

            // 依赖处理：只动自己装的
            if !removable.is_empty() {
                ui.add_space(10.0);
                ui.separator();
                ui.add_space(4.0);
                ui.label(RichText::new("本管理器在安装时自动装了这些依赖：").strong());
                for dep in &removable {
                    ui.label(
                        RichText::new(format!(
                            "  • {} → {}（由 {} 安装）",
                            dep.command, dep.package, dep.manager
                        ))
                        .color(INFO_BLUE)
                        .small(),
                    );
                }
                ui.checkbox(&mut remove_deps, "卸载项目时一并卸载这些依赖");
                if remove_deps {
                    match remove_manager {
                        Some(kind) => {
                            for command in kind.remove_commands(&remove_packages) {
                                ui.monospace(format!("  $ sudo {command}"));
                            }
                        }
                        None => {
                            ui.label(
                                RichText::new("⚠ 无法确定当初使用的包管理器，不会卸载这些依赖")
                                    .color(WARN_AMBER),
                            );
                        }
                    }
                } else {
                    ui.label(
                        RichText::new("不勾选则保留这些依赖（推荐：其他软件可能也在用）")
                            .color(MUTED)
                            .small(),
                    );
                }
            }

            if !untouched.is_empty() {
                ui.add_space(6.0);
                ui.label(
                    RichText::new(format!(
                        "安装前就已存在的依赖不会被改动：{}",
                        untouched.join("、")
                    ))
                    .color(OK_GREEN)
                    .small(),
                );
            }

            // 源码目录：本管理器自己建的，可以顺手删掉
            if let Some(base) = &source_text {
                ui.add_space(10.0);
                ui.separator();
                ui.add_space(4.0);
                ui.label(RichText::new("克隆下来的源码目录：").strong());
                ui.monospace(format!("  {base}"));
                if let Some(recorded) = &recorded_source
                    && Some(recorded) != source_text.as_ref()
                {
                    ui.label(
                        RichText::new(format!("  （安装时记录的位置：{recorded}）"))
                            .color(MUTED)
                            .small(),
                    );
                }
                if sources_present {
                    ui.checkbox(&mut remove_sources, "同时删除这个源码目录");
                } else {
                    ui.label(RichText::new("  （目录已不存在）").color(MUTED).small());
                }
            }

            ui.add_space(12.0);
            ui.horizontal(|ui| {
                if ui.button("取消").clicked() {
                    close = true;
                }
                let button = egui::Button::new(RichText::new("执行卸载").strong());
                if ui.add(button).clicked() {
                    confirmed = true;
                }
            });
        });

        if response.should_close() {
            close = true;
        }

        if confirmed {
            if let Some(entry) = &entry {
                let packages = if remove_deps && remove_manager.is_some() {
                    remove_packages
                } else {
                    Vec::new()
                };
                let planned = steps::uninstall_steps(
                    &project_root,
                    entry,
                    &commands,
                    &packages,
                    remove_manager,
                    remove_sources && sources_present,
                );
                self.start_job(project_id, JobKind::Uninstall, planned, DepRecord::default());
            }
            return None;
        }
        if close {
            return None;
        }

        Some(Dialog::ConfirmUninstall {
            project_id,
            commands,
            removable,
            untouched,
            remove_deps,
            remove_sources,
        })
    }

    /// 设置窗口：更新 / 执行 / 位置。
    fn settings_window(&mut self, ctx: &egui::Context) {
        let was_open = self.show_settings;
        let mut open = self.show_settings;
        let mut update_now = false;
        let mut add_row = false;
        let mut remove_row: Option<usize> = None;

        let max_height = (ctx.content_rect().height() - 96.0).max(240.0);
        egui::Window::new("设置")
            .open(&mut open)
            .resizable(true)
            .vscroll(true)
            .default_size([620.0, 560.0])
            .max_height(max_height)
            .show(ctx, |ui| {
                // ---------------- 更新 ----------------
                ui.heading("更新");
                ui.add_space(4.0);
                ui.checkbox(&mut self.state.auto_update_on_start, "启动时自动更新列表")
                    .on_hover_text("每次启动时后台从配置仓库拉一次最新的列表");
                ui.add_space(2.0);
                ui.label(
                    RichText::new(format!("配置仓库：{CONFIG_REPO_URL}"))
                        .color(MUTED)
                        .small(),
                );
                ui.add_space(4.0);
                let response = ui.add_enabled(!self.busy(), egui::Button::new("立即更新"));
                if response.clicked() {
                    update_now = true;
                }
                ui.label(
                    RichText::new("更新会把仓库里的 project_list.json 覆盖到项目根目录")
                        .color(MUTED)
                        .small(),
                );

                ui.add_space(10.0);
                ui.separator();

                // ---------------- 执行 ----------------
                ui.heading("执行");
                ui.add_space(4.0);
                ui.horizontal_wrapped(|ui| {
                    ui.label("工作目录：");
                    let width = (ui.available_width() - 120.0).clamp(160.0, 420.0);
                    ui.add(
                        egui::TextEdit::singleline(&mut self.work_dir_input)
                            .desired_width(width)
                            .hint_text("没有 clone-command 的项目在此目录下构建"),
                    );
                    if ui.small_button("用项目目录").clicked() {
                        self.work_dir_input = self.project_root.display().to_string();
                    }
                });

                ui.add_space(8.0);
                ui.label(RichText::new("命令环境变量").strong());
                ui.label(
                    RichText::new("下面这些变量会在每条命令执行前先导出（root 命令同样生效）")
                        .color(MUTED)
                        .small(),
                );
                ui.add_space(4.0);

                let mut invalid: Vec<String> = Vec::new();
                for (index, var) in self.state.env.iter_mut().enumerate() {
                    ui.horizontal(|ui| {
                        ui.add(
                            egui::TextEdit::singleline(&mut var.key)
                                .desired_width(150.0)
                                .hint_text("KEY"),
                        );
                        ui.label("=");
                        ui.add(
                            egui::TextEdit::singleline(&mut var.value)
                                .desired_width(240.0)
                                .hint_text("值"),
                        );
                        if ui.small_button("删除").clicked() {
                            remove_row = Some(index);
                        }
                    });
                    if !var.key.trim().is_empty() && !var.is_usable() {
                        invalid.push(var.key.clone());
                    }
                }
                if ui.button("+ 添加一行").clicked() {
                    add_row = true;
                }
                if !invalid.is_empty() {
                    ui.add_space(2.0);
                    ui.label(
                        RichText::new(format!(
                            "⚠ 这些键名不合法，会被忽略：{}（只能字母/下划线开头）",
                            invalid.join("、")
                        ))
                        .color(WARN_AMBER)
                        .small(),
                    );
                }

                ui.add_space(10.0);
                ui.separator();

                // ---------------- 位置 ----------------
                ui.heading("位置");
                ui.add_space(4.0);
                ui.label(
                    RichText::new(format!("项目目录：{}", self.project_root.display()))
                        .color(MUTED)
                        .small(),
                );
                if let Some(path) = &self.state_file {
                    ui.label(
                        RichText::new(format!("状态文件：{}", path.display()))
                            .color(MUTED)
                            .small(),
                    );
                }
                match crate::logging::current_path() {
                    Some(path) => {
                        ui.label(
                            RichText::new(format!(
                                "日志文件：{}（{} 级）",
                                path.display(),
                                crate::logging::level_name(crate::logging::current_level())
                            ))
                            .color(MUTED)
                            .small(),
                        );
                    }
                    None => {
                        ui.label(RichText::new("日志：仅输出到 stderr").color(MUTED).small());
                    }
                }
                ui.add_space(4.0);
                ui.label(
                    RichText::new("设置保存在状态文件里，关闭本窗口时写入")
                        .color(MUTED)
                        .small(),
                );
            });

        if let Some(index) = remove_row {
            self.state.env.remove(index);
        }
        if add_row {
            self.state.env.push(state::EnvVar::default());
        }
        if update_now {
            self.begin_update(false);
        }

        // 关窗时保存（不在每次按键时写文件）
        if was_open && !open {
            log::info!(
                "保存设置：自动更新={}，环境变量 {} 条",
                self.state.auto_update_on_start,
                self.state.env.len()
            );
            self.save_state();
        }
        self.show_settings = open;
    }

    fn help_window(&mut self, ctx: &egui::Context) {
        let mut open = self.show_help;
        // 内容比屏幕高时必须能滚动，否则下面的说明根本看不到
        let max_height = (ctx.content_rect().height() - 96.0).max(240.0);
        egui::Window::new("project_list.json 格式说明")
            .open(&mut open)
            .resizable(true)
            .vscroll(true)
            .default_size([660.0, 560.0])
            .max_height(max_height)
            .show(ctx, |ui| {
                ui.label("每个条目支持以下字段：");
                ui.add_space(4.0);
                ui.label(RichText::new("• id：唯一标识，显示为列表标题，也用作源码目录名").strong());
                ui.label(RichText::new("• author：作者 / 维护者，显示在标题旁边").strong());
                ui.label(RichText::new("• describe：说明文字，可包含换行").strong());
                ui.label(
                    RichText::new("• github-url：项目链接，界面上显示成可点击的超链接").strong(),
                );
                ui.label(RichText::new("• bilibili-url：B 站视频链接，同样显示成超链接").strong());
                ui.label(
                    RichText::new("• platform：适用平台数组，如 [\"all-linux\"]，显示成小标签").strong(),
                );
                ui.label(RichText::new("• tips：额外提示（可多行），显示成醒目的提示框").strong());
                ui.label(RichText::new("• clone-command：把源码克隆下来的命令").strong());
                ui.label(
                    RichText::new("• dependency：依赖的命令名数组，会检查是否在 PATH 中").strong(),
                );
                ui.label(RichText::new("• install-commands：安装命令数组").strong());
                ui.label(
                    RichText::new("• uninstall-commands：可选，卸载命令数组")
                        .strong()
                        .color(OK_GREEN),
                );
                ui.label(
                    RichText::new(
                        "• button-1 / button-2 / …：操作按钮，数字任意大小都认；\
                         每项含 name（按钮文字）、describe（悬停说明）、commands（点击后执行的命令数组）、\
                         background（可选，true 表示放后台跑，不占用界面）",
                    )
                    .strong()
                    .color(INFO_BLUE),
                );
                ui.add_space(6.0);
                ui.label("命令的两种写法（permission 缺省为 normal）：");
                ui.add(
                    egui::TextEdit::multiline(&mut self.help_sample)
                        .code_editor()
                        .desired_rows(20)
                        .desired_width(f32::INFINITY),
                );

                ui.add_space(8.0);
                ui.label(RichText::new("安装时会发生什么").strong());
                ui.label(
                    "1. 检查依赖：已存在的只记录，卸载时不会动；缺失的询问自动安装还是手动安装。\n\
                     2. 准备源码：把仓库克隆到 <项目根>/sources/<id> 这个本项目专属的目录里，\
                     不同项目互不干扰；目录已存在时默认跳过，也可以勾选重新克隆。\n\
                     3. 执行 install-commands；permission 为 root 的命令在真正执行到那一步时\
                     才弹窗询问 sudo 密码，密码只留在内存里。",
                );
                ui.add_space(8.0);
                ui.label(RichText::new("卸载时会发生什么").strong());
                ui.label(
                    "1. 执行 uninstall-commands（有源码目录时在源码根目录里执行）。\n\
                     2. 由本管理器自动安装的依赖会被列出，默认保留，勾选后才卸载。\n\
                     3. 安装前就已存在的依赖绝不会被改动。\n\
                     4. 可以顺手删除克隆下来的源码目录（默认保留）。",
                );
                ui.add_space(8.0);
                ui.label(RichText::new("卸载命令").strong());
                ui.label(
                    "卸载只执行 uninstall-commands 里写好的命令，**不做任何推导**：\
                     从 install-commands 反推（比如 make install → make uninstall）并不可靠，\
                     改写出来的命令有可能删错东西。没有写 uninstall-commands 的条目无法卸载，\
                     列表里会直接提示。",
                );

                ui.add_space(8.0);
                ui.label(RichText::new("日志").strong());
                match crate::logging::current_path() {
                    Some(path) => {
                        ui.label(format!("日志文件：{}（{} 级）", path.display(),
                            crate::logging::level_name(crate::logging::current_level())));
                    }
                    None => {
                        ui.label("没有可写的日志文件，日志只输出到 stderr。");
                    }
                }
                ui.label(
                    "用 W2L_LOG 调整级别：error / warn / info（默认）/ debug / trace。\n                     debug 及以上会把每条命令的原始输出也写进日志，排查安装失败最有用；\n                     用 W2L_LOG_FILE 可以指定日志文件位置。",
                );

                ui.add_space(8.0);
                ui.separator();
                match &self.font {
                    Ok(font) => {
                        ui.label(
                            RichText::new(format!(
                                "界面字体：{}（{}）",
                                font.display_name(),
                                font.path.display()
                            ))
                            .color(MUTED)
                            .small(),
                        );
                    }
                    Err(err) => {
                        ui.label(RichText::new(format!("⚠ {err}")).color(WARN_AMBER).small());
                    }
                }
            });
        self.show_help = open;
    }
}

/// 帮助窗口里展示的 JSON 示例。
const HELP_SAMPLE: &str = r#"{
    "lists": [
        {
            "id": "ChenPi11-cmd",
            "author": "ChenPi11",
            "describe": "Windows cmd.exe 命令解释器在 Unix 上的忠实重实现。",
            "github-url": "https://github.com/ChenPi11/cmd",
            "bilibili-url": "https://www.bilibili.com/video/BV1wkuH64EE8",
            "platform": ["all-linux"],
            "tips": "作者留的额外说明，会显示成提示框",
            "clone-command": "git clone --depth=1 https://github.com/ChenPi11/cmd.git",
            "dependency": ["make"],
            "install-commands": [
                { "permission": "normal", "command": "make" },
                { "permission": "normal", "command": "make install PREFIX=/usr/local" }
            ],
            "uninstall-commands": [
                { "permission": "root", "command": "rm -f /usr/local/bin/cmd" }
            ],
            "button-1": {
                "name": "运行",
                "describe": "打开一个新的 cmd",
                "background": true,
                "commands": [ { "permission": "normal", "command": "cmd" } ]
            },
            "button-2": { "name": "终止", "commands": [ "pkill -f cmd" ] }
        }
    ]
}"#;

/// 把秒数写成「1 分 05 秒」这样的人话。
fn short_duration(secs: u64) -> String {
    if secs < 60 {
        format!("{secs} 秒")
    } else {
        format!("{} 分 {:02} 秒", secs / 60, secs % 60)
    }
}

/// 链接的显示文本：去掉协议和 `www.`，看着短一点。
///
/// 打开链接用的仍是原始 URL，完整地址放在悬停提示里。
fn short_url(url: &str) -> String {
    let trimmed = url
        .trim()
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_start_matches("www.");
    trimmed.trim_end_matches('/').to_string()
}

/// 状态行里用于分组的小竖线。
fn status_divider(ui: &mut Ui) {
    ui.label(RichText::new("│").color(MUTED).small());
}

/// 统一的字体与间距，让界面不至于挤在一起。
fn apply_style(ctx: &egui::Context) {
    use eframe::egui::{FontFamily, FontId, TextStyle};

    // 亮色 / 暗色主题都改一遍
    ctx.all_styles_mut(|style| {
        style.text_styles = [
            (
                TextStyle::Heading,
                FontId::new(21.0, FontFamily::Proportional),
            ),
            (TextStyle::Body, FontId::new(15.0, FontFamily::Proportional)),
            (
                TextStyle::Button,
                FontId::new(15.0, FontFamily::Proportional),
            ),
            (TextStyle::Small, FontId::new(12.5, FontFamily::Proportional)),
            (
                TextStyle::Monospace,
                FontId::new(14.0, FontFamily::Monospace),
            ),
        ]
        .into();
        // 松一点的行距和按钮内边距，读起来更舒服
        style.spacing.item_spacing = egui::vec2(9.0, 7.0);
        style.spacing.button_padding = egui::vec2(11.0, 5.0);
        style.spacing.window_margin = egui::Margin::same(12);
        style.spacing.indent = 18.0;
        style.spacing.extra_text_line_spacing = 2.0;
    });
}

/// 渲染一条带权限标注的命令。
fn permission_line(ui: &mut Ui, index: usize, spec: &CommandSpec) {
    let needs_root = spec.permission().needs_root();
    ui.horizontal_wrapped(|ui| {
        ui.monospace(format!("  {index}. {}", spec.command()));
        if needs_root {
            ui.label(RichText::new("root").color(WARN_AMBER).small());
        }
    });
}

fn log_line_ui(ui: &mut Ui, line: &LogLine) {
    let text = if line.text.starts_with('─') {
        // 任务分隔线
        RichText::new(&line.text).color(MUTED).strong()
    } else {
        let color = match line.stream {
            LogStream::Info => INFO_BLUE,
            LogStream::Stdout => ui.visuals().text_color(),
            LogStream::Stderr => ERR_RED,
        };
        RichText::new(&line.text).monospace().color(color)
    };
    ui.add(egui::Label::new(text).wrap());
}

fn project_root_of(list_path: &Path) -> PathBuf {
    list_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn file_label(path: &Path) -> String {
    path.file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("project_list.json")
        .to_string()
}

fn running_as_root() -> bool {
    #[cfg(unix)]
    {
        // SAFETY: geteuid 无参数、无副作用。
        unsafe { libc::geteuid() == 0 }
    }
    #[cfg(not(unix))]
    {
        false
    }
}

impl eframe::App for ManagerApp {
    /// 每帧在绘制之前调用：只做状态更新，不画界面。
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if ctx.input(|i| i.modifiers.command && i.key_pressed(egui::Key::R)) {
            self.reload();
        }
        self.tick();
        // 任务在等密码时要立刻重绘
        if self.password_owner().is_some() {
            ctx.request_repaint();
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // Context 可以廉价克隆，这样弹窗（Window/Modal 仍以 Context 为入口）
        // 就能在面板绘制期间独立使用。
        let ctx = ui.ctx().clone();

        self.top_bar(ui);
        self.bottom_log(ui);
        egui::CentralPanel::default().show(ui, |ui| self.central(ui));
        self.dialogs(&ctx);
        self.password_modal(&ctx);
        self.settings_window(&ctx);
        self.help_window(&ctx);

        if self.busy() {
            ctx.request_repaint_after(Duration::from_millis(120));
        } else if !self.background.is_empty() {
            // 后台任务可能要跑很久（比如启动一个 GUI 程序），
            // 只是为了让结束时能被发现，不必刷太勤。
            ctx.request_repaint_after(Duration::from_millis(800));
        } else if self.auto_reload {
            ctx.request_repaint_after(AUTO_RELOAD_INTERVAL);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{DepRecord, ManagerState};

    fn setup(tag: &str, list_json: &str) -> (ManagerApp, PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "w2l-app-{tag}-{}-{}",
            std::process::id(),
            state::now_secs()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let list_path = dir.join("project_list.json");
        fs::write(&list_path, list_json).unwrap();

        // 字体加载需要 egui 上下文，这里用不到，直接给一个错误值。
        let mut app = ManagerApp::build(list_path, Err("测试环境不加载字体".to_string()));
        app.reload();
        // 别碰用户真实的 XDG 状态文件
        app.state_paths = vec![dir.join("state.json")];
        app.state = ManagerState::default();
        app.state_file = None;
        (app, dir)
    }

    const ONE_ENTRY: &str = r#"{
        "lists": [
            {
                "id": "demo",
                "describe": "演示项目",
                "github-url": "https://github.com/example/demo",
                "clone-command": "git clone https://github.com/example/demo.git",
                "install-commands": [ { "permission": "normal", "command": "make install" } ],
                "uninstall-commands": [ { "permission": "root", "command": "rm -f /usr/local/bin/demo" } ]
            }
        ]
    }"#;

    /// 回归测试：`central()` 渲染时会临时把 `self.entries` take 走，
    /// 这时处理点击绝不能再去 `self.entries` 里查表，否则永远查不到，
    /// 用户看到的就是「列表里已经找不到 xxx 了」而且什么都不执行。
    #[test]
    fn clicking_install_works_even_while_entries_are_taken() {
        let (mut app, dir) = setup("click-install", ONE_ENTRY);
        assert_eq!(app.entries.len(), 1);

        let entry = app.entries[0].clone();
        let action = Action::Install(Box::new(entry));

        // 模拟 central() 中间态：条目已被 take 走
        let taken = std::mem::take(&mut app.entries);
        assert!(app.entries.is_empty());
        app.handle_action(action);

        assert!(
            matches!(app.dialog, Some(Dialog::ConfirmInstall { .. })),
            "应打开安装确认框"
        );
        assert!(app.toast.is_none(), "不该出现任何错误提示");

        app.entries = taken;
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn clicking_uninstall_works_even_while_entries_are_taken() {
        let (mut app, dir) = setup("click-uninstall", ONE_ENTRY);
        let entry = app.entries[0].clone();

        // 先让它处于「已安装」状态，卸载按钮才可用
        app.state
            .mark_installed("demo", vec!["make install".into()], None, DepRecord::default());
        assert!(app.state.is_installed("demo"));

        let action = Action::Uninstall(Box::new(entry));
        let taken = std::mem::take(&mut app.entries);
        app.handle_action(action);

        match &app.dialog {
            Some(Dialog::ConfirmUninstall { commands, .. }) => {
                assert_eq!(commands.len(), 1);
                assert_eq!(commands[0].command(), "rm -f /usr/local/bin/demo");
                assert!(commands[0].permission().needs_root());
            }
            _ => panic!("应打开卸载确认框"),
        }
        assert!(app.toast.is_none(), "不该出现任何错误提示");

        app.entries = taken;
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn install_dialog_shows_clone_target_and_source_isolation() {
        let (mut app, dir) = setup("clone-target", ONE_ENTRY);
        let entry = app.entries[0].clone();
        app.handle_action(Action::Install(Box::new(entry)));

        // 确认框的数据源：源码目录应当是 <项目根>/sources/<id>
        assert!(matches!(app.dialog, Some(Dialog::ConfirmInstall { .. })));
        let expected = dir.join("sources").join("demo");
        assert_eq!(
            paths::project_source_base(&project_root_of(&app.list_path), "demo"),
            expected
        );

        let _ = fs::remove_dir_all(&dir);
    }

    /// 回归测试：project_list.json 在程序运行时被改写，必须能自动刷新。
    #[test]
    fn detects_project_list_changes_while_running() {
        let (mut app, dir) = setup("reload", ONE_ENTRY);
        assert_eq!(app.entries.len(), 1);
        assert_eq!(app.entries[0].id, "demo");

        // 内容没变 → 不该重载
        assert!(!app.refresh_if_changed());

        // 运行时被别的程序改写（这里模拟“项目列表被更新”）
        let updated = ONE_ENTRY.replace("demo", "demo-renamed");
        fs::write(dir.join("project_list.json"), &updated).unwrap();
        assert!(app.refresh_if_changed(), "应检测到内容变化");
        assert_eq!(app.entries[0].id, "demo-renamed");

        // 已经读过了，不该反复重载
        assert!(!app.refresh_if_changed());

        // 文件被删掉也要能发现
        fs::remove_file(dir.join("project_list.json")).unwrap();
        assert!(app.refresh_if_changed(), "文件消失也应被发现");
        assert!(app.load_error.is_some());

        let _ = fs::remove_dir_all(&dir);
    }

    /// 只改一个字符也要能发现（内容哈希，而不是 mtime + 大小）。
    #[test]
    fn detects_single_character_edits() {
        let (mut app, dir) = setup("tiny-edit", ONE_ENTRY);
        let path = dir.join("project_list.json");
        let original = fs::read_to_string(&path).unwrap();

        // 把 "normal" 换成 "normal " 是无效 JSON，这里换成等长的改动
        let edited = original.replace("演示项目", "演示项M");
        fs::write(&path, edited).unwrap();
        assert!(app.refresh_if_changed(), "等长修改也必须被发现");

        let _ = fs::remove_dir_all(&dir);
    }

    const OTHER_ENTRY: &str = r#"{
        "lists": [
            {
                "id": "another-project",
                "author": "someone",
                "describe": "来自配置仓库的条目",
                "github-url": "https://github.com/example/another",
                "bilibili-url": "https://www.bilibili.com/video/BVxxxx",
                "install-commands": [ { "permission": "normal", "command": "make" } ]
            }
        ]
    }"#;

    /// 点「立即更新」成功后：项目根目录下的列表被换成最新的一份。
    #[test]
    fn update_reloads_the_list_from_the_project_root() {
        let (mut app, dir) = setup("update", ONE_ENTRY);
        assert_eq!(app.entries[0].id, "demo");

        // 模拟更新流程的结果：配置仓库里的内容已经被取到项目根目录
        fs::write(dir.join("project_list.json"), OTHER_ENTRY).unwrap();

        app.apply_update();

        assert_eq!(app.entries.len(), 1);
        assert_eq!(app.entries[0].id, "another-project");
        assert_eq!(app.entries[0].author(), Some("someone"));
        assert_eq!(app.entries[0].links().len(), 2, "github + bilibili");
        assert!(app.load_error.is_none());

        // 列表永远读项目根目录下那一份
        assert_eq!(app.list_path, dir.join("project_list.json"));
        assert_eq!(
            app.list_path_input,
            dir.join("project_list.json").display().to_string()
        );
        assert!(matches!(
            app.toast.as_ref().map(|t| t.kind),
            Some(ToastKind::Ok)
        ));

        let _ = fs::remove_dir_all(&dir);
    }

    /// 克隆出来的目录里没有列表文件时，要报错而不是把列表清空。
    #[test]
    fn update_without_the_list_file_reports_an_error() {
        let (mut app, dir) = setup("update-missing", ONE_ENTRY);
        let before = app.list_path.clone();

        // 更新后项目根目录下没有列表文件（比如仓库结构不对）
        fs::remove_file(dir.join("project_list.json")).unwrap();
        app.apply_update();

        assert_eq!(app.list_path, before, "不该切换路径");
        assert_eq!(app.entries[0].id, "demo", "原有列表应保持不变");
        assert!(matches!(
            app.toast.as_ref().map(|t| t.kind),
            Some(ToastKind::Error)
        ));

        let _ = fs::remove_dir_all(&dir);
    }


    /// 关掉「启动自动更新」后不该发起任何任务（也就不会联网）。
    /// 后台按钮：不占任务位、不卡界面，结束后提示一句。
    #[test]
    fn background_buttons_do_not_block_the_interface() {
        let (mut app, dir) = setup("background", ONE_ENTRY);

        app.start_action_job(
            "demo",
            "启动",
            &[CommandSpec::normal("sleep 3")],
            true,
        );

        // 关键：主任务位是空的，界面照常能用
        assert!(app.job.is_none(), "后台任务不该占用主任务位");
        assert!(!app.busy(), "界面不该被卡住");
        assert_eq!(app.background.len(), 1);
        assert!(app.dialog.is_none(), "后台任务不该弹出挡界面的对话框");

        // 这时候还能开别的任务（比如点安装）—— 只检查不会被 busy 拦下
        assert!(!app.busy());

        // 等它结束：走真实的每帧 tick()，而不是直接调内部函数
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        while !app.background.is_empty() {
            assert!(
                std::time::Instant::now() < deadline,
                "后台任务没能在 15 秒内结束"
            );
            app.tick();
            std::thread::sleep(Duration::from_millis(50));
        }

        assert!(app.background.is_empty(), "结束后应该移出后台列表");
        let toast = app.toast.as_ref().expect("结束时应提示一句");
        assert!(toast.text.contains("后台任务已结束"), "{}", toast.text);

        let _ = fs::remove_dir_all(&dir);
    }

    /// 前台按钮仍然占用任务位（行为和以前一样）。
    #[test]
    fn foreground_buttons_still_occupy_the_job_slot() {
        let (mut app, dir) = setup("foreground", ONE_ENTRY);

        app.start_action_job("demo", "运行", &[CommandSpec::normal("sleep 3")], false);

        assert!(app.job.is_some(), "前台按钮应该占用主任务位");
        assert!(app.busy());
        assert!(app.background.is_empty());

        if let Some(job) = &app.job {
            job.cancel();
        }
        let _ = fs::remove_dir_all(&dir);
    }

    const BACKGROUND_ENTRY: &str = r#"{
        "lists": [
            {
                "id": "bg-demo",
                "github-url": "https://github.com/example/bg",
                "install-commands": [ { "permission": "normal", "command": "make install" } ],
                "button-1": {
                    "name": "启动",
                    "describe": "后台跑",
                    "background": true,
                    "commands": [ "sleep 1" ]
                },
                "button-2": { "name": "停止", "commands": [ "true" ] }
            }
        ]
    }"#;

    /// JSON 里的 background 要一路传到确认框（再由它决定怎么起任务）。
    #[test]
    fn background_flag_flows_from_json_into_the_confirm_dialog() {
        let (mut app, dir) = setup("bg-dialog", BACKGROUND_ENTRY);
        let entry = app.entries[0].clone();
        assert_eq!(entry.buttons.len(), 2);
        assert!(entry.buttons[0].background);
        assert!(!entry.buttons[1].background, "没写 background 就是前台");

        app.begin_action(&entry, 0);
        match app.dialog.as_ref() {
            Some(Dialog::ConfirmAction {
                background,
                label,
                commands,
                ..
            }) => {
                assert!(*background, "确认框要带上后台标记");
                assert_eq!(label, "启动");
                assert_eq!(commands.len(), 1);
            }
            _ => panic!("应该弹出确认框"),
        }

        app.dialog = None;
        app.begin_action(&entry, 1);
        match app.dialog.as_ref() {
            Some(Dialog::ConfirmAction { background, .. }) => {
                assert!(!*background, "停止按钮仍是前台执行")
            }
            _ => panic!("应该弹出确认框"),
        }

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn auto_update_is_skipped_when_disabled() {
        let (mut app, dir) = setup("auto-off", ONE_ENTRY);
        app.state.auto_update_on_start = false;

        app.maybe_auto_update();

        assert!(app.job.is_none(), "关闭时不该发起更新任务");
        assert!(app.toast.is_none(), "关闭时也不该打扰用户");

        let _ = fs::remove_dir_all(&dir);
    }

    /// 启动自动更新占着任务位时，用户点安装应该能把它挤掉，而不是被挡住。
    #[test]
    fn user_action_preempts_the_automatic_update() {
        let (mut app, dir) = setup("preempt", ONE_ENTRY);
        let entry = app.entries[0].clone();

        let console = Console::new(false);
        app.job = Some(Job::spawn(JobRequest {
            project_id: steps::CONFIG_DIR_NAME.to_string(),
            kind: JobKind::Update,
            steps: vec![crate::exec::JobStep::user("sleep 30")],
            work_dir: dir.clone(),
            sudo_password: None,
            source_base: None,
            env: Vec::new(),
            console,
        }));
        app.update_automatic = true;
        assert!(app.busy());

        app.handle_action(Action::Install(Box::new(entry)));

        assert!(!app.update_automatic, "自动更新标记应被清掉");
        assert!(
            matches!(app.dialog, Some(Dialog::ConfirmInstall { .. })),
            "安装确认框应该正常弹出"
        );
        assert!(app.toast.is_none() || !matches!(app.toast.as_ref().unwrap().kind, ToastKind::Warn));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn action_while_busy_is_rejected_with_a_toast() {
        let (mut app, dir) = setup("busy", ONE_ENTRY);
        let entry = app.entries[0].clone();

        // 造一个正在运行的任务
        let console = Console::new(false);
        app.job = Some(Job::spawn(JobRequest {
            project_id: "other".to_string(),
            kind: JobKind::Install,
            steps: vec![crate::exec::JobStep::user("sleep 5")],
            work_dir: dir.clone(),
            sudo_password: None,
            source_base: None,
            env: Vec::new(),
            console,
        }));
        assert!(app.busy());

        app.handle_action(Action::Install(Box::new(entry)));
        assert!(app.dialog.is_none(), "忙碌时不该打开对话框");
        assert!(app.toast.is_some());

        if let Some(job) = &app.job {
            job.cancel();
        }
        let _ = fs::remove_dir_all(&dir);
    }
}
