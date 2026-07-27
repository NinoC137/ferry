# ferry

> A zero-dependency workbench for moving between your host and embedded Linux or Android targets.

`ferry` installs as a single Rust binary named `fy`. It gives SSH, ADB, and serial-console devices one profile model and one operational vocabulary: discover a board, identify it after an IP change, open a shell, transfer an artifact with verification, recover a tunnel, collect hardware facts, or keep a serial crash recorder running.

**Status:** early-stage, usable for lab and bring-up workflows. The stable automation surface is the `fy` CLI; the browser workbench (`fy ui`) and Tauri desktop workbench are interactive clients built on the same Ferry modules.

| Property | Value |
| --- | --- |
| Host platforms | macOS and Linux |
| Target transports | SSH, ADB, serial console |
| Runtime dependencies | System `ssh`; optional `adb`, `rsync`; system `stty` for serial |
| Rust dependencies | None in the core CLI |
| License | MIT |

## Contents

- [Why Ferry](#why-ferry)
- [Install](#install)
- [Quick start](#quick-start)
- [Core workflows](#core-workflows)
- [Interactive workbenches](#interactive-workbenches)
- [Hardware inventory](#hardware-inventory)
- [Automation and JSON](#automation-and-json)
- [Safety and operational boundaries](#safety-and-operational-boundaries)
- [Project layout](#project-layout)
- [Development](#development)

## Why Ferry

Embedded bring-up normally spans several unrelated tools: OpenSSH for a networked board, `adb` for Android, a serial terminal for boot failures, ad hoc scripts for transfers, and manual routing changes when a target cannot reach the network. Ferry makes those paths interoperable.

It is designed around the moments that consume time in a lab:

| Problem | Ferry workflow | What it provides |
| --- | --- | --- |
| A board changed IP after reflashing | `fy scan`, `fy info` | mDNS/TCP discovery, MAC and fingerprint matching |
| A target only has a serial console | `fy up` | Serial login, network probing, SSH promotion where possible |
| A large image transfer failed near the end | `fy push`, `fy pull` | Resume, prefix validation, integrity verification |
| A serial-only failure happened overnight | `fy bb`, `fy blame` | Continuous recording and panic/OOM incident capture |
| A board needs host network access | `fy share`, `fy usb net` | Proxy tunnel by default; explicit NAT where appropriate |
| A tunnel disappears after reboot | `fy watch`, `fy fwd` | Health checks and forward/share replay |
| A target needs hardware facts captured | `fy hw` | Read-only procfs/sysfs/live-DT inventory and a human brief |
| A script or agent needs reliable output | `fy --json` | Stable structured output and non-interactive failure semantics |

Ferry does not replace `ssh`, `adb`, or your debugger. It composes them into repeatable workflows and keeps the device profile, identity facts, and runtime state in one place.

## Install

### Prerequisites

- Rust stable toolchain
- OpenSSH client, preferably OpenSSH 8.4 or later
- Optional: Android Platform Tools (`adb`)
- Optional: `rsync` for the fastest sync path

Build a release binary:

```bash
git clone <your-fork-or-clone-url> ferry
cd ferry
cargo build --release

# Install into a directory already present in PATH.
install -m755 target/release/fy /usr/local/bin/fy

fy --help
fy doctor
```

For a user-owned or external-volume installation, choose any directory and add it to `PATH`:

```bash
mkdir -p /path/to/applications/bin
install -m755 target/release/fy /path/to/applications/bin/fy
export PATH="/path/to/applications/bin:$PATH"
```

Ferry stores local state under `~/.config/ferry/`:

```text
devices.toml  device profiles
facts/        identity fingerprints and observed facts
state.toml    forwards, shares, black-box and watcher state
```

## Quick start

Create a profile for one target. The transport is a primary path, not a permanent constraint: a serial profile can later be promoted to SSH.

```bash
# SSH target
fy add rk --ssh root@192.168.1.37

# Legacy Dropbear/OpenSSH target
fy add old-board --ssh root@10.0.0.5 --legacy

# One connected ADB device, or specify a serial/IP explicitly
fy add phone --adb

# Serial console
fy add mcu --serial /dev/tty.usbserial-1420 --baud 1500000
```

Alternatively, discover candidates first:

```bash
fy scan
fy scan --add
```

Then use the same device name across workflows:

```bash
fy                       # parallel reachability and saved identity facts
fy sh rk                 # interactive shell
fy sh rk -- uname -a     # one remote command
fy info rk               # identity card
fy push rk ./app /tmp/   # verified, resumable transfer
fy run rk ./app --help   # push, chmod, run, return remote exit code
```

## Core workflows

### Device discovery and identity

`fy scan` combines mDNS, a bounded concurrent TCP scan, SSH banner reads, ARP MAC lookup, ADB enumeration, and serial-port enumeration. It can recognize a saved board after DHCP or reflashing has moved its address.

```bash
fy scan
fy scan --subnet 192.168.2.0/24
fy scan --no-mdns
fy scan --add
```

Use `fy forget <device>` after a board is reflashed and its SSH host key needs to be removed. Use `fy keyup <device>` to install a public key and move away from password authentication.

### Shell, logs, and multi-device execution

```bash
fy sh rk
fy sh rk -- 'systemctl status my-service'
fy log rk
fy log phone
fy top
fy all -- uname -a
fy all rk cam -- 'df -h /'
```

`fy log` selects `journalctl`, syslog, `dmesg`, or `logcat` according to the transport and target. `fy top` samples all networked profiles in parallel.

### Transfer, deploy, and debug

`fy push` and `fy pull` are transfer engines rather than thin `scp` wrappers. They check remote space, resume only after validating the existing prefix, and verify SHA-256 where the target provides a supported hash utility.

```bash
fy push rk ./rootfs.img /tmp/
fy push --all ./app /opt/app --only rk
fy pull rk /var/log/messages ./logs/

fy cp ./firmware.bin rk:/tmp/
fy cp rk:/var/log/messages ./logs/
fy cp rk:/tmp/fw.bin cam:/data/local/tmp/

fy run rk ./app --verbose
fy debug rk ./app --port 3333
fy sync rk ./build /opt/app --exec 'systemctl restart app'
```

Useful transfer controls:

```text
--force       retransmit even when the target appears identical
--no-resume   disable resumable transfer
--no-verify   disable post-transfer verification
```

### Plugins and sysroots

Ferry plugins are local, reviewable packages for host-side workflows that do not belong in the core binary. A package contains a `plugin.toml` manifest and the executable named by its `entry` field. The manifest declares its required transport, host dependencies, arguments, risk category, and a preview of its work. Ferry refuses entrypoint path escapes and installs packages under `~/.config/ferry/plugins/<id>/` (or `$FERRY_HOME/plugins/<id>/`).

```bash
# See installed packages and built-ins.
fy plugin ls

# Install Ferry's maintained sysroot package.
fy plugin install sysroot-sync
fy plugin show sysroot-sync

# Sync an SSH target into a conventional system location. sudo is interactive in the CLI.
fy plugin run sysroot-sync rk -- --dest /opt/sysroot

# A user-writable sysroot needs no sudo. --delete mirrors target-side deletions locally.
fy plugin run sysroot-sync rk -- --dest ~/ferry-sysroots/rk --no-sudo --delete

# Install a reviewed package developed locally.
fy plugin install /path/to/my-ferry-plugin
```

`sysroot-sync` uses the selected SSH profile and runs the equivalent of the following three incremental transfers, preserving Ferry's SSH port, identity file, legacy compatibility, known-host policy, and connection reuse settings:

```text
rsync -av -e "ssh <Ferry SSH options>" user@target:/lib         <sysroot>/
rsync -av -e "ssh <Ferry SSH options>" user@target:/usr/lib     <sysroot>/usr/
rsync -av -e "ssh <Ferry SSH options>" user@target:/usr/include <sysroot>/usr/
```

It reads from the target and writes only the chosen host directory. `--delete` is deliberately opt-in because it can remove local sysroot files. The desktop Plugins workbench uses SSH key authentication only and never passes a saved profile password to a plugin. It defaults to a user-writable destination; selecting sudo for `/opt/sysroot` requires a previously authorized `sudo -v` session because a background desktop command cannot safely prompt for a host password.

### Forwarding and resilient connectivity

```bash
fy fwd rk 8080                  # localhost:8080 -> target:8080
fy fwd rk 8080:80               # localhost:8080 -> target:80
fy fwd rk R:9000:8000           # target:9000 -> host:8000
fy fwd rk D:1080                # local SOCKS5 proxy through SSH
fy fwd ls
fy fwd rm f1

fy watch start
fy watch status
```

SSH forwards use a ControlMaster and can be replayed by `fy watch` after a disconnect. ADB forward/reverse rules are handled through the matching ADB commands.

### Network diagnosis and host-network sharing

```bash
fy net rk
fy net rk --no-speed

# Default: host proxy plus reverse tunnel, no sudo.
fy share rk
fy share rk --upstream auto
fy share rk --persist

# Explicitly opt into NAT for a directly connected SSH target.
fy share rk --nat
fy share rk --off
```

The default share mode exposes a combined HTTP/SOCKS5 proxy through a reverse tunnel. NAT changes host routing/firewall state and is intentionally a separate explicit mode.

### Serial recovery and black box

```bash
fy bb start mcu
fy bb status
fy blame mcu
fy bb stop mcu

fy up mcu
fy up mcu --boot
```

The black box continuously records a serial port and recognizes common kernel panic, Oops, lockup, and OOM signatures. While it runs, `fy sh` attaches through the shared Unix socket instead of taking exclusive ownership of the serial device.

`fy up` is a best-effort promotion workflow: serial login, board/network inspection, USB gadget or DHCP attempts where applicable, SSH reachability, and optional public-key setup. The original serial path remains useful if promotion cannot complete.

### USB networking and local transfer service

```bash
# Host-side USB network setup
fy usb net --share

# Generate or install a target-side configfs gadget script
fy usb gadget --out ferry-gadget.sh --mode ncm
fy usb install rk --autostart

# Serve artifacts to a minimal/recovery target with wget or curl
fy serve ./out --for rk
fy serve ./out --upload ./inbox
```

`ncm` is the recommended gadget mode for macOS/Linux. Use `rndis` only where compatibility requires it.

## Interactive workbenches

### Browser workbench: `fy ui`

```bash
fy ui
fy ui --port 8000 --no-open
```

`fy ui` binds only to `127.0.0.1` by default. Its main area is a real system PTY rendered by xterm.js over a persistent WebSocket, so interactive tools such as `vim`, `top`, completion, control sequences, and long-running commands behave as they do in a normal terminal. The side panel makes Ferry shortcuts visible while injecting the resulting command into that same terminal.

### Tauri desktop workbench

The repository also contains a native desktop client in `desktop/`. It reuses Ferry device modules and the same PTY/WebSocket transport, with fleet overview, discovery, device profile drafts, xterm sessions, transfer/forward/top/black-box panels, guarded network/recovery workflows, and a Plugins workbench for installing local packages and running sysroot synchronization with a visible preflight plan.

```bash
cd desktop
npm ci
npm run tauri dev
```

The desktop client needs the normal Tauri system prerequisites in addition to the Rust and Node toolchains. The CLI remains the preferred entry point for scripts and CI.

## Hardware inventory

`fy hw` collects a read-only target snapshot from procfs, sysfs, and the live device tree. It does not load modules, scan I2C/SPI buses, write sysfs, or alter target security policy.

```bash
fy hw rk --out ./rk-hardware
fy hw rk --out ./rk-hardware --max-dt-nodes 1024
fy hw rk --out ./rk-hardware --no-bundle

# Recreate a readable peripheral brief offline.
fy hw brief ./rk-hardware/hardware.json
fy hw brief ./rk-hardware/hardware.json --out ./peripherals.md
```

The report directory contains `hardware.json`, `peripherals.md`, and, when the target supports `tar`, a `device-tree.tar` archive. Hardware identifiers are redacted by default; opt in only when appropriate for your environment.

## Automation and JSON

Most non-streaming commands expose a machine-readable contract:

```bash
fy --json ls
fy --json sh rk -- 'uname -a'
fy --json push rk ./firmware.bin /tmp/
fy --json net rk
fy help --json
```

In JSON mode:

- stdout contains one JSON document only; progress and diagnostics go to stderr;
- prompts are disabled; an ambiguous or unsafe action returns a structured failure with a hint;
- `FERRY_JSON=1` enables JSON mode for a process environment;
- stable exit codes are documented by `fy help --json`;
- streaming commands such as `ui`, `serve`, `log`, and `top` reject JSON mode instead of mixing terminal output into JSON.

## Safety and operational boundaries

Ferry intentionally exposes powerful host and target operations. Review the command before execution, especially on shared lab networks.

- Use `fy -n` or `fy --dry-run` before any operation that changes device, routing, firewall, or service state.
- `fy share --nat` and `fy usb net --share` may require `sudo` and alter host forwarding/firewall/interface configuration.
- `fy up`, gadget installation, and persistent proxy settings can modify target network or boot-time configuration.
- Ferry may store a password in `~/.config/ferry/devices.toml`; it attempts to apply mode `0600`. Prefer `fy keyup` and key-based authentication.
- `--legacy` enables retired SSH algorithms for old isolated targets. Do not use it as a general compatibility default.
- Scan only networks you are authorized to probe.

## Project layout

```text
ferry/
├── Cargo.toml
├── src/
│   ├── main.rs          CLI dispatch and command parsing
│   ├── lib.rs           reusable Ferry modules for the desktop client
│   ├── config.rs        profiles, fingerprint facts, runtime state
│   ├── sshx.rs          SSH options, ControlMaster, askpass, transfer helpers
│   ├── adbx.rs          ADB transport and Wi-Fi transition
│   ├── serialx.rs       serial configuration and console/expect engine
│   ├── xfer.rs          resumable verified transfer and device-to-device copy
│   ├── scan.rs          mDNS/TCP discovery and fingerprint claim
│   ├── up.rs            serial-to-network promotion workflow
│   ├── blackbox.rs      persistent serial recorder and incident capture
│   ├── share.rs         proxy and NAT sharing
│   ├── usbnet.rs        USB network and gadget support
│   ├── netdiag.rs       network diagnosis and bandwidth checks
│   ├── ui.rs            browser workbench and PTY/WebSocket bridge
│   └── hwprobe.rs       read-only hardware inventory and brief generation
├── assets/
│   ├── ferry-gadget.sh  target-side USB gadget script
│   ├── hwprobe.sh       target-side hardware collector
│   └── plugins/         bundled local plugin packages (installed explicitly)
└── desktop/             Tauri + React + xterm.js desktop workbench
```

## Development

```bash
# Core library and CLI tests
cargo test -p ferry --lib

# Build the release CLI
cargo build --release

# Desktop client checks
cd desktop
npm ci
npm run build
cd ..
cargo check -p ferry-desktop
```

When contributing, keep changes transport-aware, preserve the JSON contract for automation, and document side effects and rollback behavior for new privileged operations. Small, focused changes with a reproducible test path are preferred.

## License

MIT, as declared in `Cargo.toml`.
