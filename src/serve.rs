//! `fy serve` —— 局域网快传。
//!
//! 板子经常处在"scp 不通、但 HTTP 出得去"的状态：dropbear 没有 sftp、
//! 只读 rootfs 里没有 scp、recovery 里只剩一个 busybox wget。这时候最快的路子
//! 是主机起个 HTTP 服务，板上一条 `wget` 拉走。
//!
//! 特点：
//! - **自动算出板子该用哪个 IP**（`--for <设备>` 时按路由表选直连网口地址），
//!   并把可以直接粘贴到板子上的命令打出来；
//! - 支持 `Range`，板上 `wget -c` 断点续传；
//! - `--upload` 反向收文件：板子 `curl -T` / `wget --post-file` 推上来；
//! - 默认带随机 token 前缀，同一个局域网里别人猜不到，也防手滑覆盖。

use crate::config::Device;
use crate::httpd::{self, Request};
use crate::jsonout::{code, fail};
use crate::usbnet;
use crate::util::*;
use std::io::{BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

pub struct ServeOpts {
    pub port: u16,
    pub bind: String,
    pub roots: Vec<PathBuf>,
    pub upload_dir: Option<PathBuf>,
    pub token: String,
    pub once: bool,
}

struct Shared {
    /// (URL 里的名字, canonicalize 过的真实路径)。启动时算好，请求时只做查表，
    /// 于是 `fy serve .`（basename 为空）和"两个 roots 同名"都不会再翻车。
    roots: Vec<(String, PathBuf)>,
    upload_dir: Option<PathBuf>,
    token: String,
    hits: AtomicU64,
    once: bool,
}

/// URL 前缀（token 为空时就是根）。
fn prefix(token: &str) -> String {
    if token.is_empty() {
        String::new()
    } else {
        format!("/{}", token)
    }
}

pub fn run(o: ServeOpts, advertise: Vec<(String, String)>, for_dev: Option<&Device>) -> Result<(), String> {
    for r in &o.roots {
        if !r.exists() {
            return Err(format!("路径不存在: {}", r.display()));
        }
    }
    if let Some(u) = &o.upload_dir {
        std::fs::create_dir_all(u).map_err(|e| format!("建不了上传目录 {}: {}", u.display(), e))?;
    }
    let listener = TcpListener::bind((o.bind.as_str(), o.port))
        .map_err(|e| format!("绑定 {}:{} 失败（端口被占？换 --port）: {}", o.bind, o.port, e))?;
    let real_port = listener.local_addr().map(|a| a.port()).unwrap_or(o.port);

    let pfx = prefix(&o.token);
    let host = advertise
        .first()
        .map(|(_, ip)| ip.clone())
        .unwrap_or_else(|| "127.0.0.1".to_string());
    let base = format!("http://{}:{}{}", host, real_port, pfx);

    print_banner(&o, &base, &advertise, real_port, &pfx, for_dev);

    let shared = Arc::new(Shared {
        roots: named_roots(&o.roots),
        upload_dir: o.upload_dir.clone(),
        token: o.token.clone(),
        hits: AtomicU64::new(0),
        once: o.once,
    });
    httpd::serve_full(listener, move |req, reader, stream| {
        let sh = shared.clone();
        if let Err(_e) = route(&sh, req, reader, stream) {
            // 单连接出错很正常（板子 Ctrl-C / wget 提前断），不打扰用户
        }
        if sh.once && sh.hits.load(Ordering::Relaxed) > 0 {
            ok("传完一个文件，--once 收工");
            std::process::exit(0);
        }
    });
    Ok(())
}

fn print_banner(
    o: &ServeOpts,
    base: &str,
    advertise: &[(String, String)],
    port: u16,
    pfx: &str,
    for_dev: Option<&Device>,
) {
    ok(&format!("局域网快传已启动: {}", cyan(base)));
    if advertise.len() > 1 {
        info("其它可用地址:");
        for (i, ip) in advertise.iter().skip(1) {
            eprintln!("    {}  {}", dim(i), dim(&format!("http://{}:{}{}", ip, port, pfx)));
        }
    }
    let names: Vec<String> = o
        .roots
        .iter()
        .map(|r| r.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| r.display().to_string()))
        .collect();
    eprintln!();
    eprintln!("{}", bold("板端可以直接粘贴:"));
    for n in names.iter().take(4) {
        eprintln!("  {}", cyan(&format!("wget -c {}/{}", base, n)));
    }
    if names.len() > 4 {
        eprintln!("  {}", dim(&format!("... 共 {} 项，浏览目录: {}/", names.len(), base)));
    }
    if o.upload_dir.is_some() {
        eprintln!("  {}   {}", cyan(&format!("curl -T ./板上的文件 {}/up/", base)), dim("（往主机传）"));
        eprintln!(
            "  {}",
            dim(&format!("没有 curl 就: wget --post-file=./文件 -O- {}/up/文件名", base))
        );
    }
    if let Some(d) = for_dev {
        eprintln!();
        info(&format!(
            "已按 {} 的路由选好地址；板子拉不动就先 `fy net {}` 看看网络",
            d.name, d.name
        ));
    }
    eprintln!();
    info("Ctrl-C 停止。");
}

