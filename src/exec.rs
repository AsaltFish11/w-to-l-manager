//! 后台命令执行：按顺序跑一组命令，实时把输出流回界面**并镜像到命令行**，
//! 支持逐步提权与取消。
//!
//! 提权策略：先用 `sudo -S -v` 校验并缓存密码（密码只存在于内存中），
//! 之后需要 root 的命令用 `sudo -n` 非交互执行，不需要 root 的命令直接执行，
//! 避免把密码喂给被测程序的标准输入。

use std::collections::VecDeque;
use std::io::{BufReader, Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

use crate::paths;
use crate::state::now_secs;

/// 日志保留的最大行数，超出后丢弃最旧的行。
pub const MAX_LOG_LINES: usize = 50_000;

/// 长时间运行的命令每隔这么久报一次「还在跑」，方便区分「慢」和「卡死」。
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(10);

/// 一次操作是安装还是卸载。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobKind {
    Install,
    Uninstall,
    /// 拉取最新的 project_list.json（克隆配置仓库）。
    Update,
    /// 点击条目里的 `button-N` 执行的操作命令。
    Action,
}

impl JobKind {
    pub fn label(self) -> &'static str {
        match self {
            JobKind::Install => "安装",
            JobKind::Uninstall => "卸载",
            JobKind::Update => "更新列表",
            JobKind::Action => "操作",
        }
    }

    /// 进行时文案，例如“正在安装”。
    pub fn gerund(self) -> &'static str {
        match self {
            JobKind::Install => "正在安装",
            JobKind::Uninstall => "正在卸载",
            JobKind::Update => "正在更新列表",
            JobKind::Action => "正在执行",
        }
    }

    pub fn past(self) -> &'static str {
        match self {
            JobKind::Install => "安装",
            JobKind::Uninstall => "卸载",
            JobKind::Update => "更新列表",
            JobKind::Action => "操作",
        }
    }

    /// 会不会改动安装状态（只有安装/卸载会）。
    pub fn changes_install_state(self) -> bool {
        matches!(self, JobKind::Install | JobKind::Uninstall)
    }
}

/// 某一步的工作目录。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkDir {
    /// 明确指定的目录。
    Fixed(PathBuf),
    /// 项目源码根目录：克隆完成后才能确定，由工作线程延迟解析。
    ProjectSource,
}

/// 一条待执行的命令。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobStep {
    pub command: String,
    /// 需要 root 权限（例如 `make install` 或调用包管理器）。
    /// 这种步骤执行到跟前才会向用户索要密码。
    pub needs_root: bool,
    /// 失败也不中断整个任务。
    ///
    /// 用于「卸载后清理依赖 / 源码」这类尽力而为的步骤：目标可能已经被别人删掉了，
    /// 这时不该把整个卸载判定为失败（那会导致状态文件不被清理）。
    pub optional: bool,
    /// 覆盖任务默认的工作目录。
    pub work_dir: Option<WorkDir>,
}

impl JobStep {
    /// 普通命令，用当前用户身份执行。
    pub fn user(command: impl Into<String>) -> Self {
        Self {
            command: command.into(),
            needs_root: false,
            optional: false,
            work_dir: None,
        }
    }

    /// 需要 root 的命令。
    pub fn root(command: impl Into<String>) -> Self {
        Self {
            command: command.into(),
            needs_root: true,
            optional: false,
            work_dir: None,
        }
    }

    /// 标记为「失败可以忽略」。
    pub fn optional(mut self) -> Self {
        self.optional = true;
        self
    }

