# 90FTP Release & Update Agent Protocol (قواعد وكيل التحديث والإصدارات)

Official release engineering protocol, Semantic Versioning hierarchy, and asset publishing rules for 90FTP.

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

## 3. Mandatory GitHub Release & Asset Publishing
Every release published on GitHub MUST include:
1. Git Tag `vX.Y.Z` pushed to `origin`.
2. Official GitHub Release with markdown changelog highlights.
3. Attached binary asset: signed `90FTP-Setup-vX.Y.Z.exe` uploaded to GitHub Release Assets so the client application's built-in update modal can detect and download it directly.

---

## 4. One-Command Automated Release Pipeline
To execute the complete pipeline (bump version, test, build, sign with DigiCert RFC 3161 timestamp, compile installer, push git tag, create GitHub Release, and upload installer asset):

```powershell
.\scripts\release.ps1 -Version "X.Y.Z" -Message "Summary of changes"
```
