//! ssh 封装：连接复用(ControlMaster)、独立 known_hosts、老设备算法兼容、
//! 免 sshpass 的密码注入(自身充当 SSH_ASKPASS)、push/pull 多级回退。

use crate::config::Device;
use crate::util::*;
use std::io::Write as _;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

static KEYUP_ASKPASS_ID: AtomicU64 = AtomicU64::new(1);

/// 公共 ssh 选项。lab 板子重刷是常态：独立 known_hosts + accept-new，
/// 变了指纹用 `fy forget` 一键清除。
pub fn base_opts(d: &Device) -> Vec<String> {
    let mut o: Vec<String> = vec![];
    let kh = known_hosts().display().to_string();
    let _ = ensure_dir(&cfg_dir());
    let _ = ensure_dir(&cm_dir());
    for s in [
        "-o",
        &format!("UserKnownHostsFile={}", kh),
        "-o",
        "StrictHostKeyChecking=accept-new",
        "-o",
        "ConnectTimeout=6",
        "-o",
        "ServerAliveInterval=15",
        "-o",
        "ServerAliveCountMax=3",
        "-o",
        "LogLevel=ERROR",
        "-o",
        "ControlMaster=auto",
        "-o",
        &format!("ControlPath={}/%C", cm_dir().display()),
        "-o",
        "ControlPersist=600",
    ] {
        o.push(s.to_string());
    }
    if d.legacy {
        // 老 dropbear / 旧 OpenSSH：把淘汰算法加回白名单
        for s in [
            "-o",
            "HostKeyAlgorithms=+ssh-rsa,ssh-dss",
            "-o",
            "PubkeyAcceptedAlgorithms=+ssh-rsa",
            "-o",
            "KexAlgorithms=+diffie-hellman-group1-sha1,diffie-hellman-group14-sha1",
            "-o",
            "Ciphers=+aes128-cbc,aes256-cbc,3des-cbc",
            "-o",
            "MACs=+hmac-sha1",
        ] {
            o.push(s.to_string());
        }
    }
    if let Some(k) = &d.key {
        o.push("-i".into());
        o.push(k.clone());
    }
    o.push("-p".into());
    o.push(d.port.to_string());
    o
}

/// scp 用的选项（-p 变 -P）。
fn scp_opts(d: &Device) -> Vec<String> {
    let mut o = base_opts(d);
    for i in 0..o.len() {
        if o[i] == "-p" {
            o[i] = "-P".into();
        }
    }
    o
}

pub fn target(d: &Device) -> String {
    format!("{}@{}", d.user, d.host)
}

/// 密码注入环境：让 ssh 回调 `fy __askpass`，从档案里取密码。
/// 免装 sshpass；需要 OpenSSH >= 8.4（macOS Ventura+ / 主流发行版都满足）。
pub fn askpass_env(d: &Device) -> Vec<(String, String)> {
    if d.password.is_none() {
        return vec![];
    }
    vec![
        ("SSH_ASKPASS".into(), self_exe().display().to_string()),
        ("SSH_ASKPASS_REQUIRE".into(), "force".into()),
        (
            "DISPLAY".into(),
            std::env::var("DISPLAY").unwrap_or_else(|_| ":0".into()),
        ),
        ("FERRY_ASKPASS_DEV".into(), d.name.clone()),
    ]
}

/// `fy __askpass` 入口：ssh 把提示词作为 argv 传来，我们输出密码。
pub fn askpass_main(_prompt: &str) -> i32 {
    let dev = std::env::var("FERRY_ASKPASS_DEV").unwrap_or_default();
    let cfg = crate::config::Config::load();
    if let Some(d) = cfg.devices.get(&dev) {
        if let Some(p) = &d.password {
            println!("{}", p);
            return 0;
        }
    }
    1
}

fn ssh_argv(d: &Device, extra: &[String], remote_cmd: Option<&str>) -> Vec<String> {
    let mut a = vec!["ssh".to_string()];
    a.extend(base_opts(d));
    a.extend(extra.iter().cloned());
    a.push(target(d));
    if let Some(c) = remote_cmd {
        a.push(c.to_string());
    }
    a
}

