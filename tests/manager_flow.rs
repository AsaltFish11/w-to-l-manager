//! 端到端测试：按 `project_list.json` 的内容真实执行「装依赖 → 克隆源码 →
//! 安装 → 卸载」，并检查文件系统、状态文件、依赖记录与源码目录隔离。
//!
//! 这些测试走的是界面按钮背后完全相同的代码路径
//! （`model::parse` → `deps::plan` → `steps::install_steps` → `exec::Job`
//! → `state::ManagerState`），只是不启动图形界面。

#![cfg(unix)]

use std::fs;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use w_to_l_manager::deps::{self, PackageManagerKind};
use w_to_l_manager::exec::{Console, Job, JobKind, JobOutcome, JobRequest, JobStep, StepStatus};
use w_to_l_manager::model::{
    self, CommandSpec, Permission, ProjectEntry, UninstallSupport,
};
use w_to_l_manager::paths;
use w_to_l_manager::source::{self, CloneState};
use w_to_l_manager::state::{self, AutoDep, DepRecord, ManagerState};
use w_to_l_manager::steps::{self, PlannedJob};

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "w2l-e2e-{tag}-{}-{}",
        std::process::id(),
        state::now_secs()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("应能创建临时目录");
    dir
}

fn wait_for_outcome(job: &Job, timeout: Duration) -> JobOutcome {
    let start = Instant::now();
    loop {
        let outcome = job.lock().outcome;
        if outcome != JobOutcome::Running {
            return outcome;
        }
        assert!(
            start.elapsed() < timeout,
            "任务在 {timeout:?} 内没有结束（可能是死锁或未取消成功）"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

fn console_text(console: &Console) -> String {
    console
        .lock()
        .lines()
        .map(|l| l.text.clone())
        .collect::<Vec<_>>()
        .join("\n")
}

/// 执行一个组装好的计划并等待成功。
fn run_plan(project_id: &str, kind: JobKind, plan: PlannedJob, work_dir: &Path) -> (Job, Console) {
    let console = Console::new(false);
    let job = Job::spawn(JobRequest {
        project_id: project_id.to_string(),
        kind,
        steps: plan.steps,
        work_dir: work_dir.to_path_buf(),
        sudo_password: None,
        source_base: plan.source_base,
        env: Vec::new(),
        console: console.clone(),
    });
    let outcome = wait_for_outcome(&job, Duration::from_secs(30));
    assert_eq!(
        outcome,
        JobOutcome::Succeeded,
        "任务应成功，实际 {outcome:?}，日志：\n{}",
        console_text(&console)
    );
    (job, console)
}

/// 用一条不依赖网络的命令模拟 `git clone`：在源码基目录里建出仓库子目录。
fn fake_clone_command() -> &'static str {
    "mkdir -p demo-cmd && printf 'all:\\n\\t@echo built ok\\n' > demo-cmd/Makefile && echo cloned"
}

fn demo_list_json() -> String {
    format!(
        r#"{{
            "lists": [
                {{
                    "id": "demo-cmd",
                    "describe": "演示项目",
                    "github-url": "https://github.com/example/demo",
                    "clone-command": "{clone}",
                    "dependency": ["sh"],
                    "install-commands": [
                        {{ "permission": "normal", "command": "pwd > pwd.txt" }},
                        {{ "permission": "normal", "command": "cat Makefile" }},
                        {{ "permission": "normal", "command": "make" }}
                    ],
                    "uninstall-commands": [
                        {{ "permission": "normal", "command": "rm -f pwd.txt" }}
                    ]
                }}
            ]
        }}"#,
        clone = fake_clone_command()
    )
}

