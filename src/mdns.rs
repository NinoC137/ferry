//! 零依赖 mDNS 一次性查询（RFC 6762 的 one-shot query 那一半）。
//!
//! `fy scan` 原本靠"把网段里每个 IP 都敲一遍 22/23/5555 端口"来找板子——
//! /24 还行，/16 就没法看了，而且 U-Boot 里的板子、只开 telnet 的板子容易漏。
//! mDNS 是**主动问、大家答**：一个组播包出去，1.5 秒内该露面的都露面了，
//! 顺带还能拿到主机名和服务类型。
//!
//! 实现要点：
//! - 用**临时端口**发查询并把 QU 位（qclass 最高位）置上，让应答直接单播回来。
//!   这样不必抢占 5353（macOS 上被 mDNSResponder 占着，抢不到）。
//! - 本机每个 IPv4 各绑一个 socket 发一遍：USB 直连网段那种非默认路由的网口
//!   才不会被漏掉。
//! - 应答解析支持名字压缩指针，并且**限制跳转次数**，别人发个环指针也打不死我们。

use std::collections::BTreeMap;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};
use std::time::{Duration, Instant};

const MDNS_ADDR: Ipv4Addr = Ipv4Addr::new(224, 0, 0, 251);
const MDNS_PORT: u16 = 5353;

/// 值得问一嘴的服务类型：ssh、adb（Android 11+ 无线调试）、通用工作站、以及
/// 树莓派/OpenWrt 之类常见的设备信息服务。
pub const SERVICES: &[&str] = &[
    "_ssh._tcp.local",
    "_sftp-ssh._tcp.local",
    "_workstation._tcp.local",
    "_adb-tls-connect._tcp.local",
    "_adb._tcp.local",
    "_telnet._tcp.local",
    "_http._tcp.local",
    "_device-info._tcp.local",
];

#[derive(Debug, Clone, Default)]
pub struct MdnsHit {
    pub ip: String,
    /// 主机名（去掉尾巴上的 `.local`）
    pub host: String,
    /// 看到的服务实例名
    pub services: Vec<String>,
    pub port: u16,
}

// ---------------- 报文构造 ----------------

fn encode_name(name: &str, out: &mut Vec<u8>) {
    for label in name.trim_end_matches('.').split('.') {
        let b = label.as_bytes();
        // 单个 label 最长 63 字节，超了就截断（我们查的名字都很短，走不到这儿）
        let n = b.len().min(63);
        out.push(n as u8);
        out.extend_from_slice(&b[..n]);
    }
    out.push(0);
}

/// 构造一个 PTR 查询，qclass 带 QU 位（要求单播应答）。
pub fn build_query(names: &[&str]) -> Vec<u8> {
    let mut p = Vec::with_capacity(64);
    p.extend_from_slice(&[0, 0]); // ID：mDNS 里无所谓
    p.extend_from_slice(&[0, 0]); // flags：标准查询
    p.extend_from_slice(&(names.len() as u16).to_be_bytes()); // QDCOUNT
    p.extend_from_slice(&[0, 0, 0, 0, 0, 0]); // AN/NS/AR
    for n in names {
        encode_name(n, &mut p);
        p.extend_from_slice(&12u16.to_be_bytes()); // QTYPE=PTR
        p.extend_from_slice(&0x8001u16.to_be_bytes()); // QCLASS=IN | QU
    }
    p
}

// ---------------- 报文解析 ----------------

struct Reader<'a> {
    b: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn u8(&mut self) -> Option<u8> {
        let v = *self.b.get(self.pos)?;
        self.pos += 1;
        Some(v)
    }
    fn u16(&mut self) -> Option<u16> {
        let hi = self.u8()? as u16;
        let lo = self.u8()? as u16;
        Some((hi << 8) | lo)
    }
    fn u32(&mut self) -> Option<u32> {
        let a = self.u16()? as u32;
        let b = self.u16()? as u32;
        Some((a << 16) | b)
    }
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let s = self.b.get(self.pos..self.pos + n)?;
        self.pos += n;
        Some(s)
    }
    /// 读一个（可能被压缩的）域名。
    fn name(&mut self) -> Option<String> {
        let mut out = String::new();
        let mut jumps = 0;
        let mut pos = self.pos;
        let mut moved = false;
        loop {
            let len = *self.b.get(pos)? as usize;
            if len == 0 {
                pos += 1;
                break;
            }
            if len & 0xc0 == 0xc0 {
                // 压缩指针；只在第一次跳转前记录真实游标
                let lo = *self.b.get(pos + 1)? as usize;
                let target = ((len & 0x3f) << 8) | lo;
                if !moved {
                    self.pos = pos + 2;
                    moved = true;
                }
                jumps += 1;
                if jumps > 16 || target >= self.b.len() {
                    return None; // 环指针 / 越界：别陪它玩
                }
                pos = target;
                continue;
            }
            let s = self.b.get(pos + 1..pos + 1 + len)?;
            if !out.is_empty() {
                out.push('.');
            }
            out.push_str(&String::from_utf8_lossy(s));
            pos += 1 + len;
        }
        if !moved {
            self.pos = pos;
        }
        Some(out)
    }
}