/// 交互 shell（exec 直接替换进程，tty 原生）。
pub fn shell(d: &Device) -> std::io::Result<i32> {
    run_exec(&ssh_argv(d, &argv(&["-t"]), None), &askpass_env(d))
}

/// 远端执行一条命令，输出直通。
pub fn exec_inherit(d: &Device, cmd: &str, tty: bool) -> std::io::Result<i32> {
    let extra = if tty { argv(&["-t"]) } else { vec![] };
    run_inherit(&ssh_argv(d, &extra, Some(cmd)), &askpass_env(d))
}

/// 远端执行并捕获输出。
pub fn exec_capture(d: &Device, cmd: &str) -> std::io::Result<Output> {
    run_capture(&ssh_argv(d, &[], Some(cmd)), &askpass_env(d))
}

/// master 连接控制：check / exit / forward / cancel。
pub fn master_ctl(d: &Device, op: &str, fwd_args: &[String]) -> std::io::Result<Output> {
    let mut a = vec!["ssh".to_string()];
    a.extend(base_opts(d));
    a.push("-O".into());
    a.push(op.into());
    a.extend(fwd_args.iter().cloned());
    a.push(target(d));
    run_capture(&a, &askpass_env(d))
}

/// 确保 master 存活（转发要挂在它身上）。
pub fn ensure_master(d: &Device) -> std::io::Result<()> {
    if dry() {
        // dry 模式也展示将要发生什么
        let _ = master_ctl(d, "check", &[]);
        return Ok(());
    }
    if master_ctl(d, "check", &[])?.status == 0 {
        return Ok(());
    }
    // 起一个后台 master（-N 不执行命令）
    let mut a = vec!["ssh".to_string()];
    a.extend(base_opts(d));
    a.push("-fN".into());
    a.push(target(d));
    let out = run_capture(&a, &askpass_env(d))?;
    if out.status != 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            format!("无法建立 ssh 连接: {}", out.stderr.trim()),
        ));
    }
    Ok(())
}

/// push：scp → scp -O → tar 管道 三级回退（busybox/dropbear 味的板子总有一款适合）。
pub fn push(d: &Device, local: &Path, remote: &str) -> std::io::Result<bool> {
    let mut a = vec!["scp".to_string()];
    a.extend(scp_opts(d));
    if local.is_dir() {
        a.push("-r".into());
    }
    a.push(local.display().to_string());
    a.push(format!("{}:{}", target(d), remote));
    let st = run_inherit(&a, &askpass_env(d))?;
    if st == 0 {
        return Ok(true);
    }
    warn("scp 失败，试 scp -O（老 sftp-server 不在板上时常见）...");
    let mut a2 = vec!["scp".to_string(), "-O".to_string()];
    a2.extend(a[1..].iter().cloned());
    if run_inherit(&a2, &askpass_env(d))? == 0 {
        return Ok(true);
    }
    warn("scp -O 也失败，改走 tar 管道（只要板子有 tar/busybox 就能过）...");
    tar_push(d, local, remote)
}

/// tar 管道推送：本地 tar c | ssh "cd dest && tar x"。
pub fn tar_push(d: &Device, local: &Path, remote: &str) -> std::io::Result<bool> {
    let parent = local
        .parent()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| ".".into());
    let base = local
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| ".".into());
    let remote_cmd = format!(
        "mkdir -p {} && cd {} && tar xf -",
        shell_quote(remote),
        shell_quote(remote)
    );
    let mut ssh_part = vec!["ssh".to_string()];
    ssh_part.extend(base_opts(d));
    ssh_part.push(target(d));
    ssh_part.push(remote_cmd);
    let line = format!(
        "tar cf - -C {} {} | {}",
        shell_quote(&parent),
        shell_quote(&base),
        render_cmd(&ssh_part)
    );
    let st = run_inherit(&argv(&["/bin/sh", "-c", &line]), &askpass_env(d))?;
    Ok(st == 0)
}

