//! `fy up`：通道爬升 —— 一条命令把板子带到"最好的连接"。
//!
//!   串口 ──自动登录──▶ 探测板况 ──┬─ 板子已有 IP ──▶ 直接认领 ssh
//!                                ├─ 有 UDC ──▶ 串口灌 USB gadget → 主机配网 → ssh
//!                                └─ 有网口 ──▶ 串口跑 DHCP / 配静态 IP → ssh
//!   顺手：串口通道直接把主机公钥装进板子 → ssh 到手即免密。
//!   全程指纹入档；ssh 装不了就明说，串口继续当家。

use crate::config::{self, Config, Device, Transport};
use crate::fingerprint::{self, Exec};
use crate::serialx::{self, Expecter};
use crate::sshx;
use crate::usbnet;
use crate::util::*;
use std::fs::File;
use std::net::TcpStream;
use std::time::Duration;

pub struct SerialSession {
    pub w: File,
    pub exp: Expecter,
    n: u32,
}

impl SerialSession {
    pub fn open(port: &str, baud: u32) -> std::io::Result<SerialSession> {
        let f = serialx::open_port(port, baud)?;
        let r = f.try_clone()?;
        Ok(SerialSession {
            w: f,
            exp: Expecter::new(r),
            n: 0,
        })
    }

    fn send(&mut self, s: &str) {
        let _ = serialx::send_line(&mut self.w, s);
    }

    /// 板端跑命令：`echo 标记"B"; cmd; echo 标记"E"$?` —— 标记拆写避开回显误匹配。
    pub fn run(&mut self, cmd: &str, timeout: Duration) -> (String, i32) {
        self.n += 1;
        let mb = format!("FMK{}B", self.n);
        let me = format!("FMK{}E", self.n);
        let line = format!(
            "echo FMK{n}\"B\"; {c}; echo FMK{n}\"E\"$?",
            n = self.n,
            c = cmd
        );
        let start = self.exp.transcript.len();
        self.send(&line);
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let _ = self.exp.drain(Duration::from_millis(120));
            let t = &self.exp.transcript[start..];
            if let Some(epos) = t.find(&me) {
                let after = &t[epos + me.len()..];
                if let Some(nl) = after.find('\n') {
                    let status: i32 = after[..nl].trim().parse().unwrap_or(-1);
                    let bpos = t.find(&mb).map(|p| p + mb.len()).unwrap_or(0);
                    let body = t[bpos..epos]
                        .trim_start_matches(['\r', '\n'])
                        .rsplit_once('\n')
                        .map(|(a, _)| a)
                        .unwrap_or("")
                        .to_string();
                    return (body.replace('\r', ""), status);
                }
            }
            if std::time::Instant::now() >= deadline {
                return (String::new(), -1);
            }
        }
    }

    /// 自动登录：状态机只匹配"每次发送之后"新到的字节（transcript 标记），
    /// 因此串口世界里常见的回显、双重 login 重打印都不会造成误判/死循环。
    pub fn login(&mut self, user: &str, pass: &str, boot_ok: bool) -> Result<(), String> {
        // 0=password 1=incorrect 2=login: 3=username: 4="# " 5="$ " 6=bootloader
        let pats = [
            "password:",
            "incorrect",
            "login:",
            "username:",
            "# ",
            "$ ",
            "autoboot",
        ];
        let deadline =
            std::time::Instant::now() + Duration::from_secs(if boot_ok { 90 } else { 25 });
        let mut nudges = 0;
        loop {
            if std::time::Instant::now() >= deadline {
                return Err("登录超时：检查波特率/接线，或板子没在跑系统".into());
            }
            // 先把已有输出沉淀掉，再标记、敲一下、只看敲之后的回应
            let _ = self.exp.drain(Duration::from_millis(200));
            let m = self.exp.mark();
            self.send(""); // 回车触发一次新提示
            match self.exp.expect_from(m, &pats, Duration::from_secs(3)) {
                Some(0) => {
                    // password 提示
                    return self.do_password(pass);
                }
                Some(1) => return Err("密码不对（fy add 或 devices.toml 里改 password）".into()),
                Some(2) | Some(3) => {
                    step(&format!("发用户名 {}", user));
                    let mu = self.exp.mark();
                    self.send(user);
                    // 只等"发用户名之后"的 password / shell，忽略之前那条 login:
                    match self.exp.expect_from(
                        mu,
                        &["password:", "# ", "$ ", "incorrect"],
                        Duration::from_secs(6),
                    ) {
                        Some(0) => return self.do_password(pass),
                        Some(1) => return Err("用户名/密码不对".into()),
                        Some(_) => return self.verify_shell(),
                        None => continue, // 没等到，回到顶部重来
                    }
                }
                Some(4) | Some(5) => return self.verify_shell(),
                Some(6) => {
                    if boot_ok {
                        step("检测到 bootloader，发 boot 引导内核（最多等 60s）...");
                        let mb = self.exp.mark();
                        self.send("boot");
                        let _ = self.exp.expect_from(
                            mb,
                            &["login:", "# ", "$ "],
                            Duration::from_secs(60),
                        );
                    } else {
                        return Err("板子停在 bootloader（U-Boot）。确认要引导就加 --boot".into());
                    }
                }
                _ => {
                    // 什么提示都没有：也许已经在一个静默 shell 里
                    nudges += 1;
                    if nudges >= 3 && self.verify_shell().is_ok() {
                        return Ok(());
                    }
                    if nudges > 8 {
                        return self.verify_shell();
                    }
                }
            }
        }
    }

    /// 已在 password 提示：发密码，等 shell / incorrect。
    fn do_password(&mut self, pass: &str) -> Result<(), String> {
        step("发密码");
        let m = self.exp.mark();
        self.send(pass);
        let r = self.exp.expect_from(
            m,
            &["incorrect", "denied", "failure", "login:", "# ", "$ "],
            Duration::from_secs(6),
        );
        match r {
            Some(0) | Some(1) | Some(2) | Some(3) => {
                Err("密码不对（fy add 或 devices.toml 里改 password）".into())
            }
            Some(_) => self.verify_shell(),
            None => self.verify_shell(), // 可能是无回显的静默 shell
        }
    }

    /// 用算术展开确认真 shell（命令行回显里是 FERRYOK$((1+1))，只有真 shell 求值才出 FERRYOK2）。
    fn verify_shell(&mut self) -> Result<(), String> {
        for _ in 0..2 {
            let m = self.exp.mark();
            self.send("echo FERRYOK$((1+1))");
            if self
                .exp
                .expect_from(m, &["FERRYOK2"], Duration::from_secs(3))
                .is_some()
            {
                return Ok(());
            }
        }
        Err("提示符验证失败（没进到可用 shell）".into())
    }
}