#[derive(Debug, Default)]
pub struct Parsed {
    /// 主机名 → IPv4
    pub a: BTreeMap<String, String>,
    /// 服务实例名 → (目标主机, 端口)
    pub srv: BTreeMap<String, (String, u16)>,
    /// 服务类型 → 实例名们
    pub ptr: Vec<(String, String)>,
}

pub fn parse(buf: &[u8]) -> Option<Parsed> {
    let mut r = Reader { b: buf, pos: 0 };
    let _id = r.u16()?;
    let _flags = r.u16()?;
    let qd = r.u16()?;
    let an = r.u16()?;
    let ns = r.u16()?;
    let ar = r.u16()?;
    for _ in 0..qd {
        r.name()?;
        r.u16()?;
        r.u16()?;
    }
    let mut out = Parsed::default();
    let total = an as u32 + ns as u32 + ar as u32;
    for _ in 0..total {
        let name = r.name()?;
        let rtype = r.u16()?;
        let _class = r.u16()?;
        let _ttl = r.u32()?;
        let rdlen = r.u16()? as usize;
        let end = r.pos + rdlen;
        if end > buf.len() {
            return Some(out); // 截断的包：把已解出来的留着
        }
        match rtype {
            1 => {
                // A
                if let Some(d) = r.take(4) {
                    out.a.insert(name, format!("{}.{}.{}.{}", d[0], d[1], d[2], d[3]));
                }
            }
            12 => {
                // PTR
                let inst = r.name().unwrap_or_default();
                if !inst.is_empty() {
                    out.ptr.push((name, inst));
                }
            }
            33 => {
                // SRV: prio(2) weight(2) port(2) target
                let _prio = r.u16()?;
                let _w = r.u16()?;
                let port = r.u16()?;
                let target = r.name().unwrap_or_default();
                out.srv.insert(name, (target, port));
            }
            _ => {}
        }
        r.pos = end; // 不管上面读没读完，都对齐到这条记录的末尾
    }
    Some(out)
}

// ---------------- 查询 ----------------

/// 发一轮 mDNS 查询并收集 `wait` 时间内的应答。
/// `local_ips` 是本机各网口地址：每个都发一遍，USB 直连那种网段才不会漏。
pub fn discover(local_ips: &[String], wait: Duration) -> Vec<MdnsHit> {
    let query = build_query(SERVICES);
    let dst = SocketAddr::V4(SocketAddrV4::new(MDNS_ADDR, MDNS_PORT));
    let mut socks: Vec<UdpSocket> = vec![];

    let mut binds: Vec<String> = vec!["0.0.0.0".to_string()];
    binds.extend(local_ips.iter().cloned());
    for b in binds {
        if let Ok(s) = UdpSocket::bind((b.as_str(), 0)) {
            let _ = s.set_multicast_ttl_v4(255);
            let _ = s.set_multicast_loop_v4(true);
            let _ = s.set_read_timeout(Some(Duration::from_millis(200)));
            if s.send_to(&query, dst).is_ok() {
                socks.push(s);
            }
        }
    }
    if socks.is_empty() {
        return vec![];
    }

    let mut agg = Parsed::default();
    let deadline = Instant::now() + wait;
    let mut buf = [0u8; 4096];
    while Instant::now() < deadline {
        let mut got_any = false;
        for s in &socks {
            match s.recv_from(&mut buf) {
                Ok((n, _from)) => {
                    got_any = true;
                    if let Some(p) = parse(&buf[..n]) {
                        agg.a.extend(p.a);
                        agg.srv.extend(p.srv);
                        agg.ptr.extend(p.ptr);
                    }
                }
                Err(_) => continue, // 超时是常态
            }
        }
        if !got_any {
            std::thread::sleep(Duration::from_millis(30));
        }
    }
    fold(agg)
}

