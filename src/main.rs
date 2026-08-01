//! ferry (fy) — 上位机↔下位机摆渡人。
//! ssh / adb / 串口 三种通道一套命令；设备档案 + 指纹认领 + 通道爬升 +
//! 端口转发管理 + 借网 + USB 一键配网 + 保存即上板 + 串口黑匣子。

mod adbx;
mod all;
mod blackbox;
mod config;
mod doctor;
mod fingerprint;
mod fwd;
mod hash;
mod httpd;
mod hwprobe;
mod jsonout;
mod logs;
mod mdns;
mod netdiag;
mod peripheral_brief;
mod proxyd;
mod pty;
#[allow(dead_code)]
mod plugins;
mod runx;
mod scan;
mod serialx;
mod serve;
mod share;
mod sshx;
mod sync;
mod tomlite;
mod ui;
mod up;
mod usbnet;
mod util;
mod watchd;
mod wsutil;
mod xfer;

use config::{Config, Device, Transport};
use jsonout::{code, fail, fail_hint, J};
use std::net::TcpStream;
use std::path::PathBuf;
use std::time::Duration;
use util::*;

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();

    // ssh 把我们当 askpass 回调时：argv[0] 是提示词，且带环境变量标记
    if std::env::var("FERRY_ASKPASS_DEV").is_ok() && !args.iter().any(|a| a == "__askpass") {
        // ssh 调用形如: fy "user@host's password:"
        if args.len() == 1 && !args[0].starts_with("__") && !known_command(&args[0]) {
            std::process::exit(sshx::askpass_main(&args[0]));
        }
    }

    // 全局旗标。FERRY_JSON=1 等价于处处加 --json，方便 agent 一次设好。
    let mut want_json = std::env::var("FERRY_JSON")
        .map(|v| v != "0" && !v.is_empty())
        .unwrap_or(false);
    let mut rest: Vec<String> = vec![];
    for a in args.drain(..) {
        match a.as_str() {
            "-n" | "--dry-run" => DRY.store(true, std::sync::atomic::Ordering::Relaxed),
            "--plain" | "--no-color" => PLAIN.store(true, std::sync::atomic::Ordering::Relaxed),
            "-q" | "--quiet" => QUIET.store(true, std::sync::atomic::Ordering::Relaxed),
            "--json" => want_json = true,
            "-y" | "--yes" | "--non-interactive" => jsonout::set_noninteractive(true),
            "-V" | "--version" => {
                println!("ferry {}", VERSION);
                return;
            }
            _ => rest.push(a),
        }
    }
    if want_json {
        // JSON 模式：stdout 归 JSON 独占，过程信息压到 stderr，且全程不交互
        jsonout::set_json(true);
        PLAIN.store(true, std::sync::atomic::Ordering::Relaxed);
        QUIET.store(true, std::sync::atomic::Ordering::Relaxed);
    }
    let code = dispatch(rest);
    std::process::exit(code);
}

fn known_command(s: &str) -> bool {
    matches!(
        s,
        "ls" | "add"
            | "rm"
            | "sh"
            | "push"
            | "pull"
            | "cp"
            | "run"
            | "debug"
            | "fwd"
            | "share"
            | "scan"
            | "usb"
            | "up"
            | "sync"
            | "log"
            | "top"
            | "info"
            | "blame"
            | "bb"
            | "all"
            | "keyup"
            | "forget"
            | "wifi"
            | "doctor"
            | "fix"
            | "ui"
            | "serve"
            | "net"
            | "watch"
            | "proxy"
            | "hw"
            | "plugin"
            | "help"
            | "-h"
            | "--help"
    )
}

fn dispatch(args: Vec<String>) -> i32 {
    let cmd = args.first().cloned().unwrap_or_default();
    let tail: Vec<String> = args.iter().skip(1).cloned().collect();
    jsonout::set_cmd(if cmd.is_empty() { "ls" } else { &cmd });
    if jsonout::json_mode() && !json_capable(&cmd) {
        return fail_hint(
            code::USAGE,
            &format!("`fy {}` 还没有 --json 输出（它的输出是交互式/流式的）", cmd),
            Some("机器可读的命令清单看 `fy help --json` 里的 commands[].json 字段"),
        );
    }
    match cmd.as_str() {
        "" | "ls" => cmd_ls(),
        "-h" | "--help" | "help" => cmd_help(tail),
        "add" => cmd_add(tail),
        "rm" => cmd_rm(tail),
        "sh" => cmd_sh(tail),
        "push" => cmd_push(tail),
        "pull" => cmd_pull(tail),
        "cp" => cmd_cp(tail),
        "run" => cmd_run(tail, false),
        "debug" => cmd_run(tail, true),
        "fwd" => cmd_fwd(tail),
        "share" => cmd_share(tail),
        "scan" => cmd_scan(tail),
        "usb" => cmd_usb(tail),
        "up" => cmd_up(tail),
        "sync" => cmd_sync(tail),
        "log" => cmd_log(tail),
        "top" => {
            logs::top(&Config::load());
            0
        }
        "info" => cmd_info(tail),
        "blame" => cmd_blame(tail),
        "bb" => cmd_bb(tail),
        "all" => cmd_all(tail),
        "keyup" => cmd_keyup(tail),
        "forget" => cmd_forget(tail),
        "wifi" => cmd_wifi(tail),
        "doctor" => cmd_doctor(tail),
        "fix" => cmd_fix(tail),
        "ui" => cmd_ui(tail),
        "serve" => cmd_serve(tail),
        "net" => cmd_net(tail),
        "watch" => cmd_watch(tail),
        "proxy" => cmd_proxy(tail),
        "hw" => cmd_hw(tail),
        "plugin" => cmd_plugin(tail),
        // 内部入口
        "__askpass" => sshx::askpass_main(tail.first().map(|s| s.as_str()).unwrap_or("")),
        "__proxyd" => {
            let port = flag_val(&tail, "--port")
                .and_then(|p| p.parse().ok())
                .unwrap_or(proxyd::DEFAULT_PORT);
            let up = flag_val(&tail, "--upstream")
                .map(|u| proxyd::Upstream::parse(&u).unwrap_or(proxyd::Upstream::Direct))
                .unwrap_or(proxyd::Upstream::Direct);
            match proxyd::main_loop(port, up) {
                Ok(_) => 0,
                Err(e) => {
                    eprintln!("proxyd: {}", e);
                    1
                }
            }
        }
        "__bbd" => {
            if tail.len() < 3 {
                eprintln!("usage: fy __bbd <name> <port> <baud>");
                return 2;
            }
            let baud = tail[2].parse().unwrap_or(115200);
            blackbox::daemon_main(&tail[0], &tail[1], baud)
        }
        "__watchd" => watchd::daemon_main(),
        other => fail_hint(
            code::USAGE,
            &format!("不认识的命令 '{}'", other),
            Some("fy --help 看全览；fy help --json 拿机器可读的命令清单"),
        ),
    }
}

/// 哪些子命令保证"stdout 只有一份 JSON"。不在名单里的在 --json 下直接拒绝，
/// 免得人类表格混进 stdout 把 agent 的解析搞崩。
fn json_capable(cmd: &str) -> bool {
    matches!(
        cmd,
        "" | "ls"
            | "add"
            | "rm"
            | "sh"
            | "push"
            | "pull"
            | "cp"
            | "fwd"
            | "share"
            | "proxy"
            | "watch"
            | "net"
            | "scan"
            | "info"
            | "all"
            | "bb"
            | "blame"
            | "keyup"
            | "forget"
            | "wifi"
            | "doctor"
            | "fix"
            | "help"
            | "-h"
            | "--help"
            | "serve"
            | "hw"
            | "plugin"
    )
}

fn flag_val(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1).cloned())
}
fn has_flag(args: &[String], name: &str) -> bool {
    args.iter().any(|a| a == name)
}
/// 去掉旗标后的裸参数
fn positional(args: &[String], flags_with_val: &[&str]) -> Vec<String> {
    let mut out = vec![];
    let mut skip = false;
    for a in args.iter() {
        if skip {
            skip = false;
            continue;
        }
        if a.starts_with("--") {
            if flags_with_val.contains(&a.as_str()) {
                skip = true;
            }
            continue;
        }
        out.push(a.clone());
    }
    out
}

// ---------------- 设备解析（给 --json 用的稳定错误码） ----------------

/// 统一的"选一台设备"入口。找不到 / 有歧义 / 非交互没法问，各有各的退出码，
/// agent 拿到 `code` 就知道该怎么补参数。
fn need_dev(cfg: &Config, name: Option<&str>) -> Result<Device, i32> {
    if let Some(n) = name {
        return match cfg.resolve(n) {
            config::Pick::Found(d) => Ok(d),
            config::Pick::Missing => Err(fail_hint(
                code::NO_DEVICE,
                &format!("没有名为 '{}' 的设备档案", n),
                Some("`fy ls` 看已有设备，`fy add <名字> --ssh root@IP` 添加，`fy scan` 自动发现"),
            )),
            config::Pick::Ambiguous(hits) => Err(fail_hint(
                code::AMBIGUOUS,
                &format!("'{}' 匹配到多台设备: {}", n, hits.join(", ")),
                Some("把名字写全一点"),
            )),
        };
    }
    if cfg.devices.is_empty() {
        return Err(fail_hint(
            code::NO_DEVICE,
            "还没有任何设备档案",
            Some("先 `fy add <名字> --ssh root@IP` 或 `fy scan --add`"),
        ));
    }
    if cfg.devices.len() == 1 {
        return Ok(cfg.devices.values().next().cloned().unwrap());
    }
    let items: Vec<String> = cfg
        .devices
        .values()
        .map(|d| {
            format!(
                "{}  {}",
                d.name,
                dim(&format!("[{}] {}", d.transport.as_str(), d.endpoint()))
            )
        })
        .collect();
    match pick("选择设备:", &items) {
        Some(i) => Ok(cfg.devices.values().nth(i).cloned().unwrap()),
        None => {
            let names: Vec<String> = cfg.devices.keys().cloned().collect();
            Err(fail_hint(
                code::NEED_INPUT,
                &format!("有 {} 台设备，得指名道姓", names.len()),
                Some(&format!("可选: {}", names.join(", "))),
            ))
        }
    }
}

fn transport_json(d: &Device) -> J {
    J::obj(vec![
        ("name", J::s(&d.name)),
        ("transport", J::s(d.transport.as_str())),
        ("endpoint", J::s(d.endpoint())),
    ])
}

