//! 传输引擎：**进度条 + 断点续传 + sha256 校验**，零依赖。
//!
//! 为什么不直接用 scp/rsync：
//! - scp 断了就得从头来，几百 MB 的 rootfs 在弱网/USB 网卡上很痛；
//! - 板子上多半没有 rsync；
//! - scp 传完不校验，坏了你不知道，烧进 flash 才发现。
//!
//! 这里的做法：**一条 ssh 管道 + 板上只用 cat/head/tail/wc**（busybox 就够），
//! 传前探测远端尺寸、传中画进度、传后两边对 sha256。断点续传前会先比对
//! **已有部分的前缀哈希**——不一致就老实全量重传，绝不在错误的前缀上追加。
//!
//! adb 通道走 `adb push/pull`（原生更快，且自带进度），但**照样做哈希校验**，
//! 并在哈希一致时直接跳过传输（等价于文件粒度的续传）。

use crate::adbx;
use crate::config::{Device, Transport};
use crate::hash;
use crate::sshx;
use crate::util::*;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const CHUNK: usize = 256 * 1024;

#[derive(Debug, Clone)]
pub struct XferOpts {
    /// 允许在远端已有的部分之后续传（前缀哈希一致才续）。
    pub resume: bool,
    /// 传完做全量 sha256 比对。
    pub verify: bool,
    /// 无视远端已有内容，强制全量重传。
    pub force: bool,
    /// 校验发现两边已一致时是否跳过（`--force` 时无效）。
    pub skip_same: bool,
}

impl Default for XferOpts {
    fn default() -> Self {
        XferOpts { resume: true, verify: true, force: false, skip_same: true }
    }
}

/// 单个文件的传输结果。`fy push --json` 里 `files[]` 的元素就是它。
#[derive(Debug, Clone, Default)]
pub struct FileResult {
    pub name: String,
    pub remote: String,
    pub total: u64,
    /// 本次真正走线的字节数（跳过时是 0，续传时是剩余部分）。
    pub sent: u64,
    pub resumed_from: u64,
    pub skipped: bool,
    pub verified: bool,
    pub secs: f64,
}

impl FileResult {
    pub fn rate(&self) -> f64 {
        if self.secs > 0.0 {
            self.sent as f64 / self.secs
        } else {
            0.0
        }
    }
}

// ---------------- 远端探测 ----------------

#[derive(Debug, Default, Clone)]
pub struct RemoteInfo {
    /// 'd' 目录 / 'f' 文件 / 'n' 不存在
    pub kind: char,
    /// 文件字节数；不是普通文件时为 -1
    pub size: i64,
    /// sha256sum / shasum / busybox / openssl / none
    pub hasher: String,
    /// 目标所在分区剩余 KB；拿不到为 -1
    pub free_kb: i64,
}

fn rexec(d: &Device, cmd: &str) -> std::io::Result<Output> {
    match d.transport {
        Transport::Ssh => sshx::exec_capture(d, cmd),
        Transport::Adb => adbx::exec_capture(d, cmd),
        Transport::Serial => Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "串口通道不支持文件传输，先 `fy up <设备>` 爬升到 ssh",
        )),
    }
}

/// 一次往返把该问的都问清楚：类型、尺寸、有没有哈希工具、还剩多少空间。
pub fn probe_remote(d: &Device, remote: &str) -> std::io::Result<RemoteInfo> {
    let q = shell_quote(remote);
    let cmd = format!(
        "p={q}; \
         if [ -d \"$p\" ]; then echo T:d; elif [ -e \"$p\" ]; then echo T:f; else echo T:n; fi; \
         if [ -f \"$p\" ]; then echo S:$(wc -c < \"$p\" 2>/dev/null | tr -d ' \t'); else echo S:-1; fi; \
         if command -v sha256sum >/dev/null 2>&1; then echo H:sha256sum; \
         elif command -v shasum >/dev/null 2>&1; then echo H:shasum; \
         elif busybox sha256sum /dev/null >/dev/null 2>&1; then echo H:busybox; \
         elif command -v openssl >/dev/null 2>&1; then echo H:openssl; \
         else echo H:none; fi; \
         echo F:$(df -k \"$(dirname \"$p\")\" 2>/dev/null | tail -1)",
        q = q
    );
    let out = rexec(d, &cmd)?;
    let mut info = RemoteInfo { kind: 'n', size: -1, hasher: "none".into(), free_kb: -1 };
    for line in out.stdout.lines() {
        let line = line.trim();
        if let Some(v) = line.strip_prefix("T:") {
            info.kind = v.chars().next().unwrap_or('n');
        } else if let Some(v) = line.strip_prefix("S:") {
            info.size = v.trim().parse().unwrap_or(-1);
        } else if let Some(v) = line.strip_prefix("H:") {
            info.hasher = v.trim().to_string();
        } else if let Some(v) = line.strip_prefix("F:") {
            // df -k 的最后一行：Filesystem 1K-blocks Used Available ...
            let cols: Vec<&str> = v.split_whitespace().collect();
            if cols.len() >= 4 {
                info.free_kb = cols[cols.len() - 3].parse().unwrap_or(-1);
            }
        }
    }
    Ok(info)
}

