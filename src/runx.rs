//! `fy run`：交叉编译产物一键上板执行（push + chmod + 运行 + 回传退出码）。
//! `fy debug`：gdbserver 一条龙（起服务 + 端口转发 + 给出 gdb 连接命令）。

use crate::adbx;
use crate::config::{Config, Device, Transport};
use crate::fwd;
use crate::sshx;
use crate::util::*;
use std::path::Path;

fn remote_bin_path(d: &Device, local: &Path) -> String {
    let base = local.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| "a.out".into());
    format!("{}/{}", d.dest.trim_end_matches('/'), base)
}

pub fn run(cfg: &Config, d: &Device, local: &Path, args: &[String], tty: bool) -> std::io::Result<i32> {
    if !local.exists() && !dry() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("找不到 {}（先交叉编译出来）", local.display()),
        ));
    }
    let rpath = remote_bin_path(d, local);
    info(&format!("推送 {} → {}:{}", local.display(), d.name, rpath));
    let pushed = match d.transport {
        Transport::Ssh => sshx::push(d, local, &d.dest)?,
        Transport::Adb => adbx::push(d, local, &rpath)?,
        Transport::Serial => {
            return Err(std::io::Error::new(std::io::ErrorKind::Other, "串口推不了文件，先 fy up"))
        }
    };
    if !pushed && !dry() {
        return Err(std::io::Error::new(std::io::ErrorKind::Other, "推送失败"));
    }
    let argstr = args.iter().map(|a| shell_quote(a)).collect::<Vec<_>>().join(" ");
    let cmd = format!("chmod +x {p} && {p} {a}", p = shell_quote(&rpath), a = argstr);
    info(&format!("运行: {} {}", rpath, argstr));
    let code = match d.transport {
        Transport::Ssh => sshx::exec_inherit(d, &cmd, tty)?,
        Transport::Adb => adbx::exec_inherit(d, &cmd, tty)?,
        Transport::Serial => unreachable!(),
    };
    if code == 0 {
        ok("退出码 0");
    } else {
        warn(&format!("退出码 {}", code));
    }
    let _ = cfg;
    Ok(code)
}

pub fn debug(cfg: &Config, d: &Device, local: &Path, args: &[String], port: u16) -> std::io::Result<i32> {
    // 板上有 gdbserver 吗
    let probe = "command -v gdbserver >/dev/null 2>&1 && echo yes || echo no";
    let has = match d.transport {
        Transport::Ssh => sshx::exec_capture(d, probe)?.stdout.contains("yes"),
        Transport::Adb => adbx::exec_capture(d, probe)?.stdout.contains("yes"),
        Transport::Serial => false,
    };
    if !has && !dry() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            "板上没有 gdbserver。把交叉工具链里的 gdbserver push 上去（fy push <dev> gdbserver /usr/bin/）",
        ));
    }
    let rpath = remote_bin_path(d, local);
    if local.exists() {
        info(&format!("推送 {} → {}", local.display(), rpath));
        match d.transport {
            Transport::Ssh => {
                sshx::push(d, local, &d.dest)?;
            }
            Transport::Adb => {
                adbx::push(d, local, &rpath)?;
            }
            Transport::Serial => {}
        }
    }
    // 端口转发：本机 port → 板 port
    let spec = format!("{}:{}", port, port);
    if let Err(e) = fwd::add(cfg, d, &spec) {
        warn(&format!("转发建立失败（可能已存在）: {}", e));
    }
    let argstr = args.iter().map(|a| shell_quote(a)).collect::<Vec<_>>().join(" ");
    println!();
    println!("{}", bold("另开一个终端，用交叉 gdb 连接:"));
    println!(
        "  {}",
        cyan(&format!(
            "gdb {} -ex 'target remote 127.0.0.1:{}' -ex 'set sysroot .'",
            local.display(),
            port
        ))
    );
    println!();
    info("gdbserver 前台运行中，Ctrl-C 结束调试");
    let cmd = format!("chmod +x {p} 2>/dev/null; gdbserver :{port} {p} {a}", p = shell_quote(&rpath), port = port, a = argstr);
    let code = match d.transport {
        Transport::Ssh => sshx::exec_inherit(d, &cmd, true)?,
        Transport::Adb => adbx::exec_inherit(d, &cmd, true)?,
        Transport::Serial => unreachable!(),
    };
    Ok(code)
}
