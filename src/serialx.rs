//! 串口：零依赖实现。端口参数用 stty 配置，读写用普通文件句柄，
//! 本地终端 raw 模式也走 stty（-g 保存/恢复）。
//! 提供交互 console、expect 引擎（fy up 自动登录用）、黑匣子 attach。

use crate::util::*;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::Path;
use std::process::Command;
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// 枚举候选串口设备。
pub fn serial_ports() -> Vec<String> {
    let mut out: Vec<String> = vec![];
    #[cfg(target_os = "macos")]
    let pats: &[&str] = &["cu.usbserial", "cu.usbmodem", "cu.wchusbserial", "cu.SLAB", "cu.PL2303", "cu."];
    #[cfg(not(target_os = "macos"))]
    let pats: &[&str] = &["ttyUSB", "ttyACM"];
    if let Ok(rd) = std::fs::read_dir("/dev") {
        let mut names: Vec<String> = rd
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        names.sort();
        // 按 pats 的优先级排列，避免 mac 上蓝牙口排前面
        for p in pats {
            for n in &names {
                if n.starts_with(p) {
                    let full = format!("/dev/{}", n);
                    if !out.contains(&full) && !n.contains("Bluetooth") && !n.contains("debug-console") {
                        out.push(full);
                    }
                }
            }
        }
    }
    out
}

/// 用 stty 配置串口：raw、8N1、指定波特率、无流控。
pub fn configure(dev: &str, baud: u32) -> std::io::Result<()> {
    #[cfg(target_os = "macos")]
    let flag = "-f";
    #[cfg(not(target_os = "macos"))]
    let flag = "-F";
    let a = argv(&[
        "stty", flag, dev,
        &baud.to_string(),
        "raw", "-echo", "-echoe", "-echok",
        "cs8", "-cstopb", "-parenb", "-crtscts", "clocal",
    ]);
    let out = run_capture(&a, &[])?;
    if out.status != 0 && !dry() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            format!("stty 配置串口失败: {}", out.stderr.trim()),
        ));
    }
    Ok(())
}

pub fn open_port(dev: &str, baud: u32) -> std::io::Result<File> {
    configure(dev, baud)?;
    OpenOptions::new().read(true).write(true).open(dev)
}

// ---------------- 本地终端 raw ----------------

pub struct RawTty {
    saved: Option<String>,
}

impl RawTty {
    /// 把本地终端切到 raw（stty -g 先存档）。
    pub fn enter() -> RawTty {
        if dry() || !is_tty(0) {
            return RawTty { saved: None };
        }
        let saved = Command::new("stty")
            .arg("-g")
            .stdin(std::process::Stdio::inherit())
            .output()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());
        let _ = Command::new("stty")
            .args(["raw", "-echo"])
            .stdin(std::process::Stdio::inherit())
            .status();
        RawTty { saved }
    }
}

impl Drop for RawTty {
    fn drop(&mut self) {
        if let Some(s) = &self.saved {
            let _ = Command::new("stty").arg(s).stdin(std::process::Stdio::inherit()).status();
        }
    }
}

pub const ESCAPE_BYTE: u8 = 0x1d; // Ctrl-]

/// 双向泵：本地 stdin/stdout ↔ (reader, writer)。Ctrl-] 退出。
/// 可选把下行数据 tee 到日志文件。
pub fn pump_console<R, W>(mut from_dev: R, mut to_dev: W, log: Option<&Path>) -> std::io::Result<()>
where
    R: Read + Send + 'static,
    W: Write + Send + 'static,
{
    eprintln!("{}", dim("[已连接] Ctrl-] 退出"));
    let mut logf = match log {
        Some(p) => Some(OpenOptions::new().create(true).append(true).open(p)?),
        None => None,
    };
    let _raw = RawTty::enter();
    let (tx_done, rx_done) = mpsc::channel::<&'static str>();

    // 下行：设备 → 屏幕(+日志)
    let tx1 = tx_done.clone();
    let down = std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        let mut out = std::io::stdout();
        loop {
            match from_dev.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    let _ = out.write_all(&buf[..n]);
                    let _ = out.flush();
                    if let Some(f) = logf.as_mut() {
                        let _ = f.write_all(&buf[..n]);
                    }
                }
                Err(_) => break,
            }
        }
        let _ = tx1.send("device closed");
    });

    // 上行：键盘 → 设备（拦截 Ctrl-]）
    let up = std::thread::spawn(move || {
        let mut stdin = std::io::stdin();
        let mut b = [0u8; 1024];
        loop {
            match stdin.read(&mut b) {
                Ok(0) => break,
                Ok(n) => {
                    if let Some(pos) = b[..n].iter().position(|&c| c == ESCAPE_BYTE) {
                        let _ = to_dev.write_all(&b[..pos]);
                        break;
                    }
                    if to_dev.write_all(&b[..n]).is_err() {
                        break;
                    }
                    let _ = to_dev.flush();
                }
                Err(_) => break,
            }
        }
        let _ = tx_done.send("detached");
    });

    let why = rx_done.recv().unwrap_or("closed");
    // raw 复原由 Drop 负责；直接退出进程避免卡在残留的阻塞 read 上
    drop(_raw);
    eprintln!("\r\n{}", dim(&format!("[断开: {}]", why)));
    let _ = (down, up); // 线程随进程退出回收
    std::process::exit(0);
}