fn remote_hash_cmd(hasher: &str, path_q: &str, prefix: Option<u64>) -> Option<String> {
    let c = match (hasher, prefix) {
        ("sha256sum", None) => format!("sha256sum {}", path_q),
        ("sha256sum", Some(n)) => format!("head -c {} {} | sha256sum", n, path_q),
        ("shasum", None) => format!("shasum -a 256 {}", path_q),
        ("shasum", Some(n)) => format!("head -c {} {} | shasum -a 256", n, path_q),
        ("busybox", None) => format!("busybox sha256sum {}", path_q),
        ("busybox", Some(n)) => format!("busybox head -c {} {} | busybox sha256sum", n, path_q),
        ("openssl", None) => format!("openssl dgst -sha256 {}", path_q),
        ("openssl", Some(n)) => format!("head -c {} {} | openssl dgst -sha256", n, path_q),
        _ => return None,
    };
    Some(c)
}

/// 从任意命令输出里抠出那串 64 位十六进制。
pub fn extract_sha256(s: &str) -> Option<String> {
    for tok in s.split(|c: char| !c.is_ascii_hexdigit()) {
        if tok.len() == 64 && tok.chars().all(|c| c.is_ascii_hexdigit()) {
            return Some(tok.to_ascii_lowercase());
        }
    }
    None
}

fn remote_sha256(d: &Device, hasher: &str, remote: &str, prefix: Option<u64>) -> Option<String> {
    let cmd = remote_hash_cmd(hasher, &shell_quote(remote), prefix)?;
    let out = rexec(d, &cmd).ok()?;
    extract_sha256(&out.stdout)
}

// ---------------- push ----------------

/// 把本地文件/目录送上板子。remote 以 `/` 结尾或本身是目录时，按目录处理。
pub fn push(d: &Device, local: &Path, remote: &str, o: &XferOpts) -> Result<Vec<FileResult>, String> {
    if d.transport == Transport::Serial {
        return Err("串口通道传不了文件，先 `fy up <设备>` 爬升到 ssh".into());
    }
    if !local.exists() {
        return Err(format!("本地路径不存在: {}", local.display()));
    }
    if local.is_dir() {
        return push_dir(d, local, remote, o);
    }
    let dest = resolve_push_dest(d, local, remote)?;
    let mut total_prog = None;
    Ok(vec![push_one(d, local, &dest, o, &mut total_prog)?])
}

/// 决定文件最终落在板子的哪个路径。
fn resolve_push_dest(d: &Device, local: &Path, remote: &str) -> Result<String, String> {
    let base = local
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .ok_or_else(|| "本地路径没有文件名".to_string())?;
    if remote.ends_with('/') {
        return Ok(format!("{}{}", remote, base));
    }
    // 远端已存在且是目录 → 落进去
    match probe_remote(d, remote) {
        Ok(i) if i.kind == 'd' => Ok(format!("{}/{}", remote.trim_end_matches('/'), base)),
        _ => Ok(remote.to_string()),
    }
}

