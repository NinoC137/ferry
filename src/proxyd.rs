//! 内置代理守护进程（`fy __proxyd`）：让没网的板子借主机上网。
//!
//! 相比最初只会 HTTP CONNECT 的版本，现在是：
//!
//! - **一个端口，两种协议**。第一个字节是 0x05 就按 SOCKS5 伺候，否则按 HTTP 代理。
//!   板子上 `http_proxy=` 和 `all_proxy=socks5://` 指同一个地址即可，
//!   apt/opkg/git/curl 全都照顾到了。
//! - **上游代理链**。主机自己在梯子后面时，`--upstream http://127.0.0.1:7897`
//!   或 `socks5://...` 就能把板子的流量接着往上游送；`--upstream auto` 直接读
//!   主机的 `https_proxy`/`all_proxy` 环境变量。板子于是"借到了你的梯子"。
//! - 域名**永远在出口侧解析**（没有上游时是主机，有上游时是上游），
//!   所以板子连 DNS 都不用配。

use crate::util::*;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream, ToSocketAddrs};
use std::time::Duration;

pub const DEFAULT_PORT: u16 = 7979;

/// 上游代理。Direct = 直连出网。
#[derive(Debug, Clone, PartialEq)]
pub enum Upstream {
    Direct,
    Http(String),
    Socks5(String),
}

impl Upstream {
    /// 解析 `http://host:port` / `socks5://host:port` / `host:port`（当 http）/
    /// `auto`（读环境变量）/ `none`。
    pub fn parse(s: &str) -> Result<Upstream, String> {
        let s = s.trim();
        if s.is_empty() || s.eq_ignore_ascii_case("none") || s.eq_ignore_ascii_case("direct") {
            return Ok(Upstream::Direct);
        }
        if s.eq_ignore_ascii_case("auto") {
            return Ok(Upstream::from_env());
        }
        let (scheme, rest) = match s.split_once("://") {
            Some((a, b)) => (a.to_ascii_lowercase(), b),
            None => ("http".to_string(), s),
        };
        // 去掉可能带的用户名密码和路径
        let rest = rest.rsplit('@').next().unwrap_or(rest);
        let hostport = rest.split('/').next().unwrap_or(rest).to_string();
        if hostport.is_empty() {
            return Err(format!("看不懂的上游代理地址: {}", s));
        }
        match scheme.as_str() {
            "http" | "https" => Ok(Upstream::Http(hostport)),
            "socks5" | "socks5h" | "socks" => Ok(Upstream::Socks5(hostport)),
            other => Err(format!(
                "不支持的上游代理协议 '{}'（只认 http / socks5）",
                other
            )),
        }
    }

    /// 从主机环境变量里捡一个上游出来。
    pub fn from_env() -> Upstream {
        for k in [
            "all_proxy",
            "ALL_PROXY",
            "https_proxy",
            "HTTPS_PROXY",
            "http_proxy",
            "HTTP_PROXY",
        ] {
            if let Ok(v) = std::env::var(k) {
                if !v.trim().is_empty() {
                    if let Ok(u) = Upstream::parse(&v) {
                        if u != Upstream::Direct {
                            return u;
                        }
                    }
                }
            }
        }
        Upstream::Direct
    }

    pub fn describe(&self) -> String {
        match self {
            Upstream::Direct => "直连".into(),
            Upstream::Http(h) => format!("http://{}", h),
            Upstream::Socks5(h) => format!("socks5://{}", h),
        }
    }

    pub fn as_arg(&self) -> String {
        match self {
            Upstream::Direct => "none".into(),
            Upstream::Http(h) => format!("http://{}", h),
            Upstream::Socks5(h) => format!("socks5://{}", h),
        }
    }
}

