use ferry::config::{Config, Device, Transport};
use ferry::httpd;
use ferry::pty::Pty;
use ferry::wsutil::{self, WsMsg};
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::{BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

static NEXT_SESSION_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct DeviceSummary {
    name: String,
    transport: String,
    endpoint: String,
    online: bool,
    status: String,
    hostname: String,
    last_seen: i64,
    has_password: bool,
    blackbox_running: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DeviceForm {
    name: String,
    transport: String,
    host: String,
    port: u16,
    user: String,
    key: String,
    legacy: bool,
    adb_serial: String,
    dev: String,
    baud: u32,
    dest: String,
    notes: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ProbeResult {
    online: bool,
    detail: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct TerminalStarted {
    session_id: String,
    title: String,
    transport: String,
    ws_url: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TransferRequest { name: String, direction: String, local: String, remote: String, force: bool, resume: bool, verify: bool }

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct OperationResult { ok: bool, detail: String }

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ForwardView { id: String, device: String, channel: String, detail: String, alive: bool }

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct TopRow { name: String, online: bool, cpu: String, memory: String, temperature: String, load: String }

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct BlackboxView { name: String, running: bool, incidents: usize, log_path: String }

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct WorkflowPlan { kind: String, device: String, preflight_ok: bool, preflight: String, steps: Vec<String>, rollback: String }

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WorkflowRequest { kind: String, device: String, nat: bool, persist: bool, boot_ok: bool, mode: String, confirmed: bool }

fn probe_device(d: &Device) -> ProbeResult {
    match d.transport {
        Transport::Ssh => {
            let address = format!("{}:{}", d.host, d.port);
            let online = address
                .parse()
                .ok()
                .map(|addr| TcpStream::connect_timeout(&addr, Duration::from_millis(700)).is_ok())
                .unwrap_or(false);
            ProbeResult {
                online,
                detail: if online {
                    "SSH port reachable".into()
                } else {
                    "SSH port is unreachable".into()
                },
            }
        }
        Transport::Adb => {
            let (online, detail) = ferry::adbx::probe(d);
            ProbeResult { online, detail }
        }
        Transport::Serial => {
            let blackbox = ferry::blackbox::running_for(&d.name);
            let present = d
                .dev
                .as_ref()
                .map(|path| std::path::Path::new(path).exists())
                .unwrap_or(false);
            ProbeResult {
                online: present,
                detail: match (present, blackbox) {
                    (true, true) => "Serial device present; black box is recording".into(),
                    (true, false) => "Serial device present".into(),
                    (false, _) => "Serial device is not present".into(),
                },
            }
        }
    }
}

fn to_form(d: &Device) -> DeviceForm {
    DeviceForm {
        name: d.name.clone(),
        transport: d.transport.as_str().into(),
        host: d.host.clone(),
        port: d.port,
        user: d.user.clone(),
        key: d.key.clone().unwrap_or_default(),
        legacy: d.legacy,
        adb_serial: d.adb_serial.clone().unwrap_or_default(),
        dev: d.dev.clone().unwrap_or_default(),
        baud: d.baud,
        dest: d.dest.clone(),
        notes: d.notes.clone(),
    }
}

fn validate_name(name: &str) -> Result<(), String> {
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_alphanumeric() || c == '_' || c == '-')
    {
        return Err("Device names may contain only letters, numbers, '_' and '-' .".into());
    }
    Ok(())
}

#[tauri::command]
fn list_devices() -> Vec<DeviceSummary> {
    let cfg = Config::load();
    cfg.devices
        .values()
        .map(|d| {
            let probe = probe_device(d);
            let facts = ferry::config::facts_load(&d.name);
            DeviceSummary {
                name: d.name.clone(),
                transport: d.transport.as_str().into(),
                endpoint: d.endpoint(),
                online: probe.online,
                status: probe.detail,
                hostname: facts.hostname,
                last_seen: facts.last_seen,
                has_password: d.password.is_some(),
                blackbox_running: ferry::blackbox::running_for(&d.name),
            }
        })
        .collect()
}

#[tauri::command]
fn get_device(name: String) -> Result<DeviceForm, String> {
    Config::load()
        .find(&name)
        .map(|d| to_form(&d))
        .ok_or_else(|| format!("Unknown device '{name}'."))
}

#[tauri::command]
fn save_device(form: DeviceForm) -> Result<DeviceForm, String> {
    validate_name(&form.name)?;
    let transport = Transport::parse(&form.transport).ok_or("Unknown transport.")?;
    let mut cfg = Config::load();
    let mut d = cfg
        .devices
        .get(&form.name)
        .cloned()
        .unwrap_or_else(|| Device::new(&form.name, transport));

    d.transport = transport;
    d.host = form.host.trim().to_string();
    d.port = if form.port == 0 { 22 } else { form.port };
    d.user = if form.user.trim().is_empty() {
        "root".into()
    } else {
        form.user.trim().into()
    };
    d.key = (!form.key.trim().is_empty()).then(|| form.key.trim().to_string());
    d.legacy = form.legacy;
    d.adb_serial = (!form.adb_serial.trim().is_empty()).then(|| form.adb_serial.trim().to_string());
    d.dev = (!form.dev.trim().is_empty()).then(|| form.dev.trim().to_string());
    d.baud = if form.baud == 0 { 115_200 } else { form.baud };
    d.dest = if form.dest.trim().is_empty() {
        "/tmp".into()
    } else {
        form.dest.trim().into()
    };
    d.notes = form.notes.trim().into();
    cfg.devices.insert(d.name.clone(), d.clone());
    cfg.save().map_err(|e| e.to_string())?;
    Ok(to_form(&d))
}

#[tauri::command]
fn check_connection(name: String) -> Result<ProbeResult, String> {
    let d = Config::load()
        .find(&name)
        .ok_or_else(|| format!("Unknown device '{name}'."))?;
    Ok(probe_device(&d))
}

#[tauri::command]
fn transfer(request: TransferRequest) -> Result<OperationResult, String> {
    let device = Config::load().find(&request.name).ok_or_else(|| format!("Unknown device '{}'.", request.name))?;
    let opts = ferry::xfer::XferOpts { force: request.force, resume: request.resume, verify: request.verify, skip_same: true };
    let files = if request.direction == "push" {
        ferry::xfer::push(&device, Path::new(&request.local), &request.remote, &opts)
    } else {
        ferry::xfer::pull(&device, &request.remote, Path::new(&request.local), &opts)
    }?;
    let sent: u64 = files.iter().map(|file| file.sent).sum();
    let skipped = files.iter().filter(|file| file.skipped).count();
    Ok(OperationResult { ok: true, detail: format!("{} file(s), {} bytes transferred, {} unchanged", files.len(), sent, skipped) })
}

#[tauri::command]
fn list_forwards() -> Vec<ForwardView> {
    ferry::fwd::collect(&Config::load()).into_iter().map(|entry| ForwardView {
        id: entry.id, device: entry.dev, channel: entry.channel, detail: entry.human, alive: entry.alive,
    }).collect()
}

#[tauri::command]
fn add_forward(name: String, spec: String) -> Result<OperationResult, String> {
    let cfg = Config::load();
    let device = cfg.find(&name).ok_or_else(|| format!("Unknown device '{name}'."))?;
    let id = ferry::fwd::add(&cfg, &device, &spec)?;
    Ok(OperationResult { ok: true, detail: format!("Forward {} created.", if id.is_empty() { spec } else { id }) })
}

#[tauri::command]
fn remove_forward(id: String) -> OperationResult {
    ferry::fwd::remove(&Config::load(), &id);
    OperationResult { ok: true, detail: format!("Forward {id} removed.") }
}

#[tauri::command]
fn top_snapshot() -> Vec<TopRow> {
    let cfg = Config::load();
    cfg.devices.values().filter(|device| device.transport != Transport::Serial).map(|device| {
        let command = "printf 'CPU '; awk '/^cpu /{print $2+$3+$4,$2+$3+$4+$5}' /proc/stat; free 2>/dev/null | awk '/Mem:/{print \"MEM \"$3\" \"$2}'; cat /sys/class/thermal/thermal_zone0/temp 2>/dev/null | sed 's/^/TMP /'; awk '{print \"LAV \"$1}' /proc/loadavg";
        let output = match device.transport { Transport::Ssh => ferry::sshx::exec_capture(device, command).ok(), Transport::Adb => ferry::adbx::exec_capture(device, command).ok(), Transport::Serial => None };
        let mut row = TopRow { name: device.name.clone(), online: false, cpu: "--".into(), memory: "--".into(), temperature: "--".into(), load: "--".into() };
        if let Some(output) = output { row.online = output.status == 0 || !output.stdout.is_empty(); for line in output.stdout.lines() { let fields: Vec<&str> = line.split_whitespace().collect(); match fields.first().copied() { Some("CPU") if fields.len() == 3 => { let busy: f64 = fields[1].parse().unwrap_or(0.0); let total: f64 = fields[2].parse().unwrap_or(1.0); row.cpu = format!("{:.0}%", 100.0 * busy / total.max(1.0)); }, Some("MEM") if fields.len() == 3 => row.memory = format!("{:.0}/{:.0} MiB", fields[1].parse::<f64>().unwrap_or(0.0)/1024.0, fields[2].parse::<f64>().unwrap_or(1.0)/1024.0), Some("TMP") if fields.len() == 2 => { let raw: f64 = fields[1].parse().unwrap_or(0.0); row.temperature = format!("{:.1} C", if raw > 1000.0 { raw/1000.0 } else { raw }); }, Some("LAV") if fields.len() == 2 => row.load = fields[1].into(), _ => {} } } }
        row
    }).collect()
}

#[tauri::command]
fn blackboxes() -> Vec<BlackboxView> {
    Config::load().devices.values().filter(|device| device.transport == Transport::Serial).map(|device| BlackboxView {
        name: device.name.clone(), running: ferry::blackbox::running_for(&device.name), incidents: std::fs::read_dir(ferry::blackbox::incidents_dir(&device.name)).map(|entries| entries.count()).unwrap_or(0), log_path: ferry::blackbox::log_path(&device.name).display().to_string(),
    }).collect()
}

#[tauri::command]
fn set_blackbox(name: String, enabled: bool) -> Result<OperationResult, String> {
    if enabled { ferry::blackbox::start(&Config::load(), &name)?; } else { ferry::blackbox::stop(&name); }
    Ok(OperationResult { ok: true, detail: format!("Black box for {name} {}.", if enabled { "started" } else { "stopped" }) })
}

#[tauri::command]
fn blackbox_blame(name: String, lines: usize) -> Result<String, String> {
    let mut incidents: Vec<_> = std::fs::read_dir(ferry::blackbox::incidents_dir(&name)).map_err(|e| e.to_string())?.flatten().map(|entry| entry.path()).collect();
    incidents.sort();
    if let Some(last) = incidents.last() { return std::fs::read_to_string(last).map_err(|e| e.to_string()); }
    let log = std::fs::read_to_string(ferry::blackbox::log_path(&name)).unwrap_or_default();
    Ok(log.lines().rev().take(lines.max(1)).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("\n"))
}

#[tauri::command]
fn workflow_preview(kind: String, device: String, nat: bool, persist: bool, boot_ok: bool, mode: String) -> Result<WorkflowPlan, String> {
    let current = Config::load().find(&device).ok_or_else(|| format!("Unknown device '{device}'."))?;
    let probe = probe_device(&current);
    let mut steps = vec![format!("Preflight: {}", probe.detail), "Dry-run preview: inspect the generated Ferry operation before modifying host or device state.".into()];
    let rollback = match kind.as_str() {
        "share" => { steps.push(format!("Enable {} share{}{}.", if nat { "NAT" } else { "proxy" }, if persist { " and persist environment" } else { "" }, "")); "Disable the share workflow; proxy tunnel or Ferry NAT state is removed.".into() }
        "up" => { steps.push(format!("Promote {} from serial toward SSH{}.", current.name, if boot_ok { "; bootloader boot is allowed" } else { "" })); "The serial profile remains usable; inspect the terminal and revert any board network setting manually if promotion stops midway.".into() }
        "usb-net" => { steps.push("Wait for a newly attached USB network interface, assign the Ferry /30 address, then probe the board SSH endpoint.".into()); "Disable Ferry NAT if enabled and remove the host USB interface address with the operating system network tool.".into() }
        "gadget-install" => { steps.push(format!("Install the {} USB gadget script{} on the selected board.", mode, if persist { " and register autostart" } else { "" })); "Disable the installed service/script on the board and remove its gadget configuration.".into() }
        _ => return Err("Unknown workflow.".into()),
    };
    Ok(WorkflowPlan { kind, device, preflight_ok: probe.online || current.transport == Transport::Serial, preflight: probe.detail, steps, rollback })
}

#[tauri::command]
fn workflow_execute(request: WorkflowRequest) -> Result<OperationResult, String> {
    if !request.confirmed { return Err("Execution requires an explicit confirmation after reviewing the plan.".into()); }
    let mut cfg = Config::load();
    let device = cfg.find(&request.device).ok_or_else(|| format!("Unknown device '{}'.", request.device))?;
    match request.kind.as_str() {
        "share" => ferry::share::enable(&cfg, &device, request.nat, request.persist, false, None)?,
        "up" => ferry::up::up(&mut cfg, &request.device, request.boot_ok)?,
        "usb-net" => ferry::usbnet::usb_net(&mut cfg, request.nat, Some(request.device.clone()))?,
        "gadget-install" => ferry::usbnet::gadget_install(&device, &request.mode, request.persist)?,
        _ => return Err("Unknown workflow.".into()),
    }
    Ok(OperationResult { ok: true, detail: format!("{} workflow completed for {}.", request.kind, request.device) })
}

fn session_token(sequence: u64) -> String {
    let mut bytes = [0_u8; 16];
    if File::open("/dev/urandom")
        .and_then(|mut source| source.read_exact(&mut bytes))
        .is_err()
    {
        let fallback = format!("{}:{}", std::process::id(), sequence);
        for (index, byte) in fallback.bytes().enumerate() {
            bytes[index % bytes.len()] ^= byte;
        }
    }
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[tauri::command]
fn start_terminal(name: String, cols: u16, rows: u16, command: Option<String>) -> Result<TerminalStarted, String> {
    let device = Config::load()
        .find(&name)
        .ok_or_else(|| format!("Unknown device '{name}'."))?;
    let sequence = NEXT_SESSION_ID.fetch_add(1, Ordering::Relaxed);
    let session_id = format!("{}-{}", device.name, sequence);
    let token = session_token(sequence);
    let transport = device.transport.as_str().to_string();
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .map_err(|e| format!("Unable to reserve the local terminal port: {e}"))?;
    let port = listener
        .local_addr()
        .map_err(|e| format!("Unable to inspect the local terminal port: {e}"))?
        .port();

    let connection_token = token.clone();
    std::thread::spawn(move || {
        if let Ok((stream, _)) = listener.accept() {
            let _ = serve_terminal_connection(
                stream,
                connection_token,
                device,
                rows.max(2),
                cols.max(2), command,
            );
        }
    });

    Ok(TerminalStarted {
        session_id,
        title: name,
        transport,
        ws_url: format!("ws://127.0.0.1:{port}/terminal?token={token}"),
    })
}

fn serve_terminal_connection(
    stream: TcpStream,
    token: String,
    device: Device,
    rows: u16,
    cols: u16,
    command: Option<String>,
) -> Result<(), String> {
    let mut incoming = BufReader::new(stream);
    let request = httpd::parse_request(&mut incoming).ok_or("Invalid local terminal request.")?;
    if !request.is_websocket()
        || request.path != "/terminal"
        || request.q("token") != Some(token.as_str())
    {
        return Err("Rejected an unauthorised local terminal request.".into());
    }
    let key = request
        .header("sec-websocket-key")
        .ok_or("WebSocket handshake did not include a key.")?;
    let mut response = incoming
        .get_ref()
        .try_clone()
        .map_err(|e| format!("Unable to write WebSocket handshake: {e}"))?;
    write_handshake(&mut response, key)?;

    match device.transport {
        Transport::Ssh | Transport::Adb => bridge_pty(&mut incoming, response, &device, rows, cols, command),
        Transport::Serial => bridge_serial(&mut incoming, response, &device),
    }
}

fn write_handshake(stream: &mut TcpStream, key: &str) -> Result<(), String> {
    let response = format!(
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {}\r\n\r\n",
        wsutil::ws_accept(key)
    );
    stream
        .write_all(response.as_bytes())
        .and_then(|_| stream.flush())
        .map_err(|e| format!("Unable to complete WebSocket handshake: {e}"))
}

fn bridge_pty(
    incoming: &mut BufReader<TcpStream>,
    response: TcpStream,
    device: &Device,
    rows: u16,
    cols: u16,
    command: Option<String>,
) -> Result<(), String> {
    let (program, args) = match device.transport {
        Transport::Ssh => {
            let mut args = ferry::sshx::base_opts(device);
            args.push("-tt".into());
            args.push(ferry::sshx::target(device));
            if let Some(command) = command.as_deref() { args.push(command.into()); }
            ("ssh", args)
        }
        Transport::Adb => {
            let rest: Vec<&str> = if let Some(command) = command.as_deref() { vec!["shell", command] } else { vec!["shell"] };
            let mut args = ferry::adbx::adb_argv(device, &rest);
            args.remove(0);
            ("adb", args)
        }
        Transport::Serial => unreachable!(),
    };
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let env = vec![
        ("TERM".to_string(), "xterm-256color".to_string()),
        ("FERRY_DESKTOP".to_string(), "1".to_string()),
    ];
    let pty = Pty::spawn(program, &refs, rows, cols, &env)
        .map_err(|e| format!("Unable to start {program}: {e}"))?;
    let pty = Arc::new(Mutex::new(pty));
    let reader = pty
        .lock()
        .map_err(|_| "Terminal state became unavailable.")?
        .reader()
        .map_err(|e| format!("Unable to read terminal output: {e}"))?;
    let writer = pty
        .lock()
        .map_err(|_| "Terminal state became unavailable.")?
        .writer()
        .map_err(|e| format!("Unable to write terminal input: {e}"))?;
    bridge_pty_io(incoming, response, pty, reader, writer);
    Ok(())
}

fn bridge_pty_io(
    incoming: &mut BufReader<TcpStream>,
    response: TcpStream,
    pty: Arc<Mutex<Pty>>,
    mut reader: File,
    mut writer: File,
) {
    let output = Arc::new(Mutex::new(response));
    let reader_output = output.clone();
    let reader_thread = std::thread::spawn(move || {
        let mut bytes = [0_u8; 8192];
        while let Ok(count) = reader.read(&mut bytes) {
            if count == 0 {
                break;
            }
            let Ok(mut stream) = reader_output.lock() else {
                break;
            };
            if wsutil::ws_write_binary(&mut *stream, &bytes[..count]).is_err() {
                break;
            }
        }
        if let Ok(mut stream) = reader_output.lock() {
            let _ = wsutil::ws_write_text(
                &mut *stream,
                "\r\n\x1b[2m[ferry] terminal session ended\x1b[0m\r\n",
            );
            let _ = wsutil::ws_write(&mut *stream, 0x8, b"");
        }
    });

    while let Ok(message) = wsutil::ws_read(incoming) {
        match message {
            WsMsg::Binary(bytes) => {
                if writer
                    .write_all(&bytes)
                    .and_then(|_| writer.flush())
                    .is_err()
                {
                    break;
                }
            }
            WsMsg::Text(text) => {
                let text = String::from_utf8_lossy(&text);
                if let Some((rows, cols)) = parse_resize(&text) {
                    if let Ok(pty) = pty.lock() {
                        pty.resize(rows, cols);
                    }
                }
            }
            WsMsg::Ping(bytes) => {
                if let Ok(mut stream) = output.lock() {
                    let _ = wsutil::ws_write(&mut *stream, 0xA, &bytes);
                }
            }
            WsMsg::Close => break,
            WsMsg::Pong(_) => {}
        }
    }
    if let Ok(mut pty) = pty.lock() {
        pty.kill();
    }
    let _ = reader_thread.join();
}

fn bridge_serial(
    incoming: &mut BufReader<TcpStream>,
    response: TcpStream,
    device: &Device,
) -> Result<(), String> {
    let (reader, writer): (Box<dyn Read + Send>, Box<dyn Write + Send>) =
        if ferry::blackbox::running_for(&device.name) {
            let stream = UnixStream::connect(ferry::blackbox::sock_path(&device.name))
                .map_err(|e| format!("Cannot attach to black box: {e}"))?;
            let reader = stream.try_clone().map_err(|e| e.to_string())?;
            (Box::new(reader), Box::new(stream))
        } else {
            let path = device
                .dev
                .as_deref()
                .ok_or("The device profile has no serial path.")?;
            let stream = ferry::serialx::open_port(path, device.baud)
                .map_err(|e| format!("Cannot open serial port: {e}"))?;
            let reader = stream.try_clone().map_err(|e| e.to_string())?;
            (Box::new(reader), Box::new(stream))
        };
    bridge_stream_io(incoming, response, reader, writer);
    Ok(())
}

fn bridge_stream_io(
    incoming: &mut BufReader<TcpStream>,
    response: TcpStream,
    mut reader: Box<dyn Read + Send>,
    mut writer: Box<dyn Write + Send>,
) {
    let output = Arc::new(Mutex::new(response));
    let reader_output = output.clone();
    std::thread::spawn(move || {
        let mut bytes = [0_u8; 8192];
        while let Ok(count) = reader.read(&mut bytes) {
            if count == 0 {
                break;
            }
            let Ok(mut stream) = reader_output.lock() else {
                break;
            };
            if wsutil::ws_write_binary(&mut *stream, &bytes[..count]).is_err() {
                break;
            }
        }
    });

    while let Ok(message) = wsutil::ws_read(incoming) {
        match message {
            WsMsg::Binary(bytes) => {
                if writer
                    .write_all(&bytes)
                    .and_then(|_| writer.flush())
                    .is_err()
                {
                    break;
                }
            }
            WsMsg::Ping(bytes) => {
                if let Ok(mut stream) = output.lock() {
                    let _ = wsutil::ws_write(&mut *stream, 0xA, &bytes);
                }
            }
            WsMsg::Close => break,
            WsMsg::Text(_) | WsMsg::Pong(_) => {}
        }
    }
}

fn parse_resize(message: &str) -> Option<(u16, u16)> {
    if !message.contains("resize") {
        return None;
    }
    let rows = extract_number(message, "rows")?.clamp(1, u16::MAX as u32) as u16;
    let cols = extract_number(message, "cols")?.clamp(1, u16::MAX as u32) as u16;
    Some((rows, cols))
}

fn extract_number(message: &str, key: &str) -> Option<u32> {
    let key_at = message.find(&format!("\"{key}\""))?;
    let after_key = &message[key_at + key.len() + 2..];
    let colon = after_key.find(':')?;
    let value = after_key[colon + 1..].trim_start();
    let end = value
        .find(|character: char| !character.is_ascii_digit())
        .unwrap_or(value.len());
    value[..end].parse().ok()
}

fn main() {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![
            list_devices,
            get_device,
            save_device,
            check_connection,
            start_terminal,
        ])
        .run(tauri::generate_context!())
        .expect("error while running Ferry Desktop");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_resize_control_messages() {
        assert_eq!(
            parse_resize(r#"{"t":"resize","rows":24,"cols":90}"#),
            Some((24, 90))
        );
        assert_eq!(
            parse_resize(r#"{"t":"resize","rows":0,"cols":90000}"#),
            Some((1, u16::MAX))
        );
        assert_eq!(parse_resize("echo not a control message"), None);
    }
}