fn push_one(
    d: &Device,
    local: &Path,
    remote: &str,
    o: &XferOpts,
    shared: &mut Option<Progress>,
) -> Result<FileResult, String> {
    let meta = std::fs::metadata(local).map_err(|e| format!("读不到 {}: {}", local.display(), e))?;
    let total = meta.len();
    let name = local.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
    let mut r = FileResult { name: name.clone(), remote: remote.to_string(), total, ..Default::default() };

    if dry() {
        info(&format!(
            "DRY: 推 {} → {}:{}（{}）",
            local.display(),
            d.name,
            remote,
            human_bytes(total)
        ));
        return Ok(r);
    }

    // adb 通道：交给 adb push，但照样校验/跳过
    if d.transport == Transport::Adb {
        return push_adb(d, local, remote, o, total, name);
    }

    let t0 = std::time::Instant::now();
    let info_r = probe_remote(d, remote).map_err(|e| format!("探测远端失败: {}", e))?;
    if info_r.kind == 'd' {
        return Err(format!("远端 {} 是个目录，给个具体文件名", remote));
    }

    // 已经一模一样？直接跳过
    let local_hash = if o.verify || (o.skip_same && info_r.size as u64 == total) {
        Some(hash::sha256_file(local, None).map_err(|e| e.to_string())?)
    } else {
        None
    };
    if !o.force && o.skip_same && info_r.size >= 0 && info_r.size as u64 == total {
        if let (Some(lh), Some(rh)) = (&local_hash, remote_sha256(d, &info_r.hasher, remote, None)) {
            if *lh == rh {
                r.skipped = true;
                r.verified = true;
                r.secs = t0.elapsed().as_secs_f64();
                return Ok(r);
            }
        }
    }

    // 能不能续？远端比本地短，且前缀哈希对得上
    let mut offset = 0u64;
    if o.resume && !o.force && info_r.size > 0 && (info_r.size as u64) < total && info_r.hasher != "none" {
        let off = info_r.size as u64;
        let lp = hash::sha256_file(local, Some(off)).map_err(|e| e.to_string())?;
        match remote_sha256(d, &info_r.hasher, remote, Some(off)) {
            Some(rp) if rp == lp => {
                offset = off;
                info(&format!("续传：远端已有 {}，从这里接着传", human_bytes(off)));
            }
            _ => warn("远端已有部分和本地对不上，全量重传"),
        }
    }

    // 空间够吗（额外留 1 MB 余量，别把板子的 rootfs 塞到一个字节不剩）
    let need = total - offset;
    if info_r.free_kb >= 0 && (info_r.free_kb as u64) * 1024 < need + 1024 * 1024 {
        return Err(format!(
            "板子上空间不够：还需 {}，{} 所在分区只剩 {}",
            human_bytes(need),
            remote,
            human_bytes((info_r.free_kb as u64) * 1024)
        ));
    }

    let sent = stream_to_remote(d, local, remote, offset, shared, &name)?;
    r.sent = sent;
    r.resumed_from = offset;
    r.secs = t0.elapsed().as_secs_f64();

    // 校验
    if o.verify {
        let lh = match local_hash {
            Some(h) => h,
            None => hash::sha256_file(local, None).map_err(|e| e.to_string())?,
        };
        match remote_sha256(d, &info_r.hasher, remote, None) {
            Some(rh) if rh == lh => r.verified = true,
            Some(rh) => {
                return Err(format!(
                    "校验不一致！本地 {}… 远端 {}…（文件可能损坏，重传一次: fy push --force）",
                    &lh[..12.min(lh.len())],
                    &rh[..12.min(rh.len())]
                ))
            }
            None => {
                // 板上没有任何哈希工具：退化成尺寸比对，并明说
                let after = probe_remote(d, remote).map_err(|e| e.to_string())?;
                if after.size as u64 != total {
                    return Err(format!(
                        "远端尺寸对不上（期望 {}，实际 {}）",
                        total,
                        after.size
                    ));
                }
                warn("板上没有 sha256 工具，只比对了尺寸（装个 busybox 就能全量校验）");
            }
        }
    }
    Ok(r)
}

