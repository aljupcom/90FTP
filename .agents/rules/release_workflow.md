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
