//! `fy net` —— 一条命令说清"板子的网到底哪儿坏了"。
//!
//! 排查下位机网络问题时来回试的那几样，这里一次做完：
//! 链路层（网口状态/MTU/收发错误）、可达性（TCP 建连延迟/抖动/丢包）、
//! 路由（默认网关、网关通不通）、DNS（配了吗、解析得动吗）、
//! 出网（能不能真的摸到外面）、以及**上下行实测带宽**。
//!
//! 全部零依赖：延迟用 TCP 建连计时（不需要 ICMP 权限），带宽用 ssh 管道打流，
//! 板端只用到 `dd`/`cat`/`/sys` 里的文件。

use crate::adbx;
use crate::config::{Device, Transport};
use crate::jsonout::J;
use crate::sshx;
use crate::util::*;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// 打流的上限：时间和体积谁先到算谁，免得在慢链路上跑到天荒地老。
const SPEED_MAX_BYTES: u64 = 16 * 1024 * 1024;
const SPEED_MAX_SECS: f64 = 3.0;

#[derive(Debug, Default, Clone)]
pub struct Latency {
    pub sent: u32,
    pub recv: u32,
    pub min_ms: f64,
    pub avg_ms: f64,
    pub max_ms: f64,
    /// 相邻两次的平均抖动，看链路稳不稳
    pub jitter_ms: f64,
}

impl Latency {
    pub fn loss_pct(&self) -> f64 {
        if self.sent == 0 {
            return 0.0;
        }
        (self.sent - self.recv) as f64 * 100.0 / self.sent as f64
    }
}

#[derive(Debug, Default, Clone)]
pub struct NetReport {
    pub device: String,
    pub transport: String,
    pub endpoint: String,
    pub latency: Latency,
    pub up_bps: f64,
    pub down_bps: f64,
    pub iface: String,
    pub iface_mtu: i64,
    pub host_mtu: i64,
    pub carrier: String,
    pub speed_mbps: i64,
    pub rx_err: i64,
    pub tx_err: i64,
    pub rx_drop: i64,
    pub tx_drop: i64,
    pub gateway: String,
    pub gw_reachable: Option<bool>,
    pub dns_servers: Vec<String>,
    pub dns_ok: Option<bool>,
    pub inet_ok: Option<bool>,
    pub proxy_env: String,
    pub notes: Vec<String>,
}

// ---------------- 延迟 / 丢包 ----------------

/// 用 TCP 建连计时代替 ICMP：不用 root、被防火墙挡的概率还更低。
pub fn tcp_latency(host: &str, port: u16, count: u32, timeout_ms: u64) -> Latency {
    let mut l = Latency {
        sent: count,
        ..Default::default()
    };
    let addr: SocketAddr = match format!("{}:{}", host, port).parse() {
        Ok(a) => a,
        Err(_) => return l,
    };
    let mut samples = vec![];
    for i in 0..count {
        let t = Instant::now();
        if TcpStream::connect_timeout(&addr, Duration::from_millis(timeout_ms)).is_ok() {
            samples.push(t.elapsed().as_secs_f64() * 1000.0);
        }
        if i + 1 < count {
            std::thread::sleep(Duration::from_millis(60));
        }
    }
    l.recv = samples.len() as u32;
    if samples.is_empty() {
        return l;
    }
    l.min_ms = samples.iter().cloned().fold(f64::MAX, f64::min);
    l.max_ms = samples.iter().cloned().fold(0.0, f64::max);
    l.avg_ms = samples.iter().sum::<f64>() / samples.len() as f64;
    if samples.len() > 1 {
        let d: f64 = samples.windows(2).map(|w| (w[1] - w[0]).abs()).sum();
        l.jitter_ms = d / (samples.len() - 1) as f64;
    }
    l
}

/// adb 设备没有 IP 可连，改用一次 `adb shell echo` 的往返当延迟。
fn adb_latency(d: &Device, count: u32) -> Latency {
    let mut l = Latency {
        sent: count,
        ..Default::default()
    };
    let mut samples = vec![];
    for _ in 0..count {
        let t = Instant::now();
        if matches!(adbx::exec_capture(d, "echo p"), Ok(o) if o.status == 0) {
            samples.push(t.elapsed().as_secs_f64() * 1000.0);
        }
    }
    l.recv = samples.len() as u32;
    if !samples.is_empty() {
        l.min_ms = samples.iter().cloned().fold(f64::MAX, f64::min);
        l.max_ms = samples.iter().cloned().fold(0.0, f64::max);
        l.avg_ms = samples.iter().sum::<f64>() / samples.len() as f64;
        if samples.len() > 1 {
            l.jitter_ms = samples.windows(2).map(|w| (w[1] - w[0]).abs()).sum::<f64>()
                / (samples.len() - 1) as f64;
        }
    }
    l
}

