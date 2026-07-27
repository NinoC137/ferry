//! 黑匣子：后台守护进程常驻串口，持续录制 + panic 侦测 + 桌面通知。
//! 板子半夜崩了、重启了，现场都在。`fy sh` 会自动 attach 共享串口，
//! 记录与交互两不误（串口独占问题从此消失）。

use crate::config::{Config, State, Transport};
use crate::serialx;
use crate::util::*;
use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const PANIC_PATTERNS: &[&str] = &[
    "Kernel panic",
    "Oops:",
    "BUG: ",
    "Unable to handle kernel",
    "rcu_sched detected stall",
    "rcu: INFO:",
    "watchdog: BUG: soft lockup",
    "Internal error:",
    "Segmentation fault",
    "Out of memory: Kill",
];
const RING_LINES: usize = 400; // 事故现场保留的行数
const COOLDOWN_SECS: i64 = 30;

pub fn sock_path(dev: &str) -> PathBuf {
    bb_dir().join(format!("{}.sock", dev))
}
pub fn log_path(dev: &str) -> PathBuf {
    bb_dir().join(format!("{}.log", dev))
}
pub fn incidents_dir(dev: &str) -> PathBuf {
    bb_dir().join(format!("{}.incidents", dev))
}

// ---------------- 管理命令 ----------------

pub fn start(cfg: &Config, name: &str) -> Result<(), String> {
    let d = cfg.devices.get(name).ok_or_else(|| format!("没有设备 '{}'", name))?;
    let port = d.dev.clone().ok_or("该设备档案没有串口 (dev 字段)。fy add 时用 --serial 指定")?;
    let mut st = State::load();
    let pid = st.get_int(&format!("bb.{}", name), "pid") as i32;
    if pid > 0 && pid_alive(pid) {
        info(&format!("黑匣子已在运行 (pid {})", pid));
        return Ok(());
    }
    let _ = ensure_dir(&bb_dir());
    let _ = ensure_dir(&incidents_dir(name));
    let pid = spawn_daemon(
        &argv(&[
            &self_exe().display().to_string(),
            "__bbd",
            name,
            &port,
            &d.baud.to_string(),
        ]),
        &bb_dir().join(format!("{}.daemon.log", name)),
    )
    .map_err(|e| e.to_string())?;
    st.set_int(&format!("bb.{}", name), "pid", pid as i64);
    st.save();
    ok(&format!(
        "黑匣子已启动 (pid {})。录制: {}  事故: {}",
        pid,
        log_path(name).display(),
        incidents_dir(name).display()
    ));
    info("此后 fy sh 会自动经黑匣子共享串口，随开随关不打架。");
    Ok(())
}

pub fn stop(name: &str) {
    let mut st = State::load();
    let key = format!("bb.{}", name);
    let pid = st.get_int(&key, "pid") as i32;
    if pid > 0 && pid_alive(pid) {
        kill_pid(pid);
        ok(&format!("黑匣子已停止 (pid {})", pid));
    } else {
        info("黑匣子本来就没在跑");
    }
    st.drop_table(&key);
    st.save();
    let _ = std::fs::remove_file(sock_path(name));
}

pub fn status(cfg: &Config) {
    let st = State::load();
    let mut rows = vec![];
    for name in st.doc.children("bb") {
        let pid = st.get_int(&format!("bb.{}", name), "pid") as i32;
        let alive = pid_alive(pid);
        let sz = std::fs::metadata(log_path(&name)).map(|m| m.len()).unwrap_or(0);
        let n_inc = std::fs::read_dir(incidents_dir(&name)).map(|r| r.count()).unwrap_or(0);
        rows.push(vec![
            name.clone(),
            if alive { green(&format!("运行中 pid {}", pid)) } else { red("已死") },
            format!("{:.1} MB", sz as f64 / 1e6),
            format!("{} 起事故", n_inc),
        ]);
    }
    if rows.is_empty() {
        info("没有运行中的黑匣子。fy bb start <串口设备> 开一个。");
        let serials: Vec<String> = cfg
            .devices
            .values()
            .filter(|d| d.dev.is_some())
            .map(|d| d.name.clone())
            .collect();
        if !serials.is_empty() {
            info(&format!("有串口的设备: {}", serials.join(", ")));
        }
        return;
    }
    print_table(&["设备", "状态", "录制大小", "事故"], &rows);
}

