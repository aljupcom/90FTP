use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::app_state::ScriptCallback;
#[allow(unused_imports)]
use crate::models::FileItem;
use crate::remote::RemoteClient;

fn default_transfer_mode() -> String {
    "auto".to_string()
}
fn default_auth_type() -> String {
    "password".to_string()
}
fn default_true() -> bool {
    true
}
fn default_overwrite() -> String {
    "overwrite".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteTransferTask {
    pub source_is_remote: bool,
    pub sources: Vec<String>,
    #[serde(default = "default_transfer_mode")]
    pub transfer_mode: String, // "auto", "direct_scp", "direct_rsync", "stream_relay"
    pub target_protocol: String, // "sftp", "scp", "ssh", "ftp", "ftps"
    pub target_host: String,
    pub target_port: u16,
    pub target_user: String,
    #[serde(default)]
    pub target_pass: String,
    #[serde(default = "default_auth_type")]
    pub target_auth_type: String, // "password" or "key"
    #[serde(default)]
    pub target_key_path: Option<String>,
    #[serde(default)]
    pub target_passphrase: Option<String>,
    pub target_path: String,
    #[serde(default = "default_true")]
    pub recursive: bool,
    #[serde(default = "default_overwrite")]
    pub overwrite: String, // "overwrite", "skip"
}

#[derive(Debug, Clone)]
pub struct TransferItem {
    pub relative_path: String,
    pub is_dir: bool,
    pub size: u64,
    pub source_full_path: String,
}

pub fn format_size(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;

    let b = bytes as f64;
    if b >= GB {
        format!("{:.2} GB", b / GB)
    } else if b >= MB {
        format!("{:.2} MB", b / MB)
    } else if b >= KB {
        format!("{:.1} KB", b / KB)
    } else {
        format!("{} B", bytes)
    }
}

pub fn test_target_connection(task: &RemoteTransferTask) -> Result<serde_json::Value, String> {
    let mut client = RemoteClient::connect(
        &task.target_protocol,
        &task.target_host,
        task.target_port,
        &task.target_user,
        &task.target_pass,
        &task.target_auth_type,
        task.target_key_path.as_deref(),
        task.target_passphrase.as_deref(),
        true,
    )?;

    let target_dir = task.target_path.trim();
    let clean_path = if target_dir.is_empty() { "." } else { target_dir };

    let pwd = client.pwd();
    let stat_res = client.stat(clean_path);

    match stat_res {
        Ok(Some(item)) => {
            Ok(json!({
                "connected": true,
                "target_pwd": pwd,
                "path_exists": true,
                "is_dir": item.is_dir,
                "message": format!("Destination path verified (Type: {}, Current dir: {})", if item.is_dir { "Directory" } else { "File" }, pwd)
            }))
        }
        Ok(None) => {
            Ok(json!({
                "connected": true,
                "target_pwd": pwd,
                "path_exists": false,
                "is_dir": true,
                "message": format!("Target directory does not exist yet. It will be created automatically under {}", pwd)
            }))
        }
        Err(e) => {
            Ok(json!({
                "connected": true,
                "target_pwd": pwd,
                "path_exists": false,
                "is_dir": true,
                "warning": format!("Connected, but path check returned: {}", e),
                "message": format!("Connected successfully to {}. Target path will be auto-created.", task.target_host)
            }))
        }
    }
}

pub fn list_target_directory(
    task: &RemoteTransferTask,
    sub_path: Option<&str>,
) -> Result<serde_json::Value, String> {
    let mut client = RemoteClient::connect(
        &task.target_protocol,
        &task.target_host,
        task.target_port,
        &task.target_user,
        &task.target_pass,
        &task.target_auth_type,
        task.target_key_path.as_deref(),
        task.target_passphrase.as_deref(),
        true,
    )?;

    let current = if let Some(p) = sub_path {
        if !p.is_empty() {
            client.change_dir(p)?;
            client.pwd()
        } else {
            client.pwd()
        }
    } else if !task.target_path.trim().is_empty() {
        let _ = client.change_dir(&task.target_path);
        client.pwd()
    } else {
        client.pwd()
    };

    let items = client.list(Some(&current))?;
    Ok(json!({
        "current_dir": current,
        "items": items
    }))
}

pub fn collect_local_items(
    sources: &[String],
    cancelled: &Arc<AtomicBool>,
) -> Result<Vec<TransferItem>, String> {
    let mut items = Vec::new();
    for src in sources {
        if cancelled.load(Ordering::Relaxed) {
            return Err("Transfer cancelled by user".to_string());
        }
        let p = std::path::Path::new(src);
        if !p.exists() {
            continue;
        }
        let meta = std::fs::metadata(p).map_err(|e| format!("Cannot read metadata for {:?}: {}", p, e))?;
        let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("item").to_string();

        if meta.is_dir() {
            items.push(TransferItem {
                relative_path: name.clone(),
                is_dir: true,
                size: 0,
                source_full_path: src.clone(),
            });
            walk_local_dir(p, &name, &mut items, cancelled)?;
        } else {
            items.push(TransferItem {
                relative_path: name,
                is_dir: false,
                size: meta.len(),
                source_full_path: src.clone(),
            });
        }
    }
    Ok(items)
}

fn walk_local_dir(
    dir: &std::path::Path,
    rel_prefix: &str,
    items: &mut Vec<TransferItem>,
    cancelled: &Arc<AtomicBool>,
) -> Result<(), String> {
    if cancelled.load(Ordering::Relaxed) {
        return Err("Transfer cancelled by user".to_string());
    }
    let entries = std::fs::read_dir(dir).map_err(|e| format!("Cannot read directory {:?}: {}", dir, e))?;
    for entry in entries.flatten() {
        if cancelled.load(Ordering::Relaxed) {
            return Err("Transfer cancelled by user".to_string());
        }
        let p = entry.path();
        let fname = match p.file_name().and_then(|n| n.to_str()) {
            Some(n) => n.to_string(),
            None => continue,
        };
        let rel_path = format!("{}/{}", rel_prefix, fname);
        let meta = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };

        if meta.is_dir() {
            items.push(TransferItem {
                relative_path: rel_path.clone(),
                is_dir: true,
                size: 0,
                source_full_path: p.to_string_lossy().to_string(),
            });
            walk_local_dir(&p, &rel_path, items, cancelled)?;
        } else {
            items.push(TransferItem {
                relative_path: rel_path,
                is_dir: false,
                size: meta.len(),
                source_full_path: p.to_string_lossy().to_string(),
            });
        }
    }
    Ok(())
}

