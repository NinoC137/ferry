//! Human-oriented peripheral brief rendered from the stable hwprobe/v1 JSON.
//! The target only collects facts; this module owns host-side interpretation.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

#[derive(Debug, Clone)]
enum Value {
    Null,
    Bool(bool),
    Number(String),
    String(String),
    Array(Vec<Value>),
    Object(BTreeMap<String, Value>),
}

struct Parser<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Parser<'a> {
    fn parse(input: &'a str) -> Result<Value, String> {
        let mut p = Parser {
            bytes: input.as_bytes(),
            pos: 0,
        };
        let value = p.value()?;
        p.ws();
        if p.pos == p.bytes.len() {
            Ok(value)
        } else {
            Err(format!("JSON 在字节 {} 后仍有未解析内容", p.pos))
        }
    }

    fn ws(&mut self) {
        while self
            .bytes
            .get(self.pos)
            .is_some_and(|c| matches!(c, b' ' | b'\t' | b'\r' | b'\n'))
        {
            self.pos += 1;
        }
    }

    fn take(&mut self, expected: u8) -> Result<(), String> {
        self.ws();
        match self.bytes.get(self.pos) {
            Some(c) if *c == expected => {
                self.pos += 1;
                Ok(())
            }
            _ => Err(format!(
                "JSON 字节 {} 期待字符 {}",
                self.pos, expected as char
            )),
        }
    }

    fn value(&mut self) -> Result<Value, String> {
        self.ws();
        match self.bytes.get(self.pos).copied() {
            Some(b'{') => self.object(),
            Some(b'[') => self.array(),
            Some(b'"') => self.string().map(Value::String),
            Some(b't') => self.literal(b"true", Value::Bool(true)),
            Some(b'f') => self.literal(b"false", Value::Bool(false)),
            Some(b'n') => self.literal(b"null", Value::Null),
            Some(b'-' | b'0'..=b'9') => self.number(),
            _ => Err(format!("JSON 字节 {} 不是合法值", self.pos)),
        }
    }

    fn literal(&mut self, expected: &[u8], value: Value) -> Result<Value, String> {
        if self.bytes.get(self.pos..self.pos + expected.len()) == Some(expected) {
            self.pos += expected.len();
            Ok(value)
        } else {
            Err(format!("JSON 字节 {} literal 非法", self.pos))
        }
    }

    fn object(&mut self) -> Result<Value, String> {
        self.take(b'{')?;
        let mut out = BTreeMap::new();
        self.ws();
        if self.bytes.get(self.pos) == Some(&b'}') {
            self.pos += 1;
            return Ok(Value::Object(out));
        }
        loop {
            let key = self.string()?;
            self.take(b':')?;
            let value = self.value()?;
            out.insert(key, value);
            self.ws();
            match self.bytes.get(self.pos) {
                Some(b',') => self.pos += 1,
                Some(b'}') => {
                    self.pos += 1;
                    return Ok(Value::Object(out));
                }
                _ => return Err(format!("JSON object 字节 {} 缺少逗号或右花括号", self.pos)),
            }
        }
    }

    fn array(&mut self) -> Result<Value, String> {
        self.take(b'[')?;
        let mut out = vec![];
        self.ws();
        if self.bytes.get(self.pos) == Some(&b']') {
            self.pos += 1;
            return Ok(Value::Array(out));
        }
        loop {
            out.push(self.value()?);
            self.ws();
            match self.bytes.get(self.pos) {
                Some(b',') => self.pos += 1,
                Some(b']') => {
                    self.pos += 1;
                    return Ok(Value::Array(out));
                }
                _ => return Err(format!("JSON array 字节 {} 缺少逗号或右方括号", self.pos)),
            }
        }
    }

    fn number(&mut self) -> Result<Value, String> {
        let start = self.pos;
        while self
            .bytes
            .get(self.pos)
            .is_some_and(|c| matches!(c, b'-' | b'+' | b'.' | b'e' | b'E' | b'0'..=b'9'))
        {
            self.pos += 1;
        }
        std::str::from_utf8(&self.bytes[start..self.pos])
            .map(|s| Value::Number(s.to_string()))
            .map_err(|_| "JSON 数字不是 UTF-8".into())
    }

    fn string(&mut self) -> Result<String, String> {
        self.take(b'"')?;
        let mut out = String::new();
        while let Some(&c) = self.bytes.get(self.pos) {
            self.pos += 1;
            match c {
                b'"' => return Ok(out),
                b'\\' => {
                    let escaped = *self
                        .bytes
                        .get(self.pos)
                        .ok_or_else(|| "JSON 字符串以反斜杠结束".to_string())?;
                    self.pos += 1;
                    match escaped {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{0008}'),
                        b'f' => out.push('\u{000c}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => {
                            let end = self.pos + 4;
                            let hex = std::str::from_utf8(
                                self.bytes
                                    .get(self.pos..end)
                                    .ok_or_else(|| "JSON unicode 转义不完整".to_string())?,
                            )
                            .map_err(|_| "JSON unicode 转义非法".to_string())?;
                            let code = u32::from_str_radix(hex, 16)
                                .map_err(|_| "JSON unicode 转义非十六进制".to_string())?;
                            out.push(char::from_u32(code).unwrap_or('\u{fffd}'));
                            self.pos = end;
                        }
                        _ => return Err("JSON 字符串转义非法".into()),
                    }
                }
                c if c < 0x20 => return Err("JSON 字符串含未转义控制字符".into()),
                _ => out.push(c as char),
            }
        }
        Err("JSON 字符串未闭合".into())
    }
}

