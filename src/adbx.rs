//! adb 封装：-s 选址、shell/push/pull/forward/reverse、一键切 WiFi adb。

use crate::config::{Config, Device, Transport};
use crate::util::*;
use std::path::Path;
use std::time::Duration;

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
    let d = resolved(d);
    run_exec(&adb_argv(&d, &["shell"]), &[])
}

pub fn exec_inherit(d: &Device, cmd: &str, tty: bool) -> std::io::Result<i32> {
    let d = resolved(d);
    let mut rest = vec!["shell"];
    if tty {
        rest.push("-t");
    }
    rest.push(cmd);
    run_inherit(&adb_argv(&d, &rest), &[])
}

pub fn exec_capture(d: &Device, cmd: &str) -> std::io::Result<Output> {
    // 同 list_devices：先确保 server 已在跑，避免 adb 首次运行 fork 守护
    // 进程后继承并挂住我们的捕获管道。resolved() 里已会拉起 server。
    let d = resolved(d);
    run_capture(&adb_argv(&d, &["shell", cmd]), &[])
}

pub fn push(d: &Device, local: &Path, remote: &str) -> std::io::Result<bool> {
    let d = resolved(d);
    let st = run_inherit(
        &adb_argv(&d, &["push", &local.display().to_string(), remote]),
        &[],
    )?;
    Ok(st == 0)
}

pub fn pull(d: &Device, remote: &str, local: &Path) -> std::io::Result<bool> {
    let d = resolved(d);
    let st = run_inherit(
        &adb_argv(&d, &["pull", remote, &local.display().to_string()]),
        &[],
    )?;
    Ok(st == 0)
}

