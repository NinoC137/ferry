//! USB 一键配网：主机侧网口识别/配 IP/NAT 共享，板端 gadget 脚本生成与安装。

use crate::config::{Config, Device, State, Transport};
use crate::sshx;
use crate::util::*;
use std::collections::BTreeSet;
use std::net::TcpStream;
use std::time::Duration;

pub const HOST_IP: &str = "10.55.0.1";
pub const BOARD_IP: &str = "10.55.0.2";
pub const SUBNET: &str = "10.55.0.0/30";

pub const GADGET_SH: &str = include_str!("../assets/ferry-gadget.sh");

// ---------------- 网口枚举 ----------------

pub fn list_ifaces() -> BTreeSet<String> {
    #[cfg(target_os = "macos")]
    {
        if let Ok(o) = run_capture(&argv(&["ifconfig", "-l"]), &[]) {
            return o.stdout.split_whitespace().map(|s| s.to_string()).collect();
        }
        BTreeSet::new()
    }
    #[cfg(not(target_os = "macos"))]
    {
        let mut s = BTreeSet::new();
        if let Ok(rd) = std::fs::read_dir("/sys/class/net") {
            for e in rd.flatten() {
                s.insert(e.file_name().to_string_lossy().to_string());
            }
        }
        s
    }
}

/// 网口的 IPv4 地址。
pub fn local_ip_on(ifname: &str) -> Option<String> {
    #[cfg(target_os = "macos")]
    let out = run_capture(&argv(&["ifconfig", ifname]), &[]).ok()?;
    #[cfg(not(target_os = "macos"))]
    let out = run_capture(
        &argv(&["ip", "-o", "-4", "addr", "show", "dev", ifname]),
        &[],
    )
    .ok()?;
    for line in out.stdout.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("inet ") {
            let ip = rest.split_whitespace().next()?.split('/').next()?;
            return Some(ip.to_string());
        }
        if line.contains(" inet ") {
            if let Some(pos) = line.find(" inet ") {
                let rest = &line[pos + 6..];
                let ip = rest.split_whitespace().next()?.split('/').next()?;
                return Some(ip.to_string());
            }
        }
    }
    None
}

/// 本机所有非回环 IPv4：(网口, 地址)。`fy serve` 用它告诉你板子该访问哪个地址。
pub fn local_ipv4s() -> Vec<(String, String)> {
    let mut out = vec![];
    for ifname in list_ifaces() {
        if ifname == "lo" || ifname == "lo0" {
            continue;
        }
        if let Some(ip) = local_ip_on(&ifname) {
            if !ip.starts_with("127.") && !ip.starts_with("169.254.") {
                out.push((ifname, ip));
            }
        }
    }
    // 默认路由那块网卡排前面：多半就是你要的那个地址
    if let Some(def) = default_iface() {
        out.sort_by_key(|(i, _)| u8::from(*i != def));
    }
    out
}

/// 默认路由的外网口。
pub fn default_iface() -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        let o = run_capture(&argv(&["route", "-n", "get", "default"]), &[]).ok()?;
        for line in o.stdout.lines() {
            let line = line.trim();
            if let Some(rest) = line.strip_prefix("interface:") {
                return Some(rest.trim().to_string());
            }
        }
        None
    }
    #[cfg(not(target_os = "macos"))]
    {
        let o = run_capture(&argv(&["ip", "route", "show", "default"]), &[]).ok()?;
        let toks: Vec<&str> = o.stdout.split_whitespace().collect();
        toks.iter()
            .position(|t| *t == "dev")
            .map(|i| toks[i + 1].to_string())
    }
}

/// 通往某 IP 的直连网口 + 推测网段（/24 或 /30）。
pub fn route_iface_for(ip: &str) -> Option<(String, String)> {
    #[cfg(target_os = "macos")]
    let ifname = {
        let o = run_capture(&argv(&["route", "-n", "get", ip]), &[]).ok()?;
        let mut found = None;
        for line in o.stdout.lines() {
            if let Some(rest) = line.trim().strip_prefix("interface:") {
                found = Some(rest.trim().to_string());
            }
        }
        found?
    };
    #[cfg(not(target_os = "macos"))]
    let ifname = {
        let o = run_capture(&argv(&["ip", "route", "get", ip]), &[]).ok()?;
        let toks: Vec<&str> = o.stdout.split_whitespace().collect();
        toks.iter()
            .position(|t| *t == "dev")
            .map(|i| toks[i + 1].to_string())?
    };
    // 网段推测：ferry 的 /30 优先，否则按 /24 报
    let subnet = if ip.starts_with("10.55.0.") {
        SUBNET.to_string()
    } else {
        let mut p: Vec<&str> = ip.split('.').collect();
        if p.len() == 4 {
            p[3] = "0";
            format!("{}/24", p.join("."))
        } else {
            return None;
        }
    };
    Some((ifname, subnet))
}

