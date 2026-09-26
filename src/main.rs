//! Windows 移植 Linux 管理器
//!
//! 一个基于 egui 的桌面程序：每次读取项目根目录下的 `project_list.json`，
//! 把其中的移植项目列成列表展示给用户，并支持安装与卸载。

// 发布版本在 Windows 上不弹出额外的控制台窗口。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::path::{Path, PathBuf};

use w_to_l_manager::{app, logging, paths};

fn main() -> eframe::Result<()> {
    // 日志要尽早初始化：GUI 模式下 stderr 常常看不到，日志文件才是排查问题的关键。
    let list_path = paths::locate_project_list();
    let project_root = list_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let log_path = logging::init_first_writable(&paths::log_file_candidates(&project_root));

    log::info!("{0} 启动（v{1}）", app::APP_TITLE, env!("CARGO_PKG_VERSION"));
    log::info!("配置文件：{}", list_path.display());
    log::info!("项目根目录：{}", project_root.display());
    match &log_path {
        Some(path) => log::info!(
            "日志文件：{}（级别 {}，用 W2L_LOG=debug 可记录命令原始输出）",
            path.display(),
            logging::level_name(logging::current_level())
        ),
        None => log::warn!("未能确定日志文件路径，日志只会输出到 stderr"),
    }

    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_title(app::APP_TITLE)
            .with_inner_size([1180.0, 780.0])
            .with_min_inner_size([860.0, 560.0]),
        ..Default::default()
    };

    let result = eframe::run_native(
        app::APP_TITLE,
        options,
        Box::new(|cc| Ok(Box::new(app::ManagerApp::new(cc)))),
    );

    match &result {
        Ok(()) => log::info!("窗口关闭，程序正常退出"),
        Err(err) => {
            log::error!("{0} 无法启动：{err}", app::APP_TITLE);
            eprintln!("{0} 无法启动：{err}", app::APP_TITLE);
        }
    }
    result
}
