# Ferry

> One workbench for the trip between your host and an embedded Linux or Android target.

**Ferry** is a zero-dependency Rust CLI (`fy`) and optional native desktop workbench for lab bring-up. It gives SSH, ADB, and serial-console targets one profile model and one operational vocabulary: discover a board, recognise it after an address change, open a shell, move artifacts with verification, recover connectivity, collect hardware facts, or keep a serial crash recorder running.

[中文文档](README.zh-CN.md) · [Documentation](docs/README.md) · [Architecture](docs/architecture.md) · [Operations guide](docs/operations.md) · [Contributing](CONTRIBUTING.md) · [Security](SECURITY.md)

## Status

Ferry is early-stage but intended for real lab and bring-up work. The `fy` CLI is the stable automation surface. The browser workbench (`fy ui`) and native Tauri app are interactive clients over the same Ferry modules.

## Why Ferry?

Embedded work tends to fracture across SSH, ADB, serial tools, ad-hoc copy scripts, network setup, and notes that do not survive a board reflash. Ferry keeps the device profile, observed identity, and operational workflows together.

| Situation | Ferry workflow | What it gives you |
| --- | --- | --- |
| A board moved after DHCP or reflash | `fy scan`, `fy info` | mDNS/TCP discovery and MAC/fingerprint recognition |
| Only a serial console is alive | `fy up`, `fy bb` | Recovery path and persistent crash recording |
| A large image transfer stopped | `fy push`, `fy pull` | Resume, prefix validation, and integrity verification |
| A target needs host connectivity | `fy share`, `fy usb net` | Proxy tunnel by default; explicit NAT when needed |
| A connection keeps dropping | `fy fwd`, `fy watch` | Managed forwards and replay after reconnect |
| You need evidence of the hardware | `fy hw` | Read-only procfs/sysfs/live-device-tree inventory |
| A script or agent calls Ferry | `fy --json` | One JSON document on stdout and non-interactive failures |

## Highlights

- **Three transports, one profile:** SSH, ADB, and serial consoles use the same device name across operations.
- **Identity-aware discovery:** verified SSH banners and authorised network ADB endpoints only; saved facts help recognise boards after an IP or USB-port change.
- **Reliable transfer and deploy:** resumable, verified transfers; board-to-board copy through the host; `run`, `debug`, and `sync` workflows.
- **Connectivity and recovery:** forwards, connection watching, proxy/NAT sharing, USB networking, serial-to-SSH promotion, and serial black-box recording.
- **Hardware evidence:** a read-only collector produces `hardware.json`, an optional `peripherals.md`, and a raw device-tree archive when available.
- **Reviewable extensions:** local plugin packages declare their requirements, risk, arguments, and a dry-run preview before execution.
- **Two interactive workbenches:** a local browser PTY and a Tauri desktop app with fleet, discovery, terminal, operations, and plugin views.

## Install

### Prerequisites

- Rust stable (the desktop crate requires Rust 1.77.2 or newer)
- OpenSSH client; OpenSSH 8.4 or newer is recommended
- Optional: Android Platform Tools (`adb`)
- Optional: `rsync` for the fastest sync path

Build the CLI from a checkout:

```bash
git clone https://github.com/NinoC137/ferry.git
cd ferry
cargo build --release

# Install into a directory already in PATH.
install -m755 target/release/fy /usr/local/bin/fy

fy --version
fy doctor
```

Ferry stores local state in `~/.config/ferry/`:

```text
devices.toml  saved device profiles
facts/        observed identity fingerprints and hardware facts
state.toml    forwards, shares, black boxes, and watcher state
plugins/      installed local extension packages
```

### Native desktop app

The optional desktop workbench lives in [`desktop/`](desktop). It needs the normal Tauri macOS prerequisites in addition to Rust and Node.js.

```bash
cd desktop
npm ci
npm run tauri dev       # development
npm run tauri build     # release bundle
```

On macOS, the release bundle is created at `target/release/bundle/macos/Ferry Desktop.app`. The CLI remains the preferred interface for scripts and CI.

