# ferry (fy) — 上位机↔下位机摆渡人

> 一套命令，把 **ssh / adb / 串口** 三种下位机通道摆平。为嵌入式 Linux/Android 下位机开发而生。
> 纯 Rust、零第三方依赖（只调用系统里的 `ssh`/`adb`/`stty`），单文件二进制，`cargo build --release` 就能用。

---

## 为什么再造一个轮子

日常你大概是这样开发下位机的：`ssh root@192.168.1.37`、`adb -s xxxx shell`、拿 minicom/串口助手连 console……几个痛点一直没人正经解决：

- **三套工具、三套心智模型。** ssh 一套参数，adb 一套子命令，串口又是另一套 GUI。同一块板子从串口调到网络，脑子要来回切换。
- **板子换了 IP 就不认识了。** 重刷系统 / DHCP 重新分配，`known_hosts` 冲突、档案里的 IP 全废，你得重新找一遍。
- **借网难。** 板子在隔离网段没有外网，装个包、curl 个东西都难。传统做法要配 NAT、改路由、开 IP 转发，一堆 sudo。
- **USB 配网是玄学。** configfs / g_ether / RNDIS / NCM，主机侧还要手动认网口、配 IP，每次都查文档。
- **串口是"独占"的。** 一个人连着 console，另一个人就连不上；关掉终端，板子半夜 panic 的现场也就没了。
- **改一行代码，上板要五步。** 交叉编译 → scp → chmod → 跑 → 看输出，循环几十次。
- **传大文件靠运气。** 几百 MB 的 rootfs 传到 90% 断了，scp 只能从头再来；传完也不知道对不对，烧进 flash 才发现坏了。
- **让 AI 帮忙做不到。** 想让 agent 替你部署/排查，它面对的是一堆给人看的彩色表格，没法可靠地解析。

ferry 把这些揉进**一套动词**里，并加了几个"市面上没有"的功能：

| 能力 | 命令 | 别处的做法 |
|---|---|---|
| **通道爬升** | `fy up` | 手动串口登录→手动配网→手动 ssh |
| **指纹认领**（换 IP 也认识你） | `fy scan` | 手动比对 MAC / 重新记 IP |
| **串口黑匣子**（后台录制+panic 侦测+桌面通知） | `fy bb` | 关了终端现场就没了 |
| **借网给板子**（零 sudo、全平台） | `fy share` | 手配 NAT + 路由 + IP 转发 |
| **USB 一键配网** | `fy usb net` | 查文档配 configfs + 认网口 |
| **保存即上板** | `fy sync` | 手动 scp 循环 |
| **多板仪表盘** | `fy top` | 挨个 ssh 上去 top |
| **断点续传 + 校验的传输** | `fy push` / `fy pull` | scp 断了从头来，且传完不校验 |
| **局域网快传** | `fy serve` | 板上没 scp 时只能干瞪眼 |
| **板↔板直传** | `fy cp a:/x b:/y` | 先拉到主机再推上去 |
| **网络体检一条龙** | `fy net` | ping/ip/route/resolv.conf 挨个敲 |
| **隧道断线自愈** | `fy watch` | 转发全废了才发现，手动重来 |
| **机器可读接口** | `fy --json ...` | 拿 awk 去啃人类输出 |

---

## 安装

需要本机有 `ssh`（OpenSSH ≥ 8.4 更佳）、可选 `adb`、`rsync`。串口和终端用系统自带的 `stty`，无需 minicom。

```bash
cd ferry
cargo build --release
# 二进制在 target/release/fy，丢进 PATH 即可
install -m755 target/release/fy /usr/local/bin/fy    # 或 ~/bin/fy
fy --help
fy doctor          # 先体检一下主机环境
```

macOS / Linux 都能编。配置存在 `~/.config/ferry/`（`devices.toml` 设备档案、`facts/` 指纹、`state.toml` 运行态）。

---

## 硬件快照与外设简报

fy hw 将目标端的 procfs、sysfs 和 live device tree 回收到稳定的 hardware.json，再在主机端生成面向人的 peripherals.md。采集器只读取，不加载模块、不扫描 I2C/SPI、不改 SELinux 或 sysfs；简报把运行时枚举到的设备和设备树能力线索分开表达。

    fy hw rk --out ./rk-hardware
    fy hw rk --out ./rk-hardware --max-dt-nodes 1024
    fy hw rk --out ./rk-hardware --no-brief

