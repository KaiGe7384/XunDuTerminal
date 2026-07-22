<div align="center">
  <img src="public/xundu-terminal-icon.svg" width="104" alt="XunDuTerminal icon" />
  <h1>XunDuTerminal</h1>
  <p><strong>A unified Windows workspace for server operations</strong></p>
  <p>Bring SSH terminals, SFTP file management, system monitoring, process inspection, and native remote desktops into persistent workspaces.</p>
  <p>
    <a href="https://github.com/KaiGe7384/XunDuTerminal/releases"><img src="https://img.shields.io/github/v/release/KaiGe7384/XunDuTerminal?include_prereleases&style=flat-square&label=Release" alt="Release" /></a>
    <a href="https://github.com/KaiGe7384/XunDuTerminal/actions/workflows/ci.yml"><img src="https://github.com/KaiGe7384/XunDuTerminal/actions/workflows/ci.yml/badge.svg" alt="CI" /></a>
    <a href="LICENSE"><img src="https://img.shields.io/github/license/KaiGe7384/XunDuTerminal?style=flat-square" alt="MIT License" /></a>
    <img src="https://img.shields.io/badge/Windows-10%20%7C%2011-0078D4?style=flat-square&logo=windows11&logoColor=white" alt="Windows 10 / 11" />
  </p>
  <p>
    <a href="https://github.com/KaiGe7384/XunDuTerminal/releases/tag/v0.1.0"><strong>Download pre-release</strong></a>
    · <a href="#feature-overview">Features</a>
    · <a href="#development-and-builds">Development</a>
    · <a href="https://github.com/KaiGe7384/XunDuTerminal/issues">Report an issue</a>
  </p>
  <p><a href="README.md">简体中文</a> · <strong>English</strong></p>
</div>

<br />

<p align="center">
  <a href="docs/screenshots/workspace-overview.png">
    <img src="docs/screenshots/workspace-overview.png" alt="XunDuTerminal multi-panel server workspace" width="96%" />
  </a>
</p>
<p align="center"><sub>Combine terminals, file management, resource monitoring, and processes in one workspace. The screen uses safe sandbox data.</sub></p>

## Feature overview

XunDuTerminal is built for developers and operators who work with multiple Windows and Linux servers. Everyday tools live in one window while connection profiles, workspace layouts, and appearance preferences persist between sessions.

<table>
  <tr>
    <td width="50%" valign="top">
      <strong>Terminals and connections</strong><br /><br />
      Local and SSH terminals with password, private-key, and SSH Agent authentication, plus imports from connection text or <code>~/.ssh/config</code>.
    </td>
    <td width="50%" valign="top">
      <strong>Files and transfers</strong><br /><br />
      SFTP browsing, text editing, drag-and-drop uploads, download progress, and a shared transfer manager for every file operation.
    </td>
  </tr>
  <tr>
    <td width="50%" valign="top">
      <strong>Monitoring and processes</strong><br /><br />
      Inspect CPU, memory, disks, networking, and system processes without switching between terminal sessions and separate monitoring tools.
    </td>
    <td width="50%" valign="top">
      <strong>Remote desktop and workspaces</strong><br /><br />
      Native RDP, dynamic resolution, clipboard text, and file transfer in draggable, resizable, restorable multi-workspace layouts.
    </td>
  </tr>
  <tr>
    <td width="50%" valign="top">
      <strong>Themes and accessibility</strong><br /><br />
      Dark and light appearances, eight file-backed themes, custom wallpapers, interface transparency, terminal sizing, and reduced motion.
    </td>
    <td width="50%" valign="top">
      <strong>Credential and connection safety</strong><br /><br />
      Secrets live in Windows Credential Manager. SSH host-key changes block the connection until the user confirms a safe replacement.
    </td>
  </tr>
</table>

## Make the workspace yours

<p align="center">
  <a href="docs/screenshots/theme-presets.png">
    <img src="docs/screenshots/theme-presets.png" alt="XunDuTerminal theme presets" width="88%" />
  </a>
