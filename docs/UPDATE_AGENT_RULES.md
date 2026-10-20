# 90FTP Release & Update Agent Rules (قواعد وكيل التحديث والإصدارات)

This document establishes the official release engineering protocol, Semantic Versioning hierarchy, and verification pipeline for 90FTP.

---

## 1. Semantic Versioning Protocol (SemVer)
Version numbers strictly adhere to `MAJOR.MINOR.PATCH` (e.g. `v2.6.0`):

- **PATCH (`x.y.Z`):** Bug fixes, visual polish, startup flash fixes, minor performance improvements.
- **MINOR (`x.Y.0`):** New features, cryptographic vault enhancements, remote update integration, new protocol support.
- **MAJOR (`X.0.0`):** Breaking architectural redesigns or backward-incompatible API/storage rewrites.

---

## 2. Synchronized File Locations
Every version change MUST be applied simultaneously across:
- `Cargo.toml`: `version = "X.Y.Z"`
- `installer.iss`: `#define MyAppVersion "X.Y.Z"`
- `src/ui/index.html`: `APP_CURRENT_VERSION = 'X.Y.Z'`, About Modal badge, Update Modal badge
- Inno Setup output: `dist\90FTP-Setup-vX.Y.Z.exe`

---

## 3. Automated Release Pipeline
1. Run test suite: `cargo test --bin ninety_ftp`
2. Build release binary: `cargo build --release`
3. Copy binary: `Copy-Item target\release\ninety_ftp.exe 90FTP.exe`
4. Sign binary with Authenticode (`assets\certs\aljailane.pfx`) and RFC 3161 timestamp (`DigiCert`).
5. Compile Inno Setup installer (`ISCC.exe installer.iss`).
6. Sign installer executable with Authenticode and DigiCert timestamp.
7. Git commit with Conventional Commits tag (`release: vX.Y.Z`).
8. Push to GitHub `origin main`.