// ---------------- ls (仪表盘) ----------------

fn probe_status(d: &Device) -> (bool, String) {
    match d.transport {
        Transport::Ssh => {
            let okk = format!("{}:{}", d.host, d.port)
                .parse()
                .ok()
                .map(|a| TcpStream::connect_timeout(&a, Duration::from_millis(500)).is_ok())
                .unwrap_or(false);
            (
                okk,
                if okk {
                    "在线".into()
                } else {
                    "不可达".into()
                },
            )
        }
        Transport::Adb => adbx::probe(d),
        Transport::Serial => {
            let p = d.dev.clone().unwrap_or_default();
            let exists = PathBuf::from(&p).exists();
            let bb = blackbox::running_for(&d.name);
            (
                exists,
                match (exists, bb) {
                    (true, true) => "在线+黑匣子".into(),
                    (true, false) => "在线".into(),
                    (false, _) => "没插".into(),
                },
            )
        }
    }
}

fn probe_all(devs: &[Device]) -> Vec<(bool, String)> {
    let handles: Vec<_> = devs
        .iter()
        .map(|d| {
            let d = d.clone();
            std::thread::spawn(move || probe_status(&d))
        })
        .collect();
    handles
        .into_iter()
        .map(|h| h.join().unwrap_or((false, "?".into())))
        .collect()
}

fn cmd_ls() -> i32 {
    let cfg = Config::load();
    let devs: Vec<Device> = cfg.devices.values().cloned().collect();
    let states = probe_all(&devs);

    if jsonout::json_mode() {
        let items: Vec<J> = devs
            .iter()
            .zip(states.iter())
            .map(|(d, (on, why))| {
                let f = config::facts_load(&d.name);
                J::obj(vec![
                    ("name", J::s(&d.name)),
                    ("transport", J::s(d.transport.as_str())),
                    ("endpoint", J::s(d.endpoint())),
                    ("host", J::s(&d.host)),
                    ("port", J::i(d.port as i64)),
                    ("online", J::b(*on)),
                    ("status", J::s(why)),
                    ("hostname", J::s(&f.hostname)),
                    ("os", J::s(&f.os)),
                    ("kernel", J::s(&f.kernel)),
                    ("arch", J::s(&f.arch)),
                    ("macs", J::strs(&f.macs)),
                    ("last_ip", J::s(&f.last_ip)),
                    ("last_seen", J::i(f.last_seen)),
                ])
            })
            .collect();
        return jsonout::emit_ok(vec![
            ("count", J::i(items.len() as i64)),
            ("devices", J::arr(items)),
        ]);
    }

    if cfg.devices.is_empty() {
        println!("{}", bold("ferry — 上位机↔下位机摆渡人"));
        println!();
        println!("还没有设备档案。三种起步方式:");
        println!(
            "  {}   交互建档（ssh/adb/串口任一）",
            cyan("fy add <名字> --ssh root@192.168.1.x")
        );
        println!(
            "  {}                       扫描周围的板子并建档",
            cyan("fy scan --add")
        );
        println!(
            "  {}                    插 USB 线一键配网",
            cyan("fy usb net")
        );
        return 0;
    }
    let mut rows = vec![];
    for (d, (on, why)) in devs.iter().zip(states.iter()) {
        let f = config::facts_load(&d.name);
        let id_hint = if !f.hostname.is_empty() {
            f.hostname.clone()
        } else {
            f.os.clone()
        };
        rows.push(vec![
            bold(&d.name),
            d.transport.as_str().to_string(),
            d.endpoint(),
            if *on {
                green(&format!("● {}", why))
            } else {
                red(&format!("○ {}", why))
            },
            dim(&id_hint),
            dim(&human_ago(f.last_seen)),
        ]);
    }
    print_table(&["设备", "通道", "地址", "状态", "身份", "上次见到"], &rows);
    println!();
    println!(
        "{}",
        dim("fy sh <设备> 进 shell · fy up <设备> 通道爬升 · fy net <设备> 网络体检 · fy --help 全览")
    );
    0
}

// ---------------- push / pull / cp ----------------

fn xfer_opts(args: &[String]) -> xfer::XferOpts {
    xfer::XferOpts {
        resume: !has_flag(args, "--no-resume"),
        verify: !has_flag(args, "--no-verify"),
        force: has_flag(args, "--force"),
        skip_same: !has_flag(args, "--force"),
    }
}

fn files_json(rs: &[xfer::FileResult]) -> J {
    J::arr(
        rs.iter()
            .map(|r| {
                J::obj(vec![
                    ("name", J::s(&r.name)),
                    ("remote", J::s(&r.remote)),
                    ("size", J::i(r.total as i64)),
                    ("transferred", J::i(r.sent as i64)),
                    ("resumed_from", J::i(r.resumed_from as i64)),
                    ("skipped", J::b(r.skipped)),
                    ("verified", J::b(r.verified)),
                    ("seconds", J::f(r.secs)),
                    ("bytes_per_sec", J::f(r.rate())),
                ])
            })
            .collect(),
    )
}

fn report_xfer(dev: &str, rs: &[xfer::FileResult], verb: &str) -> i32 {
    let sent: u64 = rs.iter().map(|r| r.sent).sum();
    let total: u64 = rs.iter().map(|r| r.total).sum();
    let secs: f64 = rs.iter().map(|r| r.secs).sum();
    let skipped = rs.iter().filter(|r| r.skipped).count();
    let verified = rs.iter().filter(|r| r.verified).count();
    if jsonout::json_mode() {
        return jsonout::emit_ok(vec![
            ("device", J::s(dev)),
            ("files", files_json(rs)),
            ("file_count", J::i(rs.len() as i64)),
            ("skipped", J::i(skipped as i64)),
            ("verified", J::i(verified as i64)),
            ("bytes_transferred", J::i(sent as i64)),
            ("bytes_total", J::i(total as i64)),
            ("seconds", J::f(secs)),
        ]);
    }
    if dry() {
        return 0;
    }
    let rate = if secs > 0.0 { sent as f64 / secs } else { 0.0 };
    let mut msg = format!("{} {} 个文件 · {}", verb, rs.len(), human_bytes(total));
    if sent < total {
        msg.push_str(&format!("（实传 {}）", human_bytes(sent)));
    }
    if skipped > 0 {
        msg.push_str(&format!(" · {} 个已是最新跳过", skipped));
    }
    if sent > 0 {
        msg.push_str(&format!(" · {} · {}", human_dur(secs), human_rate(rate)));
    }
    if verified == rs.len() && !rs.is_empty() {
        msg.push_str(&format!(" · {}", green("已校验")));
    }
    ok(&msg);
    0
}

fn cmd_push(args: Vec<String>) -> i32 {
    let pos = positional(&args, &["--only"]);
    let o = xfer_opts(&args);
    let cfg = Config::load();

    if has_flag(&args, "--all") {
        if pos.is_empty() {
            return fail(
                code::USAGE,
                "用法: fy push --all <本地路径> [远端路径] [--only 前缀]",
            );
        }
        let local = PathBuf::from(&pos[0]);
        let remote = pos.get(1).cloned();
        return push_all(&cfg, &local, remote, &o, flag_val(&args, "--only"));
    }
    if pos.len() < 2 {
        return fail_hint(
            code::USAGE,
            "用法: fy push <设备> <本地路径> [远端路径]",
            Some("批量分发用 --all；断点续传/校验默认开，可用 --no-resume / --no-verify / --force 调整"),
        );
    }
    let d = match need_dev(&cfg, Some(&pos[0])) {
        Ok(d) => d,
        Err(c) => return c,
    };
    let local = PathBuf::from(&pos[1]);
    let remote = pos.get(2).cloned().unwrap_or_else(|| d.dest.clone());
    // --scp：走老路子（scp → scp -O → tar 管道）。新引擎在某块板子上水土不服时
    // 的逃生口；代价是没有续传也没有校验。
    if has_flag(&args, "--scp") {
        return legacy_xfer(&d, || sshx::push(&d, &local, &remote), "已推送(scp)");
    }
    match xfer::push(&d, &local, &remote, &o) {
        Ok(rs) => report_xfer(&d.name, &rs, "已推送"),
        Err(e) => fail(transfer_code(&e), &e),
    }
}

/// 老 scp/tar 级联路径的统一收尾。
fn legacy_xfer<F: FnOnce() -> std::io::Result<bool>>(d: &Device, run: F, verb: &str) -> i32 {
    if d.transport != Transport::Ssh {
        return fail(code::UNSUPPORTED, "--scp 只对 ssh 通道有意义");
    }
    warn("--scp 走老路子：没有断点续传，也不做 sha256 校验");
    match run() {
        Ok(true) => {
            ok(verb);
            jsonout::emit_ok(vec![
                ("device", J::s(&d.name)),
                ("mode", J::s("scp")),
                ("verified", J::b(false)),
            ])
        }
        _ => fail(code::TRANSFER, "scp 级联三种方式都失败了"),
    }
}

/// 校验类失败单独给一个码：agent 看到 15 就知道该重传而不是改参数。
fn transfer_code(e: &str) -> i32 {
    if e.contains("校验不一致") {
        code::CHECKSUM
    } else if e.contains("串口") {
        code::UNSUPPORTED
    } else {
        code::TRANSFER
    }
}

