# Release Process

## Versioning

Keep these versions aligned:

- `package.json`
- `src-tauri/Cargo.toml`
- `src-tauri/tauri.conf.json`

Update `CHANGELOG.md` before creating a tag.

## Required Verification

```powershell
npm ci
npm run lint
npm run build
npm run test:native
npx playwright install chromium
npm run test:sandbox
npm run desktop:build
```

## Publishing

1. Create an annotated `vMAJOR.MINOR.PATCH` tag.
2. Push the tag and wait for the Release workflow.
3. Download the draft artifacts and verify `SHA256SUMS.txt` and `latest.json` against the NSIS installer.
4. Test installation, upgrade, uninstall, credential migration, SSH, and RDP on a clean Windows user profile.
5. Publish the draft only after the checks below pass.

Publishing a stable release triggers a second job that copies the release's `latest.json` into `deploy/xunduterminal/latest.json` on the default branch. Prereleases never update the public client manifest.

## Signing Gate

The public release workflow creates a draft and marks it as a prerelease. Do not publish production installers until an Authenticode certificate is configured and the resulting `.exe` and `.msi` signatures have been verified with `Get-AuthenticodeSignature`.

The current updater verifies the official GitHub Release URL, exact file size, and SHA-256 before it allows the user to launch the installer. Authenticode remains the production trust gate. Store signing material only in protected repository/environment secrets; never commit certificates, private keys, passwords, or exported secret files.

## Final Checklist

- [ ] Version and changelog are aligned.
- [ ] CI and sandbox regression pass.
- [ ] Installers are Authenticode signed.
- [ ] SHA256 checksums match.
- [ ] `latest.json` references the NSIS installer and contains the same size and SHA-256.
- [ ] A clean installation can download, cancel, retry, verify, and launch the update installer.
- [ ] Upgrade preserves metadata and migrates credentials.
- [ ] Exported configuration contains no passwords.
- [ ] Diagnostics contain no credentials or private infrastructure details.
- [ ] The release notes document known limitations.
