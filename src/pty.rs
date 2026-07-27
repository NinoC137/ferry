//! 零依赖 PTY：直接 extern "C" 调 libc（libc 本就被链接，不引入任何 crate）。
//! 起一个真伪终端跑用户的 shell，供 GUI 的"系统终端"用。支持窗口大小调整。

#![allow(non_camel_case_types)]

use std::fs::File;
use std::os::unix::io::{FromRawFd, RawFd};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};

type c_int = i32;
type c_char = i8;

extern "C" {
    fn posix_openpt(flags: c_int) -> c_int;
    fn grantpt(fd: c_int) -> c_int;
    fn unlockpt(fd: c_int) -> c_int;
    fn ptsname(fd: c_int) -> *mut c_char;
    fn setsid() -> c_int;
    fn ioctl(fd: c_int, req: u64, ...) -> c_int;
    fn close(fd: c_int) -> c_int;
}

#[cfg(target_os = "linux")]
mod sys {
    pub const O_RDWR: super::c_int = 0x2;
    pub const O_NOCTTY: super::c_int = 0x100;
    pub const TIOCSCTTY: u64 = 0x540E;
    pub const TIOCSWINSZ: u64 = 0x5414;
}
#[cfg(target_os = "macos")]
mod sys {
    pub const O_RDWR: super::c_int = 0x2;
    pub const O_NOCTTY: super::c_int = 0x20000;
    pub const TIOCSCTTY: u64 = 0x20007461;
    pub const TIOCSWINSZ: u64 = 0x80087467;
}

#[repr(C)]
struct Winsize {
    row: u16,
    col: u16,
    xpixel: u16,
    ypixel: u16,
}

/// 一个 PTY 会话：master 端可读写，slave 上跑着 shell。
pub struct Pty {
    master: File,
    master_fd: RawFd,
    child: Child,
}

impl Pty {
    /// 起一条 PTY，运行给定程序（通常是登录 shell）。
    pub fn spawn(program: &str, args: &[&str], rows: u16, cols: u16, env: &[(String, String)]) -> std::io::Result<Pty> {
        unsafe {
            let m = posix_openpt(sys::O_RDWR | sys::O_NOCTTY);
            if m < 0 {
                return Err(std::io::Error::last_os_error());
            }
            if grantpt(m) != 0 || unlockpt(m) != 0 {
                close(m);
                return Err(std::io::Error::new(std::io::ErrorKind::Other, "grantpt/unlockpt 失败"));
            }
            let name_ptr = ptsname(m);
            if name_ptr.is_null() {
                close(m);
                return Err(std::io::Error::new(std::io::ErrorKind::Other, "ptsname 失败"));
            }
            let mut len = 0usize;
            while *name_ptr.add(len) != 0 {
                len += 1;
            }
            let sname = String::from_utf8_lossy(std::slice::from_raw_parts(name_ptr as *const u8, len)).into_owned();

            // 初始窗口大小
            let ws = Winsize { row: rows.max(1), col: cols.max(1), xpixel: 0, ypixel: 0 };
            ioctl(m, sys::TIOCSWINSZ, &ws as *const _);

            // 每个 stdio 一份独立 owned fd（try_clone 各自 dup），交给 Command；
            // 满足 Rust IO 安全（不手搓 raw fd 的重复关闭）。
            let slave = std::fs::OpenOptions::new().read(true).write(true).open(&sname)?;
            let s_in = slave.try_clone()?;
            let s_out = slave.try_clone()?;
            let s_err = slave.try_clone()?;

            let mut cmd = Command::new(program);
            cmd.args(args);
            cmd.stdin(Stdio::from(s_in));
            cmd.stdout(Stdio::from(s_out));
            cmd.stderr(Stdio::from(s_err));
            for (k, v) in env {
                cmd.env(k, v);
            }
            // 让 shell 成为新会话首领并把 slave(此时已是 fd0) 设为控制终端
            cmd.pre_exec(move || {
                setsid();
                ioctl(0, sys::TIOCSCTTY, 0u64);
                Ok(())
            });
            let child = match cmd.spawn() {
                Ok(c) => c,
                Err(e) => {
                    close(m);
                    return Err(e);
                }
            };
            drop(slave); // 父进程不再需要 slave（子进程已持有各自的 dup）

            let master = File::from_raw_fd(m);
            Ok(Pty { master, master_fd: m, child })
        }
    }

    /// 用户默认 shell 起一个交互登录会话。
    pub fn spawn_shell(rows: u16, cols: u16, extra_env: &[(String, String)]) -> std::io::Result<Pty> {
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".into());
        let mut env: Vec<(String, String)> = vec![
            ("TERM".into(), "xterm-256color".into()),
            ("FERRY_UI".into(), "1".into()),
        ];
        env.extend_from_slice(extra_env);
        // -i 交互；bash/zsh 都认
        Pty::spawn(&shell, &["-i"], rows, cols, &env)
    }

    /// 克隆一个 master 读句柄（供读线程）。
    pub fn reader(&self) -> std::io::Result<File> {
        self.master.try_clone()
    }
    /// 克隆一个 master 写句柄（供写线程）。
    pub fn writer(&self) -> std::io::Result<File> {
        self.master.try_clone()
    }

    /// 调整终端窗口大小（xterm.js resize 时调用）。
    pub fn resize(&self, rows: u16, cols: u16) {
        let ws = Winsize { row: rows.max(1), col: cols.max(1), xpixel: 0, ypixel: 0 };
        unsafe {
            ioctl(self.master_fd, sys::TIOCSWINSZ, &ws as *const _);
        }
    }

    pub fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Pty {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