## Quick start

Create a profile for a target. A transport is the current path to a device, not a permanent constraint: a serial device can later be promoted to SSH.

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

Discover first when you do not know the endpoint:

```bash
fy scan
fy scan --add
```

Then use the same name across workflows:

```bash
fy                         # reachability and saved identity facts
fy sh rk                    # interactive shell
fy sh rk -- uname -a        # one remote command
fy info rk                  # identity card
fy push rk ./app /tmp/      # verified, resumable upload
fy run rk ./app --help      # upload, chmod, run, return remote exit code
```

## Common workflows

| Goal | Start here |
| --- | --- |
| Find and identify a board | [`fy scan`, `fy info`](docs/operations.md#discover-and-identify) |
| Shell, logs, and parallel commands | [`fy sh`, `fy log`, `fy all`](docs/operations.md#operate-targets) |
| Transfer, deploy, or debug | [`fy push`, `fy run`, `fy debug`, `fy sync`](docs/operations.md#transfer-and-deploy) |
| Forward ports or lend a network | [`fy fwd`, `fy share`, `fy net`](docs/operations.md#connectivity-and-networking) |
| Recover a serial-only board | [`fy bb`, `fy blame`, `fy up`](docs/operations.md#serial-recovery) |
| Collect a hardware report | [`fy hw`](docs/operations.md#hardware-inventory) |
| Add an audited local workflow | [`fy plugin`](docs/operations.md#local-plugins) |
| Build a machine integration | [`fy --json`, `fy help --json`](docs/operations.md#automation-and-json) |

## Interactive workbenches

### Browser workbench

```bash
fy ui
fy ui --port 8000 --no-open
```

`fy ui` binds to `127.0.0.1` by default. Its main surface is a real system PTY rendered with xterm.js over a persistent WebSocket, so `vim`, completion, Ctrl-C, full-screen tools, and long-running commands behave as they do in a normal terminal.

### Desktop workbench

The Tauri app offers a fleet overview, verified network discovery, editable profile drafts, xterm sessions, transfer/forward/top/black-box controls, guarded network and recovery workflows, and a plugin workbench. High-impact workflows expose a preflight plan before execution. See [the architecture guide](docs/architecture.md#interactive-clients) for the client boundary and security constraints.

## Documentation

| Document | Contents |
| --- | --- |
| [Documentation index](docs/README.md) | Navigation for English and Chinese materials |
| [Operations guide](docs/operations.md) | Command-oriented workflows, safety notes, and plugins |
| [Architecture](docs/architecture.md) | Modules, state model, transport boundaries, and clients |
| [中文操作指南](docs/operations.zh-CN.md) | 中文命令与操作说明 |
| [中文架构说明](docs/architecture.zh-CN.md) | 中文架构、状态与客户端说明 |
| [`fy --help`](#quick-start) | The installed version's authoritative command list |

## Safety

Ferry can alter target and host state. Read the plan and use `fy --dry-run` before changing device, routing, firewall, or service configuration.

- `fy share --nat` and `fy usb net --share` can require `sudo` and change host forwarding, firewall, or interface state.
- `fy up`, USB-gadget installation, and persistent proxy settings may change target network or boot-time configuration.
- A saved profile may contain a password in `~/.config/ferry/devices.toml`; Ferry attempts mode `0600`. Prefer `fy keyup` and key-based authentication.
- `--legacy` enables retired SSH algorithms for isolated legacy targets only.
- Scan only networks you are authorised to probe.

## Development

```bash
# Core library and CLI tests
cargo test -p ferry --lib

# Build the release CLI
cargo build --release

# Desktop checks
cd desktop
npm ci
npm run build
cd ..
cargo check -p ferry-desktop
```

See [CONTRIBUTING.md](CONTRIBUTING.md) for development expectations and [SECURITY.md](SECURITY.md) for responsible disclosure.

## License

Ferry is released under the [MIT License](LICENSE).