fn object(v: &Value) -> Option<&BTreeMap<String, Value>> {
    match v {
        Value::Object(v) => Some(v),
        _ => None,
    }
}

fn array(v: Option<&Value>) -> &[Value] {
    match v {
        Some(Value::Array(v)) => v,
        _ => &[],
    }
}

fn field<'a>(v: &'a Value, key: &str) -> Option<&'a Value> {
    object(v)?.get(key)
}

fn text(v: Option<&Value>) -> Option<&str> {
    match v {
        Some(Value::String(v)) | Some(Value::Number(v)) => Some(v),
        _ => None,
    }
}

fn bool_text(v: Option<&Value>) -> Option<bool> {
    match v {
        Some(Value::Bool(v)) => Some(*v),
        _ => None,
    }
}

fn value_text(v: Option<&Value>, fallback: &str) -> String {
    text(v).unwrap_or(fallback).trim().replace('\n', " ")
}

fn list_text(v: Option<&Value>) -> Vec<String> {
    array(v)
        .iter()
        .filter_map(|v| text(Some(v)))
        .map(|v| v.trim().replace('\n', " "))
        .filter(|v| !v.is_empty())
        .collect()
}

fn markdown_cell(v: &str) -> String {
    v.replace('|', "\\|").replace('\n', " ").trim().to_string()
}

fn is_virtual_block_device(block: &Value) -> bool {
    matches!(
        text(field(block, "name")),
        Some(name) if name.starts_with("loop") || name.starts_with("ram") || name.starts_with("zram")
    )
}

fn is_loopback_interface(interface: &Value) -> bool {
    text(field(interface, "name")) == Some("lo")
}