// ---------------- fy usb net：主机侧一键 ----------------

pub fn usb_net(cfg: &mut Config, share: bool, add_as: Option<String>) -> Result<(), String> {
    info("记录当前网口快照 ...");
    let before = list_ifaces();
    println!(
        "{}",
        yellow("现在插上（或重新插拔）板子的 USB 线 / 让板子跑 ferry-gadget.sh start ...")
    );
    let newif = if dry() {
        "usbX(dry)".to_string()
    } else {
        let mut found = None;
        for _ in 0..60 {
            std::thread::sleep(Duration::from_millis(500));
            let now = list_ifaces();
            let diff: Vec<String> = now.difference(&before).cloned().collect();
            if let Some(f) = diff.into_iter().find(|n| !n.starts_with("lo")) {
                found = Some(f);
                break;
            }
        }
        found
            .ok_or("30 秒内没等到新网口。检查线缆/板端 gadget 是否启动（fy usb gadget 生成脚本）")?
    };
    ok(&format!("发现新网口: {}", newif));

    info(&format!(
        "给 {} 配地址 {}/30（需要 sudo）...",
        newif, HOST_IP
    ));
    #[cfg(target_os = "macos")]
    let st = run_inherit(
        &argv(&[
            "sudo",
            "ifconfig",
            &newif,
            "inet",
            HOST_IP,
            "netmask",
            "255.255.255.252",
            "up",
        ]),
        &[],
    );
    #[cfg(not(target_os = "macos"))]
    let st = (|| {
        let _ = run_inherit(&argv(&["sudo", "ip", "addr", "flush", "dev", &newif]), &[]);
        let _ = run_inherit(
            &argv(&[
                "sudo",
                "ip",
                "addr",
                "add",
                &format!("{}/30", HOST_IP),
                "dev",
                &newif,
            ]),
            &[],
        );
        run_inherit(&argv(&["sudo", "ip", "link", "set", &newif, "up"]), &[])
    })();
    if st.map_err(|e| e.to_string())? != 0 && !dry() {
        return Err("配地址失败".into());
    }

    info(&format!("等板子 {} 出现 ...", BOARD_IP));
    let mut ssh_ok = false;
    if !dry() {
        for _ in 0..30 {
            if TcpStream::connect_timeout(
                &format!("{}:22", BOARD_IP).parse().unwrap(),
                Duration::from_millis(400),
            )
            .is_ok()
            {
                ssh_ok = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(500));
        }
    }
    if ssh_ok {
        ok(&format!("板子 ssh 可达: {}:22", BOARD_IP));
    } else if !dry() {
        warn(&format!(
            "{}:22 暂不可达（板端没起 sshd？串口跑一下 ferry-gadget.sh 看输出）",
            BOARD_IP
        ));
    }

    // 顺手建档
    if let Some(name) = add_as {
        let mut d = cfg
            .devices
            .get(&name)
            .cloned()
            .unwrap_or_else(|| Device::new(&name, Transport::Ssh));
        d.transport = Transport::Ssh;
        d.host = BOARD_IP.into();
        d.port = 22;
        cfg.devices.insert(name.clone(), d);
        cfg.save().map_err(|e| e.to_string())?;
        ok(&format!(
            "档案已更新: {} → ssh {}@{}",
            name, "root", BOARD_IP
        ));
    } else if ssh_ok && !cfg.devices.values().any(|d| d.host == BOARD_IP) {
        info(&format!(
            "提示: fy add <名字> --ssh root@{} 建档，之后 fy sh <名字> 直连",
            BOARD_IP
        ));
    }

    if share {
        nat_enable(SUBNET).map_err(|e| e.to_string())?;
        info("板端把网关指到主机即可全量上网：fy share <设备> --nat 一步到位");
    }
    Ok(())
}

// ---------------- NAT ----------------

