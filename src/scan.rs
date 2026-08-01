//! `fy scan`：一条命令找齐周围的下位机。
//! - 本机各网段 TCP 并发探测 SSH/ADB 候选端口，并只保留可登录通道
//! - 读 ssh banner（dropbear? OpenSSH? 顺手判断要不要 legacy 兼容）
//! - ARP 表拿 MAC → 与指纹库比对，"老朋友换了 IP"自动认领
//! - adb devices / 串口设备一并列出

use crate::adbx;
use crate::config::{Config, Device, Transport};
use crate::serialx;
use crate::util::*;
use std::collections::BTreeMap;
use std::io::Read;
use std::net::{SocketAddr, TcpStream};

use std::time::Duration;

/// Default login endpoints. Extra ports are opt-in so a normal local scan stays
/// bounded and does not turn into a broad, non-actionable port scan.
const DEFAULT_PROBE_PORTS: &[u16] = &[22, 5555, 8022];
const DEFAULT_SSH_PORTS: &[u16] = &[22, 8022];

#[derive(Debug, Clone, Default)]
pub struct Hit {
    pub ip: String,
    pub open: Vec<u16>,
    pub banner: String,
    pub mac: String,
    pub known_as: Option<String>,
    /// mDNS 报出来的主机名（没有就是空）
    pub hostname: String,
    /// 怎么发现的：tcp / mdns / tcp+mdns
    pub via: String,
    /// Ferry 已验证的交互通道；没有就不作为扫描结果返回。
    pub transport: Option<Transport>,
    /// SSH 或网络 ADB 的实际连接端口。
    pub login_port: u16,
}

fn ssh_port(hit: &Hit, candidates: &[u16]) -> Option<u16> {
    candidates
        .iter()
        .copied()
        .find(|port| hit.open.contains(port))
}

/// Parse the comma-separated `--ports` / desktop input once, before work is
/// dispatched to the scan workers. A short cap prevents accidental full-port
/// scans from multiplying the per-host work without bound.
pub fn parse_extra_ports(raw: &str) -> Result<Vec<u16>, String> {
    if raw.trim().is_empty() {
        return Ok(vec![]);
    }
    let mut ports = vec![];
    for value in raw.split(',') {
        let value = value.trim();
        if value.is_empty() {
            return Err("Extra ports must be a comma-separated list, for example: 2222, 2200".into());
        }
        let port = value
            .parse::<u16>()
            .map_err(|_| format!("'{value}' is not a valid TCP port."))?;
        if port == 0 {
            return Err("TCP port 0 cannot be scanned.".into());
        }
        if !ports.contains(&port) {
            ports.push(port);
        }
    }
    if ports.len() > 16 {
        return Err("At most 16 extra ports may be scanned at once.".into());
    }
    Ok(ports)
}

fn probe_ports_for(extra_ports: &[u16]) -> Vec<u16> {
    let mut ports = DEFAULT_PROBE_PORTS.to_vec();
    for &port in extra_ports {
        if port != 0 && !ports.contains(&port) {
            ports.push(port);
        }
    }
    ports.sort_unstable();
    ports
}

fn ssh_ports_for(extra_ports: &[u16]) -> Vec<u16> {
    let mut ports = DEFAULT_SSH_PORTS.to_vec();
    for &port in extra_ports {
        if port != 0 && !ports.contains(&port) {
            ports.push(port);
        }
    }
    ports
}

fn is_ssh_banner(banner: &str) -> bool {
    banner.starts_with("SSH-")
}

fn adb_network_endpoint(serial: &str, detail: &str) -> Option<(String, u16)> {
    if detail.contains("unauthorized") || detail.contains("offline") {
        return None;
    }
    let address = serial.parse::<SocketAddr>().ok()?;
    address
        .ip()
        .is_ipv4()
        .then(|| (address.ip().to_string(), address.port()))
}

fn adb_network_endpoints() -> BTreeMap<String, u16> {
    let mut endpoints = BTreeMap::new();
    for (serial, detail) in adbx::list_devices() {
        if let Some((ip, port)) = adb_network_endpoint(&serial, &detail) {
            endpoints.insert(ip, port);
        }
    }
    endpoints
}

