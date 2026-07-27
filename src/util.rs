//! 通用工具：彩色输出 / 表格 / 命令执行(含 dry-run) / 路径 / 提示交互 / 守护进程。

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};

pub static DRY: AtomicBool = AtomicBool::new(false);
pub static PLAIN: AtomicBool = AtomicBool::new(false);
pub static QUIET: AtomicBool = AtomicBool::new(false);
/// 批量并行传输时把进度条关掉——十几个进度条抢同一行只会糊成一片。
pub static NOPROG: AtomicBool = AtomicBool::new(false);

pub fn dry() -> bool {
    DRY.load(Ordering::Relaxed)
}

fn use_color() -> bool {
    if PLAIN.load(Ordering::Relaxed) || std::env::var_os("NO_COLOR").is_some() {
        return false;
    }
    is_tty(1)
}

/// 判断 fd 是否是终端。`test -t` 是 POSIX sh 内建，一次进程开销；
/// 但上色时每个字符串都要问一次，所以结果按 fd 缓存（进程生命周期内不会变）。
pub fn is_tty(fd: i32) -> bool {
    use std::sync::OnceLock;
    static CACHE: OnceLock<[bool; 3]> = OnceLock::new();
    let probe = |fd: i32| {
        Command::new("/bin/sh")
            .arg("-c")
            .arg(format!("test -t {}", fd))
            .stdin(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    };
    let c = CACHE.get_or_init(|| [probe(0), probe(1), probe(2)]);
    match fd {
        0..=2 => c[fd as usize],
        _ => probe(fd),
    }
}

fn paint(code: &str, s: &str) -> String {
    if use_color() {
        format!("\x1b[{}m{}\x1b[0m", code, s)
    } else {
        s.to_string()
    }
}

pub fn bold(s: &str) -> String { paint("1", s) }
pub fn dim(s: &str) -> String { paint("2", s) }
pub fn red(s: &str) -> String { paint("31", s) }
pub fn green(s: &str) -> String { paint("32", s) }
pub fn yellow(s: &str) -> String { paint("33", s) }
pub fn blue(s: &str) -> String { paint("34", s) }
pub fn magenta(s: &str) -> String { paint("35", s) }
pub fn cyan(s: &str) -> String { paint("36", s) }

pub fn info(msg: &str) {
    if !QUIET.load(Ordering::Relaxed) {
        eprintln!("{} {}", cyan("::"), msg);
    }
}
pub fn ok(msg: &str) {
    if !QUIET.load(Ordering::Relaxed) {
        eprintln!("{} {}", green("ok"), msg);
    }
}
pub fn warn(msg: &str) {
    eprintln!("{} {}", yellow("!!"), msg);
}
pub fn err(msg: &str) {
    eprintln!("{} {}", red("xx"), msg);
}

/// 终端显示宽度（CJK 记 2）。
pub fn disp_width(s: &str) -> usize {
    s.chars().map(cwidth).sum()
}
fn cwidth(c: char) -> usize {
    let u = c as u32;
    if (0x1100..=0x115F).contains(&u)
        || (0x2E80..=0xA4CF).contains(&u)
        || (0xAC00..=0xD7A3).contains(&u)
        || (0xF900..=0xFAFF).contains(&u)
        || (0xFE30..=0xFE4F).contains(&u)
        || (0xFF00..=0xFF60).contains(&u)
        || (0xFFE0..=0xFFE6).contains(&u)
        || (0x20000..=0x3FFFD).contains(&u)
    {
        2
    } else {
        1
    }
}

/// 简易表格：列自动对齐（考虑 CJK 宽度与 ANSI 颜色码）。
pub fn print_table(header: &[&str], rows: &[Vec<String>]) {
    let strip_ansi = |s: &str| -> String {
        let mut out = String::new();
        let mut in_esc = false;
        for c in s.chars() {
            if in_esc {
                if c == 'm' {
                    in_esc = false;
                }
            } else if c == '\x1b' {
                in_esc = true;
            } else {
                out.push(c);
            }
        }
        out
    };
    let ncol = header.len();
    let mut w = vec![0usize; ncol];
    for (i, h) in header.iter().enumerate() {
        w[i] = w[i].max(disp_width(h));
    }
    for r in rows {
        for (i, cell) in r.iter().enumerate().take(ncol) {
            w[i] = w[i].max(disp_width(&strip_ansi(cell)));
        }
    }
    let pad = |s: &str, width: usize| -> String {
        let vis = disp_width(&strip_ansi(s));
        let mut out = s.to_string();
        for _ in vis..width {
            out.push(' ');
        }
        out
    };
    let head: Vec<String> = header.iter().enumerate().map(|(i, h)| pad(&bold(h), w[i])).collect();
    println!("{}", head.join("  "));
    for r in rows {
        let line: Vec<String> = r.iter().enumerate().map(|(i, c)| pad(c, w[i])).collect();
        println!("{}", line.join("  "));
    }
}

// ---------------- 命令执行 ----------------

pub fn shell_quote(s: &str) -> String {
    if !s.is_empty() && s.chars().all(|c| c.is_alphanumeric() || "_-./=:@,+%".contains(c)) {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

pub fn render_cmd(argv: &[String]) -> String {
    argv.iter().map(|a| shell_quote(a)).collect::<Vec<_>>().join(" ")
}

pub fn announce(argv: &[String]) {
    if dry() {
        // JSON 模式下 stdout 只许有一份 JSON 文档，dry 回显改走 stderr
        if crate::jsonout::json_mode() {
            eprintln!("{} {}", magenta("DRY→"), render_cmd(argv));
        } else {
            println!("{} {}", magenta("DRY→"), render_cmd(argv));
        }
    } else if !QUIET.load(Ordering::Relaxed) {
        eprintln!("{} {}", dim("→"), dim(&render_cmd(argv)));
    }
}

pub struct Output {
    pub status: i32,
    pub stdout: String,
    pub stderr: String,
}

/// 运行并捕获输出。dry-run 下只打印不执行（返回成功空输出）。
pub fn run_capture(argv: &[String], envs: &[(String, String)]) -> io::Result<Output> {
    announce(argv);
    if dry() {
        return Ok(Output { status: 0, stdout: String::new(), stderr: String::new() });
    }
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..]);
    for (k, v) in envs {
        cmd.env(k, v);
    }
    let out = cmd.stdin(Stdio::null()).output()?;
    Ok(Output {
        status: out.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    })
}

/// 运行，stdio 直通终端（交互/流式输出）。返回退出码。
pub fn run_inherit(argv: &[String], envs: &[(String, String)]) -> io::Result<i32> {
    announce(argv);
    if dry() {
        return Ok(0);
    }
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..]);
    for (k, v) in envs {
        cmd.env(k, v);
    }
    let st = cmd.status()?;
    Ok(st.code().unwrap_or(-1))
}

