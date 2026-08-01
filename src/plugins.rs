//! Local Ferry plugin packages.
//!
//! A plugin is deliberately a local, reviewable package rather than a dynamic
//! library: `plugin.toml` declares the entrypoint and constraints, while a
//! normal executable/script implements the feature. Ferry supplies the chosen
//! device context and consistent SSH options, but never saves plugin secrets.

use crate::config::{Device, Transport};
use crate::hwprobe;
use crate::sshx;
use crate::tomlite::Doc;
use crate::util::{cfg_dir, ensure_dir, home, render_cmd, run_capture, run_inherit, which, Output};
use std::fs;
use std::path::{Component, Path, PathBuf};

const MANIFEST: &str = "plugin.toml";
const SYSROOT_MANIFEST: &str = include_str!("../assets/plugins/sysroot-sync/plugin.toml");
const SYSROOT_ENTRY: &str = include_str!("../assets/plugins/sysroot-sync/run.sh");
const DEVICE_TREE_PULL_ID: &str = "device-tree-pull";
const DEVICE_TREE_MANIFEST: &str = include_str!("../assets/plugins/device-tree-pull/plugin.toml");
const DEVICE_TREE_ENTRY: &str = include_str!("../assets/plugins/device-tree-pull/run.sh");

#[derive(Debug, Clone)]
pub struct Plugin {
    pub id: String,
    pub name: String,
    pub version: String,
    pub description: String,
    pub entry: String,
    pub transport: String,
    pub risk: String,
    pub requires: Vec<String>,
    pub arguments: Vec<String>,
    pub summary: String,
    pub preview: Vec<String>,
    pub dir: PathBuf,
}

#[derive(Debug, Clone)]
pub struct PluginOutput {
    pub status: i32,
    pub stdout: String,
    pub stderr: String,
}

pub fn plugins_dir() -> PathBuf {
    cfg_dir().join("plugins")
}

pub fn builtin_ids() -> &'static [&'static str] {
    &["sysroot-sync", DEVICE_TREE_PULL_ID]
}

fn builtin_assets(id: &str) -> Result<(&'static str, &'static str), String> {
    match id {
        "sysroot-sync" => Ok((SYSROOT_MANIFEST, SYSROOT_ENTRY)),
        DEVICE_TREE_PULL_ID => Ok((DEVICE_TREE_MANIFEST, DEVICE_TREE_ENTRY)),
        _ => Err(format!("no built-in plugin named '{id}'")),
    }
}

fn is_device_tree_pull(plugin: &Plugin) -> bool {
    plugin.id == DEVICE_TREE_PULL_ID
}

fn valid_id(value: &str) -> bool {
    !value.is_empty()
        && value
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '-' || character == '_')
}

fn relative_entry(value: &str) -> bool {
    let path = Path::new(value);
    !path.is_absolute()
        && !value.is_empty()
        && path.components().all(|component| matches!(component, Component::Normal(_)))
}

fn required_string(doc: &Doc, key: &str) -> Result<String, String> {
    doc.get("plugin", key)
        .and_then(|value| value.as_str())
        .map(str::to_string)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| format!("plugin.toml: missing [plugin].{key}"))
}

fn optional_string(doc: &Doc, key: &str) -> String {
    doc.get("plugin", key)
        .and_then(|value| value.as_str())
        .unwrap_or_default()
        .to_string()
}

fn optional_list(doc: &Doc, key: &str) -> Vec<String> {
    doc.get("plugin", key)
        .and_then(|value| value.as_arr())
        .map(|items| items.to_vec())
        .unwrap_or_default()
}

fn parse_manifest(src: &str, dir: PathBuf) -> Result<Plugin, String> {
    let doc = Doc::parse(src).map_err(|error| format!("invalid plugin.toml: {error}"))?;
    let id = required_string(&doc, "id")?;
    let entry = required_string(&doc, "entry")?;
    if !valid_id(&id) {
        return Err("plugin.toml: id must contain only letters, numbers, '-' or '_'".into());
    }
    if !relative_entry(&entry) {
        return Err("plugin.toml: entry must be a relative file path within the plugin package".into());
    }
    let transport = required_string(&doc, "transport")?;
    if !matches!(transport.as_str(), "ssh" | "adb" | "serial" | "any") {
        return Err("plugin.toml: transport must be ssh, adb, serial, or any".into());
    }
    Ok(Plugin {
        id,
        name: required_string(&doc, "name")?,
        version: required_string(&doc, "version")?,
        description: required_string(&doc, "description")?,
        entry,
        transport,
        risk: required_string(&doc, "risk")?,
        requires: optional_list(&doc, "requires"),
        arguments: optional_list(&doc, "arguments"),
        summary: optional_string(&doc, "summary"),
        preview: optional_list(&doc, "preview"),
        dir,
    })
}

