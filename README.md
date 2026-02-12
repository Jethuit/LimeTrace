# LimeTrace (Windows 时间追踪)

English: [README.en.md](README.en.md)

LimeTrace 是使用rust语言编写的一个轻量化 Windows 桌面时间追踪工具。
它会在后台记录你使用过的应用和时长，并在时间轴里展示出来。

![LimeTrace](ui.png)

目前只支持 Windows。

## 如何安装

1. 打开这个仓库的 `Releases` 页面。
2. 下载 `LimeTraceSetup.exe`。
3. 双击安装。
4. 安装完成后会自动启动后台追踪并打开界面。

## 说明

- 界面程序是 `limetrace.exe`
- 后台程序是 `limetrace-backend.exe`，写入开机启动，当服务未运行时打开此程序。
- 开机启动注册表项：`HKCU\Software\Microsoft\Windows\CurrentVersion\Run\LimeTraceBackend`

## 数据存储位置

- 默认数据库：`%LOCALAPPDATA%\LimeTrace\tracker.db`

## 如何确认运行正常

1. 安装后先正常使用电脑几分钟。
2. 打开开始菜单里的 `LimeTrace -> Open LimeTrace`。
3. 如果能看到时间轴和应用时长，说明工作正常。

## 联系方式

- QQ: `1084490278`
- Email: `jethuit@outlook.com`
- 如果有问题请提交 issue。