    /// 在指定目录里执行这一步。
    pub fn in_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.work_dir = Some(WorkDir::Fixed(dir.into()));
        self
    }

    /// 在项目的源码根目录里执行这一步（克隆完成后自动解析）。
    pub fn in_project_source(mut self) -> Self {
        self.work_dir = Some(WorkDir::ProjectSource);
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepStatus {
    Pending,
    Running,
    Ok,
    Failed,
}

impl StepStatus {
    pub fn symbol(self) -> &'static str {
        match self {
            StepStatus::Pending => "○",
            StepStatus::Running => "▶",
            StepStatus::Ok => "✔",
            StepStatus::Failed => "✖",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogStream {
    /// 管理器自己输出的提示。
    Info,
    Stdout,
    Stderr,
}

#[derive(Debug, Clone)]
pub struct LogLine {
    /// 产生这行日志的任务编号，0 表示与具体任务无关。
    pub job_id: u64,
    /// 所属项目 id，便于在累积日志里区分。
    pub project_id: String,
    pub step: usize,
    pub stream: LogStream,
    pub text: String,
}

/// 累积的控制台输出。
///
/// 所有任务都往同一个缓冲区追加，因此界面里能看到本次会话的**全部**输出；
/// 每一行同时也会镜像到进程自己的 stdout / stderr，从终端启动时就能看到。
#[derive(Clone)]
pub struct Console {
    inner: Arc<Mutex<ConsoleInner>>,
}

pub struct ConsoleInner {
    lines: VecDeque<LogLine>,
    dropped: u64,
    mirror_to_terminal: bool,
}

impl Console {
    pub fn new(mirror_to_terminal: bool) -> Self {
        Self {
            inner: Arc::new(Mutex::new(ConsoleInner {
                lines: VecDeque::new(),
                dropped: 0,
                mirror_to_terminal,
            })),
        }
    }

    pub fn lock(&self) -> MutexGuard<'_, ConsoleInner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn clear(&self) {
        let mut guard = self.lock();
        guard.lines.clear();
        guard.dropped = 0;
    }

    /// 追加一行，并按需镜像到命令行。
    pub fn push(&self, line: LogLine) {
        let mirror = {
            let mut guard = self.lock();
            let mirror = guard.mirror_to_terminal;
            guard.lines.push_back(line.clone());
            while guard.lines.len() > MAX_LOG_LINES {
                guard.lines.pop_front();
                guard.dropped += 1;
            }
            mirror
        };

        if mirror {
            match line.stream {
                // 错误输出走 stderr，其余走 stdout，方便在命令行里分流。
                LogStream::Stderr => eprintln!("{}", line.text),
                _ => println!("{}", line.text),
            }
        }
    }
}

impl ConsoleInner {
    pub fn lines(&self) -> impl Iterator<Item = &LogLine> {
        self.lines.iter()
    }

    pub fn len(&self) -> usize {
        self.lines.len()
    }

    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    pub fn dropped(&self) -> u64 {
        self.dropped
    }
}

/// 界面用来显示「这个任务正在等密码」的状态。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PasswordState {
    /// 没有在等待。
    Idle,
    /// 正在等待用户输入，附带需要 root 的步骤与命令。
    Waiting { step: usize, command: String },
    /// 用户已经提供（本次任务内会被复用）。
    Supplied(String),
    /// 用户拒绝提供。
    Declined,
}

impl PasswordState {
    fn waiting_on(&self) -> Option<(usize, String)> {
        match self {
            PasswordState::Waiting { step, command } => Some((*step, command.clone())),
            _ => None,
        }
    }
}

/// 任务与界面之间同步 sudo 密码的小门闸。
struct PasswordGate {
    state: Mutex<PasswordState>,
    changed: Condvar,
}

impl PasswordGate {
    fn new(initial: Option<String>) -> Self {
        Self {
            state: Mutex::new(match initial {
                Some(password) => PasswordState::Supplied(password),
                None => PasswordState::Idle,
            }),
            changed: Condvar::new(),
        }
    }

    fn lock(&self) -> MutexGuard<'_, PasswordState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobOutcome {
    Running,
    Succeeded,
    Failed,
    Cancelled,
}

/// 任务状态（不含日志，日志统一放在 [`Console`] 里）。
#[derive(Debug)]
pub struct JobShared {
    pub id: u64,
    pub project_id: String,
    pub kind: JobKind,
    pub steps: Vec<JobStep>,
    pub step_status: Vec<StepStatus>,
    pub outcome: JobOutcome,
    pub message: String,
    /// 失败但被忽略（`optional`）的步骤数。
    pub ignored_failures: usize,
    pub started_at: u64,
    pub finished_at: Option<u64>,
}

impl JobShared {
    fn new(id: u64, project_id: String, kind: JobKind, steps: Vec<JobStep>) -> Self {
        let step_status = vec![StepStatus::Pending; steps.len()];
        Self {
            id,
            project_id,
            kind,
            steps,
            step_status,
            outcome: JobOutcome::Running,
            message: String::new(),
            ignored_failures: 0,
            started_at: now_secs(),
            finished_at: None,
        }
    }

    /// 已完成（成功或失败）的步骤数，用于进度条。
    pub fn progress(&self) -> (usize, usize) {
        let done = self
            .step_status
            .iter()
            .filter(|s| matches!(s, StepStatus::Ok | StepStatus::Failed))
            .count();
        (done, self.steps.len())
    }

    pub fn is_running(&self) -> bool {
        self.outcome == JobOutcome::Running
    }

    /// 需要 root 的步骤数。
    pub fn root_steps(&self) -> usize {
        self.steps.iter().filter(|s| s.needs_root).count()
    }
}

/// 启动任务所需的全部输入。
pub struct JobRequest {
    pub project_id: String,
    pub kind: JobKind,
    pub steps: Vec<JobStep>,
    pub work_dir: PathBuf,
    /// 界面内存里缓存的 sudo 密码。
    ///
    /// `None` 表示还没缓存：执行到需要 root 的步骤时会通过
    /// [`Job::password_request`] 向界面索要，并在这里等待用户输入。
    pub sudo_password: Option<String>,
    /// 项目源码目录（`<项目根>/sources/<id>`），用于解析 [`WorkDir::ProjectSource`]。
    pub source_base: Option<PathBuf>,
    /// 每条命令执行前先导出的环境变量（在设置页面里配置）。
    pub env: Vec<(String, String)>,
    pub console: Console,
}

static NEXT_JOB_ID: AtomicU64 = AtomicU64::new(1);

/// 一个正在后台执行（或已结束）的任务。
pub struct Job {
    shared: Arc<Mutex<JobShared>>,
    cancel_flag: Arc<AtomicBool>,
    /// 当前子进程（进程组）的 pid，0 表示没有正在运行的子进程。
    ///
    /// 刻意只存 pid 而不是 `Child`：工作线程要在不持锁的情况下 `wait()`，
    /// 否则取消操作会被阻塞到命令自然结束。
    pid: Arc<AtomicI32>,
    /// 需要 root 权限时向界面索要密码的通道。
    gate: Arc<PasswordGate>,
    /// 这次任务里 sudo 有没有报过认证类错误（密码错 / 需要密码）。
    auth_failed: Arc<AtomicBool>,
}

impl Job {
    /// 启动任务并立刻返回；实际命令在后台线程中依次执行。
    pub fn spawn(req: JobRequest) -> Self {
        let id = NEXT_JOB_ID.fetch_add(1, Ordering::Relaxed);
        log::debug!(
            "创建任务 #{}：{} {}，{} 步",
            id,
            req.kind.label(),
            req.project_id,
            req.steps.len()
        );

        let shared = Arc::new(Mutex::new(JobShared::new(
            id,
            req.project_id,
            req.kind,
            req.steps.clone(),
        )));
        let cancel_flag = Arc::new(AtomicBool::new(false));
        let pid = Arc::new(AtomicI32::new(0));
        let gate = Arc::new(PasswordGate::new(req.sudo_password));
        let auth_failed = Arc::new(AtomicBool::new(false));

        let worker = Worker {
            shared: Arc::clone(&shared),
            cancel: Arc::clone(&cancel_flag),
            pid: Arc::clone(&pid),
            gate: Arc::clone(&gate),
            auth_failed: Arc::clone(&auth_failed),
            steps: req.steps,
            work_dir: req.work_dir,
            source_base: req.source_base,
            env: req.env,
            console: req.console,
        };
        thread::spawn(move || worker.run());

        Self {
            shared,
            cancel_flag,
            pid,
            gate,
            auth_failed,
        }
    }

    /// sudo 是不是报过认证类错误（用来提示用户重新输入密码）。
    pub fn auth_failed(&self) -> bool {
        self.auth_failed.load(Ordering::SeqCst)
    }

    /// 这个任务是不是正在等用户输入 sudo 密码。
    pub fn password_request(&self) -> Option<(usize, String)> {
        self.gate.lock().waiting_on()
    }

    /// 把用户输入的密码交给任务（本次任务内会被复用）。
    pub fn supply_password(&self, password: String) {
        *self.gate.lock() = PasswordState::Supplied(password);
        self.gate.changed.notify_all();
    }

    /// 用户拒绝输入密码，任务会以失败结束。
    pub fn decline_password(&self) {
        *self.gate.lock() = PasswordState::Declined;
        self.gate.changed.notify_all();
    }

    /// 取得共享状态；锁中毒时依然可读（恢复内部数据）。
    pub fn lock(&self) -> MutexGuard<'_, JobShared> {
        self.shared.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn is_running(&self) -> bool {
        self.lock().is_running()
    }

    /// 请求取消：立即给整个进程组发 SIGTERM，3 秒后仍未退出则 SIGKILL。
    ///
    /// 这个函数不会阻塞，可以从 UI 线程安全调用。
    pub fn cancel(&self) {
        log::warn!("收到取消请求，正在终止任务进程组");
        self.cancel_flag.store(true, Ordering::SeqCst);
        let pid = self.pid.load(Ordering::SeqCst);
        if pid <= 0 {
            return; // 目前没有正在运行的命令，无需终止任何东西。
        }

        #[cfg(unix)]
        {
            kill_group(pid, libc::SIGTERM);

            let pid_holder = Arc::clone(&self.pid);
            thread::spawn(move || {
                thread::sleep(Duration::from_secs(3));
                if pid_holder.load(Ordering::SeqCst) == pid {
                    kill_group(pid, libc::SIGKILL);
                }
            });
        }
        #[cfg(not(unix))]
        {
            // 非 unix 平台无法用信号终止进程组，只能等当前命令结束；
            // cancel_flag 保证后续命令不会再被执行。
            let _ = pid;
        }
    }
}

impl Drop for Job {
    fn drop(&mut self) {
        // 窗口关闭时不要留下还在跑的安装进程。
        if self.shared.lock().unwrap_or_else(|e| e.into_inner()).is_running() {
            self.cancel();
        }
    }
}

struct Worker {
    shared: Arc<Mutex<JobShared>>,
    cancel: Arc<AtomicBool>,
    pid: Arc<AtomicI32>,
    gate: Arc<PasswordGate>,
    auth_failed: Arc<AtomicBool>,
    steps: Vec<JobStep>,
    work_dir: PathBuf,
    source_base: Option<PathBuf>,
    env: Vec<(String, String)>,
    console: Console,
}

impl Worker {
    fn run(self) {
        let job_started = Instant::now();
        let result = self.run_steps();
        let total_secs = job_started.elapsed().as_secs_f32();
        let (outcome, message) = match result {
            Ok(()) => (JobOutcome::Succeeded, String::new()),
            Err(StepError::Cancelled) => (JobOutcome::Cancelled, "已取消".to_string()),
            Err(StepError::Message(msg)) => (JobOutcome::Failed, msg),
        };

        let (id, project_id, total, ignored) = {
            let mut guard = self.lock();
            guard.outcome = outcome;
            guard.message = message.clone();
            guard.finished_at = Some(now_secs());
            // 被打断的那一步不应停留在“执行中”。
            for status in guard.step_status.iter_mut() {
                if *status == StepStatus::Running {
                    *status = StepStatus::Failed;
                }
            }
            (
                guard.id,
                guard.project_id.clone(),
                guard.steps.len(),
                guard.ignored_failures,
            )
        };

        log::info!(
            "任务 #{} 结束：{outcome:?}（总耗时 {total_secs:.1} 秒）",
            self.lock().id
        );

        let (stream, text) = match outcome {
            JobOutcome::Succeeded if ignored > 0 => (
                LogStream::Info,
                format!(
                    "✔ 全部命令执行完毕（其中 {ignored} 条返回非 0，已忽略；总耗时 {total_secs:.1} 秒）"
                ),
            ),
            JobOutcome::Succeeded => (
                LogStream::Info,
                format!("✔ 全部命令执行成功（总耗时 {total_secs:.1} 秒）"),
            ),
            JobOutcome::Cancelled => (
                LogStream::Info,
                format!("■ 任务已取消（已运行 {total_secs:.1} 秒）"),
            ),
            _ => (
                LogStream::Info,
                format!("✖ 任务失败（已运行 {total_secs:.1} 秒）：{message}"),
            ),
        };
        self.console.push(LogLine {
            job_id: id,
            project_id,
            step: total,
            stream,
            text,
        });
    }

    fn lock(&self) -> MutexGuard<'_, JobShared> {
        self.shared.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn log(&self, step: usize, stream: LogStream, text: impl Into<String>) {
        let (job_id, project_id) = {
            let guard = self.lock();
            (guard.id, guard.project_id.clone())
        };
        self.console.push(LogLine {
            job_id,
            project_id,
            step,
            stream,
            text: text.into(),
        });
    }

    fn run_steps(&self) -> Result<(), StepError> {
        if self.steps.is_empty() {
            return Err(StepError::Message("没有可执行的命令".to_string()));
        }

        if !self.env.is_empty() {
            let shown: Vec<String> = self
                .env
                .iter()
                .map(|(key, value)| format!("{key}={value}"))
                .collect();
            log::info!("本任务会先导出环境变量：{}", shown.join(" "));
        }

        let already_root = running_as_root();
        let mut sudo_primed = false;
        let mut source_root: Option<PathBuf> = None;

        for (index, step) in self.steps.iter().enumerate() {
            self.check_cancelled()?;

            let dir = self.step_dir(index, step, &mut source_root)?;
            log::info!(
                "步骤 {}/{} 开始（{}）：{}（目录 {}）",
                index + 1,
                self.steps.len(),
                if step.needs_root { "root" } else { "普通" },
                step.command,
                dir.display()
            );

            // 只有真的需要 root 的步骤才去要密码。
            let sudo_password = if !step.needs_root || already_root {
                None
            } else {
                let password = self.root_password(index, &step.command)?;
                if !sudo_primed {
                    // 先用 sudo -v 校验一次，密码错了立刻给出明确提示
                    self.log(
                        0,
                        LogStream::Info,
                        "🔐 正在通过 sudo 校验密码（密码仅保存在内存中）…",
                    );
                    self.prime_sudo(&password)?;
                    self.log(0, LogStream::Info, "✔ sudo 密码校验通过");
                    sudo_primed = true;
                }
                Some(password)
            };

            {
                let mut guard = self.lock();
                guard.step_status[index] = StepStatus::Running;
            }

            let prefix = if step.needs_root { "$ (root) " } else { "$ " };
            self.log(index, LogStream::Info, format!("{prefix}{}", step.command));

            let auth_before = self.auth_failed.load(Ordering::SeqCst);
            let step_started = Instant::now();
            let code = self.exec_one(index, step, &dir, sudo_password.as_deref())?;
            let step_secs = step_started.elapsed().as_secs_f32();
            // 这一步是不是因为 sudo 认证失败才挂的
            let auth_from_this_step = !auth_before && self.auth_failed.load(Ordering::SeqCst);

            {
                let mut guard = self.lock();
                guard.step_status[index] = if code == 0 {
                    StepStatus::Ok
                } else {
                    StepStatus::Failed
                };
            }
            if code == 0 {
                log::info!(
                    "步骤 {}/{} 完成（耗时 {step_secs:.1} 秒）",
                    index + 1,
                    self.steps.len()
                );
                // 慢命令（比如国内克隆仓库）把耗时也显示出来，便于判断是否正常
                if step_secs >= 1.0 {
                    self.log(
                        index,
                        LogStream::Info,
                        format!("✔ 完成（耗时 {step_secs:.1} 秒）"),
                    );
                }
            }
            if code != 0 {
                let shown = if code < 0 {
                    "被信号终止".to_string()
                } else {
                    format!("退出码 {code}")
                };
                log::warn!(
                    "步骤 {}/{} 失败（{shown}）：{}",
                    index + 1,
                    self.steps.len(),
                    step.command
                );
                // 认证失败一定要停下来：卸载命令虽然容忍非 0，
                // 但密码不对导致的“没卸掉”和“没东西可卸”完全是两回事。
                if step.optional && !auth_from_this_step {
                    self.lock().ignored_failures += 1;
                    self.log(
                        index,
                        LogStream::Info,
                        format!(
                            "⚠ 第 {} 步返回非 0（{shown}），这一步可以忽略，继续执行后面的",
                            index + 1
                        ),
                    );
                    continue;
                }
                if auth_from_this_step {
                    return Err(StepError::Message(format!(
                        "第 {} 条命令因 sudo 认证失败而中止（密码不对或没有权限），\
                         这一步不能当作“没什么可卸”忽略掉",
                        index + 1
                    )));
                }
                return Err(StepError::Message(format!(
                    "第 {} 条命令失败（{shown}）：{}",
                    index + 1,
                    step.command
                )));
            }
        }
        Ok(())
    }

    /// 解析这一步该在哪个目录里执行。
    fn step_dir(
        &self,
        index: usize,
        step: &JobStep,
        source_root: &mut Option<PathBuf>,
    ) -> Result<PathBuf, StepError> {
        let dir = match &step.work_dir {
            None => self.work_dir.clone(),
            Some(WorkDir::Fixed(path)) => path.clone(),
            Some(WorkDir::ProjectSource) => {
                let base = self.source_base.clone().ok_or_else(|| {
                    StepError::Message("缺少源码目录信息，无法定位项目源码".to_string())
                })?;
                // 克隆完成后才知道仓库根目录，因此在这里才解析并缓存。
                let resolved = source_root
                    .get_or_insert_with(|| crate::paths::resolve_source_root(&base))
                    .clone();
                if resolved.is_dir() {
                    resolved
                } else {
                    // 源码被删掉了（例如卸载时）也不要卡住，退回默认目录。
                    self.log(
                        index,
                        LogStream::Info,
                        format!(
                            "⚠ 源码目录 {} 不存在，改用 {}",
                            resolved.display(),
                            self.work_dir.display()
                        ),
                    );
                    self.work_dir.clone()
                }
            }
        };

        if !dir.is_dir() {
            return Err(StepError::Message(format!(
                "工作目录不存在：{}",
                dir.display()
            )));
        }
        Ok(dir)
    }

    /// 取得 root 密码：先用本次任务里已经确认过的，否则向界面索要并等待。
    fn root_password(&self, step: usize, command: &str) -> Result<String, StepError> {
        if paths::which("sudo").is_none() {
            log::error!("这一步需要 root 权限，但系统里找不到 sudo");
            return Err(StepError::Message(
                "这一步需要 root 权限，但系统里找不到 sudo".to_string(),
            ));
        }

        let mut guard = self.gate.lock();
        loop {
            match &*guard {
                PasswordState::Supplied(password) => return Ok(password.clone()),
                PasswordState::Declined => {
                    log::warn!("用户取消了密码输入，任务中止");
                    return Err(StepError::Message(
                        "这一步需要 root 权限，但用户取消了密码输入".to_string(),
                    ));
                }
                _ => {}
            }

            if !matches!(&*guard, PasswordState::Waiting { .. }) {
                *guard = PasswordState::Waiting {
                    step,
                    command: command.to_string(),
                };
            }

            let (next, _) = self
                .gate
                .changed
                .wait_timeout(guard, Duration::from_millis(200))
                .unwrap_or_else(|e| e.into_inner());
            guard = next;

            if self.cancel.load(Ordering::SeqCst) {
                return Err(StepError::Cancelled);
            }
        }
    }

    fn check_cancelled(&self) -> Result<(), StepError> {
        if self.cancel.load(Ordering::SeqCst) {
            Err(StepError::Cancelled)
        } else {
            Ok(())
        }
    }

    /// 用密码预授权 sudo，使后续 `sudo -n` 可以直接执行。
    fn prime_sudo(&self, password: &str) -> Result<(), StepError> {
        if paths::which("sudo").is_none() {
            log::error!("需要 root 权限，但系统里找不到 sudo");
            return Err(StepError::Message(
                "系统中找不到 sudo，无法提权".to_string(),
            ));
        }

        log::info!("正在用 sudo 校验权限（密码只保存在内存中）");
        let mut command = Command::new("sudo");
        command
            .args(["-S", "-p", "", "-v"])
            .current_dir(&self.work_dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        new_session(&mut command);

        let mut child = command
            .spawn()
            .map_err(|e| StepError::Message(format!("无法启动 sudo：{e}")))?;

        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(password.as_bytes());
            let _ = stdin.write_all(b"\n");
            let _ = stdin.flush();
            // 关闭 stdin，避免 sudo 继续等待输入。
        }

        let output = child
            .wait_with_output()
            .map_err(|e| StepError::Message(format!("sudo 执行失败：{e}")))?;

        if output.status.success() {
            return Ok(());
        }

        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        if looks_like_auth_failure(&stderr) || stderr.is_empty() {
            self.auth_failed.store(true, Ordering::SeqCst);
        }
        log::error!("sudo 认证失败：{stderr}");
        if !stderr.is_empty() {
            self.log(0, LogStream::Stderr, stderr.clone());
        }
        Err(StepError::Message(if stderr.is_empty() {
            format!(
                "sudo 认证失败（退出码 {}）",
                output.status.code().unwrap_or(-1)
            )
        } else {
            format!("sudo 认证失败：{stderr}")
        }))
    }

    /// 执行单条命令并流式转发输出，返回退出码。
    fn exec_one(
        &self,
        step: usize,
        job_step: &JobStep,
        dir: &std::path::Path,
        sudo_password: Option<&str>,
    ) -> Result<i32, StepError> {
        let use_sudo = sudo_password.is_some();

        // 需要 root 时把密码通过 stdin 交给 sudo，**不依赖 sudo 的免密时间戳**：
        // GUI 里每条命令都在自己的会话（setsid）里跑，而且往往没有控制终端，
        // 上面刚 `sudo -v` 成功、下面 `sudo -n` 就报“需要密码”就是这么来的。
        //
        // 前缀 `exec 0</dev/null` 是为了把命令的 stdin 切断：
        // 万一 sudo 用了缓存没读走密码，残留的那一行也不会被命令读到。
        let mut script = String::new();
        // 先导出用户在设置里配置的环境变量
        if !self.env.is_empty() {
            script.push_str(&env_prefix(&self.env));
        }
        if use_sudo {
            script.push_str("exec 0</dev/null; ");
        }
        script.push_str(&job_step.command);

        let mut command = if use_sudo {
            let mut c = Command::new("sudo");
            c.args(["-S", "-p", "", "sh", "-c", &script]);
            c.stdin(Stdio::piped());
            c
        } else {
            let mut c = Command::new("sh");
            c.args(["-c", &script]);
            c.stdin(Stdio::null());
            c
        };

        command
            .current_dir(dir)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        new_session(&mut command);

        let mut child = command
            .spawn()
            .map_err(|e| StepError::Message(format!("无法启动命令 `{}`：{e}", job_step.command)))?;

        // 把密码喂给 sudo，然后关掉 stdin。
        if let Some(password) = sudo_password
            && let Some(mut stdin) = child.stdin.take()
        {
            let _ = stdin.write_all(password.as_bytes());
            let _ = stdin.write_all(b"\n");
            let _ = stdin.flush();
        }

        // 先登记 pid，取消操作才能作用于这个进程组。
        self.pid.store(child.id() as i32, Ordering::SeqCst);

        let stdout = child.stdout.take();
        let stderr = child.stderr.take();

        let (job_id, project_id) = {
            let guard = self.lock();
            (guard.id, guard.project_id.clone())
        };

        let out_handle = stdout.map(|out| {
            pump(
                out,
                self.console.clone(),
                job_id,
                project_id.clone(),
                step,
                LogStream::Stdout,
                None,
            )
        });
        let err_handle = stderr.map(|err| {
            pump(
                err,
                self.console.clone(),
                job_id,
                project_id.clone(),
                step,
                LogStream::Stderr,
                // 只有 root 步骤的 stderr 才需要识别 sudo 认证错误
                use_sudo.then(|| Arc::clone(&self.auth_failed)),
            )
        });

        // 不持任何锁地等待子进程退出；读取管道的线程仍在并行排空输出。
        //
        // 用 try_wait 轮询而不是直接 wait：这样每 10 秒能报一次「还在跑」，
        // 国内克隆仓库要几分钟，没有心跳根本分不清是慢还是卡死。
        let started = Instant::now();
        let mut last_heartbeat = Instant::now();
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break Ok(status),
                Ok(None) => {
                    if last_heartbeat.elapsed() >= HEARTBEAT_INTERVAL {
                        last_heartbeat = Instant::now();
                        let secs = started.elapsed().as_secs();
                        log::info!("步骤 {} 仍在执行（已耗时 {secs} 秒）", step + 1);
                        self.log(
                            step,
                            LogStream::Info,
                            format!("⏳ 仍在执行… 已耗时 {secs} 秒"),
                        );
                    }
                    thread::sleep(Duration::from_millis(200));
                }
                Err(err) => break Err(err),
            }
        };
        self.pid.store(0, Ordering::SeqCst);

        if let Some(handle) = out_handle {
            let _ = handle.join();
        }
        if let Some(handle) = err_handle {
            let _ = handle.join();
        }

        if self.cancel.load(Ordering::SeqCst) {
            return Err(StepError::Cancelled);
        }

        let status = status.map_err(|e| StepError::Message(format!("等待命令结束失败：{e}")))?;
        Ok(exit_code(&status))
    }
}

