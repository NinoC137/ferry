# Ferry

> 在主机与嵌入式 Linux / Android 目标机之间往返的一体化工作台。

**Ferry** 是一个零第三方 Rust 依赖的命令行工具（`fy`），并提供可选的原生桌面工作台。它把 SSH、ADB 与串口控制台统一为一种设备档案和一套操作语言：发现开发板、在地址变化后重新认领、打开终端、带校验地传输产物、恢复网络、采集硬件事实，或持续记录串口崩溃现场。

[English](README.md) · [文档索引](docs/README.md) · [架构说明](docs/architecture.zh-CN.md) · [操作指南](docs/operations.zh-CN.md) · [参与贡献](CONTRIBUTING.md) · [安全策略](SECURITY.md)

## 项目状态

Ferry 仍处于早期阶段，但目标是服务真实的实验室与板级 bring-up 工作。`fy` CLI 是稳定的自动化接口；浏览器工作台（`fy ui`）和 Tauri 桌面客户端则是在同一组 Ferry 模块之上的交互式客户端。

## Ferry 解决什么问题？

嵌入式开发常常被 SSH、ADB、串口工具、临时拷贝脚本、配网步骤和会随着刷机丢失的笔记切碎。Ferry 把设备档案、已观察到的身份信息和日常工作流放在同一个地方。

| 场景 | Ferry 工作流 | 带来的能力 |
| --- | --- | --- |
| DHCP 或刷机后开发板地址变化 | `fy scan`、`fy info` | mDNS/TCP 发现，以及 MAC / 指纹认领 |
| 只剩串口控制台可用 | `fy up`、`fy bb` | 恢复路径与持续的崩溃记录 |
| 大镜像传输中断 | `fy push`、`fy pull` | 断点续传、已有前缀验证与完整性校验 |
| 目标机需要使用主机网络 | `fy share`、`fy usb net` | 默认代理隧道；需要时显式启用 NAT |
| 隧道频繁断开 | `fy fwd`、`fy watch` | 受管转发与重连后的规则回放 |
| 需要留下硬件证据 | `fy hw` | 只读 procfs / sysfs / 在线设备树快照 |
| 脚本或 Agent 调用 Ferry | `fy --json` | stdout 只输出一份 JSON，且失败不交互 |

## 核心特性

- **三种通道，一份档案：** SSH、ADB 和串口控制台都能使用同一个设备名。
- **具备身份感知的发现：** 只接纳已验证的 SSH banner 和已授权的网络 ADB 端点；保存的事实可帮助设备在 IP 或 USB 口变化后重新认领。
- **可靠的传输与部署：** 支持可续传、可验证传输，经过主机的板间复制，以及 `run`、`debug`、`sync` 工作流。
- **连通性与恢复：** 端口转发、连接守护、代理/NAT 借网、USB 配网、串口升格至 SSH 和串口黑匣子。
- **硬件事实采集：** 只读采集器生成 `hardware.json`、可选的 `peripherals.md`，并在可用时保存原始设备树归档。
- **可审查的扩展：** 本地插件包会声明依赖、风险、参数和执行前预览。
- **两种交互工作台：** 本地浏览器 PTY，以及包含设备总览、发现、终端、操作和插件视图的 Tauri 桌面应用。

## 安装

### 前置条件

- Rust stable（桌面 crate 需要 Rust 1.77.2 或更新版本）
- OpenSSH 客户端；建议 OpenSSH 8.4 或更新版本
- 可选：Android Platform Tools（`adb`）
- 可选：`rsync`，用于最快的同步路径

从源码构建 CLI：

```bash
git clone https://github.com/NinoC137/ferry.git
cd ferry
cargo build --release

# 安装到 PATH 中已有的目录。
install -m755 target/release/fy /usr/local/bin/fy

fy --version
fy doctor
```

Ferry 的本地状态保存在 `~/.config/ferry/`：

```text
devices.toml  已保存的设备档案
facts/        已观察到的身份指纹与硬件事实
state.toml    转发、借网、黑匣子和守护进程状态
plugins/      已安装的本地扩展包
```

### 原生桌面应用

可选的桌面工作台在 [`desktop/`](desktop) 中。除 Rust 和 Node.js 外，它还需要常规的 Tauri macOS 构建前置条件。

```bash
cd desktop
npm ci
npm run tauri dev       # 开发模式
npm run tauri build     # 发布 bundle
```

macOS 的发布产物位于 `target/release/bundle/macos/Ferry Desktop.app`。脚本与 CI 场景仍建议使用 CLI。

## 快速开始

