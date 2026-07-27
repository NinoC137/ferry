//! `fy sync`：保存即上板。轮询监视本地目录（零依赖、够快），
//! 变化 → rsync / tar 管道 / adb push 增量部署 → 可选钩子命令（如重启服务）。

use crate::adbx;
use crate::config::{Device, Transport};
use crate::sshx;
use crate::util::*;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

const IGNORES: &[&str] = &[".git", "target", "node_modules", ".DS_Store", "__pycache__", ".cache", "build"];

fn snapshot(root: &Path, extra_ignores: &[String]) -> BTreeMap<PathBuf, (u64, u64)> {
    let mut m = BTreeMap::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let rd = match std::fs::read_dir(&dir) {
            Ok(r) => r,
            Err(_) => continue,
        };
        for e in rd.flatten() {
            let p = e.path();
            let name = e.file_name().to_string_lossy().to_string();
            if IGNORES.contains(&name.as_str()) || extra_ignores.contains(&name) || name.ends_with(".swp") || name.ends_with('~') {
                continue;
            }
            let md = match e.metadata() {
                Ok(m) => m,
                Err(_) => continue,
            };
            if md.is_dir() {
                stack.push(p);
            } else if md.is_file() {
                let mtime = md
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                m.insert(p, (mtime, md.len()));
            }
        }
    }
    m
}

/// 单次部署整棵目录/单文件。
pub fn deploy(d: &Device, local: &Path, remote: &str, use_rsync: bool) -> std::io::Result<bool> {
    match d.transport {
        Transport::Adb => adbx::push(d, local, remote),
        Transport::Ssh => {
            if use_rsync {
                let mut ssh_cmd = vec!["ssh".to_string()];
                ssh_cmd.extend(sshx::base_opts(d));
                let src = if local.is_dir() {
                    format!("{}/", local.display())
                } else {
                    local.display().to_string()
                };
                let a = vec![
                    "rsync".to_string(),
                    "-az".to_string(),
                    "--delete".to_string(),
                    "-e".to_string(),
                    render_cmd(&ssh_cmd),
                    src,
                    format!("{}:{}", sshx::target(d), remote),
                ];
                let st = run_inherit(&a, &sshx::askpass_env(d))?;
                if st == 0 {
                    return Ok(true);
                }
                warn("rsync 失败（板上可能没有 rsync），退回 tar 管道");
            }
            sshx::tar_push(d, local, remote)
        }
        Transport::Serial => Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            "串口没法同步文件，先 fy up 打通网络",
        )),
    }
}

/// 板上有 rsync 吗？（只探测一次）
fn remote_has_rsync(d: &Device) -> bool {
    if which("rsync").is_none() {
        return false;
    }
    match d.transport {
        Transport::Ssh => sshx::exec_capture(d, "command -v rsync >/dev/null && echo yes")
            .map(|o| o.stdout.contains("yes"))
            .unwrap_or(false),
        _ => false,
    }
}

pub fn sync_cmd(
    d: &Device,
    local: &Path,
    remote: &str,
    hook: Option<&str>,
    once: bool,
    extra_ignores: Vec<String>,
) -> std::io::Result<()> {
    if !local.exists() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("本地路径不存在: {}", local.display()),
        ));
    }
    let use_rsync = remote_has_rsync(d);
    if use_rsync {
        info("板上有 rsync，走增量同步（--delete 生效）");
    } else {
        info("走 tar 管道全量推送（busybox 板子友好；装个 rsync 会更快）");
    }

    let run_hook = |d: &Device| {
        if let Some(h) = hook {
            info(&format!("钩子: {}", h));
            let r = match d.transport {
                Transport::Ssh => sshx::exec_inherit(d, h, false),
                Transport::Adb => adbx::exec_inherit(d, h, false),
                Transport::Serial => return,
            };
            if let Ok(code) = r {
                if code != 0 {
                    warn(&format!("钩子退出码 {}", code));
                }
            }
        }
    };

    // 首次全量
    info(&format!("首次部署 {} → {}:{}", local.display(), d.name, remote));
    if deploy(d, local, remote, use_rsync)? {
        ok("部署完成");
        run_hook(d);
    } else {
        return Err(std::io::Error::new(std::io::ErrorKind::Other, "部署失败"));
    }
    if once || dry() {
        return Ok(());
    }

    info("监视中（保存即上板，Ctrl-C 退出）...");
    let mut last = snapshot(local, &extra_ignores);
    loop {
        std::thread::sleep(Duration::from_millis(400));
        let now = snapshot(local, &extra_ignores);
        if now != last {
            // 变更抖动等待：连续 300ms 稳定再推
            std::thread::sleep(Duration::from_millis(300));
            let now2 = snapshot(local, &extra_ignores);
            let changed: Vec<String> = now2
                .iter()
                .filter(|(k, v)| last.get(*k) != Some(v))
                .map(|(k, _)| k.strip_prefix(local).unwrap_or(k).display().to_string())
                .take(5)
                .collect();
            let n_removed = last.keys().filter(|k| !now2.contains_key(*k)).count();
            eprintln!(
                "{} 变更: {}{}",
                cyan("↻"),
                changed.join(", "),
                if n_removed > 0 { format!(" (-{} 删除)", n_removed) } else { String::new() }
            );
            match deploy(d, local, remote, use_rsync) {
                Ok(true) => {
                    ok("已上板");
                    run_hook(d);
                }
                _ => warn("这次部署失败（板子掉线？下次变更会重试）"),
            }
            last = now2;
        }
    }
}