/// pull：scp → scp -O → tar 管道。
pub fn pull(d: &Device, remote: &str, local: &Path) -> std::io::Result<bool> {
    let mut a = vec!["scp".to_string()];
    a.extend(scp_opts(d));
    a.push("-r".into());
    a.push(format!("{}:{}", target(d), remote));
    a.push(local.display().to_string());
    if run_inherit(&a, &askpass_env(d))? == 0 {
        return Ok(true);
    }
    warn("scp 失败，试 scp -O ...");
    let mut a2 = vec!["scp".to_string(), "-O".to_string()];
    a2.extend(a[1..].iter().cloned());
    if run_inherit(&a2, &askpass_env(d))? == 0 {
        return Ok(true);
    }
    warn("scp -O 也失败，改走 tar 管道 ...");
    let rdir = if remote.ends_with('/') {
        remote.trim_end_matches('/')
    } else {
        remote
    };
    let (rparent, rbase) = match rdir.rfind('/') {
        Some(i) if i > 0 => (&rdir[..i], &rdir[i + 1..]),
        _ => ("/", rdir.trim_start_matches('/')),
    };
    let mut ssh_part = vec!["ssh".to_string()];
    ssh_part.extend(base_opts(d));
    ssh_part.push(target(d));
    ssh_part.push(format!(
        "cd {} && tar cf - {}",
        shell_quote(rparent),
        shell_quote(rbase)
    ));
    let line = format!(
        "{} | tar xf - -C {}",
        render_cmd(&ssh_part),
        shell_quote(&local.display().to_string())
    );
    let st = run_inherit(&argv(&["/bin/sh", "-c", &line]), &askpass_env(d))?;
    Ok(st == 0)
}

/// 免密：把本机公钥装到板子 authorized_keys（自动生成密钥、兼容 dropbear 路径）。
pub fn keyup(d: &Device) -> std::io::Result<()> {
    keyup_with_password(d, d.password.as_deref())
}

/// Install a public key using an explicitly supplied, non-persistent password.
/// GUI callers use this path because their executable is not the `fy` askpass
/// callback binary used by the CLI.
pub fn keyup_with_password(d: &Device, password: Option<&str>) -> std::io::Result<()> {
    let home = crate::util::home();
    let candidates = ["id_ed25519.pub", "id_rsa.pub", "id_ecdsa.pub"];
    let mut pubkey = String::new();
    for c in candidates {
        let p = home.join(".ssh").join(c);
        if p.exists() {
            pubkey = slurp(&p).trim().to_string();
            break;
        }
    }
    if pubkey.is_empty() {
        info("本机还没有 ssh 密钥，生成一个 ed25519 ...");
        let st = run_inherit(
            &argv(&[
                "ssh-keygen",
                "-t",
                "ed25519",
                "-N",
                "",
                "-f",
                &home.join(".ssh/id_ed25519").display().to_string(),
            ]),
            &[],
        )?;
        if st != 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                "ssh-keygen 失败",
            ));
        }
        pubkey = slurp(&home.join(".ssh/id_ed25519.pub")).trim().to_string();
    }
    // 同时覆盖 OpenSSH 与 dropbear 的常见路径；grep 防重复
    let cmd = format!(
        "k='{}'; mkdir -p ~/.ssh && touch ~/.ssh/authorized_keys && chmod 700 ~/.ssh && chmod 600 ~/.ssh/authorized_keys; \
         grep -qF \"$k\" ~/.ssh/authorized_keys 2>/dev/null || echo \"$k\" >> ~/.ssh/authorized_keys; \
         if [ -d /etc/dropbear ]; then touch /etc/dropbear/authorized_keys; \
         grep -qF \"$k\" /etc/dropbear/authorized_keys 2>/dev/null || echo \"$k\" >> /etc/dropbear/authorized_keys; \
         chmod 600 /etc/dropbear/authorized_keys; fi; echo FERRY_KEY_OK",
        pubkey.replace('\'', "'\\''")
    );
    let out = if let Some(password) = password.filter(|password| !password.is_empty()) {
        exec_capture_with_password(d, &cmd, password)?
    } else {
        exec_capture(d, &cmd)?
    };
    if dry() {
        return Ok(());
    }
    if out.stdout.contains("FERRY_KEY_OK") {
        ok(&format!("公钥已装入 {}，之后连接免密。", d.name));
        Ok(())
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            format!("装公钥失败: {}", out.stderr.trim()),
        ))
    }
}