结果目录默认包含 hardware.json、peripherals.md，以及目标端有 tar 时的 device-tree.tar。peripherals.md 优先展示板型/SoC、CPU、实际存储、网络、I2C/SPI/USB、UART/显示/相机/PCIe/GPIO 的设备树线索、温度与电源状态；完整的寄存器、时钟、中断和节点属性仍保留在 JSON 与 raw DT archive 中。

已保存的报告无需重连开发板即可重新解释：

    fy hw brief ./rk-hardware/hardware.json
    fy hw brief ./rk-hardware/hardware.json --out ./comparison.md

## 5 分钟上手

```bash
# 1) 建档（三选一）
fy add rk --ssh root@192.168.1.37            # ssh 板子
fy add rk --ssh root@10.0.0.5 --legacy       # 老 dropbear / 旧算法，加 --legacy
fy add cam --adb                             # 唯一一台 adb 设备
fy add mcu --serial /dev/tty.usbserial-1420 --baud 1500000   # 串口

# 或者让它自己找：
fy scan --add                                # 扫周围 ssh/adb/串口，交互建档

# 2) 看板子
fy                                           # 总览（并行探活 + 身份）
fy sh rk                                      # 进 shell
fy sh rk -- uname -a                          # 跑一条命令

# 3) 传文件 / 上板跑
fy push rk ./firmware.bin /tmp/               # 带进度条、断点续传、传完对 sha256
fy push --all ./firmware.bin /tmp/            # 一次推到所有在线板子
fy cp rk:/var/log/messages ./                 # 拉回来；两端都写 设备:/路径 就是板↔板直传
fy run rk ./a.out --verbose                   # push+chmod+运行+回传退出码

# 4) 网络不对劲时
fy net rk                                     # 延迟/丢包/MTU/路由/DNS/出网 + 上下行实测带宽
fy share rk                                   # 板子没网？借主机的
fy watch start                                # 隧道断了自动重连并重放所有转发

# 5) 免密 & 忘掉重刷的板子
fy keyup rk                                    # 装公钥，之后免密（兼容 dropbear）
fy forget rk                                   # 板子重刷后清 host key
```

---

## 图形工作台 `fy ui`

不想记命令？`fy ui` 起一个本地 Web 工作台，浏览器自动打开：

```bash
fy ui                 # 默认 127.0.0.1:7900，自动开浏览器
fy ui --port 8000 --no-open
```

- **主区是一个真·系统终端**（PTY 跑你的登录 shell，xterm.js 经 WebSocket 双向流）——vim、top、任何交互命令照跑，`fy` 已在 PATH 里随手可用。
- **侧栏是便捷工具**：设备列表（实时状态圆点、身份、一键「终端/日志/借网/爬升/信息」）、快捷动作（扫描/总览/仪表盘/体检/USB 配网）、端口转发面板（含新建表单）、串口黑匣子（事故计数、一键看现场）、借网状态。
- **点侧栏 = 往终端注入命令**，所见即所得——你永远看得到实际执行了什么，不搞黑箱。

技术上依然零依赖：内置 HTTP/WebSocket 服务（自己实现 SHA-1/base64 握手与帧编解码）、PTY 直接用 libc（`extern "C"`，不引入任何 crate）。前端单文件，xterm.js 从 CDN 加载。

## 招牌功能

### `fy up` — 通道爬升
一条命令把板子带到"能用的最好通道"。串口自动登录 → 采指纹 → 探测板子有没有 IP / UDC / 网口 → 灌 USB gadget 或跑 DHCP → 起 ssh，并顺手经串口把你的公钥装进去（ssh 到手即免密）。爬不上去？串口 shell 照样能用，指纹也已入档。

```bash
fy up mcu            # 串口起步，尽力爬到 ssh
fy up mcu --boot     # 板子停在 U-Boot 时，允许它 boot 引导内核
```

### `fy push` / `fy pull` — 断点续传 + 校验的传输

传输不再是 `scp` 的薄包装，而是自己的引擎：**传前**探远端尺寸和可用空间，
**传中**画进度条（速度/ETA），**传后**两边对 sha256。断了再敲一次就从断点接上——
接之前会先比对**已有部分的前缀哈希**，对不上就老实全量重来，绝不在错误的前缀上追加。

板端只用到 `cat`/`head`/`tail`/`wc`，busybox 就够；连 sftp-server 和 scp 都不需要。
两边一模一样时直接跳过，反复 `fy push` 是幂等的。

