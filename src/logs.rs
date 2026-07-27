//! `fy log`：跟日志（journalctl/syslog/dmesg/logcat 自动选）。
//! `fy top`：多板实时仪表盘（CPU/内存/温度/rootfs，2s 刷新）。

use crate::adbx;
use crate::config::{Config, Device, Transport};
use crate::sshx;
use crate::util::*;
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub fn log_follow(cfg: &Config, d: &Device, save: Option<&str>) -> std::io::Result<i32> {
    // 保存的话用 tee（省得自己拆流）
    let wrap = |cmd: Vec<String>| -> Vec<String> {
        match save {
            Some(f) => argv(&[
                "/bin/sh",
                "-c",
                &format!("{} | tee -a {}", render_cmd(&cmd), shell_quote(f)),
            ]),
            None => cmd,
        }
    };
    match d.transport {
        Transport::Ssh => {
            // journalctl → syslog/messages tail → dmesg 兜底，一条远端 sh 搞定
            let remote = "if command -v journalctl >/dev/null 2>&1; then exec journalctl -f -n 100; \
                 elif [ -f /var/log/messages ] || [ -f /var/log/syslog ]; then exec tail -n 100 -F /var/log/messages /var/log/syslog 2>/dev/null; \
                 else dmesg 2>/dev/null | tail -n 100; echo '--- dmesg follow (1s poll) ---'; \
                 while :; do dmesg -c 2>/dev/null || dmesg | tail -5; sleep 1; done; fi";
            let mut a = vec!["ssh".to_string()];
            a.extend(sshx::base_opts(d));
            a.push("-t".into());
            a.push(sshx::target(d));
            a.push(remote.to_string());
            run_inherit(&wrap(a), &sshx::askpass_env(d))
        }
        Transport::Adb => {
            let a = adbx::adb_argv(d, &["logcat", "-v", "color,threadtime"]);
            run_inherit(&wrap(a), &[])
        }
        Transport::Serial => {
            // 串口的"日志"= console 本身；黑匣子在跑就看录制
            if crate::blackbox::running_for(&d.name) {
                let f = crate::blackbox::log_path(&d.name);
                info(&format!("黑匣子录制中，跟随 {}", f.display()));
                run_inherit(
                    &argv(&["tail", "-n", "50", "-F", &f.display().to_string()]),
                    &[],
                )
            } else {
                info("串口设备直接进 console（fy bb start 可后台持续录）");
                crate::blackbox::serial_shell(cfg, &d.name).map(|_| 0)
            }
        }
    }
}

// ---------------- fy top ----------------

struct Sample {
    ok: bool,
    line: String,
}