pub fn main_loop(port: u16, up: Upstream) -> std::io::Result<()> {
    let listener = TcpListener::bind(("127.0.0.1", port))?;
    eprintln!(
        "ferry proxyd 监听 127.0.0.1:{}（HTTP + SOCKS5 同端口），上游: {}",
        port,
        up.describe()
    );
    for conn in listener.incoming() {
        match conn {
            Ok(stream) => {
                let up = up.clone();
                std::thread::spawn(move || {
                    let _ = dispatch(stream, &up);
                });
            }
            Err(_) => continue,
        }
    }
    Ok(())
}

/// 靠第一个字节分流：SOCKS5 的版本号是 0x05，HTTP 方法名都是 ASCII 字母。
fn dispatch(client: TcpStream, up: &Upstream) -> std::io::Result<()> {
    client.set_read_timeout(Some(Duration::from_secs(30)))?;
    let mut first = [0u8; 1];
    // peek 不消费数据，后面各自的解析照常从头读
    let n = match client.peek(&mut first) {
        Ok(n) => n,
        Err(_) => return Ok(()),
    };
    if n == 0 {
        return Ok(());
    }
    if first[0] == 0x05 {
        handle_socks5(client, up)
    } else {
        handle_http(client, up)
    }
}

// ---------------- HTTP 代理 ----------------

fn handle_http(client: TcpStream, up: &Upstream) -> std::io::Result<()> {
    let peer = client.try_clone()?;
    let mut reader = BufReader::new(peer);

    let mut head = String::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            return Ok(());
        }
        if line == "\r\n" || line == "\n" {
            break;
        }
        head.push_str(&line);
        if head.len() > 64 * 1024 {
            return Ok(());
        }
    }
    let mut lines = head.lines();
    let request = lines.next().unwrap_or("").to_string();
    let mut parts = request.split_whitespace();
    let method = parts.next().unwrap_or("").to_uppercase();
    let uri = parts.next().unwrap_or("").to_string();
    let version = parts.next().unwrap_or("HTTP/1.1").to_string();

    if method == "CONNECT" {
        let upstream = match connect_via(up, &uri, 443) {
            Ok(s) => s,
            Err(_) => {
                let mut c = client;
                let _ = c.write_all(b"HTTP/1.1 502 Bad Gateway\r\n\r\n");
                return Ok(());
            }
        };
        let mut c = client;
        c.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")?;
        splice(reader, c, upstream)
    } else if let Some(rest) = uri.strip_prefix("http://") {
        let (hostport, path) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, "/"),
        };
        // 上游本身就是 HTTP 代理时，把**绝对地址**请求原样转给它——这正是代理协议
        let http_upstream = match up {
            Upstream::Http(h) => Some(h.clone()),
            _ => None,
        };
        let conn = match &http_upstream {
            Some(h) => connect_direct(h),
            None => connect_via(up, hostport, 80),
        };
        let mut conn = match conn {
            Ok(s) => s,
            Err(_) => {
                let mut c = client;
                let _ = c.write_all(b"HTTP/1.1 502 Bad Gateway\r\n\r\n");
                return Ok(());
            }
        };
        let target = if http_upstream.is_some() {
            uri.as_str()
        } else {
            path
        };
        let mut fwd = format!("{} {} {}\r\n", method, target, version);
        let mut has_host = false;
        for l in lines {
            let low = l.to_lowercase();
            if low.starts_with("proxy-connection:") {
                continue;
            }
            if low.starts_with("host:") {
                has_host = true;
            }
            fwd.push_str(l);
            fwd.push_str("\r\n");
        }
        if !has_host {
            fwd.push_str(&format!("Host: {}\r\n", hostport));
        }
        fwd.push_str("\r\n");
        conn.write_all(fwd.as_bytes())?;
        splice(reader, client, conn)
    } else {
        let mut c = client;
        let _ = c.write_all(
            "HTTP/1.1 400 Bad Request\r\n\r\nferry proxyd: 只支持 CONNECT 或绝对地址 http 请求\r\n"
                .as_bytes(),
        );
        Ok(())
    }
}

// ---------------- SOCKS5 ----------------

