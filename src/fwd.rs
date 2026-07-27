//! 端口转发管理器：ssh 隧道挂在 ControlMaster 上动态增删（-O forward/cancel），
//! adb 用 forward/reverse。统一 spec 语法，统一 `fy fwd ls` 视图。
//!
//! spec 语法（一看就懂，不用背 ssh 手册）：
//!   8080            本机 8080 → 板子 127.0.0.1:8080
//!   8080:80         本机 8080 → 板子 127.0.0.1:80
//!   8080:ip:80      本机 8080 → 板子可达的 ip:80
//!   R:9000:8000     板子 9000 → 本机 127.0.0.1:8000（反向）
//!   D:1080          SOCKS5 动态代理（仅 ssh）

use crate::adbx;
use crate::config::{Config, Device, State, Transport};
use crate::sshx;
use crate::util::*;
use crate::watchd;

#[derive(Debug, Clone, PartialEq)]
pub enum Spec {
    L { lp: u16, rh: String, rp: u16 },
    R { rp: u16, lh: String, lp: u16 },
    D { lp: u16 },
}

impl Spec {
    pub fn parse(s: &str) -> Result<Spec, String> {
        let parts: Vec<&str> = s.split(':').collect();
        let bad = || format!("看不懂的转发规则 '{}'（用法见 fy help fwd）", s);
        let port = |x: &str| x.parse::<u16>().map_err(|_| bad());
        match parts.as_slice() {
            [p] => {
                let n = port(p)?;
                Ok(Spec::L {
                    lp: n,
                    rh: "127.0.0.1".into(),
                    rp: n,
                })
            }
            ["D" | "d", p] => Ok(Spec::D { lp: port(p)? }),
            ["R" | "r", rp, lp] => Ok(Spec::R {
                rp: port(rp)?,
                lh: "127.0.0.1".into(),
                lp: port(lp)?,
            }),
            ["R" | "r", rp, lh, lp] => Ok(Spec::R {
                rp: port(rp)?,
                lh: lh.to_string(),
                lp: port(lp)?,
            }),
            ["L" | "l", lp, rh, rp] => Ok(Spec::L {
                lp: port(lp)?,
                rh: rh.to_string(),
                rp: port(rp)?,
            }),
            [lp, rp] => Ok(Spec::L {
                lp: port(lp)?,
                rh: "127.0.0.1".into(),
                rp: port(rp)?,
            }),
            [lp, rh, rp] => Ok(Spec::L {
                lp: port(lp)?,
                rh: rh.to_string(),
                rp: port(rp)?,
            }),
            _ => Err(bad()),
        }
    }

    pub fn canon(&self) -> String {
        match self {
            Spec::L { lp, rh, rp } => format!("L:{}:{}:{}", lp, rh, rp),
            Spec::R { rp, lh, lp } => format!("R:{}:{}:{}", rp, lh, lp),
            Spec::D { lp } => format!("D:{}", lp),
        }
    }

    pub fn human(&self) -> String {
        match self {
            Spec::L { lp, rh, rp } => format!("本机:{} → 板:{}:{}", lp, rh, rp),
            Spec::R { rp, lh, lp } => format!("板:{} → 本机:{}:{}", rp, lh, lp),
            Spec::D { lp } => format!("SOCKS5 @本机:{}", lp),
        }
    }

    pub fn ssh_args(&self) -> Vec<String> {
        match self {
            Spec::L { lp, rh, rp } => argv(&["-L", &format!("{}:{}:{}", lp, rh, rp)]),
            Spec::R { rp, lh, lp } => argv(&["-R", &format!("{}:{}:{}", rp, lh, lp)]),
            Spec::D { lp } => argv(&["-D", &lp.to_string()]),
        }
    }
}

pub fn add(cfg: &Config, dev: &Device, spec_str: &str) -> Result<String, String> {
    add_opts(cfg, dev, spec_str, true)
}