fn push_all(
    cfg: &Config,
    local: &std::path::Path,
    remote: Option<String>,
    o: &xfer::XferOpts,
    only: Option<String>,
) -> i32 {
    let devs: Vec<Device> = cfg
        .devices
        .values()
        .filter(|d| d.transport != Transport::Serial)
        .filter(|d| {
            only.as_ref()
                .map(|p| d.name.starts_with(p.as_str()))
                .unwrap_or(true)
        })
        .cloned()
        .collect();
    if devs.is_empty() {
        return fail(code::NO_DEVICE, "没有匹配的 ssh/adb 设备");
    }
    let states = probe_all(&devs);
    let live: Vec<Device> = devs
        .iter()
        .zip(states.iter())
        .filter(|(_, (on, _))| *on)
        .map(|(d, _)| d.clone())
        .collect();
    let offline: Vec<(String, String)> = devs
        .iter()
        .zip(states.iter())
        .filter(|(_, (on, _))| !*on)
        .map(|(d, (_, why))| (d.name.clone(), why.clone()))
        .collect();
    if live.is_empty() {
        let detail: Vec<String> = offline
            .iter()
            .map(|(n, w)| format!("{}({})", n, w))
            .collect();
        return fail_hint(
            code::UNREACHABLE,
            &format!(
                "匹配到 {} 台设备，但一台在线的都没有: {}",
                devs.len(),
                detail.join(" ")
            ),
            Some("先 `fy ls` 看状态，或 `fy scan` 认领换了 IP 的板子"),
        );
    }
    if !offline.is_empty() {
        warn(&format!(
            "跳过 {} 台不在线的: {}",
            offline.len(),
            offline
                .iter()
                .map(|(n, _)| n.as_str())
                .collect::<Vec<_>>()
                .join(" ")
        ));
    }
    info(&format!("并行分发到 {} 台在线设备 ...", live.len()));
    // 多个进度条抢一行没法看，批量模式统一关掉，改用结果表
    NOPROG.store(true, std::sync::atomic::Ordering::Relaxed);

    let handles: Vec<_> = live
        .into_iter()
        .map(|d| {
            let local = local.to_path_buf();
            let remote = remote.clone();
            let o = o.clone();
            std::thread::spawn(move || {
                let dest = remote.unwrap_or_else(|| d.dest.clone());
                (d.name.clone(), xfer::push(&d, &local, &dest, &o))
            })
        })
        .collect();

    let mut rows = vec![];
    let mut items = vec![];
    let mut worst = 0;
    for h in handles {
        let (name, res) = match h.join() {
            Ok(v) => v,
            Err(_) => continue,
        };
        match res {
            Ok(rs) => {
                let sent: u64 = rs.iter().map(|r| r.sent).sum();
                let secs: f64 = rs.iter().map(|r| r.secs).sum();
                let verified = rs.iter().all(|r| r.verified);
                rows.push(vec![
                    bold(&name),
                    green("✓"),
                    format!("{} 个文件", rs.len()),
                    human_bytes(sent),
                    human_dur(secs),
                    if verified {
                        green("已校验")
                    } else {
                        dim("未校验")
                    },
                ]);
                items.push(J::obj(vec![
                    ("device", J::s(&name)),
                    ("ok", J::b(true)),
                    ("files", files_json(&rs)),
                    ("bytes_transferred", J::i(sent as i64)),
                    ("seconds", J::f(secs)),
                ]));
            }
            Err(e) => {
                worst = 1;
                rows.push(vec![
                    bold(&name),
                    red("✗"),
                    e.clone(),
                    String::new(),
                    String::new(),
                    String::new(),
                ]);
                items.push(J::obj(vec![
                    ("device", J::s(&name)),
                    ("ok", J::b(false)),
                    ("error", J::s(&e)),
                ]));
            }
        }
    }
    if jsonout::json_mode() {
        return jsonout::emit_ok(vec![
            ("results", J::arr(items)),
            (
                "skipped_offline",
                J::arr(
                    offline
                        .iter()
                        .map(|(n, w)| J::obj(vec![("device", J::s(n)), ("reason", J::s(w))]))
                        .collect(),
                ),
            ),
            ("failed", J::i(worst as i64)),
        ]);
    }
    println!();
    print_table(&["设备", "结果", "文件", "实传", "耗时", "校验"], &rows);
    worst
}

fn cmd_pull(args: Vec<String>) -> i32 {
    let pos = positional(&args, &[]);
    if pos.len() < 2 {
        return fail(code::USAGE, "用法: fy pull <设备> <远端路径> [本地路径]");
    }
    let cfg = Config::load();
    let d = match need_dev(&cfg, Some(&pos[0])) {
        Ok(d) => d,
        Err(c) => return c,
    };
    let remote = pos[1].clone();
    let local = PathBuf::from(pos.get(2).cloned().unwrap_or_else(|| ".".into()));
    if has_flag(&args, "--scp") {
        return legacy_xfer(&d, || sshx::pull(&d, &remote, &local), "已拉取(scp)");
    }
    match xfer::pull(&d, &remote, &local, &xfer_opts(&args)) {
        Ok(rs) => report_xfer(&d.name, &rs, "已拉取"),
        Err(e) => fail(transfer_code(&e), &e),
    }
}

/// `dev:/path` → (设备, 路径)；纯本地路径 → None。
fn split_devref(cfg: &Config, s: &str) -> Option<(Device, String)> {
    let (head, rest) = s.split_once(':')?;
    if head.is_empty() || head.contains('/') || head.contains('.') {
        return None;
    }
    match cfg.resolve(head) {
        config::Pick::Found(d) => Some((d, rest.to_string())),
        _ => None,
    }
}

fn cmd_cp(args: Vec<String>) -> i32 {
    let pos = positional(&args, &[]);
    if pos.len() < 2 {
        return fail_hint(
            code::USAGE,
            "用法: fy cp <源> <目标>",
            Some("路径写成 设备名:/板上路径 或本地路径；板↔板直传不落主机磁盘，例: fy cp rk:/tmp/a.bin cam:/data/"),
        );
    }
    let cfg = Config::load();
    let o = xfer_opts(&args);
    let src = split_devref(&cfg, &pos[0]);
    let dst = split_devref(&cfg, &pos[1]);
    match (src, dst) {
        (None, None) => fail(code::USAGE, "两端都是本地路径，这活儿 cp 命令就能干"),
        (None, Some((d, rp))) => {
            let local = PathBuf::from(&pos[0]);
            match xfer::push(&d, &local, &rp, &o) {
                Ok(rs) => report_xfer(&d.name, &rs, "已推送"),
                Err(e) => fail(transfer_code(&e), &e),
            }
        }
        (Some((d, rp)), None) => {
            let local = PathBuf::from(&pos[1]);
            match xfer::pull(&d, &rp, &local, &o) {
                Ok(rs) => report_xfer(&d.name, &rs, "已拉取"),
                Err(e) => fail(transfer_code(&e), &e),
            }
        }
        (Some((sd, sp)), Some((dd, mut dp))) => {
            if sd.name == dd.name {
                return fail(code::USAGE, "源和目标是同一台设备，直接 fy sh 上去 cp 更快");
            }
            // 目标以 / 结尾 → 沿用源文件名
            if dp.ends_with('/') || dp.is_empty() {
                let base = sp
                    .trim_end_matches('/')
                    .rsplit('/')
                    .next()
                    .unwrap_or("file");
                dp = format!(
                    "{}{}",
                    if dp.is_empty() { "/tmp/" } else { dp.as_str() },
                    base
                );
            }
            info(&format!(
                "{}:{} → {}:{}（经主机中转，不落盘）",
                sd.name, sp, dd.name, dp
            ));
            match xfer::device_to_device(&sd, &sp, &dd, &dp) {
                Ok(n) => {
                    if jsonout::json_mode() {
                        return jsonout::emit_ok(vec![
                            (
                                "from",
                                J::obj(vec![("device", J::s(&sd.name)), ("path", J::s(&sp))]),
                            ),
                            (
                                "to",
                                J::obj(vec![("device", J::s(&dd.name)), ("path", J::s(&dp))]),
                            ),
                            ("bytes", J::i(n as i64)),
                        ]);
                    }
                    ok(&format!("已搬运 {}", human_bytes(n)));
                    0
                }
                Err(e) => fail(code::TRANSFER, &e),
            }
        }
    }
}

// ---------------- add / rm ----------------

fn cmd_add(args: Vec<String>) -> i32 {
    let pos = positional(
        &args,
        &[
            "--ssh",
            "--adb",
            "--serial",
            "--baud",
            "--password",
            "--key",
            "--dest",
            "--notes",
        ],
    );
    let name = match pos.first() {
        Some(n) => n.clone(),
        None => {
            return fail(
                code::USAGE,
                "用法: fy add <名字> [--ssh user@host[:port]] [--adb [serial]] [--serial /dev/xxx --baud 115200] [--password P] [--legacy]",
            )
        }
    };
    if !name
        .chars()
        .all(|c| c.is_alphanumeric() || "_-".contains(c))
    {
        return fail(code::USAGE, "名字只能用字母数字-_（会用作文件名）");
    }
    let mut cfg = Config::load();
    let mut d = cfg
        .devices
        .get(&name)
        .cloned()
        .unwrap_or_else(|| Device::new(&name, Transport::Ssh));

    if let Some(ssh) = flag_val(&args, "--ssh") {
        d.transport = Transport::Ssh;
        // user@host[:port]
        let (user, hostport) = match ssh.split_once('@') {
            Some((u, h)) => (u.to_string(), h.to_string()),
            None => (d.user.clone(), ssh),
        };
        let (host, port) = match hostport.rsplit_once(':') {
            Some((h, p)) if p.chars().all(|c| c.is_ascii_digit()) => {
                (h.to_string(), p.parse().unwrap_or(22))
            }
            _ => (hostport, 22),
        };
        d.user = user;
        d.host = host;
        d.port = port;
    }
    if has_flag(&args, "--adb") || flag_val(&args, "--adb").is_some() {
        d.transport = Transport::Adb;
        if let Some(s) = flag_val(&args, "--adb") {
            if !s.starts_with("--") {
                d.adb_serial = Some(s);
            }
        }
    }
    if let Some(sp) = flag_val(&args, "--serial") {
        if d.host.is_empty() && d.adb_serial.is_none() {
            d.transport = Transport::Serial;
        }
        d.dev = Some(sp);
    }
    if let Some(b) = flag_val(&args, "--baud").and_then(|b| b.parse().ok()) {
        d.baud = b;
    }
    if let Some(p) = flag_val(&args, "--password") {
        d.password = Some(p);
    }
    if let Some(k) = flag_val(&args, "--key") {
        d.key = Some(k);
    }
    if let Some(x) = flag_val(&args, "--dest") {
        d.dest = x;
    }
    if let Some(x) = flag_val(&args, "--notes") {
        d.notes = x;
    }
    if has_flag(&args, "--legacy") {
        d.legacy = true;
    }
    let summary = format!("[{}] {}", d.transport.as_str(), d.endpoint());
    let has_pw = d.password.is_some();
    cfg.devices.insert(name.clone(), d);
    if let Err(e) = cfg.save() {
        return fail(code::CONFIG, &format!("保存失败: {}", e));
    }
    ok(&format!("{} = {}", name, summary));
    if has_pw {
        info(&format!(
            "密码明文存在 devices.toml (0600)。跑一次 fy keyup {} 就能转免密。",
            name
        ));
    }
    let saved = Config::load();
    match saved.devices.get(&name) {
        Some(d) => jsonout::emit_ok(vec![("device", transport_json(d))]),
        None => jsonout::emit_ok(vec![("device", J::s(&name))]),
    }
}