/// 打开 主机→外网 的 NAT 转发（幂等）。
pub fn nat_enable(subnet: &str) -> std::io::Result<()> {
    let ext = default_iface().unwrap_or_else(|| "en0".into());
    info(&format!("开启 NAT: {} → {}（需要 sudo）", subnet, ext));
    #[cfg(target_os = "macos")]
    {
        let _ = run_inherit(
            &argv(&["sudo", "sysctl", "-w", "net.inet.ip.forwarding=1"]),
            &[],
        )?;
        // 在 Apple 原始 pf.conf 的翻译规则区插入我们的 nat，保住系统规则
        let orig = slurp(std::path::Path::new("/etc/pf.conf"));
        let nat_line = format!("nat on {ext} from {subnet} to any -> ({ext})\n");
        let conf = if orig.contains("rdr-anchor") {
            let mut out = String::new();
            let mut inserted = false;
            for line in orig.lines() {
                out.push_str(line);
                out.push('\n');
                if !inserted && line.trim_start().starts_with("rdr-anchor") {
                    out.push_str(&nat_line);
                    inserted = true;
                }
            }
            if !inserted {
                out = format!("{}{}", nat_line, orig);
            }
            out
        } else {
            format!("{}{}", nat_line, orig)
        };
        let tmp = std::env::temp_dir().join("ferry-pf.conf");
        std::fs::write(&tmp, conf)?;
        let _ = run_inherit(
            &argv(&["sudo", "pfctl", "-E", "-f", &tmp.display().to_string()]),
            &[],
        )?;
        ok("pf NAT 已加载（fy share --off 恢复系统原始规则）");
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = run_inherit(
            &argv(&["sudo", "sysctl", "-w", "net.ipv4.ip_forward=1"]),
            &[],
        )?;
        let check = run_capture(
            &argv(&[
                "sudo",
                "iptables",
                "-t",
                "nat",
                "-C",
                "POSTROUTING",
                "-s",
                subnet,
                "-o",
                &ext,
                "-j",
                "MASQUERADE",
            ]),
            &[],
        )?;
        if check.status != 0 {
            let _ = run_inherit(
                &argv(&[
                    "sudo",
                    "iptables",
                    "-t",
                    "nat",
                    "-A",
                    "POSTROUTING",
                    "-s",
                    subnet,
                    "-o",
                    &ext,
                    "-j",
                    "MASQUERADE",
                ]),
                &[],
            )?;
        }
        for dir in [["-s", subnet], ["-d", subnet]] {
            let c = run_capture(
                &argv(&[
                    "sudo", "iptables", "-C", "FORWARD", dir[0], dir[1], "-j", "ACCEPT",
                ]),
                &[],
            )?;
            if c.status != 0 {
                let _ = run_inherit(
                    &argv(&[
                        "sudo", "iptables", "-A", "FORWARD", dir[0], dir[1], "-j", "ACCEPT",
                    ]),
                    &[],
                )?;
            }
        }
        ok("iptables NAT 已配置");
    }
    let mut st = State::load();
    st.set_str("nat", "subnet", subnet);
    st.save();
    Ok(())
}

pub fn nat_disable() -> std::io::Result<()> {
    let mut st = State::load();
    let subnet = st.get_str("nat", "subnet");
    if subnet.is_empty() {
        return Ok(());
    }
    #[cfg(target_os = "macos")]
    {
        let _ = &subnet; // macOS 直接恢复系统 pf.conf，不按网段逐条删
        let _ = run_inherit(&argv(&["sudo", "pfctl", "-f", "/etc/pf.conf"]), &[])?;
        ok("已恢复系统 pf 规则");
    }
    #[cfg(not(target_os = "macos"))]
    {
        let ext = default_iface().unwrap_or_else(|| "en0".into());
        let _ = run_capture(
            &argv(&[
                "sudo",
                "iptables",
                "-t",
                "nat",
                "-D",
                "POSTROUTING",
                "-s",
                &subnet,
                "-o",
                &ext,
                "-j",
                "MASQUERADE",
            ]),
            &[],
        );
        let _ = run_capture(
            &argv(&[
                "sudo", "iptables", "-D", "FORWARD", "-s", &subnet, "-j", "ACCEPT",
            ]),
            &[],
        );
        let _ = run_capture(
            &argv(&[
                "sudo", "iptables", "-D", "FORWARD", "-d", &subnet, "-j", "ACCEPT",
            ]),
            &[],
        );
        ok("已移除 iptables NAT 规则");
    }
    st.drop_table("nat");
    st.save();
    Ok(())
}