impl Exec for SerialSession {
    fn xrun(&mut self, cmd: &str) -> String {
        self.run(cmd, Duration::from_secs(5)).0
    }
}

fn step(msg: &str) {
    eprintln!("  {} {}", cyan("▸"), msg);
}

fn tcp_ok(host: &str, port: u16, ms: u64) -> bool {
    format!("{}:{}", host, port)
        .parse()
        .ok()
        .map(|a| TcpStream::connect_timeout(&a, Duration::from_millis(ms)).is_ok())
        .unwrap_or(false)
}

/// 主机本地公钥（给串口装免密用）。
fn host_pubkey() -> Option<String> {
    for c in ["id_ed25519.pub", "id_rsa.pub", "id_ecdsa.pub"] {
        let p = home().join(".ssh").join(c);
        if p.exists() {
            let k = slurp(&p).trim().to_string();
            if !k.is_empty() {
                return Some(k);
            }
        }
    }
    None
}

pub fn up(cfg: &mut Config, name: &str, boot_ok: bool) -> Result<(), String> {
    let d = cfg
        .find(name)
        .ok_or_else(|| format!("没有设备 '{}'", name))?;
    println!("{}", bold(&format!("fy up {} — 通道爬升", d.name)));

    // 0) 已经是可达的 ssh？
    if d.transport == Transport::Ssh && !d.host.is_empty() {
        step(&format!("探测 ssh {}:{} ...", d.host, d.port));
        if tcp_ok(&d.host, d.port, 800) {
            ok("ssh 已可达 —— 你已经在最好的通道上");
            let f = fingerprint::remember(&d, &d.host);
            after_ssh_ready(cfg, &d, &f)?;
            return Ok(());
        }
        warn("ssh 不可达，向下找兜底通道 ...");
    }

    // 0.5) adb 设备：在线就完事
    if d.transport == Transport::Adb {
        let (on, why) = crate::adbx::probe(&d);
        if on {
            ok(&format!("adb 在线 ({})", why));
            let f = fingerprint::remember(&d, "");
            let _ = f;
            info("提示: fy wifi <dev> 可切 WiFi adb；fy share <dev> 可借网");
            return Ok(());
        }
        step(&format!("adb 不在线 ({})", why));
        if let Some(s) = &d.adb_serial {
            if s.contains(':') {
                step(&format!("试着重连 {}", s));
                let _ = run_capture(&argv(&["adb", "connect", s]), &[]);
                let (on2, _) = crate::adbx::probe(&d);
                if on2 {
                    ok("adb 重连成功");
                    return Ok(());
                }
            }
        }
        if d.dev.is_none() {
            return Err("adb 不在线且档案没有串口兜底。插 USB 线或补充 --serial".into());
        }
        warn("落到串口兜底 ...");
    }

    // 1) 串口爬升
    let port = d
        .dev
        .clone()
        .or_else(|| {
            let ports = serialx::serial_ports();
            if ports.len() == 1 {
                info(&format!("档案没写串口，用发现的唯一串口 {}", ports[0]));
                Some(ports[0].clone())
            } else {
                None
            }
        })
        .ok_or("没有串口可用（fy add 时加 --serial /dev/xxx，或 fy scan 看看）")?;
    if dry() {
        println!(
            "{} 串口自动登录 {} @{} → 探测板况 → 配网 → 爬升 ssh（dry-run 不实际操作串口）",
            magenta("DRY→"),
            port,
            d.baud
        );
        return Ok(());
    }
    if crate::blackbox::running_for(&d.name) {
        return Err(format!(
            "黑匣子占着串口。fy bb stop {} 后再 up（之后可再开）",
            d.name
        ));
    }
    step(&format!("打开串口 {} @{}", port, d.baud));
    let mut ss = SerialSession::open(&port, d.baud).map_err(|e| format!("开串口失败: {}", e))?;
    step("自动登录 ...");
    ss.login(&d.user, d.password.as_deref().unwrap_or(""), boot_ok)?;
    ok("串口 shell 已就绪");

    // 2) 指纹
    step("采集设备指纹 ...");
    let mut facts = fingerprint::collect(&mut ss);
    step(&format!(
        "认识你了: {} {} ({})",
        if facts.hostname.is_empty() {
            "?"
        } else {
            &facts.hostname
        },
        facts.kernel,
        facts.arch
    ));

    // 3) 板上网络自查
    step("看看板子有没有 IP ...");
    let (ipout, _) = ss.run(
        "ip -4 addr 2>/dev/null || ifconfig 2>/dev/null",
        Duration::from_secs(5),
    );
    let mut board_ips: Vec<String> = vec![];
    for line in ipout.lines() {
        if let Some(ip) = crate::adbx::extract_ipv4(line) {
            if !ip.starts_with("169.254") && !board_ips.contains(&ip) {
                board_ips.push(ip);
            }
        }
    }
    let (sshd_out, _) = ss.run(
        "command -v dropbear >/dev/null 2>&1 && echo HAVE_DROPBEAR; command -v sshd >/dev/null 2>&1 || ls /usr/sbin/sshd >/dev/null 2>&1 && echo HAVE_SSHD",
        Duration::from_secs(5),
    );
    let has_sshd = sshd_out.contains("HAVE_DROPBEAR") || sshd_out.contains("HAVE_SSHD");

    // 3a) 已有 IP → 主机探测认领
    for ip in &board_ips {
        step(&format!("板子有 IP {}，主机侧探测 ...", ip));
        if !has_sshd {
            let _ = ss.run("(dropbear 2>/dev/null || /usr/sbin/sshd 2>/dev/null || sshd 2>/dev/null) >/dev/null 2>&1; true", Duration::from_secs(4));
        } else {
            let _ = ss.run("pgrep dropbear >/dev/null 2>&1 || pgrep sshd >/dev/null 2>&1 || (dropbear 2>/dev/null || /usr/sbin/sshd 2>/dev/null) >/dev/null 2>&1; true", Duration::from_secs(4));
        }
        if tcp_ok(ip, 22, 900) {
            return promote_to_ssh(cfg, &d, ip, &mut ss, &mut facts);
        }
        step(&format!("{}:22 不通（可能不同网段/没有 sshd）", ip));
    }

    // 3b) USB gadget 路线
    let (udc, _) = ss.run("ls /sys/class/udc 2>/dev/null", Duration::from_secs(4));
    if !udc.trim().is_empty() {
        step(&format!("板子有 UDC ({})，走 USB gadget 配网", udc.trim()));
        let before = usbnet::list_ifaces();
        // 串口灌一段精简 gadget 配置（configfs → g_ether 兜底）
        let seq: &[&str] = &[
            "mount -t configfs none /sys/kernel/config 2>/dev/null; modprobe libcomposite 2>/dev/null; true",
            "G=/sys/kernel/config/usb_gadget/ferry; mkdir -p $G && cd $G && echo 0x1d6b > idVendor && echo 0x0104 > idProduct; true",
            "cd /sys/kernel/config/usb_gadget/ferry && mkdir -p strings/0x409 configs/c.1 functions/ncm.usb0 && echo ferry > strings/0x409/manufacturer && echo usbnet > strings/0x409/product && (cat /etc/machine-id 2>/dev/null || echo f1) > strings/0x409/serialnumber; true",
            "cd /sys/kernel/config/usb_gadget/ferry && ln -sf functions/ncm.usb0 configs/c.1/ 2>/dev/null; ls /sys/class/udc | head -1 > UDC 2>/dev/null || echo UDC_FAIL",
            "sleep 1; (ip addr add 10.55.0.2/30 dev usb0 2>/dev/null; ip link set usb0 up 2>/dev/null) || ifconfig usb0 10.55.0.2 netmask 255.255.255.252 up 2>/dev/null; true",
        ];
        let mut gadget_ok = true;
        for c in seq {
            let (out, st) = ss.run(c, Duration::from_secs(8));
            if out.contains("UDC_FAIL") || st == -1 {
                gadget_ok = false;
                break;
            }
        }
        if !gadget_ok {
            // configfs 不行 → g_ether
            step("configfs 不顺利，试 g_ether 老路 ...");
            let (_, _) = ss.run("modprobe g_ether 2>/dev/null; sleep 1; (ip addr add 10.55.0.2/30 dev usb0 2>/dev/null; ip link set usb0 up) || ifconfig usb0 10.55.0.2 netmask 255.255.255.252 up; true", Duration::from_secs(8));
        }
        // 主机侧等新网口
        step("主机侧等 USB 网口出现（10s）...");
        let mut newif = None;
        for _ in 0..20 {
            std::thread::sleep(Duration::from_millis(500));
            let now = usbnet::list_ifaces();
            if let Some(f) = now.difference(&before).next() {
                newif = Some(f.clone());
                break;
            }
        }
        if let Some(nif) = newif {
            step(&format!(
                "主机新网口 {}，配 {}（sudo）",
                nif,
                usbnet::HOST_IP
            ));
            #[cfg(target_os = "macos")]
            let _ = run_inherit(
                &argv(&[
                    "sudo",
                    "ifconfig",
                    &nif,
                    "inet",
                    usbnet::HOST_IP,
                    "netmask",
                    "255.255.255.252",
                    "up",
                ]),
                &[],
            );
            #[cfg(not(target_os = "macos"))]
            {
                let _ = run_inherit(
                    &argv(&[
                        "sudo",
                        "ip",
                        "addr",
                        "add",
                        &format!("{}/30", usbnet::HOST_IP),
                        "dev",
                        &nif,
                    ]),
                    &[],
                );
                let _ = run_inherit(&argv(&["sudo", "ip", "link", "set", &nif, "up"]), &[]);
            }
            if !has_sshd {
                warn("板上没有 dropbear/sshd —— USB 网通了，但 ssh 登不进。串口继续当家（fy sh）");
            } else {
                let _ = ss.run("pgrep dropbear >/dev/null 2>&1 || pgrep sshd >/dev/null 2>&1 || (dropbear -R 2>/dev/null || dropbear 2>/dev/null || /usr/sbin/sshd 2>/dev/null) ; true", Duration::from_secs(5));
                for _ in 0..10 {
                    if tcp_ok(usbnet::BOARD_IP, 22, 500) {
                        return promote_to_ssh(cfg, &d, usbnet::BOARD_IP, &mut ss, &mut facts);
                    }
                    std::thread::sleep(Duration::from_millis(500));
                }
                warn(&format!(
                    "{}:22 没等到 —— 看看板端 sshd 日志",
                    usbnet::BOARD_IP
                ));
            }
        } else {
            warn("主机没看到新网口（线是数据线吗？板子的 device 口接对了吗？）");
        }
    } else {
        step("板子没有 UDC（不支持 USB device 模式）");
    }

    // 3c) 以太网口 DHCP 尝试
    let (eth, _) = ss.run(
        "ls /sys/class/net 2>/dev/null | grep -E '^(eth|end|enp)' | head -1",
        Duration::from_secs(4),
    );
    let eth = eth.trim().to_string();
    if !eth.is_empty() {
        step(&format!(
            "板子有网口 {}，插着网线的话试试 DHCP（10s）...",
            eth
        ));
        let (out, _) = ss.run(
            &format!("ip link set {e} up 2>/dev/null; (udhcpc -i {e} -n -q -t 5 2>/dev/null || dhclient -1 {e} 2>/dev/null); ip -4 addr show {e} 2>/dev/null || ifconfig {e}", e = eth),
            Duration::from_secs(20),
        );
        if let Some(ip) = crate::adbx::extract_ipv4(&out) {
            if !ip.starts_with("169.254") {
                step(&format!("{} 拿到 {}", eth, ip));
                if has_sshd && tcp_ok(&ip, 22, 900) {
                    return promote_to_ssh(cfg, &d, &ip, &mut ss, &mut facts);
                }
            }
        }
    }

    // 尽力了：串口保底 + 指纹入档
    facts.last_seen = now_epoch();
    config::facts_save(&d.name, &facts);
    let mut dd = d.clone();
    dd.dev = Some(port);
    cfg.devices.insert(dd.name.clone(), dd);
    let _ = cfg.save();
    warn("没爬到 ssh，但串口 shell 是好的：fy sh 直接用；建议板里补装 dropbear。指纹已入档。");
    Ok(())
}

