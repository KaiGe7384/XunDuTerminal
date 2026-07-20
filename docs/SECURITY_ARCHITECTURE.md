# Security Architecture

## Credentials

SSH passwords, SSH key passphrases, RDP passwords, and workspace-only RDP passwords are stored as generic credentials in Windows Credential Manager. Browser storage contains connection metadata and stable credential identifiers only.

At startup, legacy plaintext records are written to the system vault first. The plaintext browser-storage copy is removed only after the write and read-back path succeeds. Credentials are hydrated into process memory when needed by the UI and native connection commands.

## SSH Trust

Interactive terminal sessions use the Windows OpenSSH client with `StrictHostKeyChecking=accept-new`. Native SSH/SFTP helper sessions read the same user-level `~/.ssh/known_hosts` file, add previously unseen keys, and reject changed keys.

Supported profile authentication methods are password, private-key file with an optional passphrase, and SSH Agent.

## Diagnostics

Diagnostics are written under `%LOCALAPPDATA%/XunDuTerminal/logs`, rotate at 5 MiB, and keep one backup. Common password, token, passphrase, secret, RDP `/pass:`, and private-key markers are redacted before writing.

Diagnostic redaction is defense in depth, not permission to log secrets. New code must avoid passing secrets into log functions in the first place.

## Remaining Boundaries

- A process running as the same Windows user can potentially request that user's generic credentials.
- Secrets exist in application memory while a connection is active or its profile is hydrated.
- Release signing and updater signing are operational requirements outside the source tree.