enum StepError {
    Cancelled,
    Message(String),
}

/// 把环境变量拼成 shell 前缀：`export KEY='VALUE'; ...`。
///
/// 用 shell 前缀而不是 `Command::env`，是因为 root 命令要经过 `sudo`，
/// 而 `sudo` 默认会重置环境变量，直接传环境就丢了。
fn env_prefix(env: &[(String, String)]) -> String {
    let mut out = String::new();
    for (key, value) in env {
        out.push_str("export ");
        out.push_str(key);
        out.push('=');
        out.push_str(&crate::model::shell_quote(value));
        out.push_str("; ");
    }
    out
}

/// sudo 认证类错误：说明存下来的密码不可用，应当让用户重新输入。
pub fn looks_like_auth_failure(text: &str) -> bool {
    let lowered = text.to_ascii_lowercase();
    [
        "incorrect password",
        "sorry, try again",
        "a password is required",
        "no password was provided",
        "a terminal is required",
        "密码不正确",
        "对不起，请重试",
        "需要密码",
    ]
    .iter()
    .any(|pattern| lowered.contains(pattern))
}

/// 把一段字节推到控制台（顺带写日志、识别 sudo 认证错误）。
#[allow(clippy::too_many_arguments)]
fn push_output(
    console: &Console,
    job_id: u64,
    project_id: &str,
    step: usize,
    stream: LogStream,
    auth_failed: &Option<Arc<AtomicBool>>,
    bytes: &[u8],
) {
    let text = String::from_utf8_lossy(bytes).into_owned();
    if text.is_empty() {
        return;
    }
    match stream {
        LogStream::Stderr => log::debug!("[stderr] {text}"),
        _ => log::debug!("[stdout] {text}"),
    }
    if let Some(flag) = auth_failed
        && looks_like_auth_failure(&text)
    {
        flag.store(true, Ordering::SeqCst);
    }
    console.push(LogLine {
        job_id,
        project_id: project_id.to_string(),
        step,
        stream,
        text,
    });
}

