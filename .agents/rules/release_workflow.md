---
trigger: always_on
---

# 90FTP Release Workflow & Architecture Guide

## Release Process
- Always use `.\scripts\release.ps1 -Version "<ver>" -Message "<msg>"`
- The script automatically handles:
  1. Multi-file version sync (`Cargo.toml`, `installer.iss`, `src/ui/index.html`)
  2. Test suite (`cargo test`)
  3. Release build (`cargo build --release`)
  4. Code signing with `assets/certs/aljailane.pfx` via `signtool.exe`
  5. Inno Setup installer compilation (`ISCC.exe`)
  6. Installer signing
  7. Git commit, tag, and push
  8. GitHub Release creation & installer asset upload
- Never commit hardcoded tokens into files.
- In-app updates require a GitHub Release object with `.exe` asset attached.

## Code Signing Credentials
- The PFX password is never stored in the repository. `release.ps1` resolves it from
  `NINETYFTP_PFX_PASS` first, then from the gitignored `assets/certs/.pfxpass`.
- The script aborts on any non-zero exit code from `cargo test`, `cargo build` or
  `ISCC`. A failing stage must never publish — `$ErrorActionPreference = "Stop"` does
  not catch native exit codes, so every native call is checked explicitly.
- Because the certificate is self-signed, `signtool verify /pa` fails on any machine
  where the root is not installed. `Sign-File` therefore falls back to comparing the
  embedded signer thumbprint against `assets/certs/aljailane.crt`.
- Rotating the PFX password only re-locks the container: the certificate and private
  key stay the same, so previously published signatures remain valid.
- Never sign with a silently-failing `try/catch`. An unsigned build must stop the
  release, not ship.

## Structured Changelog Enforcement
- Never publish a single-line or vague changelog.
- Every release must categorize items into:
  - `### 🚀 Added` (New features and capabilities)
  - `### ⚡ Changed` (Improvements and optimizations)
  - `### 🛡️ Fixed` (Bug fixes and security resilience)
- Pass structured parameters to `release.ps1`:
  `-Added @("...") -Changed @("...") -Fixed @("...")` or `-Changelog @"..."@`.
- In-app update manager (`src/ui/index.html`) automatically formats category badges (`[ADDED]`, `[CHANGED]`, `[FIXED]`) with distinctive color coding.