/// 真正的推流：`ssh 板子 'mkdir -p 目录 && cat >> 文件'`，本地按块写入并画进度。
fn stream_to_remote(
    d: &Device,
    local: &Path,
    remote: &str,
    offset: u64,
    shared: &mut Option<Progress>,
    label: &str,
) -> Result<u64, String> {
    let meta = std::fs::metadata(local).map_err(|e| e.to_string())?;
    let total = meta.len();
    let rq = shell_quote(remote);
    // offset>0 追加；否则截断重写。`: >` 比 `truncate` 通用（busybox 也有）。
    let redir = if offset > 0 { ">>" } else { ">" };
    let remote_cmd = format!(
        "d=$(dirname {rq}); mkdir -p \"$d\" 2>/dev/null; cat {redir} {rq}",
        rq = rq,
        redir = redir
    );

    let mut a = vec!["ssh".to_string()];
    a.extend(sshx::base_opts(d));
    a.push(sshx::target(d));
    a.push(remote_cmd);
    announce(&a);

    let mut cmd = Command::new(&a[0]);
    cmd.args(&a[1..]);
    for (k, v) in sshx::askpass_env(d) {
        cmd.env(k, v);
    }
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("起 ssh 失败: {}（主机装了 ssh 吗）", e))?;

    let mut f = std::fs::File::open(local).map_err(|e| e.to_string())?;
    if offset > 0 {
        use std::io::Seek;
        f.seek(std::io::SeekFrom::Start(offset)).map_err(|e| e.to_string())?;
    }

    let own_prog = shared.is_none();
    if own_prog {
        *shared = Some(Progress::new(label, total));
        if let Some(p) = shared.as_mut() {
            p.set(offset);
        }
    }

    let mut sent = 0u64;
    let mut buf = vec![0u8; CHUNK];
    {
        let sin = child.stdin.as_mut().ok_or("拿不到 ssh 的 stdin")?;
        loop {
            let n = f.read(&mut buf).map_err(|e| format!("读本地文件失败: {}", e))?;
            if n == 0 {
                break;
            }
            if let Err(e) = sin.write_all(&buf[..n]) {
                // 对端提前挂了（磁盘满/权限/连接断）——把 stderr 捞出来说人话
                let _ = child.kill();
                let out = child.wait_with_output().ok();
                let msg = out
                    .map(|o| String::from_utf8_lossy(&o.stderr).trim().to_string())
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| e.to_string());
                if let Some(p) = shared.as_mut() {
                    p.finish();
                }
                return Err(format!("传输中断（已传 {}）: {}", human_bytes(sent), msg));
            }
            sent += n as u64;
            if let Some(p) = shared.as_mut() {
                p.add(n as u64);
            }
        }
    }
    // 关掉 stdin 让远端 cat 收工
    drop(child.stdin.take());
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    if own_prog {
        if let Some(p) = shared.as_mut() {
            p.finish();
        }
        *shared = None;
    }
    if !out.status.success() {
        let e = String::from_utf8_lossy(&out.stderr);
        return Err(format!("远端写入失败: {}", e.trim()));
    }
    Ok(sent)
}

fn push_adb(
    d: &Device,
    local: &Path,
    remote: &str,
    o: &XferOpts,
    total: u64,
    name: String,
) -> Result<FileResult, String> {
    let t0 = std::time::Instant::now();
    let mut r = FileResult { name, remote: remote.to_string(), total, ..Default::default() };
    let info_r = probe_remote(d, remote).map_err(|e| e.to_string())?;
    let local_hash = hash::sha256_file(local, None).map_err(|e| e.to_string())?;
    if !o.force && o.skip_same && info_r.size as u64 == total {
        if let Some(rh) = remote_sha256(d, &info_r.hasher, remote, None) {
            if rh == local_hash {
                r.skipped = true;
                r.verified = true;
                r.secs = t0.elapsed().as_secs_f64();
                return Ok(r);
            }
        }
    }
    let okk = adbx::push(d, local, remote).map_err(|e| e.to_string())?;
    if !okk {
        return Err("adb push 失败".into());
    }
    r.sent = total;
    r.secs = t0.elapsed().as_secs_f64();
    if o.verify {
        match remote_sha256(d, &info_r.hasher, remote, None) {
            Some(rh) if rh == local_hash => r.verified = true,
            Some(_) => return Err("校验不一致！adb push 后哈希对不上".into()),
            None => warn("板上没有 sha256 工具，跳过内容校验"),
        }
    }
    Ok(r)
}

// ---------------- 目录 ----------------

fn walk(dir: &Path, base: &Path, out: &mut Vec<(PathBuf, String, u64)>) -> std::io::Result<()> {
    for e in std::fs::read_dir(dir)? {
        let e = e?;
        let p = e.path();
        let ft = e.file_type()?;
        if ft.is_symlink() {
            continue; // 软链接不跟：避免环，也避免把主机路径带上板
        }
        if ft.is_dir() {
            walk(&p, base, out)?;
        } else if ft.is_file() {
            let rel = p.strip_prefix(base).unwrap_or(&p).to_string_lossy().replace('\\', "/");
            let sz = e.metadata().map(|m| m.len()).unwrap_or(0);
            out.push((p, rel, sz));
        }
    }
    Ok(())
}

