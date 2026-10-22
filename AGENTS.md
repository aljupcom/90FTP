# 90FTP - Project Architecture & Agent Context Rules

## 1. Project Overview & Architecture
- **Product Name**: 90FTP - Modern Site Manager (Desktop FTP/FTPS/SFTP Client).
- **Core Stack**: Rust (`tao` windowing + `wry` WebView2 host + `russh` SFTP + `suppaftp` FTP) + Single-Page UI in `src/ui/index.html` (Vanilla JS, modern CSS, vector SVGs).
- **Strict UI Language Rule**: All code, UI elements, button labels, logs, messages, and HTML attributes MUST be 100% in English. Arabic is used exclusively for chat communication with the user.
- **Strict Browser Rule**: NEVER launch external web browsers (Chrome, Edge, etc.) unless the user explicitly requests it (`لا تقم بتشغيل المتصفح الا بطلب مني`).

## 2. Release & Versioning Pipeline (Mandatory Workflow)
- **Release Automation Script**: `.\scripts\release.ps1`
- **Release Execution**:
  ```powershell
  .\scripts\release.ps1 -Version "<version>" -Message "<release notes>"
  ```
- **Automated Pipeline Stages**:
  1. Synchronizes version across `Cargo.toml`, `installer.iss`, and `src/ui/index.html`.
  2. Runs test suite: `cargo test --bin ninety_ftp`.
  3. Builds optimized release binary: `cargo build --release`.
  4. Signs executables (`target\release\ninety_ftp.exe` and `90FTP.exe`) using `signtool.exe` with certificate `assets\certs\aljailane.pfx`.
  5. Dynamically compiles Inno Setup installer using `ISCC.exe` -> `dist\90FTP-Setup-v<version>.exe`.
  6. Signs installer package with digital timestamp.
  7. Commits and pushes branch and tag to GitHub.
  8. Creates official **GitHub Release** via GitHub API and uploads `90FTP-Setup-v<version>.exe` as release asset.

## 3. GitHub Push Protection & Secret Management
- **NEVER** write or commit the GitHub Personal Access Token (PAT) directly into source code files or scripts.
- The release script extracts the token dynamically at runtime from local Git origin (`git remote get-url origin`) to prevent triggering GitHub Push Protection (`GH013`).

## 4. In-App Update Mechanism Context
- In-app updater checks: `https://api.github.com/repos/aljupcom/90FTP/releases/latest`.
- Merely pushing a Git Tag (`git push --tags`) does NOT notify the app; a GitHub Release entity with the attached `.exe` installer asset MUST exist.
- When publishing a new release, always verify the GitHub Release is created so users receive the green update badge.

## 5. Agent Command Runner Resilience
- If the background runner fails with `STATUS_DLL_INIT_FAILED (0xC0000142 / -1073741502)` due to Windows Desktop Heap constraints in non-interactive sessions:
  - Do NOT assume the release failed; give the user the exact single PowerShell line to trigger `release.ps1`.
  - Maintain ownership of the workflow: ensure all files, scripts, and configurations are 100% ready and validated.
