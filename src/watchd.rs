//! 隧道保活与断线自愈。
//!
//! ferry 的端口转发和"借网"都挂在 ssh 的 ControlMaster 上。板子重启一次、
//! WiFi 抖一下、USB 网卡重新枚举——master 就没了，于是：`fy fwd ls` 里所有
//! 转发变成"断"，gdb 连不上，板子上的 `http_proxy` 也哑了，而你往往是过了
//! 十分钟才发现。
//!
//! `fy watch` 起一个后台守护进程盯着：周期性 `ssh -O check`，一旦发现掉线就
//! 重建 master，并**把这台设备的所有转发和 share 反向隧道重新挂回去**。
//! 连不上就指数退避（最长 60 秒一次），恢复时桌面通知你一声。

use crate::config::{Config, Device, State, Transport};
use crate::fwd::Spec;
use crate::proxyd;
use crate::sshx;
use crate::util::*;
use std::time::Duration;

const DEFAULT_INTERVAL: u64 = 15;
const MAX_BACKOFF: u64 = 60;

// ---------------- 生命周期 ----------------

pub fn is_running() -> bool {
    let st = State::load();
    let pid = st.get_int("watch", "pid") as i32;
    pid > 0 && pid_alive(pid)
}

pub fn start(interval: u64, quiet_if_running: bool) -> Result<i32, String> {
    let mut st = State::load();
    let pid = st.get_int("watch", "pid") as i32;
    if pid > 0 && pid_alive(pid) {
        if !quiet_if_running {
            info(&format!("保活守护进程已经在跑 (pid {})", pid));
        }
        return Ok(pid);
    }
    let log = cfg_dir().join("watchd.log");
    let pid = spawn_daemon(
        &argv(&[
            &self_exe().display().to_string(),
            "__watchd",
            "--interval",
            &interval.to_string(),
        ]),
        &log,
    )
    .map_err(|e| format!("起守护进程失败: {}", e))?;
    if dry() {
        return Ok(0);
    }
    st.set_int("watch", "pid", pid as i64);
    st.set_int("watch", "interval", interval as i64);
    st.set_int("watch", "started", now_epoch());
    st.save();
    ok(&format!("隧道保活已开启 (pid {}, 每 {}s 探一次)", pid, interval));
    Ok(pid)
}