fn plugin_dir(id: &str) -> PathBuf {
    plugins_dir().join(id)
}

pub fn list() -> Result<Vec<Plugin>, String> {
    let root = plugins_dir();
    if !root.exists() {
        return Ok(vec![]);
    }
    let entries = fs::read_dir(&root).map_err(|error| format!("cannot read plugin directory: {error}"))?;
    let mut plugins = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        match load_dir(&path) {
            Ok(plugin) => plugins.push(plugin),
            Err(error) => eprintln!("ferry: ignoring plugin {}: {error}", path.display()),
        }
    }
    plugins.sort_by(|left, right| left.id.cmp(&right.id));
    Ok(plugins)
}

pub fn load(id: &str) -> Result<Plugin, String> {
    if !valid_id(id) {
        return Err("invalid plugin id".into());
    }
    load_dir(&plugin_dir(id))
}

fn load_dir(dir: &Path) -> Result<Plugin, String> {
    if !dir.is_dir() {
        return Err(format!("plugin package not found: {}", dir.display()));
    }
    let manifest = dir.join(MANIFEST);
    let src = fs::read_to_string(&manifest)
        .map_err(|error| format!("cannot read {}: {error}", manifest.display()))?;
    let plugin = parse_manifest(&src, dir.to_path_buf())?;
    let entry = plugin.dir.join(&plugin.entry);
    if !entry.is_file() {
        return Err(format!("plugin entrypoint is missing: {}", entry.display()));
    }
    Ok(plugin)
}

fn copy_tree(source: &Path, target: &Path) -> Result<(), String> {
    if source.symlink_metadata().map_err(|error| error.to_string())?.file_type().is_symlink() {
        return Err(format!("refusing symbolic link in plugin package: {}", source.display()));
    }
    fs::create_dir_all(target).map_err(|error| error.to_string())?;
    for entry in fs::read_dir(source).map_err(|error| error.to_string())? {
        let entry = entry.map_err(|error| error.to_string())?;
        let from = entry.path();
        let to = target.join(entry.file_name());
        let metadata = entry.metadata().map_err(|error| error.to_string())?;
        if metadata.is_dir() {
            copy_tree(&from, &to)?;
        } else if metadata.is_file() {
            fs::copy(&from, &to).map_err(|error| error.to_string())?;
        } else {
            return Err(format!("unsupported plugin package entry: {}", from.display()));
        }
    }
    Ok(())
}

#[cfg(unix)]
fn make_entry_executable(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let mut permissions = fs::metadata(path).map_err(|error| error.to_string())?.permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions).map_err(|error| error.to_string())
}

#[cfg(not(unix))]
fn make_entry_executable(_path: &Path) -> Result<(), String> {
    Ok(())
}

fn install_tree(source: &Path, force: bool) -> Result<Plugin, String> {
    let candidate = load_dir(source)?;
    ensure_dir(&plugins_dir()).map_err(|error| error.to_string())?;
    let destination = plugin_dir(&candidate.id);
    if destination.exists() && !force {
        return Err(format!("plugin '{}' is already installed; use --force to replace it", candidate.id));
    }
    let staging = plugins_dir().join(format!(".{}-{}", candidate.id, std::process::id()));
    if staging.exists() {
        fs::remove_dir_all(&staging).map_err(|error| error.to_string())?;
    }
    copy_tree(source, &staging)?;
    let staged = load_dir(&staging)?;
    make_entry_executable(&staged.dir.join(&staged.entry))?;
    if destination.exists() {
        fs::remove_dir_all(&destination).map_err(|error| error.to_string())?;
    }
    fs::rename(&staging, &destination).map_err(|error| error.to_string())?;
    load(&candidate.id)
}

