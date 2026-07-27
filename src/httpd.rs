//! 极简 HTTP/1.1 服务（零依赖）：请求解析 + 响应构造 + 路由循环。
//! 支持 WebSocket 升级（把裸 TcpStream 交给上层）。
//!
//! `fy ui` 绑 127.0.0.1；`fy serve` 要给板子下载文件，会绑到局域网地址，
//! 所以这里额外提供了 Range 断点续传、流式发送、以及**不把请求体读进内存**的
//! `serve_full`（上传几百 MB 的镜像不能 OOM）。

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};

pub struct Request {
    #[allow(dead_code)]
    pub method: String,
    pub path: String,
    pub query: HashMap<String, String>,
    pub headers: HashMap<String, String>,
    #[allow(dead_code)]
    pub body: Vec<u8>,
    /// 声明的请求体长度。`body` 为空但它 > 0 时，表示体还留在连接里等你流式读。
    pub content_length: u64,
    /// true = 体已经读进 `body`；false = 体还在流里。
    pub body_buffered: bool,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(&name.to_lowercase()).map(|s| s.as_str())
    }
    pub fn q(&self, key: &str) -> Option<&str> {
        self.query.get(key).map(|s| s.as_str())
    }
    pub fn is_websocket(&self) -> bool {
        self.header("upgrade").map(|u| u.eq_ignore_ascii_case("websocket")).unwrap_or(false)
    }
    /// 解析 `Range: bytes=start-[end]`（只支持单区间，够用了）。
    pub fn range(&self, size: u64) -> Option<(u64, u64)> {
        let h = self.header("range")?;
        let spec = h.trim().strip_prefix("bytes=")?;
        if spec.contains(',') {
            return None;
        }
        let (a, b) = spec.split_once('-')?;
        let (start, end) = if a.is_empty() {
            // bytes=-N：最后 N 字节
            let n: u64 = b.trim().parse().ok()?;
            (size.saturating_sub(n), size.saturating_sub(1))
        } else {
            let start: u64 = a.trim().parse().ok()?;
            let end = if b.trim().is_empty() {
                size.saturating_sub(1)
            } else {
                b.trim().parse::<u64>().ok()?.min(size.saturating_sub(1))
            };
            (start, end)
        };
        if size == 0 || start > end || start >= size {
            return None;
        }
        Some((start, end))
    }
}

/// 单次请求体的内存上限：超过就不预读，交给上层流式处理。
const BODY_INLINE_MAX: u64 = 8 * 1024 * 1024;

pub fn parse_request(stream: &mut BufReader<TcpStream>) -> Option<Request> {
    let mut line = String::new();
    if stream.read_line(&mut line).ok()? == 0 {
        return None;
    }
    let mut parts = line.trim_end().split_whitespace();
    let method = parts.next()?.to_string();
    let raw_path = parts.next()?.to_string();
    let (path, query) = split_query(&raw_path);

    let mut headers = HashMap::new();
    // 头部条数封顶：`fy serve` 会绑到局域网，别让一个连接把内存吃光
    for _ in 0..200 {
        let mut h = String::new();
        if stream.read_line(&mut h).ok()? == 0 {
            break;
        }
        let h = h.trim_end();
        if h.is_empty() {
            break;
        }
        if let Some((k, v)) = h.split_once(':') {
            headers.insert(k.trim().to_lowercase(), v.trim().to_string());
        }
    }
    let content_length = headers
        .get("content-length")
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(0);
    let mut body = Vec::new();
    let mut body_buffered = false;
    if content_length > 0 && content_length <= BODY_INLINE_MAX {
        body.resize(content_length as usize, 0);
        stream.read_exact(&mut body).ok()?;
        body_buffered = true;
    }
    Some(Request { method, path, query, headers, body, content_length, body_buffered })
}

fn split_query(raw: &str) -> (String, HashMap<String, String>) {
    let mut map = HashMap::new();
    match raw.split_once('?') {
        Some((p, q)) => {
            for kv in q.split('&') {
                if let Some((k, v)) = kv.split_once('=') {
                    map.insert(url_decode(k), url_decode(v));
                } else if !kv.is_empty() {
                    map.insert(url_decode(kv), String::new());
                }
            }
            (p.to_string(), map)
        }
        None => (raw.to_string(), map),
    }
}