fn node_hints(nodes: &[Value], label: &str, keys: &[&str]) -> String {
    let mut compatible_hits = vec![];
    let mut path_fallback_hits = vec![];
    for node in nodes {
        let path = value_text(field(node, "path"), "/unknown");
        let compat = list_text(field(node, "compatible")).join(", ");
        let path_lower = path.to_ascii_lowercase();
        let compat_lower = compat.to_ascii_lowercase();
        let row = format!(
            "- {}：{}（{}）",
            path,
            if compat.is_empty() {
                "无 compatible".into()
            } else {
                compat
            },
            value_text(field(node, "status"), "status 未声明")
        );
        if keys.iter().any(|key| compat_lower.contains(key)) {
            compatible_hits.push(row);
        } else if keys.iter().any(|key| path_lower.contains(key))
            && !["/gpio", "pinctrl", "_pins", "pin-", "_gpio"]
                .iter()
                .any(|noise| path_lower.contains(noise))
        {
            path_fallback_hits.push(row);
        }
    }
    compatible_hits.extend(path_fallback_hits);
    let hits: Vec<String> = compatible_hits.into_iter().take(8).collect();
    if hits.is_empty() {
        format!(
            "### {}\n- 未在已采集的设备树节点中识别到匹配项；这不证明该能力不存在。\n\n",
            label
        )
    } else {
        format!("### {}\n{}\n\n", label, hits.join("\n"))
    }
}