// ---------------- 带宽 ----------------

/// 上行：本机往 `ssh 板子 'cat > /dev/null'` 里灌数据。
/// 计时到进程真正退出为止 —— 否则内核缓冲会让数字虚高。
pub fn speed_up(d: &Device) -> Result<f64, String> {
    if d.transport != Transport::Ssh {
        return Err("上行测速只支持 ssh 通道".into());
    }
    let mut a = vec!["ssh".to_string()];
    a.extend(sshx::base_opts(d));
    a.push(sshx::target(d));
    a.push("cat > /dev/null".to_string());
    let mut cmd = Command::new(&a[0]);
    cmd.args(&a[1..]);
    for (k, v) in sshx::askpass_env(d) {
        cmd.env(k, v);
    }
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| e.to_string())?;
    // 伪随机填充：全零会被某些链路/压缩层"优化"掉，测出来的数字不真
    let mut buf = vec![0u8; 256 * 1024];
    let mut x: u32 = 0x1234_5678;
    for b in buf.iter_mut() {
        x = x.wrapping_mul(1664525).wrapping_add(1013904223);
        *b = (x >> 24) as u8;
    }
    let t0 = Instant::now();
    let mut sent = 0u64;
    {
        let sin = child.stdin.as_mut().ok_or("拿不到 stdin")?;
        while sent < SPEED_MAX_BYTES && t0.elapsed().as_secs_f64() < SPEED_MAX_SECS {
            if sin.write_all(&buf).is_err() {
                break;
            }
            sent += buf.len() as u64;
        }
    }
    drop(child.stdin.take());
    let _ = child.wait();
    let secs = t0.elapsed().as_secs_f64();
    if secs <= 0.0 || sent == 0 {
        return Err("测不出来".into());
    }
    Ok(sent as f64 / secs)
}

/// 下行：让板子 `dd` 吐数据，本机读到底。
pub fn speed_down(d: &Device) -> Result<f64, String> {
    if d.transport != Transport::Ssh {
        return Err("下行测速只支持 ssh 通道".into());
    }
    let count = SPEED_MAX_BYTES / 65536;
    let mut a = vec!["ssh".to_string()];
    a.extend(sshx::base_opts(d));
    a.push(sshx::target(d));
    a.push(format!(
        "dd if=/dev/zero bs=65536 count={} 2>/dev/null",
        count
    ));
    let mut cmd = Command::new(&a[0]);
    cmd.args(&a[1..]);
    for (k, v) in sshx::askpass_env(d) {
        cmd.env(k, v);
    }
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| e.to_string())?;
    let t0 = Instant::now();
    let mut got = 0u64;
    {
        let sout = child.stdout.as_mut().ok_or("拿不到 stdout")?;
        let mut buf = vec![0u8; 256 * 1024];
        loop {
            match sout.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => got += n as u64,
            }
            if t0.elapsed().as_secs_f64() >= SPEED_MAX_SECS {
                break;
            }
        }
    }
    let secs = t0.elapsed().as_secs_f64();
    let _ = child.kill();
    let _ = child.wait();
    if secs <= 0.0 || got == 0 {
        return Err("测不出来".into());
    }
    Ok(got as f64 / secs)
}

// ---------------- 板端体检 ----------------

fn rexec(d: &Device, cmd: &str) -> Option<String> {
    match d.transport {
        Transport::Ssh => sshx::exec_capture(d, cmd).ok().map(|o| o.stdout),
        Transport::Adb => adbx::exec_capture(d, cmd).ok().map(|o| o.stdout),
        Transport::Serial => None,
    }
}