pub fn stop() {
    let mut st = State::load();
    let pid = st.get_int("watch", "pid") as i32;
    if pid > 0 && pid_alive(pid) {
        kill_pid(pid);
        for _ in 0..20 {
            if !pid_alive(pid) {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        ok("隧道保活已停止");
    } else {
        info("保活守护进程没在跑");
    }
    st.drop_table("watch");
    st.save();
}

/// 有转发/借网时顺手把保活拉起来（`--no-watch` 可以拒绝）。
pub fn autostart_if_useful() {
    if dry() || is_running() {
        return;
    }
    let st = State::load();
    let has_work = !st.forwards().is_empty() || !st.doc.children("share").is_empty();
    if has_work {
        let _ = start(DEFAULT_INTERVAL, true);
    }
}

#[derive(Debug, Clone, Default)]
pub struct WatchStatus {
    pub running: bool,
    pub pid: i32,
    pub interval: i64,
    pub started: i64,
    /// (设备, 上次探活成功时间, 累计重连次数)
    pub devices: Vec<(String, i64, i64)>,
}

pub fn status() -> WatchStatus {
    let st = State::load();
    let pid = st.get_int("watch", "pid") as i32;
    let mut s = WatchStatus {
        running: pid > 0 && pid_alive(pid),
        pid,
        interval: st.get_int("watch", "interval"),
        started: st.get_int("watch", "started"),
        devices: vec![],
    };
    for name in st.doc.children("watch") {
        let t = format!("watch.{}", name);
        s.devices.push((name, st.get_int(&t, "last_ok"), st.get_int(&t, "reconnects")));
    }
    s
}

// ---------------- 守护进程本体 ----------------

pub fn daemon_main() -> i32 {
    let args: Vec<String> = std::env::args().collect();
    let interval = args
        .iter()
        .position(|a| a == "--interval")
        .and_then(|i| args.get(i + 1))
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(DEFAULT_INTERVAL)
        .clamp(3, 3600);
    eprintln!("ferry watchd: 每 {}s 检查一次隧道", interval);
    // 每台设备各自的退避倍数：连不上的板子不该拖慢其它板子的检查
    let mut backoff: std::collections::HashMap<String, u64> = Default::default();
    let mut down: std::collections::HashMap<String, bool> = Default::default();
    let mut skip_until: std::collections::HashMap<String, i64> = Default::default();

    loop {
        let cfg = Config::load();
        let st = State::load();
        let mut targets: Vec<String> = st.forwards().iter().map(|f| f.dev.clone()).collect();
        targets.extend(st.doc.children("share"));
        targets.sort();
        targets.dedup();

        for name in targets {
            let now = now_epoch();
            if skip_until.get(&name).copied().unwrap_or(0) > now {
                continue;
            }
            let d = match cfg.devices.get(&name) {
                Some(d) if d.transport == Transport::Ssh => d.clone(),
                _ => continue, // adb 的 forward 由 adb server 自己管，不用我们操心
            };
            let alive = sshx::master_ctl(&d, "check", &[]).map(|o| o.status == 0).unwrap_or(false);
            if alive {
                if down.get(&name).copied().unwrap_or(false) {
                    notify("ferry", &format!("{} 的隧道已恢复", name));
                    eprintln!("[{}] 恢复", name);
                }
                down.insert(name.clone(), false);
                backoff.insert(name.clone(), 1);
                mark(&name, "last_ok", now);
                continue;
            }

            let first_time = !down.get(&name).copied().unwrap_or(false);
            if first_time {
                eprintln!("[{}] 隧道断了，尝试重连 ...", name);
            }
            down.insert(name.clone(), true);

            match reattach(&cfg, &d) {
                Ok(n) => {
                    eprintln!("[{}] 重连成功，恢复了 {} 条转发/隧道", name, n);
                    notify("ferry", &format!("{} 已重连，{} 条转发已恢复", name, n));
                    down.insert(name.clone(), false);
                    backoff.insert(name.clone(), 1);
                    bump(&name, "reconnects");
                    mark(&name, "last_ok", now_epoch());
                }
                Err(e) => {
                    let b = backoff.entry(name.clone()).or_insert(1);
                    *b = (*b * 2).min(MAX_BACKOFF / interval.max(1) + 1);
                    let wait = (interval * *b).min(MAX_BACKOFF);
                    if first_time {
                        eprintln!("[{}] 重连失败: {}（{}s 后再试）", name, e, wait);
                    }
                    skip_until.insert(name.clone(), now_epoch() + wait as i64);
                }
            }
        }
        std::thread::sleep(Duration::from_secs(interval));
    }
}

/// 重建 master 并把这台设备**所有**已登记的转发 + share 隧道挂回去。
/// 返回恢复的条目数。
fn reattach(cfg: &Config, d: &Device) -> Result<usize, String> {
    sshx::ensure_master(d).map_err(|e| e.to_string())?;
    let st = State::load();
    let mut n = 0;
    for f in st.forwards() {
        if f.dev != d.name {
            continue;
        }
        if let Ok(spec) = Spec::parse(&f.spec) {
            let out = sshx::master_ctl(d, "forward", &spec.ssh_args()).map_err(|e| e.to_string())?;
            if out.status == 0 {
                n += 1;
            }
        }
    }
    // 借网（代理模式）的反向隧道
    let mode = st.get_str(&format!("share.{}", d.name), "mode");
    if mode == "proxy" {
        let port = st.get_int("proxy", "port") as u16;
        let port = if port == 0 { proxyd::DEFAULT_PORT } else { port };
        // 主机侧的代理进程也可能一起没了，先确保它活着
        let _ = proxyd::ensure_running(port);
        let args = argv(&["-R", &format!("{}:127.0.0.1:{}", port, port)]);
        if sshx::master_ctl(d, "forward", &args).map(|o| o.status == 0).unwrap_or(false) {
            n += 1;
        }
    }
    let _ = cfg;
    Ok(n)
}

fn mark(dev: &str, key: &str, v: i64) {
    let mut st = State::load();
    st.set_int(&format!("watch.{}", dev), key, v);
    st.save();
}

fn bump(dev: &str, key: &str) {
    let mut st = State::load();
    let t = format!("watch.{}", dev);
    let v = st.get_int(&t, key);
    st.set_int(&t, key, v + 1);
    st.save();
}

#[cfg(test)]
mod tests {
    use super::*;

    // 注意：这里刻意不碰 FERRY_HOME —— 环境变量是进程级的，测试并行跑时
    // 改它会把同进程里其它测试（比如 ui 的端到端）的配置目录一起换掉。
    #[test]
    fn status_reads_without_blowing_up() {
        let s = status();
        // 干净环境下不该有 pid；有的话至少得是个正数并且和 running 自洽
        assert!(s.pid >= 0);
        if !s.running {
            assert!(s.pid == 0 || !pid_alive(s.pid));
        }
    }

    #[test]
    fn backoff_never_exceeds_the_cap() {
        // 守护循环里的退避算式：翻倍但夹在 MAX_BACKOFF 以内
        let interval = 15u64;
        let mut b = 1u64;
        for _ in 0..12 {
            b = (b * 2).min(MAX_BACKOFF / interval.max(1) + 1);
            let wait = (interval * b).min(MAX_BACKOFF);
            assert!(wait <= MAX_BACKOFF, "退避超过上限: {}", wait);
            assert!(wait >= interval);
        }
    }
}
