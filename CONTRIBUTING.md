# Contributing to Ferry

Thank you for helping make embedded bring-up more repeatable.

## Before you start

- Check existing issues and discussions before opening a large proposal.
- Keep a change focused on one user-visible behaviour or one internal boundary.
- Never include device passwords, private keys, serial logs with secrets, customer data, or private network topology in a report, fixture, or screenshot.
- For behaviour that touches a board, state the transport, target OS, Ferry version, exact command, expected result, actual result, and any rollback performed.

## Development setup

```bash
git clone https://github.com/NinoC137/ferry.git
cd ferry
cargo test -p ferry --lib

cd desktop
npm ci
npm run build
cd ..
cargo check -p ferry-desktop
```

Use the narrowest relevant check. This repository intentionally has some pre-existing formatting drift; run `rustfmt` only on Rust files you changed, rather than formatting the entire workspace.

## Design expectations

### Preserve transport semantics

SSH, ADB, and serial are different failure domains. Do not make a new feature silently retarget a device or treat a successful TCP connection as proof that a usable target exists. Discovery should remain bounded and actionable.

### Keep automation trustworthy

For commands that support `--json`, stdout must contain one JSON document only; progress and diagnostics belong on stderr. New side effects need clear errors, predictable exit status, and a non-interactive path where practical.

### Make state changes reviewable

Document what a workflow changes on the host and target, its prerequisites, and how to roll it back. Prefer a preflight or `--dry-run` plan for potentially disruptive work. Default to read-only or least-privilege behaviour.

### Test the boundary you changed

- Add hermetic unit tests for parsing, matching, and state transitions.
- Test process/transport boundaries where the behaviour depends on a real PTY, WebSocket, or external command.
- For desktop UI changes, run the frontend build and the relevant Rust desktop check.
- Avoid committing generated `target/`, `dist/`, or machine-local configuration files.

## Pull requests

Use a clear title and explain:

1. the problem and affected workflow;
2. the chosen behaviour and trade-offs;
3. safety implications and rollback, if state can change;
4. verification commands and their result; and
5. documentation updates needed for users or automations.

Small, reviewable pull requests are easier to validate against real hardware. See [SECURITY.md](SECURITY.md) instead of filing a public issue for a security vulnerability.