fn cmd_rm(args: Vec<String>) -> i32 {
    let mut cfg = Config::load();
    let name = match args.first() {
        Some(n) => n.clone(),
        None => return fail(code::USAGE, "用法: fy rm <设备>"),
    };
    if cfg.devices.remove(&name).is_some() {
        if let Err(e) = cfg.save() {
            return fail(code::CONFIG, &format!("保存失败: {}", e));
        }
        let _ = std::fs::remove_file(config::facts_path(&name));
        ok(&format!("已删除 {}", name));
        jsonout::emit_ok(vec![("removed", J::s(&name))])
    } else {
        fail(code::NO_DEVICE, &format!("没有 '{}'", name))
    }
}

// ---------------- sh / push / pull ----------------

fn split_dashdash(args: &[String]) -> (Vec<String>, Option<Vec<String>>) {
    match args.iter().position(|a| a == "--") {
        Some(i) => (args[..i].to_vec(), Some(args[i + 1..].to_vec())),
        None => (args.to_vec(), None),
    }
}

fn cmd_sh(args: Vec<String>) -> i32 {
    let (head, cmdv) = split_dashdash(&args);
    let cfg = Config::load();
    let d = match need_dev(&cfg, head.first().map(|s| s.as_str())) {
        Ok(d) => d,
        Err(c) => return c,
    };
    let one_shot = cmdv.map(|v| v.join(" "));

    // --json 下不能把交互 shell 接到终端上，只能跑一次性命令并把三件套交出来
    if jsonout::json_mode() {
        let cmd = match one_shot {
            Some(c) if !c.trim().is_empty() => c,
            _ => {
                return fail_hint(
                    code::USAGE,
                    "--json 模式下 fy sh 必须带一条命令",
                    Some("写成 fy --json sh <设备> -- uname -a"),
                )
            }
        };
        let out = match d.transport {
            Transport::Ssh => sshx::exec_capture(&d, &cmd),
            Transport::Adb => adbx::exec_capture(&d, &cmd),
            Transport::Serial => {
                return fail(
                    code::UNSUPPORTED,
                    "串口设备不支持一次性命令，先 fy up 爬到 ssh",
                )
            }
        };
        return match out {
            Ok(o) => {
                jsonout::emit_ok(vec![
                    ("device", J::s(&d.name)),
                    ("command", J::s(&cmd)),
                    ("exit_code", J::i(o.status as i64)),
                    ("stdout", J::s(&o.stdout)),
                    ("stderr", J::s(&o.stderr)),
                ]);
                // 退出码透传：agent 既能读 JSON 里的 exit_code，也能直接看 $?
                o.status
            }
            Err(e) => fail(code::UNREACHABLE, &format!("{}", e)),
        };
    }

    let r = match (&d.transport, one_shot) {
        (Transport::Ssh, None) => sshx::shell(&d),
        (Transport::Ssh, Some(c)) => sshx::exec_inherit(&d, &c, false),
        (Transport::Adb, None) => adbx::shell(&d),
        (Transport::Adb, Some(c)) => adbx::exec_inherit(&d, &c, false),
        (Transport::Serial, None) => blackbox::serial_shell(&cfg, &d.name).map(|_| 0),
        (Transport::Serial, Some(_)) => {
            return fail(
                code::UNSUPPORTED,
                "串口设备不支持一次性命令，先 fy up 爬到 ssh",
            );
        }
    };
    match r {
        Ok(c) => c,
        Err(e) => fail(code::UNREACHABLE, &format!("{}", e)),
    }
}

// ---------------- run / debug ----------------

fn cmd_run(args: Vec<String>, is_debug: bool) -> i32 {
    let cfg = Config::load();
    if args.len() < 2 {
        err(if is_debug {
            "用法: fy debug <设备> <可执行文件> [参数...] [--port 3333]"
        } else {
            "用法: fy run <设备> <可执行文件> [参数...]"
        });
        return 2;
    }
    let d = match need_dev(&cfg, Some(&args[0])) {
        Ok(d) => d,
        Err(c) => return c,
    };
    let bin = PathBuf::from(&args[1]);
    let mut rest: Vec<String> = args[2..].to_vec();
    let mut port = 3333u16;
    if is_debug {
        if let Some(p) = flag_val(&rest, "--port").and_then(|p| p.parse().ok()) {
            port = p;
            rest.retain(|a| a != "--port" && a != &port.to_string());
        }
    }
    let r = if is_debug {
        runx::debug(&cfg, &d, &bin, &rest, port)
    } else {
        runx::run(&cfg, &d, &bin, &rest, false)
    };
    match r {
        Ok(c) => c,
        Err(e) => {
            err(&format!("{}", e));
            1
        }
    }
}

// ---------------- fwd / share ----------------

fn cmd_fwd(args: Vec<String>) -> i32 {
    let cfg = Config::load();
    let pos = positional(&args, &[]);
    match pos.first().map(|s| s.as_str()) {
        None | Some("ls") => {
            let views = fwd::collect(&cfg);
            if jsonout::json_mode() {
                let items: Vec<J> = views
                    .iter()
                    .map(|v| {
                        J::obj(vec![
                            ("id", J::s(&v.id)),
                            ("device", J::s(&v.dev)),
                            ("channel", J::s(&v.channel)),
                            ("spec", J::s(&v.spec)),
                            ("human", J::s(&v.human)),
                            ("alive", J::b(v.alive)),
                            ("added", J::i(v.added)),
                        ])
                    })
                    .collect();
                return jsonout::emit_ok(vec![
                    ("forwards", J::arr(items)),
                    ("watchdog", J::b(watchd::is_running())),
                ]);
            }
            fwd::list(&cfg);
            0
        }
        Some("rm") => match pos.get(1) {
            Some(id) => {
                fwd::remove(&cfg, id);
                jsonout::emit_ok(vec![("removed", J::s(id))])
            }
            None => fail(code::USAGE, "用法: fy fwd rm <ID|all>"),
        },
        Some(dev) => {
            let spec =
                match pos.get(1) {
                    Some(s) => s.clone(),
                    None => return fail_hint(
                        code::USAGE,
                        "缺少转发规则",
                        Some("规则形如: 8080 · 8080:80 · 8080:10.0.0.9:80 · R:9000:8000 · D:1080"),
                    ),
                };
            let d = match need_dev(&cfg, Some(dev)) {
                Ok(d) => d,
                Err(c) => return c,
            };
            match fwd::add_opts(&cfg, &d, &spec, !has_flag(&args, "--no-watch")) {
                Ok(id) => jsonout::emit_ok(vec![
                    ("id", J::s(&id)),
                    ("device", J::s(&d.name)),
                    ("spec", J::s(&spec)),
                    ("watchdog", J::b(watchd::is_running())),
                ]),
                Err(e) => fail(
                    if d.transport == Transport::Serial {
                        code::UNSUPPORTED
                    } else {
                        code::FAIL
                    },
                    &e,
                ),
            }
        }
    }
}

fn cmd_share(args: Vec<String>) -> i32 {
    let cfg = Config::load();
    let pos = positional(&args, &["--upstream"]);
    let d = match need_dev(&cfg, pos.first().map(|s| s.as_str())) {
        Ok(d) => d,
        Err(c) => return c,
    };
    let upstream = match flag_val(&args, "--upstream") {
        Some(u) => match proxyd::Upstream::parse(&u) {
            Ok(u) => Some(u),
            Err(e) => return fail(code::USAGE, &e),
        },
        None => None,
    };
    let off = has_flag(&args, "--off");
    let r = if off {
        share::disable(&cfg, &d)
    } else {
        share::enable(
            &cfg,
            &d,
            has_flag(&args, "--nat"),
            has_flag(&args, "--persist"),
            has_flag(&args, "--android-global"),
            upstream,
        )
    };
    match r {
        Ok(_) => {
            let port = proxyd::current_port();
            if off {
                jsonout::emit_ok(vec![("device", J::s(&d.name)), ("sharing", J::b(false))])
            } else {
                jsonout::emit_ok(vec![
                    ("device", J::s(&d.name)),
                    ("sharing", J::b(true)),
                    ("mode", J::s(if has_flag(&args, "--nat") { "nat" } else { "proxy" })),
                    ("proxy_port", J::i(port as i64)),
                    ("upstream", J::s(proxyd::current_upstream().as_arg())),
                    (
                        "board_env",
                        J::s(format!(
                            "export http_proxy=http://127.0.0.1:{p} https_proxy=http://127.0.0.1:{p} all_proxy=socks5://127.0.0.1:{p}",
                            p = port
                        )),
                    ),
                ])
            }
        }
        Err(e) => fail(code::FAIL, &e),
    }
}

// ---------------- scan / usb / up ----------------

fn cmd_scan(args: Vec<String>) -> i32 {
    let mut cfg = Config::load();
    let subnet = flag_val(&args, "--subnet");
    let use_mdns = !has_flag(&args, "--no-mdns");
    let extra_ports = match flag_val(&args, "--ports") {
        Some(value) => match scan::parse_extra_ports(&value) {
            Ok(ports) => ports,
            Err(error) => return fail(code::USAGE, &error),
        },
        None => vec![],
    };
    if jsonout::json_mode() {
        let fields = if extra_ports.is_empty() {
            scan::scan_json(&cfg, subnet.as_deref(), use_mdns)
        } else {
            scan::scan_json_with_ports(&cfg, subnet.as_deref(), use_mdns, &extra_ports)
        };
        return jsonout::emit_ok(fields);
    }
    if extra_ports.is_empty() {
        scan::scan_cmd(
            &mut cfg,
            subnet.as_deref(),
            has_flag(&args, "--add"),
            use_mdns,
        );
    } else {
        scan::scan_cmd_with_ports(
            &mut cfg,
            subnet.as_deref(),
            has_flag(&args, "--add"),
            use_mdns,
            &extra_ports,
        );
    }
    0
}