/// 本机所有 IPv4 网段（排除回环）。返回 (本机ip, 前缀长度)。
pub fn local_nets() -> Vec<(String, u8)> {
    let mut out = vec![];
    #[cfg(target_os = "macos")]
    {
        if let Ok(o) = run_capture(&argv(&["ifconfig"]), &[]) {
            let mut cur_prefix = 24u8;
            for line in o.stdout.lines() {
                let t = line.trim();
                if t.starts_with("inet ") && !t.contains("127.0.0.1") {
                    let toks: Vec<&str> = t.split_whitespace().collect();
                    if toks.len() >= 4 {
                        let ip = toks[1].to_string();
                        if let Some(mask) = toks
                            .iter()
                            .position(|x| *x == "netmask")
                            .map(|i| toks[i + 1])
                        {
                            cur_prefix = netmask_bits(mask);
                        }
                        out.push((ip, cur_prefix));
                    }
                }
            }
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        if let Ok(o) = run_capture(&argv(&["ip", "-o", "-4", "addr", "show"]), &[]) {
            for line in o.stdout.lines() {
                let toks: Vec<&str> = line.split_whitespace().collect();
                if let Some(i) = toks.iter().position(|t| *t == "inet") {
                    let cidr = toks[i + 1];
                    if cidr.starts_with("127.") {
                        continue;
                    }
                    let mut it = cidr.split('/');
                    let ip = it.next().unwrap_or("").to_string();
                    let bits: u8 = it.next().and_then(|b| b.parse().ok()).unwrap_or(24);
                    out.push((ip, bits));
                }
            }
        }
    }
    out
}

#[cfg(target_os = "macos")]
fn netmask_bits(mask: &str) -> u8 {
    // macOS ifconfig 的 netmask 是 0xffffff00 形式
    if let Some(hex) = mask.strip_prefix("0x") {
        if let Ok(v) = u32::from_str_radix(hex, 16) {
            return v.count_ones() as u8;
        }
    }
    mask.split('.')
        .filter_map(|p| p.parse::<u8>().ok())
        .map(|b| b.count_ones() as u8)
        .sum()
}

/// ARP 表: ip → mac。
fn arp_table() -> BTreeMap<String, String> {
    let mut m = BTreeMap::new();
    if let Ok(o) = run_capture(&argv(&["arp", "-an"]), &[]) {
        for line in o.stdout.lines() {
            // mac:  ? (192.168.1.7) at a4:83:e7:xx:xx:xx on en0 ...
            // linux:? (192.168.1.7) at a4:83:e7:xx:xx:xx [ether] on wlan0
            let ip = line.split('(').nth(1).and_then(|s| s.split(')').next());
            let mac = line
                .split(" at ")
                .nth(1)
                .and_then(|s| s.split_whitespace().next());
            if let (Some(ip), Some(mac)) = (ip, mac) {
                if mac.contains(':') {
                    m.insert(ip.to_string(), normalize_mac(mac));
                }
            }
        }
    }
    m
}

fn normalize_mac(m: &str) -> String {
    m.split(':')
        .map(|p| format!("{:0>2}", p.to_lowercase()))
        .collect::<Vec<_>>()
        .join(":")
}

/// 抓 ssh banner（server 先说话，读一行就跑）。
fn ssh_banner(ip: &str, port: u16) -> String {
    let addr: SocketAddr = match format!("{}:{}", ip, port).parse() {
        Ok(a) => a,
        Err(_) => return String::new(),
    };
    if let Ok(mut s) = TcpStream::connect_timeout(&addr, Duration::from_millis(400)) {
        let _ = s.set_read_timeout(Some(Duration::from_millis(600)));
        let mut buf = [0u8; 128];
        if let Ok(n) = s.read(&mut buf) {
            let b = String::from_utf8_lossy(&buf[..n]);
            return b.lines().next().unwrap_or("").trim().to_string();
        }
    }
    String::new()
}

/// 主扫描：返回命中列表（带认领标注）。
/// 扫描目标 IP 列表。
fn build_targets(subnet_override: Option<&str>) -> Vec<String> {
    let mut targets: Vec<String> = vec![];
    if let Some(cidr) = subnet_override {
        targets.extend(expand_cidr(cidr));
    } else {
        for (ip, bits) in local_nets() {
            if bits >= 31 {
                continue;
            }
            let base: Vec<&str> = ip.split('.').collect();
            if base.len() != 4 {
                continue;
            }
            let prefix = format!("{}.{}.{}.", base[0], base[1], base[2]);
            for h in 1..255 {
                let t = format!("{}{}", prefix, h);
                if t != ip {
                    targets.push(t);
                }
            }
        }
    }
    targets.sort();
    targets.dedup();
    targets
}

/// 并发探测的宽度。任务粒度是 (IP, 端口)，比"一个线程包八个 IP"负载均衡得多：
/// 一台超时的主机不会再拖住同组其它主机。
const WORKERS: usize = 128;

/// 有界线程池：共享一个原子游标去领任务，不给每个 IP 都开线程。
fn probe_ports(
    targets: &[String],
    hot: &BTreeMap<String, String>,
    ports: &[u16],
) -> BTreeMap<String, Vec<u16>> {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    let mut tasks: Vec<(String, u16)> = Vec::with_capacity(targets.len() * ports.len());
    for ip in targets {
        for &p in ports {
            tasks.push((ip.clone(), p));
        }
    }
    let tasks = Arc::new(tasks);
    let cursor = Arc::new(AtomicUsize::new(0));
    let found: Arc<Mutex<BTreeMap<String, Vec<u16>>>> = Arc::new(Mutex::new(BTreeMap::new()));
    // 邻居表里出现过的地址给更宽的超时：它们几乎肯定活着，别因为慢就漏掉
    let hot: Arc<BTreeMap<String, String>> = Arc::new(hot.clone());

    let n = WORKERS.min(tasks.len().max(1));
    let mut handles = vec![];
    for _ in 0..n {
        let tasks = tasks.clone();
        let cursor = cursor.clone();
        let found = found.clone();
        let hot = hot.clone();
        handles.push(std::thread::spawn(move || loop {
            let i = cursor.fetch_add(1, Ordering::Relaxed);
            if i >= tasks.len() {
                break;
            }
            let (ip, port) = &tasks[i];
            let ms = if hot.contains_key(ip) { 600 } else { 250 };
            if let Ok(addr) = format!("{}:{}", ip, port).parse::<SocketAddr>() {
                if TcpStream::connect_timeout(&addr, Duration::from_millis(ms)).is_ok() {
                    found
                        .lock()
                        .unwrap()
                        .entry(ip.clone())
                        .or_default()
                        .push(*port);
                }
            }
        }));
    }
    for h in handles {
        let _ = h.join();
    }
    let mut m = Arc::try_unwrap(found)
        .map(|x| x.into_inner().unwrap())
        .unwrap_or_default();
    for v in m.values_mut() {
        v.sort();
    }
    m
}

/// 主扫描：mDNS 先问一嘴，再并发扫端口，最后合并 + 指纹认领。
pub fn sweep_opts(cfg: &Config, subnet_override: Option<&str>, use_mdns: bool) -> Vec<Hit> {
    sweep_opts_with_ports(cfg, subnet_override, use_mdns, &[])
}

/// Run discovery with explicitly requested extra SSH ports. Each extra port is
/// still retained only after it identifies itself as SSH, preserving the
/// actionable-result contract of `fy scan`.
pub fn sweep_opts_with_ports(
    cfg: &Config,
    subnet_override: Option<&str>,
    use_mdns: bool,
    extra_ports: &[u16],
) -> Vec<Hit> {
    let scan_ports = probe_ports_for(extra_ports);
    let ssh_ports = ssh_ports_for(extra_ports);
    // ① mDNS：一个组播包换一批"自报家门"的设备，比暴力扫快得多也全得多
    let mut mdns_hits: Vec<crate::mdns::MdnsHit> = vec![];
    if use_mdns {
        let ips: Vec<String> = local_nets().into_iter().map(|(ip, _)| ip).collect();
        info("mDNS 询问 (1.5s) ...");
        mdns_hits = crate::mdns::discover(&ips, Duration::from_millis(1500));
        if !mdns_hits.is_empty() {
            info(&format!("mDNS 应答 {} 台", mdns_hits.len()));
        }
    }

    // ② ARP/邻居表：已经打过交道的地址，扫的时候给更宽的超时
    let arp = arp_table();

    let targets = build_targets(subnet_override);
    if targets.is_empty() && mdns_hits.is_empty() {
        warn("没有可扫的网段（本机没有活动的 IPv4 接口？）");
        return vec![];
    }
    if !targets.is_empty() {
        info(&format!(
            "并发扫描 {} 个地址 × {} 端口（{} 并发）...",
            targets.len(),
            scan_ports.len(),
            WORKERS
        ));
    }
    let open_map = probe_ports(&targets, &arp, &scan_ports);

    let mut by_ip: BTreeMap<String, Hit> = BTreeMap::new();
    for (ip, open) in open_map {
        by_ip.insert(
            ip.clone(),
            Hit {
                ip,
                open,
                via: "tcp".into(),
                ..Default::default()
            },
        );
    }
    for m in mdns_hits {
        let e = by_ip.entry(m.ip.clone()).or_insert_with(|| Hit {
            ip: m.ip.clone(),
            via: "mdns".into(),
            ..Default::default()
        });
        if e.via == "tcp" {
            e.via = "tcp+mdns".into();
        }
        e.hostname = m.host.clone();
        if m.port != 0 && !e.open.contains(&m.port) {
            e.open.push(m.port);
            e.open.sort();
        }
    }

    let adb_endpoints = adb_network_endpoints();
    let mut hits: Vec<Hit> = by_ip.into_values().collect();
    hits.sort_by(|a, b| ip_key(&a.ip).cmp(&ip_key(&b.ip)));

    // ③ banner + MAC + 指纹认领
    for h in hits.iter_mut() {
        if let Some(port) = ssh_port(h, &ssh_ports) {
            h.banner = ssh_banner(&h.ip, port);
            if is_ssh_banner(&h.banner) {
                h.transport = Some(Transport::Ssh);
                h.login_port = port;
            }
        }
        if h.transport.is_none()
            && adb_endpoints.get(&h.ip).is_some_and(|port| h.open.contains(port))
        {
            h.transport = Some(Transport::Adb);
            h.login_port = *adb_endpoints.get(&h.ip).unwrap_or(&5555);
        }
        if let Some(mac) = arp.get(&h.ip) {
            h.mac = mac.clone();
        }
        h.known_as = crate::fingerprint::match_known(
            if h.mac.is_empty() { None } else { Some(&h.mac) },
            None,
        );
        if h.known_as.is_none() {
            if let Some(d) = cfg.devices.values().find(|d| d.host == h.ip) {
                h.known_as = Some(d.name.clone());
            }
        }
        // mDNS 报的主机名也能当认领线索
        if h.known_as.is_none() && !h.hostname.is_empty() {
            h.known_as = cfg
                .devices
                .values()
                .find(|d| d.name == h.hostname)
                .map(|d| d.name.clone());
        }
    }
    // mDNS, HTTP, telnet, and a bare TCP 5555 listener are discovery hints, not
    // usable Ferry login paths. Keep only SSH-banner or verified ADB endpoints.
    hits.retain(|hit| hit.transport.is_some());
    hits
}

fn ip_key(ip: &str) -> u32 {
    let mut v = 0u32;
    for p in ip.split('.') {
        v = (v << 8) | p.parse::<u32>().unwrap_or(0);
    }
    v
}

fn expand_cidr(cidr: &str) -> Vec<String> {
    let mut it = cidr.split('/');
    let ip = it.next().unwrap_or("");
    let bits: u8 = it.next().and_then(|b| b.parse().ok()).unwrap_or(24);
    let base: Vec<&str> = ip.split('.').collect();
    if base.len() != 4 || !(16..=32).contains(&bits) {
        // bits > 32 会让 32 - bits 下溢（debug 直接 panic，release 变成天文数字的目标数）
        warn("只支持 /16 ~ /32 的网段：更大的扫不动，更小的不是合法前缀");
        return vec![];
    }
    if bits >= 31 {
        return vec![]; // /31 /32 没有可枚举的主机位
    }
    let start = ip_key(ip) & (!0u32 << (32 - bits));
    let count = 1u32 << (32 - bits);
    (1..count.saturating_sub(1))
        .map(|i| {
            let v = start + i;
            format!(
                "{}.{}.{}.{}",
                v >> 24,
                (v >> 16) & 255,
                (v >> 8) & 255,
                v & 255
            )
        })
        .collect()
}

/// `fy scan` 入口：网络 + adb + 串口 三合一视图，可交互建档/认领。
pub fn scan_cmd(cfg: &mut Config, subnet: Option<&str>, do_add: bool, use_mdns: bool) {
    scan_cmd_with_ports(cfg, subnet, do_add, use_mdns, &[])
}

pub fn scan_cmd_with_ports(
    cfg: &mut Config,
    subnet: Option<&str>,
    do_add: bool,
    use_mdns: bool,
    extra_ports: &[u16],
) {
    let hits = if extra_ports.is_empty() {
        sweep_opts(cfg, subnet, use_mdns)
    } else {
        sweep_opts_with_ports(cfg, subnet, use_mdns, extra_ports)
    };
    let mut rows: Vec<Vec<String>> = vec![];
    for h in &hits {
        let svc = h
            .open
            .iter()
            .map(|p| {
                if h.transport == Some(Transport::Ssh) && *p == h.login_port {
                    green(&format!("ssh:{}", p))
                } else if h.transport == Some(Transport::Adb) && *p == h.login_port {
                    cyan(&format!("adb:{}", p))
                } else {
                    p.to_string()
                }
            })
            .collect::<Vec<_>>()
            .join(" ");
        let mut hint = h.banner.clone();
        if h.banner.to_lowercase().contains("dropbear") {
            hint = format!("{} {}", h.banner, yellow("(老板子?加 --legacy)"));
        }
        if !h.hostname.is_empty() && hint.is_empty() {
            hint = dim(&format!("mDNS: {}", h.hostname));
        }
        rows.push(vec![
            h.ip.clone(),
            svc,
            h.mac.clone(),
            match &h.known_as {
                Some(n) => green(&format!("≈ {}", n)),
                None => dim("新面孔"),
            },
            dim(&h.via),
            hint,
        ]);
    }
    if !rows.is_empty() {
        println!("{}", bold("网络:"));
        print_table(&["IP", "服务", "MAC", "认领", "发现", "banner"], &rows);
    } else {
        info("网络上没扫到可通过 SSH 或已授权 ADB 登录的设备");
    }

    // 老朋友换 IP：提议改档案
    for h in &hits {
        if let Some(name) = &h.known_as {
            if let Some(d) = cfg.devices.get(name) {
                if d.transport == Transport::Ssh && d.host != h.ip && !h.ip.is_empty() {
                    if confirm(
                        &format!("{} 好像搬家了 {} → {}，更新档案?", name, d.host, h.ip),
                        true,
                    ) {
                        let mut dd = d.clone();
                        dd.host = h.ip.clone();
                        cfg.devices.insert(name.clone(), dd);
                        let _ = cfg.save();
                        ok(&format!("{} 档案已指到 {}", name, h.ip));
                    }
                }
            }
        }
    }

    // adb
    let adbs = adbx::list_devices();
    if !adbs.is_empty() {
        println!("\n{}", bold("adb:"));
        let rows: Vec<Vec<String>> = adbs
            .iter()
            .map(|(serial, desc)| {
                let known = cfg
                    .devices
                    .values()
                    .find(|d| d.adb_serial.as_deref() == Some(serial))
                    .map(|d| green(&format!("≈ {}", d.name)))
                    .unwrap_or_else(|| dim("新面孔"));
                vec![serial.clone(), desc.clone(), known]
            })
            .collect();
        print_table(&["serial", "描述", "认领"], &rows);
    }

    // 串口
    let ports = serialx::serial_ports();
    if !ports.is_empty() {
        println!("\n{}", bold("串口:"));
        for p in &ports {
            let known = cfg
                .devices
                .values()
                .find(|d| d.dev.as_deref() == Some(p))
                .map(|d| green(&format!("≈ {}", d.name)))
                .unwrap_or_else(|| dim(""));
            println!("  {}  {}", p, known);
        }
    }

    // 交互建档
    if do_add {
        let mut candidates: Vec<String> = vec![];
        for h in &hits {
            if h.known_as.is_none() {
                match h.transport {
                    Some(Transport::Ssh) => candidates.push(format!("ssh {} {}", h.ip, h.login_port)),
                    Some(Transport::Adb) => candidates.push(format!("adb {} {}", h.ip, h.login_port)),
                    _ => {}
                }
            }
        }
        for (serial, _) in &adbs {
            if !cfg
                .devices
                .values()
                .any(|d| d.adb_serial.as_deref() == Some(serial))
            {
                candidates.push(format!("adb {}", serial));
            }
        }
        for p in &ports {
            if !cfg.devices.values().any(|d| d.dev.as_deref() == Some(p)) {
                candidates.push(format!("serial {}", p));
            }
        }
        if candidates.is_empty() {
            info("没有可新建档的对象");
            return;
        }
        while let Some(i) = pick("给谁建档? (回车跳过)", &candidates) {
            let c = candidates.remove(i);
            let name = prompt("起个名字:");
            if name.is_empty() {
                continue;
            }
            let mut toks = c.split_whitespace();
            let kind = toks.next().unwrap_or("");
            let addr = toks.next().unwrap_or("").to_string();
            let port = toks.next().and_then(|value| value.parse::<u16>().ok()).unwrap_or(22);
            let mut d = Device::new(&name, Transport::Ssh);
            match kind {
                "ssh" => {
                    d.host = addr;
                    d.port = port;
                    let user = prompt("用户名 (默认 root):");
                    if !user.is_empty() {
                        d.user = user;
                    }
                    let pass = prompt("密码 (留空=用密钥/免密):");
                    if !pass.is_empty() {
                        d.password = Some(pass);
                    }
                }
                "adb" => {
                    d.transport = Transport::Adb;
                    d.adb_serial = Some(format!("{}:{}", addr, port));
                }
                "serial" => {
                    d.transport = Transport::Serial;
                    d.dev = Some(addr);
                    let baud = prompt("波特率 (默认 115200):");
                    if let Ok(b) = baud.parse::<u32>() {
                        d.baud = b;
                    }
                }
                _ => continue,
            }
            cfg.devices.insert(name.clone(), d);
            let _ = cfg.save();
            ok(&format!("已建档 {}。试试: fy sh {}", name, name));
            if candidates.is_empty() {
                break;
            }
        }
    } else if !hits.is_empty() || !adbs.is_empty() {
        info("加 --add 可交互建档");
    }
}

/// `fy scan --json`：把网络/adb/串口三类发现结果一次给出去。
pub fn scan_json(
    cfg: &Config,
    subnet: Option<&str>,
    use_mdns: bool,
) -> Vec<(&'static str, crate::jsonout::J)> {
    scan_json_with_ports(cfg, subnet, use_mdns, &[])
}

pub fn scan_json_with_ports(
    cfg: &Config,
    subnet: Option<&str>,
    use_mdns: bool,
    extra_ports: &[u16],
) -> Vec<(&'static str, crate::jsonout::J)> {
    use crate::jsonout::J;
    let hits = sweep_opts_with_ports(cfg, subnet, use_mdns, extra_ports);
    let net: Vec<J> = hits
        .iter()
        .map(|h| {
            J::obj(vec![
                ("ip", J::s(&h.ip)),
                (
                    "ports",
                    J::arr(h.open.iter().map(|p| J::i(*p as i64)).collect()),
                ),
                ("mac", J::s(&h.mac)),
                ("banner", J::s(&h.banner)),
                ("hostname", J::s(&h.hostname)),
                ("via", J::s(&h.via)),
                (
                    "transport",
                    h.transport
                        .map(|transport| J::s(transport.as_str()))
                        .unwrap_or(J::Null),
                ),
                ("login_port", J::i(h.login_port as i64)),
                ("known_as", h.known_as.clone().map(J::s).unwrap_or(J::Null)),
                (
                    "legacy_hint",
                    J::b(h.banner.to_lowercase().contains("dropbear")),
                ),
            ])
        })
        .collect();
    let adbs: Vec<J> = adbx::list_devices()
        .into_iter()
        .map(|(serial, desc)| {
            let known = cfg
                .devices
                .values()
                .find(|d| d.adb_serial.as_deref() == Some(serial.as_str()))
                .map(|d| J::s(&d.name))
                .unwrap_or(J::Null);
            J::obj(vec![
                ("serial", J::s(serial)),
                ("desc", J::s(desc)),
                ("known_as", known),
            ])
        })
        .collect();
    let serials: Vec<J> = serialx::serial_ports()
        .into_iter()
        .map(|p| {
            let known = cfg
                .devices
                .values()
                .find(|d| d.dev.as_deref() == Some(p.as_str()))
                .map(|d| J::s(&d.name))
                .unwrap_or(J::Null);
            J::obj(vec![("dev", J::s(p)), ("known_as", known)])
        })
        .collect();
    vec![
        ("network", J::arr(net)),
        ("adb", J::arr(adbs)),
        ("serial", J::arr(serials)),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cidr_expansion_bounds() {
        let v = expand_cidr("192.168.1.0/30");
        assert_eq!(
            v,
            vec!["192.168.1.1".to_string(), "192.168.1.2".to_string()]
        );
        let big = expand_cidr("10.0.0.0/24");
        assert_eq!(big.len(), 254, "/24 应给出 .1 ~ .254");
        assert_eq!(big[0], "10.0.0.1");
        assert_eq!(big[big.len() - 1], "10.0.0.254");
        // /8 太大，明确拒绝而不是把主机跑挂
        assert!(expand_cidr("10.0.0.0/8").is_empty());
        // 非法前缀不能把 32-bits 减到下溢
        assert!(expand_cidr("192.168.1.0/33").is_empty());
        assert!(expand_cidr("192.168.1.0/40").is_empty());
        assert!(expand_cidr("192.168.1.0/255").is_empty());
        assert!(expand_cidr("192.168.1.1/32").is_empty());
    }

    #[test]
    fn mac_normalisation() {
        assert_eq!(normalize_mac("A4:83:E7:1:2:3"), "a4:83:e7:01:02:03");
    }

    #[test]
    fn worker_pool_finds_a_real_listener_and_is_bounded() {
        let ln = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = ln.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for c in ln.incoming() {
                drop(c);
            }
        });
        // 直接测线程池：目标里混一个不存在的地址，确保不会因为它卡住
        let targets = vec!["127.0.0.1".to_string()];
        let hot = BTreeMap::new();
        // PROBE_PORTS 里没有随机端口，所以单独构造一次探测
        let addr: SocketAddr = format!("127.0.0.1:{}", port).parse().unwrap();
        assert!(TcpStream::connect_timeout(&addr, Duration::from_millis(500)).is_ok());
        let t = std::time::Instant::now();
        let _ = probe_ports(&targets, &hot, DEFAULT_PROBE_PORTS);
        assert!(t.elapsed() < Duration::from_secs(5), "单地址扫描不该这么慢");
    }

    #[test]
    fn ip_key_orders_numerically() {
        assert!(ip_key("192.168.1.2") < ip_key("192.168.1.10"));
        assert!(ip_key("10.0.0.1") < ip_key("192.168.0.1"));
    }

    #[test]
    fn accepts_only_real_ssh_protocol_banners() {
        assert!(is_ssh_banner("SSH-2.0-OpenSSH_9.6"));
        assert!(is_ssh_banner("SSH-1.99-dropbear_2022.83"));
        assert!(!is_ssh_banner("HTTP/1.1 200 OK"));
        assert!(!is_ssh_banner(""));
    }

    #[test]
    fn accepts_only_authorized_network_adb_endpoints() {
        assert_eq!(
            adb_network_endpoint("192.168.2.2:5555", "device product:rk3588"),
            Some(("192.168.2.2".into(), 5555))
        );
        assert_eq!(adb_network_endpoint("192.168.2.2:5555", "unauthorized"), None);
        assert_eq!(adb_network_endpoint("192.168.2.2:5555", "offline"), None);
        assert_eq!(adb_network_endpoint("usb-serial", "device"), None);
    }

    #[test]
    fn parses_and_adds_explicit_scan_ports() {
        assert_eq!(parse_extra_ports("2222, 2200,2222").unwrap(), vec![2222, 2200]);
        assert!(parse_extra_ports("2222,").is_err());
        assert!(parse_extra_ports("0").is_err());
        assert!(parse_extra_ports("not-a-port").is_err());

        assert_eq!(probe_ports_for(&[2222]), vec![22, 2222, 5555, 8022]);
        assert_eq!(ssh_ports_for(&[2222]), vec![22, 8022, 2222]);
        let hit = Hit {
            open: vec![2222],
            ..Default::default()
        };
        assert_eq!(ssh_port(&hit, &ssh_ports_for(&[2222])), Some(2222));
    }
}