/// Confirm that authentication succeeds without a password prompt.
pub fn verify_key_auth(d: &Device) -> std::io::Result<()> {
    let extra = argv(&[
        "-o",
        "BatchMode=yes",
        "-o",
        "PasswordAuthentication=no",
        "-o",
        "KbdInteractiveAuthentication=no",
    ]);
    let out = run_capture(&ssh_argv(d, &extra, Some("true")), &[])?;
    if out.status == 0 {
        Ok(())
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!("public-key verification failed: {}", out.stderr.trim()),
        ))
    }
}

fn exec_capture_with_password(d: &Device, command: &str, password: &str) -> std::io::Result<Output> {
    use std::os::unix::fs::PermissionsExt;

    let script = std::env::temp_dir().join(format!(
        "ferry-keyup-askpass-{}-{}",
        std::process::id(),
        KEYUP_ASKPASS_ID.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::write(&script, "#!/bin/sh\nprintf '%s\\n' \"$FERRY_KEYUP_PASSWORD\"\n")?;
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700))?;
    let env = vec![
        ("SSH_ASKPASS".into(), script.display().to_string()),
        ("SSH_ASKPASS_REQUIRE".into(), "force".into()),
        ("DISPLAY".into(), ":0".into()),
        ("FERRY_KEYUP_PASSWORD".into(), password.into()),
    ];
    let result = run_capture(&ssh_argv(d, &[], Some(command)), &env);
    let _ = std::fs::remove_file(script);
    result
}

/// 板子重刷后清除 host key 记录。
pub fn forget(d: &Device) {
    for host in [d.host.clone(), format!("[{}]:{}", d.host, d.port)] {
        let _ = run_capture(
            &argv(&[
                "ssh-keygen",
                "-R",
                &host,
                "-f",
                &known_hosts().display().to_string(),
            ]),
            &[],
        );
    }
    // 顺手断掉旧 master
    let dd = d.clone();
    let _ = master_ctl(&dd, "exit", &[]);
    ok(&format!(
        "已忘记 {} 的 host key（重刷后首连自动重新记录）。",
        d.name
    ));
}

/// 写入一段内容到远端文件（通过 stdin 管道，适合小文件/脚本）。
pub fn write_remote_file(
    d: &Device,
    remote_path: &str,
    content: &str,
    mode: &str,
) -> std::io::Result<bool> {
    if dry() {
        let mut ssh_part = vec!["ssh".to_string()];
        ssh_part.extend(base_opts(d));
        ssh_part.push(target(d));
        ssh_part.push(format!(
            "cat > {} && chmod {} {}",
            remote_path, mode, remote_path
        ));
        println!(
            "{} (stdin<<content) {}",
            magenta("DRY→"),
            render_cmd(&ssh_part)
        );
        return Ok(true);
    }
    let mut a = vec!["ssh".to_string()];
    a.extend(base_opts(d));
    a.push(target(d));
    a.push(format!(
        "cat > {p} && chmod {m} {p}",
        p = shell_quote(remote_path),
        m = mode
    ));
    let mut cmd = std::process::Command::new(&a[0]);
    cmd.args(&a[1..]);
    for (k, v) in askpass_env(d) {
        cmd.env(k, v);
    }
    cmd.stdin(std::process::Stdio::piped());
    let mut child = cmd.spawn()?;
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(content.as_bytes())?;
    let st = child.wait()?;
    Ok(st.success())
}