/// 把零散的 A/SRV/PTR 记录拼成"一台设备一行"。
pub fn fold(p: Parsed) -> Vec<MdnsHit> {
    let mut by_ip: BTreeMap<String, MdnsHit> = BTreeMap::new();
    // 先把 SRV 指向的主机解析成 IP
    for (inst, (target, port)) in &p.srv {
        if let Some(ip) = p.a.get(target) {
            let e = by_ip.entry(ip.clone()).or_insert_with(|| MdnsHit {
                ip: ip.clone(),
                host: target.trim_end_matches(".local").to_string(),
                ..Default::default()
            });
            let short = inst.split('.').next().unwrap_or(inst).to_string();
            if !e.services.contains(&short) {
                e.services.push(short);
            }
            if e.port == 0 {
                e.port = *port;
            }
        }
    }
    // 只广播了 A 记录、没有 SRV 的主机也算一台
    for (host, ip) in &p.a {
        by_ip.entry(ip.clone()).or_insert_with(|| MdnsHit {
            ip: ip.clone(),
            host: host.trim_end_matches(".local").to_string(),
            ..Default::default()
        });
    }
    by_ip.into_values().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_is_wellformed() {
        let q = build_query(&["_ssh._tcp.local"]);
        assert_eq!(&q[0..4], &[0, 0, 0, 0]);
        assert_eq!(u16::from_be_bytes([q[4], q[5]]), 1, "QDCOUNT");
        // 名字编码：4"_ssh" 4"_tcp" 5"local" 0
        assert_eq!(q[12], 4);
        assert_eq!(&q[13..17], b"_ssh");
        let tail = &q[q.len() - 4..];
        assert_eq!(u16::from_be_bytes([tail[0], tail[1]]), 12, "QTYPE=PTR");
        assert_eq!(u16::from_be_bytes([tail[2], tail[3]]), 0x8001, "QU 位要置上");
    }

    /// 手搓一个应答包：A + SRV，SRV 的 target 用压缩指针指回 A 的名字。
    fn fake_response() -> Vec<u8> {
        let mut p = vec![0, 0, 0x84, 0x00]; // 应答标志
        p.extend_from_slice(&0u16.to_be_bytes()); // QD
        p.extend_from_slice(&2u16.to_be_bytes()); // AN = 2
        p.extend_from_slice(&[0, 0, 0, 0]);
        let name_off = p.len();
        encode_name("rk3588.local", &mut p);
        p.extend_from_slice(&1u16.to_be_bytes()); // A
        p.extend_from_slice(&1u16.to_be_bytes());
        p.extend_from_slice(&120u32.to_be_bytes());
        p.extend_from_slice(&4u16.to_be_bytes());
        p.extend_from_slice(&[192, 168, 1, 37]);
        // SRV: 实例名 → 压缩指针指向 rk3588.local
        encode_name("board._ssh._tcp.local", &mut p);
        p.extend_from_slice(&33u16.to_be_bytes());
        p.extend_from_slice(&1u16.to_be_bytes());
        p.extend_from_slice(&120u32.to_be_bytes());
        let rd = {
            let mut r = vec![];
            r.extend_from_slice(&0u16.to_be_bytes());
            r.extend_from_slice(&0u16.to_be_bytes());
            r.extend_from_slice(&22u16.to_be_bytes());
            r.extend_from_slice(&[0xc0 | ((name_off >> 8) as u8), (name_off & 0xff) as u8]);
            r
        };
        p.extend_from_slice(&(rd.len() as u16).to_be_bytes());
        p.extend_from_slice(&rd);
        p
    }

    #[test]
    fn parses_a_and_compressed_srv() {
        let p = parse(&fake_response()).expect("应该解得开");
        assert_eq!(p.a.get("rk3588.local").map(|s| s.as_str()), Some("192.168.1.37"));
        let (target, port) = p.srv.get("board._ssh._tcp.local").expect("有 SRV");
        assert_eq!(target, "rk3588.local", "压缩指针没跟对");
        assert_eq!(*port, 22);

        let hits = fold(p);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].ip, "192.168.1.37");
        assert_eq!(hits[0].host, "rk3588");
        assert_eq!(hits[0].services, vec!["board".to_string()]);
        assert_eq!(hits[0].port, 22);
    }

    #[test]
    fn survives_hostile_packets() {
        // 空包 / 截断 / 自指的压缩指针，都不许 panic 或死循环
        assert!(parse(&[]).is_none());
        assert!(parse(&[0, 0, 0x84, 0]).is_none());
        let mut loopy = vec![0, 0, 0x84, 0, 0, 0, 0, 1, 0, 0, 0, 0];
        loopy.extend_from_slice(&[0xc0, 0x0c]); // 指向自己
        loopy.extend_from_slice(&[0, 1, 0, 1, 0, 0, 0, 60, 0, 4, 1, 2, 3, 4]);
        let _ = parse(&loopy); // 只要求它返回，不许卡住
    }

    #[test]
    fn discover_without_responders_returns_empty_fast() {
        let t = Instant::now();
        let hits = discover(&[], Duration::from_millis(300));
        assert!(hits.is_empty() || hits.iter().all(|h| !h.ip.is_empty()));
        assert!(t.elapsed() < Duration::from_secs(3));
    }
}