fn sample_device(d: &Device) -> Sample {
    let cmd = "c1=$(head -1 /proc/stat); sleep 1; c2=$(head -1 /proc/stat); \
        echo \"CPU $c1|$c2\"; \
        free 2>/dev/null | grep -i mem: | awk '{printf \"MEM %d %d\\n\", $3, $2}'; \
        t=$(cat /sys/class/thermal/thermal_zone0/temp 2>/dev/null); [ -n \"$t\" ] && echo \"TMP $t\"; \
        df -k / 2>/dev/null | tail -1 | awk '{printf \"DSK %s %s\\n\", $5, $4}'; \
        cat /proc/loadavg 2>/dev/null | awk '{printf \"LAV %s %s %s\\n\", $1, $2, $3}'; \
        echo \"UPT $(cat /proc/uptime 2>/dev/null | cut -d. -f1)\"";
    let out = match d.transport {
        Transport::Ssh => sshx::exec_capture(d, cmd).map(|o| o).ok(),
        Transport::Adb => adbx::exec_capture(d, cmd).map(|o| o).ok(),
        Transport::Serial => None,
    };
    let out = match out {
        Some(o) if o.status == 0 || !o.stdout.is_empty() => o.stdout,
        _ => {
            return Sample {
                ok: false,
                line: format!("{}  {}", d.name, red("离线")),
            };
        }
    };
    // 解析
    let mut cpu = String::from("  --  ");
    let mut mem = String::from("   --   ");
    let mut tmp = String::from("  -- ");
    let mut dsk = String::from("  -- ");
    let mut lav = String::from("--");
    let mut upt = String::from("--");
    for line in out.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("CPU ") {
            if let Some((a, b)) = rest.split_once('|') {
                cpu = cpu_pct(a, b).map(|p| format!("{:4.0}%", p)).unwrap_or(cpu);
            }
        } else if let Some(rest) = line.strip_prefix("MEM ") {
            let t: Vec<&str> = rest.split_whitespace().collect();
            if t.len() == 2 {
                let (u, tt) = (
                    t[0].parse::<f64>().unwrap_or(0.0),
                    t[1].parse::<f64>().unwrap_or(1.0),
                );
                mem = format!("{:3.0}/{:.0}M", u / 1024.0, tt / 1024.0);
            }
        } else if let Some(rest) = line.strip_prefix("TMP ") {
            if let Ok(v) = rest.trim().parse::<f64>() {
                let c = if v > 1000.0 { v / 1000.0 } else { v };
                tmp = format!("{:4.1}°", c);
            }
        } else if let Some(rest) = line.strip_prefix("DSK ") {
            let t: Vec<&str> = rest.split_whitespace().collect();
            if !t.is_empty() {
                dsk = t[0].to_string();
            }
        } else if let Some(rest) = line.strip_prefix("LAV ") {
            lav = rest.split_whitespace().next().unwrap_or("--").to_string();
        } else if let Some(rest) = line.strip_prefix("UPT ") {
            if let Ok(s) = rest.trim().parse::<i64>() {
                upt = if s > 86400 {
                    format!("{}d{}h", s / 86400, (s % 86400) / 3600)
                } else {
                    format!("{}h{}m", s / 3600, (s % 3600) / 60)
                };
            }
        }
    }
    Sample {
        ok: true,
        line: format!(
            "{:<12} {} cpu {}  mem {}  {}  / {}  load {}  up {}",
            d.name,
            green("●"),
            cpu,
            mem,
            tmp,
            dsk,
            lav,
            upt
        ),
    }
}

/// /proc/stat 两次采样算利用率。
fn cpu_pct(a: &str, b: &str) -> Option<f64> {
    let parse = |s: &str| -> Option<Vec<u64>> {
        let v: Vec<u64> = s
            .split_whitespace()
            .skip(1)
            .filter_map(|x| x.parse().ok())
            .collect();
        if v.len() >= 4 {
            Some(v)
        } else {
            None
        }
    };
    let (x, y) = (parse(a)?, parse(b)?);
    let tot_a: u64 = x.iter().sum();
    let tot_b: u64 = y.iter().sum();
    let idle_a = x.get(3).copied().unwrap_or(0) + x.get(4).copied().unwrap_or(0);
    let idle_b = y.get(3).copied().unwrap_or(0) + y.get(4).copied().unwrap_or(0);
    let dt = tot_b.saturating_sub(tot_a) as f64;
    if dt <= 0.0 {
        return None;
    }
    Some(100.0 * (1.0 - (idle_b.saturating_sub(idle_a) as f64) / dt))
}

pub fn top(cfg: &Config) {
    let devs: Vec<Device> = cfg
        .devices
        .values()
        .filter(|d| d.transport != Transport::Serial)
        .cloned()
        .collect();
    if devs.is_empty() {
        err("没有可监控的 ssh/adb 设备");
        return;
    }
    if dry() {
        for d in &devs {
            let _ = sample_device(d);
        }
        return;
    }
    println!("{}", dim("fy top — 2s 刷新，Ctrl-C 退出"));
    let results: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(vec![String::new(); devs.len()]));
    loop {
        let mut handles = vec![];
        for (i, d) in devs.iter().enumerate() {
            let d = d.clone();
            let results = results.clone();
            handles.push(std::thread::spawn(move || {
                let s = sample_device(&d);
                results.lock().unwrap()[i] = s.line;
                let _ = s.ok;
            }));
        }
        for h in handles {
            let _ = h.join();
        }
        // 重绘
        print!("\x1b[2J\x1b[H");
        println!(
            "{}   {}",
            bold("ferry top"),
            dim(&format!("{} 台设备", devs.len()))
        );
        println!();
        for line in results.lock().unwrap().iter() {
            println!("  {}", line);
        }
        let _ = std::io::Write::flush(&mut std::io::stdout());
        std::thread::sleep(Duration::from_millis(1000));
    }
}
