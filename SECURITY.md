# Security policy

## Supported versions

Security fixes are made against the current development branch. Ferry is pre-1.0; users should build or update from the latest maintained source before reporting that an issue affects a supported version.

## Reporting a vulnerability

Please do **not** open a public issue for a suspected vulnerability. Use [GitHub's private security advisory form](https://github.com/NinoC137/ferry/security/advisories/new) and include:

- a concise description and impact;
- affected Ferry revision and host OS;
- reproduction steps or a minimal proof of concept;
- whether the issue needs a target device, local network access, or saved profile data; and
- any suggested mitigation.

Please redact secrets, private keys, passwords, serial logs, hostnames, and private IP addresses. A maintainer will acknowledge the report, work on a fix or mitigation, and coordinate disclosure through the advisory.

## Security model and user responsibilities

Ferry invokes host tools such as `ssh`, optional `adb`, `rsync`, and `stty`, then stores device profiles under `~/.config/ferry/`. It aims to use the selected profile and conservative defaults, but users remain responsible for authorising network scans and target changes.

- Prefer SSH keys over stored passwords; use `fy keyup` where appropriate.
- Treat `--legacy` as an isolated-lab compatibility option, not a general SSH setting.
- Review local plugin code before installation. Plugins can run host-side commands according to their declared risk.
- Review `fy --dry-run` output before network, forwarding, gadget, NAT, or persistent configuration changes.
- Do not run Ferry with elevated privileges unless an operation explicitly requires it and you understand its plan.