fn cmd_usb(args: Vec<String>) -> i32 {
    let sub = args.first().cloned().unwrap_or_else(|| "net".into());
    match sub.as_str() {
        "net" => {
            let mut cfg = Config::load();
            match usbnet::usb_net(
                &mut cfg,
                has_flag(&args, "--share"),
                flag_val(&args, "--as"),
            ) {
                Ok(_) => 0,
                Err(e) => {
                    err(&e);
                    1
                }
            }
        }
        "gadget" => {
            let mode = flag_val(&args, "--mode").unwrap_or_else(|| "ncm".into());
            match usbnet::gadget_emit(flag_val(&args, "--out").as_deref(), &mode) {
                Ok(_) => 0,
                Err(e) => {
                    err(&format!("{}", e));
                    1
                }
            }
        }
        "install" => {
            let cfg = Config::load();
            let d = match need_dev(&cfg, args.get(1).map(|s| s.as_str())) {
                Ok(d) => d,
                Err(c) => return c,
            };
            let mode = flag_val(&args, "--mode").unwrap_or_else(|| "ncm".into());
            match usbnet::gadget_install(&d, &mode, has_flag(&args, "--autostart")) {
                Ok(_) => 0,
                Err(e) => {
                    err(&e);
                    1
                }
            }
        }
        other => {
            err(&format!("fy usb 只有 net/gadget/install，没有 '{}'", other));
            2
        }
    }
}

fn cmd_up(args: Vec<String>) -> i32 {
    let mut cfg = Config::load();
    let pos = positional(&args, &[]);
    let name = match pos.first() {
        Some(n) => n.clone(),
        None => match need_dev(&cfg, None) {
            Ok(d) => d.name,
            Err(c) => return c,
        },
    };
    match up::up(&mut cfg, &name, has_flag(&args, "--boot")) {
        Ok(_) => 0,
        Err(e) => {
            err(&e);
            1
        }
    }
}

// ---------------- sync / log / info / blame / bb ----------------

fn cmd_sync(args: Vec<String>) -> i32 {
    let cfg = Config::load();
    let pos = positional(&args, &["--exec", "--ignore"]);
    if pos.len() < 3 {
        err("用法: fy sync <设备> <本地目录> <远端目录> [--exec '重启命令'] [--once] [--ignore 名字]");
        return 2;
    }
    let d = match need_dev(&cfg, Some(&pos[0])) {
        Ok(d) => d,
        Err(c) => return c,
    };
    let hook = flag_val(&args, "--exec");
    let mut ignores = vec![];
    let mut it = args.iter().peekable();
    while let Some(a) = it.next() {
        if a == "--ignore" {
            if let Some(v) = it.next() {
                ignores.push(v.clone());
            }
        }
    }
    match sync::sync_cmd(
        &d,
        &PathBuf::from(&pos[1]),
        &pos[2],
        hook.as_deref(),
        has_flag(&args, "--once"),
        ignores,
    ) {
        Ok(_) => 0,
        Err(e) => {
            err(&format!("{}", e));
            1
        }
    }
}

fn cmd_log(args: Vec<String>) -> i32 {
    let cfg = Config::load();
    let pos = positional(&args, &["--save"]);
    let d = match need_dev(&cfg, pos.first().map(|s| s.as_str())) {
        Ok(d) => d,
        Err(c) => return c,
    };
    match logs::log_follow(&cfg, &d, flag_val(&args, "--save").as_deref()) {
        Ok(c) => c,
        Err(e) => {
            err(&format!("{}", e));
            1
        }
    }
}

fn cmd_info(args: Vec<String>) -> i32 {
    let cfg = Config::load();
    let d = match need_dev(&cfg, args.first().map(|s| s.as_str())) {
        Ok(d) => d,
        Err(c) => return c,
    };
    if jsonout::json_mode() {
        let f = fingerprint::remember(&d, &d.host);
        let (on, why) = probe_status(&d);
        return jsonout::emit_ok(vec![
            ("device", transport_json(&d)),
            ("online", J::b(on)),
            ("status", J::s(&why)),
            ("hostname", J::s(&f.hostname)),
            ("os", J::s(&f.os)),
            ("kernel", J::s(&f.kernel)),
            ("arch", J::s(&f.arch)),
            ("machine_id", J::s(&f.machine_id)),
            ("cpu_serial", J::s(&f.cpu_serial)),
            ("macs", J::strs(&f.macs)),
            ("last_ip", J::s(&f.last_ip)),
            ("last_seen", J::i(f.last_seen)),
            ("notes", J::s(&d.notes)),
        ]);
    }
    fingerprint::info_card(&d);
    0
}

fn cmd_hw(args: Vec<String>) -> i32 {
    let pos = positional(&args, &["--out", "--max-dt-nodes"]);
    if pos.first().map(|s| s.as_str()) == Some("brief") {
        let report = match pos.get(1) {
            Some(v) => PathBuf::from(v),
            None => {
                return fail(
                    code::USAGE,
                    "用法: fy hw brief <hardware.json> [--out peripherals.md]",
                )
            }
        };
        let output = flag_val(&args, "--out")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                report
                    .parent()
                    .unwrap_or_else(|| std::path::Path::new("."))
                    .join("peripherals.md")
            });
        if let Err(e) = peripheral_brief::write(&report, &output) {
            return fail(code::CONFIG, &format!("生成外设简报失败: {}", e));
        }
        if jsonout::json_mode() {
            return jsonout::emit_ok(vec![
                ("report", J::s(report.display().to_string())),
                ("peripheral_brief", J::s(output.display().to_string())),
            ]);
        }
        ok(&format!("外设简报已保存: {}", output.display()));
        return 0;
    }
    if pos.first().map(|s| s.as_str()) == Some("agent") {
        let out = match flag_val(&args, "--out") {
            Some(v) => PathBuf::from(v),
            None => return fail(code::USAGE, "用法: fy hw agent --out ./hwprobe.sh"),
        };
        if out.exists() {
            return fail(code::CONFIG, &format!("目标文件已存在: {}", out.display()));
        }
        if let Some(parent) = out.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                return fail(code::CONFIG, &format!("不能创建输出目录: {}", e));
            }
        }
        if let Err(e) = std::fs::write(&out, hwprobe::SCRIPT) {
            return fail(code::CONFIG, &format!("写 hwprobe agent 失败: {}", e));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&out, std::fs::Permissions::from_mode(0o755));
        }
        if jsonout::json_mode() {
            return jsonout::emit_ok(vec![("agent", J::s(out.display().to_string()))]);
        }
        ok(&format!("已导出目标端采集器: {}", out.display()));
        return 0;
    }
    if pos.is_empty() {
        return fail_hint(
            code::USAGE,
            "用法: fy hw <设备> [--out 目录] [--no-bundle] [--no-brief] [--keep-remote] [--include-identifiers] [--max-dt-nodes N]；或 fy hw brief <hardware.json>",
            Some("默认只读采集并清理目标端临时目录；fy hw agent --out ./hwprobe.sh 可单独导出脚本"),
        );
    }
    let d = match need_dev(&Config::load(), Some(&pos[0])) {
        Ok(v) => v,
        Err(c) => return c,
    };
    let max_dt_nodes = match flag_val(&args, "--max-dt-nodes") {
        Some(v) => match v.parse::<u32>() {
            Ok(n) => Some(n),
            Err(_) => return fail(code::USAGE, "--max-dt-nodes 必须是非负整数"),
        },
        None => None,
    };
    let options = hwprobe::Options {
        output_dir: flag_val(&args, "--out")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(format!("hwprobe-{}-{}", d.name, now_epoch()))),
        bundle: !has_flag(&args, "--no-bundle"),
        brief: !has_flag(&args, "--no-brief"),
        keep_remote: has_flag(&args, "--keep-remote"),
        include_identifiers: has_flag(&args, "--include-identifiers"),
        max_dt_nodes,
    };
    match hwprobe::collect(&d, &options) {
        Ok(r) => {
            if jsonout::json_mode() {
                return jsonout::emit_ok(vec![
                    ("device", transport_json(&d)),
                    ("output_dir", J::s(r.output_dir.display().to_string())),
                    ("report", J::s(r.report.display().to_string())),
                    (
                        "device_tree_archive",
                        r.archive
                            .map(|p| J::s(p.display().to_string()))
                            .unwrap_or(J::Null),
                    ),
                    (
                        "peripheral_brief",
                        r.brief
                            .map(|p| J::s(p.display().to_string()))
                            .unwrap_or(J::Null),
                    ),
                    ("remote_dir", J::s(r.remote_dir)),
                    ("identifiers_included", J::b(options.include_identifiers)),
                ]);
            }
            ok(&format!("硬件清单已保存: {}", r.report.display()));
            if let Some(archive) = r.archive {
                ok(&format!("原始设备树已保存: {}", archive.display()));
            }
            if let Some(brief) = r.brief {
                ok(&format!("外设简报已保存: {}", brief.display()));
            }
            0
        }
        Err(e) => fail(code::TRANSFER, &e),
    }
}

// ---------------- plugins ----------------

fn plugin_json(plugin: &plugins::Plugin) -> J {
    J::obj(vec![
        ("id", J::s(&plugin.id)),
        ("name", J::s(&plugin.name)),
        ("version", J::s(&plugin.version)),
        ("description", J::s(&plugin.description)),
        ("transport", J::s(&plugin.transport)),
        ("risk", J::s(&plugin.risk)),
        ("requires", J::strs(&plugin.requires)),
        ("arguments", J::strs(&plugin.arguments)),
        ("summary", J::s(&plugin.summary)),
        ("preview", J::strs(&plugin.preview)),
        ("path", J::s(plugin.dir.display().to_string())),
    ])
}

fn print_plugin(plugin: &plugins::Plugin) {
    println!("{} {} v{}", bold(&plugin.id), dim("-"), plugin.version);
    println!("  {}", plugin.name);
    println!("  {}", plugin.description);
    println!("  transport: {}    risk: {}", plugin.transport, plugin.risk);
    if !plugin.requires.is_empty() {
        println!("  host requirements: {}", plugin.requires.join(", "));
    }
    println!("  arguments: {}", plugins::display_arguments(plugin));
    println!("  package: {}", plugin.dir.display());
}