const S5_VER: u8 = 0x05;
const S5_CMD_CONNECT: u8 = 0x01;
const S5_ATYP_V4: u8 = 0x01;
const S5_ATYP_DOMAIN: u8 = 0x03;
const S5_ATYP_V6: u8 = 0x04;

fn read_exact_n(s: &mut impl Read, n: usize) -> std::io::Result<Vec<u8>> {
    let mut v = vec![0u8; n];
    s.read_exact(&mut v)?;
    Ok(v)
}

/// SOCKS5 服务端（只做 CONNECT，不做认证——它只监听 127.0.0.1，
/// 板子经反向隧道过来，本来就已经过了 ssh 那一关）。
fn handle_socks5(mut client: TcpStream, up: &Upstream) -> std::io::Result<()> {
    // 1) 握手：VER NMETHODS METHODS...
    let head = read_exact_n(&mut client, 2)?;
    if head[0] != S5_VER {
        return Ok(());
    }
    let _methods = read_exact_n(&mut client, head[1] as usize)?;
    client.write_all(&[S5_VER, 0x00])?; // 选"无需认证"

    // 2) 请求：VER CMD RSV ATYP DST.ADDR DST.PORT
    let req = read_exact_n(&mut client, 4)?;
    if req[0] != S5_VER {
        return Ok(());
    }
    let atyp = req[3];
    let host = match atyp {
        S5_ATYP_V4 => {
            let a = read_exact_n(&mut client, 4)?;
            format!("{}.{}.{}.{}", a[0], a[1], a[2], a[3])
        }
        S5_ATYP_DOMAIN => {
            let l = read_exact_n(&mut client, 1)?[0] as usize;
            let raw = read_exact_n(&mut client, l)?;
            // 必须原样保留字节：from_utf8_lossy 会把每个非法字节换成 3 字节的 U+FFFD，
            // 转发给上游时 `len as u8` 一截断，长度和内容就对不上了（可被用来往
            // 上游走私一段自选数据）。域名本来就只能是 ASCII，非 ASCII 直接回绝。
            match std::str::from_utf8(&raw) {
                Ok(h) if h.is_ascii() && !h.is_empty() => h.to_string(),
                _ => {
                    let _ = client.write_all(&s5_reply(0x08)); // address type not supported
                    return Ok(());
                }
            }
        }
        S5_ATYP_V6 => {
            let a = read_exact_n(&mut client, 16)?;
            let mut segs = vec![];
            for i in 0..8 {
                segs.push(format!(
                    "{:x}",
                    u16::from_be_bytes([a[i * 2], a[i * 2 + 1]])
                ));
            }
            format!("[{}]", segs.join(":"))
        }
        _ => {
            let _ = client.write_all(&s5_reply(0x08)); // address type not supported
            return Ok(());
        }
    };
    let pb = read_exact_n(&mut client, 2)?;
    let port = u16::from_be_bytes([pb[0], pb[1]]);

    if req[1] != S5_CMD_CONNECT {
        // BIND / UDP ASSOCIATE 我们不做：板子上的场景用不到，老实说不支持
        let _ = client.write_all(&s5_reply(0x07));
        return Ok(());
    }

    let hostport = format!("{}:{}", host, port);
    let upstream = match connect_via(up, &hostport, port) {
        Ok(s) => s,
        Err(e) => {
            let rep = match e.kind() {
                std::io::ErrorKind::TimedOut => 0x06,
                std::io::ErrorKind::ConnectionRefused => 0x05,
                _ => 0x04,
            };
            let _ = client.write_all(&s5_reply(rep));
            return Ok(());
        }
    };
    client.write_all(&s5_reply(0x00))?;
    let r = BufReader::new(client.try_clone()?);
    splice(r, client, upstream)
}

/// SOCKS5 应答。BND.ADDR 一律填 0.0.0.0:0 —— 客户端对 CONNECT 不看这个字段。
fn s5_reply(code: u8) -> [u8; 10] {
    [S5_VER, code, 0x00, S5_ATYP_V4, 0, 0, 0, 0, 0, 0]
}