pub fn install_local(source: &Path, force: bool) -> Result<Plugin, String> {
    install_tree(source, force)
}

pub fn install_builtin(id: &str, force: bool) -> Result<Plugin, String> {
    let (manifest, entry) = builtin_assets(id)?;
    ensure_dir(&plugins_dir()).map_err(|error| error.to_string())?;
    let staging = plugins_dir().join(format!(".builtin-{id}-{}", std::process::id()));
    if staging.exists() {
        fs::remove_dir_all(&staging).map_err(|error| error.to_string())?;
    }
    fs::create_dir_all(&staging).map_err(|error| error.to_string())?;
    fs::write(staging.join(MANIFEST), manifest).map_err(|error| error.to_string())?;
    fs::write(staging.join("run.sh"), entry).map_err(|error| error.to_string())?;
    make_entry_executable(&staging.join("run.sh"))?;
    let candidate = load_dir(&staging)?;
    let destination = plugin_dir(&candidate.id);
    if destination.exists() && !force {
        let _ = fs::remove_dir_all(&staging);
        return Err(format!("plugin '{}' is already installed; use --force to replace it", candidate.id));
    }
    if destination.exists() {
        fs::remove_dir_all(&destination).map_err(|error| error.to_string())?;
    }
    fs::rename(&staging, &destination).map_err(|error| error.to_string())?;
    load(id)
}

fn transport_matches(plugin: &Plugin, device: &Device) -> bool {
    plugin.transport == "any" || plugin.transport == device.transport.as_str()
}

pub fn preflight(plugin: &Plugin, device: &Device) -> Result<(), String> {
    if is_device_tree_pull(plugin) {
        match device.transport {
            Transport::Ssh if which("ssh").is_none() => {
                return Err("device-tree-pull requires the host ssh command".into())
            }
            Transport::Adb if which("adb").is_none() => {
                return Err("device-tree-pull requires the host adb command".into())
            }
            Transport::Serial => {
                return Err("device-tree-pull requires an SSH or ADB profile; serial cannot safely recover binary device-tree data".into())
            }
            _ => {}
        }
    }
    if !transport_matches(plugin, device) {
        return Err(format!("plugin '{}' requires {} transport; '{}' uses {}", plugin.id, plugin.transport, device.name, device.transport.as_str()));
    }
    for command in &plugin.requires {
        if which(command).is_none() {
            return Err(format!("plugin '{}' requires host command '{command}'", plugin.id));
        }
    }
    if device.transport == Transport::Ssh && (device.host.trim().is_empty() || device.user.trim().is_empty()) {
        return Err("SSH profile needs both host and user before running this plugin".into());
    }
    Ok(())
}

pub fn preview(plugin: &Plugin, device: &Device, arguments: &[String]) -> Result<Vec<String>, String> {
    preflight(plugin, device)?;
    if is_device_tree_pull(plugin) {
        let options = device_tree_options(arguments)?;
        let target = if device.transport == Transport::Ssh {
            format!("{}@{}:{}", device.user, device.host, device.port)
        } else {
            device.endpoint()
        };
        let mut steps = vec![
            format!("Target: {target}"),
            "Risk: target-read + host-write".into(),
            "Deploy Ferry's read-only hardware collector into a private target temporary directory.".into(),
            format!("Recover raw device tree: {}", options.output_dir.join("device-tree.tar").display()),
            format!("Recover hardware report: {}", options.output_dir.join("hardware.json").display()),
            "Remove the target temporary directory after collection, including on a failed transfer.".into(),
        ];
        if options.brief {
            steps.push(format!("Generate local peripheral brief: {}", options.output_dir.join("peripherals.md").display()));
        }
        if let Some(max) = options.max_dt_nodes {
            steps.push(format!("Limit decoded device-tree nodes in hardware.json to {max}; raw device-tree.tar remains complete."));
        }
        return Ok(steps);
    }
    let destination = arguments
        .windows(2)
        .find(|window| window[0] == "--dest" || window[0] == "--out")
        .map(|window| window[1].as_str())
        .unwrap_or("<destination>");
    let target = if device.transport == Transport::Ssh {
        format!("{}@{}:{}", device.user, device.host, device.port)
    } else {
        device.endpoint()
    };
    let mut steps = vec![format!("Target: {target}"), format!("Risk: {}", plugin.risk)];
    if !plugin.summary.is_empty() {
        steps.push(plugin.summary.clone());
    }
    for line in &plugin.preview {
        steps.push(line.replace("<destination>", destination));
    }
    steps.push(format!("Entrypoint: {}", render_cmd(&invocation_argv(plugin, arguments)?)));
    Ok(steps)
}