/// 预启动 adb 服务端，并把它的 stdio 全部接到 /dev/null。
///
/// 关键：`adb devices` 在服务端未运行时会 fork 出一个常驻 server 守护进程。
/// 若此刻我们用 `Command::output()`（管道）去捕获，守护进程会继承并**终身
/// 持有**这对 stdout/stderr 管道写端，导致 `output()` 永远等不到 EOF——桌面
/// 端首次扫描"走到 adb 这一步就卡死"正是此因（CLI 下终端里 server 往往已在
/// 跑，故不复现）。先用 null stdio 显式把 server 拉起来，之后的 `adb devices`
/// 便只是连接既有 server、立即返回，既不再 fork，也不再挂住我们的管道。
pub fn ensure_server() {
    if dry() {
        return;
    }
    let _ = std::process::Command::new("adb")
        .arg("start-server")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

/// `adb devices -l` 解析：Vec<(serial, 描述)>。
///
/// 加了一小段"空结果重试"：`adb start-server` 只保证 server 进程起来，USB 设备的
/// 枚举/登记是**异步**的，紧接着的 `adb devices` 有时还没把传输挂上——这正是
/// "scan 有时扫不出 adb 设备"的主因（尤其桌面端刚启动、或另一套 adb server
/// 因版本不一致被 start-server 杀掉重启的瞬间）。空结果时短轮询几次兜住这个
/// 竞态；真没有设备时总预算也就 ~0.75s，很快返回。命令超时（server 卡死）不
/// 重试，直接返回已知结果，避免把 8s 超时叠成几十秒。
pub fn list_devices() -> Vec<(String, String)> {
    ensure_server();
    let mut last: Vec<(String, String)> = vec![];
    for attempt in 0..4 {
        let out = match run_capture_timeout(
            &argv(&["adb", "devices", "-l"]),
            &[],
            Duration::from_secs(8),
        ) {
            Ok(o) => o,
            Err(_) => return last,
        };
        if out.status == -1 {
            // run_capture_timeout 超时被杀：别再叠加重试。
            return last;
        }
        last = parse_devices(&out.stdout);
        if !last.is_empty() {
            return last;
        }
        if attempt < 3 {
            std::thread::sleep(Duration::from_millis(250));
        }
    }
    last
}

fn parse_devices(stdout: &str) -> Vec<(String, String)> {
    let mut v = vec![];
    for line in stdout.lines().skip(1) {
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

/// 采集设备**内部**的稳定标识：换 USB 口不会变（不像 adb 的 serial，缺 USB
/// 序列号描述符时会退化成端口路径）。优先 Android 硬件序列号，退回
/// machine-id / 网卡 MAC。设备离线/未授权时拿不到，返回 None。
pub fn capture_stable_id(serial: &str) -> Option<String> {
    if dry() {
        return None;
    }
    ensure_server();
    let script = "getprop ro.serialno 2>/dev/null; \
                  getprop ro.boot.serialno 2>/dev/null; \
                  cat /etc/machine-id 2>/dev/null; \
                  cat /sys/class/net/eth0/address 2>/dev/null; \
                  cat /sys/class/net/wlan0/address 2>/dev/null";
    let out = run_capture_timeout(
        &argv(&["adb", "-s", serial, "shell", script]),
        &[],
        Duration::from_secs(6),
    )
    .ok()?;
    pick_stable_id(&out.stdout)
}

/// 从探测脚本的多行输出里挑第一条像样的稳定标识：跳过空行、`unknown`、纯 `0`
/// 和过短（<4 字符）的行。抽成纯函数便于单测。
fn pick_stable_id(stdout: &str) -> Option<String> {
    for line in stdout.lines() {
        let t = line.trim();
        if t.len() >= 4 && t != "unknown" && !t.eq_ignore_ascii_case("0") {
            return Some(t.to_string());
        }
    }
    None
}

/// 在给定的当前设备列表里，算出此刻真正对应这台档案设备的 adb serial。
/// 顺序：① 档案里的 serial 仍在列表里就直接用；② 否则用稳定标识
/// `adb_id` 逐台比对认领（换了 USB 口 serial 变了也能找回）；③ 档案没写死
/// serial、且此刻只有一台设备时，认领它。
fn live_serial_from(d: &Device, devs: &[(String, String)]) -> Option<String> {
    if let Some(s) = &d.adb_serial {
        if devs.iter().any(|(serial, _)| serial == s) {
            return Some(s.clone());
        }
    }
    if let Some(id) = &d.adb_id {
        for (serial, desc) in devs {
            if desc.contains("unauthorized") || desc.contains("offline") {
                continue;
            }
            if capture_stable_id(serial).as_deref() == Some(id.as_str()) {
                return Some(serial.clone());
            }
        }
    }
    if d.adb_serial.is_none() && devs.len() == 1 {
        return Some(devs[0].0.clone());
    }
    None
}

/// 此刻应当传给 `adb -s` 的 serial。网络地址（ip:port）原样返回、不去比对。
pub fn live_serial(d: &Device) -> Option<String> {
    // ip:port 形态本身就是稳定端点，不受 USB 口影响。
    if d.adb_serial.as_deref().is_some_and(|s| s.contains(':')) {
        return d.adb_serial.clone();
    }
    live_serial_from(d, &list_devices())
}

/// 返回一个把 `adb_serial` 替换成"此刻真正在线 serial"的设备副本，供各 adb
/// 操作使用：换了 USB 口也能命中同一台。认不出来就原样返回（让底层 adb 自己报错）。
pub fn resolved(d: &Device) -> Device {
    if d.transport != Transport::Adb {
        return d.clone();
    }
    let mut d = d.clone();
    if let Some(s) = live_serial(&d) {
        d.adb_serial = Some(s);
    }
    d
}

/// 有 Config 在手时调用：把"此刻在线 serial"和"稳定标识 adb_id"写回档案。
/// 这就是"第一次连接就记录标识、之后换口也认得"的落地：
/// - 换了 USB 口 → 更新 adb_serial 指向新 serial；
/// - 档案还没有 adb_id 且设备在线 → 采集并存下来，作为以后认领的锚点。
pub fn sync_profile(cfg: &mut Config, name: &str) {
    let d = match cfg.devices.get(name) {
        Some(d) if d.transport == Transport::Adb => d.clone(),
        _ => return,
    };
    // 网络 adb（ip:port）不涉及 USB 口漂移，跳过。
    if d.adb_serial.as_deref().is_some_and(|s| s.contains(':')) {
        return;
    }
    let live = match live_serial_from(&d, &list_devices()) {
        Some(s) => s,
        None => return,
    };
    let mut changed = false;
    if d.adb_serial.as_deref() != Some(live.as_str()) {
        if let Some(dd) = cfg.devices.get_mut(name) {
            dd.adb_serial = Some(live.clone());
            changed = true;
        }
    }
    if d.adb_id.is_none() {
        if let Some(id) = capture_stable_id(&live) {
            if let Some(dd) = cfg.devices.get_mut(name) {
                dd.adb_id = Some(id);
                changed = true;
            }
        }
    }
    if changed {
        let _ = cfg.save();
    }
}

/// 设备在线？（unauthorized/offline 都算不在线，但给出原因）
pub fn probe(d: &Device) -> (bool, String) {
    let devs = list_devices();
    if devs.is_empty() {
        return (false, "no adb device".into());
    }
    match live_serial_from(d, &devs) {
        Some(s) => {
            for (serial, desc) in &devs {
                if serial == &s {
                    if desc.contains("unauthorized") {
                        return (false, "unauthorized(手机上点允许)".into());
                    }
                    if desc.contains("offline") {
                        return (false, "offline".into());
                    }
                    // 只有"档案里原本钉着某个 serial、此刻却在另一个 serial 上靠
                    // adb_id 找回"才算换口认领；没钉 serial 的单设备默认认领不算。
                    let reclaimed = d.adb_serial.as_deref().is_some_and(|old| old != s.as_str());
                    if reclaimed {
                        // 靠稳定标识在新 USB 口上找回的
                        return (true, format!("{} (换口认领: {})", desc.clone(), s));
                    }
                    return (true, desc.clone());
                }
            }
            (false, "not found".into())
        }
        None => {
            if d.adb_serial.is_some() {
                (false, "not found（可能换了 USB 口且未记录标识）".into())
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

#[cfg(test)]
mod tests {
    use super::*;

    fn adb_dev(serial: Option<&str>, id: Option<&str>) -> Device {
        let mut d = Device::new("t", Transport::Adb);
        d.adb_serial = serial.map(|s| s.to_string());
        d.adb_id = id.map(|s| s.to_string());
        d
    }

    #[test]
    fn parse_devices_skips_header_and_blanks() {
        let out = "List of devices attached\n\
                   ABC123\tdevice product:x model:y\n\
                   \n\
                   192.168.1.9:5555 offline\n";
        let v = parse_devices(out);
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].0, "ABC123");
        assert!(v[0].1.contains("model:y"), "描述应保留 product/model");
        assert_eq!(
            v[1],
            ("192.168.1.9:5555".to_string(), "offline".to_string())
        );
    }

    #[test]
    fn pick_stable_id_skips_noise() {
        // unknown / 纯 0 / 过短行都跳过，取第一条像样的
        let s = "unknown\n0\nab\n0123456789ABCDEF\naa:bb:cc:dd:ee:ff\n";
        assert_eq!(pick_stable_id(s).as_deref(), Some("0123456789ABCDEF"));
        assert_eq!(pick_stable_id("unknown\n0\n").as_deref(), None);
        assert_eq!(pick_stable_id("").as_deref(), None);
    }

    #[test]
    fn live_serial_from_uses_pinned_serial_when_present() {
        let d = adb_dev(Some("ABC123"), None);
        let devs = vec![
            ("ABC123".to_string(), "device".to_string()),
            ("ZZZ999".to_string(), "device".to_string()),
        ];
        assert_eq!(live_serial_from(&d, &devs).as_deref(), Some("ABC123"));
    }

    #[test]
    fn live_serial_from_claims_only_single_device_when_no_serial() {
        let d = adb_dev(None, None);
        let one = vec![("ONLY".to_string(), "device".to_string())];
        assert_eq!(live_serial_from(&d, &one).as_deref(), Some("ONLY"));
        // 多台且没钉 serial、没 adb_id → 不猜
        let many = vec![
            ("A".to_string(), "device".to_string()),
            ("B".to_string(), "device".to_string()),
        ];
        assert_eq!(live_serial_from(&d, &many), None);
    }

    #[test]
    fn live_serial_from_gives_up_when_pinned_serial_absent_and_no_id() {
        // 钉了 serial 但已不在列表、又没有 adb_id 可比对 → None，
        // 绝不退化成"随便认列表里的另一台"。
        let d = adb_dev(Some("GONE"), None);
        let devs = vec![("OTHER".to_string(), "device".to_string())];
        assert_eq!(live_serial_from(&d, &devs), None);
    }
}
