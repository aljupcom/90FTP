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

## Structured Changelog Enforcement
- Never publish a single-line or vague changelog.
- Every release must categorize items into:
  - `### 🚀 Added` (New features and capabilities)
  - `### ⚡ Changed` (Improvements and optimizations)
  - `### 🛡️ Fixed` (Bug fixes and security resilience)
- Pass structured parameters to `release.ps1`:
  `-Added @("...") -Changed @("...") -Fixed @("...")` or `-Changelog @"..."@`.
- In-app update manager (`src/ui/index.html`) automatically formats category badges (`[ADDED]`, `[CHANGED]`, `[FIXED]`) with distinctive color coding.