```bash
fy push rk ./rootfs.img /tmp/          # 断了？再敲一次，从断点接着传
fy push rk ./build /opt/               # 目录按 rsync 语义（./build/ 则把内容铺进去）
fy push --all ./app /opt/ --only rk    # 并行分发到所有在线板子（--only 按前缀筛）
fy pull rk /var/log/messages ./logs/
# --force 强制重传 · --no-resume 关续传 · --no-verify 关校验 · --scp 退回老路子
```

### `fy serve` — 局域网快传

板子上没有 scp、只读 rootfs、recovery 里只剩一个 busybox wget——这时候起个 HTTP 服务最快。
自动算出**板子该访问哪个地址**（`--for` 时按路由表选直连网口），把能直接粘贴到板子上的命令打出来；
支持 Range 所以板上 `wget -c` 能续传；`--upload` 还能反着收板子传上来的文件。
默认带随机 token 前缀，同一个局域网里别人猜不到。

```bash
fy serve ./out --for rk               # 打印板端可直接粘贴的 wget 命令
fy serve ./out --upload ./inbox       # 板子: curl -T core.dump http://<主机>:8000/<tok>/up/
```

### `fy cp` — 一个入口，三种方向

```bash
fy cp ./a.bin rk:/tmp/                # 本地 → 板
fy cp rk:/var/log/messages ./         # 板 → 本地
fy cp rk:/tmp/fw.bin cam:/data/       # 板 → 板，经主机流式中转，不落主机磁盘
```

### `fy net` — 网络体检

排查板子网络时来回敲的那几样，一条命令做完：TCP 建连的延迟/抖动/丢包（不需要 ICMP 权限）、
网口载波与 MTU（**和主机比对**，MTU 不一致正是"scp 传一半卡死"的经典原因）、
`/proc/net/dev` 的收发错误与丢包计数、默认路由与网关可达性、DNS 配没配/解得动不动、
真正的出网测试，外加**上下行实测带宽**（各约 3 秒，零依赖打流）。

最后不是丢一堆数字给你，而是给结论："板子没有默认路由，`fy share rk` 可以直接借主机的网"。

```bash
fy net rk               # 完整体检 + 测速
fy net rk --no-speed    # 只体检，不打流
```

### `fy watch` — 隧道断线自愈

端口转发和借网都挂在 ssh 的 ControlMaster 上，板子重启一次就全废，而你往往十分钟后才发现。
`fy watch` 起个守护进程周期探活，掉线就重建连接并**把这台设备的所有转发和 share 反向隧道重新挂回去**，
连不上就指数退避，恢复时桌面通知你一声。建了转发或开了借网之后会自动拉起（`--no-watch` 可拒绝）。

```bash
fy watch start          # 默认 15s 一次
fy watch status         # 谁在被盯着、上次探活、自愈过几次
```

### `fy scan` — 发现 + 认领
先发一个 **mDNS 组播查询**（1.5 秒，本机每个网口各发一遍，USB 直连网段也不漏），
让该露面的设备自报家门；再用 128 并发的线程池扫网段的 22/23/80/5555/8022——
任务粒度是 (IP, 端口)，一台超时的主机不会拖住同组其它主机，邻居表里出现过的地址还会给更宽的超时。
读 ssh banner（顺便判断要不要 `--legacy`），用 ARP 的 MAC 和指纹库比对——**板子换了 IP 也能认出"这是上次那台 rk"**，
并提示你更新档案。adb 设备、串口一并列出。`--no-mdns` 可以只走端口扫描。

### `fy share` — 借网给板子
让没有外网的板子借主机上网。默认"代理模式"：内置代理 + 反向隧道，**零 sudo、ssh/adb 可达就行、DNS 都在主机侧解析**。

内置代理**一个端口同时说 HTTP 代理和 SOCKS5**（靠首字节 0x05 分流），所以板子上
`http_proxy` 和 `all_proxy=socks5://` 指同一个地址即可，apt/opkg/wget/curl/git 全照顾到。
主机自己在梯子后面时，`--upstream` 把板子的流量接着往上游送——**等于把你的梯子也借给了板子**。
要全协议（ping、原始套接字）就 `--nat` 走真 NAT。

```bash
fy share rk                                      # 代理模式（推荐，最省事）
fy share rk --upstream http://127.0.0.1:7897     # 链到主机的上游代理
fy share rk --upstream auto                      # 直接读主机的 https_proxy/all_proxy
fy share rk --persist                            # 顺手写进板子 /etc/profile.d
fy share rk --nat                                # 直连板子做真 NAT（要 sudo，全协议）
fy share rk --off                                # 关闭并恢复
fy proxy status                                  # 代理进程/端口/上游/谁在借网
```