#[test]
fn full_install_and_uninstall_cycle_with_clone() {
    let root = temp_dir("cycle");
    let list_json = demo_list_json();
    fs::write(root.join("project_list.json"), &list_json).unwrap();

    let loaded = model::parse(&list_json).expect("列表应能解析");
    let entry = &loaded.entries[0];
    assert_eq!(entry.id, "demo-cmd");
    assert_eq!(
        entry.github_url.as_deref(),
        Some("https://github.com/example/demo")
    );
    assert!(entry.needs_clone());

    // 依赖：sh 已存在 → 只记录，不安装
    let dep_plan = deps::plan(&entry.dependency, None);
    assert_eq!(dep_plan.present, vec!["sh"]);
    assert!(dep_plan.missing.is_empty());

    // 组装安装步骤（没有缺失依赖，所以不会调用包管理器）
    let planned = steps::install_steps(&root, entry, &[], None, CloneState::Fresh);
    let source_base = planned.source_base.clone().expect("应有源码目录");
    assert_eq!(source_base, root.join("sources").join("demo-cmd"));

    let (_job, console) = run_plan(&entry.id, JobKind::Install, planned, &root);

    // 源码被克隆到本项目专属目录里的仓库子目录
    let repo = source_base.join("demo-cmd");
    assert!(repo.join("Makefile").is_file(), "应克隆出 Makefile");
    assert!(console_text(&console).contains("cloned"));

    // 安装命令确实跑在源码根目录里（而不是项目根目录）
    let pwd_file = repo.join("pwd.txt");
    assert!(pwd_file.is_file(), "pwd.txt 应在 {}", repo.display());
    assert!(!root.join("pwd.txt").exists(), "不应写在项目根目录");
    assert_eq!(
        fs::read_to_string(&pwd_file).unwrap().trim(),
        repo.to_string_lossy()
    );
    // make 在源码目录里跑通了，说明 Makefile 找得到
    assert!(
        console_text(&console).contains("built ok"),
        "make 应执行成功：\n{}",
        console_text(&console)
    );

    // 记录安装状态（含源码位置）
    let state_path = root.join("state.json");
    let mut manager_state = ManagerState::default();
    manager_state.mark_installed(
        &entry.id,
        entry
            .install_commands
            .iter()
            .map(|c| c.command().to_string())
            .collect(),
        Some(source_base.display().to_string()),
        DepRecord {
            pre_existing: dep_plan.present.clone(),
            auto_installed: Vec::new(),
        },
    );
    manager_state
        .save(std::slice::from_ref(&state_path))
        .unwrap();

    let (reloaded, warning) = ManagerState::load(&state_path);
    assert!(warning.is_none());
    let record = &reloaded.installed["demo-cmd"];
    assert!(record.installed_at > 0);
    assert_eq!(
        record.source_dir.as_deref(),
        Some(source_base.to_string_lossy().as_ref())
    );
    assert_eq!(record.dependencies.pre_existing, vec!["sh"]);

    // 卸载：不删源码 → 源码目录还在，但 pwd.txt 被删掉
    let uninstall = match model::uninstall_plan(entry) {
        UninstallSupport::Available(plan) => plan,
        other => panic!("应有卸载计划，实际 {other:?}"),
    };
    let planned = steps::uninstall_steps(&root, entry, &uninstall.commands, &[], None, false);
    run_plan(&entry.id, JobKind::Uninstall, planned, &root);
    assert!(!pwd_file.exists(), "卸载命令应删除 pwd.txt");
    assert!(repo.is_dir(), "未勾选删除源码时应保留");

    // 再卸载一次，这次连源码一起删掉
    let planned = steps::uninstall_steps(&root, entry, &uninstall.commands, &[], None, true);
    run_plan(&entry.id, JobKind::Uninstall, planned, &root);
    assert!(!source_base.exists(), "应删除源码目录");

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn different_projects_get_isolated_source_dirs() {
    let root = temp_dir("isolation");

    let entries: Vec<ProjectEntry> = (0..2)
        .map(|i| ProjectEntry {
            id: format!("proj-{i}"),
            clone_command: Some(fake_clone_command().to_string()),
            install_commands: vec![CommandSpec::normal("pwd > where.txt")],
            ..Default::default()
        })
        .collect();

    let mut dirs = Vec::new();
    for entry in &entries {
        let planned = steps::install_steps(&root, entry, &[], None, CloneState::Fresh);
        let base = planned.source_base.clone().unwrap();
        dirs.push(base.clone());
        run_plan(&entry.id, JobKind::Install, planned, &root);
        assert!(base.join("demo-cmd").join("where.txt").is_file());
    }

    // 两个项目的目录互不相同，也互不覆盖
    assert_ne!(dirs[0], dirs[1]);
    assert!(dirs[0].join("demo-cmd").is_dir());
    assert!(dirs[1].join("demo-cmd").is_dir());

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn second_install_reuses_the_existing_clone() {
    let root = temp_dir("reuse");
    let entry = ProjectEntry {
        id: "demo".to_string(),
        clone_command: Some(fake_clone_command().to_string()),
        install_commands: vec![CommandSpec::normal("pwd > where.txt")],
        ..Default::default()
    };

    let planned = steps::install_steps(&root, &entry, &[], None, CloneState::Fresh);
    let base = planned.source_base.clone().unwrap();
    run_plan(&entry.id, JobKind::Install, planned, &root);

    // 第二次安装：目录已有内容 → 跳过克隆
    let state = CloneState::decide(&base, false);
    assert_eq!(state, CloneState::Existing);
    let planned = steps::install_steps(&root, &entry, &[], None, state);
    let (_job, console) = run_plan(&entry.id, JobKind::Install, planned, &root);
    assert!(
        console_text(&console).contains("跳过克隆"),
        "应提示跳过克隆：\n{}",
        console_text(&console)
    );

    // 要求重新克隆时：先删后建
    let planned = steps::install_steps(&root, &entry, &[], None, CloneState::ReClone);
    let (_job, console) = run_plan(&entry.id, JobKind::Install, planned, &root);
    let text = console_text(&console);
    assert!(text.contains("rm -rf"), "重新克隆应先删除：\n{text}");

    let _ = fs::remove_dir_all(&root);
}

/// 回归测试：上次克隆被中断留下的半成品目录，绝不能被当成“已克隆好”。
///
/// 用户遇到的就是这个：`git clone` 先建出目标目录、再慢慢签出文件，
/// 中途取消后目录还在但内容是空的，于是 `make` 在半个仓库里跑，
/// 报出“没有指明目标并且找不到 makefile”。
#[test]
fn interrupted_clone_is_redone_on_the_next_install() {
    let root = temp_dir("interrupted");
    let entry = ProjectEntry {
        id: "demo".to_string(),
        clone_command: Some(fake_clone_command().to_string()),
        install_commands: vec![CommandSpec::normal("pwd > where.txt")],
        ..Default::default()
    };
    let base = paths::project_source_base(&root, "demo");

    // 模拟上次克隆被取消：目录建出来了，内容还没签出
    fs::create_dir_all(base.join("demo-cmd")).unwrap();
    assert!(
        !source::has_complete_sources(&base),
        "空目录不该被当成完整源码"
    );

    // 再点一次安装：必须先清掉半成品再重新克隆
    let state = CloneState::decide(&base, false);
    assert_eq!(state, CloneState::ReClone);
    let planned = steps::install_steps(&root, &entry, &[], None, state);
    let (_job, console) = run_plan(&entry.id, JobKind::Install, planned, &root);

    let text = console_text(&console);
    assert!(text.contains("rm -rf"), "应先清掉半成品：\n{text}");
    assert!(text.contains("cloned"), "应重新克隆：\n{text}");
    assert!(base.join("demo-cmd").join("where.txt").is_file());

    // 克隆成功后应写下完整性标记，下次才会跳过克隆
    assert!(
        base.join(source::CLONE_MARKER).is_file(),
        "克隆成功后应写下 {} 标记",
        source::CLONE_MARKER
    );
    assert!(source::has_complete_sources(&base));
    assert_eq!(CloneState::decide(&base, false), CloneState::Existing);

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn failed_dependency_install_aborts_the_project_install() {
    let dir = temp_dir("depfail");
    let marker = dir.join("should-not-exist.marker");

    // 模拟「先装依赖，再装项目」，依赖安装失败。
    // 真实场景里装依赖是 root 步骤，会先弹窗要密码；这里用普通步骤代替，
    // 重点验证「依赖失败会中断后续的项目安装」。
    let steps = vec![
        JobStep::user("echo 假装安装依赖失败; exit 9"),
        JobStep::user(format!("touch {}", marker.display())),
    ];

    let console = Console::new(false);
    let job = Job::spawn(JobRequest {
        project_id: "demo-depfail".to_string(),
        kind: JobKind::Install,
        steps,
        work_dir: dir.clone(),
        sudo_password: None,
        source_base: None,
        env: Vec::new(),
        console: console.clone(),
    });

    assert_eq!(
        wait_for_outcome(&job, Duration::from_secs(30)),
        JobOutcome::Failed
    );
    assert!(!marker.exists(), "依赖没装好就不该继续安装项目本身");
    assert_eq!(job.lock().step_status[0], StepStatus::Failed);
    assert_eq!(job.lock().step_status[1], StepStatus::Pending);

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn only_manager_installed_dependencies_are_removed() {
    let root = temp_dir("depremove");
    let state_path = root.join("state.json");

    // 安装前就有 make；管理器自己装了 cmake
    let record = DepRecord {
        pre_existing: vec!["make".to_string()],
        auto_installed: vec![AutoDep {
            command: "cmake".to_string(),
            package: "cmake".to_string(),
            manager: "pacman".to_string(),
            installed_at: state::now_secs(),
        }],
    };

    let mut manager_state = ManagerState::default();
    manager_state.mark_installed("demo", vec!["make install".into()], None, record);
    manager_state
        .save(std::slice::from_ref(&state_path))
        .unwrap();

    let (reloaded, _) = ManagerState::load(&state_path);
    let saved = &reloaded.installed["demo"].dependencies;
    assert_eq!(saved.auto_packages(), vec!["cmake"]);
    assert_eq!(saved.manager_name(), Some("pacman"));

    let entry = ProjectEntry {
        id: "demo".to_string(),
        install_commands: vec![CommandSpec::normal("make install")],
        uninstall_commands: vec![CommandSpec::normal("make uninstall")],
        ..Default::default()
    };

    // 用户勾选「一并卸载依赖」：卸载命令里只有 cmake，绝不含预先存在的 make
    let manager = PackageManagerKind::from_name(saved.manager_name().unwrap());
    let planned = steps::uninstall_steps(
        &root,
        &entry,
        &entry.uninstall_commands,
        &saved.auto_packages(),
        manager,
        false,
    );
    let commands: Vec<&str> = planned.steps.iter().map(|s| s.command.as_str()).collect();
    assert_eq!(commands.len(), 2);
    assert_eq!(commands[0], "make uninstall");
    assert!(commands[1].contains("cmake"), "{commands:?}");
    assert!(
        !commands[1].contains("make "),
        "不应把预先存在的依赖也卸掉：{commands:?}"
    );
    assert!(planned.steps[1].needs_root);
    assert!(planned.steps[1].optional, "依赖清理失败不应中断卸载");

    // 不勾选时：只有项目自己的卸载命令
    let planned =
        steps::uninstall_steps(&root, &entry, &entry.uninstall_commands, &[], manager, false);
    assert_eq!(planned.steps.len(), 1);
    assert!(!planned.steps[0].needs_root);

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn author_provided_uninstall_commands_take_priority() {
    let root = temp_dir("explicit");
    let marker = root.join("explicit.marker");
    let list_json = format!(
        r#"{{
            "lists": [
                {{
                    "id": "demo-explicit",
                    "install-commands": [
                        {{ "permission": "normal", "command": "echo installed > {marker}" }}
                    ],
                    "uninstall-commands": [
                        {{ "permission": "normal", "command": "rm -f {marker}" }}
                    ]
                }}
            ]
        }}"#,
        marker = marker.display()
    );

    let loaded = model::parse(&list_json).expect("列表应能解析");
    let entry = &loaded.entries[0];

    let plan = match model::uninstall_plan(entry) {
        UninstallSupport::Available(plan) => plan,
        other => panic!("应有卸载计划，实际 {other:?}"),
    };
    assert_eq!(plan.commands, entry.uninstall_commands);

    let planned = steps::install_steps(&root, entry, &[], None, CloneState::Existing);
    run_plan(&entry.id, JobKind::Install, planned, &root);
    assert!(marker.is_file(), "安装应创建标记文件");

    let planned = steps::uninstall_steps(&root, entry, &plan.commands, &[], None, false);
    run_plan(&entry.id, JobKind::Uninstall, planned, &root);
    assert!(!marker.exists(), "卸载应删除标记文件");

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn a_failing_command_stops_the_install_and_is_reported() {
    let dir = temp_dir("failure");
    let console = Console::new(false);

    let job = Job::spawn(JobRequest {
        project_id: "demo-fail".to_string(),
        kind: JobKind::Install,
        steps: vec![
            JobStep::user("echo before-failure"),
            JobStep::user("exit 7"),
            JobStep::user("echo after-failure"),
        ],
        work_dir: dir.clone(),
        sudo_password: None,
        source_base: None,
        env: Vec::new(),
        console: console.clone(),
    });

    assert_eq!(
        wait_for_outcome(&job, Duration::from_secs(30)),
        JobOutcome::Failed
    );
    let text = console_text(&console);
    assert!(text.contains("before-failure"), "{text}");
    assert!(!text.contains("after-failure"), "失败后不应继续执行：{text}");
    assert!(
        job.lock().message.contains("退出码 7"),
        "{}",
        job.lock().message
    );

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn wrong_sudo_password_fails_without_hanging() {
    if paths::which("sudo").is_none() {
        eprintln!("跳过：系统里没有 sudo");
        return;
    }
    // 已经是 root 时 sudo 不需要密码，这个用例没有意义。
    // SAFETY: geteuid 无参数、无副作用。
    if unsafe { libc::geteuid() } == 0 {
        eprintln!("跳过：当前是 root");
        return;
    }

    let dir = temp_dir("sudo");
    let console = Console::new(false);
    let job = Job::spawn(JobRequest {
        project_id: "sudo-test".to_string(),
        kind: JobKind::Install,
        steps: vec![JobStep::root("echo should-not-run-as-root")],
        work_dir: dir.clone(),
        sudo_password: Some("definitely-not-the-right-password".to_string()),
        source_base: None,
        env: Vec::new(),
        console: console.clone(),
    });

    let outcome = wait_for_outcome(&job, Duration::from_secs(30));
    let text = console_text(&console);
    match outcome {
        // 绝大多数机器上是密码错误；配置了 NOPASSWD 时会成功，两种都可以接受，
        // 关键是不能卡住、也不能静默继续。
        JobOutcome::Failed => {
            assert!(
                job.lock().message.contains("sudo") || text.contains("sudo"),
                "失败原因应提到 sudo：{text}"
            );
            assert!(!text.contains("should-not-run-as-root"));
        }
        JobOutcome::Succeeded => eprintln!("注意：这台机器上的 sudo 不需要密码"),
        other => panic!("不应出现 {other:?}"),
    }

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn the_projects_own_project_list_is_valid_and_actionable() {
    let path = paths::locate_project_list();
    assert!(path.is_file(), "应能找到 {}", path.display());
    let root = path.parent().expect("配置文件应有父目录");

    let text = fs::read_to_string(&path).unwrap();
    let loaded = model::parse(&text).expect("项目根目录的 project_list.json 应能解析");
    assert!(!loaded.entries.is_empty(), "列表里应至少有一个条目");

    // 这个文件由维护者随时更新，所以只验证“每个条目都能被正确处理”，
    // 不针对某个具体条目做断言。
    for entry in &loaded.entries {
        assert!(!entry.id.trim().is_empty(), "每个条目都应有 id");
        assert!(
            !entry.install_commands.is_empty(),
            "{} 没有 install-commands，装不了",
            entry.id
        );

        let planned = steps::install_steps(root, entry, &[], None, CloneState::Fresh);
        if entry.needs_clone() {
            // 源码目录必须是本项目专属的 sources/<id>
            let expected = paths::project_source_base(root, &entry.id);
            assert_eq!(
                planned.source_dir(),
                Some(expected.as_path()),
                "{} 的源码目录不对",
                entry.id
            );
            // 克隆成功之后必须紧接着写下完整性标记：
            // 下一次安装靠它判断能不能跳过克隆。
            let marker_at = planned
                .steps
                .iter()
                .position(|s| s.command.contains(source::CLONE_MARKER))
                .unwrap_or_else(|| {
                    panic!("{} 应该有写完整性标记的步骤", entry.id)
                });
            let clone_at = planned
                .steps
                .iter()
                .position(|s| Some(&s.command) == entry.clone_command.as_ref())
                .expect("应该有克隆步骤");
            assert!(
                clone_at < marker_at,
                "{} 的完整性标记必须写在克隆之后",
                entry.id
            );
        }

        // 卸载：要么给出可执行命令，要么明确说明为什么不能卸载
        match model::uninstall_plan(entry) {
            UninstallSupport::Available(plan) => {
                assert!(!plan.commands.is_empty(), "{} 的卸载计划是空的", entry.id);
            }
            UninstallSupport::Unavailable { reason } => {
                assert!(!reason.is_empty(), "{} 应说明不能卸载的原因", entry.id);
            }
        }
    }
}

/// 卸载只执行作者写好的命令：不再从 install-commands 反推。
#[test]
fn uninstall_requires_author_provided_commands() {
    let entry = ProjectEntry {
        id: "no-uninstall".to_string(),
        install_commands: vec![
            CommandSpec::normal("./configure"),
            CommandSpec::new("make install PREFIX=/usr/local", Permission::Root),
        ],
        ..Default::default()
    };

    match model::uninstall_plan(&entry) {
        UninstallSupport::Unavailable { reason } => {
            assert!(reason.contains("uninstall-commands"), "{reason}");
        }
        other => panic!("没有 uninstall-commands 就不该能卸载，实际是 {other:?}"),
    }

    // 写了就能卸载，权限照搬
    let with_uninstall = ProjectEntry {
        uninstall_commands: vec![CommandSpec::new("make uninstall", Permission::Root)],
        ..entry
    };
    match model::uninstall_plan(&with_uninstall) {
        UninstallSupport::Available(plan) => {
            assert_eq!(plan.commands.len(), 1);
            assert_eq!(plan.commands[0].command(), "make uninstall");
            assert_eq!(plan.commands[0].permission(), Permission::Root);
        }
        other => panic!("应该可以卸载，实际是 {other:?}"),
    }
}

/// 更新流程的「取文件 + 清理」部分不需要联网，单独跑一遍验证。
#[test]
fn update_extracts_the_list_and_cleans_up_the_tmp_dir() {
    let root = temp_dir("update-extract");
    let clone = steps::clone_dir(&root);

    // 模拟“配置仓库已经克隆到临时目录”
    fs::create_dir_all(&clone).unwrap();
    fs::write(
        clone.join(steps::CONFIG_LIST_FILE),
        r#"{"lists":[{"id":"brand-new","install-commands":["true"]}]}"#,
    )
    .unwrap();
    // 项目根下先放一份旧的，它必须被新的覆盖掉
    fs::write(
        root.join(paths::PROJECT_LIST_FILE),
        r#"{"lists":[{"id":"OLD"}]}"#,
    )
    .unwrap();

    // 只跑后三步（拷贝 → 改名 → 清理），跳过 rm/mkdir/clone 这些联网步骤
    let steps_to_run: Vec<_> = steps::update_steps(&root).into_iter().skip(3).collect();
    assert_eq!(steps_to_run.len(), 3, "应该是 拷贝 / 改名 / 清理 三步");

    let console = Console::new(false);
    let job = Job::spawn(JobRequest {
        project_id: steps::CONFIG_DIR_NAME.to_string(),
        kind: JobKind::Update,
        steps: steps_to_run,
        work_dir: root.clone(),
        sudo_password: None,
        source_base: None,
        env: Vec::new(),
        console: console.clone(),
    });
    assert_eq!(
        wait_for_outcome(&job, Duration::from_secs(20)),
        JobOutcome::Succeeded,
        "{}\n{}",
        job.lock().message,
        console_text(&console)
    );

    // 项目根目录下的列表被换成了最新的
    let text = fs::read_to_string(root.join(paths::PROJECT_LIST_FILE)).unwrap();
    let loaded = model::parse(&text).unwrap();
    assert_eq!(loaded.entries[0].id, "brand-new");

    // 临时目录和中间文件都不该留下
    assert!(!clone.exists(), "克隆目录应被删除");
    assert!(!steps::tmp_dir(&root).exists(), "tmp 空了就应该被删掉");
    assert!(!root.join(".project_list.json.new").exists(), "中间文件应被改名走");

    let _ = fs::remove_dir_all(&root);
}

/// 临时目录里还有别的东西时，只删我们自己的克隆，不动别人的内容。
#[test]
fn update_keeps_unrelated_files_in_the_tmp_dir() {
    let root = temp_dir("update-keep");
    let clone = steps::clone_dir(&root);
    fs::create_dir_all(&clone).unwrap();
    fs::write(
        clone.join(steps::CONFIG_LIST_FILE),
        r#"{"lists":[{"id":"new","install-commands":["true"]}]}"#,
    )
    .unwrap();
    // ./tmp 里放一份“别人的”文件
    fs::write(steps::tmp_dir(&root).join("keep-me.txt"), "hello").unwrap();

    let steps_to_run: Vec<_> = steps::update_steps(&root).into_iter().skip(3).collect();
    let console = Console::new(false);
    let job = Job::spawn(JobRequest {
        project_id: steps::CONFIG_DIR_NAME.to_string(),
        kind: JobKind::Update,
        steps: steps_to_run,
        work_dir: root.clone(),
        sudo_password: None,
        source_base: None,
        env: Vec::new(),
        console: console.clone(),
    });
    assert_eq!(
        wait_for_outcome(&job, Duration::from_secs(20)),
        JobOutcome::Succeeded,
        "{}",
        console_text(&console)
    );

    assert!(!clone.exists(), "我们自己的克隆目录应被删除");
    assert!(
        steps::tmp_dir(&root).join("keep-me.txt").is_file(),
        "tmp 里别人的文件不该被动"
    );

    let _ = fs::remove_dir_all(&root);
}

/// 点 button-N：命令在项目源码目录里执行。
#[test]
fn button_commands_run_in_the_project_source_directory() {
    let root = temp_dir("button-run");
    let base = paths::project_source_base(&root, "demo");
    fs::create_dir_all(&base).unwrap();

    let entry = ProjectEntry {
        id: "demo".to_string(),
        clone_command: Some("true".to_string()),
        ..Default::default()
    };
    let button = model::ActionButton {
        name: "运行".to_string(),
        describe: "跑一下".to_string(),
        commands: vec![CommandSpec::normal("echo button-ran > marker.txt")],
        background: false,
    };

    let plan = steps::action_steps(&root, &entry, &button);
    assert_eq!(plan.steps.len(), 1);
    assert_eq!(plan.source_dir(), Some(base.as_path()));

    let _ = run_plan("demo", JobKind::Action, plan, &root);

    assert!(
        base.join("marker.txt").is_file(),
        "按钮的命令应该在 sources/demo 里执行"
    );

    let _ = fs::remove_dir_all(&root);
}

/// 不需要克隆的项目，按钮命令在工作目录里执行。
#[test]
fn buttons_of_projects_without_clone_run_in_the_work_dir() {
    let root = temp_dir("button-noclone");
    fs::create_dir_all(&root).unwrap();
    let work = temp_dir("button-work");
    fs::create_dir_all(&work).unwrap();

    let entry = ProjectEntry {
        id: "demo".to_string(),
        ..Default::default()
    };
    let button = model::ActionButton {
        name: "运行".to_string(),
        describe: String::new(),
        commands: vec![CommandSpec::normal("echo no-clone > marker.txt")],
        background: false,
    };

    let plan = steps::action_steps(&root, &entry, &button);
    assert!(plan.source_base.is_none(), "没有 clone-command 就不该有源码目录");

    let _ = run_plan("demo", JobKind::Action, plan, &work);

    assert!(work.join("marker.txt").is_file(), "应该落在工作目录里");

    let _ = fs::remove_dir_all(&root);
    let _ = fs::remove_dir_all(&work);
}

/// 设置里配的环境变量要真的出现在每条命令的环境里（值里有空格也要正确引用）。
#[test]
fn configured_environment_variables_reach_every_command() {
    let dir = temp_dir("env");
    let console = Console::new(false);

    let job = Job::spawn(JobRequest {
        project_id: "env-test".to_string(),
        kind: JobKind::Install,
        steps: vec![
            JobStep::user("echo \"GREETING=$W2L_GREETING\""),
            JobStep::user("echo \"SECOND=$W2L_SECOND\""),
        ],
        work_dir: dir.clone(),
        sudo_password: None,
        source_base: None,
        env: vec![
            ("W2L_GREETING".to_string(), "hello world".to_string()),
            ("W2L_SECOND".to_string(), "it's fine".to_string()),
        ],
        console: console.clone(),
    });

    assert_eq!(
        wait_for_outcome(&job, Duration::from_secs(20)),
        JobOutcome::Succeeded,
        "{}",
        console_text(&console)
    );
    let text = console_text(&console);
    assert!(text.contains("GREETING=hello world"), "带空格的值应被正确引用：\n{text}");
    assert!(text.contains("SECOND=it's fine"), "含单引号的值也要正确：\n{text}");

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn dependency_plan_explains_what_it_can_and_cannot_install() {
    let dependency = vec!["sh".to_string(), "definitely-not-a-real-tool".to_string()];

    // 软件源里有对应的包 → 可以自动安装
    let plan = deps::plan_with(
        &dependency,
        Some(PackageManagerKind::Pacman),
        |_, package| package == "definitely-not-a-real-tool",
    );
    assert_eq!(plan.present, vec!["sh"]);
    assert!(plan.auto_installable());
    assert_eq!(
        plan.auto_packages(),
        vec!["definitely-not-a-real-tool".to_string()]
    );

    // 软件源里也没有 → 阻止自动安装并给出可读原因
    let plan = deps::plan_with(&dependency, Some(PackageManagerKind::Apt), |_, _| false);
    assert!(!plan.auto_installable());
    let reason = plan.missing[0].reason.clone().unwrap_or_default();
    assert!(reason.contains("apt"), "{reason}");

    // 连包管理器都没有 → 提示手动安装
    let plan = deps::plan_with(&dependency, None, |_, _| true);
    assert!(!plan.auto_installable());
    let reason = plan.missing[0].reason.clone().unwrap_or_default();
    assert!(reason.contains("手动安装"), "{reason}");
}


