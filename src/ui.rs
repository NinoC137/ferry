//! `fy ui` —— 本地 Web GUI。
//! 主区是一个**真·系统终端**（PTY 跑用户 shell，xterm.js 经 WebSocket 双向流）；
//! 侧栏是便捷工具（设备列表/实时状态、快捷动作、转发、黑匣子、借网）。
//! 侧栏动作把命令"注入"到终端里执行，所见即所得。零依赖。

use crate::adbx;
use crate::config::{self, Config, Transport};
use crate::httpd::{self, Request};
use crate::pty::Pty;
use crate::util::*;
use crate::wsutil::{self, WsMsg};
use std::io::{BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const UI_HTML: &str = include_str!("../assets/ui.html");

pub fn run(port: u16, open: bool) -> std::io::Result<()> {
    let listener = TcpListener::bind(("127.0.0.1", port)).map_err(|e| {
        err(&format!("绑定 127.0.0.1:{} 失败（端口被占？换 --port）", port));
        e
    })?;
    let url = format!("http://127.0.0.1:{}", port);
    ok(&format!("ferry GUI 已启动: {}", cyan(&url)));
    info("主区是真实系统终端，侧栏是便捷工具。Ctrl-C 关闭。");
    if open {
        open_browser(&url);
    }
    httpd::serve(listener, move |req, stream| {
        if let Err(_e) = route(req, stream) {
            // 单连接错误静默（浏览器频繁开关连接很正常）
        }
    });
    Ok(())
}

fn route(req: Request, mut stream: TcpStream) -> std::io::Result<()> {
    match req.path.as_str() {
        "/" => httpd::ok_html(&mut stream, UI_HTML),
        "/pty" if req.is_websocket() => handle_terminal(req, stream),
        "/api/devices" => httpd::ok_json(&mut stream, &devices_json()),
        "/api/state" => httpd::ok_json(&mut stream, &state_json()),
        "/api/ping" => httpd::ok_json(&mut stream, "{\"ok\":true}"),
        _ => httpd::not_found(&mut stream),
    }
}

// ---------------- 终端 WebSocket 桥 ----------------

fn handle_terminal(req: Request, stream: TcpStream) -> std::io::Result<()> {
    let key = match req.header("sec-websocket-key") {
        Some(k) => k.to_string(),
        None => return Ok(()),
    };
    let accept = wsutil::ws_accept(&key);
    let mut ws = stream;
    let resp = format!(
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {}\r\n\r\n",
        accept
    );
    ws.write_all(resp.as_bytes())?;
    ws.flush()?;

    let rows: u16 = req.q("rows").and_then(|s| s.parse().ok()).unwrap_or(30);
    let cols: u16 = req.q("cols").and_then(|s| s.parse().ok()).unwrap_or(110);

    // 让终端里能直接敲 fy：把自身所在目录塞进 PATH
    let mut env = vec![];
    if let Some(dir) = self_exe().parent() {
        let path = std::env::var("PATH").unwrap_or_default();
        env.push(("PATH".to_string(), format!("{}:{}", dir.display(), path)));
    }
    let pty = match Pty::spawn_shell(rows, cols, &env) {
        Ok(p) => p,
        Err(e) => {
            let _ = wsutil::ws_write_text(&mut ws, &format!("\r\n[ferry] 打不开终端: {}\r\n", e));
            return Ok(());
        }
    };
    let pty = Arc::new(Mutex::new(pty));

    // 写向客户端的一端加锁共享（PTY→WS 线程 与 pong 都要用）
    let ws_out = Arc::new(Mutex::new(ws.try_clone()?));

    // PTY master → WS(binary)
    let mut pty_reader = pty.lock().unwrap().reader()?;
    let ws_out_r = ws_out.clone();
    let reader_thread = std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        loop {
            match std::io::Read::read(&mut pty_reader, &mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let mut o = ws_out_r.lock().unwrap();
                    if wsutil::ws_write_binary(&mut *o, &buf[..n]).is_err() {
                        break;
                    }
                }
            }
        }
        // shell 结束 → 通知前端
        if let Ok(mut o) = ws_out_r.lock() {
            let _ = wsutil::ws_write_text(&mut *o, "\r\n\x1b[2m[ferry] 终端会话已结束，刷新页面重开]\x1b[0m\r\n");
            let _ = wsutil::ws_write(&mut *o, 0x8, b""); // Close
        }
    });

    // WS → PTY master
    let mut pty_writer = pty.lock().unwrap().writer()?;
    let mut r = BufReader::new(ws);
    loop {
        match wsutil::ws_read(&mut r) {
            Ok(WsMsg::Binary(d)) => {
                if std::io::Write::write_all(&mut pty_writer, &d).is_err() {
                    break;
                }
                let _ = std::io::Write::flush(&mut pty_writer);
            }
            Ok(WsMsg::Text(t)) => {
                // 控制消息：{"t":"resize","rows":R,"cols":C} 或直接注入的按键
                let s = String::from_utf8_lossy(&t);
                if let Some((rr, cc)) = parse_resize(&s) {
                    pty.lock().unwrap().resize(rr, cc);
                } else {
                    let _ = std::io::Write::write_all(&mut pty_writer, t.as_slice());
                    let _ = std::io::Write::flush(&mut pty_writer);
                }
            }
            Ok(WsMsg::Ping(p)) => {
                if let Ok(mut o) = ws_out.lock() {
                    let _ = wsutil::ws_write(&mut *o, 0xA, &p);
                }
            }
            Ok(WsMsg::Close) | Err(_) => break,
            _ => {}
        }
    }
    pty.lock().unwrap().kill();
    let _ = reader_thread.join();
    Ok(())
}