// ---------------- 出站（含上游链） ----------------

/// 逐字节把 HTTP 响应头读完（读到 CRLFCRLF 为止），一个多余的字节都不吞。
fn read_http_head(s: &mut TcpStream) -> std::io::Result<String> {
    let mut head = Vec::with_capacity(256);
    let mut b = [0u8; 1];
    while head.len() < 16 * 1024 {
        let n = s.read(&mut b)?;
        if n == 0 {
            break;
        }
        head.push(b[0]);
        if head.ends_with(b"\r\n\r\n") || head.ends_with(b"\n\n") {
            break;
        }
    }
    Ok(String::from_utf8_lossy(&head).into_owned())
}

fn connect_direct(hostport: &str) -> std::io::Result<TcpStream> {
    let addr = if hostport.contains(':') {
        hostport.to_string()
    } else {
        format!("{}:80", hostport)
    };
    let sock = addr
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "域名解析不出来"))?;
    TcpStream::connect_timeout(&sock, Duration::from_secs(10))
}

/// 按上游设置建立到 `hostport` 的连接。有上游时**域名交给上游解析**。
fn connect_via(up: &Upstream, hostport: &str, default_port: u16) -> std::io::Result<TcpStream> {
    let hostport = if hostport.contains(':') {
        hostport.to_string()
    } else {
        format!("{}:{}", hostport, default_port)
    };
    match up {
        Upstream::Direct => connect_direct(&hostport),
        Upstream::Http(h) => {
            let mut s = connect_direct(h)?;
            s.set_read_timeout(Some(Duration::from_secs(15)))?;
            let req = format!(
                "CONNECT {hp} HTTP/1.1\r\nHost: {hp}\r\nProxy-Connection: keep-alive\r\n\r\n",
                hp = hostport
            );
            s.write_all(req.as_bytes())?;
            // 逐字节读到空行为止。**不能用 BufReader**：它会预读到 8 KiB，
            // 而 CONNECT 成功之后紧接着就是隧道数据——一旦被读进缓冲再随 reader
            // 丢掉，服务器先说话的协议（SMTP/IMAP/FTP/MySQL…）就永远收不到问候语。
            let head = read_http_head(&mut s)?;
            let first = head.lines().next().unwrap_or("").to_string();
            if !first.contains(" 200") {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::ConnectionRefused,
                    format!("上游 HTTP 代理拒绝: {}", first.trim()),
                ));
            }
            s.set_read_timeout(None)?;
            Ok(s)
        }
        Upstream::Socks5(h) => {
            let mut s = connect_direct(h)?;
            s.set_read_timeout(Some(Duration::from_secs(15)))?;
            s.write_all(&[S5_VER, 0x01, 0x00])?; // 只提"无认证"
            let hello = read_exact_n(&mut s, 2)?;
            if hello[0] != S5_VER || hello[1] != 0x00 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "上游 SOCKS5 要求认证，ferry 目前只支持免认证上游",
                ));
            }
            let (host, port_s) = hostport
                .rsplit_once(':')
                .unwrap_or((hostport.as_str(), "80"));
            let port: u16 = port_s.parse().unwrap_or(default_port);
            let host = host.trim_matches(|c| c == '[' || c == ']');
            if host.is_empty() || host.len() > 255 {
                // SOCKS5 的域名长度字段只有一个字节，装不下就老实报错，
                // 绝不 `as u8` 截断（那会让上游读到长度与内容不符的报文）
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "目标域名过长，SOCKS5 装不下",
                ));
            }
            let mut req = vec![
                S5_VER,
                S5_CMD_CONNECT,
                0x00,
                S5_ATYP_DOMAIN,
                host.len() as u8,
            ];
            req.extend_from_slice(host.as_bytes());
            req.extend_from_slice(&port.to_be_bytes());
            s.write_all(&req)?;
            let rep = read_exact_n(&mut s, 4)?;
            if rep[1] != 0x00 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::ConnectionRefused,
                    format!("上游 SOCKS5 拒绝，代码 {}", rep[1]),
                ));
            }
            // 把 BND.ADDR 吃掉
            match rep[3] {
                S5_ATYP_V4 => {
                    read_exact_n(&mut s, 4 + 2)?;
                }
                S5_ATYP_V6 => {
                    read_exact_n(&mut s, 16 + 2)?;
                }
                S5_ATYP_DOMAIN => {
                    let l = read_exact_n(&mut s, 1)?[0] as usize;
                    read_exact_n(&mut s, l + 2)?;
                }
                _ => {}
            }
            s.set_read_timeout(None)?;
            Ok(s)
        }
    }
}

