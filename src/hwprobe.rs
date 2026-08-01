//! One-shot deployment and recovery of the embedded hwprobe.sh agent.

use crate::adbx;
use crate::config::{Device, Transport};
use crate::peripheral_brief;
use crate::sshx;
use crate::util::{dry, run_capture, shell_quote, Output};
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub const SCRIPT: &str = include_str!("../assets/hwprobe.sh");

#[derive(Clone)]
pub struct Options {
    pub output_dir: PathBuf,
    pub bundle: bool,
    pub brief: bool,
    pub keep_remote: bool,
    pub include_identifiers: bool,
    pub max_dt_nodes: Option<u32>,
}

pub struct Result {
    pub output_dir: PathBuf,
    pub report: PathBuf,
    pub archive: Option<PathBuf>,
    pub brief: Option<PathBuf>,
    pub remote_dir: String,
}

fn remote_capture(d: &Device, command: &str) -> std::io::Result<Output> {
    match d.transport {
        Transport::Ssh => sshx::exec_capture(d, command),
        Transport::Adb => adbx::exec_capture(d, command),
        Transport::Serial => Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "serial transport",
        )),
    }
}

fn target_dir(base: &str) -> String {
    format!(
        "{}/ferry-hwprobe-{}-{}",
        base,
        std::process::id(),
        crate::util::now_epoch()
    )
}

fn is_android_target(d: &Device) -> bool {
    if d.transport == Transport::Adb {
        return true;
    }
    remote_capture(
        d,
        "command -v getprop >/dev/null 2>&1 && { test -d /system || test -d /apex || test -n \"$(getprop ro.build.version.sdk 2>/dev/null)\"; }",
    )
    .map(|output| output.status == 0)
    .unwrap_or(false)
}

fn temp_bases(transport: Transport, android: bool) -> &'static [&'static str] {
    if !android {
        return &["/tmp"];
    }
    match transport {
        // `adb shell` normally owns /data/local/tmp. Public storage is a
        // fallback for restricted devices where that namespace is unavailable.
        Transport::Adb => &["/data/local/tmp", "/sdcard", "/storage/emulated/0"],
        // SimpleSSHD runs under its application user, which can write its own
        // files directory even on Android builds that mount /tmp read-only.
        Transport::Ssh => &[
            "/data/data/org.galexander.sshd/files",
            "/data/local/tmp",
            "/sdcard",
            "/storage/emulated/0",
        ],
        Transport::Serial => &[],
    }
}

fn prepare_remote_dir(d: &Device) -> std::result::Result<String, String> {
    let android = is_android_target(d);
    let bases = temp_bases(d.transport, android);
    let mut failures = vec![];
    for base in bases {
        let dir = target_dir(base);
        let quoted = shell_quote(&dir);
        let check = format!(
            "umask 077; mkdir -p {q} && test -d {q} && test -w {q}",
            q = quoted
        );
        match remote_capture(d, &check) {
            Ok(output) if output.status == 0 => return Ok(dir),
            Ok(output) => {
                let detail = if output.stderr.trim().is_empty() {
                    output.stdout.trim()
                } else {
                    output.stderr.trim()
                };
                failures.push(format!("{base}: {detail}"));
            }
            Err(error) => failures.push(format!("{base}: {error}")),
        }
    }
    let kind = if android { "Android" } else { "target" };
    Err(format!(
        "无法在 {kind} 上创建 Ferry 临时目录（尝试 {}）: {}",
        bases.join(", "),
        failures.join("; ")
    ))
}

fn upload(d: &Device, remote_dir: &str, remote_script: &str) -> std::io::Result<()> {
    match d.transport {
        Transport::Ssh => {
            let mut args = vec!["ssh".to_string()];
            args.extend(sshx::base_opts(d));
            args.push(sshx::target(d));
            args.push(format!(
                "mkdir -p {d} && cat > {s} && (chmod 700 {s} 2>/dev/null || true)",
                d = shell_quote(remote_dir),
                s = shell_quote(remote_script)
            ));
            let mut child = Command::new(&args[0])
                .args(&args[1..])
                .envs(sshx::askpass_env(d))
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .spawn()?;
            child
                .stdin
                .as_mut()
                .expect("piped stdin")
                .write_all(SCRIPT.as_bytes())?;
            let out = child.wait_with_output()?;
            if out.status.success() {
                Ok(())
            } else {
                Err(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    String::from_utf8_lossy(&out.stderr).to_string(),
                ))
            }
        }
        Transport::Adb => {
            let prepared = remote_capture(d, &format!("mkdir -p {}", shell_quote(remote_dir)))?;
            if prepared.status != 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    prepared.stderr,
                ));
            }
            let local =
                std::env::temp_dir().join(format!("ferry-hwprobe-{}.sh", std::process::id()));
            fs::write(&local, SCRIPT)?;
            let local_s = local.display().to_string();
            let out = run_capture(&adbx::adb_argv(d, &["push", &local_s, remote_script]), &[])?;
            let _ = fs::remove_file(local);
            if out.status != 0 {
                return Err(std::io::Error::new(std::io::ErrorKind::Other, out.stderr));
            }
            // The collector is invoked as `sh <script>`; execution permission
            // is unnecessary and public Android storage commonly rejects chmod.
            let _ = remote_capture(d, &format!("chmod 700 {} 2>/dev/null", shell_quote(remote_script)));
            Ok(())
        }
        Transport::Serial => Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "serial transport",
        )),
    }
}

