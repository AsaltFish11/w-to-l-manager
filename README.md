# Windows 移植 Linux 管理器

用 **Rust + egui** 写的桌面工具。它读取项目根目录下的 `project_list.json`，
把里面的「Windows 移植到 Linux」项目列成清单，显示**说明和 GitHub 链接**，
并支持一键**克隆源码 → 安装 / 卸载**。安装卸载的输出显示在界面下方的日志面板里，
同时打印到命令行。
注意: 本项目完全使用AI开发, 且不会长期稳定

## 快速开始

需要 Rust 1.85+，以及 Wayland / X11、OpenGL、fontconfig 等开发库
（Debian / Ubuntu：`sudo apt install libxkbcommon-dev libwayland-dev libx11-dev libgl1-mesa-dev libfontconfig1-dev`）。

```bash
cargo run --release
```

把 `project_list.json` 放在项目根目录即可；也可以用环境变量 `W2L_PROJECT_LIST` 指定路径。

## 怎么用

- **列表**：每个项目显示说明、GitHub 链接、依赖和「已安装 / 未安装」状态。
- **安装**：先检查依赖 —— 缺依赖时会问你是自动安装（用系统的 pacman / apt）还是手动安装；
  然后把源码克隆到 `sources/<项目 id>/`，在该仓库根目录里执行安装命令。
- **卸载**：列出将要执行的命令，并询问是否一并移除「由本管理器装过的依赖」和克隆的源码目录
  （默认都保留）；安装前就已存在的依赖永远不会被动。
- **sudo 密码**：只有执行到需要 root 的命令时才弹窗询问，密码只保存在内存中，退出即消失。
- **日志**：下方面板显示本次会话的完整输出，可滚动、可触摸滑动，同时也会打印到命令行。

## 文件位置

| 内容 | 位置 |
| --- | --- |
| 源码 | `sources/<项目 id>/`（项目根目录下，每个项目独立） |
| 安装状态 | `~/.local/share/w-to-l-manager/state.json`（可用 `W2L_STATE_FILE` 改） |
| 日志 | `~/.local/state/w-to-l-manager/w-to-l-manager.log`（`W2L_LOG=debug` 记录更详细） |

## 开发

```bash
cargo test
cargo clippy --all-targets
```
