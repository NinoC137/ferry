//! 设备指纹：连接时顺手采集 machine-id / MAC / CPU 序列号等身份信息，
//! 让"板子换了 IP / 重刷了系统"之后 ferry 还能认出它。

use crate::adbx;
use crate::config::{self, Device, Facts, Transport};
use crate::sshx;
use crate::util::*;

/// 三种通道共用的"在板上跑一条命令拿输出"抽象。
pub trait Exec {
    fn xrun(&mut self, cmd: &str) -> String;
}

pub struct SshExec<'a>(pub &'a Device);
impl<'a> Exec for SshExec<'a> {
    fn xrun(&mut self, cmd: &str) -> String {
        sshx::exec_capture(self.0, cmd).map(|o| o.stdout).unwrap_or_default()
    }
}

pub struct AdbExec<'a>(pub &'a Device);
impl<'a> Exec for AdbExec<'a> {
    fn xrun(&mut self, cmd: &str) -> String {
        adbx::exec_capture(self.0, cmd).map(|o| o.stdout).unwrap_or_default()
    }
}

/// 采集身份信息（对 busybox / Android 都友好，全部容错）。
pub fn collect<E: Exec>(e: &mut E) -> Facts {
    let mut f = Facts::default();
    f.machine_id = first_line(&e.xrun("cat /etc/machine-id 2>/dev/null || cat /var/lib/dbus/machine-id 2>/dev/null"));
    let macs_raw = e.xrun("cat /sys/class/net/*/address 2>/dev/null");
    f.macs = macs_raw
        .lines()
        .map(|l| l.trim().to_lowercase())
        .filter(|l| l.len() == 17 && l != "00:00:00:00:00:00")
        .collect();
    f.macs.sort();
    f.macs.dedup();
    f.cpu_serial = first_line(&e.xrun("grep -i '^serial' /proc/cpuinfo 2>/dev/null | head -1 | cut -d: -f2"));
    f.hostname = first_line(&e.xrun("hostname 2>/dev/null || getprop ro.product.model 2>/dev/null"));
    f.kernel = first_line(&e.xrun("uname -r 2>/dev/null"));
    f.arch = first_line(&e.xrun("uname -m 2>/dev/null"));
    let android = e.xrun("getprop ro.build.version.release 2>/dev/null");
    f.os = if !android.trim().is_empty() {
        format!("android {}", android.trim())
    } else {
        "linux".into()
    };
    f.last_seen = now_epoch();
    f
}

fn first_line(s: &str) -> String {
    s.lines().next().unwrap_or("").trim().to_string()
}

/// 连接成功后调用：采集并入档，返回是否首次认识。
pub fn remember(d: &Device, ip_seen: &str) -> Facts {
    let mut f = match d.transport {
        Transport::Ssh => collect(&mut SshExec(d)),
        Transport::Adb => collect(&mut AdbExec(d)),
        Transport::Serial => return config::facts_load(&d.name), // 串口由 up 流程负责
    };
    let old = config::facts_load(&d.name);
    if f.machine_id.is_empty() {
        f.machine_id = old.machine_id.clone();
    }
    f.last_ip = ip_seen.to_string();
    config::facts_save(&d.name, &f);
    f
}

/// 用 MAC / machine-id 匹配一个"老朋友"。
pub fn match_known(mac: Option<&str>, machine_id: Option<&str>) -> Option<String> {
    for (name, f) in config::all_facts() {
        if let Some(m) = mac {
            if f.macs.iter().any(|x| x == &m.to_lowercase()) {
                return Some(name);
            }
        }
        if let Some(id) = machine_id {
            if !id.is_empty() && f.machine_id == id {
                return Some(name);
            }
        }
    }
    None
}

/// `fy info <dev>`：身份卡片 + 实时状态。
pub fn info_card(d: &Device) {
    let f = config::facts_load(&d.name);
    println!("{}  {}", bold(&d.name), dim(&format!("[{}] {}", d.transport.as_str(), d.endpoint())));
    let mut rows: Vec<Vec<String>> = vec![];
    let mut push = |k: &str, v: String| {
        if !v.is_empty() {
            rows.push(vec![dim(k), v]);
        }
    };
    push("系统", f.os.clone());
    push("内核", f.kernel.clone());
    push("架构", f.arch.clone());
    push("主机名", f.hostname.clone());
    push("machine-id", f.machine_id.chars().take(16).collect::<String>() + if f.machine_id.len() > 16 { "…" } else { "" });
    push("CPU 序列号", f.cpu_serial.clone());
    push("MAC", f.macs.join(", "));
    push("最近 IP", f.last_ip.clone());
    push("上次见到", human_ago(f.last_seen));
    if !d.notes.is_empty() {
        push("备注", d.notes.clone());
    }
    print_table(&["", ""], &rows);

    // 实时状态（能连上就顺手看一眼）
    let live = match d.transport {
        Transport::Ssh => Some(sshx::exec_capture(d, LIVE_CMD).map(|o| o.stdout).unwrap_or_default()),
        Transport::Adb => Some(adbx::exec_capture(d, LIVE_CMD).map(|o| o.stdout).unwrap_or_default()),
        Transport::Serial => None,
    };
    if let Some(out) = live {
        if !out.trim().is_empty() {
            println!("\n{}", bold("实时:"));
            for line in out.lines().filter(|l| !l.trim().is_empty()) {
                println!("  {}", line.trim_end());
            }
        }
    }
}

pub const LIVE_CMD: &str = "echo \"uptime: $(uptime 2>/dev/null | sed 's/^ *//')\"; \
 df -h / 2>/dev/null | tail -1 | awk '{print \"rootfs: \"$3\"/\"$2\" used (\"$5\")\"}'; \
 free 2>/dev/null | grep -i mem | awk '{printf \"mem: %.0f/%.0f MB\\n\", $3/1024, $2/1024}'; \
 t=$(cat /sys/class/thermal/thermal_zone0/temp 2>/dev/null); [ -n \"$t\" ] && echo \"temp: $((t/1000)).$(( (t%1000)/100 ))°C\"; \
 date '+date: %Y-%m-%d %H:%M:%S %Z'";
