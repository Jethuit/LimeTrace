# LimeTrace (Windows)

这是一个类似 ManicTime 的时间追踪工具，目标是低资源占用、可 24 小时后台运行。

你现在只需要关心一件事：拿到 `LimeTraceSetup.exe` 并双击安装。

## 给不会写代码的人：最简单使用方式

1. 打开这个项目的 GitHub 仓库页面。
2. 点 `Actions`。
3. 选择 `Build Windows Installer`。
4. 点 `Run workflow`。
5. 等待构建完成后，在该任务的 `Artifacts` 下载 `limetrace-setup`。
6. 解压后得到 `LimeTraceSetup.exe`，双击安装。

安装完成后会自动：
- 创建开始菜单入口（打开时间轴）
- 立即启动后台追踪
- 写入开机启动（下次开机自动后台运行）

## 一劳永逸下载方式（推荐给普通用户）

项目维护者打版本标签（例如 `v1.0.0`）后，工作流会自动把安装器发布到 GitHub `Releases`。

之后普通用户只要：
1. 打开仓库 `Releases` 页面。
2. 下载 `LimeTraceSetup.exe`。
3. 双击安装。

## 你会得到什么

- 单文件安装器：`LimeTraceSetup.exe`
- 安装后主程序路径：`C:\Program Files\LimeTrace\`
- 时间轴程序：`limetrace.exe`
- 后台追踪程序：`limetrace-backend.exe`

## 开机启动说明

安装器会写入注册表：
- `HKCU\Software\Microsoft\Windows\CurrentVersion\Run\LimeTraceBackend`

含义：当前 Windows 用户登录后，自动启动后台追踪。

## 数据存储位置

默认数据库：
- `%LOCALAPPDATA%\LimeTrace\tracker.db`

## 项目结构（开发者）

- 后台追踪：`crates/limetrace-backend`
- 时间轴界面：`crates/limetrace`
- 安装器脚本：`installer/LimeTrace.iss`
- CI 自动打包：`.github/workflows/build-windows.yml`

## 本地一键测试（开发者推荐）

本机安装 Rust 后，直接运行：

```powershell
powershell -ExecutionPolicy Bypass -File .\scripts\dev-open.ps1
```

默认行为：
- 编译 `debug` 版本（更快）
- 启动 `limetrace-backend.exe`（最小化）
- 打开 `limetrace.exe`
- 默认使用 `%LOCALAPPDATA%\LimeTrace\tracker.db`

常用参数：

```powershell
# 编译并运行 release
powershell -ExecutionPolicy Bypass -File .\scripts\dev-open.ps1 -Profile release

# 只打开 UI（不启动后台）
powershell -ExecutionPolicy Bypass -File .\scripts\dev-open.ps1 -OnlyUi

# 复用已编译产物，不重新 build
powershell -ExecutionPolicy Bypass -File .\scripts\dev-open.ps1 -NoBuild
```

停止后台进程：

```powershell
taskkill /IM limetrace-backend.exe /F
```

## 本地“像安装包一样”双击测试（不走 GitHub）

```powershell
cargo build --release
.\scripts\package-windows.ps1
```

然后打开 `dist/windows/limetrace-windows/`，双击：
- `Start LimeTrace Backend.cmd`
- `Open LimeTrace.cmd`