#[allow(dead_code)]
pub fn collect_remote_items(
    client: &mut RemoteClient,
    sources: &[String],
    cancelled: &Arc<AtomicBool>,
) -> Result<Vec<TransferItem>, String> {
    collect_remote_items_with_progress(client, sources, cancelled, &|_| {})
}

/// Same as [`collect_remote_items`], but reports how many items have been
/// enumerated so far. A deep tree on a slow link takes a long time and used to
/// sit behind a single "Scanning..." line, which reads as a hung connection.
pub fn collect_remote_items_with_progress(
    client: &mut RemoteClient,
    sources: &[String],
    cancelled: &Arc<AtomicBool>,
    progress: &dyn Fn(usize),
) -> Result<Vec<TransferItem>, String> {
    let mut items = Vec::new();
    for src in sources {
        if cancelled.load(Ordering::Relaxed) {
            return Err("Transfer cancelled by user".to_string());
        }
        let raw = src.trim();
        if raw.is_empty() {
            continue;
        }
        let clean = raw.trim_end_matches('/');
        let clean_path = if clean.is_empty() { "/" } else { clean };

        let stat_item = client.stat(clean_path)?;
        let name = clean_path.rsplit('/').next().unwrap_or(clean_path).to_string();

        if let Some(item) = stat_item {
            if item.is_dir {
                items.push(TransferItem {
                    relative_path: name.clone(),
                    is_dir: true,
                    size: 0,
                    source_full_path: clean_path.to_string(),
                });
                walk_remote_dir(client, clean_path, &name, &mut items, cancelled, progress)?;
            } else {
                items.push(TransferItem {
                    relative_path: name,
                    is_dir: false,
                    size: item.size,
                    source_full_path: clean_path.to_string(),
                });
            }
        } else {
            // Assume single file if stat couldn't determine
            items.push(TransferItem {
                relative_path: name,
                is_dir: false,
                size: 0,
                source_full_path: clean_path.to_string(),
            });
        }
    }
    Ok(items)
}

fn walk_remote_dir(
    client: &mut RemoteClient,
    dir_path: &str,
    rel_prefix: &str,
    items: &mut Vec<TransferItem>,
    cancelled: &Arc<AtomicBool>,
    progress: &dyn Fn(usize),
) -> Result<(), String> {
    if cancelled.load(Ordering::Relaxed) {
        return Err("Transfer cancelled by user".to_string());
    }
    let list = client.list(Some(dir_path))?;
    for entry in list {
        if cancelled.load(Ordering::Relaxed) {
            return Err("Transfer cancelled by user".to_string());
        }
        if entry.name == "." || entry.name == ".." {
            continue;
        }
        let rel_path = format!("{}/{}", rel_prefix, entry.name);
        let full_path = entry.path.clone();

        if entry.is_dir {
            items.push(TransferItem {
                relative_path: rel_path.clone(),
                is_dir: true,
                size: 0,
                source_full_path: full_path.clone(),
            });
            walk_remote_dir(client, &full_path, &rel_path, items, cancelled, progress)?;
        } else {
            items.push(TransferItem {
                relative_path: rel_path,
                is_dir: false,
                size: entry.size,
                source_full_path: full_path,
            });
        }
    }
    progress(items.len());
    Ok(())
}

fn strip_ansi_codes(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut in_escape = false;
    for c in input.chars() {
        if c == '\x1b' {
            in_escape = true;
        } else if in_escape {
            if c.is_ascii_alphabetic() || c == '~' {
                in_escape = false;
            }
        } else if c != '\r' {
            out.push(c);
        }
    }
    out
}

fn parse_size_with_unit(s: &str) -> Option<u64> {
    let s = s.trim().replace(',', "");
    let (num_str, mult) = if s.ends_with("TB") || s.ends_with("TiB") {
        (s.trim_end_matches(|c: char| c.is_alphabetic()), 1024u64 * 1024 * 1024 * 1024)
    } else if s.ends_with("GB") || s.ends_with("GiB") {
        (s.trim_end_matches(|c: char| c.is_alphabetic()), 1024u64 * 1024 * 1024)
    } else if s.ends_with("MB") || s.ends_with("MiB") {
        (s.trim_end_matches(|c: char| c.is_alphabetic()), 1024u64 * 1024)
    } else if s.ends_with("KB") || s.ends_with("KiB") {
        (s.trim_end_matches(|c: char| c.is_alphabetic()), 1024u64)
    } else if s.ends_with('B') {
        (s.trim_end_matches('B'), 1u64)
    } else if let Ok(n) = s.parse::<u64>() {
        return Some(n);
    } else {
        return None;
    };

    if let Ok(num) = num_str.parse::<f64>() {
        Some((num * mult as f64) as u64)
    } else {
        None
    }
}

pub fn parse_scp_progress_line(chunk: &str) -> Option<(f64, Option<String>, Option<String>, Option<u64>)> {
    let clean = strip_ansi_codes(chunk);
    let mut pct_opt: Option<f64> = None;
    let mut speed_opt: Option<String> = None;
    let mut file_opt: Option<String> = None;
    let mut bytes_opt: Option<u64> = None;

    for line in clean.lines().chain(clean.split('\r')) {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let tokens: Vec<&str> = trimmed.split_whitespace().collect();
        for (i, &t) in tokens.iter().enumerate() {
            if t.ends_with('%') && t.len() >= 2 {
                if let Ok(p) = t[..t.len() - 1].parse::<f64>() {
                    if p >= 0.0 && p <= 100.0 {
                        pct_opt = Some(p);
                        if i > 0 && file_opt.is_none() {
                            let candidate = tokens[0];
                            if !candidate.ends_with('%') && !candidate.contains("/s") && candidate != "ETA" {
                                file_opt = Some(candidate.to_string());
                            }
                        }
                    }
                }
            }
            if (t.ends_with("/s") || t.ends_with("/S")) && t.len() >= 4 {
                speed_opt = Some(t.to_string());
            }
            if (t.ends_with("KB") || t.ends_with("MB") || t.ends_with("GB") || t.ends_with("TB")) && bytes_opt.is_none() {
                if let Some(b) = parse_size_with_unit(t) {
                    bytes_opt = Some(b);
                }
            }
        }
    }

    pct_opt.map(|p| (p, speed_opt, file_opt, bytes_opt))
}