fn cmd_plugin(args: Vec<String>) -> i32 {
    let action = args.first().map(String::as_str).unwrap_or("ls");
    match action {
        "ls" | "list" => match plugins::list() {
            Ok(items) => {
                if jsonout::json_mode() {
                    let builtins = plugins::builtin_ids()
                        .iter()
                        .map(|id| id.to_string())
                        .collect::<Vec<_>>();
                    return jsonout::emit_ok(vec![
                        ("plugins", J::arr(items.iter().map(plugin_json).collect())),
                        ("builtins", J::strs(&builtins)),
                    ]);
                }
                if items.is_empty() {
                    println!("No plugins installed.");
                    println!("Built-ins available: {}", plugins::builtin_ids().join(", "));
                    println!("Install one with: fy plugin install sysroot-sync");
                } else {
                    for plugin in &items {
                        print_plugin(plugin);
                    }
                }
                0
            }
            Err(error) => fail(code::CONFIG, &error),
        },
        "show" => {
            let Some(id) = args.get(1) else {
                return fail(code::USAGE, "usage: fy plugin show <plugin-id>");
            };
            match plugins::load(id) {
                Ok(plugin) => {
                    if jsonout::json_mode() {
                        jsonout::emit_ok(vec![("plugin", plugin_json(&plugin))])
                    } else {
                        print_plugin(&plugin);
                        for step in &plugin.preview {
                            println!("  - {step}");
                        }
                        0
                    }
                }
                Err(error) => fail(code::CONFIG, &error),
            }
        }
        "install" => {
            let Some(source) = args.get(1) else {
                return fail_hint(
                    code::USAGE,
                    "usage: fy plugin install <builtin-id|local-plugin-directory> [--force]",
                    Some("Built-in: fy plugin install sysroot-sync; local packages need plugin.toml and its declared entrypoint"),
                );
            };
            if jsonout::json_mode() {
                return fail(code::USAGE, "plugin installation writes local files and is not available with --json");
            }
            let force = has_flag(&args, "--force");
            let installed = if plugins::builtin_ids().contains(&source.as_str()) {
                plugins::install_builtin(source, force)
            } else {
                plugins::install_local(&PathBuf::from(source), force)
            };
            match installed {
                Ok(plugin) => {
                    ok(&format!("plugin '{}' installed", plugin.id));
                    print_plugin(&plugin);
                    0
                }
                Err(error) => fail(code::CONFIG, &error),
            }
        }
        "run" => {
            if jsonout::json_mode() {
                return fail(code::USAGE, "plugin run streams plugin output and is not available with --json");
            }
            let (Some(id), Some(device_name)) = (args.get(1), args.get(2)) else {
                return fail_hint(
                    code::USAGE,
                    "usage: fy plugin run <plugin-id> <device> [-- plugin arguments]",
                    Some("Example: fy plugin run sysroot-sync rk -- --dest /opt/sysroot"),
                );
            };
            let plugin = match plugins::load(id) {
                Ok(plugin) => plugin,
                Err(error) => return fail(code::CONFIG, &error),
            };
            let device = match need_dev(&Config::load(), Some(device_name)) {
                Ok(device) => device,
                Err(exit) => return exit,
            };
            let plugin_args: Vec<String> = args
                .iter()
                .skip(3)
                .filter(|argument| argument.as_str() != "--")
                .cloned()
                .collect();
            match plugins::preview(&plugin, &device, &plugin_args) {
                Ok(steps) => {
                    for step in steps {
                        info(&step);
                    }
                }
                Err(error) => return fail(code::MISSING_DEP, &error),
            }
            match plugins::run_inherit_plugin(&plugin, &device, &plugin_args) {
                Ok(0) => 0,
                Ok(status) => fail(code::FAIL, &format!("plugin '{}' exited with status {status}", plugin.id)),
                Err(error) => fail(code::FAIL, &format!("plugin '{}' failed: {error}", plugin.id)),
            }
        }
        "help" | "-h" | "--help" => {
            println!("fy plugin ls");
            println!("fy plugin show <plugin-id>");
            println!("fy plugin install <builtin-id|local-plugin-directory> [--force]");
            println!("fy plugin run <plugin-id> <device> [-- plugin arguments]");
            println!();
            println!("Plugins are local, reviewable packages. {}.", plugins::source_hint());
            println!("The built-in sysroot-sync plugin mirrors /lib, /usr/lib and /usr/include over SSH.");
            0
        }
        other => fail(code::USAGE, &format!("fy plugin supports ls, show, install, run; not '{other}'")),
    }
}

fn cmd_blame(args: Vec<String>) -> i32 {
    let cfg = Config::load();
    let pos = positional(&args, &["-n"]);
    let d = match need_dev(&cfg, pos.first().map(|s| s.as_str())) {
        Ok(d) => d,
        Err(c) => return c,
    };
    let n = flag_val(&args, "-n")
        .and_then(|v| v.parse().ok())
        .unwrap_or(60);
    if jsonout::json_mode() {
        let dir = blackbox::incidents_dir(&d.name);
        let mut files: Vec<std::path::PathBuf> = std::fs::read_dir(&dir)
            .map(|rd| rd.flatten().map(|e| e.path()).collect())
            .unwrap_or_default();
        files.sort();
        let latest = files.last().cloned();
        let text = latest.as_ref().map(|p| slurp(p)).unwrap_or_default();
        let tail: Vec<&str> = text.lines().rev().take(n).collect();
        let tail: Vec<String> = tail.into_iter().rev().map(|s| s.to_string()).collect();
        return jsonout::emit_ok(vec![
            ("device", J::s(&d.name)),
            ("incident_count", J::i(files.len() as i64)),
            (
                "latest",
                latest
                    .map(|p| J::s(p.display().to_string()))
                    .unwrap_or(J::Null),
            ),
            ("lines", J::strs(&tail)),
        ]);
    }
    blackbox::blame(&d.name, n);
    0
}

fn cmd_bb(args: Vec<String>) -> i32 {
    let cfg = Config::load();
    match args.first().map(|s| s.as_str()) {
        Some("start") => {
            let d = match need_dev(&cfg, args.get(1).map(|s| s.as_str())) {
                Ok(d) => d,
                Err(c) => return c,
            };
            match blackbox::start(&cfg, &d.name) {
                Ok(_) => {
                    jsonout::emit_ok(vec![("device", J::s(&d.name)), ("recording", J::b(true))])
                }
                Err(e) => fail(code::FAIL, &e),
            }
        }
        Some("stop") => match args.get(1) {
            Some(n) => {
                blackbox::stop(n);
                jsonout::emit_ok(vec![("device", J::s(n)), ("recording", J::b(false))])
            }
            None => fail(code::USAGE, "用法: fy bb stop <设备>"),
        },
        None | Some("status") => {
            if jsonout::json_mode() {
                let st = config::State::load();
                let items: Vec<J> = st
                    .doc
                    .children("bb")
                    .into_iter()
                    .map(|name| {
                        let pid = st.get_int(&format!("bb.{}", name), "pid") as i32;
                        let n = std::fs::read_dir(blackbox::incidents_dir(&name))
                            .map(|r| r.count())
                            .unwrap_or(0);
                        J::obj(vec![
                            ("device", J::s(&name)),
                            ("alive", J::b(pid_alive(pid))),
                            ("pid", J::i(pid as i64)),
                            ("incidents", J::i(n as i64)),
                        ])
                    })
                    .collect();
                return jsonout::emit_ok(vec![("blackboxes", J::arr(items))]);
            }
            blackbox::status(&cfg);
            0
        }
        Some(other) => fail(
            code::USAGE,
            &format!("fy bb 只有 start/stop/status，没有 '{}'", other),
        ),
    }
}

fn cmd_all(args: Vec<String>) -> i32 {
    let (head, cmdv) = split_dashdash(&args);
    let cmd = match cmdv {
        Some(v) if !v.is_empty() => v.join(" "),
        _ => {
            return fail_hint(
                code::USAGE,
                "缺少要执行的命令",
                Some("用法: fy all [设备前缀...] -- <命令>，例: fy all -- uname -a"),
            )
        }
    };
    let cfg = Config::load();
    if jsonout::json_mode() {
        return match all::all_json(&cfg, &head, &cmd) {
            Ok(rs) => {
                let items: Vec<J> = rs
                    .iter()
                    .map(|(name, code_, out, err_)| {
                        J::obj(vec![
                            ("device", J::s(name)),
                            ("exit_code", J::i(*code_ as i64)),
                            ("stdout", J::s(out)),
                            ("stderr", J::s(err_)),
                        ])
                    })
                    .collect();
                let failed = rs.iter().filter(|(_, c, _, _)| *c != 0).count();
                jsonout::emit_ok(vec![
                    ("command", J::s(&cmd)),
                    ("results", J::arr(items)),
                    ("failed", J::i(failed as i64)),
                ])
            }
            Err(e) => fail(code::NO_DEVICE, &e),
        };
    }
    all::all_cmd(&cfg, &head, &cmd)
}

// ---------------- keyup / forget / wifi / doctor / fix ----------------

fn cmd_keyup(args: Vec<String>) -> i32 {
    let cfg = Config::load();
    let d = match need_dev(&cfg, args.first().map(|s| s.as_str())) {
        Ok(d) => d,
        Err(c) => return c,
    };
    if d.transport != Transport::Ssh {
        return fail(code::UNSUPPORTED, "keyup 只对 ssh 设备有意义");
    }
    match sshx::keyup(&d) {
        Ok(_) => jsonout::emit_ok(vec![
            ("device", J::s(&d.name)),
            ("passwordless", J::b(true)),
        ]),
        Err(e) => fail(code::FAIL, &format!("{}", e)),
    }
}

fn cmd_forget(args: Vec<String>) -> i32 {
    let cfg = Config::load();
    let d = match need_dev(&cfg, args.first().map(|s| s.as_str())) {
        Ok(d) => d,
        Err(c) => return c,
    };
    sshx::forget(&d);
    jsonout::emit_ok(vec![("device", J::s(&d.name)), ("forgotten", J::b(true))])
}

fn cmd_wifi(args: Vec<String>) -> i32 {
    let mut cfg = Config::load();
    let d = match need_dev(&cfg, args.first().map(|s| s.as_str())) {
        Ok(d) => d,
        Err(c) => return c,
    };
    if d.transport != Transport::Adb {
        return fail(
            code::UNSUPPORTED,
            "wifi 是 adb 设备专属（把 USB adb 切成 WiFi adb）",
        );
    }
    match adbx::wifi(&mut cfg, &d) {
        Ok(_) => {
            let ep = Config::load()
                .devices
                .get(&d.name)
                .map(|x| x.endpoint())
                .unwrap_or_default();
            jsonout::emit_ok(vec![("device", J::s(&d.name)), ("endpoint", J::s(&ep))])
        }
        Err(e) => fail(code::FAIL, &format!("{}", e)),
    }
}

fn cmd_doctor(args: Vec<String>) -> i32 {
    let cfg = Config::load();
    let d = args.first().and_then(|n| cfg.find(n));
    if jsonout::json_mode() {
        return jsonout::emit_ok(doctor::doctor_json(&cfg, d.as_ref()));
    }
    doctor::doctor(&cfg, d.as_ref());
    0
}

fn cmd_ui(args: Vec<String>) -> i32 {
    let port = flag_val(&args, "--port")
        .and_then(|p| p.parse().ok())
        .unwrap_or(7900);
    let open = !has_flag(&args, "--no-open");
    match ui::run(port, open) {
        Ok(_) => 0,
        Err(e) => {
            err(&format!("{}", e));
            1
        }
    }
}