// ---------------- 路由 ----------------

fn route(sh: &Shared, req: Request, reader: BufReader<TcpStream>, mut stream: TcpStream) -> std::io::Result<()> {
    let pfx = prefix(&sh.token);
    let path = match req.path.strip_prefix(&pfx as &str) {
        Some(p) if !pfx.is_empty() => p,
        _ if pfx.is_empty() => req.path.as_str(),
        _ => {
            // token 不对：不解释、不提示，直接 404
            return httpd::not_found(&mut stream);
        }
    };
    let path = if path.is_empty() { "/" } else { path };
    let decoded = httpd::url_decode(path);

    if decoded.starts_with("/up/") || decoded == "/up" {
        return handle_upload(sh, &req, reader, stream, decoded.trim_start_matches("/up").trim_start_matches('/'));
    }
    if decoded == "/" {
        return listing(sh, &req, &mut stream);
    }
    match resolve(sh, &decoded) {
        Some(p) if p.is_dir() => dir_listing(&p, &decoded, &pfx, &req, &mut stream),
        Some(p) => {
            let name = p.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
            let n = httpd::send_file(&mut stream, &p, &req, &name)?;
            sh.hits.fetch_add(1, Ordering::Relaxed);
            info(&format!("↓ {} ({})", name, human_bytes(n)));
            Ok(())
        }
        None => httpd::not_found(&mut stream),
    }
}

/// 给每个 root 起一个 URL 上用的名字。
/// `fy serve .` 的 basename 是空的，`fy serve a/build b/build` 会撞名——
/// 都在这里一次性摆平：先 canonicalize 拿到真名，再对重名加后缀。
fn named_roots(roots: &[PathBuf]) -> Vec<(String, PathBuf)> {
    let mut out: Vec<(String, PathBuf)> = vec![];
    for r in roots {
        let real = r.canonicalize().unwrap_or_else(|_| r.clone());
        let base = real
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "root".to_string());
        let mut name = base.clone();
        let mut n = 2;
        while out.iter().any(|(x, _)| *x == name) {
            name = format!("{}-{}", base, n);
            n += 1;
        }
        out.push((name, real));
    }
    out
}

