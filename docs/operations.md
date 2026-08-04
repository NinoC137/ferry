# Operations guide

This guide groups Ferry commands by the job they perform. Use `fy --help` for the exact syntax in your installed version, and use `fy --dry-run` before an operation that can alter host or target state.

## Discover and identify

```bash
fy scan
fy scan --subnet 192.0.2.0/24
fy scan --ports 2222,2200
fy scan --no-mdns
fy scan --add

fy info rk
fy forget rk
```

`fy scan` combines mDNS, a bounded concurrent TCP scan, SSH banner verification, ARP MAC lookup, ADB enumeration, and serial-port enumeration. Network candidates are deliberately conservative: a host must present a valid SSH protocol banner, or be an already-authorised network ADB endpoint, before it is offered as a profile.

Ferry saves observed facts and can use them to recognise a known target after DHCP or reflash changes its address. Use `fy forget <device>` when the board was reflashed and the saved SSH host key should be removed. Do not scan networks you are not authorised to probe.

## Operate targets

```bash
fy sh rk
fy sh rk -- 'systemctl status my-service'
fy log rk
fy log phone
fy top
fy all -- uname -a
fy all rk cam -- 'df -h /'
```

`fy sh` opens an interactive session or runs one command. `fy log` selects `journalctl`, syslog, `dmesg`, or `logcat` for the selected transport. `fy top` samples compatible target health information in parallel, while `fy all` executes a command against matching profiles.

Set up key-based SSH as soon as practical:

```bash
fy keyup rk
```

`fy keyup` can generate and install a public key while respecting the selected profile's compatibility settings. It is preferable to saving a password in a local profile.

## Transfer and deploy

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

`push` and `pull` validate an existing prefix before resuming, check available remote space where possible, and verify SHA-256 when the target has a compatible hash utility. `cp` supports local-to-device, device-to-local, and device-to-device transfers; board-to-board data passes through the host but is not written as an intermediate host file.

Useful transfer controls:

| Flag | Meaning |
| --- | --- |
| `--force` | retransmit even when the target looks identical |
| `--no-resume` | disable resumable transfer |
| `--no-verify` | disable post-transfer verification |
| `--scp` | use the legacy scp/tar path as an escape hatch; it has no Ferry resume/verify semantics |

`fy run` transfers, marks executable, runs, and returns the remote exit code. `fy debug` starts a `gdbserver` workflow with the required forward and reports the connection command. `fy sync` chooses an appropriate rsync/tar/ADB path and can execute a command after syncing.

## Connectivity and networking

```bash
fy fwd rk 8080
fy fwd rk 8080:80
fy fwd rk R:9000:8000
fy fwd rk D:1080
fy fwd ls
fy fwd rm f1

fy watch start
fy watch status

fy net rk
fy net rk --no-speed
```

For SSH profiles, Ferry uses connection reuse and can replay forwards through `fy watch` after a disconnect. Matching ADB forward/reverse rules use ADB's native forwarding commands.

```bash
# Default: reverse tunnel to a combined HTTP/SOCKS5 host proxy; no sudo.
fy share rk
fy share rk --upstream auto
fy share rk --persist

# Explicitly request host NAT for a directly connected SSH target.
fy share rk --nat
fy share rk --off

# Host-side USB network setup and optional sharing.
fy usb net --share
```

The default `share` mode exposes a host proxy through a reverse tunnel. `--nat` is deliberately separate because it can change host routing and firewall state. `fy net` measures and reports latency, jitter, packet loss, MTU, routes, DNS, external connectivity, and optional real throughput.

## Serial recovery

```bash
fy bb start mcu
fy bb status
fy blame mcu
fy bb stop mcu

fy up mcu
fy up mcu --boot
```

The serial black box continuously records the console and recognises common kernel panic, Oops, lockup, and OOM signatures. When it is active, `fy sh` attaches through the shared Unix socket instead of competing for the serial port.