/// 串口在手 → 装公钥 → 确认 ssh → 更新档案。
fn promote_to_ssh(
    cfg: &mut Config,
    d: &Device,
    ip: &str,
    ss: &mut SerialSession,
    facts: &mut config::Facts,
) -> Result<(), String> {
    if let Some(k) = host_pubkey() {
        step("经串口把主机公钥装进板子（ssh 到手即免密）...");
        let cmd = format!(
            "k='{}'; mkdir -p ~/.ssh /etc/dropbear 2>/dev/null; touch ~/.ssh/authorized_keys 2>/dev/null; \
             chmod 700 ~/.ssh 2>/dev/null; grep -qF \"$k\" ~/.ssh/authorized_keys 2>/dev/null || echo \"$k\" >> ~/.ssh/authorized_keys; \
             chmod 600 ~/.ssh/authorized_keys 2>/dev/null; \
             [ -d /etc/dropbear ] && {{ touch /etc/dropbear/authorized_keys; grep -qF \"$k\" /etc/dropbear/authorized_keys || echo \"$k\" >> /etc/dropbear/authorized_keys; chmod 600 /etc/dropbear/authorized_keys; }}; true",
            k.replace('\'', "'\\''")
        );
        let _ = ss.run(&cmd, Duration::from_secs(6));
    } else {
        step("本机还没有 ssh 公钥（回头 fy keyup 会自动生成并安装）");
    }
    facts.last_ip = ip.to_string();
    facts.last_seen = now_epoch();
    config::facts_save(&d.name, facts);

    let mut dd = d.clone();
    dd.transport = Transport::Ssh;
    dd.host = ip.to_string();
    if dd.port == 0 {
        dd.port = 22;
    }
    cfg.devices.insert(dd.name.clone(), dd.clone());
    cfg.save().map_err(|e| e.to_string())?;

    println!();
    ok(&format!(
        "爬升完成: {} 现在走 ssh {}@{}:{}（串口档案保留作兜底）",
        dd.name, dd.user, dd.host, dd.port
    ));
    info(&format!(
        "试试: fy sh {}   fy push {} <文件>   fy share {}（借主机上网）",
        dd.name, dd.name, dd.name
    ));
    after_ssh_ready(cfg, &dd, facts)?;
    Ok(())
}

fn after_ssh_ready(cfg: &mut Config, d: &Device, _facts: &config::Facts) -> Result<(), String> {
    // 有密码档案的，顺手转免密
    if d.password.is_some() {
        step("装公钥转免密 ...");
        if let Err(e) = sshx::keyup(d) {
            warn(&format!("免密安装没成功（不影响使用）: {}", e));
        }
    }
    let _ = cfg;
    Ok(())
}
