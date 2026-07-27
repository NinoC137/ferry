//! `fy all -- <cmd>`：对所有（或按名指定的）在线设备并行执行同一条命令，
//! 输出按设备名着色前缀区分。多板一致性检查/批量部署的好帮手。

use crate::adbx;
use crate::config::{Config, Transport};
use crate::sshx;
use crate::util::*;
use std::io::{BufRead, BufReader};
use std::process::Stdio;

pub fn all_cmd(cfg: &Config, filter: &[String], cmd: &str) -> i32 {
    let devs: Vec<_> = cfg
        .devices
        .values()
        .filter(|d| d.transport != Transport::Serial)
        .filter(|d| filter.is_empty() || filter.iter().any(|f| d.name.starts_with(f.as_str())))
        .cloned()
        .collect();
    if devs.is_empty() {
        err("没有匹配的 ssh/adb 设备");
        return 1;
    }
    info(&format!("在 {} 台设备上执行: {}", devs.len(), cmd));
    let palette = [cyan, green, yellow, magenta, blue];
    let mut handles = vec![];
    for (i, d) in devs.into_iter().enumerate() {
        let cmd = cmd.to_string();
        let color = palette[i % palette.len()];
        handles.push(std::thread::spawn(move || -> (String, i32) {
            let (argv_full, envs) = match d.transport {
                Transport::Ssh => {
                    let mut a = vec!["ssh".to_string()];
                    a.extend(sshx::base_opts(&d));
                    a.push(sshx::target(&d));
                    a.push(cmd.clone());
                    (a, sshx::askpass_env(&d))
                }
                Transport::Adb => (adbx::adb_argv(&d, &["shell", &cmd]), vec![]),
                Transport::Serial => unreachable!(),
            };
            if dry() {
                println!("{} {}", magenta("DRY→"), render_cmd(&argv_full));
                return (d.name, 0);
            }
            let child = std::process::Command::new(&argv_full[0])
                .args(&argv_full[1..])
                .envs(envs.iter().map(|(k, v)| (k.clone(), v.clone())))
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn();
            let mut child = match child {
                Ok(c) => c,
                Err(e) => {
                    eprintln!(
                        "{} {}",
                        color(&format!("[{}]", d.name)),
                        red(&e.to_string())
                    );
                    return (d.name, -1);
                }
            };
            let tag = color(&format!("[{}]", d.name));
            let t_out = child.stdout.take().map(|out| {
                let tag = tag.clone();
                std::thread::spawn(move || {
                    for line in BufReader::new(out).lines().map_while(Result::ok) {
                        println!("{} {}", tag, line);
                    }
                })
            });
            let t_err = child.stderr.take().map(|out| {
                let tag = tag.clone();
                std::thread::spawn(move || {
                    for line in BufReader::new(out).lines().map_while(Result::ok) {
                        eprintln!("{} {}", tag, line);
                    }
                })
            });
            let code = child.wait().map(|s| s.code().unwrap_or(-1)).unwrap_or(-1);
            if let Some(t) = t_out {
                let _ = t.join();
            }
            if let Some(t) = t_err {
                let _ = t.join();
            }
            (d.name, code)
        }));
    }
    let mut worst = 0;
    let mut summary = vec![];
    for h in handles {
        if let Ok((name, code)) = h.join() {
            if code != 0 {
                worst = worst.max(1);
            }
            summary.push(format!(
                "{} {}",
                name,
                if code == 0 {
                    green("✓")
                } else {
                    red(&format!("✗({})", code))
                }
            ));
        }
    }
    println!("\n{} {}", bold("结果:"), summary.join("  "));
    worst
}

/// `fy all --json`：并行执行并把每台设备的 stdout/stderr/退出码原样收上来。
pub fn all_json(
    cfg: &Config,
    filter: &[String],
    cmd: &str,
) -> Result<Vec<(String, i32, String, String)>, String> {
    let devs: Vec<_> = cfg
        .devices
        .values()
        .filter(|d| d.transport != Transport::Serial)
        .filter(|d| filter.is_empty() || filter.iter().any(|f| d.name.starts_with(f.as_str())))
        .cloned()
        .collect();
    if devs.is_empty() {
        return Err("没有匹配的 ssh/adb 设备".into());
    }
    let handles: Vec<_> = devs
        .into_iter()
        .map(|d| {
            let cmd = cmd.to_string();
            std::thread::spawn(move || {
                let out = match d.transport {
                    Transport::Ssh => sshx::exec_capture(&d, &cmd),
                    Transport::Adb => adbx::exec_capture(&d, &cmd),
                    Transport::Serial => unreachable!(),
                };
                match out {
                    Ok(o) => (d.name, o.status, o.stdout, o.stderr),
                    Err(e) => (d.name, -1, String::new(), e.to_string()),
                }
            })
        })
        .collect();
    Ok(handles.into_iter().filter_map(|h| h.join().ok()).collect())
}