pub fn url_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' if i + 2 < b.len() => {
                let hi = hexval(b[i + 1]);
                let lo = hexval(b[i + 2]);
                if let (Some(h), Some(l)) = (hi, lo) {
                    out.push(h * 16 + l);
                    i += 3;
                    continue;
                }
                out.push(b[i]);
                i += 1;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hexval(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

// ---------------- 响应 ----------------

pub fn respond(stream: &mut TcpStream, status: &str, content_type: &str, body: &[u8]) -> std::io::Result<()> {
    let head = format!(
        "HTTP/1.1 {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        status,
        content_type,
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()
}

pub fn ok_json(stream: &mut TcpStream, json: &str) -> std::io::Result<()> {
    respond(stream, "200 OK", "application/json; charset=utf-8", json.as_bytes())
}
pub fn ok_html(stream: &mut TcpStream, html: &str) -> std::io::Result<()> {
    respond(stream, "200 OK", "text/html; charset=utf-8", html.as_bytes())
}
pub fn not_found(stream: &mut TcpStream) -> std::io::Result<()> {
    respond(stream, "404 Not Found", "text/plain; charset=utf-8", b"not found")
}
pub fn ok_text(stream: &mut TcpStream, text: &str) -> std::io::Result<()> {
    respond(stream, "200 OK", "text/plain; charset=utf-8", text.as_bytes())
}
pub fn bad(stream: &mut TcpStream, status: &str, msg: &str) -> std::io::Result<()> {
    respond(stream, status, "text/plain; charset=utf-8", msg.as_bytes())
}

/// 按 MIME 猜个类型。猜不出就 octet-stream —— 浏览器会直接下载，正合我意。
pub fn mime_of(name: &str) -> &'static str {
    let ext = name.rsplit('.').next().unwrap_or("").to_lowercase();
    match ext.as_str() {
        "html" | "htm" => "text/html; charset=utf-8",
        "txt" | "log" | "md" | "cfg" | "conf" | "ini" => "text/plain; charset=utf-8",
        "json" => "application/json; charset=utf-8",
        "js" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "svg" => "image/svg+xml",
        "pdf" => "application/pdf",
        "gz" | "tgz" => "application/gzip",
        "zip" => "application/zip",
        "tar" => "application/x-tar",
        _ => "application/octet-stream",
    }
}

/// 流式发文件，支持 Range 续传。板子上 `wget -c` 断了能接着下。
pub fn send_file(
    stream: &mut TcpStream,
    path: &std::path::Path,
    req: &Request,
    filename: &str,
) -> std::io::Result<u64> {
    use std::io::Seek;
    let meta = std::fs::metadata(path)?;
    let size = meta.len();
    let mut f = std::fs::File::open(path)?;
    let (start, end) = req.range(size).unwrap_or((0, size.saturating_sub(1)));
    let partial = req.range(size).is_some();
    let len = if size == 0 { 0 } else { end - start + 1 };
    let head = if partial {
        format!(
            "HTTP/1.1 206 Partial Content\r\nContent-Type: {}\r\nContent-Length: {}\r\nContent-Range: bytes {}-{}/{}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n",
            mime_of(filename), len, start, end, size
        )
    } else {
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: {}\r\nContent-Length: {}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n",
            mime_of(filename), len
        )
    };
    stream.write_all(head.as_bytes())?;
    if req.method == "HEAD" || len == 0 {
        return stream.flush().map(|_| 0);
    }
    f.seek(std::io::SeekFrom::Start(start))?;
    let mut left = len;
    let mut buf = vec![0u8; 256 * 1024];
    let mut sent = 0u64;
    while left > 0 {
        let want = buf.len().min(left as usize);
        let n = f.read(&mut buf[..want])?;
        if n == 0 {
            break;
        }
        stream.write_all(&buf[..n])?;
        sent += n as u64;
        left -= n as u64;
    }
    stream.flush()?;
    Ok(sent)
}

/// 启动服务：对每个连接解析一次请求并交给 handler。
/// handler 返回 false 表示它已经接管了这个连接（如 WebSocket），本循环不再动它。
pub fn serve<F>(listener: TcpListener, handler: F)
where
    F: Fn(Request, TcpStream) + Send + Sync + 'static,
{
    serve_full(listener, move |req, _reader, stream| handler(req, stream))
}

/// 和 `serve` 一样，但把 BufReader 也交给 handler —— 需要**流式读请求体**
/// （大文件上传）时用这个，避免把整个镜像读进内存。
pub fn serve_full<F>(listener: TcpListener, handler: F)
where
    F: Fn(Request, BufReader<TcpStream>, TcpStream) + Send + Sync + 'static,
{
    let handler = std::sync::Arc::new(handler);
    for conn in listener.incoming() {
        let stream = match conn {
            Ok(s) => s,
            Err(_) => continue,
        };
        let handler = handler.clone();
        std::thread::spawn(move || {
            // 读头部时给个超时：绑在 0.0.0.0 上的 `fy serve` 不该被一个
            // 半开的连接永久占住一个线程（慢速攻击，或者只是板子拔了网线）
            let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(30)));
            let mut reader = BufReader::new(match stream.try_clone() {
                Ok(s) => s,
                Err(_) => return,
            });
            if let Some(req) = parse_request(&mut reader) {
                // 头部读完就撤掉超时：正文可能很大，传得慢是正常的
                let _ = stream.set_read_timeout(None);
                handler(req, reader, stream);
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req_with(range: &str) -> Request {
        let mut headers = HashMap::new();
        if !range.is_empty() {
            headers.insert("range".into(), range.into());
        }
        Request {
            method: "GET".into(),
            path: "/x".into(),
            query: HashMap::new(),
            headers,
            body: vec![],
            content_length: 0,
            body_buffered: false,
        }
    }

    #[test]
    fn range_parsing() {
        assert_eq!(req_with("bytes=0-99").range(1000), Some((0, 99)));
        assert_eq!(req_with("bytes=500-").range(1000), Some((500, 999)));
        assert_eq!(req_with("bytes=-100").range(1000), Some((900, 999)));
        // 越界 end 被夹到文件末尾
        assert_eq!(req_with("bytes=0-99999").range(1000), Some((0, 999)));
        // 起点超出文件 / 空文件 / 多区间 / 没有头 → 不认
        assert_eq!(req_with("bytes=1000-1001").range(1000), None);
        assert_eq!(req_with("bytes=0-9").range(0), None);
        assert_eq!(req_with("bytes=0-9,20-29").range(1000), None);
        assert_eq!(req_with("").range(1000), None);
    }

    #[test]
    fn url_decoding_and_mime() {
        assert_eq!(url_decode("a%20b"), "a b");
        assert_eq!(url_decode("%E6%9D%BF%E5%AD%90"), "板子");
        assert_eq!(url_decode("a+b"), "a b");
        assert_eq!(mime_of("x.tar.gz"), "application/gzip");
        assert_eq!(mime_of("boot.img"), "application/octet-stream");
        assert_eq!(mime_of("README.md"), "text/plain; charset=utf-8");
    }
}