/// exec 替换当前进程（交互 shell 用，保证 tty 语义原生）。dry-run 下仅打印。
pub fn run_exec(argv: &[String], envs: &[(String, String)]) -> io::Result<i32> {
    announce(argv);
    if dry() {
        return Ok(0);
    }
    use std::os::unix::process::CommandExt;
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..]);
    for (k, v) in envs {
        cmd.env(k, v);
    }
    let e = cmd.exec(); // 只有失败才返回
    Err(e)
}

/// 静默探测某命令是否存在。
pub fn which(bin: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let p = dir.join(bin);
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

pub fn argv(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| s.to_string()).collect()
}

// ---------------- 路径 ----------------

pub fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/tmp"))
}

/// 配置根目录 ~/.config/ferry（可用 FERRY_HOME 覆盖）。
pub fn cfg_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("FERRY_HOME") {
        return PathBuf::from(d);
    }
    home().join(".config").join("ferry")
}

pub fn ensure_dir(p: &Path) -> io::Result<()> {
    if !p.exists() {
        std::fs::create_dir_all(p)?;
        let _ = Command::new("chmod").arg("700").arg(p).status();
    }
    Ok(())
}

pub fn state_path() -> PathBuf { cfg_dir().join("state.toml") }
pub fn devices_path() -> PathBuf { cfg_dir().join("devices.toml") }
pub fn facts_dir() -> PathBuf { cfg_dir().join("facts") }
pub fn cm_dir() -> PathBuf { cfg_dir().join("cm") }
pub fn bb_dir() -> PathBuf { cfg_dir().join("bb") }
pub fn known_hosts() -> PathBuf { cfg_dir().join("known_hosts") }

// ---------------- 交互 ----------------

pub fn prompt(msg: &str) -> String {
    if !crate::jsonout::interactive() {
        return String::new();
    }
    eprint!("{} {} ", cyan("?"), msg);
    let _ = io::stderr().flush();
    let mut s = String::new();
    let _ = io::stdin().read_line(&mut s);
    s.trim().to_string()
}

/// 询问确认。**非交互模式（`--json` / `-y`）直接返回默认值，绝不阻塞。**
pub fn confirm(msg: &str, default_yes: bool) -> bool {
    if !crate::jsonout::interactive() {
        return default_yes;
    }
    let hint = if default_yes { "[Y/n]" } else { "[y/N]" };
    let a = prompt(&format!("{} {}", msg, hint));
    if a.is_empty() {
        default_yes
    } else {
        a.eq_ignore_ascii_case("y") || a.eq_ignore_ascii_case("yes")
    }
}