fn parse_resize(s: &str) -> Option<(u16, u16)> {
    if !s.contains("resize") {
        return None;
    }
    let rows = extract_num(s, "rows")?;
    let cols = extract_num(s, "cols")?;
    Some((rows as u16, cols as u16))
}

fn extract_num(s: &str, key: &str) -> Option<u32> {
    let idx = s.find(&format!("\"{}\"", key))?;
    let after = &s[idx + key.len() + 2..];
    let colon = after.find(':')?;
    let rest = after[colon + 1..].trim_start();
    let end = rest.find(|c: char| !c.is_ascii_digit()).unwrap_or(rest.len());
    rest[..end].parse().ok()
}

// ---------------- REST: 侧栏数据 ----------------

fn probe(d: &config::Device) -> (bool, String) {
    match d.transport {
        Transport::Ssh => {
            let okk = format!("{}:{}", d.host, d.port)
                .parse()
                .ok()
                .map(|a| TcpStream::connect_timeout(&a, Duration::from_millis(400)).is_ok())
                .unwrap_or(false);
            (okk, if okk { "在线".into() } else { "不可达".into() })
        }
        Transport::Adb => adbx::probe(d),
        Transport::Serial => {
            let exists = d.dev.as_ref().map(|p| std::path::Path::new(p).exists()).unwrap_or(false);
            let bb = crate::blackbox::running_for(&d.name);
            (exists, if bb { "在线+黑匣子".into() } else if exists { "在线".into() } else { "没插".into() })
        }
    }
}