/// 直连串口 console。
pub fn console(dev: &str, baud: u32, log: Option<&Path>) -> std::io::Result<()> {
    if dry() {
        println!("{} serial console {} @{} (Ctrl-] 退出)", magenta("DRY→"), dev, baud);
        return Ok(());
    }
    let port = open_port(dev, baud)?;
    let port_w = port.try_clone()?;
    pump_console(port, port_w, log)
}

// ---------------- expect 引擎 ----------------

/// 对任意 Read 端做"等待模式串"的小引擎，fy up 的串口自动登录靠它。
pub struct Expecter {
    rx: mpsc::Receiver<Vec<u8>>,
    pub transcript: String, // 全程记录（供诊断/黑匣子/从标记处匹配）
}

impl Expecter {
    pub fn new<R: Read + Send + 'static>(mut reader: R) -> Expecter {
        let (tx, rx) = mpsc::channel::<Vec<u8>>();
        std::thread::spawn(move || {
            let mut buf = [0u8; 1024];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        if tx.send(buf[..n].to_vec()).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });
        Expecter { rx, transcript: String::new() }
    }

    fn ingest(&mut self, data: &[u8]) {
        self.transcript.push_str(&String::from_utf8_lossy(data));
    }

    /// 当前记录长度，作为"从此刻之后"的匹配起点。
    pub fn mark(&self) -> usize {
        self.transcript.len()
    }

    /// 只匹配 `from` 之后到达的数据 —— 避免历史回显/旧提示符造成误命中。
    pub fn expect_from(&mut self, from: usize, patterns: &[&str], timeout: Duration) -> Option<usize> {
        let deadline = Instant::now() + timeout;
        let from = from.min(self.transcript.len());
        loop {
            let low = self.transcript[from..].to_lowercase();
            for (i, p) in patterns.iter().enumerate() {
                if low.contains(&p.to_lowercase()) {
                    return Some(i);
                }
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return None;
            }
            match self.rx.recv_timeout(left.min(Duration::from_millis(150))) {
                Ok(data) => self.ingest(&data),
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(_) => return None,
            }
        }
    }

    /// 从当前时刻起等待模式（等价 expect_from(mark(), ...)）。
    #[allow(dead_code)]
    pub fn expect(&mut self, patterns: &[&str], timeout: Duration) -> Option<usize> {
        let m = self.mark();
        self.expect_from(m, patterns, timeout)
    }

    /// 静默收集一段时间的输出（返回新增部分）。
    pub fn drain(&mut self, d: Duration) -> String {
        let deadline = Instant::now() + d;
        let start = self.transcript.len();
        while let Some(left) = deadline.checked_duration_since(Instant::now()) {
            if left.is_zero() {
                break;
            }
            match self.rx.recv_timeout(left.min(Duration::from_millis(100))) {
                Ok(data) => self.ingest(&data),
                Err(mpsc::RecvTimeoutError::Timeout) => break,
                Err(_) => break,
            }
        }
        self.transcript[start..].to_string()
    }
}

/// 便捷：向 writer 发一行（自动 \n… 串口世界普遍吃 \n；uboot 也认）。
pub fn send_line<W: Write>(w: &mut W, line: &str) -> std::io::Result<()> {
    w.write_all(line.as_bytes())?;
    w.write_all(b"\n")?;
    w.flush()
}
