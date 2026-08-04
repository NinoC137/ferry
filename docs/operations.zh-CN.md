# 操作指南

本指南按实际任务组织 Ferry 命令。准确语法以你所安装版本的 `fy --help` 为准；会改变主机或目标机状态的操作前，请先执行 `fy --dry-run`。

## 发现与身份确认

```bash
fy scan
fy scan --subnet 192.0.2.0/24
fy scan --ports 2222,2200
fy scan --no-mdns
fy scan --add

fy info rk
fy forget rk
```

`fy scan` 会组合 mDNS、受限并发 TCP 扫描、SSH banner 验证、ARP MAC 查询、ADB 枚举和串口枚举。网络候选项刻意采取保守策略：只有给出有效 SSH 协议 banner 的主机，或已经获得授权的网络 ADB 端点，才会被提供为设备档案候选。

Ferry 会保存已观察到的事实，以便在 DHCP 或刷机改变地址后重新识别熟悉的目标机。设备刷机后需要清除已保存 SSH 主机密钥时，使用 `fy forget <设备>`。不要扫描未经授权的网络。

## 日常操作

```bash
fy sh rk
fy sh rk -- 'systemctl status my-service'
fy log rk
fy log phone
fy top
fy all -- uname -a
fy all rk cam -- 'df -h /'
```

`fy sh` 打开交互会话或执行一条命令。`fy log` 会按所选通道自动选择 `journalctl`、syslog、`dmesg` 或 `logcat`。`fy top` 并行采样兼容目标机的健康信息；`fy all` 会对匹配的档案并行执行命令。

应尽早配置基于密钥的 SSH：

```bash
fy keyup rk
```

`fy keyup` 可以在尊重当前档案兼容性设置的前提下生成并安装公钥。相较于把密码保存到本地档案中，这是一条更推荐的路径。

## 传输与部署

```bash
fy push rk ./rootfs.img /tmp/
fy push --all ./app /opt/app --only rk
fy pull rk /var/log/messages ./logs/

fy cp ./firmware.bin rk:/tmp/
fy cp rk:/var/log/messages ./logs/
fy cp rk:/tmp/fw.bin cam:/data/local/tmp/

fy run rk ./app --verbose
fy debug rk ./app --port 3333
fy sync rk ./build /opt/app --exec 'systemctl restart app'
```

`push` 和 `pull` 会在续传前验证既有文件前缀，在可能时检查远端空间，并在目标机具备兼容哈希工具时做 SHA-256 验证。`cp` 支持本地到设备、设备到本地以及设备到设备；板间数据经过主机但不会作为中间文件落在主机磁盘上。

常用传输控制项：

| 旗标 | 含义 |
| --- | --- |
| `--force` | 即使目标端看起来相同也重新传输 |
| `--no-resume` | 禁用断点续传 |
| `--no-verify` | 禁用传输后的完整性校验 |
| `--scp` | 使用旧的 scp/tar 逃生路径；它没有 Ferry 的续传/校验语义 |

`fy run` 会传输、设置可执行、运行并返回远端退出码。`fy debug` 会建立 `gdbserver` 工作流和所需转发，并给出连接命令。`fy sync` 会选择合适的 rsync/tar/ADB 路径，并可在同步后执行一条命令。

## 连通性与网络

```bash
fy fwd rk 8080
fy fwd rk 8080:80
fy fwd rk R:9000:8000
fy fwd rk D:1080
fy fwd ls
fy fwd rm f1

fy watch start
fy watch status

fy net rk
fy net rk --no-speed
```

对于 SSH 档案，Ferry 使用连接复用，并能通过 `fy watch` 在断线后回放转发。对应的 ADB 正向/反向转发则通过 ADB 原生命令实现。

```bash
# 默认模式：通过反向隧道使用主机上的 HTTP/SOCKS5 代理，不需要 sudo。
fy share rk
fy share rk --upstream auto
fy share rk --persist

# 显式要求为直连 SSH 目标机开启主机 NAT。
fy share rk --nat
fy share rk --off

# 主机侧 USB 配网，并按需共享网络。
fy usb net --share
```

默认 `share` 模式经由反向隧道公开主机代理。`--nat` 被刻意设计成独立模式，因为它可能改变主机的路由和防火墙状态。`fy net` 会采集延迟、抖动、丢包、MTU、路由、DNS、外网连通性，以及可选的真实吞吐。

## 串口恢复

```bash
fy bb start mcu
fy bb status
fy blame mcu
fy bb stop mcu

fy up mcu
fy up mcu --boot
```