fn cmd_fix(args: Vec<String>) -> i32 {
    match args.first().map(|s| s.as_str()) {
        Some("time") => {
            let cfg = Config::load();
            let d = match need_dev(&cfg, args.get(1).map(|s| s.as_str())) {
                Ok(d) => d,
                Err(c) => return c,
            };
            match doctor::fix_time(&d) {
                Ok(_) => {
                    jsonout::emit_ok(vec![("device", J::s(&d.name)), ("time_synced", J::b(true))])
                }
                Err(e) => fail(code::FAIL, &e),
            }
        }
        _ => fail(
            code::USAGE,
            "目前有: fy fix time <设备>（把主机时间打进板子）",
        ),
    }
}

// ---------------- serve / net / watch / proxy ----------------

fn cmd_serve(args: Vec<String>) -> i32 {
    let valflags = ["--port", "--bind", "--upload", "--token", "--for"];
    let pos = positional(&args, &valflags);
    let port: u16 = flag_val(&args, "--port")
        .and_then(|p| p.parse().ok())
        .unwrap_or(8000);
    let cfg = Config::load();
    let for_dev = match flag_val(&args, "--for") {
        Some(n) => match need_dev(&cfg, Some(&n)) {
            Ok(d) => Some(d),
            Err(c) => return c,
        },
        None => None,
    };
    let upload = if has_flag(&args, "--upload") {
        Some(PathBuf::from(
            flag_val(&args, "--upload").unwrap_or_else(|| ".".into()),
        ))
    } else {
        None
    };
    let roots: Vec<PathBuf> = pos.iter().map(PathBuf::from).collect();
    serve::serve_cmd(
        serve::ServeCli {
            roots,
            port,
            bind: flag_val(&args, "--bind"),
            upload,
            token: flag_val(&args, "--token"),
            no_token: has_flag(&args, "--no-token"),
            once: has_flag(&args, "--once"),
        },
        for_dev.as_ref(),
    )
}

fn cmd_net(args: Vec<String>) -> i32 {
    let pos = positional(&args, &["-c", "--count"]);
    let cfg = Config::load();
    let d = match need_dev(&cfg, pos.first().map(|s| s.as_str())) {
        Ok(d) => d,
        Err(c) => return c,
    };
    let count: u32 = flag_val(&args, "-c")
        .or_else(|| flag_val(&args, "--count"))
        .and_then(|v| v.parse().ok())
        .unwrap_or(8);
    let speed = !has_flag(&args, "--no-speed");
    let r = netdiag::diagnose(&d, count.clamp(1, 100), speed);
    if jsonout::json_mode() {
        return jsonout::emit_ok(netdiag::report_json(&r));
    }
    netdiag::print_report(&r);
    if r.latency.recv == 0 {
        code::UNREACHABLE
    } else {
        0
    }
}

fn cmd_watch(args: Vec<String>) -> i32 {
    match args.first().map(|s| s.as_str()) {
        Some("start") => {
            let iv = flag_val(&args, "--interval")
                .and_then(|v| v.parse().ok())
                .unwrap_or(15u64);
            match watchd::start(iv, false) {
                Ok(pid) => {
                    if jsonout::json_mode() {
                        return jsonout::emit_ok(vec![
                            ("running", J::b(true)),
                            ("pid", J::i(pid as i64)),
                        ]);
                    }
                    0
                }
                Err(e) => fail(code::FAIL, &e),
            }
        }
        Some("stop") => {
            watchd::stop();
            jsonout::emit_ok(vec![("running", J::b(false))])
        }
        None | Some("status") => {
            let s = watchd::status();
            if jsonout::json_mode() {
                let devs: Vec<J> = s
                    .devices
                    .iter()
                    .map(|(n, last, rc)| {
                        J::obj(vec![
                            ("device", J::s(n)),
                            ("last_ok", J::i(*last)),
                            ("reconnects", J::i(*rc)),
                        ])
                    })
                    .collect();
                return jsonout::emit_ok(vec![
                    ("running", J::b(s.running)),
                    ("pid", J::i(s.pid as i64)),
                    ("interval", J::i(s.interval)),
                    ("started", J::i(s.started)),
                    ("devices", J::arr(devs)),
                ]);
            }
            if !s.running {
                info("隧道保活没在跑。开启: fy watch start");
                return 0;
            }
            println!(
                "{} pid {} · 每 {}s 探一次 · 已运行 {}",
                green("● 保活中"),
                s.pid,
                s.interval,
                human_ago(s.started).trim_end_matches(" ago")
            );
            if s.devices.is_empty() {
                println!(
                    "{}",
                    dim("还没盯上任何设备（建个转发或 fy share 就会自动纳管）")
                );
            } else {
                let rows: Vec<Vec<String>> = s
                    .devices
                    .iter()
                    .map(|(n, last, rc)| {
                        vec![
                            bold(n),
                            if *last > 0 {
                                human_ago(*last)
                            } else {
                                dim("never")
                            },
                            rc.to_string(),
                        ]
                    })
                    .collect();
                print_table(&["设备", "上次探活", "自愈次数"], &rows);
            }
            0
        }
        Some(other) => fail(
            code::USAGE,
            &format!("fy watch 只有 start/stop/status，没有 '{}'", other),
        ),
    }
}

fn cmd_proxy(args: Vec<String>) -> i32 {
    let sub = args.first().cloned().unwrap_or_else(|| "status".into());
    let port: u16 = flag_val(&args, "--port")
        .and_then(|p| p.parse().ok())
        .unwrap_or_else(proxyd::current_port);
    match sub.as_str() {
        "start" => {
            let up = match flag_val(&args, "--upstream") {
                Some(u) => match proxyd::Upstream::parse(&u) {
                    Ok(u) => Some(u),
                    Err(e) => return fail(code::USAGE, &e),
                },
                None => None,
            };
            match proxyd::ensure_running_with(port, up) {
                Ok(p) => jsonout::emit_ok(vec![
                    ("running", J::b(true)),
                    ("port", J::i(p as i64)),
                    ("upstream", J::s(proxyd::current_upstream().as_arg())),
                ]),
                Err(e) => fail(code::FAIL, &format!("{}", e)),
            }
        }
        "stop" => {
            proxyd::stop();
            jsonout::emit_ok(vec![("running", J::b(false))])
        }
        "status" => {
            let pid = proxyd::running_pid();
            let up = proxyd::current_upstream();
            let sharing = share::active();
            if jsonout::json_mode() {
                return jsonout::emit_ok(vec![
                    ("running", J::b(pid > 0)),
                    ("pid", J::i(pid as i64)),
                    ("port", J::i(proxyd::current_port() as i64)),
                    ("upstream", J::s(up.as_arg())),
                    ("protocols", J::arr(vec![J::s("http"), J::s("socks5")])),
                    (
                        "sharing",
                        J::arr(
                            sharing
                                .iter()
                                .map(|(dev, mode)| {
                                    J::obj(vec![("device", J::s(dev)), ("mode", J::s(mode))])
                                })
                                .collect(),
                        ),
                    ),
                ]);
            }
            if pid > 0 {
                println!(
                    "{} pid {} · 127.0.0.1:{} · HTTP + SOCKS5 · 上游 {}",
                    green("● 运行中"),
                    pid,
                    proxyd::current_port(),
                    up.describe()
                );
            } else {
                info("代理没在跑。`fy share <设备>` 会自动拉起，或 `fy proxy start`");
            }
            if !sharing.is_empty() {
                println!(
                    "{} {}",
                    dim("正在借网:"),
                    sharing
                        .iter()
                        .map(|(d, m)| format!("{}({})", d, m))
                        .collect::<Vec<_>>()
                        .join(" ")
                );
            }
            0
        }
        other => fail(
            code::USAGE,
            &format!("fy proxy 只有 start/stop/status，没有 '{}'", other),
        ),
    }
}

// ---------------- help ----------------

fn cmd_help(args: Vec<String>) -> i32 {
    if jsonout::json_mode() || has_flag(&args, "--json") {
        return emit_catalog();
    }
    print_help();
    0
}