fn invocation_argv(plugin: &Plugin, arguments: &[String]) -> Result<Vec<String>, String> {
    let entry = plugin.dir.join(&plugin.entry);
    if !entry.is_file() {
        return Err(format!("plugin entrypoint is missing: {}", entry.display()));
    }
    let mut argv = vec![entry.display().to_string()];
    argv.extend(arguments.iter().cloned());
    Ok(argv)
}

fn expand_home(value: &str) -> PathBuf {
    if value == "~" {
        return home();
    }
    value
        .strip_prefix("~/")
        .map(|relative| home().join(relative))
        .unwrap_or_else(|| PathBuf::from(value))
}

fn device_tree_options(arguments: &[String]) -> Result<hwprobe::Options, String> {
    let mut output_dir = None;
    let mut brief = true;
    let mut max_dt_nodes = None;
    let mut index = 0;
    while index < arguments.len() {
        match arguments[index].as_str() {
            "--out" => {
                let Some(value) = arguments.get(index + 1) else {
                    return Err("device-tree-pull: --out requires a local directory".into());
                };
                output_dir = Some(expand_home(value));
                index += 2;
            }
            "--no-brief" => {
                brief = false;
                index += 1;
            }
            "--max-dt-nodes" => {
                let Some(value) = arguments.get(index + 1) else {
                    return Err("device-tree-pull: --max-dt-nodes requires a positive integer".into());
                };
                let parsed = value
                    .parse::<u32>()
                    .map_err(|_| "device-tree-pull: --max-dt-nodes requires a positive integer".to_string())?;
                if parsed == 0 {
                    return Err("device-tree-pull: --max-dt-nodes must be greater than zero".into());
                }
                max_dt_nodes = Some(parsed);
                index += 2;
            }
            "-h" | "--help" => {
                return Err("usage: device-tree-pull --out <local-directory> [--no-brief] [--max-dt-nodes N]".into())
            }
            value => return Err(format!("device-tree-pull: unknown argument '{value}'")),
        }
    }
    let output_dir = output_dir.ok_or("device-tree-pull: --out <local-directory> is required")?;
    Ok(hwprobe::Options {
        output_dir,
        bundle: true,
        brief,
        keep_remote: false,
        include_identifiers: false,
        max_dt_nodes,
    })
}

fn run_device_tree_pull(
    device: &Device,
    arguments: &[String],
    include_profile_password: bool,
) -> Result<PluginOutput, String> {
    let options = device_tree_options(arguments)?;
    let mut device = device.clone();
    if !include_profile_password {
        // The desktop bundle cannot act as Ferry's CLI askpass helper. More
        // importantly, this native operation needs no password exposure.
        device.password = None;
    }
    let result = hwprobe::collect(&device, &options)?;
    let mut stdout = format!(
        "Device-tree collection completed for {}\nOutput directory: {}\nHardware report: {}\n",
        device.name,
        result.output_dir.display(),
        result.report.display(),
    );
    if let Some(archive) = result.archive {
        stdout.push_str(&format!("Raw device tree: {}\n", archive.display()));
    } else {
        stdout.push_str("Raw device-tree archive was unavailable: target tar support may be missing.\n");
    }
    if let Some(brief) = result.brief {
        stdout.push_str(&format!("Peripheral brief: {}\n", brief.display()));
    }
    stdout.push_str("Target temporary files were removed.\n");
    Ok(PluginOutput {
        status: 0,
        stdout,
        stderr: String::new(),
    })
}

