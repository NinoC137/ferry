//! 设备档案（devices.toml）、设备指纹（facts/*.toml）、运行状态（state.toml）。

use crate::tomlite::{Doc, Val};
use crate::util::*;
use std::collections::BTreeMap;
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    Ssh,
    Adb,
    Serial,
}

impl Transport {
    pub fn as_str(&self) -> &'static str {
        match self {
            Transport::Ssh => "ssh",
            Transport::Adb => "adb",
            Transport::Serial => "serial",
        }
    }
    pub fn parse(s: &str) -> Option<Transport> {
        match s {
            "ssh" => Some(Transport::Ssh),
            "adb" => Some(Transport::Adb),
            "serial" => Some(Transport::Serial),
            _ => None,
        }
    }
}

/// 一台下位机的档案。字段尽量少而够用；缺省有合理默认。
#[derive(Debug, Clone)]
pub struct Device {
    pub name: String,
    pub transport: Transport,
    // ssh
    pub host: String,
    pub port: u16,
    pub user: String,
    pub password: Option<String>,
    pub key: Option<String>,
    pub legacy: bool, // 老 dropbear/旧算法兼容
    // adb
    pub adb_serial: Option<String>, // adb -s 目标（USB 序列号或 ip:port）
    // serial
    pub dev: Option<String>, // /dev/cu.* 或 /dev/ttyUSB*
    pub baud: u32,
    // 通用
    pub dest: String, // push 默认目标目录
    pub notes: String,
}

impl Device {
    pub fn new(name: &str, transport: Transport) -> Device {
        Device {
            name: name.to_string(),
            transport,
            host: String::new(),
            port: 22,
            user: "root".into(),
            password: None,
            key: None,
            legacy: false,
            adb_serial: None,
            dev: None,
            baud: 115200,
            dest: "/tmp".into(),
            notes: String::new(),
        }
    }

    /// 一行摘要，用于列表/选择器。
    pub fn endpoint(&self) -> String {
        match self.transport {
            Transport::Ssh => format!("{}@{}:{}", self.user, self.host, self.port),
            Transport::Adb => self.adb_serial.clone().unwrap_or_else(|| "(usb)".into()),
            Transport::Serial => format!("{}@{}", self.dev.clone().unwrap_or_default(), self.baud),
        }
    }
}

pub struct Config {
    pub devices: BTreeMap<String, Device>,
    doc: Doc,
}

/// `Config::resolve` 的结果。
pub enum Pick {
    Found(Device),
    Missing,
    Ambiguous(Vec<String>),
}

impl Config {
    pub fn load() -> Config {
        let _ = ensure_dir(&cfg_dir());
        let src = slurp(&devices_path());
        let doc = Doc::parse(&src).unwrap_or_else(|e| {
            warn(&format!("devices.toml 解析失败（按空档案继续）: {}", e));
            Doc::default()
        });
        let mut devices = BTreeMap::new();
        for name in doc.children("devices") {
            let t = format!("devices.{}", name);
            let gs = |k: &str| doc.get(&t, k).and_then(|v| v.as_str()).map(|s| s.to_string());
            let gi = |k: &str| doc.get(&t, k).and_then(|v| v.as_int());
            let gb = |k: &str| doc.get(&t, k).and_then(|v| v.as_bool());
            let transport = gs("transport").and_then(|s| Transport::parse(&s)).unwrap_or(Transport::Ssh);
            let mut d = Device::new(&name, transport);
            if let Some(v) = gs("host") { d.host = v; }
            if let Some(v) = gi("port") { d.port = v as u16; }
            if let Some(v) = gs("user") { d.user = v; }
            d.password = gs("password");
            d.key = gs("key");
            d.legacy = gb("legacy").unwrap_or(false);
            d.adb_serial = gs("adb_serial");
            d.dev = gs("dev");
            if let Some(v) = gi("baud") { d.baud = v as u32; }
            if let Some(v) = gs("dest") { d.dest = v; }
            if let Some(v) = gs("notes") { d.notes = v; }
            devices.insert(name, d);
        }
        Config { devices, doc }
    }

