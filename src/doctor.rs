//! `fy doctor`：主机环境自检 + 板子体检。
//! `fy fix time`：无 RTC 电池的板子一键对时（告别 1970 年）。

use crate::adbx;
use crate::config::{Config, Device, Transport};
use crate::sshx;
use crate::util::*;

pub fn doctor(cfg: &Config, dev: Option<&Device>) {
    match dev {
        None => host_doctor(cfg),
        Some(d) => board_doctor(d),
    }
}

fn check(name: &str, pass: bool, hint: &str) {
    if pass {
        println!("  {} {}", green("✓"), name);
    } else {
        println!("  {} {}  {}", red("✗"), name, dim(hint));
    }
}

fn host_doctor(cfg: &Config) {
    println!("{}", bold("主机环境:"));
    let sshv = run_capture(&argv(&["ssh", "-V"]), &[])
        .map(|o| if o.stderr.is_empty() { o.stdout } else { o.stderr })
        .unwrap_or_default();
    check("ssh 客户端", which("ssh").is_some(), "装 OpenSSH: xcode-select --install / apt install openssh-client");
    if !sshv.is_empty() {
        let modern = !sshv.contains("OpenSSH_7") && !sshv.contains("OpenSSH_6");
        check(
            &format!("ssh 版本支持免 sshpass 密码注入 ({})", sshv.trim()),
            modern,
            "OpenSSH >= 8.4 才有 SSH_ASKPASS_REQUIRE，太老的话密码档案不生效",
        );
    }
    check("adb", which("adb").is_some(), "brew install android-platform-tools / apt install adb（不用 Android 可忽略）");
    check("stty (串口/raw 模式)", which("stty").is_some(), "系统应自带");
    check("rsync (增量同步更快)", which("rsync").is_some(), "brew/apt install rsync（没有会退回 tar 管道）");
    check("ssh-keygen", which("ssh-keygen").is_some(), "免密 (fy keyup) 需要");

    let cfgd = cfg_dir();
    check(&format!("配置目录 {}", cfgd.display()), cfgd.exists() || ensure_dir(&cfgd).is_ok(), "");
    println!("\n{}", bold("档案:"));
    println!("  {} 台设备，devices.toml 在 {}", cfg.devices.len(), devices_path().display());
    let st = crate::config::State::load();
    let fwds = st.forwards().len();
    if fwds > 0 {
        println!("  {} 条 ssh 转发记录 (fy fwd ls 查看)", fwds);
    }
    let serial_ports = crate::serialx::serial_ports();
    if !serial_ports.is_empty() {
        println!("\n{}", bold("本机串口:"));
        for p in serial_ports {
            println!("  {}", p);
        }
    }
}

fn board_doctor(d: &Device) {
    println!("{}", bold(&format!("板子体检: {}", d.name)));
    let script = "echo T:$(date +%s 2>/dev/null); \
        echo RO:$(grep ' / ' /proc/mounts 2>/dev/null | grep -c ro,); \
        echo DF:$(df -k / 2>/dev/null | tail -1 | awk '{print $5}' | tr -d %); \
        echo DNS:$(grep -c nameserver /etc/resolv.conf 2>/dev/null); \
        echo OOM:$(dmesg 2>/dev/null | grep -ci 'out of memory'); \
        echo SSHD:$( (pgrep dropbear >/dev/null 2>&1 || pgrep sshd >/dev/null 2>&1) && echo 1 || echo 0)";
    let out = match d.transport {
        Transport::Ssh => sshx::exec_capture(d, script),
        Transport::Adb => adbx::exec_capture(d, script),
        Transport::Serial => {
            warn("串口设备先 fy up 打通网络再体检");
            return;
        }
    };
    let out = match out {
        Ok(o) if !o.stdout.is_empty() || dry() => o.stdout,
        _ => {
            err("连不上板子");
            return;
        }
    };
    if dry() {
        return;
    }
    let get = |k: &str| -> Option<String> {
        out.lines()
            .find_map(|l| l.trim().strip_prefix(&format!("{}:", k)).map(|v| v.trim().to_string()))
    };
    if let Some(t) = get("T").and_then(|v| v.parse::<i64>().ok()) {
        let drift = (now_epoch() - t).abs();
        check(
            &format!("系统时间 (偏差 {}s)", drift),
            drift < 60,
            "时间不对会导致 TLS/编译时间戳问题 → fy fix time <dev>",
        );
    }
    if let Some(ro) = get("RO") {
        check("rootfs 可写", ro.trim() == "0", "rootfs 只读挂载：mount -o remount,rw /");
    }
    if let Some(dfp) = get("DF").and_then(|v| v.parse::<i64>().ok()) {
        check(&format!("rootfs 空间 (已用 {}%)", dfp), dfp < 90, "快满了，清理一下");
    }
    if let Some(dns) = get("DNS").and_then(|v| v.parse::<i64>().ok()) {
        check("DNS 配置", dns > 0, "没有 nameserver：fy share <dev> 借网时会自动处理，或手动写 /etc/resolv.conf");
    }
    if let Some(oom) = get("OOM").and_then(|v| v.parse::<i64>().ok()) {
        check("无 OOM 记录", oom == 0, &format!("dmesg 里有 {} 条 Out of memory", oom));
    }
    if let Some(s) = get("SSHD") {
        check("ssh 服务", s.trim() == "1", "没起 dropbear/sshd（串口/adb 板子可忽略）");
    }
}