### `fy usb net` / `fy usb gadget` — USB 一键配网
插上 USB 线，主机侧自动识别新网口、配 IP、探测板子、可选开 NAT 共享。板端脚本一键生成/安装，支持 NCM（macOS/Linux 首选）和 RNDIS（老 Windows），可注册开机自启。

```bash
fy usb net --share                   # 主机侧：认网口→配IP→探板→借网
fy usb gadget --out g.sh --mode ncm  # 生成板端脚本
fy usb install rk --autostart        # 推到板上并注册开机自启
```

### `fy bb` — 串口黑匣子
后台守护进程常驻串口，**持续录制 + 侦测 kernel panic/oops/OOM + 桌面通知**。之后 `fy sh` 会自动经黑匣子**共享**串口（录制与交互两不误，告别"串口被占用"）。板子半夜崩了，`fy blame` 看现场——重启也不丢。

```bash
fy bb start mcu       # 开始后台录制
fy blame mcu          # 最近一次崩溃现场
fy bb status          # 谁在录、录了多少、几起事故
```

### `fy sync` — 保存即上板
监视本地目录，一有改动就增量部署（板上有 `rsync` 走 rsync，没有就 tar 管道兜底 / adb push），并可跑钩子命令（比如重启服务）。

```bash
fy sync rk ./build /opt/app --exec "systemctl restart app"
```

### 还有
- `fy fwd` — 端口转发管理器：`8080` · `8080:80` · `R:9000:8000`（反向）· `D:1080`（SOCKS5）。ssh 隧道挂在连接复用上动态增删，`fy fwd ls` 一览死活；掉线由 `fy watch` 自动补回。
- `fy debug` — gdbserver 一条龙：起服务 + 端口转发 + 给出交叉 gdb 连接命令。
- `fy top` — 多板实时仪表盘（CPU/内存/温度/rootfs/负载/uptime）。
- `fy all -- <命令>` — 对所有在线设备并行执行，彩色前缀区分。
- `fy log` — 跟日志，journalctl/syslog/dmesg/logcat 自动选。
- `fy doctor` / `fy fix time` — 主机自检、板子体检、无 RTC 板子一键对时（告别 1970）。
- `fy wifi` — adb 一键从 USB 切到 WiFi。

---

## 给 AI agent 用：`--json`

ferry 的另一半用户是 agent。让 agent 去解析给人看的彩色表格是不现实的，所以每个命令都有
机器可读的一面：

```bash
fy --json ls                     # 设备清单 + 在线状态 + 指纹
fy --json sh rk -- 'uname -a'    # {stdout, stderr, exit_code}
fy --json push rk ./fw.bin /tmp/ # 每个文件的字节数/是否跳过/是否校验通过/速率
fy --json net rk                 # 结构化体检报告
fy help --json                   # 命令清单 + 参数 + 退出码表，一次读全
```

契约（agent 可以依赖这几条）：

- 加了 `--json`，**stdout 有且只有一份 JSON 文档**；`→ ssh ...`、进度条、提示全部走 stderr。
  `fy --json ls | jq` 永远是干净的。
- `--json` 隐含**非交互**：不会弹设备选择器、不会问 y/n。需要人拍板的地方直接以
  `code 19` 失败，并在 `hint` 字段里说清楚该补哪个参数——比如"有 3 台设备，得指名道姓，可选: rk, cam, mcu"。
- 失败长这样：`{"ok":false,"cmd":"push","code":15,"error_kind":"checksum_mismatch","error":"...","hint":"..."}`。
- **退出码稳定**，只增不改：

  | 码 | 含义 | 码 | 含义 |
  |---|---|---|---|
  | 0 | 成功 | 13 | 该通道不支持此操作 |
  | 1 | 兜底失败 | 14 | 传输失败 |
  | 2 | 用法错误 | 15 | 校验不一致（数据可能损坏） |
  | 10 | 没有这台设备 | 16 | 超时 |
  | 11 | 设备名有歧义 | 17 | 主机缺 ssh/adb 等依赖 |
  | 12 | 设备不可达 | 18 | 配置/运行态文件有问题 |
  |  |  | 19 | 需要人拍板但当前非交互 |