/// 黑匣子在跑吗？（fy sh 靠它决定直连还是 attach）
pub fn running_for(name: &str) -> bool {
    let st = State::load();
    let pid = st.get_int(&format!("bb.{}", name), "pid") as i32;
    pid > 0 && pid_alive(pid) && sock_path(name).exists()
}

/// attach 到黑匣子（成为串口的前台使用者）。
pub fn attach(name: &str) -> std::io::Result<()> {
    let s = UnixStream::connect(sock_path(name))?;
    let r = s.try_clone()?;
    info(&format!("经黑匣子共享串口（后台持续录制中），Ctrl-] 退出"));
    serialx::pump_console(r, s, None)
}

/// `fy blame <dev>`：最近一次事故现场（没有事故就给录制尾巴）。
pub fn blame(name: &str, lines: usize) {
    let dir = incidents_dir(name);
    let mut incidents: Vec<PathBuf> = std::fs::read_dir(&dir)
        .map(|r| r.flatten().map(|e| e.path()).collect())
        .unwrap_or_default();
    incidents.sort();
    if let Some(last) = incidents.last() {
        println!(
            "{} {}",
            bold("最近事故:"),
            last.file_name().unwrap_or_default().to_string_lossy()
        );
        print!("{}", slurp(last));
        return;
    }
    let log = slurp(&log_path(name));
    if log.is_empty() {
        warn("黑匣子还没有录到任何东西（fy bb start 开启后台录制）");
        return;
    }
    println!("{}", bold(&format!("没有 panic 记录，给你录制尾部 {} 行:", lines)));
    let all: Vec<&str> = log.lines().collect();
    for l in all.iter().rev().take(lines).rev() {
        println!("{}", l);
    }
}

// ---------------- 守护进程本体 (fy __bbd) ----------------