/// 把一个管道的内容按行推入控制台。
///
/// `\r` 也当作换行处理：`git clone --progress`、各种下载进度条是用 `\r`
/// 原地刷新的（后面并不跟换行），只按 `\n` 切分的话，这些进度会一直攒在
/// 缓冲区里，直到命令结束才一次性冒出来 —— 慢速克隆时看起来就像“卡死了”。
#[allow(clippy::too_many_arguments)]
fn pump<R: Read + Send + 'static>(
    reader: R,
    console: Console,
    job_id: u64,
    project_id: String,
    step: usize,
    stream: LogStream,
    auth_failed: Option<Arc<AtomicBool>>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let mut reader = BufReader::new(reader);
        let mut pending: Vec<u8> = Vec::new();
        let mut chunk = [0u8; 8192];

        loop {
            match reader.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => {
                    for &byte in &chunk[..n] {
                        if byte == b'\n' || byte == b'\r' {
                            if !pending.is_empty() {
                                push_output(
                                    &console,
                                    job_id,
                                    &project_id,
                                    step,
                                    stream,
                                    &auth_failed,
                                    &pending,
                                );
                                pending.clear();
                            }
                        } else {
                            pending.push(byte);
                        }
                    }
                }
                Err(_) => break,
            }
        }

        if !pending.is_empty() {
            push_output(
                &console,
                job_id,
                &project_id,
                step,
                stream,
                &auth_failed,
                &pending,
            );
        }
    })
}