/// 从编号列表选一个。**非交互模式下返回 None**（调用方据此报 NEED_INPUT）。
/// 只有一项时无需询问，任何模式都直接选中它。
pub fn pick(msg: &str, items: &[String]) -> Option<usize> {
    if items.is_empty() {
        return None;
    }
    if items.len() == 1 {
        return Some(0);
    }
    if !crate::jsonout::interactive() {
        return None;
    }
    for (i, it) in items.iter().enumerate() {
        eprintln!("  {} {}", dim(&format!("{})", i + 1)), it);
    }
    let a = prompt(msg);
    a.parse::<usize>().ok().filter(|n| *n >= 1 && *n <= items.len()).map(|n| n - 1)
}

// ---------------- 杂项 ----------------

pub fn now_epoch() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub fn human_ago(epoch: i64) -> String {
    if epoch <= 0 {
        return "never".into();
    }
    let d = (now_epoch() - epoch).max(0);
    match d {
        0..=59 => format!("{}s ago", d),
        60..=3599 => format!("{}m ago", d / 60),
        3600..=86399 => format!("{}h ago", d / 3600),
        _ => format!("{}d ago", d / 86400),
    }
}

/// 桌面通知（macOS osascript / Linux notify-send），失败静默。
pub fn notify(title: &str, body: &str) {
    #[cfg(target_os = "macos")]
    {
        let script = format!(
            "display notification {} with title {}",
            osa_quote(body),
            osa_quote(title)
        );
        let _ = Command::new("osascript").arg("-e").arg(script).stdout(Stdio::null()).stderr(Stdio::null()).status();
    }
    #[cfg(not(target_os = "macos"))]
    {
        if which("notify-send").is_some() {
            let _ = Command::new("notify-send").arg(title).arg(body).stdout(Stdio::null()).stderr(Stdio::null()).status();
        }
    }
}