/// `watch=false` 时不自动拉起保活守护进程（`fy fwd ... --no-watch`）。
pub fn add_opts(cfg: &Config, dev: &Device, spec_str: &str, watch: bool) -> Result<String, String> {
    let spec = Spec::parse(spec_str)?;
    match dev.transport {
        Transport::Ssh => {
            sshx::ensure_master(dev).map_err(|e| e.to_string())?;
            let out =
                sshx::master_ctl(dev, "forward", &spec.ssh_args()).map_err(|e| e.to_string())?;
            if out.status != 0 && !dry() {
                return Err(format!("转发建立失败: {}", out.stderr.trim()));
            }
            let mut st = State::load();
            let id = st.add_forward(&dev.name, &spec.canon());
            st.save();
            ok(&format!("[{}] {}  ({})", id, spec.human(), dev.name));
            if watch {
                // 转发挂在 ssh master 上，master 一断全废；顺手把保活开起来
                watchd::autostart_if_useful();
            }
            let _ = cfg;
            return Ok(id);
        }
        Transport::Adb => match &spec {
            Spec::L { lp, rh, rp } => {
                if rh != "127.0.0.1" {
                    return Err("adb forward 只支持板内 127.0.0.1 目标".into());
                }
                let o = run_capture(
                    &adbx::adb_argv(
                        dev,
                        &["forward", &format!("tcp:{}", lp), &format!("tcp:{}", rp)],
                    ),
                    &[],
                )
                .map_err(|e| e.to_string())?;
                if o.status != 0 && !dry() {
                    return Err(format!("adb forward 失败: {}", o.stderr.trim()));
                }
                ok(&format!("{}  ({})", spec.human(), dev.name));
            }
            Spec::R { rp, lh, lp } => {
                if lh != "127.0.0.1" {
                    return Err("adb reverse 只支持本机 127.0.0.1 目标".into());
                }
                let o = run_capture(
                    &adbx::adb_argv(
                        dev,
                        &["reverse", &format!("tcp:{}", rp), &format!("tcp:{}", lp)],
                    ),
                    &[],
                )
                .map_err(|e| e.to_string())?;
                if o.status != 0 && !dry() {
                    return Err(format!("adb reverse 失败: {}", o.stderr.trim()));
                }
                ok(&format!("{}  ({})", spec.human(), dev.name));
            }
            Spec::D { .. } => return Err("adb 不支持 SOCKS 动态代理（换 ssh 通道）".into()),
        },
        Transport::Serial => return Err("串口设备没有网络转发；先 `fy up` 爬升到 ssh".into()),
    }
    let _ = cfg;
    Ok(String::new())
}

/// 一条转发在视图里的样子（表格和 JSON 共用）。
#[derive(Debug, Clone)]
pub struct FwdView {
    pub id: String,
    pub dev: String,
    pub channel: String,
    pub spec: String,
    pub human: String,
    pub alive: bool,
    pub added: i64,
}

/// 汇总当前所有转发：ssh 侧读 state 并校验 master 存活，adb 侧实时查询。
pub fn collect(cfg: &Config) -> Vec<FwdView> {
    let mut out = vec![];
    let st = State::load();
    // 同一台设备只探一次 master，别为每条转发都开一次 ssh
    let mut alive_cache: std::collections::HashMap<String, bool> = Default::default();
    for f in st.forwards() {
        let alive = *alive_cache.entry(f.dev.clone()).or_insert_with(|| {
            cfg.devices
                .get(&f.dev)
                .map(|d| {
                    sshx::master_ctl(d, "check", &[])
                        .map(|o| o.status == 0)
                        .unwrap_or(false)
                })
                .unwrap_or(false)
        });
        let human = Spec::parse(&f.spec)
            .map(|s| s.human())
            .unwrap_or_else(|_| f.spec.clone());
        out.push(FwdView {
            id: f.id.clone(),
            dev: f.dev.clone(),
            channel: "ssh".into(),
            spec: f.spec.clone(),
            human,
            alive,
            added: f.added,
        });
    }
    for (kind, args) in [
        ("forward", ["forward", "--list"]),
        ("reverse", ["reverse", "--list"]),
    ] {
        if let Ok(o) = run_capture(&argv(&["adb", args[0], args[1]]), &[]) {
            for line in o.stdout.lines() {
                let toks: Vec<&str> = line.split_whitespace().collect();
                if toks.len() >= 3 {
                    let human = if kind == "forward" {
                        format!(
                            "本机:{} → 板:{}",
                            toks[1].trim_start_matches("tcp:"),
                            toks[2].trim_start_matches("tcp:")
                        )
                    } else {
                        format!(
                            "板:{} → 本机:{}",
                            toks[1].trim_start_matches("tcp:"),
                            toks[2].trim_start_matches("tcp:")
                        )
                    };
                    let dev = cfg
                        .devices
                        .values()
                        .find(|d| d.adb_serial.as_deref() == Some(toks[0]))
                        .map(|d| d.name.clone())
                        .unwrap_or_else(|| toks[0].to_string());
                    out.push(FwdView {
                        id: "-".into(),
                        dev,
                        channel: format!("adb {}", kind),
                        spec: format!("{} {}", toks[1], toks[2]),
                        human,
                        alive: true,
                        added: 0,
                    });
                }
            }
        }
    }
    out
}