</p>
<p align="center"><sub>Eight presets span dark, light, and glass-inspired directions with synchronized terminal colors, wallpaper, and transparency.</sub></p>

## Download and get started

> **Current version: [`v0.1.0` pre-release](https://github.com/KaiGe7384/XunDuTerminal/releases/tag/v0.1.0)**

| File | Best for |
| --- | --- |
| [`XunDuTerminal_0.1.0_x64-setup.exe`](https://github.com/KaiGe7384/XunDuTerminal/releases/download/v0.1.0/XunDuTerminal_0.1.0_x64-setup.exe) | Recommended NSIS installer for most users |
| [`XunDuTerminal_0.1.0_x64_en-US.msi`](https://github.com/KaiGe7384/XunDuTerminal/releases/download/v0.1.0/XunDuTerminal_0.1.0_x64_en-US.msi) | MSI-based deployment environments |
| [`SHA256SUMS.txt`](https://github.com/KaiGe7384/XunDuTerminal/releases/download/v0.1.0/SHA256SUMS.txt) | Installer integrity verification |

> [!WARNING]
> The Windows installers are not Authenticode-signed yet, so SmartScreen may display a warning. Back up important connection metadata before testing and download files only from this repository's Releases page.

Verify an installer in PowerShell:

```powershell
Get-FileHash .\XunDuTerminal_0.1.0_x64-setup.exe -Algorithm SHA256
```

Compare the result with the matching entry in `SHA256SUMS.txt`.

## Platform support

| Platform | Status |
| --- | --- |
| **Windows 10 / 11 x64** | Supported with NSIS EXE and MSI bundles |
| iOS / iPadOS | Planned, but not currently available |
| macOS / Linux / Android | No supported build yet |

XunDuTerminal currently relies on Windows Credential Manager, ConPTY, local process management, and native RDP clipboard integration. Mobile platforms need dedicated UX, native capability, and signing work rather than only a different CI runner.

## Security by design

- SSH and RDP secrets are stored in Windows Credential Manager.
- Browser storage, workspace snapshots, and exported server JSON exclude passwords.
- Existing plaintext browser-storage credentials are migrated before the plaintext copy is removed.
- SSH helper connections use the user's OpenSSH `known_hosts`; new keys can be accepted, while mismatches are blocked pending confirmation.
- Diagnostics are designed to exclude credentials, server addresses, and local paths.

See [`SECURITY.md`](SECURITY.md) for vulnerability reporting and the current support policy.

## Development and builds

<details>
  <summary><strong>Show prerequisites, verification, and packaging commands</strong></summary>

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

</details>

<details>
  <summary><strong>Show repository structure</strong></summary>

- `src/`: React UI, workspaces, xterm renderers, persistence, and the sandbox bridge.
- `Skin/`: auto-discovered, data-only themes and their JSON schema. See [`Skin/README.md`](Skin/README.md).
- `src-tauri/src/`: native shells, SSH/SFTP, system inspection, credential storage, and RDP commands.
- `src-tauri/vendor/`: the locally patched IronRDP clipboard integration.
- `tools/`: Playwright sandbox regression tests and performance helpers.
- `docs/`: architecture and feature-specific notes.
- `docs/UPDATES.md`: update manifest format and official release-link rules.

</details>

## Contributing

Read [`CONTRIBUTING.md`](CONTRIBUTING.md) before opening a pull request. Issues, logs, and screenshots must not contain passwords, private keys, access tokens, or unredacted infrastructure data.

## Community and services

- Enterprise server hosting: [XunDu Cloud](https://xunduyun.com/)
- Technical QQ group: `1090339570`
- Technical QQ group 2: `262430517`

## License

XunDuTerminal is available under the [MIT License](LICENSE). Third-party components retain their original licenses; see [`THIRD_PARTY_NOTICES.md`](THIRD_PARTY_NOTICES.md).

<p align="center"><sub>Built for a smoother and more transparent server-management workflow.</sub></p>