fn download(d: &Device, remote: &str, local: &Path) -> std::io::Result<()> {
    let file = File::create(local)?;
    let status = match d.transport {
        Transport::Ssh => {
            let mut args = vec!["ssh".to_string()];
            args.extend(sshx::base_opts(d));
            args.push(sshx::target(d));
            args.push(format!("cat {}", shell_quote(remote)));
            Command::new(&args[0])
                .args(&args[1..])
                .envs(sshx::askpass_env(d))
                .stdin(Stdio::null())
                .stdout(Stdio::from(file))
                .status()?
        }
        Transport::Adb => {
            let args = adbx::adb_argv(
                d,
                &[
                    "exec-out",
                    "sh",
                    "-c",
                    &format!("cat {}", shell_quote(remote)),
                ],
            );
            Command::new(&args[0])
                .args(&args[1..])
                .stdin(Stdio::null())
                .stdout(Stdio::from(file))
                .status()?
        }
        Transport::Serial => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "serial transport",
            ))
        }
    };
    if status.success() {
        Ok(())
    } else {
        let _ = fs::remove_file(local);
        Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            "artifact download failed",
        ))
    }
}

fn remove_remote(d: &Device, dir: &str) {
    let _ = remote_capture(d, &format!("rm -rf {}", shell_quote(dir)));
}

pub fn collect(d: &Device, o: &Options) -> std::result::Result<Result, String> {
    if d.transport == Transport::Serial {
        return Err("串口通道无法可靠回收二进制设备树；先执行 fy up 获取 ssh 或 adb 通道".into());
    }
    if dry() {
        return Ok(Result {
            output_dir: o.output_dir.clone(),
            report: o.output_dir.join("hardware.json"),
            archive: o.bundle.then(|| o.output_dir.join("device-tree.tar")),
            brief: o.brief.then(|| o.output_dir.join("peripherals.md")),
            remote_dir: "<dry-run>".into(),
        });
    }
    if o.output_dir.exists()
        && o.output_dir
            .read_dir()
            .map_err(|e| e.to_string())?
            .next()
            .is_some()
    {
        return Err(format!("输出目录非空: {}", o.output_dir.display()));
    }
    fs::create_dir_all(&o.output_dir).map_err(|e| e.to_string())?;
    let remote_dir = prepare_remote_dir(d)?;
    let script = format!("{}/hwprobe.sh", remote_dir);
    let report = format!("{}/hardware.json", remote_dir);
    let work = (|| -> std::result::Result<(Option<PathBuf>, Option<PathBuf>), String> {
        upload(d, &remote_dir, &script).map_err(|e| format!("下发采集器失败: {}", e))?;
        let mut command = format!(
            "sh {} collect --out {}",
            shell_quote(&script),
            shell_quote(&report)
        );
        if o.bundle {
            command.push_str(&format!(" --bundle {}", shell_quote(&remote_dir)));
        }
        if o.include_identifiers {
            command.push_str(" --include-identifiers");
        }
        if let Some(n) = o.max_dt_nodes {
            command.push_str(&format!(" --max-dt-nodes {}", n));
        }
        let run = remote_capture(d, &command).map_err(|e| format!("运行采集器失败: {}", e))?;
        if run.status != 0 {
            return Err(format!(
                "目标端采集器失败({}): {}",
                run.status,
                run.stderr.trim()
            ));
        }
        let local_report = o.output_dir.join("hardware.json");
        download(d, &report, &local_report)
            .map_err(|e| format!("回收 hardware.json 失败: {}", e))?;
        let remote_archive = format!("{}/device-tree.tar", remote_dir);
        let archive = if remote_capture(d, &format!("test -f {}", shell_quote(&remote_archive)))
            .map_err(|e| e.to_string())?
            .status
            == 0
        {
            let local_archive = o.output_dir.join("device-tree.tar");
            download(d, &remote_archive, &local_archive)
                .map_err(|e| format!("回收 device-tree.tar 失败: {}", e))?;
            Some(local_archive)
        } else {
            None
        };
        let brief = if o.brief {
            let local_brief = o.output_dir.join("peripherals.md");
            peripheral_brief::write(&local_report, &local_brief)
                .map_err(|e| format!("生成外设简报失败: {}", e))?;
            Some(local_brief)
        } else {
            None
        };
        Ok((archive, brief))
    })();
    if !o.keep_remote {
        remove_remote(d, &remote_dir);
    }
    work.map(|(archive, brief)| Result {
        output_dir: o.output_dir.clone(),
        report: o.output_dir.join("hardware.json"),
        archive,
        brief,
        remote_dir,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_script_has_valid_shell_syntax() {
        let path = std::env::temp_dir().join(format!("ferry-hwprobe-{}.sh", std::process::id()));
        fs::write(&path, SCRIPT).unwrap();
        assert!(Command::new("/bin/sh")
            .arg("-n")
            .arg(&path)
            .status()
            .unwrap()
            .success());
        let _ = fs::remove_file(path);
    }

    #[test]
    fn embedded_script_uses_no_external_dirname_command() {
        assert!(SCRIPT.contains("parent_dir()"));
        assert!(!SCRIPT.contains("dirname \""));
    }

    #[test]
    fn embedded_script_restores_a_system_path_before_collecting() {
        assert!(SCRIPT.contains("PATH=${HWPROBE_PATH:-/system/bin:"));
        assert!(SCRIPT.contains("install_toybox_fallbacks"));
        assert!(SCRIPT.contains("require_core_tools"));
    }

    #[test]
    fn selects_writable_android_temp_candidates_by_transport() {
        assert_eq!(temp_bases(Transport::Ssh, false), &["/tmp"]);
        assert_eq!(
            temp_bases(Transport::Adb, true),
            &["/data/local/tmp", "/sdcard", "/storage/emulated/0"]
        );
        assert_eq!(
            temp_bases(Transport::Ssh, true),
            &[
                "/data/data/org.galexander.sshd/files",
                "/data/local/tmp",
                "/sdcard",
                "/storage/emulated/0",
            ]
        );
    }
}