fn environment(device: &Device, non_interactive: bool, include_profile_password: bool) -> Vec<(String, String)> {
    let mut environment = vec![
        ("FERRY_PLUGIN".into(), "1".into()),
        ("FERRY_DEVICE_NAME".into(), device.name.clone()),
        ("FERRY_DEVICE_TRANSPORT".into(), device.transport.as_str().into()),
        ("FERRY_DEVICE_HOST".into(), device.host.clone()),
        ("FERRY_DEVICE_PORT".into(), device.port.to_string()),
        ("FERRY_DEVICE_USER".into(), device.user.clone()),
    ];
    if non_interactive {
        environment.push(("FERRY_PLUGIN_NONINTERACTIVE".into(), "1".into()));
    }
    if device.transport == Transport::Ssh {
        let mut ssh = vec!["ssh".to_string()];
        ssh.extend(sshx::base_opts(device));
        environment.push(("FERRY_SSH_RSH".into(), render_cmd(&ssh)));
        if include_profile_password {
            environment.extend(sshx::askpass_env(device));
        }
    }
    environment
}

pub fn run_inherit_plugin(plugin: &Plugin, device: &Device, arguments: &[String]) -> Result<i32, String> {
    preflight(plugin, device)?;
    if is_device_tree_pull(plugin) {
        let output = run_device_tree_pull(device, arguments, true)?;
        print!("{}", output.stdout);
        eprint!("{}", output.stderr);
        return Ok(output.status);
    }
    let argv = invocation_argv(plugin, arguments)?;
    run_inherit(&argv, &environment(device, false, true)).map_err(|error| error.to_string())
}

pub fn run_capture_plugin(plugin: &Plugin, device: &Device, arguments: &[String]) -> Result<PluginOutput, String> {
    preflight(plugin, device)?;
    if is_device_tree_pull(plugin) {
        return run_device_tree_pull(device, arguments, false);
    }
    let argv = invocation_argv(plugin, arguments)?;
    // A desktop host is not the `fy` askpass executable. It deliberately runs
    // plugins with key authentication only, so a stored profile password can
    // never be exposed to an extension or a broken askpass callback.
    let Output { status, stdout, stderr } = run_capture(&argv, &environment(device, true, false)).map_err(|error| error.to_string())?;
    Ok(PluginOutput { status, stdout, stderr })
}

pub fn display_arguments(plugin: &Plugin) -> String {
    if plugin.arguments.is_empty() {
        "(no arguments)".into()
    } else {
        plugin.arguments.join(" ")
    }
}

pub fn source_hint() -> String {
    format!("install a reviewed local directory containing {MANIFEST} and its entrypoint")
}

pub fn command_preview(plugin: &Plugin, arguments: &[String]) -> Result<String, String> {
    if is_device_tree_pull(plugin) {
        let options = device_tree_options(arguments)?;
        return Ok(format!(
            "Ferry native hardware collector --out {} --bundle",
            options.output_dir.display()
        ));
    }
    Ok(render_cmd(&invocation_argv(plugin, arguments)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_manifest_is_valid_and_has_safe_entrypoint() {
        let plugin = parse_manifest(SYSROOT_MANIFEST, PathBuf::from("/tmp/sysroot-sync")).unwrap();
        assert_eq!(plugin.id, "sysroot-sync");
        assert_eq!(plugin.transport, "ssh");
        assert!(relative_entry(&plugin.entry));
        assert!(plugin.requires.iter().any(|item| item == "rsync"));

        let tree = parse_manifest(DEVICE_TREE_MANIFEST, PathBuf::from("/tmp/device-tree-pull")).unwrap();
        assert_eq!(tree.id, DEVICE_TREE_PULL_ID);
        assert_eq!(tree.transport, "any");
        assert!(relative_entry(&tree.entry));
    }

    #[test]
    fn rejects_entrypoint_path_escape() {
        let manifest = SYSROOT_MANIFEST.replace("entry = \"run.sh\"", "entry = \"../run.sh\"");
        assert!(parse_manifest(&manifest, PathBuf::from("/tmp/plugin")).is_err());
    }

    #[test]
    fn device_tree_pull_requires_an_empty_output_directory_argument() {
        assert!(device_tree_options(&[]).is_err());
        assert!(device_tree_options(&["--out".into(), "./tree".into(), "--max-dt-nodes".into(), "0".into()]).is_err());
        let options = device_tree_options(&["--out".into(), "./tree".into(), "--no-brief".into(), "--max-dt-nodes".into(), "256".into()]).unwrap();
        assert_eq!(options.output_dir, PathBuf::from("./tree"));
        assert!(!options.brief);
        assert_eq!(options.max_dt_nodes, Some(256));
        assert!(options.bundle);
        assert!(!options.keep_remote);
    }
}