/// 一次 ssh 往返把板端该看的都看了。分段用哨兵行切开。
fn board_probe(d: &Device, r: &mut NetReport) {
    // 写成一行：`fy` 会把远端命令回显出来，多行脚本刷屏太难看
    let script = "echo '#IF'; ip -o -4 addr show 2>/dev/null | grep -v ' lo ' || ifconfig 2>/dev/null; \
         echo '#ROUTE'; ip route show default 2>/dev/null || route -n 2>/dev/null | grep '^0.0.0.0'; \
         echo '#DNS'; grep -h nameserver /etc/resolv.conf 2>/dev/null; \
         echo '#DEV'; cat /proc/net/dev 2>/dev/null; \
         echo '#PROXY'; echo \"$http_proxy|$https_proxy|$all_proxy\"; echo '#END'";
    let out = match rexec(d, script) {
        Some(o) => o,
        None => {
            r.notes.push("板端探测失败（连不上或串口通道）".into());
            return;
        }
    };
    let mut section = "";
    let mut iface_lines = vec![];
    let mut dev_lines = vec![];
    for line in out.lines() {
        let t = line.trim();
        if t.starts_with('#') {
            section = t;
            continue;
        }
        match section {
            "#IF" => iface_lines.push(t.to_string()),
            "#ROUTE" if r.gateway.is_empty() => {
                let toks: Vec<&str> = t.split_whitespace().collect();
                if let Some(i) = toks.iter().position(|x| *x == "via") {
                    r.gateway = toks.get(i + 1).unwrap_or(&"").to_string();
                } else if toks.len() >= 2 && toks[0] == "0.0.0.0" {
                    r.gateway = toks[1].to_string();
                }
            }
            "#DNS" => {
                if let Some(ns) = t.split_whitespace().nth(1) {
                    r.dns_servers.push(ns.to_string());
                }
            }
            "#DEV" => dev_lines.push(t.to_string()),
            "#PROXY" => {
                if !t.trim_matches('|').is_empty() {
                    r.proxy_env = t.to_string();
                }
            }
            _ => {}
        }
    }

    // 找出承载板子 IP 的那个网口
    if !d.host.is_empty() {
        for l in &iface_lines {
            if l.contains(&d.host) {
                // `2: eth0    inet 10.0.0.5/24 ...`
                for tok in l.split_whitespace() {
                    let name = tok.trim_end_matches(':');
                    if !name.is_empty()
                        && !name.chars().all(|c| c.is_ascii_digit())
                        && !name.contains('.')
                    {
                        r.iface = name.to_string();
                        break;
                    }
                }
                break;
            }
        }
    }
    if r.iface.is_empty() {
        r.iface = iface_lines
            .iter()
            .filter_map(|l| {
                l.split_whitespace()
                    .nth(1)
                    .map(|s| s.trim_end_matches(':').to_string())
            })
            .find(|n| n != "lo" && !n.is_empty())
            .unwrap_or_default();
    }

    // /proc/net/dev: iface: rx_bytes packets errs drop ... tx_bytes packets errs drop
    for l in &dev_lines {
        if let Some((name, rest)) = l.split_once(':') {
            if name.trim() == r.iface {
                let n: Vec<i64> = rest
                    .split_whitespace()
                    .filter_map(|x| x.parse().ok())
                    .collect();
                if n.len() >= 12 {
                    r.rx_err = n[2];
                    r.rx_drop = n[3];
                    r.tx_err = n[10];
                    r.tx_drop = n[11];
                }
            }
        }
    }

    if !r.iface.is_empty() {
        let q = shell_quote(&r.iface);
        let more = format!(
            "cat /sys/class/net/{i}/mtu 2>/dev/null; echo ---; cat /sys/class/net/{i}/carrier 2>/dev/null; \
             echo ---; cat /sys/class/net/{i}/speed 2>/dev/null",
            i = q
        );
        if let Some(o) = rexec(d, &more) {
            let parts: Vec<&str> = o.split("---").collect();
            r.iface_mtu = parts
                .first()
                .and_then(|s| s.trim().parse().ok())
                .unwrap_or(-1);
            r.carrier = match parts.get(1).map(|s| s.trim()) {
                Some("1") => "up".into(),
                Some("0") => "down(网线/USB没插好?)".into(),
                _ => "?".into(),
            };
            r.speed_mbps = parts
                .get(2)
                .and_then(|s| s.trim().parse().ok())
                .unwrap_or(-1);
        }
    }

    // 网关通不通：板上有 ping 就 ping 一下，没有就跳过
    if !r.gateway.is_empty() {
        if let Some(o) = rexec(
            d,
            &format!(
                "ping -c1 -W2 {} >/dev/null 2>&1 && echo GWOK || echo GWNO",
                shell_quote(&r.gateway)
            ),
        ) {
            if o.contains("GWOK") {
                r.gw_reachable = Some(true);
            } else if o.contains("GWNO") {
                r.gw_reachable = Some(false);
            }
        }
    }

    // DNS 能不能解析（试几种工具，板子上有啥算啥）
    if let Some(o) = rexec(
        d,
        "for h in getent nslookup ping; do :; done; \
         (getent hosts example.com || nslookup example.com || ping -c1 -W2 example.com) >/dev/null 2>&1 \
         && echo DNSOK || echo DNSNO",
    ) {
        if o.contains("DNSOK") {
            r.dns_ok = Some(true);
        } else if o.contains("DNSNO") {
            r.dns_ok = Some(false);
        }
    }

    // 真出得去吗（含代理场景：wget/curl 会自己读 http_proxy）
    if let Some(o) = rexec(
        d,
        "(wget -q -T5 -O- http://example.com >/dev/null 2>&1 || curl -s -m5 -o /dev/null http://example.com) \
         && echo NETOK || echo NETNO",
    ) {
        if o.contains("NETOK") {
            r.inet_ok = Some(true);
        } else if o.contains("NETNO") {
            r.inet_ok = Some(false);
        }
    }
}