fn push_dir(d: &Device, local: &Path, remote: &str, o: &XferOpts) -> Result<Vec<FileResult>, String> {
    let mut files = vec![];
    walk(local, local, &mut files).map_err(|e| format!("遍历本地目录失败: {}", e))?;
    if files.is_empty() {
        return Err(format!("{} 里没有文件", local.display()));
    }
    files.sort_by(|a, b| a.1.cmp(&b.1));
    let base = local.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
    // rsync 语义，看的是**本地**路径的尾巴：
    //   fy push rk ./app  /opt/   → /opt/app/...   （建同名子目录）
    //   fy push rk ./app/ /opt/   → /opt/...       （内容直接铺进去）
    let spill = local.to_string_lossy().ends_with('/');
    let root = if spill {
        remote.trim_end_matches('/').to_string()
    } else {
        format!("{}/{}", remote.trim_end_matches('/'), base)
    };
    let grand: u64 = files.iter().map(|f| f.2).sum();
    info(&format!(
        "{} 个文件 / {} → {}:{}",
        files.len(),
        human_bytes(grand),
        d.name,
        root
    ));
    let mut prog = Some(Progress::new(&format!("{} ({} 个文件)", base, files.len()), grand));
    let mut results = vec![];
    let mut done_bytes = 0u64;
    for (p, rel, sz) in &files {
        let dest = format!("{}/{}", root, rel);
        let mut one = match push_one(d, p, &dest, o, &mut prog) {
            Ok(r) => r,
            Err(e) => {
                if let Some(pr) = prog.as_mut() {
                    pr.finish();
                }
                return Err(format!("{}: {}", rel, e));
            }
        };
        one.name = rel.clone();
        // 跳过的文件也要推进总进度条，否则百分比会卡住
        done_bytes += *sz;
        if let Some(pr) = prog.as_mut() {
            pr.set(done_bytes);
        }
        results.push(one);
    }
    if let Some(pr) = prog.as_mut() {
        pr.finish();
    }
    Ok(results)
}

// ---------------- pull ----------------

pub fn pull(d: &Device, remote: &str, local: &Path, o: &XferOpts) -> Result<Vec<FileResult>, String> {
    if d.transport == Transport::Serial {
        return Err("串口通道传不了文件，先 `fy up <设备>` 爬升到 ssh".into());
    }
    if dry() {
        info(&format!("DRY: 拉 {}:{} → {}", d.name, remote, local.display()));
        return Ok(vec![FileResult { name: remote.to_string(), remote: remote.to_string(), ..Default::default() }]);
    }
    let info_r = probe_remote(d, remote).map_err(|e| format!("探测远端失败: {}", e))?;
    match info_r.kind {
        'n' => Err(format!("板子上没有 {}", remote)),
        'd' => pull_dir(d, remote, local, o),
        _ => {
            let dest = resolve_pull_dest(remote, local);
            let mut prog = None;
            Ok(vec![pull_one(d, remote, &dest, o, &info_r, &mut prog)?])
        }
    }
}

fn resolve_pull_dest(remote: &str, local: &Path) -> PathBuf {
    let base = remote.trim_end_matches('/').rsplit('/').next().unwrap_or("file");
    if local.is_dir() || local.to_string_lossy().ends_with('/') {
        local.join(base)
    } else {
        local.to_path_buf()
    }
}

