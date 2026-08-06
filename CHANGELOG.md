# Changelog

All notable changes to XunDuTerminal will be documented here. The project follows Semantic Versioning after the first stable release.

## [Unreleased]

## [0.2.2] - 2026-08-06

### Fixed

- Restore the parent-directory action in the remote file manager when the current directory is `/root` or `~`.
- Allow direct navigation to the Unix root directory `/` while keeping its parent action disabled.

## [0.2.1] - 2026-08-04

### Fixed

- Recover the local terminal automatically after a closed ConPTY pipe or Windows error 232.
- Prevent failed input from being replayed into the replacement shell and deduplicate concurrent recovery attempts.
- Clear stale local-terminal session state and suppress obsolete close events from replaced processes.
- Record local-terminal start, close, write failure, stop, and recovery diagnostics.

## [0.2.0] - 2026-07-22

### Added

- Secure in-app update downloads with progress, cancellation, retry, exact-size checks, and SHA-256 verification.
- Verified installer handoff that still requires explicit user confirmation before installation.
- Windows Credential Manager storage and plaintext credential migration.
- SSH password, private-key, and Agent authentication foundations.
- OpenSSH config import and known-host verification for helper connections.
- Open-source governance, security, and CI scaffolding.

### Changed

- Update downloads now appear in the unified file transfer manager.
- Stable GitHub Releases automatically publish a client update manifest; prereleases remain opt-in.

## [0.1.0] - 2026-07-20

- Initial public preview baseline.