`fy up` is a best-effort promotion workflow: it attempts serial login, target and network inspection, USB gadget or DHCP setup where applicable, SSH reachability, and optional public-key setup. The original serial route remains the recovery path if promotion does not complete. `--boot` permits the workflow to proceed through a bootloader boundary; review its plan before use.

## USB gadget and local artifact service

```bash
fy usb gadget --out ferry-gadget.sh --mode ncm
fy usb install rk --autostart

fy serve ./out --for rk
fy serve ./out --upload ./inbox
```

`ncm` is the recommended USB gadget mode for macOS and Linux; use `rndis` only for a compatibility requirement. `fy usb install` can place the generated gadget configuration on a target and register it for boot-time use, so it is a target-changing operation. `fy serve` provides a local artifact service for minimal/recovery targets using `wget` or `curl`; `--upload` enables reverse file collection.

## Hardware inventory

```bash
fy hw rk --out ./rk-hardware
fy hw rk --out ./rk-hardware --max-dt-nodes 1024
fy hw rk --out ./rk-hardware --no-bundle

fy hw brief ./rk-hardware/hardware.json
fy hw brief ./rk-hardware/hardware.json --out ./peripherals.md
```

`fy hw` is designed as a read-only collection: it reads procfs, sysfs, and the live device tree; it does not load modules, scan I2C/SPI buses, write sysfs, or change target security policy. A report contains `hardware.json`, an optional human-readable `peripherals.md`, and, when supported by target `tar`, `device-tree.tar`.

On Android, Ferry selects an appropriate writable deployment directory rather than assuming `/tmp` is available. Hardware identifiers are redacted by default; only opt in to collecting identifying data when it is appropriate for the environment in which the report will be stored.

## Local plugins

Plugins are reviewable local packages for host-side workflows that do not belong in the core binary. A package contains `plugin.toml` and the executable declared by its `entry` field. The manifest declares transport requirements, host dependencies, arguments, risk, summary, and a preview. Ferry rejects entrypoint path escapes and installs packages under `~/.config/ferry/plugins/<id>/` (or `$FERRY_HOME/plugins/<id>/`).

```bash
fy plugin ls

fy plugin install sysroot-sync
fy plugin show sysroot-sync
fy plugin run sysroot-sync rk -- --dest ~/ferry-sysroots/rk --no-sudo

fy plugin install device-tree-pull
fy plugin run device-tree-pull rk -- --out ./rk-hardware

fy plugin install /path/to/my-ferry-plugin
```

The bundled **Sysroot Sync** package mirrors `/lib`, `/usr/lib`, and `/usr/include` from an SSH target while preserving Ferry's profile-specific SSH options. Its `--delete` option can remove local sysroot files, so it is opt-in.

The bundled **Device Tree Pull** package uses Ferry's native read-only collector to recover `device-tree.tar`, `hardware.json`, and optionally `peripherals.md`. It supports SSH and authorised ADB profiles, requires a new or empty output directory, and removes its target temporary directory after recovery. Read plugin source before installing any package, including local packages supplied by others.

The desktop plugin workbench intentionally uses SSH keys only: a saved profile password is never passed into a background plugin command. A desktop `sudo` destination must have a previously authorised `sudo -v` session, or you should select a user-writable directory.

## Automation and JSON

```bash
fy --json ls
fy --json sh rk -- 'uname -a'
fy --json push rk ./firmware.bin /tmp/
fy --json net rk
fy help --json
```

For supported commands, JSON mode guarantees one JSON document on stdout. Progress and diagnostics go to stderr; prompts are disabled; ambiguous or unsafe actions return structured failures with hints. `FERRY_JSON=1` enables JSON mode for a process environment.

The command list and stable exit-code documentation are available through `fy help --json`. Streaming commands such as `ui`, `serve`, `log`, and `top` reject JSON mode rather than mix terminal output with machine-readable data.