- `fy sh` / `fy run` 会**透传远端命令的退出码**，理论上可能和上表撞车——撞车时以 `ok` 字段为准。
- `FERRY_JSON=1` 等价于处处加 `--json`；`-n/--dry-run` 只打印将执行的命令，不产生任何副作用
  （dry-run 下 JSON 依然是干净的）。
- 常驻/流式的命令（`fy ui`、`fy serve`、`fy log`、`fy top` 等）没有 `--json`，在该模式下会
  明确以 `code 2` 拒绝并告诉你去看 `fy help --json` 里的 `commands[].json` 字段，
  而不是吐一堆人类表格把解析搞崩。

## 设计原则

- **零第三方依赖。** 只用 Rust 标准库，通过编排系统 `ssh`/`adb`/`stty` 干活。改起来简单，编译快，单文件好分发。
  SHA-256、SOCKS5、mDNS、HTTP/WebSocket、PTY 全是自己实现的几百行。
- **`-n` / `--dry-run` 随时可用。** 任何有副作用的命令加 `-n` 只打印将执行的每条命令，不动真格。适合先看清楚再动手。
- **对老/弱板子友好。** dropbear、busybox、toybox、只读 rootfs、没有 sftp-server 的板子都照顾到。
  传输引擎只要板上有 `cat` 就能跑；有 `sha256sum`（或 `shasum`/`busybox`/`openssl`）就能全量校验，
  一个都没有会退化成尺寸比对**并明确告诉你**，不假装校验过了。
- **凭据本地化。** 密码明文存 `~/.config/ferry/devices.toml`（0600 权限），并处处提示你 `fy keyup` 转免密。
- **不骗人。** 跳过了就说跳过，没校验就说没校验，只测了尺寸就说只测了尺寸。

`fy --help` 看完整命令参考。

---

## 目录结构

```
ferry/
├── Cargo.toml
├── src/
│   ├── main.rs         # CLI 分发 + 各子命令
│   ├── jsonout.rs      # --json 输出通道 + 稳定退出码（agent 接口的契约在这）
│   ├── config.rs       # 设备档案 / 指纹 / 运行态
│   ├── tomlite.rs      # 够用的零依赖 TOML 子集
│   ├── util.rs         # 彩色输出/表格/命令执行(含 dry-run)/守护进程
│   ├── sshx.rs         # ssh 封装：连接复用/免 sshpass 密码/级联传输
│   ├── adbx.rs         # adb 封装 + WiFi 切换
│   ├── serialx.rs      # 串口(termios via stty) + expect 引擎
│   ├── xfer.rs         # 传输引擎：进度 + 断点续传 + sha256 校验 + 板↔板直传
│   ├── hash.rs         # 零依赖 SHA-256
│   ├── serve.rs        # fy serve：局域网快传（下载 + 上传）
│   ├── up.rs           # 通道爬升状态机
│   ├── fwd.rs          # 端口转发管理
│   ├── proxyd.rs       # 内置代理守护进程：HTTP + SOCKS5 同端口 + 上游代理链
│   ├── share.rs        # 借网（代理/NAT）
│   ├── watchd.rs       # 隧道保活与断线自愈
│   ├── netdiag.rs      # fy net：网络体检 + 实测带宽
│   ├── usbnet.rs       # USB 配网 + gadget 脚本
│   ├── scan.rs         # 设备发现（mDNS + 有界并发扫段）+ 指纹认领
│   ├── mdns.rs         # 零依赖 mDNS 组播查询与解析
│   ├── fingerprint.rs  # 身份采集
│   ├── blackbox.rs     # 串口黑匣子守护进程
│   ├── sync.rs         # 保存即上板
│   ├── runx.rs         # 上板运行 / gdbserver 调试
│   ├── logs.rs         # 日志跟随 + 多板 top
│   ├── doctor.rs       # 环境自检 + 对时
│   ├── ui.rs           # fy ui：Web 工作台服务（WebSocket 终端桥 + REST）
│   ├── pty.rs          # 零依赖 PTY（libc FFI，真伪终端）
│   ├── httpd.rs        # 内置 HTTP/1.1 服务
│   └── wsutil.rs       # SHA-1 / base64 / WebSocket 帧（零依赖）
└── assets/
    ├── ferry-gadget.sh # 板端 USB gadget 脚本（configfs, busybox 兼容）
    └── ui.html         # 工作台前端（xterm.js 终端 + 侧栏，单文件）
```

MIT License.