pub fn build_direct_transfer_command(task: &RemoteTransferTask) -> (String, String) {
    let sources_joined = task
        .sources
        .iter()
        .map(|s| format!("'{}'", s.replace('\'', "'\\''")))
        .collect::<Vec<_>>()
        .join(" ");

    let clean_target_dir = {
        let trimmed = task.target_path.trim();
        if trimmed.is_empty() || trimmed == "." {
            "./".to_string()
        } else if trimmed.ends_with('/') {
            trimmed.to_string()
        } else {
            format!("{}/", trimmed)
        }
    };
    let target_dest = format!(
        "{}@{}:'{}'",
        task.target_user,
        task.target_host,
        clean_target_dir.replace('\'', "'\\''")
    );

    // `accept-new` still trusts the key on first contact (there is no
    // way to pre-seed a remote-to-remote host key) but it refuses a
    // changed key, which is what stops a silent MITM on re-connect.
    // `no` was disabling that check entirely.
    //
    // IMPORTANT: When authenticating with a password via `sshpass`, we MUST NOT
    // pass `-o BatchMode=yes` because OpenSSH disables password prompts completely
    // in BatchMode, causing `scp`/`ssh` to fail immediately with
    // `Permission denied (publickey,password)`. Instead, disable pubkey probing
    // so `sshpass` receives the password prompt immediately.
    let has_password = !task.target_pass.is_empty();
    let ssh_opts = if has_password {
        "-o StrictHostKeyChecking=accept-new -o PubkeyAuthentication=no -o PreferredAuthentications=password,keyboard-interactive -o ConnectTimeout=20"
    } else {
        "-o BatchMode=yes -o StrictHostKeyChecking=accept-new -o ConnectTimeout=20"
    };

    // The password never goes on the sshpass command line (`-p`
    // exposes it to `ps` for every local user) and never reaches the
    // UI log. It is written to a 0600 file, consumed with `-f`, and
    // removed immediately afterwards.
    if task.transfer_mode == "direct_rsync" {
        if has_password {
            let esc_pass = task.target_pass.replace('\'', "'\\''");
            let cmd = format!(
                "umask 077; __p=$(mktemp /tmp/.90ftp.XXXXXX); printf '%s' '{p}' > \"$__p\"; \
sshpass -f \"$__p\" rsync -avz --info=progress2 -e 'ssh -p {port} {opts}' {src} {dst}; __rc=$?; rm -f \"$__p\"; exit $__rc",
                p = esc_pass,
                port = task.target_port,
                opts = ssh_opts,
                src = sources_joined,
                dst = target_dest
            );
            let shown = format!(
                "sshpass -f <secure-temp-file> rsync -avz --info=progress2 -e 'ssh -p {} {}' {} {}",
                task.target_port, ssh_opts, sources_joined, target_dest
            );
            (cmd, shown)
        } else {
            let cmd = format!(
                "rsync -avz --info=progress2 -e 'ssh -p {} {}' {} {}",
                task.target_port, ssh_opts, sources_joined, target_dest
            );
            (cmd.clone(), cmd)
        }
    } else if has_password {
        let esc_pass = task.target_pass.replace('\'', "'\\''");
        let cmd = format!(
            "umask 077; __p=$(mktemp /tmp/.90ftp.XXXXXX); printf '%s' '{p}' > \"$__p\"; \
sshpass -f \"$__p\" scp -P {port} -r {opts} {src} {dst}; __rc=$?; rm -f \"$__p\"; exit $__rc",
            p = esc_pass,
            port = task.target_port,
            opts = ssh_opts,
            src = sources_joined,
            dst = target_dest
        );
        let shown = format!(
            "sshpass -f <secure-temp-file> scp -P {} -r {} {} {}",
            task.target_port, ssh_opts, sources_joined, target_dest
        );
        (cmd, shown)
    } else {
        let cmd = format!(
            "scp -P {} -r {} {} {}",
            task.target_port, ssh_opts, sources_joined, target_dest
        );
        (cmd.clone(), cmd)
    }
}

