//! 零依赖 JSON 输出通道 —— ferry 面向 **AI agent / 脚本** 的机器接口。
//!
//! ## 契约（改了要同步改 README，agent 依赖它）
//!
//! - 加 `--json` 后，**stdout 有且只有一份 JSON 文档**；一切人类可读的过程信息
//!   （`→ ssh ...`、进度条、提示）全部走 stderr。管道里 `fy --json ls | jq` 永远干净。
//! - `--json` 隐含 **非交互**：不会弹设备选择器、不会问 y/n。需要人拍板的地方直接
//!   以 `NEED_INPUT` 失败，并在 `hint` 字段里说清楚该补哪个参数。
//! - 成功: `{"ok":true,"cmd":"push","...":...}`
//! - 失败: `{"ok":false,"cmd":"push","code":14,"error":"...","hint":"..."}`
//! - **`ok` 字段是唯一权威判据**。退出码同样稳定（见 `code` 模块），但 `fy sh` /
//!   `fy run` 会**透传远端命令的退出码**，理论上可能和 ferry 自己的码撞车 ——
//!   撞车时以 `ok` 为准。
//!
//! 环境变量 `FERRY_JSON=1` 等价于全局加 `--json`（方便 agent 一次性设好）。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

pub static JSON: AtomicBool = AtomicBool::new(false);
pub static NONINTERACTIVE: AtomicBool = AtomicBool::new(false);
/// 一次进程只允许吐一份 JSON 文档，防止某条路径重复 emit 把 stdout 弄脏。
static EMITTED: AtomicBool = AtomicBool::new(false);
static CMD_NAME: Mutex<String> = Mutex::new(String::new());

pub fn json_mode() -> bool {
    JSON.load(Ordering::Relaxed)
}

/// 允许向用户提问吗？（`--json` / `-y` / 非 tty 场景下为 false）
pub fn interactive() -> bool {
    !NONINTERACTIVE.load(Ordering::Relaxed)
}

pub fn set_json(on: bool) {
    JSON.store(on, Ordering::Relaxed);
    if on {
        NONINTERACTIVE.store(true, Ordering::Relaxed);
    }
}

pub fn set_noninteractive(on: bool) {
    NONINTERACTIVE.store(on, Ordering::Relaxed);
}

/// 记录当前子命令名，出现在每份 JSON 的 `cmd` 字段里。
pub fn set_cmd(name: &str) {
    if let Ok(mut g) = CMD_NAME.lock() {
        *g = name.to_string();
    }
}

fn cmd_name() -> String {
    CMD_NAME.lock().map(|g| g.clone()).unwrap_or_default()
}

// ---------------- 稳定退出码 ----------------

/// ferry 自身的退出码。**只增不改**，agent 会硬编码这些数字。
pub mod code {
    /// 一切正常。
    pub const OK: i32 = 0;
    /// 兜底失败（没有更精确的分类）。
    pub const FAIL: i32 = 1;
    /// 命令行用法错误：缺参数、参数看不懂。
    pub const USAGE: i32 = 2;
    /// 没有这台设备（档案里查无此名）。
    pub const NO_DEVICE: i32 = 10;
    /// 设备名有歧义（前缀匹配到多台）。
    pub const AMBIGUOUS: i32 = 11;
    /// 设备不可达（连不上 / 不在线 / 串口没插）。
    pub const UNREACHABLE: i32 = 12;
    /// 这个通道不支持该操作（比如串口做端口转发）。
    pub const UNSUPPORTED: i32 = 13;
    /// 传输失败。
    pub const TRANSFER: i32 = 14;
    /// 传完校验不一致（数据损坏）。
    pub const CHECKSUM: i32 = 15;
    /// 超时。
    pub const TIMEOUT: i32 = 16;
    /// 主机缺少外部依赖（ssh/adb/rsync 没装）。
    pub const MISSING_DEP: i32 = 17;
    /// 配置或运行态文件有问题。
    pub const CONFIG: i32 = 18;
    /// 需要人拍板但当前是非交互模式（`--json` 下最常见）。
    pub const NEED_INPUT: i32 = 19;

    pub fn name(c: i32) -> &'static str {
        match c {
            OK => "ok",
            FAIL => "fail",
            USAGE => "usage",
            NO_DEVICE => "no_device",
            AMBIGUOUS => "ambiguous_device",
            UNREACHABLE => "unreachable",
            UNSUPPORTED => "unsupported_transport",
            TRANSFER => "transfer_failed",
            CHECKSUM => "checksum_mismatch",
            TIMEOUT => "timeout",
            MISSING_DEP => "missing_dependency",
            CONFIG => "config_error",
            NEED_INPUT => "need_input",
            _ => "remote_exit",
        }
    }
}

// ---------------- JSON 值 ----------------

#[derive(Debug, Clone)]
pub enum J {
    Null,
    B(bool),
    I(i64),
    F(f64),
    S(String),
    A(Vec<J>),
    O(Vec<(String, J)>),
}

impl J {
    pub fn s(v: impl Into<String>) -> J {
        J::S(v.into())
    }
    pub fn i(v: impl Into<i64>) -> J {
        J::I(v.into())
    }
    pub fn f(v: f64) -> J {
        if v.is_finite() {
            J::F(v)
        } else {
            J::Null
        }
    }
    pub fn b(v: bool) -> J {
        J::B(v)
    }
    pub fn arr(v: Vec<J>) -> J {
        J::A(v)
    }
    pub fn obj(v: Vec<(&str, J)>) -> J {
        J::O(v.into_iter().map(|(k, x)| (k.to_string(), x)).collect())
    }
    pub fn strs(v: &[String]) -> J {
        J::A(v.iter().map(|s| J::S(s.clone())).collect())
    }

