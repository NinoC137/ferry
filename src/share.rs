//! `fy share`：让板子借主机上网。
//! 默认"代理模式"：零 sudo、任何 ssh/adb 可达的板子都能用（内置 HTTP 代理 + 反向隧道）。
//! `--nat` 模式：直连网段（USB 网卡/网线直连）做真 NAT，全协议通吃（要 sudo）。

use crate::config::{Config, Device, State, Transport};
use crate::proxyd::{self, Upstream};
use crate::sshx;
use crate::usbnet;
use crate::util::*;
use crate::watchd;

/// 借网。`upstream` 给定时，板子的流量会经主机再送到那个上游代理
/// （主机自己在梯子后面时，这一步等于把梯子借给了板子）。
pub fn enable(
    cfg: &Config,
    d: &Device,
    nat: bool,
    persist: bool,
    android_global: bool,
    upstream: Option<Upstream>,
) -> Result<(), String> {
    if nat {
        if upstream.is_some() {
            warn("--nat 是三层转发，上游代理对它不生效（那是应用层的事）");
        }
        return enable_nat(cfg, d);
    }
    let port =
        proxyd::ensure_running_with(proxyd::current_port(), upstream).map_err(|e| e.to_string())?;
    let up = proxyd::current_upstream();
    match d.transport {
        Transport::Ssh => {
            sshx::ensure_master(d).map_err(|e| e.to_string())?;
            let spec = crate::fwd::Spec::R {
                rp: port,
                lh: "127.0.0.1".into(),
                lp: port,
            };
            let args = argv(&["-R", &format!("{}:127.0.0.1:{}", port, port)]);
            let out = sshx::master_ctl(d, "forward", &args).map_err(|e| e.to_string())?;
            if out.status != 0 && !dry() {
                return Err(format!("反向隧道失败: {}", out.stderr.trim()));
            }
            let mut st = State::load();
            st.set_str(&format!("share.{}", d.name), "mode", "proxy");
            st.save();
            let _ = spec;
            watchd::autostart_if_useful();
            ok(&format!(
                "{} 现在可以通过 127.0.0.1:{} 上网了（HTTP + SOCKS5 同端口，上游 {}）",
                d.name,
                port,
                up.describe()
            ));
            let envs = format!(
                "export http_proxy=http://127.0.0.1:{p} https_proxy=http://127.0.0.1:{p} all_proxy=socks5://127.0.0.1:{p}",
                p = port
            );
            if persist {
                let script = format!("#!/bin/sh\n{}\n", envs);
                let okk =
                    sshx::write_remote_file(d, "/etc/profile.d/ferry-proxy.sh", &script, "755")
                        .map_err(|e| e.to_string())?;
                if okk {
                    ok("已写入板端 /etc/profile.d/ferry-proxy.sh（重新登录生效）");
                } else {
                    warn("写 /etc/profile.d 失败（板上可能没有这个目录），手动 export 也行");
                }
            }
            println!(
                "\n板端用法（当前会话立即生效）:\n  {}\n  wget/curl/opkg/apt 走 http_proxy；git/ssh 之类走 all_proxy 的 SOCKS5。\n  域名一律在主机侧解析，板子不用配 DNS。",
                cyan(&envs)
            );
        }
        Transport::Adb => {
            let o = run_capture(
                &crate::adbx::adb_argv(
                    d,
                    &[
                        "reverse",
                        &format!("tcp:{}", port),
                        &format!("tcp:{}", port),
                    ],
                ),
                &[],
            )
            .map_err(|e| e.to_string())?;
            if o.status != 0 && !dry() {
                return Err(format!("adb reverse 失败: {}", o.stderr.trim()));
            }
            let mut st = State::load();
            st.set_str(&format!("share.{}", d.name), "mode", "proxy");
            st.save();
            ok(&format!(
                "{} 现在可以通过 127.0.0.1:{} 上网了（HTTP + SOCKS5，上游 {}）",
                d.name,
                port,
                up.describe()
            ));
            if android_global {
                let _ = run_inherit(
                    &crate::adbx::adb_argv(
                        d,
                        &[
                            "shell",
                            &format!("settings put global http_proxy 127.0.0.1:{}", port),
                        ],
                    ),
                    &[],
                );
                warn("已设置 Android 全局代理（走 USB 借网）。取消: fy share <dev> --off");
            } else {
                println!(
                    "\n板端 shell 用法:\n  {}\nAndroid 应用层想全局走代理再加 --android-global（会改 settings）。",
                    cyan(&format!(
                        "export http_proxy=http://127.0.0.1:{p} https_proxy=http://127.0.0.1:{p} all_proxy=socks5://127.0.0.1:{p}",
                        p = port
                    ))
                );
            }
        }
        Transport::Serial => return Err("串口设备先 `fy up` 打通网络通道再共享上网".into()),
    }
    Ok(())
}

