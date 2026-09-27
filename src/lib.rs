//! Windows 移植 Linux 管理器 —— 库入口。
//!
//! 界面与业务逻辑拆成 lib + bin 两个目标，这样 `tests/` 下的端到端测试
//! 可以直接调用安装 / 卸载流程，而不必启动图形界面。

pub mod app;
pub mod deps;
pub mod exec;
pub mod fonts;
pub mod logging;
pub mod model;
pub mod open;
pub mod paths;
pub mod source;
pub mod state;
pub mod steps;
