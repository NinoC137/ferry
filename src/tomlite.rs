//! tomlite — 够用的 TOML 子集解析/生成器（零依赖）。
//!
//! 支持：`[table.subtable]` 小节、`key = value`，value 为
//! 基本字符串 / 字面字符串 / 整数 / 布尔 / 字符串数组，`#` 注释。
//! ferry 的 devices.toml / state.toml / facts 都用它读写。

use std::collections::BTreeMap;
use std::fmt::Write as _;

#[derive(Debug, Clone, PartialEq)]
pub enum Val {
    S(String),
    I(i64),
    B(bool),
    A(Vec<String>),
}

impl Val {
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Val::S(s) => Some(s),
            _ => None,
        }
    }
    pub fn as_int(&self) -> Option<i64> {
        match self {
            Val::I(i) => Some(*i),
            _ => None,
        }
    }
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Val::B(b) => Some(*b),
            _ => None,
        }
    }
    pub fn as_arr(&self) -> Option<&[String]> {
        match self {
            Val::A(a) => Some(a),
            _ => None,
        }
    }
}

pub type Table = BTreeMap<String, Val>;

/// 整个文档：小节名（点分路径，顶层为 ""）→ 键值表。
#[derive(Debug, Default, Clone)]
pub struct Doc {
    pub tables: BTreeMap<String, Table>,
}

impl Doc {
    pub fn get(&self, table: &str, key: &str) -> Option<&Val> {
        self.tables.get(table).and_then(|t| t.get(key))
    }
    pub fn set(&mut self, table: &str, key: &str, v: Val) {
        self.tables
            .entry(table.to_string())
            .or_default()
            .insert(key.to_string(), v);
    }
    /// 列出某前缀下的直接子小节名，如 prefix="devices" → ["rk3588", "cam1"]。
    pub fn children(&self, prefix: &str) -> Vec<String> {
        let p = format!("{prefix}.");
        let mut out = vec![];
        for name in self.tables.keys() {
            if let Some(rest) = name.strip_prefix(&p) {
                if !rest.is_empty() && !rest.contains('.') && !out.contains(&rest.to_string()) {
                    out.push(rest.to_string());
                }
            }
        }
        out
    }

    pub fn parse(src: &str) -> Result<Doc, String> {
        let mut doc = Doc::default();
        let mut cur = String::new(); // 当前小节
        doc.tables.entry(String::new()).or_default();
        for (ln, raw) in src.lines().enumerate() {
            let line = strip_comment(raw).trim().to_string();
            if line.is_empty() {
                continue;
            }
            if let Some(name) = line.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
                let name = name.trim();
                if name.is_empty()
                    || !name
                        .chars()
                        .all(|c| c.is_alphanumeric() || "._-".contains(c))
                {
                    return Err(format!("line {}: bad table name [{}]", ln + 1, name));
                }
                cur = name.to_string();
                doc.tables.entry(cur.clone()).or_default();
                continue;
            }
            let eq = line
                .find('=')
                .ok_or_else(|| format!("line {}: expected key = value", ln + 1))?;
            let key = line[..eq].trim().to_string();
            if key.is_empty() || !key.chars().all(|c| c.is_alphanumeric() || "_-".contains(c)) {
                return Err(format!("line {}: bad key '{}'", ln + 1, key));
            }
            let vs = line[eq + 1..].trim();
            let val = parse_val(vs).map_err(|e| format!("line {}: {}", ln + 1, e))?;
            doc.tables.entry(cur.clone()).or_default().insert(key, val);
        }
        Ok(doc)
    }

    pub fn to_string(&self) -> String {
        let mut out = String::new();
        // 顶层键先输出
        if let Some(t) = self.tables.get("") {
            for (k, v) in t {
                let _ = writeln!(out, "{} = {}", k, emit_val(v));
            }
            if !t.is_empty() {
                out.push('\n');
            }
        }
        for (name, t) in &self.tables {
            if name.is_empty() {
                continue;
            }
            let _ = writeln!(out, "[{}]", name);
            for (k, v) in t {
                let _ = writeln!(out, "{} = {}", k, emit_val(v));
            }
            out.push('\n');
        }
        out
    }
}