fn enable_nat(_cfg: &Config, d: &Device) -> Result<(), String> {
    if d.transport != Transport::Ssh {
        return Err("--nat 需要 ssh 可达的直连板子（USB 网卡/网线直连）".into());
    }
    // 1) 找到通往板子的本机网口与网段
    let (ifname, subnet) = usbnet::route_iface_for(&d.host).ok_or_else(|| {
        format!(
            "找不到通往 {} 的本机直连网口（--nat 只适合直连网段）",
            d.host
        )
    })?;
    info(&format!("板子在 {} 网口的 {} 网段", ifname, subnet));
    // 2) 主机开 NAT
    usbnet::nat_enable(&subnet).map_err(|e| e.to_string())?;
    // 3) 板端设默认路由 + DNS（host 侧地址当网关）
    let gw = usbnet::local_ip_on(&ifname).unwrap_or_else(|| "10.55.0.1".into());
    let cmd = format!(
        "ip route replace default via {gw} 2>/dev/null || route add default gw {gw}; \
         grep -q nameserver /etc/resolv.conf 2>/dev/null || printf 'nameserver 223.5.5.5\\nnameserver 8.8.8.8\\n' > /etc/resolv.conf; \
         echo FERRY_NAT_OK",
        gw = gw
    );
    let out = sshx::exec_capture(d, &cmd).map_err(|e| e.to_string())?;
    if !dry() && !out.stdout.contains("FERRY_NAT_OK") {
        warn(&format!("板端路由设置可能失败: {}", out.stderr.trim()));
    }
    let mut st = State::load();
    st.set_str(&format!("share.{}", d.name), "mode", "nat");
    st.set_str(&format!("share.{}", d.name), "subnet", &subnet);
    st.save();
    ok(&format!(
        "{} 已获得完整外网（NAT 模式，全协议）。测试: fy sh {} -- ping -c1 223.5.5.5",
        d.name, d.name
    ));
    Ok(())
}

pub fn disable(cfg: &Config, d: &Device) -> Result<(), String> {
    let mut st = State::load();
    let table = format!("share.{}", d.name);
    let mode = st.get_str(&table, "mode");
    match mode.as_str() {
        "nat" => {
            usbnet::nat_disable().map_err(|e| e.to_string())?;
        }
        _ => {
            let port = proxyd::current_port();
            match d.transport {
                Transport::Ssh => {
                    let args = argv(&["-R", &format!("{}:127.0.0.1:{}", port, port)]);
                    let _ = sshx::master_ctl(d, "cancel", &args);
                }
                Transport::Adb => {
                    let _ = run_capture(
                        &crate::adbx::adb_argv(
                            d,
                            &["reverse", "--remove", &format!("tcp:{}", port)],
                        ),
                        &[],
                    );
                    let _ = run_capture(
                        &crate::adbx::adb_argv(d, &["shell", "settings delete global http_proxy"]),
                        &[],
                    );
                    let _ = run_capture(
                        &crate::adbx::adb_argv(d, &["shell", "settings put global http_proxy :0"]),
                        &[],
                    );
                }
                Transport::Serial => {}
            }
        }
    }
    st.drop_table(&table);
    st.save();
    // 如果没有别的设备在共享，顺手停掉 proxyd
    let others = State::load().doc.children("share");
    if others.is_empty() {
        proxyd::stop();
    }
    let _ = cfg;
    ok(&format!("{} 的共享上网已关闭", d.name));
    Ok(())
}

/// 当前哪些设备在借网（`fy share --json` / `fy ui` 用）。
pub fn active() -> Vec<(String, String)> {
    let st = State::load();
    st.doc
        .children("share")
        .into_iter()
        .map(|n| {
            let m = st.get_str(&format!("share.{}", n), "mode");
            (n, m)
        })
        .collect()
}