fn devices_json() -> String {
    let cfg = Config::load();
    // 并行探活
    let devs: Vec<config::Device> = cfg.devices.values().cloned().collect();
    let handles: Vec<_> = devs
        .iter()
        .map(|d| {
            let d = d.clone();
            std::thread::spawn(move || (d.name.clone(), probe(&d)))
        })
        .collect();
    let mut status = std::collections::HashMap::new();
    for h in handles {
        if let Ok((name, st)) = h.join() {
            status.insert(name, st);
        }
    }
    let mut items = vec![];
    for d in &devs {
        let f = config::facts_load(&d.name);
        let (online, why) = status.get(&d.name).cloned().unwrap_or((false, "?".into()));
        let ident = if !f.hostname.is_empty() { f.hostname.clone() } else { f.os.clone() };
        items.push(format!(
            "{{\"name\":{},\"transport\":{},\"endpoint\":{},\"online\":{},\"why\":{},\"ident\":{},\"kernel\":{},\"arch\":{},\"mac\":{},\"last_ip\":{},\"last_seen\":{}}}",
            js(&d.name),
            js(d.transport.as_str()),
            js(&d.endpoint()),
            online,
            js(&why),
            js(&ident),
            js(&f.kernel),
            js(&f.arch),
            js(&f.macs.join(", ")),
            js(&f.last_ip),
            f.last_seen
        ));
    }
    format!("{{\"devices\":[{}]}}", items.join(","))
}

fn state_json() -> String {
    let cfg = Config::load();
    let st = config::State::load();
    // 转发
    let mut fwds = vec![];
    for f in st.forwards() {
        let spec = crate::fwd::Spec::parse(&f.spec).map(|s| s.human()).unwrap_or(f.spec.clone());
        fwds.push(format!(
            "{{\"id\":{},\"dev\":{},\"spec\":{}}}",
            js(&f.id),
            js(&f.dev),
            js(&spec)
        ));
    }
    // 黑匣子
    let mut bbs = vec![];
    for name in st.doc.children("bb") {
        let pid = st.get_int(&format!("bb.{}", name), "pid") as i32;
        let alive = pid_alive(pid);
        let n_inc = std::fs::read_dir(crate::blackbox::incidents_dir(&name)).map(|r| r.count()).unwrap_or(0);
        bbs.push(format!(
            "{{\"dev\":{},\"alive\":{},\"incidents\":{}}}",
            js(&name),
            alive,
            n_inc
        ));
    }
    // 借网
    let mut shares = vec![];
    for name in st.doc.children("share") {
        let mode = st.get_str(&format!("share.{}", name), "mode");
        shares.push(format!("{{\"dev\":{},\"mode\":{}}}", js(&name), js(&mode)));
    }
    let _ = cfg;
    format!(
        "{{\"forwards\":[{}],\"blackboxes\":[{}],\"shares\":[{}]}}",
        fwds.join(","),
        bbs.join(","),
        shares.join(",")
    )
}

