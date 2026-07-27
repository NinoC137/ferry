//! One-shot deployment and recovery of the embedded hwprobe.sh agent.

use crate::adbx;
use crate::config::{Device, Transport};
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
    pub keep_remote: bool,
    pub include_identifiers: bool,
    pub max_dt_nodes: Option<u32>,
}

pub struct Result {
    pub output_dir: PathBuf,
    pub report: PathBuf,
    pub archive: Option<PathBuf>,
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

fn target_dir(d: &Device) -> String {
    let base = if d.transport == Transport::Adb {
        "/data/local/tmp"
    } else {
        "/tmp"
    };
    format!(
        "{}/ferry-hwprobe-{}-{}",
        base,
        std::process::id(),
        crate::util::now_epoch()
    )
}

fn upload(d: &Device, remote_dir: &str, remote_script: &str) -> std::io::Result<()> {
    match d.transport {
        Transport::Ssh => {
            let mut args = vec!["ssh".to_string()];
            args.extend(sshx::base_opts(d));
            args.push(sshx::target(d));
            args.push(format!(
                "mkdir -p {d} && cat > {s} && chmod 700 {s}",
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
            let out = remote_capture(d, &format!("chmod 700 {}", shell_quote(remote_script)))?;
            if out.status == 0 {
                Ok(())
            } else {
                Err(std::io::Error::new(std::io::ErrorKind::Other, out.stderr))
            }
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
    let remote_dir = target_dir(d);
    let script = format!("{}/hwprobe.sh", remote_dir);
    let report = format!("{}/hardware.json", remote_dir);
    let work = (|| -> std::result::Result<Option<PathBuf>, String> {
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
        if remote_capture(d, &format!("test -f {}", shell_quote(&remote_archive)))
            .map_err(|e| e.to_string())?
            .status
            == 0
        {
            let local_archive = o.output_dir.join("device-tree.tar");
            download(d, &remote_archive, &local_archive)
                .map_err(|e| format!("回收 device-tree.tar 失败: {}", e))?;
            Ok(Some(local_archive))
        } else {
            Ok(None)
        }
    })();
    if !o.keep_remote {
        remove_remote(d, &remote_dir);
    }
    work.map(|archive| Result {
        output_dir: o.output_dir.clone(),
        report: o.output_dir.join("hardware.json"),
        archive,
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
}