/// 去掉行内注释（注意不能切掉字符串里的 #）。
fn strip_comment(line: &str) -> &str {
    let mut in_s = false;
    let mut quote = ' ';
    let mut prev_esc = false;
    for (i, c) in line.char_indices() {
        if in_s {
            if prev_esc {
                prev_esc = false;
            } else if c == '\\' && quote == '"' {
                prev_esc = true;
            } else if c == quote {
                in_s = false;
            }
        } else if c == '"' || c == '\'' {
            in_s = true;
            quote = c;
        } else if c == '#' {
            return &line[..i];
        }
    }
    line
}

fn parse_val(s: &str) -> Result<Val, String> {
    if s.is_empty() {
        return Err("empty value".into());
    }
    if s == "true" {
        return Ok(Val::B(true));
    }
    if s == "false" {
        return Ok(Val::B(false));
    }
    if s.starts_with('"') {
        return Ok(Val::S(parse_basic_string(s)?.0));
    }
    if s.starts_with('\'') {
        let inner = s
            .strip_prefix('\'')
            .and_then(|x| x.strip_suffix('\''))
            .ok_or("unterminated 'string'")?;
        return Ok(Val::S(inner.to_string()));
    }
    if s.starts_with('[') {
        let inner = s
            .strip_prefix('[')
            .and_then(|x| x.strip_suffix(']'))
            .ok_or("unterminated array")?;
        let mut items = vec![];
        let mut rest = inner.trim();
        while !rest.is_empty() {
            if !rest.starts_with('"') {
                return Err("array supports only \"strings\"".into());
            }
            let (item, used) = parse_basic_string(rest)?;
            items.push(item);
            rest = rest[used..].trim_start();
            if let Some(r) = rest.strip_prefix(',') {
                rest = r.trim_start();
            } else if !rest.is_empty() {
                return Err("expected ',' in array".into());
            }
        }
        return Ok(Val::A(items));
    }
    if let Ok(i) = s.replace('_', "").parse::<i64>() {
        return Ok(Val::I(i));
    }
    Err(format!("cannot parse value: {}", s))
}

/// 解析基本字符串，返回 (内容, 消耗的字节数含引号)。
fn parse_basic_string(s: &str) -> Result<(String, usize), String> {
    debug_assert!(s.starts_with('"'));
    let mut out = String::new();
    let mut esc = false;
    for (i, c) in s.char_indices().skip(1) {
        if esc {
            out.push(match c {
                'n' => '\n',
                't' => '\t',
                'r' => '\r',
                '\\' => '\\',
                '"' => '"',
                other => return Err(format!("bad escape \\{}", other)),
            });
            esc = false;
        } else if c == '\\' {
            esc = true;
        } else if c == '"' {
            return Ok((out, i + 1));
        } else {
            out.push(c);
        }
    }
    Err("unterminated \"string\"".into())
}

fn emit_val(v: &Val) -> String {
    match v {
        Val::S(s) => emit_str(s),
        Val::I(i) => i.to_string(),
        Val::B(b) => b.to_string(),
        Val::A(a) => {
            let items: Vec<String> = a.iter().map(|s| emit_str(s)).collect();
            format!("[{}]", items.join(", "))
        }
    }
}

fn emit_str(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let src = r#"
# 注释
top = "level"

[devices.rk3588]  # 板子
host = "192.168.55.2"
port = 22
legacy = true
tags = ["lab", "a#b"]
note = "he said \"hi\" # not a comment"
baud = 1_500_000
"#;
        let d = Doc::parse(src).unwrap();
        assert_eq!(d.get("", "top").unwrap().as_str().unwrap(), "level");
        assert_eq!(
            d.get("devices.rk3588", "port").unwrap().as_int().unwrap(),
            22
        );
        assert_eq!(
            d.get("devices.rk3588", "legacy")
                .unwrap()
                .as_bool()
                .unwrap(),
            true
        );
        assert_eq!(
            d.get("devices.rk3588", "baud").unwrap().as_int().unwrap(),
            1_500_000
        );
        assert_eq!(
            d.get("devices.rk3588", "tags").unwrap().as_arr().unwrap(),
            &["lab".to_string(), "a#b".to_string()]
        );
        assert_eq!(
            d.get("devices.rk3588", "note").unwrap().as_str().unwrap(),
            "he said \"hi\" # not a comment"
        );
        assert_eq!(d.children("devices"), vec!["rk3588".to_string()]);
        // 重新序列化再解析应一致
        let d2 = Doc::parse(&d.to_string()).unwrap();
        assert_eq!(
            d2.get("devices.rk3588", "note"),
            d.get("devices.rk3588", "note")
        );
    }
}