fn render(report: &Value) -> Result<String, String> {
    let schema = text(field(report, "schema")).ok_or_else(|| "报告缺少 schema".to_string())?;
    if schema != "hwprobe/v1" {
        return Err(format!("不支持的 hwprobe schema: {}", schema));
    }
    let platform = field(report, "platform").unwrap_or(&Value::Null);
    let cpu = field(report, "cpu").unwrap_or(&Value::Null);
    let storage = field(report, "storage").unwrap_or(&Value::Null);
    let network = field(report, "network").unwrap_or(&Value::Null);
    let buses = field(report, "buses").unwrap_or(&Value::Null);
    let thermal = field(report, "thermal").unwrap_or(&Value::Null);
    let power = field(report, "power").unwrap_or(&Value::Null);
    let dt = field(report, "device_tree").unwrap_or(&Value::Null);
    let mut out = String::new();

    out.push_str("# 外设简报\n\n");
    out.push_str("本简报由主机端根据 hardware.json 的原始采集事实生成。运行时 sysfs 枚举表示当前内核已暴露的设备；设备树命中表示板级能力或控制器线索，不能单独证明某个外接接口已经装配或接入。\n\n");
    out.push_str("## 平台与计算\n\n");
    out.push_str(&format!(
        "- 系统类型：{}\n",
        value_text(field(platform, "kind"), "未知")
    ));
    out.push_str(&format!(
        "- 内核：{}\n",
        value_text(field(platform, "kernel_release"), "不可读取")
    ));
    out.push_str(&format!(
        "- 主板型号：{}\n",
        value_text(field(dt, "model"), "未从 live DT 读取到")
    ));
    let compatible = list_text(field(dt, "compatible"));
    out.push_str(&format!(
        "- SoC/板级 compatible：{}\n",
        if compatible.is_empty() {
            "未读取到".into()
        } else {
            compatible.join("；")
        }
    ));
    out.push_str(&format!(
        "- CPU：{} 个逻辑核，online={}\n",
        array(field(cpu, "topology")).len(),
        value_text(field(cpu, "online"), "不可读取")
    ));
    out.push_str(&format!(
        "- 设备树节点：已解码 {}/{} 个{}\n\n",
        value_text(field(dt, "emitted_node_count"), "0"),
        value_text(field(dt, "node_count"), "0"),
        if bool_text(field(dt, "truncated")) == Some(true) {
            "（受采集上限截断）"
        } else {
            ""
        }
    ));

    out.push_str("## 存储\n\n");
    let blocks: Vec<&Value> = array(field(storage, "block_devices"))
        .iter()
        .filter(|block| !is_virtual_block_device(block))
        .collect();
    if blocks.is_empty() {
        out.push_str("- 没有通过 sysfs 枚举到块设备。\n\n");
    } else {
        out.push_str("| 设备 | 容量 | 型号/厂商 | 可移动 |\n| --- | ---: | --- | --- |\n");
        for block in &blocks {
            let sectors = text(field(block, "sectors_512"))
                .and_then(|s| s.parse::<u64>().ok())
                .map(|n| format!("{} MiB", n / 2048))
                .unwrap_or_else(|| "未知".into());
            let model = [text(field(block, "vendor")), text(field(block, "model"))]
                .into_iter()
                .flatten()
                .filter(|v| !v.trim().is_empty())
                .collect::<Vec<_>>()
                .join(" ");
            out.push_str(&format!(
                "| {} | {} | {} | {} |\n",
                markdown_cell(&value_text(field(block, "name"), "?")),
                sectors,
                markdown_cell(if model.is_empty() {
                    "未上报"
                } else {
                    &model
                }),
                value_text(field(block, "removable"), "未知")
            ));
        }
        out.push('\n');
    }

    out.push_str("## 网络\n\n");
    let interfaces: Vec<&Value> = array(field(network, "interfaces"))
        .iter()
        .filter(|interface| !is_loopback_interface(interface))
        .collect();
    if interfaces.is_empty() {
        out.push_str("- 没有通过 sysfs 枚举到网络接口。\n\n");
    } else {
        out.push_str("| 接口 | 当前状态 | MTU | MAC |\n| --- | --- | ---: | --- |\n");
        for iface in &interfaces {
            let mac = if bool_text(field(iface, "mac_redacted")) == Some(true) {
                "已脱敏".into()
            } else {
                value_text(field(iface, "mac"), "未上报")
            };
            out.push_str(&format!(
                "| {} | {} | {} | {} |\n",
                markdown_cell(&value_text(field(iface, "name"), "?")),
                value_text(field(iface, "operstate"), "未知"),
                value_text(field(iface, "mtu"), "未知"),
                mac
            ));
        }
        out.push('\n');
    }

    out.push_str("## 已枚举总线与外设\n\n");
    for (title, key) in [("I2C", "i2c"), ("SPI", "spi"), ("USB", "usb")] {
        let devices = array(field(buses, key));
        out.push_str(&format!("### {}（{} 项）\n", title, devices.len()));
        if devices.is_empty() {
            out.push_str("- 当前 sysfs 未暴露设备。\n\n");
            continue;
        }
        for device in devices.iter().take(12) {
            let id = value_text(field(device, "id"), "?");
            let name = [
                text(field(device, "manufacturer")),
                text(field(device, "product")),
                text(field(device, "name")),
                text(field(device, "modalias")),
            ]
            .into_iter()
            .flatten()
            .filter(|v| !v.trim().is_empty())
            .collect::<Vec<_>>()
            .join(" ");
            out.push_str(&format!(
                "- {}：{}\n",
                id,
                if name.is_empty() {
                    "内核未上报名称"
                } else {
                    &name
                }
            ));
        }
        if devices.len() > 12 {
            out.push_str("- 其余项目省略，完整列表见 hardware.json。\n");
        }
        out.push('\n');
    }

    out.push_str("## 设备树能力线索\n\n");
    out.push_str(&node_hints(
        array(field(dt, "nodes")),
        "串口与 UART",
        &["serial", "uart"],
    ));
    out.push_str(&node_hints(
        array(field(dt, "nodes")),
        "I2C 控制器与端点",
        &["i2c"],
    ));
    out.push_str(&node_hints(
        array(field(dt, "nodes")),
        "SPI 控制器与端点",
        &["spi"],
    ));
    out.push_str(&node_hints(
        array(field(dt, "nodes")),
        "MMC 与 SD 存储",
        &["mmc", "sdhci"],
    ));
    out.push_str(&node_hints(
        array(field(dt, "nodes")),
        "以太网",
        &["ethernet", "gmac"],
    ));
    out.push_str(&node_hints(
        array(field(dt, "nodes")),
        "USB 控制器",
        &["usb", "xhci", "dwc"],
    ));
    out.push_str(&node_hints(
        array(field(dt, "nodes")),
        "PCIe",
        &["pcie", "pci"],
    ));
    out.push_str(&node_hints(
        array(field(dt, "nodes")),
        "显示与 GPU",
        &["display", "gpu", "hdmi", "dsi"],
    ));
    out.push_str(&node_hints(
        array(field(dt, "nodes")),
        "相机与 CSI",
        &["camera", "csi"],
    ));
    out.push_str(&node_hints(array(field(dt, "nodes")), "GPIO", &["gpio"]));

    out.push_str("## 热管理与电源\n\n");
    for zone in array(field(thermal, "zones")) {
        let temp = text(field(zone, "temperature_millic"))
            .and_then(|v| v.parse::<i64>().ok())
            .map(|v| format!("{:.1} C", v as f64 / 1000.0))
            .unwrap_or_else(|| "不可读取".into());
        out.push_str(&format!(
            "- 热区 {}：{}，{}\n",
            value_text(field(zone, "zone"), "?"),
            value_text(field(zone, "type"), "类型未上报"),
            temp
        ));
    }
    for supply in array(field(power, "supplies")) {
        out.push_str(&format!(
            "- 电源 {}：{}，{}\n",
            value_text(field(supply, "name"), "?"),
            value_text(field(supply, "type"), "类型未上报"),
            value_text(field(supply, "status"), "状态未上报")
        ));
    }
    if array(field(thermal, "zones")).is_empty() && array(field(power, "supplies")).is_empty() {
        out.push_str("- 当前 sysfs 未暴露热区或电源供应项。\n");
    }

    out.push_str("\n## 阅读边界\n\n");
    if bool_text(field(dt, "truncated")) == Some(true) {
        out.push_str("- 当前设备树节点列表受采集上限截断；某一类线索未命中不能据此判断硬件不存在。重新采集时可提高 --max-dt-nodes。\n");
    }
    out.push_str(
        "- 本简报默认不显示 MAC、序列号等设备标识；完整报告是否包含它们取决于采集时的隐私选项。\n",
    );
    out.push_str("- 本工具不会主动扫 I2C/SPI、加载内核模块、读 MMIO 或改变设备配置；未列出并不等于硬件不存在。\n");
    out.push_str(
        "- 需要寄存器地址、中断、时钟或完整节点属性时，请回到 hardware.json 和 device-tree.tar。\n",
    );
    Ok(out)
}