fn pull_one(
    d: &Device,
    remote: &str,
    local: &Path,
    o: &XferOpts,
    info_r: &RemoteInfo,
    shared: &mut Option<Progress>,
) -> Result<FileResult, String> {
    let total = info_r.size.max(0) as u64;
    let name = remote.rsplit('/').next().unwrap_or(remote).to_string();
    let mut r = FileResult { name: name.clone(), remote: remote.to_string(), total, ..Default::default() };
    let t0 = std::time::Instant::now();

    if let Some(parent) = local.parent() {
        if !parent.as_os_str().is_empty() {
            let _ = std::fs::create_dir_all(parent);
        }
    }

    if d.transport == Transport::Adb {
        let okk = adbx::pull(d, remote, local).map_err(|e| e.to_string())?;
        if !okk {
            return Err("adb pull 失败".into());
        }
        r.sent = total;
        r.secs = t0.elapsed().as_secs_f64();
        if o.verify {
            let lh = hash::sha256_file(local, None).map_err(|e| e.to_string())?;
            match remote_sha256(d, &info_r.hasher, remote, None) {
                Some(rh) if rh == lh => r.verified = true,
                Some(_) => return Err("校验不一致！adb pull 下来的内容和板上对不上".into()),
                None => warn("板上没有 sha256 工具，跳过内容校验"),
            }
        }
        return Ok(r);
    }

    let have = std::fs::metadata(local).map(|m| m.len()).unwrap_or(0);
    let mut offset = 0u64;
    if !o.force && have > 0 {
        if have == total && o.skip_same {
            let lh = hash::sha256_file(local, None).map_err(|e| e.to_string())?;
            if let Some(rh) = remote_sha256(d, &info_r.hasher, remote, None) {
                if rh == lh {
                    r.skipped = true;
                    r.verified = true;
                    r.secs = t0.elapsed().as_secs_f64();
                    return Ok(r);
                }
            }
        }
        if o.resume && have < total && info_r.hasher != "none" {
            let lp = hash::sha256_file(local, Some(have)).map_err(|e| e.to_string())?;
            match remote_sha256(d, &info_r.hasher, remote, Some(have)) {
                Some(rp) if rp == lp => {
                    offset = have;
                    info(&format!("续传：本地已有 {}，从这里接着拉", human_bytes(have)));
                }
                _ => warn("本地已有部分和板上对不上，全量重拉"),
            }
        }
    }

    // `tail -c +N` 是 POSIX，busybox 也有；N 从 1 开始计数
    let rq = shell_quote(remote);
    let remote_cmd = if offset > 0 {
        format!("tail -c +{} {}", offset + 1, rq)
    } else {
        format!("cat {}", rq)
    };
    let mut a = vec!["ssh".to_string()];
    a.extend(sshx::base_opts(d));
    a.push(sshx::target(d));
    a.push(remote_cmd);
    announce(&a);

    let mut cmd = Command::new(&a[0]);
    cmd.args(&a[1..]);
    for (k, v) in sshx::askpass_env(d) {
        cmd.env(k, v);
    }
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("起 ssh 失败: {}", e))?;

    let mut f = if offset > 0 {
        std::fs::OpenOptions::new().append(true).open(local).map_err(|e| e.to_string())?
    } else {
        std::fs::File::create(local).map_err(|e| format!("建不了本地文件 {}: {}", local.display(), e))?
    };

    let own_prog = shared.is_none();
    if own_prog {
        *shared = Some(Progress::new(&name, total));
        if let Some(p) = shared.as_mut() {
            p.set(offset);
        }
    }

    let mut sent = 0u64;
    {
        let sout = child.stdout.as_mut().ok_or("拿不到 ssh 的 stdout")?;
        let mut buf = vec![0u8; CHUNK];
        loop {
            let n = sout.read(&mut buf).map_err(|e| format!("读远端流失败: {}", e))?;
            if n == 0 {
                break;
            }
            f.write_all(&buf[..n]).map_err(|e| format!("写本地文件失败: {}", e))?;
            sent += n as u64;
            if let Some(p) = shared.as_mut() {
                p.add(n as u64);
            }
        }
    }
    f.flush().map_err(|e| e.to_string())?;
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    if own_prog {
        if let Some(p) = shared.as_mut() {
            p.finish();
        }
        *shared = None;
    }
    if !out.status.success() {
        return Err(format!("远端读取失败: {}", String::from_utf8_lossy(&out.stderr).trim()));
    }
    r.sent = sent;
    r.resumed_from = offset;
    r.secs = t0.elapsed().as_secs_f64();

    if o.verify {
        let lh = hash::sha256_file(local, None).map_err(|e| e.to_string())?;
        match remote_sha256(d, &info_r.hasher, remote, None) {
            Some(rh) if rh == lh => r.verified = true,
            Some(rh) => {
                return Err(format!(
                    "校验不一致！板上 {}… 本地 {}…",
                    &rh[..12.min(rh.len())],
                    &lh[..12.min(lh.len())]
                ))
            }
            None => {
                // 没有哈希工具就退化成尺寸比对——但要真的比，不能只是嘴上说说
                let got = std::fs::metadata(local).map(|m| m.len()).unwrap_or(0);
                if got != total {
                    return Err(format!("拉下来的尺寸对不上（板上 {}，本地 {}）", total, got));
                }
                warn("板上没有 sha256 工具，只比对了尺寸（装个 busybox 就能全量校验）");
            }
        }
    }
    Ok(r)
}

