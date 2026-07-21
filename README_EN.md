# XunDuTerminal

[简体中文](README.md) · **English**

XunDuTerminal is an open-source, Windows-first server workspace that keeps SSH terminals, file management, system monitoring, process inspection, and native remote desktops in one persistent multi-workspace layout.

[Source](https://github.com/KaiGe7384/XunDuTerminal) · [Releases](https://github.com/KaiGe7384/XunDuTerminal/releases) · [Issues](https://github.com/KaiGe7384/XunDuTerminal/issues)

> The current `v0.1.0` build is a pre-release. Windows installers are not Authenticode-signed yet and may trigger SmartScreen. Back up important connection metadata before testing.

## Highlights

- Persistent multi-workspace layouts with draggable and resizable panels.
- Local and SSH terminals powered by xterm.js.
- Password, private-key, and SSH Agent authentication.
- Import from pasted connection text and `~/.ssh/config`.
- SFTP file browsing, editing, drag-and-drop upload, download progress, and context actions.
- CPU, memory, disk, network, and process views.
- Native RDP sessions with dynamic resolution, clipboard text, and file transfer.
- Dark and light appearances, eight file-backed themes, custom wallpapers, and interface transparency.
- Chinese and English UI, configurable terminal typography, and reduced-motion support.

## Platform Support

| Platform | Status |
| --- | --- |
| Windows 10 / 11 x64 | Supported; NSIS EXE and MSI bundles are available in Releases |
| iOS / iPadOS | Planned, but not currently available; mobile UX, native capabilities, and Apple signing still require adaptation |
| macOS / Linux / Android | No supported build yet |

XunDuTerminal currently depends on desktop capabilities including Windows Credential Manager, ConPTY, local process management, and native RDP clipboard integration. A usable iOS application therefore requires a dedicated port rather than only switching the CI runner.

## Security

- SSH and RDP secrets are stored in Windows Credential Manager.
- Browser storage, workspace snapshots, and exported server JSON exclude passwords.
- Existing plaintext browser-storage credentials are migrated before the plaintext copy is removed.
- SSH helper connections enforce the user's OpenSSH `known_hosts` file with an accept-new policy and reject key mismatches.
- Diagnostics are intended to exclude credentials and infrastructure details. Please report accidental secret exposure privately.

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

- `src/`: React UI, workspaces, xterm renderers, persistence, and the sandbox bridge.
- `Skin/`: auto-discovered, data-only themes and their JSON schema. See [Skin/README.md](Skin/README.md).
- `src-tauri/src/`: native shells, SSH/SFTP, system inspection, credential storage, and RDP commands.
- `src-tauri/vendor/`: the locally patched IronRDP clipboard integration.
- `tools/`: Playwright sandbox regression tests and performance helpers.
- `docs/`: architecture and feature-specific notes.
- `docs/UPDATES.md`: update manifest format and official release-link rules.

## Contributing

Read [CONTRIBUTING.md](CONTRIBUTING.md) before opening a pull request. Issues, logs, and screenshots must not contain passwords, private keys, access tokens, or unredacted infrastructure data.

## License

XunDuTerminal is available under the [MIT License](LICENSE). Third-party components retain their original licenses; see [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md).