/// 最小 JSON 字符串转义。
fn js(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn open_browser(url: &str) {
    #[cfg(target_os = "macos")]
    let cmd = "open";
    #[cfg(not(target_os = "macos"))]
    let cmd = "xdg-open";
    if which(cmd).is_some() {
        let _ = std::process::Command::new(cmd)
            .arg(url)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufReader, Read, Write};
    use std::time::Instant;

    /// 起真服务 + 真 WebSocket + 真 PTY，全程 loopback，一个前台进程内跑完。
    #[test]
    fn end_to_end_terminal_and_api() {
        // 独立配置目录 + 两台设备
        let home = std::env::temp_dir().join(format!("ferry_ui_test_{}", std::process::id()));
        std::env::set_var("FERRY_HOME", &home);
        std::env::set_var("SHELL", "/bin/bash");
        let _ = std::fs::remove_dir_all(&home);
        let mut cfg = Config::load();
        let mut rk = config::Device::new("rk", Transport::Ssh);
        rk.host = "127.0.0.1".into();
        rk.port = 2222;
        cfg.devices.insert("rk".into(), rk);
        let mut mcu = config::Device::new("mcu", Transport::Serial);
        mcu.dev = Some("/dev/ttyUSB0".into());
        cfg.devices.insert("mcu".into(), mcu);
        cfg.save().unwrap();

        // 起服务（临时端口）
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            httpd::serve(listener, move |req, stream| {
                let _ = route(req, stream);
            });
        });
        std::thread::sleep(Duration::from_millis(200));

        // --- HTTP: 首页 + /api ---
        let (_h, body) = http_get(port, "/");
        assert!(body.contains("ferry") && body.contains("xterm"), "首页应含 xterm");
        let (_h, dj) = http_get(port, "/api/devices");
        assert!(dj.contains("\"rk\"") && dj.contains("\"mcu\""), "设备列表: {}", dj);
        assert!(dj.contains("\"transport\":\"serial\""), "应含串口设备");
        let (_h, sj) = http_get(port, "/api/state");
        assert!(sj.contains("forwards") && sj.contains("blackboxes"), "state: {}", sj);

        // --- WebSocket 终端 ---
        let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
        let key = "dGhlIHNhbXBsZSBub25jZQ==";
        let req = format!(
            "GET /pty?rows=24&cols=90 HTTP/1.1\r\nHost: x\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: {}\r\nSec-WebSocket-Version: 13\r\n\r\n",
            key
        );
        s.write_all(req.as_bytes()).unwrap();
        // 读握手头
        let mut r = BufReader::new(s.try_clone().unwrap());
        let mut handshake = String::new();
        loop {
            let mut line = String::new();
            std::io::BufRead::read_line(&mut r, &mut line).unwrap();
            if line == "\r\n" || line.is_empty() {
                break;
            }
            handshake.push_str(&line);
        }
        assert!(handshake.contains("101"), "应 101 升级: {}", handshake);
        assert!(handshake.contains(&wsutil::ws_accept(key)), "Accept 键应匹配");

        // 发一条命令（客户端帧必须 mask）
        std::thread::sleep(Duration::from_millis(400));
        ws_send_masked(&mut s, 0x2, b"echo GUI_OK_$((3*3))\r");
        assert!(ws_wait_for(&mut r, "GUI_OK_9", 6), "终端应回显命令结果");

        // resize → stty size 反映 24x90
        ws_send_masked(&mut s, 0x1, br#"{"t":"resize","rows":24,"cols":90}"#);
        std::thread::sleep(Duration::from_millis(200));
        ws_send_masked(&mut s, 0x2, b"stty size\r");
        assert!(ws_wait_for(&mut r, "24 90", 5), "resize 应生效");

        let _ = std::fs::remove_dir_all(&home);
    }

    fn http_get(port: u16, path: &str) -> (String, String) {
        let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
        s.write_all(format!("GET {} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n", path).as_bytes()).unwrap();
        let mut buf = Vec::new();
        s.read_to_end(&mut buf).unwrap();
        let text = String::from_utf8_lossy(&buf);
        let (h, b) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
        (h.to_string(), b.to_string())
    }

    fn ws_send_masked(s: &mut TcpStream, opcode: u8, data: &[u8]) {
        let mask = [0x37u8, 0xfa, 0x21, 0x3d];
        let mut frame = vec![0x80 | opcode];
        let len = data.len();
        if len < 126 {
            frame.push(0x80 | len as u8);
        } else {
            frame.push(0x80 | 126);
            frame.extend_from_slice(&(len as u16).to_be_bytes());
        }
        frame.extend_from_slice(&mask);
        for (i, b) in data.iter().enumerate() {
            frame.push(b ^ mask[i % 4]);
        }
        s.write_all(&frame).unwrap();
        s.flush().unwrap();
    }

    fn ws_wait_for(r: &mut BufReader<TcpStream>, needle: &str, secs: u64) -> bool {
        let deadline = Instant::now() + Duration::from_secs(secs);
        let mut acc = String::new();
        while Instant::now() < deadline {
            match wsutil::ws_read(r) {
                Ok(WsMsg::Text(p)) | Ok(WsMsg::Binary(p)) => {
                    acc.push_str(&String::from_utf8_lossy(&p));
                    if acc.contains(needle) {
                        return true;
                    }
                }
                Ok(WsMsg::Close) | Err(_) => return acc.contains(needle),
                _ => {}
            }
        }
        acc.contains(needle)
    }
}