// ---------------- gadget 脚本 ----------------

pub fn gadget_emit(out: Option<&str>, mode: &str) -> std::io::Result<()> {
    let script = GADGET_SH.replace(
        "MODE=\"${MODE:-ncm}\"",
        &format!("MODE=\"${{MODE:-{}}}\"", mode),
    );
    match out {
        Some(p) => {
            std::fs::write(p, &script)?;
            let _ = std::process::Command::new("chmod")
                .arg("755")
                .arg(p)
                .status();
            ok(&format!("已生成 {}（推到板上: fy usb install <设备>）", p));
        }
        None => {
            print!("{}", script);
        }
    }
    Ok(())
}

/// 推送 gadget 脚本到板上并（可选）注册开机自启。
pub fn gadget_install(d: &Device, mode: &str, autostart: bool) -> Result<(), String> {
    let script = GADGET_SH.replace(
        "MODE=\"${MODE:-ncm}\"",
        &format!("MODE=\"${{MODE:-{}}}\"", mode),
    );
    let path = "/usr/local/bin/ferry-gadget.sh";
    let okk = match d.transport {
        Transport::Ssh => {
            sshx::write_remote_file(d, path, &script, "755").map_err(|e| e.to_string())?
        }
        Transport::Adb => {
            let tmp = std::env::temp_dir().join("ferry-gadget.sh");
            std::fs::write(&tmp, &script).map_err(|e| e.to_string())?;
            crate::adbx::push(d, &tmp, path).map_err(|e| e.to_string())?
                && crate::adbx::exec_capture(d, &format!("chmod 755 {}", path))
                    .map(|o| o.status == 0)
                    .unwrap_or(false)
        }
        Transport::Serial => {
            return Err(
                "串口通道推脚本太慢，先 fy up 打通网络，或 fy usb gadget --out 拷出去手动放".into(),
            )
        }
    };
    if !okk && !dry() {
        return Err("脚本推送失败".into());
    }
    ok(&format!("已安装 {} 到 {}", path, d.name));
    if autostart {
        let unit = "[Unit]\nDescription=ferry usb gadget network\nAfter=local-fs.target\n\n[Service]\nType=oneshot\nExecStart=/usr/local/bin/ferry-gadget.sh start\nRemainAfterExit=yes\nExecStop=/usr/local/bin/ferry-gadget.sh stop\n\n[Install]\nWantedBy=multi-user.target\n";
        let cmd = format!(
            "if command -v systemctl >/dev/null 2>&1; then \
               printf '%s' '{unit}' > /etc/systemd/system/ferry-gadget.service && \
               systemctl daemon-reload && systemctl enable ferry-gadget.service && echo FERRY_SYSTEMD_OK; \
             elif [ -f /etc/rc.local ]; then \
               grep -q ferry-gadget /etc/rc.local || sed -i 's#^exit 0#{path} start\\nexit 0#' /etc/rc.local; echo FERRY_RCLOCAL_OK; \
             elif [ -d /etc/init.d ]; then \
               ln -sf {path} /etc/init.d/S99ferry-gadget 2>/dev/null; echo FERRY_INITD_OK; \
             else echo FERRY_MANUAL; fi",
            unit = unit.replace('\'', "'\\''").replace('\n', "\\n"),
            path = path
        );
        let out = match d.transport {
            Transport::Ssh => sshx::exec_capture(d, &cmd).map_err(|e| e.to_string())?,
            Transport::Adb => crate::adbx::exec_capture(d, &cmd).map_err(|e| e.to_string())?,
            Transport::Serial => unreachable!(),
        };
        if dry() {
            return Ok(());
        }
        if out.stdout.contains("FERRY_SYSTEMD_OK") {
            ok("已注册 systemd 开机自启 (ferry-gadget.service)");
        } else if out.stdout.contains("FERRY_RCLOCAL_OK") {
            ok("已挂到 /etc/rc.local 开机自启");
        } else if out.stdout.contains("FERRY_INITD_OK") {
            ok("已链接 /etc/init.d/S99ferry-gadget（BusyBox init 风格）");
        } else {
            warn("没识别出板上的 init 系统，请手动把 ferry-gadget.sh start 挂到开机脚本");
        }
    } else {
        info(&format!(
            "板上手动启动: {} start   （加 --autostart 注册开机自启）",
            path
        ));
    }
    Ok(())
}
