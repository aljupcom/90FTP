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
- **NEVER** hardcode the code-signing password. It is resolved from `NINETYFTP_PFX_PASS`, falling back to the gitignored `assets/certs/.pfxpass`.
- `*.pfx`, `*.pfx.bak`, `*.key`, `assets/certs/.pfxpass` and `.workbuddy-ai/` are gitignored. Never force-add them, and add any new secret file to `.gitignore` before creating it.

## 4. In-App Update Mechanism Context
- In-app updater checks: `https://api.github.com/repos/aljupcom/90FTP/releases/latest`.
- Merely pushing a Git Tag (`git push --tags`) does NOT notify the app; a GitHub Release entity with the attached `.exe` installer asset MUST exist.
- When publishing a new release, always verify the GitHub Release is created so users receive the green update badge.

## 5. Agent Command Runner Resilience
- If the background runner fails with `STATUS_DLL_INIT_FAILED (0xC0000142 / -1073741502)` due to Windows Desktop Heap constraints in non-interactive sessions:
  - Do NOT assume the release failed; give the user the exact single PowerShell line to trigger `release.ps1`.
  - Maintain ownership of the workflow: ensure all files, scripts, and configurations are 100% ready and validated.

## 6. Mandatory Structured Changelog Rules (Strictly Enforced)
- **Never publish a single-line or vague changelog**: Every release MUST feature a rich, structured, categorized changelog explicitly distinguishing:
  1. `### 🚀 Added`: New features, tools, modals, or user capabilities.
  2. `### ⚡ Changed`: Enhancements, UI refinements, speedups, and optimizations.
  3. `### 🛡️ Fixed`: Bug fixes, security hardenings, stability improvements, and collision fixes.
- When executing `.\scripts\release.ps1`, you MUST provide structured items using:
  ```powershell
  .\scripts\release.ps1 -Version "<version>" -Message "<summary>" `
    -Added @("Feature 1", "Feature 2") `
    -Changed @("Improvement 1") `
    -Fixed @("Bug fix 1", "Bug fix 2")
  ```
  or pass a complete Markdown changelog via `-Changelog @"..."@`.
- In `src/ui/index.html`, `formatChangelogHtml` renders structured category badges (`[ADDED]`, `[CHANGED]`, `[FIXED]`) with distinctive color coding and icons.

## 7. Security Invariants (Non-Negotiable)
- **FTPS must stay real TLS.** `ftps` / `ftpes` always route to `ftp_client::connect_secure` (AUTH TLS). Never merge `"ftp" | "ftps"` into one branch, and never add a silent cleartext fallback — a handshake failure must surface as an error.
- **Vault key**: a random 32-byte key wrapped with Windows DPAPI (`%APPDATA%\90FTP\v_key.dat`). `legacy_env_key()` exists only to migrate old vaults and to derive stable filenames — never use it to encrypt new data, and never change the filename derivation.
- **Migration order is mandatory**: write the key first, then re-encrypt in place, and keep the self-healing path in `read_vault_encrypted`. Any anomaly must fall back to the legacy key rather than risk user data.
- **Escaping**: `escapeHtml` escapes `& < > " '`. Never reintroduce the `innerText → innerHTML` trick. `escapeJsString` escapes `"` as `&quot;` for inline handlers. Keep the CSP meta tag in both `index.html` and `terminal.html`.
- **Remote command building**: quote every interpolated path with `sq()`; validate chmod modes against `[0-7]{3,4}`; pass anything destined for `evaluate_script` through `js_literal()`.
- **Server-to-server transfers**: `StrictHostKeyChecking=accept-new` only, and `sshpass -f` with a 0600 temp file — never `sshpass -p`, and never log the command with the password in it.
- **DevTools**: `with_devtools(cfg!(debug_assertions))` only.
- Full detail lives in `.gemini/rules/security_rules.md`.

## 8. Live Filesystem Synchronisation (Do Not Break)
- `src/watcher.rs` fingerprints each directory from (name, is_dir, size, mtime); `AppState::ensure_watcher()` polls local every `interval_ms` and remote every ~4s using `try_lock` — **never `lock()`**, which would freeze the watcher behind a long upload or terminal command.
- IPC: `watch_start` / `watch_stop`, with a `generation` counter that resets fingerprints on directory changes (no false positives).
- UI: `window.onFsChanged` → `silentRefreshLocal` / `silentRefreshRemote`. They must preserve selection (via the `data-path` attribute on rows) and scroll position, and must call `invalidateFolderSizeCache(path)` before re-rendering.
- `updateFsWatcher()` is invoked at the end of `renderLocalFiles`, `renderRemoteFiles` and `setConnectedState`. Do not remove those hooks, and add the same call to any new file-listing view.
- Tests must never touch a real user profile — set `NINETYFTP_HOME` to redirect storage.
- Full detail lives in `.gemini/rules/live_sync_rules.md`.
