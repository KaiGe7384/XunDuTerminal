# Security Policy

## Supported Versions

XunDuTerminal is currently pre-release software. Security fixes are applied to the latest commit and the newest published `0.1.x` build only.

## Reporting a Vulnerability

Please use GitHub Private Vulnerability Reporting for suspected vulnerabilities. Do not open a public issue until a maintainer confirms that disclosure is safe.

Include:

- A concise description of the impact.
- Reproduction steps or a minimal proof of concept.
- The affected version and Windows version.
- Whether credentials, private keys, clipboard data, or remote files are exposed.

Never attach real passwords, private keys, access tokens, production host lists, or unredacted diagnostic logs. Maintainers will acknowledge a complete report as soon as practical, coordinate remediation, and credit reporters who want attribution.

## Security Boundaries

- Connection secrets belong in Windows Credential Manager, not browser storage or exported JSON.
- A process running as the same Windows user may be able to request that user's generic credentials. The vault protects data at rest; it is not a sandbox against a compromised user session.
- New SSH host keys are accepted into the user's `known_hosts` file. A changed key is blocked and must be investigated before the old entry is removed.
- Development and unsigned builds should not be treated as trusted production releases.
