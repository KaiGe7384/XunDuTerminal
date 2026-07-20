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
3. Download the draft artifacts and verify `SHA256SUMS.txt`.
4. Test installation, upgrade, uninstall, credential migration, SSH, and RDP on a clean Windows user profile.
5. Publish the draft only after the checks below pass.

## Signing Gate

The public release workflow creates a draft and marks it as a prerelease. Do not publish production installers until an Authenticode certificate is configured and the resulting `.exe` and `.msi` signatures have been verified with `Get-AuthenticodeSignature`.

Updater support must use a separate Tauri updater signing key. Store signing material only in protected repository/environment secrets; never commit certificates, private keys, passwords, or exported secret files.

## Final Checklist

- [ ] Version and changelog are aligned.
- [ ] CI and sandbox regression pass.
- [ ] Installers are Authenticode signed.
- [ ] SHA256 checksums match.
- [ ] Upgrade preserves metadata and migrates credentials.
- [ ] Exported configuration contains no passwords.
- [ ] Diagnostics contain no credentials or private infrastructure details.
- [ ] The release notes document known limitations.