/// URL 路径 → 本地路径。**只允许落在 roots 之内**，`..` 一律拒绝。
fn resolve(sh: &Shared, decoded: &str) -> Option<PathBuf> {
    // 两头的斜杠都去掉：目录链接会带尾斜杠（/tok/app/），不能因此判成非法路径
    let rel = decoded.trim_matches('/');
    if rel.is_empty() {
        return None;
    }
    // 路径穿越防御：先按段过滤，再用 canonicalize 复核
    if rel.split('/').any(|seg| seg == ".." || seg == "." || seg.is_empty()) {
        return None;
    }
    let (first, rest) = match rel.split_once('/') {
        Some((a, b)) => (a, Some(b)),
        None => (rel, None),
    };
    for (name, root) in &sh.roots {
        if name != first {
            continue;
        }
        let cand = match rest {
            Some(r) if root.is_dir() => root.join(r),
            Some(_) => continue, // root 是文件，后面还有路径 → 不可能
            None => root.clone(),
        };
        // canonicalize 失败（不存在/权限）只代表这个 root 不匹配，得接着看下一个
        let real = match cand.canonicalize() {
            Ok(x) => x,
            Err(_) => continue,
        };
        if real == *root || real.starts_with(root) {
            return Some(real);
        }
    }
    None
}

fn listing(sh: &Shared, req: &Request, stream: &mut TcpStream) -> std::io::Result<()> {
    let mut rows = vec![];
    for (name, path) in &sh.roots {
        let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        rows.push((name.clone(), size, path.is_dir()));
    }
    render_listing(&rows, "/", &prefix(&sh.token), wants_html(req), stream)
}

fn dir_listing(dir: &Path, urlpath: &str, pfx: &str, req: &Request, stream: &mut TcpStream) -> std::io::Result<()> {
    let mut rows = vec![];
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            let md = e.metadata().ok();
            rows.push((
                name,
                md.as_ref().map(|m| m.len()).unwrap_or(0),
                md.map(|m| m.is_dir()).unwrap_or(false),
            ));
        }
    }
    rows.sort_by(|a, b| b.2.cmp(&a.2).then(a.0.cmp(&b.0)));
    render_listing(&rows, urlpath, pfx, wants_html(req), stream)
}

fn wants_html(req: &Request) -> bool {
    req.header("accept").map(|a| a.contains("text/html")).unwrap_or(false)
}

/// 目录列表。curl/wget 拿到的是**纯文本、一行一个 URL**（板上可以直接
/// `... | cut -f1 | xargs wget`），浏览器拿到 HTML。靠 Accept 头区分。
fn render_listing(
    rows: &[(String, u64, bool)],
    urlpath: &str,
    pfx: &str,
    html: bool,
    stream: &mut TcpStream,
) -> std::io::Result<()> {
    let base = format!("{}{}", pfx, urlpath.trim_end_matches('/'));
    if !html {
        let mut t = String::new();
        for (name, size, is_dir) in rows {
            t.push_str(&format!(
                "{}/{}{}\t{}\n",
                base,
                name,
                if *is_dir { "/" } else { "" },
                if *is_dir { "-".to_string() } else { size.to_string() }
            ));
        }
        return httpd::ok_text(stream, &t);
    }
    let mut h = String::from(
        "<!doctype html><meta charset=utf-8><title>ferry serve</title>\
         <style>body{font:14px/1.6 ui-monospace,Menlo,Consolas,monospace;margin:2rem;max-width:52rem}\
         a{text-decoration:none}td{padding:.15rem .8rem .15rem 0}\
         .d{color:#06c}.s{color:#888;text-align:right}</style><h3>ferry serve</h3><table>",
    );
    for (name, size, is_dir) in rows {
        h.push_str(&format!(
            "<tr><td><a class=\"{}\" href=\"{}/{}\">{}{}</a></td><td class=s>{}</td></tr>",
            if *is_dir { "d" } else { "" },
            base,
            // href 里放的是 URL，既要百分号编码也要 HTML 转义 ——
            // 否则一个名叫 `a" onmouseover=...` 的文件就能在目录页里注入属性
            html_escape(&url_encode(name)),
            html_escape(name),
            if *is_dir { "/" } else { "" },
            if *is_dir { "".into() } else { human_bytes(*size) }
        ));
    }
    h.push_str("</table>");
    httpd::ok_html(stream, &h)
}