先为目标机创建一份档案。通道只是当前的连接路径，并非永久约束：串口设备可以在之后升格为 SSH。

```bash
# SSH 目标机
fy add rk --ssh root@192.168.1.37

# 旧版 Dropbear/OpenSSH 目标机
fy add old-board --ssh root@10.0.0.5 --legacy

# 只有一台已连接的 ADB 设备；也可显式指定 serial/IP
fy add phone --adb

# 串口控制台
fy add mcu --serial /dev/tty.usbserial-1420 --baud 1500000
```

不知道端点时，先发现再保存：

```bash
fy scan
fy scan --add
```

之后可在各类工作流中复用同一个设备名：

```bash
fy                         # 连通性与已保存的身份事实
fy sh rk                    # 交互式 shell
fy sh rk -- uname -a        # 执行一条远端命令
fy info rk                  # 身份卡片
fy push rk ./app /tmp/      # 带校验、可续传的上传
fy run rk ./app --help      # 上传、chmod、运行并返回远端退出码
```

## 常用工作流

| 目标 | 从这里开始 |
| --- | --- |
| 发现并确认一台开发板 | [`fy scan`、`fy info`](docs/operations.zh-CN.md#发现与身份确认) |
| 终端、日志与并行命令 | [`fy sh`、`fy log`、`fy all`](docs/operations.zh-CN.md#日常操作) |
| 传输、部署或调试 | [`fy push`、`fy run`、`fy debug`、`fy sync`](docs/operations.zh-CN.md#传输与部署) |
| 端口转发或借用网络 | [`fy fwd`、`fy share`、`fy net`](docs/operations.zh-CN.md#连通性与网络) |
| 恢复只剩串口的设备 | [`fy bb`、`fy blame`、`fy up`](docs/operations.zh-CN.md#串口恢复) |
| 采集硬件报告 | [`fy hw`](docs/operations.zh-CN.md#硬件采集) |
| 加入可审查的本地流程 | [`fy plugin`](docs/operations.zh-CN.md#本地插件) |
| 构建机器集成 | [`fy --json`、`fy help --json`](docs/operations.zh-CN.md#自动化与-json) |

## 交互式工作台

### 浏览器工作台

```bash
fy ui
fy ui --port 8000 --no-open
```

`fy ui` 默认只绑定到 `127.0.0.1`。其主区域是真实系统 PTY，通过持久 WebSocket 由 xterm.js 渲染，因此 `vim`、补全、Ctrl-C、全屏工具和长任务都与普通终端一致。

### 桌面工作台

Tauri 应用提供设备总览、已验证的网络发现、可编辑的档案草稿、xterm 会话、传输/转发/top/黑匣子控制、受保护的网络与恢复工作流，以及插件工作台。高影响工作流会在执行前展示预检计划。客户端边界与安全约束见[架构说明](docs/architecture.zh-CN.md#交互客户端)。

## 文档

| 文档 | 内容 |
| --- | --- |
| [文档索引](docs/README.md) | 英文与中文资料导航 |
| [中文操作指南](docs/operations.zh-CN.md) | 按命令组织的工作流、安全说明和插件 |
| [中文架构说明](docs/architecture.zh-CN.md) | 模块、状态模型、通道边界和客户端 |
| [English operations guide](docs/operations.md) | English command and workflow reference |
| [English architecture](docs/architecture.md) | English architecture and client overview |
| [`fy --help`](#快速开始) | 当前安装版本的权威命令清单 |

## 安全边界

Ferry 可以改变主机和目标机状态。涉及设备、路由、防火墙或服务配置的操作前，请先阅读计划并使用 `fy --dry-run`。

- `fy share --nat` 和 `fy usb net --share` 可能需要 `sudo`，并改变主机转发、防火墙或网络接口状态。
- `fy up`、USB gadget 安装和持久代理设置可能修改目标机网络或启动时配置。
- 设备档案可能将密码保存在 `~/.config/ferry/devices.toml`；Ferry 会尝试设置为 `0600`。优先使用 `fy keyup` 和密钥认证。
- `--legacy` 会启用已淘汰的 SSH 算法，只适用于隔离的旧设备。
- 仅扫描你有权探测的网络。

## 开发

```bash
# 核心库与 CLI 测试
cargo test -p ferry --lib

# 构建发布版 CLI
cargo build --release

# 桌面端检查
cd desktop
npm ci
npm run build
cd ..
cargo check -p ferry-desktop
```

开发约定见 [CONTRIBUTING.md](CONTRIBUTING.md)，安全问题请参见 [SECURITY.md](SECURITY.md)。

## 许可证

Ferry 以 [MIT License](LICENSE) 发布。