串口黑匣子持续记录控制台，并识别常见的 kernel panic、Oops、lockup 和 OOM 特征。当它运行时，`fy sh` 会通过共享 Unix socket 接入，而不会和它争抢串口所有权。

`fy up` 是尽力而为的通道爬升流程：它会尝试串口登录、目标机和网络探测、在适用时尝试 USB gadget 或 DHCP、检查 SSH 可达性，并可选配置公钥。若爬升未完成，原始串口仍是恢复路径。`--boot` 允许流程跨越 bootloader 边界；使用前请审阅计划。

## USB gadget 与本地产物服务

```bash
fy usb gadget --out ferry-gadget.sh --mode ncm
fy usb install rk --autostart

fy serve ./out --for rk
fy serve ./out --upload ./inbox
```

`ncm` 是 macOS 和 Linux 推荐的 USB gadget 模式；仅在兼容性确有需要时使用 `rndis`。`fy usb install` 可以将生成的 gadget 配置放到目标机上并注册为启动时服务，因此它会改变目标机。`fy serve` 为带有 `wget` 或 `curl` 的极简/恢复目标机提供本地产物服务；`--upload` 用于反向收集文件。

## 硬件采集

```bash
fy hw rk --out ./rk-hardware
fy hw rk --out ./rk-hardware --max-dt-nodes 1024
fy hw rk --out ./rk-hardware --no-bundle

fy hw brief ./rk-hardware/hardware.json
fy hw brief ./rk-hardware/hardware.json --out ./peripherals.md
```

`fy hw` 的设计目标是只读采集：它读取 procfs、sysfs 和在线设备树；不会加载模块、扫描 I2C/SPI 总线、写入 sysfs 或改变目标机安全策略。报告目录包含 `hardware.json`、可选的人类可读 `peripherals.md`，以及在目标机有 `tar` 支持时生成的 `device-tree.tar`。

在 Android 上，Ferry 会选择合适的可写部署目录，而不假设 `/tmp` 可用。硬件标识符默认会被脱敏；只有在报告保存环境合适时才应选择收集可识别数据。

## 本地插件

插件是承载不适合进入核心二进制的主机侧工作流的、可审查本地包。一个包包含 `plugin.toml` 与其中 `entry` 声明的可执行入口。清单会声明通道需求、主机依赖、参数、风险、摘要和执行预览。Ferry 会拒绝入口路径逃逸，并将包安装在 `~/.config/ferry/plugins/<id>/`（或 `$FERRY_HOME/plugins/<id>/`）。

```bash
fy plugin ls

fy plugin install sysroot-sync
fy plugin show sysroot-sync
fy plugin run sysroot-sync rk -- --dest ~/ferry-sysroots/rk --no-sudo

fy plugin install device-tree-pull
fy plugin run device-tree-pull rk -- --out ./rk-hardware

fy plugin install /path/to/my-ferry-plugin
```

内置的 **Sysroot Sync** 会从 SSH 目标机镜像 `/lib`、`/usr/lib` 和 `/usr/include`，同时保留 Ferry 档案中特有的 SSH 选项。其 `--delete` 可能删除本地 sysroot 文件，因此默认不启用。

内置的 **Device Tree Pull** 使用 Ferry 原生只读采集器来恢复 `device-tree.tar`、`hardware.json` 和可选的 `peripherals.md`。它支持 SSH 和已授权 ADB 档案，要求输出目录必须是新目录或空目录，并会在恢复后删除目标机临时目录。安装任何插件（包括他人提供的本地包）前都应阅读源代码。

桌面插件工作台刻意只使用 SSH 密钥：保存的档案密码绝不会传给后台插件命令。桌面端若要使用 `sudo` 目标路径，必须已有通过 `sudo -v` 授权的本地会话，否则应选择用户可写目录。

## 自动化与 JSON

```bash
fy --json ls
fy --json sh rk -- 'uname -a'
fy --json push rk ./firmware.bin /tmp/
fy --json net rk
fy help --json
```

对已支持的命令，JSON 模式保证 stdout 只输出一份 JSON。进度与诊断信息进入 stderr；提示被禁用；歧义或不安全的操作将返回附带提示的结构化失败。可用 `FERRY_JSON=1` 为一个进程环境启用 JSON 模式。

完整命令清单和稳定退出码说明可由 `fy help --json` 获取。`ui`、`serve`、`log`、`top` 等流式命令会拒绝 JSON 模式，而不会混合终端输出和机器可读数据。