pub fn daemon_main(name: &str, port: &str, baud: u32) -> ! {
    eprintln!("[bbd] start dev={} port={} baud={}", name, port, baud);
    let client: Arc<Mutex<Option<UnixStream>>> = Arc::new(Mutex::new(None));
    let stop_flag = Arc::new(AtomicBool::new(false));

    // unix socket 接待 fy sh attach
    let spath = sock_path(name);
    let _ = std::fs::remove_file(&spath);
    let _ = ensure_dir(&bb_dir());
    let listener = match UnixListener::bind(&spath) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("[bbd] socket bind failed: {}", e);
            std::process::exit(1);
        }
    };

    // 串口写端句柄由主循环维护；attach 客户端写来的数据经 channel 转发
    let (tx_up, rx_up) = std::sync::mpsc::channel::<Vec<u8>>();
    {
        let client = client.clone();
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                if let Ok(stream) = conn {
                    // 单前台使用者：新的顶掉旧的
                    let mut guard = client.lock().unwrap();
                    *guard = Some(stream.try_clone().unwrap());
                    drop(guard);
                    let tx = tx_up.clone();
                    let mut rs = stream;
                    std::thread::spawn(move || {
                        let mut buf = [0u8; 1024];
                        loop {
                            match rs.read(&mut buf) {
                                Ok(0) | Err(_) => break,
                                Ok(n) => {
                                    if tx.send(buf[..n].to_vec()).is_err() {
                                        break;
                                    }
                                }
                            }
                        }
                    });
                }
            }
        });
    }

    let mut ring: std::collections::VecDeque<String> = std::collections::VecDeque::with_capacity(RING_LINES);
    let mut linebuf = String::new();
    let mut last_incident: i64 = 0;

    loop {
        if stop_flag.load(Ordering::Relaxed) {
            std::process::exit(0);
        }
        // 打开串口（拔线/占用时重试）
        let file = match serialx::open_port(port, baud) {
            Ok(f) => f,
            Err(e) => {
                eprintln!("[bbd] open {} failed: {} (3s 后重试)", port, e);
                std::thread::sleep(Duration::from_secs(3));
                continue;
            }
        };
        eprintln!("[bbd] serial opened");
        let mut reader = file.try_clone().expect("clone");
        let mut writer = file;

        // 日志文件（超 8MB 轮转一份 .1）
        let lp = log_path(name);
        if std::fs::metadata(&lp).map(|m| m.len() > 8 * 1024 * 1024).unwrap_or(false) {
            let _ = std::fs::rename(&lp, bb_dir().join(format!("{}.log.1", name)));
        }
        let mut logf = std::fs::OpenOptions::new().create(true).append(true).open(&lp).ok();

        // 读串口线程 → 主循环
        let (tx_down, rx_down) = std::sync::mpsc::channel::<Vec<u8>>();
        let t_read = std::thread::spawn(move || {
            let mut buf = [0u8; 2048];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        if tx_down.send(buf[..n].to_vec()).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });

        // 主循环：下行数据分发（日志 + ring + panic 检测 + attach 客户端），上行转发
        'io: loop {
            // 上行（非阻塞尽量清空）
            while let Ok(data) = rx_up.try_recv() {
                if writer.write_all(&data).is_err() {
                    break 'io;
                }
                let _ = writer.flush();
            }
            match rx_down.recv_timeout(Duration::from_millis(100)) {
                Ok(data) => {
                    if let Some(f) = logf.as_mut() {
                        let _ = f.write_all(&data);
                    }
                    // 给 attach 的前台
                    {
                        let mut guard = client.lock().unwrap();
                        if let Some(c) = guard.as_mut() {
                            if c.write_all(&data).is_err() {
                                *guard = None;
                            }
                        }
                    }
                    // 行缓冲 + panic 检测
                    for ch in String::from_utf8_lossy(&data).chars() {
                        if ch == '\n' {
                            let line = std::mem::take(&mut linebuf);
                            if ring.len() >= RING_LINES {
                                ring.pop_front();
                            }
                            ring.push_back(line.clone());
                            let hit = PANIC_PATTERNS.iter().find(|p| line.contains(*p));
                            if let Some(p) = hit {
                                let now = now_epoch();
                                if now - last_incident > COOLDOWN_SECS {
                                    last_incident = now;
                                    save_incident(name, p, &ring);
                                }
                            }
                        } else if ch != '\r' {
                            linebuf.push(ch);
                            if linebuf.len() > 4096 {
                                linebuf.clear();
                            }
                        }
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    if t_read.is_finished() {
                        break 'io;
                    }
                }
                Err(_) => break 'io,
            }
        }
        eprintln!("[bbd] serial closed, reopen in 2s");
        std::thread::sleep(Duration::from_secs(2));
    }
}

fn save_incident(name: &str, pattern: &str, ring: &std::collections::VecDeque<String>) {
    let ts = {
        // 用 date 命令拿本地时间戳，避免自己实现日历
        let o = std::process::Command::new("date").arg("+%Y%m%d-%H%M%S").output();
        o.map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_else(|_| now_epoch().to_string())
    };
    let path = incidents_dir(name).join(format!("{}.log", ts));
    let _ = ensure_dir(&incidents_dir(name));
    let mut content = format!("# ferry blackbox incident\n# 设备: {}\n# 命中: {}\n# 时间: {}\n\n", name, pattern, ts);
    for l in ring {
        content.push_str(l);
        content.push('\n');
    }
    let _ = std::fs::write(&path, content);
    eprintln!("[bbd] INCIDENT {} -> {}", pattern, path.display());
    notify(
        &format!("ferry 黑匣子: {} 出事了", name),
        &format!("{} — 现场已保存，fy blame {} 查看", pattern, name),
    );
}

/// 串口设备的 sh 入口：黑匣子在跑就 attach，否则直连。
pub fn serial_shell(cfg: &Config, name: &str) -> std::io::Result<()> {
    let d = cfg.devices.get(name).unwrap();
    if running_for(name) {
        return attach(name);
    }
    let port = d.dev.clone().ok_or_else(|| std::io::Error::new(std::io::ErrorKind::Other, "档案缺串口路径"))?;
    let _ = Transport::Serial;
    serialx::console(&port, d.baud, None)
}