/// 双向搬运直到任一端关闭。reader 里可能还缓存着 client 已发的数据。
fn splice(
    client_r: BufReader<TcpStream>,
    client_w: TcpStream,
    upstream: TcpStream,
) -> std::io::Result<()> {
    let mut up_w = upstream.try_clone()?;
    let mut up_r = upstream;
    let mut cw = client_w;
    let _ = cw.set_read_timeout(None);
    let t = std::thread::spawn(move || {
        let buffered = client_r.buffer().to_vec();
        if !buffered.is_empty() && up_w.write_all(&buffered).is_err() {
            return;
        }
        let mut c = client_r.into_inner();
        let _ = c.set_read_timeout(None);
        let mut buf = [0u8; 16 * 1024];
        loop {
            match c.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if up_w.write_all(&buf[..n]).is_err() {
                        break;
                    }
                }
            }
        }
        let _ = up_w.shutdown(std::net::Shutdown::Write);
    });
    let mut buf = [0u8; 16 * 1024];
    loop {
        match up_r.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if cw.write_all(&buf[..n]).is_err() {
                    break;
                }
            }
        }
    }
    let _ = cw.shutdown(std::net::Shutdown::Write);
    let _ = t.join();
    Ok(())
}

// ---------------- 管理入口（fy share / fy proxy 用） ----------------

pub fn current_upstream() -> Upstream {
    let st = crate::config::State::load();
    Upstream::parse(&st.get_str("proxy", "upstream")).unwrap_or(Upstream::Direct)
}

pub fn running_pid() -> i32 {
    let st = crate::config::State::load();
    let pid = st.get_int("proxy", "pid") as i32;
    if pid > 0 && pid_alive(pid) {
        pid
    } else {
        0
    }
}

pub fn current_port() -> u16 {
    let st = crate::config::State::load();
    let p = st.get_int("proxy", "port") as u16;
    if p == 0 {
        DEFAULT_PORT
    } else {
        p
    }
}

/// 确保本机 proxyd 在跑，返回端口。
pub fn ensure_running(port: u16) -> std::io::Result<u16> {
    ensure_running_with(port, None)
}