#[cfg(target_os = "macos")]
fn osa_quote(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// 后台拉起守护进程，返回其**真实** pid（供之后精确 kill）。
/// 直接 spawn 子进程（不经 nohup/shell，避免拿错 pid）；stdio 重定向到日志/空，
/// 用 setsid 脱离会话（拿不到 setsid 也无妨，父进程正常退出不会误杀子进程）。
pub fn spawn_daemon(argv: &[String], log: &Path) -> io::Result<i32> {
    announce(argv);
    if dry() {
        return Ok(0);
    }
    let logf = std::fs::OpenOptions::new().create(true).append(true).open(log)?;
    let logf2 = logf.try_clone()?;
    // setsid 存在就用它脱离控制终端（Linux 有；macOS 默认无，退回直接 spawn）
    let use_setsid = which("setsid").is_some();
    let mut cmd = if use_setsid {
        let mut c = Command::new("setsid");
        c.arg(&argv[0]).args(&argv[1..]);
        c
    } else {
        let mut c = Command::new(&argv[0]);
        c.args(&argv[1..]);
        c
    };
    cmd.stdin(Stdio::null()).stdout(Stdio::from(logf)).stderr(Stdio::from(logf2));
    let child = cmd.spawn()?;
    let pid = child.id() as i32;
    // 不 wait —— 让它继续在后台跑；std 不会在 drop 时杀子进程
    std::mem::forget(child);
    if pid <= 0 {
        return Err(io::Error::new(io::ErrorKind::Other, "daemon spawn failed"));
    }
    let _ = use_setsid;
    Ok(pid)
}

pub fn pid_alive(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    Command::new("/bin/sh")
        .arg("-c")
        .arg(format!("kill -0 {} 2>/dev/null", pid))
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

pub fn kill_pid(pid: i32) {
    if pid > 0 {
        let _ = Command::new("kill").arg(pid.to_string()).stdout(Stdio::null()).stderr(Stdio::null()).status();
    }
}

/// 读文件全文（不存在返回空串）。
pub fn slurp(p: &Path) -> String {
    std::fs::read_to_string(p).unwrap_or_default()
}

/// 当前可执行文件绝对路径。
pub fn self_exe() -> PathBuf {
    std::env::current_exe().unwrap_or_else(|_| PathBuf::from("fy"))
}


// ---------------- 人类可读的量 ----------------

pub fn human_bytes(n: u64) -> String {
    const U: [&str; 6] = ["B", "KB", "MB", "GB", "TB", "PB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i + 1 < U.len() {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{} {}", n, U[0])
    } else if v < 10.0 {
        format!("{:.2} {}", v, U[i])
    } else {
        format!("{:.1} {}", v, U[i])
    }
}

pub fn human_rate(bytes_per_sec: f64) -> String {
    if !bytes_per_sec.is_finite() || bytes_per_sec <= 0.0 {
        return "-".into();
    }
    format!("{}/s", human_bytes(bytes_per_sec as u64))
}

pub fn human_dur(secs: f64) -> String {
    if !secs.is_finite() || secs < 0.0 {
        return "-".into();
    }
    let s = secs.round() as u64;
    match s {
        0..=59 => format!("{}s", s),
        60..=3599 => format!("{}m{:02}s", s / 60, s % 60),
        _ => format!("{}h{:02}m", s / 3600, (s % 3600) / 60),
    }
}

// ---------------- 传输进度条 ----------------

/// 单行原地刷新的进度条。画在 **stderr**，所以 `fy pull dev /x - > file` 之类的
/// 管道用法不会被污染；非 tty / -q / --json / -n 时自动全程静默。
pub struct Progress {
    label: String,
    total: u64,
    done: u64,
    start: std::time::Instant,
    last_draw: std::time::Instant,
    enabled: bool,
    drawn: bool,
}

impl Progress {
    pub fn new(label: &str, total: u64) -> Progress {
        let enabled = !dry()
            && !NOPROG.load(Ordering::Relaxed)
            && !QUIET.load(Ordering::Relaxed)
            && !crate::jsonout::json_mode()
            && is_tty(2);
        Progress {
            label: label.to_string(),
            total,
            done: 0,
            start: std::time::Instant::now(),
            last_draw: std::time::Instant::now() - std::time::Duration::from_secs(1),
            enabled,
            drawn: false,
        }
    }

    pub fn add(&mut self, n: u64) {
        self.done = self.done.saturating_add(n);
        self.draw(false);
    }

    pub fn set(&mut self, n: u64) {
        self.done = n;
        self.draw(false);
    }

    pub fn elapsed(&self) -> f64 {
        self.start.elapsed().as_secs_f64()
    }

    pub fn rate(&self) -> f64 {
        let e = self.elapsed();
        if e > 0.0 {
            self.done as f64 / e
        } else {
            0.0
        }
    }

    fn draw(&mut self, force: bool) {
        if !self.enabled {
            return;
        }
        // 12 fps 足够顺滑，又不会把 CPU 和终端刷爆
        if !force && self.last_draw.elapsed() < std::time::Duration::from_millis(80) {
            return;
        }
        self.last_draw = std::time::Instant::now();
        let rate = self.rate();
        let bar_w = 24usize;
        let (pct, bar, eta) = if self.total > 0 {
            let frac = (self.done as f64 / self.total as f64).clamp(0.0, 1.0);
            let filled = (frac * bar_w as f64).round() as usize;
            let eta = if rate > 0.0 {
                human_dur((self.total.saturating_sub(self.done)) as f64 / rate)
            } else {
                "-".into()
            };
            (
                format!("{:>3.0}%", frac * 100.0),
                format!("{}{}", "█".repeat(filled), "░".repeat(bar_w - filled)),
                eta,
            )
        } else {
            ("".into(), "".into(), "".into())
        };
        let body = if self.total > 0 {
            format!(
                "{} {} {} {}/{} {} ETA {}",
                self.label,
                bar,
                pct,
                human_bytes(self.done),
                human_bytes(self.total),
                human_rate(rate),
                eta
            )
        } else {
            format!("{} {} {}", self.label, human_bytes(self.done), human_rate(rate))
        };
        eprint!("\r\x1b[2K{}", dim(&body));
        let _ = io::stderr().flush();
        self.drawn = true;
    }

    /// 收尾：把进度条那一行擦掉，交给调用方打最终的 ok(...) 汇总。
    pub fn finish(&mut self) {
        if self.enabled && self.drawn {
            eprint!("\r\x1b[2K");
            let _ = io::stderr().flush();
        }
    }
}

impl Drop for Progress {
    fn drop(&mut self) {
        self.finish();
    }
}

/// 弱随机十六进制串：给 `fy serve` 的 URL token、临时文件名用。
/// 优先 /dev/urandom；拿不到就退回时间+pid 混合（**不做密码学用途**）。
pub fn rand_hex(n: usize) -> String {
    let bytes = n.div_ceil(2);
    let mut buf = vec![0u8; bytes];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        use std::io::Read;
        if f.read_exact(&mut buf).is_ok() {
            return crate::hash::to_hex(&buf)[..n].to_string();
        }
    }
    let seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
        ^ ((std::process::id() as u64) << 32);
    let mut x = seed | 1;
    for b in buf.iter_mut() {
        // xorshift64：够随机到不会撞，也不需要更强
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        *b = (x >> 24) as u8;
    }
    crate::hash::to_hex(&buf)[..n].to_string()
}