fn exit_code(status: &std::process::ExitStatus) -> i32 {
    if let Some(code) = status.code() {
        return code;
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            return -signal;
        }
    }
    -1
}

/// 当前进程是不是已经以 root 身份运行。
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

/// 让子进程成为独立会话/进程组的组长，便于取消时整组终止。
#[cfg(unix)]
fn new_session(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    // SAFETY: `setsid` 是 async-signal-safe 的，且在 fork 之后、exec 之前调用。
    unsafe {
        command.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
}

#[cfg(not(unix))]
fn new_session(_command: &mut Command) {}

/// 终止一个进程组。
#[cfg(unix)]
fn kill_group(pid: i32, signal: i32) {
    // SAFETY: 向已知 pid 的进程组发送信号，无内存安全影响。
    unsafe {
        libc::killpg(pid, signal);
    }
}

#[cfg(not(unix))]
fn kill_group(_pid: i32, _signal: i32) {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn wait_for(job: &Job, what: &str, timeout: Duration) -> bool {
        let start = Instant::now();
        while start.elapsed() < timeout {
            if job.lock().outcome != JobOutcome::Running {
                return true;
            }
            thread::sleep(Duration::from_millis(10));
        }
        eprintln!("等待超时：{what}");
        false
    }

    fn req(commands: &[&str]) -> (JobRequest, Console) {
        let console = Console::new(false);
        (
            JobRequest {
                project_id: "test".to_string(),
                kind: JobKind::Install,
                steps: commands.iter().map(|s| JobStep::user(*s)).collect(),
                work_dir: std::env::temp_dir(),
                sudo_password: None,
                source_base: None,
                env: Vec::new(),
                console: console.clone(),
            },
            console,
        )
    }

    fn console_text(console: &Console) -> Vec<String> {
        console.lock().lines().map(|l| l.text.clone()).collect()
    }

    #[test]
    fn runs_commands_in_order_and_captures_output() {
        let (request, console) = req(&["echo first", "echo second"]);
        let job = Job::spawn(request);
        assert!(wait_for(&job, "两条命令执行完成", Duration::from_secs(20)));

        assert_eq!(job.lock().outcome, JobOutcome::Succeeded);
        assert_eq!(job.lock().step_status, vec![StepStatus::Ok, StepStatus::Ok]);

        let text = console_text(&console);
        assert!(text.iter().any(|l| l == "first"), "{text:?}");
        assert!(text.iter().any(|l| l == "second"), "{text:?}");
        // 累积控制台里应带上任务与项目信息
        assert!(console.lock().lines().all(|l| l.project_id == "test"));
        assert!(console.lock().lines().all(|l| l.job_id > 0));
    }

    #[test]
    fn stops_at_first_failing_command() {
        let (request, console) = req(&["exit 3", "echo should-not-run"]);
        let job = Job::spawn(request);
        assert!(wait_for(&job, "失败退出", Duration::from_secs(20)));

        {
            let guard = job.lock();
            assert_eq!(guard.outcome, JobOutcome::Failed);
            assert_eq!(guard.step_status[0], StepStatus::Failed);
            assert_eq!(guard.step_status[1], StepStatus::Pending);
            assert!(guard.message.contains("退出码 3"), "{}", guard.message);
        }

        let text = console_text(&console);
        assert!(!text.iter().any(|l| l == "should-not-run"), "{text:?}");
    }

    #[test]
    fn captures_stderr_separately() {
        let (request, console) = req(&["echo oops >&2"]);
        let job = Job::spawn(request);
        assert!(wait_for(&job, "stderr 输出", Duration::from_secs(20)));

        let found = console
            .lock()
            .lines()
            .find(|l| l.text == "oops")
            .map(|l| l.stream);
        assert_eq!(found, Some(LogStream::Stderr));
    }

    #[test]
    fn console_accumulates_across_jobs() {
        let console = Console::new(false);
        for word in ["one", "two"] {
            let job = Job::spawn(JobRequest {
                project_id: word.to_string(),
                kind: JobKind::Install,
                steps: vec![JobStep::user(format!("echo {word}"))],
                work_dir: std::env::temp_dir(),
                sudo_password: None,
                source_base: None,
                env: Vec::new(),
                console: console.clone(),
            });
            assert!(wait_for(&job, word, Duration::from_secs(20)));
        }

        let guard = console.lock();
        // 第二个任务不会清掉第一个任务的输出
        assert!(guard.lines().any(|l| l.text == "one"));
        assert!(guard.lines().any(|l| l.text == "two"));
        drop(guard);

        // 清空后重新开始累计
        console.clear();
        assert!(console.lock().is_empty());
    }

    #[test]
    fn root_steps_are_counted_and_pause_for_a_password() {
        if paths::which("sudo").is_none() || running_as_root() {
            eprintln!("跳过：没有 sudo 或当前就是 root");
            return;
        }

        let console = Console::new(false);
        let job = Job::spawn(JobRequest {
            project_id: "root-count".to_string(),
            kind: JobKind::Install,
            steps: vec![
                JobStep::user("echo plain"),
                JobStep::root("echo elevated"),
            ],
            work_dir: std::env::temp_dir(),
            sudo_password: None,
            source_base: None,
            env: Vec::new(),
            console: console.clone(),
        });

        assert_eq!(job.lock().root_steps(), 1);

        // 普通步骤先跑完，然后停在需要 root 的那一步等密码
        let start = Instant::now();
        let request = loop {
            if let Some(request) = job.password_request() {
                break request;
            }
            assert!(start.elapsed() < Duration::from_secs(10), "应请求密码");
            thread::sleep(Duration::from_millis(20));
        };
        assert_eq!(request.0, 1, "应停在下标 1 的步骤");
        assert_eq!(request.1, "echo elevated");
        assert_eq!(job.lock().step_status[0], StepStatus::Ok);

        job.decline_password();
        assert!(wait_for(&job, "拒绝后结束", Duration::from_secs(10)));
        assert_eq!(job.lock().outcome, JobOutcome::Failed);
    }

    #[test]
    fn can_be_cancelled_mid_run() {
        let (request, _console) = req(&["sleep 30", "echo never"]);
        let job = Job::spawn(request);
        thread::sleep(Duration::from_millis(400));

        // cancel() 必须立刻返回：它由 UI 线程调用，绝不能阻塞到命令结束。
        let before = Instant::now();
        job.cancel();
        assert!(
            before.elapsed() < Duration::from_secs(1),
            "cancel() 阻塞了 {:?}",
            before.elapsed()
        );

        assert!(
            wait_for(&job, "取消后结束", Duration::from_secs(8)),
            "取消后任务没有及时结束"
        );
        assert_eq!(job.lock().outcome, JobOutcome::Cancelled);
    }

    #[test]
    fn root_step_asks_for_a_password_and_can_be_declined() {
        if paths::which("sudo").is_none() || running_as_root() {
            eprintln!("跳过：没有 sudo 或当前就是 root");
            return;
        }

        let console = Console::new(false);
        let job = Job::spawn(JobRequest {
            project_id: "ask-password".to_string(),
            kind: JobKind::Install,
            steps: vec![JobStep::root("echo needs-root")],
            work_dir: std::env::temp_dir(),
            // 没有缓存密码 → 执行到这一步时应该来问
            sudo_password: None,
            source_base: None,
            env: Vec::new(),
            console: console.clone(),
        });

        let start = Instant::now();
        let request = loop {
            if let Some(request) = job.password_request() {
                break request;
            }
            assert!(start.elapsed() < Duration::from_secs(10), "应请求 sudo 密码");
            thread::sleep(Duration::from_millis(20));
        };
        assert_eq!(request.0, 0);
        assert_eq!(request.1, "echo needs-root");

        // 用户取消输入 → 任务失败且不再等待
        job.decline_password();
        assert!(wait_for(&job, "拒绝密码后结束", Duration::from_secs(10)));
        assert_eq!(job.lock().outcome, JobOutcome::Failed);
        assert!(job.password_request().is_none());
        let message = job.lock().message.clone();
        assert!(message.contains("root"), "{message}");
    }

    #[test]
    fn non_root_steps_never_ask_for_a_password() {
        let console = Console::new(false);
        let job = Job::spawn(JobRequest {
            project_id: "no-password-needed".to_string(),
            kind: JobKind::Install,
            steps: vec![JobStep::user("echo plain")],
            work_dir: std::env::temp_dir(),
            sudo_password: None,
            source_base: None,
            env: Vec::new(),
            console: console.clone(),
        });
        assert!(wait_for(&job, "普通步骤", Duration::from_secs(20)));
        assert_eq!(job.lock().outcome, JobOutcome::Succeeded);
        assert!(job.password_request().is_none(), "普通步骤不该索要密码");
    }

    #[test]
    fn project_source_dir_is_resolved_after_the_clone_step() {
        let base = std::env::temp_dir().join(format!(
            "w2l-src-{}-{}",
            std::process::id(),
            now_secs()
        ));
        let _ = std::fs::remove_dir_all(&base);

        // 模拟真实流程：建目录 → 克隆出一个子目录 → 在源码根目录里执行命令
        let steps = vec![
            JobStep::user(format!("mkdir -p {}", base.display())),
            JobStep::user("mkdir -p repo && echo cloned > repo/Makefile").in_dir(base.clone()),
            JobStep::user("pwd > where.txt && cat Makefile").in_project_source(),
        ];

        let console = Console::new(false);
        let job = Job::spawn(JobRequest {
            project_id: "source-demo".to_string(),
            kind: JobKind::Install,
            steps,
            work_dir: std::env::temp_dir(),
            sudo_password: None,
            source_base: Some(base.clone()),
            env: Vec::new(),
            console: console.clone(),
        });
        assert!(wait_for(&job, "源码目录解析", Duration::from_secs(20)));
        assert_eq!(
            job.lock().outcome,
            JobOutcome::Succeeded,
            "{:?}",
            console_text(&console)
        );

        // 命令应该跑在克隆出来的 repo/ 里，而不是 base/ 里
        let repo = base.join("repo");
        assert!(!base.join("where.txt").exists());
        let where_file = repo.join("where.txt");
        assert!(where_file.is_file(), "应在 {}", repo.display());
        let printed = std::fs::read_to_string(&where_file).unwrap();
        assert_eq!(printed.trim(), repo.to_string_lossy());
        assert!(console_text(&console).iter().any(|l| l == "cloned"));

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn env_prefix_quotes_values() {
        let prefix = env_prefix(&[
            ("PLAIN".to_string(), "abc".to_string()),
            ("SPACED".to_string(), "x y".to_string()),
            ("QUOTED".to_string(), "it's".to_string()),
            ("EMPTY".to_string(), String::new()),
        ]);
        assert_eq!(
            prefix,
            "export PLAIN=abc; export SPACED='x y'; export QUOTED='it'\\''s'; export EMPTY=''; "
        );

        assert_eq!(env_prefix(&[]), "");
    }

    #[test]
    fn recognises_sudo_auth_failures() {
        // sudo -n 在需要密码时的提示
        assert!(looks_like_auth_failure("sudo: 需要密码"));
        assert!(looks_like_auth_failure(
            "sudo: a password is required"
        ));
        assert!(looks_like_auth_failure("Sorry, try again."));
        assert!(looks_like_auth_failure("密码不正确，请重试"));
        // 普通构建错误不该被误判
        assert!(!looks_like_auth_failure(
            "make: *** 没有指明目标并且找不到 makefile。 停止。"
        ));
        assert!(!looks_like_auth_failure("gcc: error: main.c: No such file"));
    }

    #[test]
    fn empty_step_list_fails_fast() {
        let (request, _console) = req(&[]);
        let job = Job::spawn(request);
        assert!(wait_for(&job, "空命令列表", Duration::from_secs(20)));
        assert_eq!(job.lock().outcome, JobOutcome::Failed);
    }

    #[test]
    fn optional_steps_do_not_fail_the_whole_job() {
        let console = Console::new(false);
        let job = Job::spawn(JobRequest {
            project_id: "optional".to_string(),
            kind: JobKind::Uninstall,
            steps: vec![
                JobStep::user("echo uninstalled"),
                // 例如依赖已经被别人删掉了，包管理器会报错
                JobStep::user("exit 5").optional(),
                JobStep::user("echo continued"),
            ],
            work_dir: std::env::temp_dir(),
            sudo_password: None,
            source_base: None,
            env: Vec::new(),
            console: console.clone(),
        });

        assert!(wait_for(&job, "optional 步骤", Duration::from_secs(20)));
        assert_eq!(
            job.lock().outcome,
            JobOutcome::Succeeded,
            "可忽略的步骤失败不应让整个任务失败"
        );
        assert_eq!(job.lock().ignored_failures, 1);

        let text = console_text(&console);
        assert!(
            text.iter().any(|l| l == "continued"),
            "可选步骤失败后应继续执行：{text:?}"
        );
        assert!(
            text.iter().any(|l| l.contains("可以忽略")),
            "应提示这一步被忽略：{text:?}"
        );
    }
}