/// 同上，但可以指定上游；上游或端口变了会重启守护进程。
pub fn ensure_running_with(port: u16, want_up: Option<Upstream>) -> std::io::Result<u16> {
    let mut st = crate::config::State::load();
    let pid = st.get_int("proxy", "pid") as i32;
    let cur = st.get_int("proxy", "port") as u16;
    let cur_up = Upstream::parse(&st.get_str("proxy", "upstream")).unwrap_or(Upstream::Direct);
    let up = want_up.unwrap_or_else(|| cur_up.clone());

    if pid > 0 && pid_alive(pid) && cur == port && up == cur_up {
        return Ok(port);
    }
    if pid > 0 && pid_alive(pid) {
        kill_pid(pid);
        for _ in 0..20 {
            if !pid_alive(pid) {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    let log = cfg_dir().join("proxyd.log");
    let pid = spawn_daemon(
        &argv(&[
            &self_exe().display().to_string(),
            "__proxyd",
            "--port",
            &port.to_string(),
            "--upstream",
            &up.as_arg(),
        ]),
        &log,
    )?;
    if !dry() {
        std::thread::sleep(Duration::from_millis(300));
    }
    st.set_int("proxy", "pid", pid as i64);
    st.set_int("proxy", "port", port as i64);
    st.set_str("proxy", "upstream", &up.as_arg());
    st.save();
    ok(&format!(
        "代理守护进程已启动 (pid {}, 127.0.0.1:{}, HTTP+SOCKS5, 上游 {})",
        pid,
        port,
        up.describe()
    ));
    Ok(port)
}

pub fn stop() {
    let mut st = crate::config::State::load();
    let pid = st.get_int("proxy", "pid") as i32;
    if pid > 0 && pid_alive(pid) {
        kill_pid(pid);
        for _ in 0..20 {
            if !pid_alive(pid) {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        ok("代理守护进程已停止");
    }
    st.drop_table("proxy");
    st.save();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upstream_parsing() {
        assert_eq!(Upstream::parse("").unwrap(), Upstream::Direct);
        assert_eq!(Upstream::parse("none").unwrap(), Upstream::Direct);
        assert_eq!(
            Upstream::parse("http://127.0.0.1:7897").unwrap(),
            Upstream::Http("127.0.0.1:7897".into())
        );
        // 没写协议就当 http
        assert_eq!(
            Upstream::parse("127.0.0.1:7897").unwrap(),
            Upstream::Http("127.0.0.1:7897".into())
        );
        assert_eq!(
            Upstream::parse("socks5://127.0.0.1:7897").unwrap(),
            Upstream::Socks5("127.0.0.1:7897".into())
        );
        assert_eq!(
            Upstream::parse("socks5h://user:pw@10.0.0.1:1080/").unwrap(),
            Upstream::Socks5("10.0.0.1:1080".into())
        );
        assert!(Upstream::parse("ftp://x:1").is_err());
        // 往返：as_arg 解析回来要一模一样
        for s in ["http://a:1", "socks5://b:2", "none"] {
            let u = Upstream::parse(s).unwrap();
            assert_eq!(Upstream::parse(&u.as_arg()).unwrap(), u);
        }
    }

    #[test]
    fn socks5_reply_shape() {
        let r = s5_reply(0x00);
        assert_eq!(r[0], 0x05);
        assert_eq!(r[1], 0x00);
        assert_eq!(r[3], S5_ATYP_V4);
        assert_eq!(r.len(), 10);
    }

    /// 起一个真的 proxyd，用真的 SOCKS5 客户端握手，去连一个真的 TCP 回显服务。
    #[test]
    fn socks5_end_to_end() {
        let echo = TcpListener::bind("127.0.0.1:0").unwrap();
        let echo_port = echo.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for c in echo.incoming().flatten() {
                std::thread::spawn(move || {
                    let mut c = c;
                    let mut b = [0u8; 256];
                    if let Ok(n) = c.read(&mut b) {
                        let _ = c.write_all(&b[..n]);
                    }
                });
            }
        });

        let px = TcpListener::bind("127.0.0.1:0").unwrap();
        let px_port = px.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for c in px.incoming().flatten() {
                std::thread::spawn(move || {
                    let _ = dispatch(c, &Upstream::Direct);
                });
            }
        });
        std::thread::sleep(Duration::from_millis(150));

        let mut s = TcpStream::connect(("127.0.0.1", px_port)).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        s.write_all(&[0x05, 0x01, 0x00]).unwrap();
        let hello = read_exact_n(&mut s, 2).unwrap();
        assert_eq!(hello, vec![0x05, 0x00], "应该选中免认证");

        let host = b"127.0.0.1";
        let mut req = vec![0x05, 0x01, 0x00, 0x03, host.len() as u8];
        req.extend_from_slice(host);
        req.extend_from_slice(&echo_port.to_be_bytes());
        s.write_all(&req).unwrap();
        let rep = read_exact_n(&mut s, 10).unwrap();
        assert_eq!(rep[1], 0x00, "CONNECT 应该成功");

        s.write_all(b"ferry").unwrap();
        let back = read_exact_n(&mut s, 5).unwrap();
        assert_eq!(&back, b"ferry", "隧道应该双向通");
    }

    /// 同一个端口也要能认出 HTTP 代理请求。
    #[test]
    fn http_and_socks_share_one_port() {
        let origin = TcpListener::bind("127.0.0.1:0").unwrap();
        let origin_port = origin.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for c in origin.incoming().flatten() {
                let mut c = c;
                let mut b = [0u8; 1024];
                let _ = c.read(&mut b);
                let _ = c.write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nhi",
                );
            }
        });
        let px = TcpListener::bind("127.0.0.1:0").unwrap();
        let px_port = px.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for c in px.incoming().flatten() {
                std::thread::spawn(move || {
                    let _ = dispatch(c, &Upstream::Direct);
                });
            }
        });
        std::thread::sleep(Duration::from_millis(150));

        let mut s = TcpStream::connect(("127.0.0.1", px_port)).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let req = format!(
            "GET http://127.0.0.1:{}/x HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n",
            origin_port
        );
        s.write_all(req.as_bytes()).unwrap();
        let mut out = String::new();
        let _ = s.read_to_string(&mut out);
        assert!(
            out.contains("200 OK") && out.ends_with("hi"),
            "HTTP 代理没通: {:?}",
            out
        );
    }

    #[test]
    fn socks5_refuses_bind_and_udp() {
        let px = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = px.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for c in px.incoming().flatten() {
                std::thread::spawn(move || {
                    let _ = dispatch(c, &Upstream::Direct);
                });
            }
        });
        std::thread::sleep(Duration::from_millis(120));
        let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        s.write_all(&[0x05, 0x01, 0x00]).unwrap();
        let _ = read_exact_n(&mut s, 2).unwrap();
        // CMD=0x03 是 UDP ASSOCIATE
        let mut req = vec![0x05, 0x03, 0x00, 0x01, 127, 0, 0, 1];
        req.extend_from_slice(&80u16.to_be_bytes());
        s.write_all(&req).unwrap();
        let rep = read_exact_n(&mut s, 10).unwrap();
        assert_eq!(rep[1], 0x07, "不支持的命令要明确回绝");
    }

    /// 上游是 HTTP 代理时，要能把 CONNECT 链下去。
    #[test]
    fn chains_through_an_http_upstream() {
        let origin = TcpListener::bind("127.0.0.1:0").unwrap();
        let origin_port = origin.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for c in origin.incoming().flatten() {
                let mut c = c;
                let mut b = [0u8; 64];
                if let Ok(n) = c.read(&mut b) {
                    let _ = c.write_all(&b[..n]);
                }
            }
        });
        // 假装自己是上游 HTTP 代理：应 CONNECT，然后转发到 origin
        let upl = TcpListener::bind("127.0.0.1:0").unwrap();
        let up_port = upl.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for c in upl.incoming().flatten() {
                std::thread::spawn(move || {
                    let _ = dispatch(c, &Upstream::Direct);
                });
            }
        });
        // 前置 proxyd 把上游设成那个假上游
        let px = TcpListener::bind("127.0.0.1:0").unwrap();
        let px_port = px.local_addr().unwrap().port();
        let up = Upstream::Http(format!("127.0.0.1:{}", up_port));
        std::thread::spawn(move || {
            for c in px.incoming().flatten() {
                let up = up.clone();
                std::thread::spawn(move || {
                    let _ = dispatch(c, &up);
                });
            }
        });
        std::thread::sleep(Duration::from_millis(200));

        let mut s = TcpStream::connect(("127.0.0.1", px_port)).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        s.write_all(format!("CONNECT 127.0.0.1:{} HTTP/1.1\r\n\r\n", origin_port).as_bytes())
            .unwrap();
        let mut head = [0u8; 39];
        s.read_exact(&mut head).unwrap();
        assert!(
            String::from_utf8_lossy(&head).contains("200"),
            "两级代理没串起来: {:?}",
            String::from_utf8_lossy(&head)
        );
        s.write_all(b"chain").unwrap();
        let back = read_exact_n(&mut s, 5).unwrap();
        assert_eq!(&back, b"chain");
    }
}