fn host_mtu_for(host: &str) -> i64 {
    if let Some((iface, _)) = crate::usbnet::route_iface_for(host) {
        #[cfg(not(target_os = "macos"))]
        {
            if let Ok(s) = std::fs::read_to_string(format!("/sys/class/net/{}/mtu", iface)) {
                return s.trim().parse().unwrap_or(-1);
            }
        }
        #[cfg(target_os = "macos")]
        {
            if let Ok(o) = run_capture(&argv(&["ifconfig", &iface]), &[]) {
                for tok in o.stdout.split_whitespace().collect::<Vec<_>>().windows(2) {
                    if tok[0] == "mtu" {
                        return tok[1].parse().unwrap_or(-1);
                    }
                }
            }
        }
        let _ = iface;
    }
    -1
}

// ---------------- 主入口 ----------------

pub fn diagnose(d: &Device, count: u32, do_speed: bool) -> NetReport {
    let mut r = NetReport {
        device: d.name.clone(),
        transport: d.transport.as_str().to_string(),
        endpoint: d.endpoint(),
        ..Default::default()
    };
    if d.transport == Transport::Serial {
        r.notes
            .push("串口设备没有网络可测；先 `fy up` 爬升到 ssh".into());
        return r;
    }
    info("① 探测可达性 ...");
    r.latency = match d.transport {
        Transport::Ssh => tcp_latency(&d.host, d.port, count, 1500),
        Transport::Adb => adb_latency(d, count.min(5)),
        Transport::Serial => Latency::default(),
    };
    if r.latency.recv == 0 {
        r.notes
            .push("完全不通：先确认板子上电、线插好、IP 没变（fy scan 能帮你重新认领）".into());
        return r;
    }

    info("② 板端网络状态 ...");
    board_probe(d, &mut r);
    if d.transport == Transport::Ssh {
        r.host_mtu = host_mtu_for(&d.host);
    }

    if do_speed {
        info("③ 实测带宽（各 3 秒左右）...");
        match speed_up(d) {
            Ok(v) => r.up_bps = v,
            Err(e) => r.notes.push(format!("上行测速跳过: {}", e)),
        }
        match speed_down(d) {
            Ok(v) => r.down_bps = v,
            Err(e) => r.notes.push(format!("下行测速跳过: {}", e)),
        }
    }

    // 结论性提示：把散落的数字翻译成"下一步做什么"
    if r.latency.loss_pct() > 0.0 {
        r.notes.push(format!(
            "有 {:.0}% 的连接没建起来——链路不稳或板子 CPU 打满，看看 fy top",
            r.latency.loss_pct()
        ));
    }
    if r.iface_mtu > 0 && r.host_mtu > 0 && r.iface_mtu != r.host_mtu {
        r.notes.push(format!(
            "MTU 两头不一致（板 {} / 主机 {}）：大包会被丢，scp 卡住多半是这个",
            r.iface_mtu, r.host_mtu
        ));
    }
    if r.rx_drop + r.tx_drop > 0 || r.rx_err + r.tx_err > 0 {
        r.notes.push(format!(
            "网口有丢包/错包（rx {}/{}，tx {}/{}）：查线、查供电、查 USB 网卡驱动",
            r.rx_err, r.rx_drop, r.tx_err, r.tx_drop
        ));
    }
    if r.gateway.is_empty() {
        r.notes
            .push("板子没有默认路由：出不了子网。`fy share <设备>` 可以直接借主机的网".into());
    } else if r.gw_reachable == Some(false) {
        r.notes
            .push("默认网关不通：路由配了但网关不在或被隔离".into());
    }
    if r.dns_servers.is_empty() {
        r.notes
            .push("/etc/resolv.conf 里没有 nameserver：域名一律解析失败".into());
    } else if r.dns_ok == Some(false) {
        r.notes
            .push("DNS 解析不动：`fy share <设备>` 的代理模式让主机替它解析，最省事".into());
    }
    if r.inet_ok == Some(false) && r.dns_ok == Some(true) {
        r.notes
            .push("能解析但出不去：多半被上游防火墙挡了，试 `fy share <设备>`".into());
    }
    if !r.proxy_env.is_empty() && r.proxy_env != "||" {
        r.notes.push(format!("板端已设代理: {}", r.proxy_env));
    }
    r
}