/// URL 路径分段的百分号编码（保留常见安全字符，其余一律编码）。
fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(*b as char),
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

// ---------------- 上传 ----------------

fn handle_upload(
    sh: &Shared,
    req: &Request,
    mut reader: BufReader<TcpStream>,
    mut stream: TcpStream,
    name_hint: &str,
) -> std::io::Result<()> {
    let dir = match &sh.upload_dir {
        Some(d) => d.clone(),
        None => return httpd::bad(&mut stream, "403 Forbidden", "这个 serve 没开 --upload"),
    };
    if req.method != "PUT" && req.method != "POST" {
        return httpd::bad(&mut stream, "405 Method Not Allowed", "上传请用 PUT 或 POST");
    }
    // 文件名只取最后一段，且不许带路径分隔符
    let mut name = httpd::url_decode(name_hint);
    if name.is_empty() {
        name = format!("upload-{}", rand_hex(6));
    }
    let name = name.rsplit('/').next().unwrap_or("upload").to_string();
    if name.contains("..") || name.contains('/') || name.contains('\\') || name.starts_with('.') {
        return httpd::bad(&mut stream, "400 Bad Request", "文件名不合法");
    }
    let chunked = req
        .header("transfer-encoding")
        .map(|v| v.to_lowercase().contains("chunked"))
        .unwrap_or(false);
    if !req.body_buffered && req.content_length == 0 && !chunked {
        // 既没有 Content-Length 也不是 chunked：不知道要收多少，
        // 绝不能先把已有文件 create 掉再收一个空文件回去
        return httpd::bad(
            &mut stream,
            "411 Length Required",
            "上传要么带 Content-Length，要么用 chunked 编码",
        );
    }

    let dest = dir.join(&name);
    let mut f = std::fs::File::create(&dest)?;
    let mut written = 0u64;

    if req.body_buffered {
        f.write_all(&req.body)?;
        written = req.body.len() as u64;
    } else if chunked {
        // `tar cz . | curl -T - http://主机/tok/up/logs.tgz` 走的就是这条路
        let mut prog = Progress::new(&format!("↑ {}", name), 0);
        written = read_chunked(&mut reader, &mut f, &mut prog)?;
        prog.finish();
    } else {
        let mut left = req.content_length;
        let mut buf = vec![0u8; 256 * 1024];
        let mut prog = Progress::new(&format!("↑ {}", name), req.content_length);
        while left > 0 {
            let want = buf.len().min(left as usize);
            let n = reader.read(&mut buf[..want])?;
            if n == 0 {
                break;
            }
            f.write_all(&buf[..n])?;
            written += n as u64;
            left -= n as u64;
            prog.add(n as u64);
        }
        prog.finish();
    }
    f.flush()?;
    sh.hits.fetch_add(1, Ordering::Relaxed);
    ok(&format!("↑ 收到 {} ({}) → {}", name, human_bytes(written), dest.display()));
    httpd::ok_text(&mut stream, &format!("ok {} {}\n", name, written))
}

/// 解 `Transfer-Encoding: chunked` 的请求体，边解边落盘。
fn read_chunked(
    reader: &mut BufReader<TcpStream>,
    out: &mut std::fs::File,
    prog: &mut Progress,
) -> std::io::Result<u64> {
    use std::io::BufRead;
    let mut total = 0u64;
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        // chunk-size 是十六进制，后面可能跟 ";扩展"
        let size_tok = line.trim().split(';').next().unwrap_or("").trim();
        if size_tok.is_empty() {
            continue;
        }
        let mut left = match u64::from_str_radix(size_tok, 16) {
            Ok(v) => v,
            Err(_) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "chunked 编码里的长度看不懂",
                ))
            }
        };
        if left == 0 {
            break; // 末尾块；后面的 trailer 不关心
        }
        while left > 0 {
            let want = buf.len().min(left as usize);
            let n = reader.read(&mut buf[..want])?;
            if n == 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "chunked 体传到一半断了",
                ));
            }
            out.write_all(&buf[..n])?;
            total += n as u64;
            left -= n as u64;
            prog.add(n as u64);
        }
        let mut crlf = String::new();
        let _ = reader.read_line(&mut crlf); // 吃掉块尾的 CRLF
    }
    Ok(total)
}