    pub fn save(&mut self) -> std::io::Result<()> {
        // 重建 devices.* 小节，保留其它小节
        let names: Vec<String> = self.doc.tables.keys().cloned().collect();
        for n in names {
            if n == "devices" || n.starts_with("devices.") {
                self.doc.tables.remove(&n);
            }
        }
        for (name, d) in &self.devices {
            let t = format!("devices.{}", name);
            self.doc.set(&t, "transport", Val::S(d.transport.as_str().into()));
            match d.transport {
                Transport::Ssh => {
                    self.doc.set(&t, "host", Val::S(d.host.clone()));
                    self.doc.set(&t, "port", Val::I(d.port as i64));
                    self.doc.set(&t, "user", Val::S(d.user.clone()));
                    if let Some(p) = &d.password { self.doc.set(&t, "password", Val::S(p.clone())); }
                    if let Some(k) = &d.key { self.doc.set(&t, "key", Val::S(k.clone())); }
                    if d.legacy { self.doc.set(&t, "legacy", Val::B(true)); }
                }
                Transport::Adb => {
                    if let Some(s) = &d.adb_serial { self.doc.set(&t, "adb_serial", Val::S(s.clone())); }
                }
                Transport::Serial => {}
            }
            // 串口信息对任何 transport 都可作为兜底通道保留
            if let Some(dev) = &d.dev { self.doc.set(&t, "dev", Val::S(dev.clone())); }
            if d.baud != 115200 { self.doc.set(&t, "baud", Val::I(d.baud as i64)); }
            if d.dest != "/tmp" { self.doc.set(&t, "dest", Val::S(d.dest.clone())); }
            if !d.notes.is_empty() { self.doc.set(&t, "notes", Val::S(d.notes.clone())); }
            // 串口/adb 设备也需要保存 user/password：fy up 串口自动登录要用；
            // 且 up 爬升后可能补全 host/port 作为 ssh 候选。
            if d.transport != Transport::Ssh {
                if d.user != "root" { self.doc.set(&t, "user", Val::S(d.user.clone())); }
                if let Some(p) = &d.password { self.doc.set(&t, "password", Val::S(p.clone())); }
                if !d.host.is_empty() {
                    self.doc.set(&t, "host", Val::S(d.host.clone()));
                    self.doc.set(&t, "port", Val::I(d.port as i64));
                }
            }
        }
        let _ = ensure_dir(&cfg_dir());
        let path = devices_path();
        std::fs::write(&path, self.doc.to_string())?;
        let _ = std::process::Command::new("chmod").arg("600").arg(&path).status();
        Ok(())
    }

    /// 解析设备名的三种结局。给 `--json` 用：歧义和查无此名要分开报码。
    pub fn resolve(&self, name: &str) -> Pick {
        if let Some(d) = self.devices.get(name) {
            return Pick::Found(d.clone());
        }
        let hits: Vec<&Device> = self.devices.values().filter(|d| d.name.starts_with(name)).collect();
        match hits.len() {
            0 => Pick::Missing,
            1 => Pick::Found(hits[0].clone()),
            _ => Pick::Ambiguous(hits.iter().map(|d| d.name.clone()).collect()),
        }
    }

    /// 找设备；支持唯一前缀匹配。
    pub fn find(&self, name: &str) -> Option<Device> {
        if let Some(d) = self.devices.get(name) {
            return Some(d.clone());
        }
        let hits: Vec<&Device> = self.devices.values().filter(|d| d.name.starts_with(name)).collect();
        if hits.len() == 1 {
            return Some(hits[0].clone());
        }
        None
    }

}

// ---------------- 设备指纹 facts ----------------

/// 连接时采集到的板子身份信息，用于"换了 IP 也认识你"。
#[derive(Debug, Clone, Default)]
pub struct Facts {
    pub machine_id: String,
    pub macs: Vec<String>,
    pub cpu_serial: String,
    pub hostname: String,
    pub kernel: String,
    pub arch: String,
    pub os: String, // linux / android
    pub last_ip: String,
    pub last_seen: i64,
}

pub fn facts_path(dev: &str) -> PathBuf {
    facts_dir().join(format!("{}.toml", dev))
}