pub fn print_report(r: &NetReport) {
    println!(
        "{} {}  {}",
        bold("网络体检"),
        cyan(&r.device),
        dim(&format!("[{}] {}", r.transport, r.endpoint))
    );
    println!();
    let mut rows: Vec<Vec<String>> = vec![];
    let l = &r.latency;
    rows.push(vec![
        "可达性".into(),
        if l.recv == 0 {
            red("不通")
        } else if l.loss_pct() > 0.0 {
            yellow(&format!("{}/{} 通", l.recv, l.sent))
        } else {
            green("通")
        },
        format!(
            "延迟 {:.1}/{:.1}/{:.1} ms (min/avg/max) 抖动 {:.1} ms 丢失 {:.0}%",
            l.min_ms,
            l.avg_ms,
            l.max_ms,
            l.jitter_ms,
            l.loss_pct()
        ),
    ]);
    if !r.iface.is_empty() {
        rows.push(vec![
            "网口".into(),
            r.iface.clone(),
            format!(
                "MTU {}{}  载波 {}{}",
                if r.iface_mtu > 0 {
                    r.iface_mtu.to_string()
                } else {
                    "?".into()
                },
                if r.host_mtu > 0 {
                    format!("(主机 {})", r.host_mtu)
                } else {
                    String::new()
                },
                r.carrier,
                if r.speed_mbps > 0 {
                    format!("  {} Mb/s", r.speed_mbps)
                } else {
                    String::new()
                }
            ),
        ]);
        rows.push(vec![
            "收发错误".into(),
            if r.rx_err + r.tx_err + r.rx_drop + r.tx_drop == 0 {
                green("干净")
            } else {
                yellow("有")
            },
            format!(
                "rx err {} drop {} · tx err {} drop {}",
                r.rx_err, r.rx_drop, r.tx_err, r.tx_drop
            ),
        ]);
    }
    rows.push(vec![
        "路由".into(),
        if r.gateway.is_empty() {
            red("无默认路由")
        } else {
            r.gateway.clone()
        },
        match r.gw_reachable {
            Some(true) => green("网关可达"),
            Some(false) => red("网关不通"),
            None => dim("未测"),
        },
    ]);
    rows.push(vec![
        "DNS".into(),
        if r.dns_servers.is_empty() {
            red("未配置")
        } else {
            r.dns_servers.join(", ")
        },
        match r.dns_ok {
            Some(true) => green("解析正常"),
            Some(false) => red("解析失败"),
            None => dim("未测"),
        },
    ]);
    rows.push(vec![
        "出网".into(),
        match r.inet_ok {
            Some(true) => green("通"),
            Some(false) => red("不通"),
            None => dim("未测"),
        },
        if r.proxy_env.is_empty() || r.proxy_env == "||" {
            String::new()
        } else {
            format!("代理: {}", r.proxy_env)
        },
    ]);
    if r.up_bps > 0.0 || r.down_bps > 0.0 {
        rows.push(vec![
            "带宽".into(),
            format!("↑ {}", human_rate(r.up_bps)),
            format!("↓ {}", human_rate(r.down_bps)),
        ]);
    }
    print_table(&["项目", "结果", "细节"], &rows);
    if !r.notes.is_empty() {
        println!();
        println!("{}", bold("值得注意:"));
        for n in &r.notes {
            println!("  {} {}", yellow("·"), n);
        }
    }
}