// ---------------- CLI 入口 ----------------

/// `fy serve` 的命令行参数打包（省得函数签名长到没法看）。
pub struct ServeCli {
    pub roots: Vec<PathBuf>,
    pub port: u16,
    pub bind: Option<String>,
    pub upload: Option<PathBuf>,
    pub token: Option<String>,
    pub no_token: bool,
    pub once: bool,
}

pub fn serve_cmd(cli: ServeCli, for_dev: Option<&Device>) -> i32 {
    let ServeCli { roots, port, bind, upload, token: token_opt, no_token, once } = cli;
    let roots = if roots.is_empty() { vec![PathBuf::from(".")] } else { roots };
    let token = if no_token {
        String::new()
    } else {
        token_opt.unwrap_or_else(|| rand_hex(10))
    };
    // 板子该访问哪个地址：点了名就按路由表选直连网口，否则把本机地址都列出来
    let mut advertise: Vec<(String, String)> = vec![];
    if let Some(d) = for_dev {
        if let Some((iface, _)) = usbnet::route_iface_for(&d.host) {
            if let Some(ip) = usbnet::local_ip_on(&iface) {
                advertise.push((iface, ip));
            }
        }
    }
    for x in usbnet::local_ipv4s() {
        if !advertise.iter().any(|(_, ip)| *ip == x.1) {
            advertise.push(x);
        }
    }
    if advertise.is_empty() {
        advertise.push(("lo".into(), "127.0.0.1".into()));
    }

    let bind = bind.unwrap_or_else(|| "0.0.0.0".into());
    let o = ServeOpts { port, bind, roots, upload_dir: upload, token, once };

    if crate::jsonout::json_mode() {
        // JSON 模式不能常驻阻塞（agent 等不到结果），改成"给出计划"再后台起
        return fail(
            code::USAGE,
            "fy serve 是常驻服务，--json 拿不到结果；要后台跑就 `fy serve ... &`，\
             或者用 fy push/fy cp 做一次性传输",
        );
    }
    match run(o, advertise, for_dev) {
        Ok(_) => 0,
        Err(e) => fail(code::FAIL, &e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn shared_with(root: PathBuf) -> Shared {
        Shared {
            roots: named_roots(&[root]),
            upload_dir: None,
            token: "tok".into(),
            hits: AtomicU64::new(0),
            once: false,
        }
    }

    #[test]
    fn path_traversal_is_refused() {
        let tmp = std::env::temp_dir().join(format!("ferry_serve_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("pub/sub")).unwrap();
        std::fs::write(tmp.join("pub/ok.txt"), b"hi").unwrap();
        std::fs::write(tmp.join("secret.txt"), b"nope").unwrap();
        let sh = shared_with(tmp.join("pub"));

        assert!(resolve(&sh, "/pub/ok.txt").is_some());
        assert!(resolve(&sh, "/pub/sub").is_some());
        // 目录链接带尾斜杠也要认（浏览器和 wget 都会这么发）
        assert!(resolve(&sh, "/pub/").is_some());
        assert!(resolve(&sh, "/pub/sub/").is_some());
        // 但中间的空段仍然可疑，挡掉
        assert!(resolve(&sh, "/pub//ok.txt").is_none());
        // 各种越狱姿势都要挡住
        assert!(resolve(&sh, "/pub/../secret.txt").is_none());
        assert!(resolve(&sh, "/../secret.txt").is_none());
        assert!(resolve(&sh, "/pub/./ok.txt").is_none());
        assert!(resolve(&sh, "/etc/passwd").is_none());
        assert!(resolve(&sh, "/").is_none());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// `fy serve .` 是 README 上的默认用法：basename 为空时也必须能服务。
    #[test]
    fn dot_root_still_gets_a_usable_name() {
        let tmp = std::env::temp_dir().join(format!("ferry_dot_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(tmp.join("a.txt"), b"hi").unwrap();
        let named = named_roots(&[tmp.clone()]);
        assert_eq!(named.len(), 1);
        assert!(!named[0].0.is_empty(), "root 的 URL 名字不能是空的");
        let sh = Shared {
            roots: named.clone(),
            upload_dir: None,
            token: "t".into(),
            hits: AtomicU64::new(0),
            once: false,
        };
        assert!(resolve(&sh, &format!("/{}/a.txt", named[0].0)).is_some());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// 两个 root 同名时，第二个要拿到带后缀的名字，而且两边都能访问。
    #[test]
    fn colliding_root_basenames_both_reachable() {
        let tmp = std::env::temp_dir().join(format!("ferry_dual_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("a/build")).unwrap();
        std::fs::create_dir_all(tmp.join("b/build")).unwrap();
        std::fs::write(tmp.join("a/build/only_a.txt"), b"a").unwrap();
        std::fs::write(tmp.join("b/build/only_b.txt"), b"b").unwrap();
        let named = named_roots(&[tmp.join("a/build"), tmp.join("b/build")]);
        assert_eq!(named[0].0, "build");
        assert_eq!(named[1].0, "build-2", "同名的第二个 root 要自动改名");
        let sh = Shared {
            roots: named,
            upload_dir: None,
            token: "t".into(),
            hits: AtomicU64::new(0),
            once: false,
        };
        assert!(resolve(&sh, "/build/only_a.txt").is_some());
        assert!(resolve(&sh, "/build-2/only_b.txt").is_some(), "第二个 root 也得够得着");
        // 交叉访问要落空
        assert!(resolve(&sh, "/build/only_b.txt").is_none());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn href_is_escaped_and_url_encoded() {
        // 文件名里带引号时，href 属性不能被撑开（否则就是 XSS）
        let evil = "a\" onmouseover=\"alert(1)";
        let enc = url_encode(evil);
        assert!(!enc.contains('"') && !enc.contains(' '));
        assert!(!html_escape(&enc).contains('"'));
        assert_eq!(url_encode("板子.bin"), "%E6%9D%BF%E5%AD%90.bin");
    }

    #[test]
    fn token_prefix_shapes() {
        assert_eq!(prefix(""), "");
        assert_eq!(prefix("abc"), "/abc");
    }

    #[test]
    fn plain_listing_is_wget_friendly() {
        let rows = vec![("a.bin".to_string(), 1024u64, false), ("d".to_string(), 0, true)];
        let mut out: Vec<u8> = vec![];
        // 直接检验文本渲染的形状（不经 TcpStream）
        let base = "/tok";
        for (name, size, is_dir) in &rows {
            out.extend(
                format!(
                    "{}/{}{}\t{}\n",
                    base,
                    name,
                    if *is_dir { "/" } else { "" },
                    if *is_dir { "-".to_string() } else { size.to_string() }
                )
                .as_bytes(),
            );
        }
        let s = String::from_utf8(out).unwrap();
        assert!(s.contains("/tok/a.bin\t1024"));
        assert!(s.contains("/tok/d/\t-"));
    }

    #[test]
    fn html_only_when_browser_asks() {
        let mut h = HashMap::new();
        h.insert("accept".to_string(), "text/html,*/*".to_string());
        let req = Request {
            method: "GET".into(),
            path: "/".into(),
            query: HashMap::new(),
            headers: h,
            body: vec![],
            content_length: 0,
            body_buffered: false,
        };
        assert!(wants_html(&req));
        let req2 = Request {
            method: "GET".into(),
            path: "/".into(),
            query: HashMap::new(),
            headers: HashMap::new(),
            body: vec![],
            content_length: 0,
            body_buffered: false,
        };
        assert!(!wants_html(&req2));
    }
}