fn pull_dir(d: &Device, remote: &str, local: &Path, o: &XferOpts) -> Result<Vec<FileResult>, String> {
    let rq = shell_quote(remote);
    // find 打不出来就退回 ls -R 不值当；直接报错让用户点名文件更清楚
    let out = rexec(d, &format!("find {} -type f 2>/dev/null | head -20000", rq))
        .map_err(|e| e.to_string())?;
    let list: Vec<String> = out.stdout.lines().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
    if list.is_empty() {
        return Err(format!("{} 下没找到文件（板上有 find 吗？没有的话点名单个文件拉）", remote));
    }
    let root = remote.trim_end_matches('/');
    let dirname = root.rsplit('/').next().unwrap_or("pulled");
    let localroot = if local.to_string_lossy().ends_with('/') || local.is_dir() {
        local.join(dirname)
    } else {
        local.to_path_buf()
    };
    info(&format!("{} 个文件 → {}", list.len(), localroot.display()));

    // 一次问全所有尺寸，省掉 N 次往返
    let sizes = remote_sizes(d, &list);
    let grand: u64 = sizes.iter().map(|(_, s)| *s).sum();
    let mut prog = Some(Progress::new(&format!("{} ({} 个文件)", dirname, list.len()), grand));
    let hasher = probe_remote(d, remote).map(|i| i.hasher).unwrap_or_else(|_| "none".into());

    let mut results = vec![];
    let mut done = 0u64;
    for (rpath, sz) in &sizes {
        let rel = rpath.strip_prefix(root).unwrap_or(rpath).trim_start_matches('/');
        let dest = localroot.join(rel);
        let ri = RemoteInfo { kind: 'f', size: *sz as i64, hasher: hasher.clone(), free_kb: -1 };
        let mut one = match pull_one(d, rpath, &dest, o, &ri, &mut prog) {
            Ok(r) => r,
            Err(e) => {
                if let Some(p) = prog.as_mut() {
                    p.finish();
                }
                return Err(format!("{}: {}", rel, e));
            }
        };
        one.name = rel.to_string();
        done += *sz;
        if let Some(p) = prog.as_mut() {
            p.set(done);
        }
        results.push(one);
    }
    if let Some(p) = prog.as_mut() {
        p.finish();
    }
    Ok(results)
}

/// 批量问尺寸：`wc -c` 一次吞一堆路径，比一个个 stat 快得多。
fn remote_sizes(d: &Device, paths: &[String]) -> Vec<(String, u64)> {
    let mut out = vec![];
    for chunk in paths.chunks(200) {
        let joined: Vec<String> = chunk.iter().map(|p| shell_quote(p)).collect();
        let cmd = format!("wc -c {} 2>/dev/null", joined.join(" "));
        let res = match rexec(d, &cmd) {
            Ok(o) => o,
            Err(_) => {
                out.extend(chunk.iter().map(|p| (p.clone(), 0u64)));
                continue;
            }
        };
        let mut seen = std::collections::HashMap::new();
        for line in res.stdout.lines() {
            let line = line.trim();
            if line.is_empty() || line.ends_with(" total") {
                continue;
            }
            if let Some((n, p)) = line.split_once(char::is_whitespace) {
                if let Ok(v) = n.trim().parse::<u64>() {
                    seen.insert(p.trim().to_string(), v);
                }
            }
        }
        for p in chunk {
            let v = seen.get(p).copied().unwrap_or(0);
            out.push((p.clone(), v));
        }
    }
    out
}

// ---------------- 板↔板直传 ----------------