pub fn report_json(r: &NetReport) -> Vec<(&'static str, J)> {
    let l = &r.latency;
    vec![
        ("device", J::s(&r.device)),
        ("transport", J::s(&r.transport)),
        ("endpoint", J::s(&r.endpoint)),
        ("reachable", J::b(l.recv > 0)),
        (
            "latency_ms",
            J::obj(vec![
                ("min", J::f(l.min_ms)),
                ("avg", J::f(l.avg_ms)),
                ("max", J::f(l.max_ms)),
                ("jitter", J::f(l.jitter_ms)),
            ]),
        ),
        (
            "probe",
            J::obj(vec![
                ("sent", J::i(l.sent as i64)),
                ("recv", J::i(l.recv as i64)),
                ("loss_pct", J::f(l.loss_pct())),
            ]),
        ),
        (
            "bandwidth_bps",
            J::obj(vec![("up", J::f(r.up_bps)), ("down", J::f(r.down_bps))]),
        ),
        (
            "iface",
            J::obj(vec![
                ("name", J::s(&r.iface)),
                ("mtu", J::i(r.iface_mtu)),
                ("host_mtu", J::i(r.host_mtu)),
                ("carrier", J::s(&r.carrier)),
                ("speed_mbps", J::i(r.speed_mbps)),
                ("rx_err", J::i(r.rx_err)),
                ("rx_drop", J::i(r.rx_drop)),
                ("tx_err", J::i(r.tx_err)),
                ("tx_drop", J::i(r.tx_drop)),
            ]),
        ),
        (
            "route",
            J::obj(vec![
                ("gateway", J::s(&r.gateway)),
                (
                    "gateway_reachable",
                    r.gw_reachable.map(J::b).unwrap_or(J::Null),
                ),
            ]),
        ),
        (
            "dns",
            J::obj(vec![
                ("servers", J::strs(&r.dns_servers)),
                ("resolves", r.dns_ok.map(J::b).unwrap_or(J::Null)),
            ]),
        ),
        ("internet", r.inet_ok.map(J::b).unwrap_or(J::Null)),
        ("proxy_env", J::s(&r.proxy_env)),
        ("notes", J::strs(&r.notes)),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loss_math() {
        let l = Latency {
            sent: 10,
            recv: 7,
            ..Default::default()
        };
        assert!((l.loss_pct() - 30.0).abs() < 1e-9);
        let z = Latency::default();
        assert_eq!(z.loss_pct(), 0.0);
    }

    #[test]
    fn unreachable_host_reports_full_loss_fast() {
        // 192.0.2.0/24 是 RFC5737 保留的文档网段，不会有人应答
        let t = Instant::now();
        let l = tcp_latency("192.0.2.1", 22, 2, 150);
        assert_eq!(l.recv, 0);
        assert_eq!(l.sent, 2);
        assert!((l.loss_pct() - 100.0).abs() < 1e-9);
        assert!(t.elapsed().as_secs() < 5, "超时设置没生效");
    }

    #[test]
    fn latency_against_a_real_listener() {
        let ln = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = ln.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for c in ln.incoming().take(4) {
                drop(c);
            }
        });
        let l = tcp_latency("127.0.0.1", port, 3, 500);
        assert_eq!(l.recv, 3, "本机 loopback 应该全通");
        assert!(l.avg_ms >= 0.0 && l.avg_ms < 500.0);
        assert!(l.min_ms <= l.avg_ms && l.avg_ms <= l.max_ms);
    }

    #[test]
    fn json_shape_has_the_keys_agents_read() {
        let r = NetReport {
            device: "rk".into(),
            ..Default::default()
        };
        let fields = report_json(&r);
        let keys: Vec<&str> = fields.iter().map(|(k, _)| *k).collect();
        for want in [
            "device",
            "reachable",
            "latency_ms",
            "bandwidth_bps",
            "dns",
            "notes",
        ] {
            assert!(keys.contains(&want), "缺字段 {}", want);
        }
    }
}
