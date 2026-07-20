# XunDuTerminal

XunDuTerminal is a Windows-first server workspace that keeps SSH terminals, file management, system monitoring, process inspection, and native remote desktops in one persistent layout.

[Source](https://github.com/KaiGe7384/XunDuTerminal) · [Releases](https://github.com/KaiGe7384/XunDuTerminal/releases) · [Issues](https://github.com/KaiGe7384/XunDuTerminal/issues)

> The project is in active pre-release development. Back up important connection metadata before testing a development build.

## Highlights

- Persistent multi-workspace layouts with draggable and resizable panels.
- Local and SSH terminals powered by xterm.js.
- Password, private-key, and SSH Agent authentication.
- Import from pasted connection text and `~/.ssh/config`.
- SFTP file browsing, editing, upload/download progress, and context actions.
- CPU, memory, disk, network, and process views.
- Native RDP sessions with dynamic resolution, clipboard text, and file transfer.
- Dark and light themes, Chinese and English UI, and configurable terminal typography.

## Security

- SSH and RDP secrets are stored in Windows Credential Manager.
- Browser storage, workspace snapshots, and exported server JSON exclude passwords.
- Existing plaintext browser-storage credentials are migrated before the plaintext copy is removed.
- SSH helper connections enforce the user's OpenSSH `known_hosts` file with an accept-new policy and reject key mismatches.
- Diagnostics are intended to exclude credentials. Please report any accidental secret exposure privately.

See [SECURITY.md](SECURITY.md) for vulnerability reporting and the current support policy.

## Development

### Prerequisites

- Windows 10 or Windows 11
- Node.js 24+
- Rust 1.89+
- Microsoft WebView2 Runtime
- Windows OpenSSH Client

### Run

```powershell
npm ci
npm run desktop:dev
```

### Verify

```powershell
npm run lint
npm run build
npm run test:native
npx playwright install chromium
npm run test:sandbox
```

### Package

```powershell
npm run desktop:build
```

Tauri writes native bundles under `src-tauri/target/release/bundle/`.

## Architecture

- `src/`: React UI, workspaces, xterm renderers, persistence, and sandbox bridge.
- `Skin/`: auto-discovered, data-only theme presets and their JSON schema. See [Skin/README.md](Skin/README.md) to create a preset without editing application code.
- `src-tauri/src/`: native shells, SSH/SFTP, system inspection, credential storage, and RDP commands.
- `src-tauri/vendor/`: the locally patched IronRDP clipboard integration.
- `tools/`: Playwright sandbox regression tests and performance helpers.
- `docs/`: architecture and feature-specific notes.
- `docs/UPDATES.md`: update manifest format and official release-link rules.

## Contributing

Read [CONTRIBUTING.md](CONTRIBUTING.md) before opening a pull request. Bug reports should use the issue templates and must not contain passwords, private keys, access tokens, or unredacted diagnostics.

## License

XunDuTerminal is available under the [MIT License](LICENSE). Third-party components retain their original licenses; see [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md).