/// `fy help --json`：把命令、参数、退出码一次性交给 agent，
/// 省得它去猜或者去 grep --help 的中文排版。
fn emit_catalog() -> i32 {
    let cmd = |name: &str, usage: &str, about: &str, json: bool| {
        J::obj(vec![
            ("name", J::s(name)),
            ("usage", J::s(usage)),
            ("about", J::s(about)),
            ("json", J::b(json)),
        ])
    };
    let cmds = vec![
        cmd("ls", "fy ls", "设备总览：并行探活 + 指纹身份", true),
        cmd("add", "fy add <名字> [--ssh user@host[:port]] [--adb [serial]] [--serial /dev/x --baud N] [--password P] [--legacy]", "新建/更新设备档案", true),
        cmd("rm", "fy rm <设备>", "删除设备档案", true),
        cmd("sh", "fy sh <设备> [-- <命令>]", "进 shell；带 -- 则执行一条命令并回传 stdout/stderr/退出码", true),
        cmd("push", "fy push <设备> <本地> [远端] | fy push --all <本地> [远端] [--only 前缀]", "上传：断点续传 + sha256 校验 + 进度；--all 并行分发到所有在线设备", true),
        cmd("pull", "fy pull <设备> <远端> [本地]", "下载：断点续传 + 校验", true),
        cmd("cp", "fy cp <源> <目标>", "统一传输入口，路径写 设备名:/路径；板↔板直传经主机中转不落盘", true),
        cmd("serve", "fy serve [路径...] [--port N] [--for 设备] [--upload [目录]] [--no-token] [--once]", "局域网快传：起 HTTP 服务给板子 wget，支持 Range 与反向上传（常驻，不支持 --json）", false),
        cmd("run", "fy run <设备> <可执行文件> [参数...]", "push + chmod + 运行 + 回传退出码", false),
        cmd("debug", "fy debug <设备> <可执行文件> [--port 3333]", "gdbserver + 端口转发一条龙", false),
        cmd("fwd", "fy fwd <设备> <规则> | fy fwd ls | fy fwd rm <ID|all>", "端口转发：8080 · 8080:80 · R:9000:8000 · D:1080", true),
        cmd("share", "fy share <设备> [--nat] [--persist] [--upstream URL] [--off]", "借网给板子：HTTP+SOCKS5 代理 + 反向隧道，可链到主机的上游代理", true),
        cmd("proxy", "fy proxy start|stop|status [--port N] [--upstream URL]", "内置代理守护进程的直接管理", true),
        cmd("watch", "fy watch start|stop|status [--interval N]", "隧道保活：断线自动重连并重放所有转发/借网", true),
        cmd("net", "fy net <设备> [-c N] [--no-speed]", "网络体检：延迟/抖动/丢包、MTU、路由、DNS、出网、上下行实测带宽", true),
        cmd("scan", "fy scan [--subnet CIDR] [--ports 2222,2200] [--add] [--no-mdns]", "发现设备：mDNS + 网段扫描 + 指纹认领", true),
        cmd("info", "fy info <设备>", "身份卡片：内核/架构/MAC/machine-id", true),
        cmd("hw", "fy hw <设备> [--out 目录] [--no-bundle] [--no-brief] [--include-identifiers] [--max-dt-nodes N] | fy hw brief <hardware.json> [--out peripherals.md]", "一次性采集硬件清单，或离线从 JSON 生成可读的 peripherals.md", true),
        cmd("plugin", "fy plugin ls|show|install|run", "本地可审阅功能插件：安装、预检、运行；内置 sysroot-sync 可同步交叉编译 sysroot", true),
        cmd("up", "fy up <设备> [--boot]", "通道爬升：串口登录→配网→ssh+免密", false),
        cmd("usb", "fy usb net|gadget|install", "USB 一键配网", false),
        cmd("sync", "fy sync <设备> <本地目录> <远端目录> [--exec 命令] [--once]", "保存即上板", false),
        cmd("log", "fy log <设备> [--save 文件]", "跟日志：journalctl/syslog/dmesg/logcat 自动选", false),
        cmd("top", "fy top", "多板实时仪表盘", false),
        cmd("all", "fy all [设备前缀...] -- <命令>", "多板并行执行同一条命令", true),
        cmd("bb", "fy bb start|stop|status [设备]", "串口黑匣子", true),
        cmd("blame", "fy blame <设备> [-n 行数]", "最近一次崩溃现场", true),
        cmd("keyup", "fy keyup <设备>", "装公钥转免密", true),
        cmd("forget", "fy forget <设备>", "清 host key（板子重刷后用）", true),
        cmd("wifi", "fy wifi <设备>", "adb 从 USB 切到 WiFi", true),
        cmd("doctor", "fy doctor [设备]", "主机自检 / 板子体检", true),
        cmd("fix", "fy fix time <设备>", "把主机时间打进板子", true),
        cmd("ui", "fy ui [--port 7900]", "浏览器图形工作台（常驻）", false),
    ];
    let codes: Vec<J> = [
        (code::OK, "一切正常"),
        (code::FAIL, "兜底失败"),
        (code::USAGE, "命令行用法错误"),
        (code::NO_DEVICE, "没有这台设备"),
        (code::AMBIGUOUS, "设备名有歧义（前缀匹配到多台）"),
        (code::UNREACHABLE, "设备不可达"),
        (code::UNSUPPORTED, "该通道不支持此操作"),
        (code::TRANSFER, "传输失败"),
        (code::CHECKSUM, "校验不一致，数据可能损坏"),
        (code::TIMEOUT, "超时"),
        (code::MISSING_DEP, "主机缺少 ssh/adb 等外部依赖"),
        (code::CONFIG, "配置或运行态文件有问题"),
        (code::NEED_INPUT, "需要人拍板但当前是非交互模式"),
    ]
    .iter()
    .map(|(c, desc)| {
        J::obj(vec![
            ("code", J::i(*c as i64)),
            ("kind", J::s(code::name(*c))),
            ("meaning", J::s(*desc)),
        ])
    })
    .collect();

    jsonout::emit_ok(vec![
        ("version", J::s(VERSION)),
        (
            "contract",
            J::obj(vec![
                ("stdout", J::s("--json 时 stdout 只有一份 JSON 文档，过程信息全在 stderr")),
                ("ok_field", J::s("ok=true/false 是唯一权威判据；fy sh/run 会透传远端退出码，可能与 ferry 码重叠")),
                ("non_interactive", J::s("--json 隐含非交互：不弹选择器、不问 y/n；需要人拍板时以 code 19 失败并给 hint")),
                ("env", J::s("FERRY_JSON=1 等价于处处加 --json；FERRY_HOME 换配置目录")),
                ("dry_run", J::s("-n/--dry-run 只打印将执行的命令，不产生副作用")),
            ]),
        ),
        (
            "global_flags",
            J::arr(vec![
                J::s("--json"),
                J::s("-n/--dry-run"),
                J::s("-y/--yes/--non-interactive"),
                J::s("-q/--quiet"),
                J::s("--plain/--no-color"),
                J::s("-V/--version"),
            ]),
        ),
        ("commands", J::arr(cmds)),
        ("exit_codes", J::arr(codes)),
    ])
}

// ---------------- help ----------------

fn print_help() {
    let h = format!(
        r#"{title} v{v} — ssh / adb / 串口，一套命令全通

{s1}
  fy                         设备总览（并行探活 + 指纹身份）
  fy add <名> --ssh root@ip[:port] [--password P] [--legacy] [--serial /dev/x]
  fy add <名> --adb [serial] | --serial /dev/x --baud 1500000
  fy scan [--subnet CIDR] [--ports 2222,2200] [--add] [--no-mdns]   mDNS + 并发扫段 + 老朋友换IP自动认领
  fy sh [设备] [-- 命令]     进 shell / 跑一条命令（串口自动经黑匣子共享）
  fy info <设备>             身份卡片: 内核/架构/MAC/machine-id/实时状态
  fy hw <设备> [--out 目录]  一次性硬件快照: JSON/设备树 + 外设简报 peripherals.md
  fy hw brief <hardware.json> [--out peripherals.md]  离线从既有 JSON 重建外设简报
  fy plugin ls/install/run    本地功能扩展；内置 sysroot-sync 可拉取目标机库和头文件

{s2}
  fy push <设备> <本地> [远端]      上传: 断点续传 + sha256 校验 + 进度条
  fy push --all <本地> [远端]       并行分发到所有在线板子 [--only 前缀]
  fy pull <设备> <远端> [本地]      下载: 同样续传 + 校验
  fy cp <源> <目标>                 路径写 设备:/路径; 板↔板直传经主机不落盘
  fy serve [路径...] [--for 设备]   局域网快传: 板上一条 wget 就拉走
                                    [--upload 目录] 反向收文件 [--port N] [--once]
  传输旗标: --force 强制重传 · --no-resume 关续传 · --no-verify 关校验
            --scp 退回老的 scp/tar 级联（无续传无校验，仅作逃生口）

{s3}
  fy up <设备> [--boot]      通道爬升: 串口自动登录→探测→USB配网/DHCP→ssh+免密
  fy usb net [--share] [--as 名]   插线一键: 识别新网口→配IP→探板→(NAT借网)
  fy usb gadget --out f.sh [--mode ncm|rndis]  生成板端 gadget 脚本
  fy usb install <设备> [--autostart]          推脚本上板+注册开机自启
  fy keyup <设备>            免密（自动生成密钥, 兼容 dropbear 路径）
  fy forget <设备>           板子重刷后清 host key
  fy wifi <设备>             adb 一键切 WiFi（拔线自由）

{s4}
  fy fwd <设备> 8080         转发管理: 8080 · 8080:80 · R:9000:8000 · D:1080
  fy fwd ls / rm <ID|all>    活隧道挂在 ssh 连接复用上, 断线可见
  fy watch start/stop/status 隧道保活: 断线自动重连并重放所有转发与借网
  fy share <设备>            借网给板子: HTTP+SOCKS5 同端口代理 + 反向隧道, 零 sudo
  fy share <设备> --upstream http://127.0.0.1:7897   把主机的梯子一起借给板子
  fy share <设备> --nat      直连板真 NAT(全协议); --off 关闭; --persist 写进板子
  fy proxy start/stop/status 直接管代理守护进程 [--port N] [--upstream URL|auto]
  fy net <设备> [-c N]       网络体检: 延迟/抖动/丢包 · MTU · 路由 · DNS · 出网 · 实测带宽

{s5}
  fy run <设备> ./a.out [参数]     push+chmod+运行+回传退出码, 像本地一样
  fy debug <设备> ./a.out [--port] gdbserver+转发一条龙, 给出 gdb 连接命令
  fy sync <设备> <本地> <远端> [--exec '命令'] 保存即上板(rsync/tar/adb 自动选)
  fy plugin install sysroot-sync
  fy plugin run sysroot-sync rk -- --dest /opt/sysroot
  fy log <设备> [--save f]   journalctl/syslog/dmesg/logcat 自动选
  fy top                     多板实时仪表盘 (CPU/内存/温度/rootfs)
  fy all [前缀] -- <命令>    多板并行执行, 彩色前缀区分

{s6}
  fy ui [--port 7900]        图形工作台: 系统终端(主) + 便捷工具侧栏(浏览器打开)
  fy bb start/stop/status [设备]   黑匣子: 后台录串口+panic侦测+桌面通知
  fy blame <设备>            最近一次崩溃现场（重启也不丢）
  fy doctor [设备]           主机自检 / 板子体检(时间/只读盘/空间/DNS/OOM)
  fy fix time <设备>         一键对时, 告别 1970 年

{s7}
  --json         机器可读输出（stdout 只有一份 JSON，过程信息走 stderr，且全程不交互）
  fy help --json 命令清单 + 参数 + 退出码表，agent 一次读全
  -n --dry-run   只看不做（打印将执行的每条命令）
  -y --yes       非交互（不弹选择器、不问 y/n）
  --plain 关颜色    -q 安静    -V 版本    FERRY_JSON=1 等价于处处 --json
  档案: ~/.config/ferry/devices.toml   指纹: facts/   密码档案建议尽快 keyup 转免密
"#,
        title = bold("ferry (fy)"),
        v = VERSION,
        s1 = cyan("── 设备与交互 ──────────────────────────────"),
        s2 = cyan("── 文件传输 ────────────────────────────────"),
        s3 = cyan("── 通道爬升与配网 ──────────────────────────"),
        s4 = cyan("── 网络: 转发 / 借网 / 体检 ────────────────"),
        s5 = cyan("── 开发闭环 ────────────────────────────────"),
        s6 = cyan("── 黑匣子与体检 ────────────────────────────"),
        s7 = cyan("── 全局 / 给 AI agent ──────────────────────"),
    );
    println!("{}", h);
}
