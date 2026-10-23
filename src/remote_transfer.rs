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

pub fn collect_remote_items(
    client: &mut RemoteClient,
    sources: &[String],
    cancelled: &Arc<AtomicBool>,
) -> Result<Vec<TransferItem>, String> {
    let mut items = Vec::new();
    for src in sources {
        if cancelled.load(Ordering::Relaxed) {
            return Err("Transfer cancelled by user".to_string());
        }
        let clean = src.trim();
        if clean.is_empty() {
            continue;
        }

        let stat_item = client.stat(clean)?;
        let name = clean.rsplit('/').next().unwrap_or(clean).to_string();

        if let Some(item) = stat_item {
            if item.is_dir {
                items.push(TransferItem {
                    relative_path: name.clone(),
                    is_dir: true,
                    size: 0,
                    source_full_path: clean.to_string(),
                });
                walk_remote_dir(client, clean, &name, &mut items, cancelled)?;
            } else {
                items.push(TransferItem {
                    relative_path: name,
                    is_dir: false,
                    size: item.size,
                    source_full_path: clean.to_string(),
                });
            }
        } else {
            // Assume single file if stat couldn't determine
            items.push(TransferItem {
                relative_path: name,
                is_dir: false,
                size: 0,
                source_full_path: clean.to_string(),
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
            walk_remote_dir(client, &full_path, &rel_path, items, cancelled)?;
        } else {
            items.push(TransferItem {
                relative_path: rel_path,
                is_dir: false,
                size: entry.size,
                source_full_path: full_path,
            });
        }
    }
    Ok(())
}

pub fn execute_transfer_thread(
    mut source_client: Option<RemoteClient>,
    task: RemoteTransferTask,
    cancelled: Arc<AtomicBool>,
    exec_script: Option<ScriptCallback>,
) {
    let emit_log = |msg: &str, kind: &str| {
        if let Some(ref cb) = exec_script {
            let escaped = msg.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', " ");
            cb(format!(
                "if(window.onRemoteTransferLog) window.onRemoteTransferLog({{ text: \"{}\", kind: \"{}\" }});",
                escaped, kind
            ));
        }
    };

    let emit_progress = |file: &str, done_files: usize, total_files: usize, done_bytes: u64, total_bytes: u64, speed_str: &str| {
        if let Some(ref cb) = exec_script {
            let escaped = file.replace('\\', "\\\\").replace('"', "\\\"");
            let pct = if total_bytes > 0 {
                (done_bytes as f64 / total_bytes as f64 * 100.0).min(100.0)
            } else if total_files > 0 {
                (done_files as f64 / total_files as f64 * 100.0).min(100.0)
            } else {
                100.0
            };
            cb(format!(
                "if(window.onRemoteTransferProgress) window.onRemoteTransferProgress({{ percent: {:.1}, current_file: \"{}\", done_files: {}, total_files: {}, done_bytes: {}, total_bytes: {}, speed: \"{}\" }});",
                pct, escaped, done_files, total_files, done_bytes, total_bytes, speed_str
            ));
        }
    };

    let emit_complete = |success: bool, msg: &str, count: usize, bytes: u64, secs: f64| {
        if let Some(ref cb) = exec_script {
            let escaped = msg.replace('\\', "\\\\").replace('"', "\\\"");
            cb(format!(
                "if(window.onRemoteTransferComplete) window.onRemoteTransferComplete({{ success: {}, message: \"{}\", count: {}, bytes: {}, elapsed_secs: {:.1} }});",
                success, escaped, count, bytes, secs
            ));
        }
    };

    let emit_error = |err: &str| {
        if let Some(ref cb) = exec_script {
            let escaped = err.replace('\\', "\\\\").replace('"', "\\\"");
            cb(format!(
                "if(window.onRemoteTransferError) window.onRemoteTransferError({{ error: \"{}\" }});",
                escaped
            ));
        }
    };

    emit_log(&format!("Starting remote transfer to {}@{}:{} ({})", task.target_user, task.target_host, task.target_port, task.target_protocol.to_uppercase()), "info");

    // 1. Direct Server-to-Server Attempt (if source is remote SSH, target is SSH, and mode is direct or auto)
    if task.source_is_remote && (task.target_protocol == "scp" || task.target_protocol == "sftp" || task.target_protocol == "ssh") {
        if let Some(ref src) = source_client {
            if task.transfer_mode == "direct_scp" || task.transfer_mode == "direct_rsync" {
                emit_log("Attempting direct Server-to-Server transfer via native remote command...", "info");
                let sources_joined = task.sources.iter().map(|s| format!("'{}'", s.replace('\'', "'\\''"))).collect::<Vec<_>>().join(" ");
                let target_dest = format!("{}@{}:'{}'", task.target_user, task.target_host, task.target_path.replace('\'', "'\\''"));

                let direct_cmd = if task.transfer_mode == "direct_rsync" {
                    format!("rsync -avz -e 'ssh -p {} -o StrictHostKeyChecking=no' {} {}", task.target_port, sources_joined, target_dest)
                } else if !task.target_pass.is_empty() {
                    let esc_pass = task.target_pass.replace('\'', "'\\''");
                    format!("sshpass -p '{}' scp -P {} -r -o StrictHostKeyChecking=no {} {}", esc_pass, task.target_port, sources_joined, target_dest)
                } else {
                    format!("scp -P {} -r -o StrictHostKeyChecking=no {} {}", task.target_port, sources_joined, target_dest)
                };

                emit_log(&format!("Executing: {}", direct_cmd), "info");
                match src.exec_command_clean(&direct_cmd) {
                    Ok((0, out)) => {
                        emit_log("Direct Server-to-Server command finished successfully!", "success");
                        if !out.trim().is_empty() {
                            emit_log(&format!("Output:\n{}", out.trim()), "info");
                        }
                        emit_complete(true, "Direct Server-to-Server transfer completed successfully", task.sources.len(), 0, 0.0);
                        return;
                    }
                    Ok((code, out)) => {
                        let err_msg = format!("Direct command exited with code {}: {}", code, out.trim());
                        emit_log(&err_msg, "warning");
                        if task.transfer_mode != "auto" {
                            emit_error(&err_msg);
                            return;
                        }
                        emit_log("Direct Server-to-Server execution failed or required interactive terminal. Falling back to Universal Stream Relay Bridge...", "info");
                    }
                    Err(e) => {
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

    // 2. Universal Stream Relay Bridge
    emit_log("Connecting to target server for Stream Relay transfer...", "info");

    let mut target_client = match RemoteClient::connect(
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
        Ok(c) => c,
        Err(e) => {
            emit_error(&format!("Failed to connect to destination server: {}", e));
            return;
        }
    };

    emit_log("Connected to destination server! Ensuring target directory exists...", "info");
    let dest_clean = task.target_path.trim().replace('\\', "/");
    let dest_base = if dest_clean.is_empty() { ".".to_string() } else { dest_clean };
    if let Err(e) = target_client.make_dir_all(&dest_base) {
        emit_log(&format!("Notice: make_dir_all on {}: {}", dest_base, e), "info");
    }

    emit_log("Scanning and enumerating source files and folders...", "info");

    let items = if task.source_is_remote {
        if let Some(ref mut src) = source_client {
            match collect_remote_items(src, &task.sources, &cancelled) {
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

    for item in &items {
        if cancelled.load(Ordering::Relaxed) {
            emit_log("Transfer cancelled by user.", "warning");
            emit_complete(false, "Transfer cancelled by user", done_files, done_bytes, start_time.elapsed().as_secs_f64());
            return;
        }

        let target_item_path = if dest_base == "." || dest_base.is_empty() {
            item.relative_path.clone()
        } else {
            format!("{}/{}", dest_base.trim_end_matches('/'), item.relative_path)
        };

        if item.is_dir {
            if let Err(e) = target_client.make_dir_all(&target_item_path) {
                emit_log(&format!("Directory create note ({}): {}", target_item_path, e), "info");
            }
        } else {
            let elapsed = start_time.elapsed().as_secs_f64();
            let speed_mb = if elapsed > 0.05 {
                (done_bytes as f64 / (1024.0 * 1024.0)) / elapsed
            } else {
                0.0
            };
            let speed_str = format!("{:.2} MB/s", speed_mb);
            emit_progress(&item.relative_path, done_files, total_files, done_bytes, total_bytes, &speed_str);

            let transfer_res = if task.source_is_remote {
                if let Some(ref mut src) = source_client {
                    match src.download_file(&item.source_full_path) {
                        Ok(data) => target_client.upload_file(&target_item_path, &data),
                        Err(e) => Err(format!("Download failed for {}: {}", item.source_full_path, e)),
                    }
                } else {
                    Err("Source client disconnected".to_string())
                }
            } else {
                target_client.upload_from_local_file(&item.source_full_path, &target_item_path).map(|_| ())
            };

            match transfer_res {
                Ok(_) => {
                    done_files += 1;
                    done_bytes += item.size;
                    emit_log(&format!("✓ Transferred: {} ({})", item.relative_path, format_size(item.size)), "success");
                }
                Err(e) => {
                    emit_log(&format!("✕ Failed to transfer {}: {}", item.relative_path, e), "error");
                }
            }
        }
    }

    let elapsed_total = start_time.elapsed().as_secs_f64();
    emit_log(
        &format!(
            "Transfer session completed! Transferred {}/{} files ({} in {:.1}s)",
            done_files,
            total_files,
            format_size(done_bytes),
            elapsed_total
        ),
        "success",
    );
    emit_complete(true, "All selected items transferred successfully", done_files, done_bytes, elapsed_total);
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
}
