# Contributing

Thank you for helping improve XunDuTerminal.

## Before You Start

1. Search existing issues and pull requests.
2. Use an issue template for bugs or substantial feature proposals.
3. Keep changes focused. Large UI or protocol changes should be discussed before implementation.
4. Never commit credentials, private infrastructure details, personal screenshots, diagnostics, or generated installers.

## Local Setup

```powershell
npm ci
npm run desktop:dev
```

The supported development environment is Windows 10/11 with Node.js 24+, Rust 1.89+, WebView2, and the Windows OpenSSH Client.

## Required Checks

```powershell
npm run lint
npm run build
npm run test:native
npx playwright install chromium
npm run test:sandbox
```

Add focused regression coverage for behavior changes. UI changes should be checked at the minimum supported window size and in both themes.

## Pull Requests

- Explain the user-visible behavior and the reason for the change.
- List verification commands and any untested areas.
- Call out persistence migrations, protocol changes, or security implications.
- Do not include unrelated formatting, generated bundles, or dependency churn.

By contributing, you agree that your contribution is licensed under the MIT License.