pub fn write(report_path: &Path, brief_path: &Path) -> Result<(), String> {
    let content =
        fs::read_to_string(report_path).map_err(|e| format!("读取 hardware.json 失败: {}", e))?;
    let report = Parser::parse(&content)?;
    let rendered = render(&report)?;
    let temp = brief_path.with_extension("md.tmp");
    fs::write(&temp, rendered).map_err(|e| format!("写外设简报临时文件失败: {}", e))?;
    fs::rename(&temp, brief_path).map_err(|e| format!("发布外设简报失败: {}", e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_hardware_facts_as_human_brief() {
        let sample = r#"{
          "schema":"hwprobe/v1",
          "platform":{"kind":"linux","kernel_release":"5.15"},
          "cpu":{"online":"0-3","topology":[{},{},{},{}]},
          "storage":{"block_devices":[{"name":"mmcblk0","sectors_512":"62500000","model":"SD"}]},
          "network":{"interfaces":[{"name":"eth0","operstate":"up","mtu":"1500","mac_redacted":true}]},
          "buses":{"i2c":[{"id":"1-0050","name":"eeprom"}],"spi":[],"usb":[]},
          "thermal":{"zones":[{"zone":"thermal_zone0","type":"cpu-thermal","temperature_millic":"42500"}]},
          "power":{"supplies":[]},
          "device_tree":{"model":"Example Board","compatible":["vendor,board"],"node_count":4,"emitted_node_count":4,"truncated":false,
            "nodes":[{"path":"/serial@0","compatible":["vendor,uart"],"status":"okay"},{"path":"/i2c@1","compatible":["vendor,i2c"],"status":"okay"}]}
        }"#;
        let brief = render(&Parser::parse(sample).unwrap()).unwrap();
        assert!(brief.contains("Example Board"));
        assert!(brief.contains("mmcblk0"));
        assert!(brief.contains("eth0"));
        assert!(brief.contains("串口与 UART"));
        assert!(brief.contains("eeprom"));
    }
}