pub fn facts_load(dev: &str) -> Facts {
    let doc = Doc::parse(&slurp(&facts_path(dev))).unwrap_or_default();
    let gs = |k: &str| doc.get("", k).and_then(|v| v.as_str()).map(|s| s.to_string()).unwrap_or_default();
    Facts {
        machine_id: gs("machine_id"),
        macs: doc.get("", "macs").and_then(|v| v.as_arr()).map(|a| a.to_vec()).unwrap_or_default(),
        cpu_serial: gs("cpu_serial"),
        hostname: gs("hostname"),
        kernel: gs("kernel"),
        arch: gs("arch"),
        os: gs("os"),
        last_ip: gs("last_ip"),
        last_seen: doc.get("", "last_seen").and_then(|v| v.as_int()).unwrap_or(0),
    }
}

pub fn facts_save(dev: &str, f: &Facts) {
    let _ = ensure_dir(&facts_dir());
    let mut doc = Doc::default();
    let mut set = |k: &str, v: &str| {
        if !v.is_empty() {
            doc.set("", k, Val::S(v.to_string()));
        }
    };
    set("machine_id", &f.machine_id);
    set("cpu_serial", &f.cpu_serial);
    set("hostname", &f.hostname);
    set("kernel", &f.kernel);
    set("arch", &f.arch);
    set("os", &f.os);
    set("last_ip", &f.last_ip);
    if !f.macs.is_empty() {
        doc.set("", "macs", Val::A(f.macs.clone()));
    }
    doc.set("", "last_seen", Val::I(f.last_seen));
    let _ = std::fs::write(facts_path(dev), doc.to_string());
}

pub fn all_facts() -> Vec<(String, Facts)> {
    let mut out = vec![];
    if let Ok(rd) = std::fs::read_dir(facts_dir()) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if let Some(stem) = name.strip_suffix(".toml") {
                out.push((stem.to_string(), facts_load(stem)));
            }
        }
    }
    out
}

// ---------------- 运行状态 state ----------------

#[derive(Debug, Clone)]
pub struct FwdEntry {
    pub id: String,
    pub dev: String,
    pub spec: String, // 规范化后的 L:.. / R:.. / D:..
    pub added: i64,
}

pub struct State {
    pub doc: Doc,
}

impl State {
    pub fn load() -> State {
        let _ = ensure_dir(&cfg_dir());
        State { doc: Doc::parse(&slurp(&state_path())).unwrap_or_default() }
    }
    pub fn save(&self) {
        let _ = std::fs::write(state_path(), self.doc.to_string());
    }

    pub fn forwards(&self) -> Vec<FwdEntry> {
        let mut out = vec![];
        for id in self.doc.children("fwd") {
            let t = format!("fwd.{}", id);
            out.push(FwdEntry {
                id: id.clone(),
                dev: self.doc.get(&t, "dev").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                spec: self.doc.get(&t, "spec").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                added: self.doc.get(&t, "added").and_then(|v| v.as_int()).unwrap_or(0),
            });
        }
        out
    }
    pub fn add_forward(&mut self, dev: &str, spec: &str) -> String {
        let mut n = 1;
        while self.doc.tables.contains_key(&format!("fwd.f{}", n)) {
            n += 1;
        }
        let id = format!("f{}", n);
        let t = format!("fwd.{}", id);
        self.doc.set(&t, "dev", Val::S(dev.into()));
        self.doc.set(&t, "spec", Val::S(spec.into()));
        self.doc.set(&t, "added", Val::I(now_epoch()));
        id
    }
    pub fn rm_forward(&mut self, id: &str) -> Option<FwdEntry> {
        let e = self.forwards().into_iter().find(|f| f.id == id)?;
        self.doc.tables.remove(&format!("fwd.{}", id));
        Some(e)
    }

    pub fn get_int(&self, table: &str, key: &str) -> i64 {
        self.doc.get(table, key).and_then(|v| v.as_int()).unwrap_or(0)
    }
    pub fn get_str(&self, table: &str, key: &str) -> String {
        self.doc.get(table, key).and_then(|v| v.as_str()).unwrap_or("").to_string()
    }
    pub fn set_int(&mut self, table: &str, key: &str, v: i64) {
        self.doc.set(table, key, Val::I(v));
    }
    pub fn set_str(&mut self, table: &str, key: &str, v: &str) {
        self.doc.set(table, key, Val::S(v.into()));
    }
    pub fn drop_table(&mut self, table: &str) {
        self.doc.tables.remove(table);
    }
}
