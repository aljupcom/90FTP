# 90FTP Release & Update Agent Protocol (قواعد وكيل الإصدارات والتحديثات)

Official release protocol, development-branch versioning policy, and dynamic installer build specification for 90FTP.

---

## 1. Development Mode Versioning Policy (الترقيم بالفرع الثالث)
- **No Large Version Bumps in Development:** Major releases (1.0, 2.0, etc.) are strictly forbidden while in the active development phase.
- **Third Branch Progression (`0.0.X`):**
  - Initial baseline: `0.0.1` (equivalent to `0.0.0.1`).
  - Increments strictly occur in the **third branch**: `0.0.1` -> `0.0.2` -> `0.0.3` -> `0.0.4` ...
  - Progression to `1.0.0` or production numbering occurs ONLY upon explicit user approval.

---

## 2. Dynamic Installer Compilation (رقم الإصدار الديناميكي)
- **No Hardcoded Versions in `installer.iss`:** Static version strings are disallowed.
- **CLI Dynamic Injection:** The version is passed dynamically at compile time via:
  ```powershell
  & "C:\Program Files\Inno Setup 7\ISCC.exe" "/DMyAppVersion=$Version" installer.iss
  ```
- **Dynamic Output Filename:** `dist\90FTP-Setup-v{$Version}.exe`.

---

## 3. Synchronized Version Checklist
- `Cargo.toml`: `version = "0.0.X"`
- `installer.iss`: dynamic `/DMyAppVersion=0.0.X`
- `src/ui/index.html`: `APP_CURRENT_VERSION = '0.0.X'`, About modal (`0.0.X Dev`), Update modal (`Current: v0.0.X`)
- Binary: `90FTP.exe` (signed)
- Installer: `dist\90FTP-Setup-v0.0.X.exe` (signed)

---

## 4. One-Command Automated Release Pipeline
```powershell
.\scripts\release.ps1 -Version "0.0.X" -Message "Changelog description"
```
Handles building, signing with DigiCert timestamp, compiling dynamic installer, pushing git commit and tag, and publishing the GitHub Release with attached installer asset.