    fn write(&self, out: &mut String, indent: usize) {
        let pad = |n: usize| "  ".repeat(n);
        match self {
            J::Null => out.push_str("null"),
            J::B(b) => out.push_str(if *b { "true" } else { "false" }),
            J::I(i) => out.push_str(&i.to_string()),
            J::F(f) => {
                // 保证输出是合法 JSON 数字（不要 1e0 之外的怪东西，也不要 NaN）
                let s = format!("{}", (f * 1000.0).round() / 1000.0);
                out.push_str(if s.contains(['n', 'i']) { "null" } else { &s });
            }
            J::S(s) => out.push_str(&esc(s)),
            J::A(a) => {
                if a.is_empty() {
                    out.push_str("[]");
                    return;
                }
                out.push_str("[\n");
                for (i, v) in a.iter().enumerate() {
                    out.push_str(&pad(indent + 1));
                    v.write(out, indent + 1);
                    if i + 1 < a.len() {
                        out.push(',');
                    }
                    out.push('\n');
                }
                out.push_str(&pad(indent));
                out.push(']');
            }
            J::O(o) => {
                if o.is_empty() {
                    out.push_str("{}");
                    return;
                }
                out.push_str("{\n");
                for (i, (k, v)) in o.iter().enumerate() {
                    out.push_str(&pad(indent + 1));
                    out.push_str(&esc(k));
                    out.push_str(": ");
                    v.write(out, indent + 1);
                    if i + 1 < o.len() {
                        out.push(',');
                    }
                    out.push('\n');
                }
                out.push_str(&pad(indent));
                out.push('}');
            }
        }
    }

    pub fn dump(&self) -> String {
        let mut s = String::new();
        self.write(&mut s, 0);
        s
    }
}

/// JSON 字符串转义（含控制字符 \u 兜底）。
pub fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

// ---------------- 输出 ----------------

fn emit_raw(mut fields: Vec<(String, J)>) {
    if EMITTED.swap(true, Ordering::Relaxed) {
        return; // 已经吐过了，绝不让 stdout 出现第二份文档
    }
    let mut head = vec![];
    std::mem::swap(&mut head, &mut fields);
    println!("{}", J::O(head).dump());
    use std::io::Write;
    let _ = std::io::stdout().flush();
}

/// 成功输出。非 JSON 模式下什么都不做（人类可读的输出由各命令自己打）。
/// 返回值就是退出码 0，方便 `return emit_ok(...)`。
pub fn emit_ok(fields: Vec<(&str, J)>) -> i32 {
    if json_mode() {
        let mut all: Vec<(String, J)> = vec![
            ("ok".to_string(), J::B(true)),
            ("cmd".to_string(), J::S(cmd_name())),
        ];
        all.extend(fields.into_iter().map(|(k, v)| (k.to_string(), v)));
        emit_raw(all);
    }
    code::OK
}

/// 失败输出：JSON 模式吐结构化错误，否则走 `util::err`。返回传入的退出码。
pub fn fail(c: i32, msg: &str) -> i32 {
    fail_hint(c, msg, None)
}

/// 带 `hint` 的失败：hint 是给 agent 的**下一步动作建议**，比如该补哪个参数。
pub fn fail_hint(c: i32, msg: &str, hint: Option<&str>) -> i32 {
    if json_mode() {
        let mut all: Vec<(String, J)> = vec![
            ("ok".to_string(), J::B(false)),
            ("cmd".to_string(), J::S(cmd_name())),
            ("code".to_string(), J::I(c as i64)),
            ("error_kind".to_string(), J::s(code::name(c))),
            ("error".to_string(), J::S(msg.to_string())),
        ];
        if let Some(h) = hint {
            all.push(("hint".to_string(), J::S(h.to_string())));
        }
        emit_raw(all);
    } else {
        crate::util::err(msg);
        if let Some(h) = hint {
            crate::util::info(h);
        }
    }
    c
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_and_shapes() {
        assert_eq!(esc("a\"b\\c\nd\te"), "\"a\\\"b\\\\c\\nd\\te\"");
        assert_eq!(esc("\u{1}"), "\"\\u0001\"");
        assert_eq!(J::A(vec![]).dump(), "[]");
        assert_eq!(J::O(vec![]).dump(), "{}");
        let v = J::obj(vec![
            ("n", J::i(3i64)),
            ("s", J::s("x")),
            ("a", J::arr(vec![J::b(true), J::Null])),
        ]);
        let d = v.dump();
        assert!(
            d.contains("\"n\": 3") && d.contains("\"s\": \"x\"") && d.contains("true"),
            "{}",
            d
        );
    }

    #[test]
    fn floats_never_emit_nan() {
        assert_eq!(J::f(f64::NAN).dump(), "null");
        assert_eq!(J::f(f64::INFINITY).dump(), "null");
        assert_eq!(J::f(1.23456).dump(), "1.235");
    }

    #[test]
    fn cjk_survives_roundtrip() {
        // 中文不转义（合法 UTF-8 JSON），agent 侧直接可读
        assert_eq!(esc("板子"), "\"板子\"");
    }
}