pub fn execute_transfer_thread(
    mut source_client: Option<RemoteClient>,
    task: RemoteTransferTask,
    cancelled: Arc<AtomicBool>,
    exec_script: Option<ScriptCallback>,
) {
    let emit_log = |msg: &str, kind: &str| {
        if let Some(ref cb) = exec_script {
            // Redact before the text leaves Rust: the transfer log is visible on
            // screen and is the one place a leaked credential would be copied
            // straight into a screenshot or a support ticket.
            let payload = json!({ "text": crate::logger::redact(msg), "kind": kind });
            let json_str = serde_json::to_string(&payload).unwrap_or_else(|_| "{}".to_string());
            cb(format!(
                "if(window.onRemoteTransferLog) window.onRemoteTransferLog({});",
                json_str
            ));
        }
    };

    let emit_progress = |file: &str, done_files: usize, total_files: usize, done_bytes: u64, total_bytes: u64, speed_str: &str, override_pct: Option<f64>| {
        if let Some(ref cb) = exec_script {
            let pct = if let Some(p) = override_pct {
                p.max(0.0).min(100.0)
            } else if total_bytes > 0 {
                (done_bytes as f64 / total_bytes as f64 * 100.0).min(100.0)
            } else if total_files > 0 {
                (done_files as f64 / total_files as f64 * 100.0).min(100.0)
            } else {
                100.0
            };
            let payload = json!({
                "percent": pct,
                "current_file": file,
                "done_files": done_files,
                "total_files": total_files,
                "done_bytes": done_bytes,
                "total_bytes": total_bytes,
                "speed": speed_str
            });
            let json_str = serde_json::to_string(&payload).unwrap_or_else(|_| "{}".to_string());
            cb(format!(
                "if(window.onRemoteTransferProgress) window.onRemoteTransferProgress({});",
                json_str
            ));
        }
    };

    let emit_complete = |success: bool, msg: &str, count: usize, bytes: u64, secs: f64| {
        if let Some(ref cb) = exec_script {
            let payload = json!({
                "success": success,
                "message": msg,
                "count": count,
                "bytes": bytes,
                "elapsed_secs": secs
            });
            let json_str = serde_json::to_string(&payload).unwrap_or_else(|_| "{}".to_string());
            cb(format!(
                "if(window.onRemoteTransferComplete) window.onRemoteTransferComplete({});",
                json_str
            ));
        }
    };

    let emit_error = |err: &str| {
        if let Some(ref cb) = exec_script {
            let payload = json!({ "error": crate::logger::redact(err) });
            let json_str = serde_json::to_string(&payload).unwrap_or_else(|_| "{}".to_string());
            cb(format!(
                "if(window.onRemoteTransferError) window.onRemoteTransferError({});",
                json_str
            ));
        }
    };

    emit_log(&format!("Starting remote transfer to {}@{}:{} ({})", task.target_user, task.target_host, task.target_port, task.target_protocol.to_uppercase()), "info");

    // Pre-connect to destination server so we can:
    // 1. Validate target credentials upfront and ensure `target_path` directory exists before SCP/rsync runs
    // 2. Reuse the connection immediately for Stream Relay Bridge if direct transfer is unavailable
    let dest_clean = task.target_path.trim().replace('\\', "/");
    let dest_base = if dest_clean.is_empty() { ".".to_string() } else { dest_clean };
    let mut pre_target_client: Option<RemoteClient> = match RemoteClient::connect(
        &task.target_protocol,
        &task.target_host,
        task.target_port,
        &task.target_user,
        &task.target_pass,
        &task.target_auth_type,
        task.target_key_path.as_deref(),
        task.target_passphrase.as_deref(),
        true,
    ) {
        Ok(mut c) => {
            if dest_base != "." && dest_base != "/" && dest_base != "~" {
                let _ = c.make_dir_all(&dest_base);
            }
            Some(c)
        }
        Err(e) => {
            emit_log(&format!("Notice: Target pre-check could not connect directly ({})", e), "warning");
            None
        }
    };

    // 1. Direct Server-to-Server Attempt (if source is remote SSH, target is SSH, and mode is direct or auto)
    if task.source_is_remote && (task.target_protocol == "scp" || task.target_protocol == "sftp" || task.target_protocol == "ssh") {
        if let Some(ref src) = source_client {
            /* `scp` runs on the *source* server, so the credential that matters
               there is not the one saved in this dialog: a private key stored on
               this PC is invisible to that command. That is exactly why key-only
               sites always ended in "Permission denied (publickey)" after a
               wasted round trip. What the source server *can* use is either the
               password we hand to sshpass, or a key that already lives on that
               server — so probe for the second instead of assuming it. */
            let wants_direct = task.transfer_mode == "direct_scp"
                || task.transfer_mode == "direct_rsync"
                || task.transfer_mode == "auto";
            let password_auth = !task.target_pass.is_empty();

            let mut server_side_key = false;
            if wants_direct && !password_auth {
                let probe = format!(
                    "ssh -o BatchMode=yes -o ConnectTimeout=8 -o StrictHostKeyChecking=accept-new -p {} {}@{} true",
                    task.target_port, task.target_user, task.target_host
                );
                server_side_key = matches!(src.exec_command_clean(&probe), Ok((0, _)));
            }

            if wants_direct && !password_auth && !server_side_key {
                emit_log(
                    "Direct transfer cannot use the SSH key saved for this site: the key is stored on this computer, but the copy command runs on the source server. Continuing with the Stream Relay Bridge, which uses these credentials directly. Add a password to the site to enable direct mode.",
                    "warning",
                );
            }

            if wants_direct && (password_auth || server_side_key) {
                emit_log("Attempting direct Server-to-Server transfer via native remote command...", "info");

                // If password authentication is required for direct SCP/rsync, check for sshpass first
                let mut can_try_direct = true;
                let has_password = !task.target_pass.is_empty();
                if has_password {
                    let mut has_sshpass = match src.exec_command_clean("command -v sshpass || which sshpass") {
                        Ok((0, out)) => !out.trim().is_empty() && !out.contains("not found"),
                        _ => false,
                    };

                    if !has_sshpass {
                        emit_log("Notice: 'sshpass' is not installed on source server. Attempting automatic installation...", "info");
                        let auto_install_cmd = "if command -v apt-get >/dev/null 2>&1; then DEBIAN_FRONTEND=noninteractive apt-get update -qq && DEBIAN_FRONTEND=noninteractive apt-get install -y -qq sshpass; \
elif command -v dnf >/dev/null 2>&1; then (dnf install -y -q epel-release || true) && dnf install -y -q sshpass; \
elif command -v yum >/dev/null 2>&1; then (yum install -y -q epel-release || true) && yum install -y -q sshpass; \
elif command -v apk >/dev/null 2>&1; then apk add --no-cache sshpass; \
elif command -v pacman >/dev/null 2>&1; then pacman -Sy --noconfirm sshpass; \
elif command -v zypper >/dev/null 2>&1; then zypper --non-interactive install sshpass; fi; command -v sshpass || which sshpass";
                        if let Ok((0, out)) = src.exec_command_clean(auto_install_cmd) {
                            if out.lines().any(|l| l.contains("sshpass") && !l.contains("not found")) {
                                has_sshpass = true;
                                emit_log("✓ Automatically installed 'sshpass' on source server!", "success");
                            }
                        }
                    }

                    if !has_sshpass {
                        can_try_direct = false;
                        emit_log("Notice: 'sshpass' utility is not available on the source server.", "warning");
                        if task.transfer_mode == "auto" {
                            emit_log("Direct password transfer requires sshpass. Falling back smoothly to Universal Stream Relay Bridge...", "info");
                        } else {
                            let err_msg = "Direct Server-to-Server transfer with password requires 'sshpass' on the source server. Please click 'Install sshpass' in the Remote Transfer dialog or switch strategy to 'Stream Relay Bridge'.";
                            emit_log(err_msg, "error");
                            emit_error(err_msg);
                            return;
                        }
                    }
                }

                if can_try_direct {
                    let sources_joined = task.sources.iter().map(|s| format!("'{}'", s.replace('\'', "'\\''"))).collect::<Vec<_>>().join(" ");
                    let (direct_cmd, display_cmd) = build_direct_transfer_command(&task);

                    let total_files = task.sources.len();
                    let first_file = task.sources.first()
                        .map(|s| s.rsplit('/').next().unwrap_or(s))
                        .unwrap_or("transfer")
                        .to_string();

                    // Probe total size on source server for real-time progress calculations
                    let mut total_bytes = 0u64;
                    let size_probe_cmd = format!("stat -c %s {} 2>/dev/null || du -sb {} 2>/dev/null | cut -f1", sources_joined, sources_joined);
                    if let Ok((0, out)) = src.exec_command_clean(&size_probe_cmd) {
                        if let Some(line) = out.lines().next() {
                            if let Ok(b) = line.trim().parse::<u64>() {
                                total_bytes = b;
                            }
                        }
                    }

                    emit_progress(&first_file, 0, total_files, 0, total_bytes, "Connecting to target...", Some(0.0));
                    emit_log(&format!("Executing: {}", display_cmd), "info");

                    let direct_start_time = Instant::now();
                    let mut last_progress_tick = Instant::now();
                    let mut cur_file = first_file.clone();
                    let mut cur_speed = "Transferring...".to_string();
                    let mut cur_done_bytes = 0u64;

                    let exec_res = src.exec_command_stream_pty(
                        &direct_cmd,
                        Some(cancelled.clone()),
                        |chunk: &str| {
                            if let Some((pct, spd_opt, file_opt, bytes_opt)) = parse_scp_progress_line(chunk) {
                                if let Some(s) = spd_opt {
                                    cur_speed = s;
                                }
                                if let Some(f) = file_opt {
                                    cur_file = f;
                                }
                                if let Some(b) = bytes_opt {
                                    cur_done_bytes = b;
                                } else if total_bytes > 0 {
                                    cur_done_bytes = (total_bytes as f64 * (pct / 100.0)) as u64;
                                }

                                if last_progress_tick.elapsed().as_millis() >= 200 || pct >= 100.0 {
                                    last_progress_tick = Instant::now();
                                    emit_progress(
                                        &cur_file,
                                        if pct >= 100.0 { total_files } else { 0 },
                                        total_files,
                                        cur_done_bytes,
                                        total_bytes,
                                        &cur_speed,
                                        Some(pct),
                                    );
                                }
                            }
                        },
                    );

                    match exec_res {
                        Ok((code, out)) => {
                            if cancelled.load(Ordering::Relaxed) {
                                emit_log("Transfer cancelled by user.", "warning");
                                emit_complete(false, "Transfer cancelled by user", 0, 0, direct_start_time.elapsed().as_secs_f64());
                                return;
                            }

                            let out_lower = out.to_lowercase();
                            let is_clean_success = code == 0
                                && !out_lower.contains("command not found")
                                && !out_lower.contains("not found")
                                && !out_lower.contains("permission denied")
                                && !out_lower.contains("connection refused")
                                && !out_lower.contains("no such file")
                                && !out_lower.contains("host key verification failed");

                            if is_clean_success {
                                emit_progress(
                                    &first_file,
                                    total_files,
                                    total_files,
                                    total_bytes,
                                    total_bytes,
                                    "Completed",
                                    Some(100.0),
                                );
                                emit_log("Direct Server-to-Server command finished successfully!", "success");
                                if !out.trim().is_empty() {
                                    emit_log(&format!("Output:\n{}", out.trim()), "info");
                                }
                                emit_complete(
                                    true,
                                    "Direct Server-to-Server transfer completed successfully",
                                    total_files,
                                    total_bytes,
                                    direct_start_time.elapsed().as_secs_f64(),
                                );
                                return;
                            }

                            /* A key-auth refusal is the normal outcome when the
                               source server holds no authorized key for the
                               destination: the relay below handles the file
                               anyway. Printing the raw `scp` error made this
                               look like a broken connection. */
                            let detail = out.trim();
                            let auth_refused = detail.contains("Permission denied")
                                || detail.contains("publickey")
                                || detail.contains("Authentication failed");

                            let err_msg = if auth_refused {
                                "Direct transfer was refused by the destination: the source server has no SSH key authorized there.".to_string()
                            } else if code != 0 {
                                format!("Direct command exited with code {}: {}", code, detail)
                            } else {
                                format!("Direct command failed: {}", detail)
                            };

                            emit_log(&err_msg, if auth_refused { "info" } else { "warning" });
                            // A refused credential is not a transfer failure: the
                            // relay below authenticates from this machine, which is
                            // where the saved credentials actually live. Only a real
                            // transfer error should stop an explicit direct run.
                            if task.transfer_mode != "auto" && !auth_refused {
                                emit_error(&err_msg);
                                return;
                            }
                            emit_log(
                                if auth_refused {
                                    "Continuing with the Stream Relay Bridge, which uses the credentials entered here — nothing else to configure."
                                } else {
                                    "Direct Server-to-Server execution failed. Falling back to Universal Stream Relay Bridge..."
                                },
                                "info",
                            );
                        }
                        Err(e) => {
                            if cancelled.load(Ordering::Relaxed) {
                                emit_log("Transfer cancelled by user.", "warning");
                                emit_complete(false, "Transfer cancelled by user", 0, 0, direct_start_time.elapsed().as_secs_f64());
                                return;
                            }
                            emit_log(&format!("Could not execute direct command: {}. Falling back to Stream Relay...", e), "warning");
                            if task.transfer_mode != "auto" {
                                emit_error(&e);
                                return;
                            }
                        }
                    }
                }
            }
        }
    }

    // 2. Universal Stream Relay Bridge
    emit_log("Connecting to target server for Stream Relay transfer...", "info");

    let mut target_client = match pre_target_client.take() {
        Some(c) => c,
        None => match RemoteClient::connect(
            &task.target_protocol,
            &task.target_host,
            task.target_port,
            &task.target_user,
            &task.target_pass,
            &task.target_auth_type,
            task.target_key_path.as_deref(),
            task.target_passphrase.as_deref(),
            true,
        ) {
            Ok(mut c) => {
                if dest_base != "." && dest_base != "/" && dest_base != "~" {
                    if let Err(e) = c.make_dir_all(&dest_base) {
                        emit_log(&format!("Notice: make_dir_all on {}: {}", dest_base, e), "info");
                    }
                }
                c
            }
            Err(e) => {
                emit_error(&format!("Failed to connect to destination server: {}", e));
                return;
            }
        },
    };

    emit_log("Connected to destination server! Ensuring target directory exists...", "info");
    if dest_base != "." && dest_base != "/" && dest_base != "~" {
        let _ = target_client.make_dir_all(&dest_base);
    }

    emit_log("Scanning and enumerating source files and folders...", "info");

    /* A deep tree can take minutes on a slow link, and one static log line
       reads as a dead connection. Report the running count instead. */
    let last_scan_report = std::cell::Cell::new(Instant::now());
    let scan_progress = |found: usize| {
        if last_scan_report.get().elapsed().as_millis() >= 900 {
            last_scan_report.set(Instant::now());
            emit_log(
                &format!("Scanning source tree... {} item(s) found so far", found),
                "info",
            );
        }
    };

    let items = if task.source_is_remote {
        if let Some(ref mut src) = source_client {
            match collect_remote_items_with_progress(src, &task.sources, &cancelled, &scan_progress) {
                Ok(it) => it,
                Err(e) => {
                    emit_error(&format!("Failed to enumerate remote sources: {}", e));
                    return;
                }
            }
        } else {
            emit_error("Source remote server is not connected");
            return;
        }
    } else {
        match collect_local_items(&task.sources, &cancelled) {
            Ok(it) => it,
            Err(e) => {
                emit_error(&format!("Failed to enumerate local sources: {}", e));
                return;
            }
        }
    };

    let total_files = items.iter().filter(|i| !i.is_dir).count();
    let total_bytes: u64 = items.iter().filter(|i| !i.is_dir).map(|i| i.size).sum();

    emit_log(
        &format!(
            "Ready to transfer {} items ({} files, total size: {})",
            items.len(),
            total_files,
            format_size(total_bytes)
        ),
        "info",
    );

    let start_time = Instant::now();
    let mut done_files = 0;
    let mut done_bytes: u64 = 0;
    let mut failed_files = 0;

    for item in &items {
        if cancelled.load(Ordering::Relaxed) {
            emit_log("Transfer cancelled by user.", "warning");
            emit_complete(false, "Transfer cancelled by user", done_files, done_bytes, start_time.elapsed().as_secs_f64());
            return;
        }

        let target_item_path = if dest_base == "." || dest_base.is_empty() || dest_base == "~" {
            item.relative_path.clone()
        } else {
            format!("{}/{}", dest_base.trim_end_matches('/'), item.relative_path)
        };

        if item.is_dir {
            if let Err(e) = target_client.make_dir_all(&target_item_path) {
                emit_log(&format!("Directory create note ({}): {}", target_item_path, e), "info");
            }
        } else {
            // Check overwrite skip option
            if task.overwrite == "skip" {
                if let Ok(Some(_)) = target_client.stat(&target_item_path) {
                    emit_log(&format!("↷ Skipped (already exists): {}", item.relative_path), "info");
                    done_files += 1;
                    done_bytes += item.size;
                    continue;
                }
            }

            // Ensure parent directory of target file exists
            if let Some(idx) = target_item_path.rfind('/') {
                let parent_dir = &target_item_path[..idx];
                if !parent_dir.is_empty() && parent_dir != "." && parent_dir != "/" && parent_dir != "~" {
                    let _ = target_client.make_dir_all(parent_dir);
                }
            }

            let base_done_bytes = done_bytes;
            let file_size = item.size;
            emit_progress(&item.relative_path, done_files, total_files, base_done_bytes, total_bytes, "Starting...", None);

            let mut transfer_res: Result<(), String> = Err("Not started".to_string());
            for attempt in 1..=3 {
                if cancelled.load(Ordering::Relaxed) {
                    transfer_res = Err("Transfer cancelled by user".to_string());
                    break;
                }

                if attempt > 1 {
                    emit_log(&format!("↻ Retrying transfer (attempt {}/3): {}", attempt, item.relative_path), "warning");
                    if let Ok(reconn) = RemoteClient::connect(
                        &task.target_protocol,
                        &task.target_host,
                        task.target_port,
                        &task.target_user,
                        &task.target_pass,
                        &task.target_auth_type,
                        task.target_key_path.as_deref(),
                        task.target_passphrase.as_deref(),
                        true,
                    ) {
                        target_client = reconn;
                    }
                }

                transfer_res = if task.source_is_remote {
                    if let Some(ref mut src) = source_client {
                        let temp_file_path = std::env::temp_dir().join(format!("90ftp_relay_{}_{}.tmp", std::process::id(), done_files));
                        let temp_str = temp_file_path.to_string_lossy().to_string();

                        // Stage 1/2: Download from source server with live progress & cancellation
                        let dl_start = Instant::now();
                        let dl_last_tick = std::sync::Mutex::new(Instant::now());
                        let dl_cancel_slot: Arc<std::sync::Mutex<Option<crate::transfer_control::ControlHandle>>> =
                            Arc::new(std::sync::Mutex::new(None));
                        let dl_cancel_slot_cb = dl_cancel_slot.clone();
                        let cancelled_dl = cancelled.clone();
                        let exec_dl = exec_script.clone();
                        let rel_dl = item.relative_path.clone();

                        let dl_ctl = crate::transfer_control::TransferControl::with_progress(
                            file_size,
                            Arc::new(move |done, _tot| {
                                if cancelled_dl.load(Ordering::Relaxed) {
                                    if let Ok(guard) = dl_cancel_slot_cb.lock() {
                                        if let Some(ref h) = *guard {
                                            h.cancel();
                                        }
                                    }
                                    return;
                                }
                                if let Ok(mut tick) = dl_last_tick.try_lock() {
                                    if tick.elapsed().as_millis() >= 200 || (file_size > 0 && done >= file_size) {
                                        *tick = Instant::now();
                                        if let Some(ref cb) = exec_dl {
                                            let el = dl_start.elapsed().as_secs_f64();
                                            let spd = if el > 0.05 { (done as f64 / (1024.0 * 1024.0)) / el } else { 0.0 };
                                            let stage_bytes = base_done_bytes + (done / 2);
                                            let pct = if total_bytes > 0 {
                                                (stage_bytes as f64 / total_bytes as f64 * 100.0).min(99.5)
                                            } else {
                                                50.0
                                            };
                                            let payload = json!({
                                                "percent": pct,
                                                "current_file": format!("[Relay 1/2: Downloading] {}", rel_dl),
                                                "done_files": done_files,
                                                "total_files": total_files,
                                                "done_bytes": stage_bytes,
                                                "total_bytes": total_bytes,
                                                "speed": format!("{:.2} MB/s", spd)
                                            });
                                            if let Ok(s) = serde_json::to_string(&payload) {
                                                cb(format!("if(window.onRemoteTransferProgress) window.onRemoteTransferProgress({});", s));
                                            }
                                        }
                                    }
                                }
                            }),
                        );
                        *dl_cancel_slot.lock().unwrap() = Some(dl_ctl.handle());

                        match src.download_to_local_file_ctl(&item.source_full_path, &temp_str, &dl_ctl) {
                            Ok(actual_dl_bytes) => {
                                // Stage 2/2: Upload to target server with live progress & cancellation
                                let eff_size = if file_size > 0 { file_size } else { actual_dl_bytes };
                                let ul_start = Instant::now();
                                let ul_last_tick = std::sync::Mutex::new(Instant::now());
                                let ul_cancel_slot: Arc<std::sync::Mutex<Option<crate::transfer_control::ControlHandle>>> =
                                    Arc::new(std::sync::Mutex::new(None));
                                let ul_cancel_slot_cb = ul_cancel_slot.clone();
                                let cancelled_ul = cancelled.clone();
                                let exec_ul = exec_script.clone();
                                let rel_ul = item.relative_path.clone();

                                let ul_ctl = crate::transfer_control::TransferControl::with_progress(
                                    eff_size,
                                    Arc::new(move |done, _tot| {
                                        if cancelled_ul.load(Ordering::Relaxed) {
                                            if let Ok(guard) = ul_cancel_slot_cb.lock() {
                                                if let Some(ref h) = *guard {
                                                    h.cancel();
                                                }
                                            }
                                            return;
                                        }
                                        if let Ok(mut tick) = ul_last_tick.try_lock() {
                                            if tick.elapsed().as_millis() >= 200 || (eff_size > 0 && done >= eff_size) {
                                                *tick = Instant::now();
                                                if let Some(ref cb) = exec_ul {
                                                    let el = ul_start.elapsed().as_secs_f64();
                                                    let spd = if el > 0.05 { (done as f64 / (1024.0 * 1024.0)) / el } else { 0.0 };
                                                    let stage_bytes = base_done_bytes + (eff_size / 2) + (done / 2);
                                                    let pct = if total_bytes > 0 {
                                                        (stage_bytes as f64 / total_bytes as f64 * 100.0).min(99.9)
                                                    } else {
                                                        90.0
                                                    };
                                                    let payload = json!({
                                                        "percent": pct,
                                                        "current_file": format!("[Relay 2/2: Uploading] {}", rel_ul),
                                                        "done_files": done_files,
                                                        "total_files": total_files,
                                                        "done_bytes": stage_bytes,
                                                        "total_bytes": total_bytes,
                                                        "speed": format!("{:.2} MB/s", spd)
                                                    });
                                                    if let Ok(s) = serde_json::to_string(&payload) {
                                                        cb(format!("if(window.onRemoteTransferProgress) window.onRemoteTransferProgress({});", s));
                                                    }
                                                }
                                            }
                                        }
                                    }),
                                );
                                *ul_cancel_slot.lock().unwrap() = Some(ul_ctl.handle());

                                let res = target_client.upload_from_local_file_ctl(&temp_str, &target_item_path, &ul_ctl).map(|_| ());
                                let _ = std::fs::remove_file(&temp_file_path);
                                res
                            }
                            Err(e) => {
                                let _ = std::fs::remove_file(&temp_file_path);
                                Err(format!("Download failed for {}: {}", item.source_full_path, e))
                            }
                        }
                    } else {
                        Err("Source client disconnected".to_string())
                    }
                } else {
                    let ul_start = Instant::now();
                    let ul_last_tick = std::sync::Mutex::new(Instant::now());
                    let ul_cancel_slot: Arc<std::sync::Mutex<Option<crate::transfer_control::ControlHandle>>> =
                        Arc::new(std::sync::Mutex::new(None));
                    let ul_cancel_slot_cb = ul_cancel_slot.clone();
                    let cancelled_ul = cancelled.clone();
                    let exec_ul = exec_script.clone();
                    let rel_ul = item.relative_path.clone();

                    let ul_ctl = crate::transfer_control::TransferControl::with_progress(
                        file_size,
                        Arc::new(move |done, _tot| {
                            if cancelled_ul.load(Ordering::Relaxed) {
                                if let Ok(guard) = ul_cancel_slot_cb.lock() {
                                    if let Some(ref h) = *guard {
                                        h.cancel();
                                    }
                                }
                                return;
                            }
                            if let Ok(mut tick) = ul_last_tick.try_lock() {
                                if tick.elapsed().as_millis() >= 200 || (file_size > 0 && done >= file_size) {
                                    *tick = Instant::now();
                                    if let Some(ref cb) = exec_ul {
                                        let el = ul_start.elapsed().as_secs_f64();
                                        let spd = if el > 0.05 { (done as f64 / (1024.0 * 1024.0)) / el } else { 0.0 };
                                        let cur_bytes = base_done_bytes + done;
                                        let pct = if total_bytes > 0 {
                                            (cur_bytes as f64 / total_bytes as f64 * 100.0).min(99.9)
                                        } else {
                                            90.0
                                        };
                                        let payload = json!({
                                            "percent": pct,
                                            "current_file": rel_ul.clone(),
                                            "done_files": done_files,
                                            "total_files": total_files,
                                            "done_bytes": cur_bytes,
                                            "total_bytes": total_bytes,
                                            "speed": format!("{:.2} MB/s", spd)
                                        });
                                        if let Ok(s) = serde_json::to_string(&payload) {
                                            cb(format!("if(window.onRemoteTransferProgress) window.onRemoteTransferProgress({});", s));
                                        }
                                    }
                                }
                            }
                        }),
                    );
                    *ul_cancel_slot.lock().unwrap() = Some(ul_ctl.handle());
                    target_client.upload_from_local_file_ctl(&item.source_full_path, &target_item_path, &ul_ctl).map(|_| ())
                };

                if transfer_res.is_ok() || cancelled.load(Ordering::Relaxed) {
                    break;
                }
            }

            if cancelled.load(Ordering::Relaxed) {
                emit_log("Transfer cancelled by user.", "warning");
                emit_complete(false, "Transfer cancelled by user", done_files, done_bytes, start_time.elapsed().as_secs_f64());
                return;
            }

            match transfer_res {
                Ok(_) => {
                    done_files += 1;
                    done_bytes += item.size;
                    let elapsed = start_time.elapsed().as_secs_f64();
                    let speed_mb = if elapsed > 0.05 {
                        (done_bytes as f64 / (1024.0 * 1024.0)) / elapsed
                    } else {
                        0.0
                    };
                    emit_progress(&item.relative_path, done_files, total_files, done_bytes, total_bytes, &format!("{:.2} MB/s", speed_mb), None);
                    emit_log(&format!("✓ Transferred: {} ({})", item.relative_path, format_size(item.size)), "success");
                }
                Err(e) => {
                    failed_files += 1;
                    emit_log(&format!("✕ Failed to transfer {}: {}", item.relative_path, e), "error");
                }
            }
        }
    }

    let elapsed_total = start_time.elapsed().as_secs_f64();
    let is_all_success = failed_files == 0;
    let completion_msg = if is_all_success {
        format!(
            "Transfer session completed successfully! Transferred {}/{} files ({} in {:.1}s)",
            done_files,
            total_files,
            format_size(done_bytes),
            elapsed_total
        )
    } else {
        format!(
            "Transfer finished with {} failures: {}/{} files succeeded ({} in {:.1}s)",
            failed_files,
            done_files,
            total_files,
            format_size(done_bytes),
            elapsed_total
        )
    };

    emit_log(&completion_msg, if is_all_success { "success" } else { "warning" });
    emit_complete(is_all_success, &completion_msg, done_files, done_bytes, elapsed_total);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    #[test]
    fn test_format_size() {
        assert_eq!(format_size(500), "500 B");
        assert_eq!(format_size(1024), "1.0 KB");
        assert_eq!(format_size(1024 * 1024), "1.00 MB");
        assert_eq!(format_size(1024 * 1024 * 1024), "1.00 GB");
    }

    #[test]
    fn test_collect_local_items() {
        let temp_dir = std::env::temp_dir().join("90ftp_rt_test");
        let _ = std::fs::create_dir_all(&temp_dir);
        let test_file = temp_dir.join("sample.txt");
        let _ = std::fs::write(&test_file, b"remote transfer test content");

        let cancelled = Arc::new(AtomicBool::new(false));
        let items = collect_local_items(&[test_file.to_string_lossy().to_string()], &cancelled).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].relative_path, "sample.txt");
        assert_eq!(items[0].size, 28);
        assert!(!items[0].is_dir);

        let _ = std::fs::remove_file(test_file);
        let _ = std::fs::remove_dir(temp_dir);
    }

    #[test]
    fn test_task_deserialization() {
        let json_data = serde_json::json!({
            "source_is_remote": true,
            "sources": ["/var/www/site/index.html"],
            "target_protocol": "scp",
            "target_host": "192.168.1.50",
            "target_port": 22,
            "target_user": "ubuntu",
            "target_path": "/var/backup"
        });

        let task: RemoteTransferTask = serde_json::from_value(json_data).unwrap();
        assert!(task.source_is_remote);
        assert_eq!(task.target_protocol, "scp");
        assert_eq!(task.transfer_mode, "auto");
        assert_eq!(task.overwrite, "overwrite");
        assert!(task.recursive);
    }

    #[test]
    fn test_direct_command_with_password_never_uses_batchmode() {
        let task = RemoteTransferTask {
            source_is_remote: true,
            sources: vec!["/home/memychat.tar.gz".to_string()],
            transfer_mode: "auto".to_string(),
            target_protocol: "sftp".to_string(),
            target_host: "84.46.254.56".to_string(),
            target_port: 22,
            target_user: "root".to_string(),
            target_pass: "MySecretPass123!".to_string(),
            target_auth_type: "password".to_string(),
            target_key_path: None,
            target_passphrase: None,
            target_path: "/home/memychat/htdocs/memychat.com".to_string(),
            recursive: true,
            overwrite: "overwrite".to_string(),
        };

        let (direct_cmd, display_cmd) = build_direct_transfer_command(&task);
        assert!(
            !direct_cmd.contains("BatchMode=yes"),
            "BatchMode=yes disables password prompts and breaks sshpass: {direct_cmd}"
        );
        assert!(
            direct_cmd.contains("StrictHostKeyChecking=accept-new"),
            "Must enforce StrictHostKeyChecking=accept-new: {direct_cmd}"
        );
        assert!(
            direct_cmd.contains("PubkeyAuthentication=no"),
            "Must disable pubkey probing when password is supplied: {direct_cmd}"
        );
        assert!(
            direct_cmd.contains("sshpass -f \"$__p\""),
            "Must pass password via secure 0600 file: {direct_cmd}"
        );
        assert!(
            !display_cmd.contains("MySecretPass123!"),
            "Display command must never expose the password: {display_cmd}"
        );
        assert!(
            direct_cmd.contains("'/home/memychat/htdocs/memychat.com/'"),
            "Target directory path should end with trailing slash: {direct_cmd}"
        );
    }

    #[test]
    fn test_direct_command_without_password_uses_batchmode() {
        let task = RemoteTransferTask {
            source_is_remote: true,
            sources: vec!["/home/memychat.tar.gz".to_string()],
            transfer_mode: "direct_scp".to_string(),
            target_protocol: "sftp".to_string(),
            target_host: "84.46.254.56".to_string(),
            target_port: 22,
            target_user: "root".to_string(),
            target_pass: String::new(),
            target_auth_type: "key".to_string(),
            target_key_path: None,
            target_passphrase: None,
            target_path: "/home/memychat/htdocs/memychat.com".to_string(),
            recursive: true,
            overwrite: "overwrite".to_string(),
        };

        let (direct_cmd, _display_cmd) = build_direct_transfer_command(&task);
        assert!(
            direct_cmd.contains("BatchMode=yes"),
            "Key-based direct transfer must use BatchMode=yes so it never hangs: {direct_cmd}"
        );
        assert!(
            direct_cmd.contains("StrictHostKeyChecking=accept-new"),
            "Must enforce StrictHostKeyChecking=accept-new: {direct_cmd}"
        );
        assert!(!direct_cmd.contains("sshpass"));
    }

    #[test]
    fn test_parse_scp_progress_line() {
        let scp_line = "backup-git.ysnapp.com-10.23.2026_18-11-47.tar.gz   45%  450MB  32.1MB/s   00:15 ETA\r";
        let res = parse_scp_progress_line(scp_line).expect("Should parse SCP progress line");
        assert_eq!(res.0, 45.0);
        assert_eq!(res.1.as_deref(), Some("32.1MB/s"));
        assert_eq!(res.2.as_deref(), Some("backup-git.ysnapp.com-10.23.2026_18-11-47.tar.gz"));
        assert_eq!(res.3, Some(450 * 1024 * 1024));

        let rsync_line = "   157,286,400  15%   32.10MB/s    0:00:25\n";
        let res2 = parse_scp_progress_line(rsync_line).expect("Should parse rsync line");
        assert_eq!(res2.0, 15.0);
        assert_eq!(res2.1.as_deref(), Some("32.10MB/s"));

        let ansi_line = "\x1b[2Kfile.zip  100%  1024KB  25.0MB/s  00:00\r";
        let res3 = parse_scp_progress_line(ansi_line).expect("Should parse line with ANSI codes");
        assert_eq!(res3.0, 100.0);
        assert_eq!(res3.1.as_deref(), Some("25.0MB/s"));
        assert_eq!(res3.2.as_deref(), Some("file.zip"));
        assert_eq!(res3.3, Some(1024 * 1024));
    }
}