#[cfg(test)]
mod regression_tests {
    use super::*;

    /// 上游是 HTTP 代理、且目标服务器**先说话**时，问候语一个字节都不能丢。
    #[test]
    fn http_upstream_keeps_server_first_greeting() {
        // 目标：连上就发 SMTP 风格的问候
        let origin = TcpListener::bind("127.0.0.1:0").unwrap();
        let origin_port = origin.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for c in origin.incoming().flatten() {
                let mut c = c;
                let _ = c.write_all(b"220 mail.example.com ESMTP ready\r\n");
                std::thread::sleep(Duration::from_millis(300));
            }
        });
        // 假上游：把 200 和目标的问候塞在同一个 write 里，逼出预读丢字节的 bug
        let upl = TcpListener::bind("127.0.0.1:0").unwrap();
        let up_port = upl.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for c in upl.incoming().flatten() {
                std::thread::spawn(move || {
                    let mut c = c;
                    let mut b = [0u8; 1024];
                    let _ = c.read(&mut b);
                    let _ = c.write_all(
                        b"HTTP/1.1 200 Connection established\r\n\r\n220 mail.example.com ESMTP ready\r\n",
                    );
                    std::thread::sleep(Duration::from_millis(300));
                });
            }
        });
        let px = TcpListener::bind("127.0.0.1:0").unwrap();
        let px_port = px.local_addr().unwrap().port();
        let up = Upstream::Http(format!("127.0.0.1:{}", up_port));
        std::thread::spawn(move || {
            for c in px.incoming().flatten() {
                let up = up.clone();
                std::thread::spawn(move || {
                    let _ = dispatch(c, &up);
                });
            }
        });
        std::thread::sleep(Duration::from_millis(200));

        let mut s = TcpStream::connect(("127.0.0.1", px_port)).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        s.write_all(format!("CONNECT 127.0.0.1:{} HTTP/1.1\r\n\r\n", origin_port).as_bytes())
            .unwrap();
        let mut got = Vec::new();
        let mut b = [0u8; 512];
        while let Ok(n) = s.read(&mut b) {
            if n == 0 {
                break;
            }
            got.extend_from_slice(&b[..n]);
            if got.windows(3).any(|w| w == b"220") {
                break;
            }
        }
        let text = String::from_utf8_lossy(&got);
        assert!(text.contains("200"), "CONNECT 应该成功: {:?}", text);
        assert!(
            text.contains("220 mail.example.com"),
            "服务器先说话的问候语被吞了: {:?}",
            text
        );
    }

    /// 客户端塞非 ASCII 域名时，必须干脆回绝，不能把畸形报文转给上游。
    #[test]
    fn socks5_rejects_non_ascii_domain() {
        let px = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = px.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for c in px.incoming().flatten() {
                std::thread::spawn(move || {
                    let _ = dispatch(c, &Upstream::Direct);
                });
            }
        });
        std::thread::sleep(Duration::from_millis(120));
        let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        s.write_all(&[0x05, 0x01, 0x00]).unwrap();
        let _ = read_exact_n(&mut s, 2).unwrap();
        let bad = vec![0xFFu8; 250];
        let mut req = vec![0x05, 0x01, 0x00, 0x03, bad.len() as u8];
        req.extend_from_slice(&bad);
        req.extend_from_slice(&80u16.to_be_bytes());
        s.write_all(&req).unwrap();
        let rep = read_exact_n(&mut s, 10).unwrap();
        assert_eq!(rep[1], 0x08, "非法域名要报 address type not supported");
    }
}
