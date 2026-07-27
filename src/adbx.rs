//! adb 封装：-s 选址、shell/push/pull/forward/reverse、一键切 WiFi adb。

use crate::config::{Config, Device};
use crate::util::*;
use std::path::Path;

pub fn adb_argv(d: &Device, rest: &[&str]) -> Vec<String> {
    let mut a = vec!["adb".to_string()];
    if let Some(s) = &d.adb_serial {
        a.push("-s".into());
        a.push(s.clone());
    }
    a.extend(rest.iter().map(|s| s.to_string()));
    a
}

pub fn shell(d: &Device) -> std::io::Result<i32> {
    run_exec(&adb_argv(d, &["shell"]), &[])
}

pub fn exec_inherit(d: &Device, cmd: &str, tty: bool) -> std::io::Result<i32> {
    let mut rest = vec!["shell"];
    if tty {
        rest.push("-t");
    }
    rest.push(cmd);
    run_inherit(&adb_argv(d, &rest), &[])
}

pub fn exec_capture(d: &Device, cmd: &str) -> std::io::Result<Output> {
    run_capture(&adb_argv(d, &["shell", cmd]), &[])
}

pub fn push(d: &Device, local: &Path, remote: &str) -> std::io::Result<bool> {
    let st = run_inherit(
        &adb_argv(d, &["push", &local.display().to_string(), remote]),
        &[],
    )?;
    Ok(st == 0)
}

pub fn pull(d: &Device, remote: &str, local: &Path) -> std::io::Result<bool> {
    let st = run_inherit(
        &adb_argv(d, &["pull", remote, &local.display().to_string()]),
        &[],
    )?;
    Ok(st == 0)
}

/// `adb devices -l` 解析：Vec<(serial, 描述)>。
pub fn list_devices() -> Vec<(String, String)> {
    let out = match run_capture(&argv(&["adb", "devices", "-l"]), &[]) {
        Ok(o) => o,
        Err(_) => return vec![],
    };
    let mut v = vec![];
    for line in out.stdout.lines().skip(1) {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let mut it = line.split_whitespace();
        if let Some(serial) = it.next() {
            let rest: Vec<&str> = it.collect();
            v.push((serial.to_string(), rest.join(" ")));
        }
    }
    v
}

/// 设备在线？（unauthorized/offline 都算不在线，但给出原因）
pub fn probe(d: &Device) -> (bool, String) {
    let devs = list_devices();
    if devs.is_empty() {
        return (false, "no adb device".into());
    }
    match &d.adb_serial {
        Some(s) => {
            for (serial, desc) in &devs {
                if serial == s {
                    if desc.contains("unauthorized") {
                        return (false, "unauthorized(手机上点允许)".into());
                    }
                    if desc.contains("offline") {
                        return (false, "offline".into());
                    }
                    return (true, desc.clone());
                }
            }
            (false, "not found".into())
        }
        None => {
            // 未指定 serial：单设备时视为它
            if devs.len() == 1 {
                (true, devs[0].1.clone())
            } else {
                (
                    false,
                    format!("{} 台设备，请 fy add 时指定 serial", devs.len()),
                )
            }
        }
    }
}

/// 一键切换到 WiFi adb：取 wlan0 IP → adb tcpip 5555 → adb connect。
pub fn wifi(cfg: &mut Config, d: &Device) -> std::io::Result<()> {
    info("读取设备 WiFi IP ...");
    let out = exec_capture(
        d,
        "ip -4 addr show wlan0 2>/dev/null || ifconfig wlan0 2>/dev/null",
    )?;
    let ip = extract_ipv4(&out.stdout);
    let ip = match ip {
        Some(ip) => ip,
        None => {
            if dry() {
                "192.168.x.x".to_string()
            } else {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    "拿不到 wlan0 的 IP（设备连 WiFi 了吗？）",
                ));
            }
        }
    };
    info(&format!("设备 WiFi IP: {}，切换 tcpip 模式 ...", ip));
    run_inherit(&adb_argv(d, &["tcpip", "5555"]), &[])?;
    std::thread::sleep(std::time::Duration::from_millis(if dry() {
        0
    } else {
        1200
    }));
    let ep = format!("{}:5555", ip);
    let st = run_inherit(&argv(&["adb", "connect", &ep]), &[])?;
    if st == 0 && !dry() {
        // 档案换成网络地址，拔线也能用
        if let Some(dd) = cfg.devices.get_mut(&d.name) {
            dd.adb_serial = Some(ep.clone());
        }
        cfg.save()?;
        ok(&format!(
            "{} 已切到 WiFi adb ({})，可以拔掉 USB 线了。恢复 USB: adb usb",
            d.name, ep
        ));
    }
    Ok(())
}

pub fn extract_ipv4(s: &str) -> Option<String> {
    // 从 ip/ifconfig 输出里抓第一个非 127 的 IPv4
    for tok in s.split(|c: char| !(c.is_ascii_digit() || c == '.' || c == '/')) {
        let ip = tok.split('/').next().unwrap_or("");
        let parts: Vec<&str> = ip.split('.').collect();
        if parts.len() == 4
            && parts.iter().all(|p| {
                !p.is_empty()
                    && p.len() <= 3
                    && p.chars().all(|c| c.is_ascii_digit())
                    && p.parse::<u16>().map(|n| n <= 255).unwrap_or(false)
            })
        {
            if !ip.starts_with("127.") && ip != "0.0.0.0" && !ip.starts_with("255.") {
                return Some(ip.to_string());
            }
        }
    }
    None
}
