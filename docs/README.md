# Ferry documentation

This directory contains the maintained reference material for Ferry. The installed CLI remains the authority for the exact command set in the version you run: use `fy --help` or `fy help --json` before automating a workflow.

| English | 中文 |
| --- | --- |
| [Project overview](../README.md) | [项目概览](../README.zh-CN.md) |
| [Operations guide](operations.md) | [操作指南](operations.zh-CN.md) |
| [Architecture](architecture.md) | [架构说明](architecture.zh-CN.md) |
| [Contribution guide](../CONTRIBUTING.md) | [贡献指南（英文）](../CONTRIBUTING.md) |
| [Security policy](../SECURITY.md) | [安全策略（英文）](../SECURITY.md) |

## Reading order

1. Start with the project overview for installation and a five-minute first target.
2. Use the operations guide when you have a concrete lab task: discovery, transfer, recovery, networking, hardware capture, or automation.
3. Read the architecture guide before changing a transport, profile persistence, WebSocket/PTy behavior, or desktop integration.

## Documentation conventions

- `<device>` means a saved Ferry profile name, such as `rk`.
- `<path>` denotes a path on the host unless the command says it is remote.
- Operations that can alter host routing, target networking, target boot configuration, or local files are called out explicitly.
- Examples use RFC 5737 documentation addresses where an address is needed; replace them with your authorised lab network.