/// 把主机时间打进板子（busybox / toybox / GNU date 全兼容尝试）。
pub fn fix_time(d: &Device) -> Result<(), String> {
    let epoch = now_epoch();
    // date -s @epoch (GNU/busybox 新版) → date -u MMDDhhmmCCYY.ss (busybox 老版/toybox)
    let out = run_capture(&argv(&["date", "-u", "+%m%d%H%M%Y.%S"]), &[]).map_err(|e| e.to_string())?;
    let stamp = out.stdout.trim().to_string();
    let cmd = format!(
        "(date -s @{e} >/dev/null 2>&1 || date -u -s @{e} >/dev/null 2>&1 || date -u {s} >/dev/null 2>&1 || su 0 date -u {s} >/dev/null 2>&1) && \
         (hwclock -w 2>/dev/null; true) && echo FERRY_TIME_OK $(date)",
        e = epoch,
        s = stamp
    );
    let o = match d.transport {
        Transport::Ssh => sshx::exec_capture(d, &cmd).map_err(|e| e.to_string())?,
        Transport::Adb => adbx::exec_capture(d, &cmd).map_err(|e| e.to_string())?,
        Transport::Serial => return Err("串口设备先 fy up 打通网络（或在 console 里手动 date -s）".into()),
    };
    if dry() {
        return Ok(());
    }
    if o.stdout.contains("FERRY_TIME_OK") {
        ok(&format!("板子时间已同步: {}", o.stdout.replace("FERRY_TIME_OK", "").trim()));
        Ok(())
    } else {
        Err(format!("对时失败（可能没权限）: {} {}", o.stdout.trim(), o.stderr.trim()))
    }
}

/// `fy doctor --json`：agent 最关心的两件事——主机依赖齐不齐、板子够不够得着。
pub fn doctor_json(cfg: &Config, dev: Option<&Device>) -> Vec<(&'static str, crate::jsonout::J)> {
    use crate::jsonout::J;
    let tool = |name: &str| -> J {
        let path = which(name);
        let ver = path.as_ref().and_then(|_| {
            run_capture(&argv(&[name, "--version"]), &[])
                .ok()
                .map(|o| {
                    let t = if o.stdout.trim().is_empty() { o.stderr } else { o.stdout };
                    t.lines().next().unwrap_or("").trim().to_string()
                })
        });
        J::obj(vec![
            ("name", J::s(name)),
            ("found", J::b(path.is_some())),
            ("path", path.map(|p| J::s(p.display().to_string())).unwrap_or(J::Null)),
            ("version", ver.map(J::s).unwrap_or(J::Null)),
        ])
    };
    let mut out = vec![
        (
            "host",
            J::obj(vec![
                ("os", J::s(std::env::consts::OS)),
                ("arch", J::s(std::env::consts::ARCH)),
                ("config_dir", J::s(cfg_dir().display().to_string())),
                ("device_count", J::i(cfg.devices.len() as i64)),
            ]),
        ),
        (
            "tools",
            J::arr(vec![tool("ssh"), tool("scp"), tool("ssh-keygen"), tool("adb"), tool("rsync"), tool("stty")]),
        ),
    ];
    if let Some(d) = dev {
        let reach = match d.transport {
            Transport::Ssh => sshx::exec_capture(d, "echo ok").map(|o| o.status == 0).unwrap_or(false),
            Transport::Adb => crate::adbx::probe(d).0,
            Transport::Serial => d.dev.as_ref().map(|p| std::path::Path::new(p).exists()).unwrap_or(false),
        };
        out.push((
            "device",
            J::obj(vec![
                ("name", J::s(&d.name)),
                ("transport", J::s(d.transport.as_str())),
                ("endpoint", J::s(d.endpoint())),
                ("reachable", J::b(reach)),
            ]),
        ));
    }
    out
}