/// A 板 → B 板，**流不落主机磁盘**：`ssh A cat f | ssh B 'cat > f'`。
/// 主机只当中继，两边都不需要能互相看见。
pub fn device_to_device(src: &Device, src_path: &str, dst: &Device, dst_path: &str) -> Result<u64, String> {
    if src.transport == Transport::Serial || dst.transport == Transport::Serial {
        return Err("串口通道不参与板↔板直传".into());
    }
    let reader: Vec<String> = match src.transport {
        Transport::Ssh => {
            let mut a = vec!["ssh".to_string()];
            a.extend(sshx::base_opts(src));
            a.push(sshx::target(src));
            a.push(format!("cat {}", shell_quote(src_path)));
            a
        }
        Transport::Adb => adbx::adb_argv(src, &["exec-out", &format!("cat {}", shell_quote(src_path))]),
        Transport::Serial => unreachable!(),
    };
    let writer: Vec<String> = match dst.transport {
        Transport::Ssh => {
            let mut a = vec!["ssh".to_string()];
            a.extend(sshx::base_opts(dst));
            a.push(sshx::target(dst));
            a.push(format!(
                "d=$(dirname {p}); mkdir -p \"$d\" 2>/dev/null; cat > {p}",
                p = shell_quote(dst_path)
            ));
            a
        }
        Transport::Adb => adbx::adb_argv(dst, &["shell", &format!("cat > {}", shell_quote(dst_path))]),
        Transport::Serial => unreachable!(),
    };

    announce(&[format!("{} | {}", render_cmd(&reader), render_cmd(&writer))]);
    if dry() {
        return Ok(0);
    }

    let mut rc = Command::new(&reader[0]);
    rc.args(&reader[1..]);
    for (k, v) in sshx::askpass_env(src) {
        rc.env(k, v);
    }
    let mut rchild = rc
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("起源端失败: {}", e))?;

    let mut wc = Command::new(&writer[0]);
    wc.args(&writer[1..]);
    for (k, v) in sshx::askpass_env(dst) {
        wc.env(k, v);
    }
    let mut wchild = wc
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("起目标端失败: {}", e))?;

    let mut moved = 0u64;
    let mut prog = Progress::new(&format!("{} → {}", src.name, dst.name), 0);
    {
        let sout = rchild.stdout.as_mut().ok_or("源端没有 stdout")?;
        let win = wchild.stdin.as_mut().ok_or("目标端没有 stdin")?;
        let mut buf = vec![0u8; CHUNK];
        loop {
            let n = sout.read(&mut buf).map_err(|e| e.to_string())?;
            if n == 0 {
                break;
            }
            win.write_all(&buf[..n]).map_err(|e| format!("写目标端失败: {}", e))?;
            moved += n as u64;
            prog.add(n as u64);
        }
    }
    drop(wchild.stdin.take());
    prog.finish();
    let ro = rchild.wait_with_output().map_err(|e| e.to_string())?;
    let wo = wchild.wait_with_output().map_err(|e| e.to_string())?;
    if !ro.status.success() {
        return Err(format!("读源端失败: {}", String::from_utf8_lossy(&ro.stderr).trim()));
    }
    if !wo.status.success() {
        return Err(format!("写目标端失败: {}", String::from_utf8_lossy(&wo.stderr).trim()));
    }
    Ok(moved)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_hash_out_of_noisy_output() {
        let h = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        assert_eq!(extract_sha256(&format!("{}  /tmp/x\n", h)).unwrap(), h);
        assert_eq!(extract_sha256(&format!("SHA256(/tmp/x)= {}", h)).unwrap(), h);
        assert_eq!(extract_sha256("no hash here"), None);
        // 短的十六进制串不能误判成哈希
        assert_eq!(extract_sha256("deadbeef /tmp/x"), None);
    }

    #[test]
    fn hash_cmds_cover_every_flavour() {
        for h in ["sha256sum", "shasum", "busybox", "openssl"] {
            assert!(remote_hash_cmd(h, "'/tmp/a b'", None).is_some(), "{}", h);
            let pre = remote_hash_cmd(h, "'/tmp/a b'", Some(1024)).unwrap();
            assert!(pre.contains("head -c 1024"), "{} → {}", h, pre);
        }
        assert!(remote_hash_cmd("none", "/x", None).is_none());
    }

    #[test]
    fn walk_skips_symlinks_and_keeps_relpaths() {
        let root = std::env::temp_dir().join(format!("ferry_walk_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("a.txt"), b"a").unwrap();
        std::fs::write(root.join("sub/b.txt"), b"bb").unwrap();
        #[cfg(unix)]
        let _ = std::os::unix::fs::symlink(root.join("a.txt"), root.join("link.txt"));
        let mut out = vec![];
        walk(&root, &root, &mut out).unwrap();
        let mut rels: Vec<String> = out.iter().map(|(_, r, _)| r.clone()).collect();
        rels.sort();
        assert_eq!(rels, vec!["a.txt".to_string(), "sub/b.txt".to_string()]);
        assert_eq!(out.iter().find(|(_, r, _)| r == "sub/b.txt").unwrap().2, 2);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn dir_push_root_follows_rsync_convention() {
        // 这里只验证路径拼接规则本身（真正的传输由集成测试跑）
        let join = |local: &str, remote: &str| -> String {
            let p = Path::new(local);
            let base = p.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
            if local.ends_with('/') {
                remote.trim_end_matches('/').to_string()
            } else {
                format!("{}/{}", remote.trim_end_matches('/'), base)
            }
        };
        assert_eq!(join("/tmp/app", "/opt/"), "/opt/app");
        assert_eq!(join("/tmp/app", "/opt"), "/opt/app");
        assert_eq!(join("/tmp/app/", "/opt/"), "/opt");
    }

    #[test]
    fn pull_dest_follows_trailing_slash() {
        let d = resolve_pull_dest("/var/log/messages", Path::new("/tmp/out.log"));
        assert_eq!(d, PathBuf::from("/tmp/out.log"));
        let d2 = resolve_pull_dest("/var/log/messages", Path::new("/tmp/"));
        assert_eq!(d2, PathBuf::from("/tmp/messages"));
    }
}