pub fn list(cfg: &Config) {
    let views = collect(cfg);
    if views.is_empty() {
        info("当前没有任何转发。加一个: fy fwd <设备> 8080");
        return;
    }
    let watching = watchd::is_running();
    let rows: Vec<Vec<String>> = views
        .iter()
        .map(|v| {
            vec![
                v.id.clone(),
                v.dev.clone(),
                v.channel.clone(),
                v.human.clone(),
                if v.alive {
                    green("活")
                } else if watching {
                    yellow("断(保活正在重连)")
                } else {
                    red("断(fy watch start 可自动重连)")
                },
                if v.added > 0 {
                    human_ago(v.added)
                } else {
                    String::new()
                },
            ]
        })
        .collect();
    print_table(&["ID", "设备", "通道", "转发", "状态", "建立"], &rows);
    if !watching && views.iter().any(|v| !v.alive) {
        info("有转发掉线了。`fy watch start` 之后会自动重连并重放转发。");
    }
}

pub fn remove(cfg: &Config, id_or_all: &str) {
    let mut st = State::load();
    let targets: Vec<String> = if id_or_all == "all" {
        st.forwards().iter().map(|f| f.id.clone()).collect()
    } else {
        vec![id_or_all.to_string()]
    };
    for id in targets {
        match st.rm_forward(&id) {
            Some(f) => {
                if let (Some(d), Ok(spec)) = (cfg.devices.get(&f.dev), Spec::parse(&f.spec)) {
                    let _ = sshx::master_ctl(d, "cancel", &spec.ssh_args());
                }
                ok(&format!("已移除 [{}] {}", id, f.spec));
            }
            None => warn(&format!(
                "没有 [{}] 这条转发（adb 的转发用 adb forward --remove-all 清）",
                id
            )),
        }
    }
    st.save();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_grammar() {
        assert_eq!(
            Spec::parse("8080").unwrap(),
            Spec::L {
                lp: 8080,
                rh: "127.0.0.1".into(),
                rp: 8080
            }
        );
        assert_eq!(
            Spec::parse("8080:80").unwrap(),
            Spec::L {
                lp: 8080,
                rh: "127.0.0.1".into(),
                rp: 80
            }
        );
        assert_eq!(
            Spec::parse("8080:10.0.0.9:80").unwrap(),
            Spec::L {
                lp: 8080,
                rh: "10.0.0.9".into(),
                rp: 80
            }
        );
        assert_eq!(
            Spec::parse("R:9000:8000").unwrap(),
            Spec::R {
                rp: 9000,
                lh: "127.0.0.1".into(),
                lp: 8000
            }
        );
        assert_eq!(Spec::parse("D:1080").unwrap(), Spec::D { lp: 1080 });
        assert!(Spec::parse("不是端口").is_err());
        assert!(Spec::parse("70000").is_err(), "端口超出 u16 要报错");
    }

    #[test]
    fn canon_roundtrips() {
        for s in ["8080", "8080:80", "R:9000:8000", "D:1080", "L:1:2.2.2.2:3"] {
            let a = Spec::parse(s).unwrap();
            let b = Spec::parse(&a.canon()).unwrap();
            assert_eq!(a, b, "{} 规范化后解析不回来", s);
        }
    }

    #[test]
    fn ssh_args_match_openssh_syntax() {
        assert_eq!(
            Spec::parse("8080:10.0.0.9:80").unwrap().ssh_args(),
            vec!["-L".to_string(), "8080:10.0.0.9:80".to_string()]
        );
        assert_eq!(
            Spec::parse("R:9000:8000").unwrap().ssh_args(),
            vec!["-R".to_string(), "9000:127.0.0.1:8000".to_string()]
        );
        assert_eq!(
            Spec::parse("D:1080").unwrap().ssh_args(),
            vec!["-D".to_string(), "1080".to_string()]
        );
    }
}
