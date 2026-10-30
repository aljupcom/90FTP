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
fn default_compression() -> String {
    "none".to_string()
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
    /// Direct engine only: "none" | "gzip" | "zstd".
    ///
    /// Compression cannot be applied to a stream and still be measured: the
    /// byte counter would report compressed bytes against a raw total. So
    /// anything other than "none" switches the engine to a temporary archive,
    /// whose exact size is known before it is shipped.
    #[serde(default = "default_compression")]
    pub compression: String,
    /// Stream Relay only: parallel connections. 0 lets the engine decide.
    #[serde(default)]
    pub workers: usize,
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

/// Kept for the regression tests that pin the SSH option set; the live path
/// builds its command through [`crate::direct_engine::build`].
#[allow(dead_code)]
pub fn build_direct_transfer_command(task: &RemoteTransferTask) -> (String, String) {
    let plan = crate::direct_engine::build(task, "/");
    (plan.command, plan.display)
}

/// Result of trying to index a remote tree in one shot.
enum TreeScan {
    Indexed(Vec<TransferItem>),
    /// The server's `find` has no `-printf` (BSD/macOS, busybox).
    Unsupported,
    Failed(String),
}

/// Indexes a remote source tree with a **single** remote `find`.
///
/// The SFTP walk in [`collect_remote_items_with_progress`] needs one round trip
/// per directory. On a real website that is tens of thousands of entries and
/// several minutes of the progress bar sitting at 0%. `find -printf` returns
/// type, size and path for the whole tree in one command, which turns that wait
/// into roughly one second. Anything unexpected falls back to the walk.
fn remote_tree_via_find(
    client: &mut RemoteClient,
    sources: &[String],
    cancelled: &Arc<AtomicBool>,
) -> TreeScan {
    let mut dirs: Vec<(String, String)> = Vec::new();
    let mut items: Vec<TransferItem> = Vec::new();

    for src in sources {
        if cancelled.load(Ordering::Relaxed) {
            return TreeScan::Failed("cancelled".to_string());
        }
        let clean = src.trim().trim_end_matches('/');
        let clean = if clean.is_empty() { "/" } else { clean };
        let meta = match client.stat(clean) {
            Ok(Some(m)) => m,
            Ok(None) => {
                return TreeScan::Failed(format!("'{}' no longer exists on the source server", clean))
            }
            Err(e) => return TreeScan::Failed(e),
        };
        let base = clean
            .rsplit('/')
            .next()
            .filter(|s| !s.is_empty())
            .unwrap_or("root")
            .to_string();
        if meta.is_dir {
            dirs.push((clean.to_string(), base));
        } else {
            items.push(TransferItem {
                relative_path: base,
                is_dir: false,
                size: meta.size,
                source_full_path: clean.to_string(),
            });
        }
    }

    if dirs.is_empty() {
        return TreeScan::Indexed(items);
    }

    let quoted = dirs
        .iter()
        .map(|(p, _)| format!("'{}'", p.replace('\'', "'\\''")))
        .collect::<Vec<_>>()
        .join(" ");
    let cmd = format!(
        "LC_ALL=C find {} -mindepth 0 -printf '%y\\t%s\\t%p\\n' 2>/dev/null",
        quoted
    );

    let (code, out) = match client.exec_command_clean(&cmd) {
        Ok(v) => v,
        Err(e) => return TreeScan::Failed(e),
    };
    if code != 0 {
        return TreeScan::Unsupported;
    }
    if out.trim().is_empty() {
        return TreeScan::Unsupported;
    }

    for line in out.lines() {
        if cancelled.load(Ordering::Relaxed) {
            return TreeScan::Failed("cancelled".to_string());
        }
        let mut parts = line.splitn(3, '\t');
        let (ty, size_s, path) = match (parts.next(), parts.next(), parts.next()) {
            (Some(a), Some(b), Some(c)) => (a, b, c),
            _ => continue,
        };
        if let Some(item) = parse_find_line(ty, size_s, path, &dirs) {
            items.push(item);
        }
    }

    sort_for_creation(&mut items);

    TreeScan::Indexed(items)
}

/// Turns one `%y\t%s\t%p` line of `find -printf` output into a transfer item,
/// relative to whichever source directory it belongs to.
fn parse_find_line(
    ty: &str,
    size_s: &str,
    path: &str,
    dirs: &[(String, String)],
) -> Option<TransferItem> {
    let is_dir = match ty {
        "d" => true,
        // Symlinks are followed by the transfer itself, exactly as the SFTP walk
        // did, so they are carried as zero-byte file entries.
        "f" | "l" => false,
        _ => return None,
    };
    let size: u64 = if is_dir { 0 } else { size_s.parse().unwrap_or(0) };

    for (clean, base) in dirs {
        let rest = match path.strip_prefix(clean.as_str()) {
            Some(r) if r.is_empty() || r.starts_with('/') => r,
            _ => continue,
        };
        let rel = rest.trim_start_matches('/');
        let relative_path = if rel.is_empty() {
            base.clone()
        } else {
            format!("{}/{}", base, rel)
        };
        return Some(TransferItem {
            relative_path,
            is_dir,
            size,
            source_full_path: path.to_string(),
        });
    }
    None
}

/// Orders items so a directory always precedes everything inside it.
///
/// Sorting a directory as if it ended in `/` makes it a strict prefix of its
/// children, so the transfer never has to create the same directory twice and
/// never meets a file before its parent.
fn sort_for_creation(items: &mut [TransferItem]) {
    items.sort_by_cached_key(|it| {
        if it.is_dir {
            format!("{}/", it.relative_path)
        } else {
            it.relative_path.clone()
        }
    });
}

/// Ships an archive that the direct engine already built on the source server.
///
/// Used when packing succeeded and only the shipping failed. The archive is
/// complete, so walking 34,000 individual files through SFTP — one round trip
/// each — would move exactly the same bytes for a fraction of the throughput.
/// Instead the archive is downloaded once, uploaded once, and unpacked by the
/// destination's own shell, so nothing is unpacked on this computer.
///
/// Cleanup is strict: the local copy, the remote copy and the source archive are
/// removed on success and on failure. The user's own files are never touched —
/// the only thing deleted is the temporary archive the engine itself created.
#[allow(clippy::too_many_arguments)]
fn ship_staged_archive(
    source: &mut RemoteClient,
    archive_path: &str,
    archive_size: u64,
    target: &mut RemoteClient,
    dest_base: &str,
    cancelled: &Arc<AtomicBool>,
    emit_log: &dyn Fn(&str, &str),
    emit_progress: &(dyn Fn(&str, usize, usize, u64, u64, &str, Option<f64>) + Sync),
) -> Result<(), String> {
    use std::sync::atomic::AtomicU64;
    use std::time::Duration;

    let file_name = archive_path
        .rsplit('/')
        .next()
        .filter(|n| !n.is_empty())
        .unwrap_or("90ftp-archive.tar")
        .to_string();
    let remote_tmp = format!("/tmp/{}", file_name);
    let local_path = std::env::temp_dir().join(&file_name).to_string_lossy().to_string();
    let total = archive_size.max(1);
    let started = Instant::now();
    let done_total = AtomicU64::new(0);

    emit_log(
        &format!(
            "Reusing the archive the source already built ({}): one download, one upload, then it is unpacked on the destination. Nothing is re-packed and the 34k per-file round trips are skipped.",
            format_size(archive_size)
        ),
        "info",
    );

    let outcome = std::thread::scope(|scope| -> Result<(), String> {

        // --- leg 1: download the archive from the source (0 - 50%) ---
        let control = crate::transfer_control::TransferControl::new(total);
        let handle = control.handle();
        let stop = Arc::new(AtomicBool::new(false));
        {
            let stop = stop.clone();
            let cancelled = cancelled.clone();
            let emit = emit_progress;
            let label = format!("Downloading archive {}", file_name);
            let done_total = &done_total;
            scope.spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    if cancelled.load(Ordering::Relaxed) {
                        handle.cancel();
                    }
                    let done = handle.bytes_transferred();
                    done_total.store(done, Ordering::Relaxed);
                    let ratio = (done as f64 / total as f64).min(1.0);
                    let secs = started.elapsed().as_secs_f64().max(0.001);
                    let speed = format!("{}/s", format_size((done as f64 / secs) as u64));
                    emit(&label, 0, 1, done, total, &speed, Some(50.0 * ratio));
                    std::thread::sleep(Duration::from_millis(250));
                }
            });
        }
        let download = source.download_to_local_file_ctl(archive_path, &local_path, &control);
        stop.store(true, Ordering::Relaxed);
        if cancelled.load(Ordering::Relaxed) {
            let _ = std::fs::remove_file(&local_path);
            return Err("Transfer cancelled".to_string());
        }
        if let Err(e) = download {
            let _ = std::fs::remove_file(&local_path);
            return Err(format!("could not download the archive from the source: {}", e));
        }

        // --- leg 2: upload it to the destination (50 - 95%) ---
        let upload_total = std::fs::metadata(&local_path).map(|m| m.len()).unwrap_or(total);
        let control = crate::transfer_control::TransferControl::new(upload_total);
        let handle = control.handle();
        let stop = Arc::new(AtomicBool::new(false));
        {
            let stop = stop.clone();
            let cancelled = cancelled.clone();
            let emit = emit_progress;
            let label = format!("Uploading archive {}", file_name);
            let done_total = &done_total;
            scope.spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    if cancelled.load(Ordering::Relaxed) {
                        handle.cancel();
                    }
                    let done = handle.bytes_transferred();
                    done_total.store(done, Ordering::Relaxed);
                    let ratio = (done as f64 / upload_total.max(1) as f64).min(1.0);
                    let secs = started.elapsed().as_secs_f64().max(0.001);
                    let speed = format!("{}/s", format_size((done as f64 / secs) as u64));
                    emit(&label, 0, 1, done, upload_total, &speed, Some(50.0 + 45.0 * ratio));
                    std::thread::sleep(Duration::from_millis(250));
                }
            });
        }
        let upload = target.upload_from_local_file_ctl(&local_path, &remote_tmp, &control);
        stop.store(true, Ordering::Relaxed);
        if cancelled.load(Ordering::Relaxed) {
            let _ = std::fs::remove_file(&local_path);
            let _ = target.exec_command_clean(&format!("rm -f {}", crate::direct_engine::shell_quote(&remote_tmp)));
            return Err("Transfer cancelled".to_string());
        }
        if let Err(e) = upload {
            let _ = std::fs::remove_file(&local_path);
            let _ = target.exec_command_clean(&format!("rm -f {}", crate::direct_engine::shell_quote(&remote_tmp)));
            return Err(format!("could not upload the archive to the destination: {}", e));
        }

        // --- leg 3: unpack it where it belongs (95 - 100%) ---
        emit_progress(
            &format!("Unpacking {} on the destination", file_name),
            0,
            1,
            upload_total,
            upload_total,
            "Unpacking…",
            Some(96.0),
        );
        let cmd = format!(
            "mkdir -p {dest} && tar {flag} {arch} -C {dest}; __r=$?; rm -f {arch}; exit $__r",
            dest = crate::direct_engine::shell_quote(dest_base),
            flag = crate::direct_engine::extract_flag_for(archive_path),
            arch = crate::direct_engine::shell_quote(&remote_tmp),
        );
        let extract = target.exec_command_clean(&cmd);
        let _ = std::fs::remove_file(&local_path);

        match extract {
            Ok((0, _)) => {}
            Ok((code, out)) => {
                return Err(format!(
                    "the destination could not unpack the archive (exit {}): {}",
                    code,
                    out.trim().lines().last().unwrap_or("").trim()
                ));
            }
            Err(e) => return Err(format!("could not run the unpack command on the destination: {}", e)),
        }

        // The source archive has served its purpose. Only that file is removed.
        let _ = source.exec_command_clean(&format!(
            "rm -f {}",
            crate::direct_engine::shell_quote(archive_path)
        ));
        emit_log(
            "The temporary archive was removed from both servers; the transferred files and your originals are untouched.",
            "info",
        );
        Ok(())
    });

    outcome
}

/// Packs the selected tree into a temporary archive **on the source server**.
///
/// This is the answer to a blocked route: when the source cannot open an SSH
/// connection to the destination, the direct engine is impossible, but the
/// files still have to get there. Compressing them where they already are turns
/// 34,000 small files into one, which is the only way that transfer finishes in
/// minutes instead of hours — and the archive is left on disk (`KEEP`) for the
/// caller to ship.
#[allow(clippy::too_many_arguments)]
fn pack_on_source(
    src: &RemoteClient,
    task: &RemoteTransferTask,
    compression: &str,
    total_bytes: u64,
    cancelled: &Arc<AtomicBool>,
    emit_log: &(dyn Fn(&str, &str) + Sync),
    emit_progress: &(dyn Fn(&str, usize, usize, u64, u64, &str, Option<f64>) + Sync),
) -> Option<(String, u64)> {
    let parsed = crate::direct_engine::Compression::parse(compression);
    let plan = crate::direct_engine::build_pack_only(task, &src.pwd(), parsed)?;

    emit_log(&format!("Executing: {}", plan.display), "info");
    emit_log(
        &format!(
            "Packing on the source server with {} — the archive is temporary and is removed once the files are in place.",
            parsed.label()
        ),
        "info",
    );

    let started = Instant::now();
    let mut last_tick = Instant::now();
    let mut markers = crate::direct_engine::MarkerBuffer::default();
    let mut packed = 0u64;
    let mut label = format!("Packing ({})…", compression);

    let exec = src.exec_command_stream_pty(&plan.command, Some(cancelled.clone()), |chunk: &str| {
        for marker in markers.feed(chunk) {
            let mut parts = marker.splitn(2, ' ');
            let head = parts.next().unwrap_or("");
            let tail = parts.next().unwrap_or("").trim();
            match head {
                "PHASE" => label = format!("Packing ({})…", compression),
                "PACK" => {
                    if let Ok(bytes) = tail.parse::<u64>() {
                        packed = bytes;
                    }
                }
                "ERROR" => emit_log(&format!("Packing failed: {}", tail), "error"),
                _ => {}
            }
        }
        if last_tick.elapsed().as_millis() < 300 {
            return;
        }
        last_tick = Instant::now();
        let ratio = if total_bytes == 0 {
            0.0
        } else {
            (packed as f64 / total_bytes as f64).clamp(0.0, 1.0)
        };
        let secs = started.elapsed().as_secs_f64().max(0.001);
        let speed = format!("{}/s", format_size((packed as f64 / secs) as u64));
        emit_progress(&label, 0, 1, packed, total_bytes.max(packed), &speed, Some(ratio * 100.0));
    });

    match exec {
        Ok((code, out)) => {
            if cancelled.load(Ordering::Relaxed) {
                return None;
            }
            let kept = crate::direct_engine::kept_archive(&out);
            if code != 0 || kept.is_none() {
                emit_log(
                    &format!(
                        "Packing on the source server did not finish (exit {}): {}",
                        code,
                        out.trim().lines().last().unwrap_or("").trim()
                    ),
                    "warning",
                );
                return None;
            }
            kept
        }
        Err(e) => {
            emit_log(&format!("Could not pack on the source server: {}", e), "warning");
            None
        }
    }
}

pub fn execute_transfer_thread(
    mut source_client: Option<RemoteClient>,
    /* The parameters behind `source_client`, so the relay can open extra
       sessions for its worker pool. A connection is stateful and cannot be
       shared across threads. */
    source_spec: Option<crate::relay_engine::ConnectionSpec>,
    task: RemoteTransferTask,
    cancelled: Arc<AtomicBool>,
    exec_script: Option<ScriptCallback>,
) {
    /* The five emitters live in `relay_engine` so the direct-SCP path and the
       relay's worker threads build byte-identical messages. Every one of them
       redacts before the text leaves Rust: the transfer log is visible on
       screen and is the one place a leaked credential would be copied straight
       into a screenshot or a support ticket. */
    let emit_log = |msg: &str, kind: &str| {
        crate::relay_engine::send_log(&exec_script, msg, kind);
    };

    // Emits a line that *replaces* the previous status line instead of
    // appending. Used for anything that updates in place (indexing progress),
    // so the log does not fill with near-identical lines.
    let emit_status = |msg: &str, kind: &str| {
        crate::relay_engine::send_status(&exec_script, msg, kind);
    };

    let emit_progress = |file: &str, done_files: usize, total_files: usize, done_bytes: u64, total_bytes: u64, speed_str: &str, override_pct: Option<f64>| {
        let pct = if let Some(p) = override_pct {
            p.max(0.0).min(100.0)
        } else if total_bytes > 0 {
            (done_bytes as f64 / total_bytes as f64 * 100.0).min(100.0)
        } else if total_files > 0 {
            (done_files as f64 / total_files as f64 * 100.0).min(100.0)
        } else {
            100.0
        };
        crate::relay_engine::send_progress(
            &exec_script, pct, file, done_files, total_files, done_bytes, total_bytes, speed_str,
        );
    };

    let emit_complete = |success: bool, msg: &str, count: usize, bytes: u64, secs: f64| {
        crate::relay_engine::send_complete(&exec_script, success, msg, count, bytes, secs);
    };

    let emit_error = |err: &str| {
        crate::relay_engine::send_error(&exec_script, err);
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

    /* Set when the direct engine packed the tree successfully but could not
       ship the archive. The archive is still on the source server, complete, and
       re-sending 30,000 files one by one instead of that single file is the
       worst possible answer to a failed connection. */
    let mut kept_archive: Option<(String, u64)> = None;

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

            /* The probe is worth running even when a password is available: it
               is the only cheap way to learn whether the source server can
               reach the destination *at all*. Skipping it meant a blocked
               outbound port was discovered only after the whole tree had been
               packed — 78 seconds of work thrown away on a connection that
               could never open. With `BatchMode=yes` a reachable server answers
               "Permission denied", which is a fine outcome here: it proves the
               route exists. */
            let mut server_side_key = false;
            let mut reachable = true;
            if wants_direct {
                emit_log("Testing whether the source server can reach the destination directly…", "info");
                let probe = format!(
                    "ssh -o BatchMode=yes -o ConnectTimeout=6 -o StrictHostKeyChecking=accept-new -p {} {}@{} true",
                    task.target_port, task.target_user, task.target_host
                );
                match src.exec_command_clean(&probe) {
                    Ok((0, _)) => {
                        server_side_key = true;
                        emit_log(
                            "The source server already trusts the destination, so the files never pass through this computer.",
                            "success",
                        );
                    }
                    Ok((_, out)) => {
                        let lowered = out.to_lowercase();
                        let blocked = lowered.contains("connection timed out")
                            || lowered.contains("connection refused")
                            || lowered.contains("no route to host")
                            || lowered.contains("network is unreachable")
                            || lowered.contains("could not resolve")
                            || lowered.contains("temporary failure in name resolution");
                        if blocked {
                            reachable = false;
                            emit_log(
                                &format!(
                                    "The source server cannot open an SSH connection to {}:{} ({}). Direct transfer is impossible from there — continuing with the Stream Relay Bridge.",
                                    task.target_host,
                                    task.target_port,
                                    out.trim().lines().last().unwrap_or("no route").trim()
                                ),
                                "warning",
                            );
                        }
                    }
                    Err(e) => {
                        reachable = false;
                        emit_log(
                            &format!("Could not probe the route from the source server ({}). Using the Stream Relay Bridge.", e),
                            "warning",
                        );
                    }
                }
            }

            if wants_direct && !password_auth && !server_side_key && reachable {
                emit_log(
                    "Direct transfer cannot use the SSH key saved for this site: the key is stored on this computer, but the copy command runs on the source server. Continuing with the Stream Relay Bridge, which uses these credentials directly. Add a password to the site to enable direct mode.",
                    "warning",
                );
            }

            /* No route between the servers, but compression was requested and
               the destination can unpack: build the archive where the files
               already are and carry that single file through this computer.
               Without this, a blocked route meant tens of thousands of tiny
               round trips — the exact case where the transfer looks hung. */
            if wants_direct && !reachable {
                let requested = crate::direct_engine::Compression::parse(&task.compression);
                let target_has_shell = matches!(pre_target_client, Some(RemoteClient::Sftp(_)));
                if requested != crate::direct_engine::Compression::None && target_has_shell {
                    let mut chosen = task.compression.clone();
                    if requested == crate::direct_engine::Compression::Zstd {
                        let has_zstd = matches!(
                            src.exec_command_clean("command -v zstd >/dev/null 2>&1 && echo __ZSTD_OK__"),
                            Ok((0, out)) if out.contains("__ZSTD_OK__")
                        );
                        if !has_zstd {
                            emit_log(
                                "zstd is not installed on the source server; packing with gzip instead.",
                                "warning",
                            );
                            chosen = "gzip".to_string();
                        }
                    }

                    let sources_joined = task
                        .sources
                        .iter()
                        .map(|s| format!("'{}'", s.replace('\'', "'\\''")))
                        .collect::<Vec<_>>()
                        .join(" ");
                    let mut total_bytes = 0u64;
                    if let Ok((0, out)) = src.exec_command_clean(&format!(
                        "stat -c %s {} 2>/dev/null || du -sb {} 2>/dev/null | cut -f1",
                        sources_joined, sources_joined
                    )) {
                        total_bytes = out
                            .lines()
                            .next()
                            .and_then(|l| l.trim().parse::<u64>().ok())
                            .unwrap_or(0);
                    }

                    kept_archive = pack_on_source(
                        src,
                        &task,
                        &chosen,
                        total_bytes,
                        &cancelled,
                        &emit_log,
                        &emit_progress,
                    );
                } else if requested != crate::direct_engine::Compression::None {
                    emit_log(
                        "Compression needs a destination that can unpack the archive; this protocol has no shell, so the files are transferred individually.",
                        "info",
                    );
                }
            }

            if wants_direct && reachable && (password_auth || server_side_key) {
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

                    /* `dd status=progress` is the only portable byte counter on
                       a stock Linux box — coreutils 8.24 (2015) and newer. An
                       older `dd` rejects the flag outright, which would kill the
                       whole pipeline, so it is probed once here and the transfer
                       drops to a plain `cat` (no live bytes) if it is missing. */
                    let dd_ok = matches!(
                        src.exec_command_clean("dd if=/dev/null bs=1 count=0 status=progress 2>/dev/null && echo __DD_OK__"),
                        Ok((0, out)) if out.contains("__DD_OK__")
                    );

                    /* zstd needs the binary on *both* ends; gzip rides on tar's
                       own -z and is always there. */
                    let mut compression = task.compression.clone();
                    if crate::direct_engine::Compression::parse(&compression) == crate::direct_engine::Compression::Zstd {
                        let has_zstd = matches!(
                            src.exec_command_clean("command -v zstd >/dev/null 2>&1 && echo __ZSTD_OK__"),
                            Ok((0, out)) if out.contains("__ZSTD_OK__")
                        );
                        if !has_zstd {
                            emit_log(
                                "zstd is not installed on the source server; using gzip for this transfer instead.",
                                "warning",
                            );
                            compression = "gzip".to_string();
                        }
                    }

                    /* A compressed transfer stages the whole tree in /tmp on the
                       source. On a box with a small tmpfs that fails halfway
                       through with "No space left on device", which reads as a
                       broken transfer rather than a full disk. Check first and
                       fall back to the streaming engine instead. */
                    if crate::direct_engine::Compression::parse(&compression) != crate::direct_engine::Compression::None
                        && total_bytes > 0
                    {
                        if let Ok((0, out)) = src.exec_command_clean("df -Pk /tmp 2>/dev/null | awk 'NR==2 {print $4}'") {
                            if let Some(free_kb) = out.trim().lines().next().and_then(|l| l.trim().parse::<u64>().ok()) {
                                if free_kb.saturating_mul(1024) < total_bytes {
                                    emit_log(
                                        &format!(
                                            "Not enough free space in the source server's /tmp for a temporary archive ({} needed, {} free). Streaming without compression instead.",
                                            format_size(total_bytes),
                                            format_size(free_kb.saturating_mul(1024))
                                        ),
                                        "warning",
                                    );
                                    compression = "none".to_string();
                                }
                            }
                        }
                    }

                    let mut plan_task = task.clone();
                    plan_task.compression = compression.clone();                    let source_cwd = src.pwd();
                    let plan = crate::direct_engine::build(&plan_task, &source_cwd);

                    let mut direct_cmd = plan.command;
                    if !dd_ok {
                        /* Without dd's status line there is no byte counter, but
                           the copy itself still has to work: keep the pipeline
                           and drop only the unsupported flag, then say so
                           instead of leaving a bar stuck at 0%. */
                        direct_cmd = direct_cmd.replace(" status=progress", "");
                        emit_log(
                            "This server's dd has no status=progress support, so live byte counts are unavailable; the bar will update when the copy finishes.",
                            "warning",
                        );
                    }

                    let display_cmd = plan.display.clone();
                    let staging = plan.staging;
                    let plan_compression = plan.compression;
                    let plan_engine = plan.engine;

                    emit_progress(&first_file, 0, total_files, 0, total_bytes, "Connecting to target...", Some(0.0));
                    emit_log(&format!("Executing: {}", display_cmd), "info");
                    emit_log(
                        &format!(
                            "Direct engine: {} — {}.",
                            plan_engine,
                            if staging {
                                "temporary archive on the source"
                            } else {
                                "single stream, no temporary files"
                            }
                        ),
                        "info",
                    );
                    if staging {
                        emit_log(
                            &format!(
                                "Compression is on ({}): the source is packed into a temporary archive, shipped, unpacked at the destination, and then only that temporary archive is deleted. Your files are never modified or removed.",
                                plan_compression.label()
                            ),
                            "info",
                        );
                    } else {
                        emit_log(
                            "Streaming the tree over SSH: the bytes travel from the source server straight to the destination and never pass through this computer.",
                            "info",
                        );
                    }

                    let direct_start_time = Instant::now();
                    let mut last_progress_tick = Instant::now();
                    let mut cur_file = first_file.clone();
                    let mut cur_speed = "Transferring...".to_string();
                    let mut cur_done_bytes = 0u64;
                    let mut phase = crate::direct_engine::Phase::Ship;
                    let mut archive_bytes = 0u64;
                    let mut markers = crate::direct_engine::MarkerBuffer::default();

                    /* The bar spans every phase of a staged transfer, so it
                       never restarts: packing is the first quarter, shipping the
                       bulk, extraction the last slice. A stream transfer is a
                       single phase and maps straight onto 0-100%. */
                    let composite = |phase: crate::direct_engine::Phase, done: u64, total: u64| -> f64 {
                        let ratio = if total == 0 {
                            0.0
                        } else {
                            (done as f64 / total as f64).clamp(0.0, 1.0)
                        };
                        if !staging {
                            return ratio * 100.0;
                        }
                        match phase {
                            crate::direct_engine::Phase::Pack => ratio * 25.0,
                            crate::direct_engine::Phase::Ship => 25.0 + ratio * 70.0,
                            crate::direct_engine::Phase::Extract => 97.0,
                            crate::direct_engine::Phase::Cleanup => 99.0,
                        }
                    };

                    let exec_res = src.exec_command_stream_pty(
                        &direct_cmd,
                        Some(cancelled.clone()),
                        |chunk: &str| {
                            for marker in markers.feed(chunk) {
                                let mut parts = marker.splitn(2, ' ');
                                let head = parts.next().unwrap_or("");
                                let tail = parts.next().unwrap_or("").trim();
                                match head {
                                    "PHASE" => {
                                        let mut bits = tail.splitn(2, ' ');
                                        if let Some(p) = bits.next().and_then(crate::direct_engine::Phase::from_token) {
                                            phase = p;
                                        }
                                        if let Some(size) = bits.next().and_then(|s| s.trim().parse::<u64>().ok()) {
                                            archive_bytes = size;
                                        }
                                        cur_file = match phase {
                                            crate::direct_engine::Phase::Pack => format!("[1/3] Packing ({})…", compression),
                                            crate::direct_engine::Phase::Ship if staging => "[2/3] Sending the archive…".to_string(),
                                            crate::direct_engine::Phase::Ship => "Streaming the files over SSH…".to_string(),
                                            crate::direct_engine::Phase::Extract => "[3/3] Unpacking at the destination…".to_string(),
                                            crate::direct_engine::Phase::Cleanup => "Removing the temporary archive…".to_string(),
                                        };
                                    }
                                    "PACK" => {
                                        if let Ok(bytes) = tail.parse::<u64>() {
                                            cur_done_bytes = bytes;
                                        }
                                    }
                                    "ERROR" => {
                                        emit_log(&format!("Transfer step failed: {}", tail), "error");
                                    }
                                    _ => {}
                                }
                            }

                            /* Two possible progress sources: `dd` for the tar
                               engine, `--info=progress2` for rsync. Only one of
                               them is ever active, so try both and take
                               whichever produced a number. */
                            if let Some(bytes) = crate::direct_engine::parse_dd_bytes(chunk) {
                                cur_done_bytes = bytes;
                            } else if let Some((pct, spd, file, bytes)) = parse_scp_progress_line(chunk) {
                                if let Some(s) = spd {
                                    cur_speed = s;
                                }
                                if let Some(f) = file {
                                    cur_file = f;
                                }
                                if let Some(b) = bytes {
                                    cur_done_bytes = b;
                                } else if total_bytes > 0 {
                                    cur_done_bytes = (total_bytes as f64 * (pct / 100.0)) as u64;
                                }
                            }

                            if last_progress_tick.elapsed().as_millis() < 200 {
                                return;
                            }
                            last_progress_tick = Instant::now();

                            let phase_total = match phase {
                                crate::direct_engine::Phase::Pack => total_bytes,
                                crate::direct_engine::Phase::Ship if staging => archive_bytes.max(cur_done_bytes),
                                _ => total_bytes,
                            };
                            let pct = composite(phase, cur_done_bytes, phase_total);

                            let elapsed = direct_start_time.elapsed().as_secs_f64().max(0.001);
                            let speed = format!("{}/s", format_size((cur_done_bytes as f64 / elapsed) as u64));
                            if cur_speed.is_empty() || cur_speed == "Transferring..." {
                                cur_speed = speed;
                            }

                            emit_progress(
                                &cur_file,
                                if pct >= 100.0 { total_files } else { 0 },
                                total_files,
                                cur_done_bytes,
                                phase_total.max(cur_done_bytes).max(total_bytes),
                                &cur_speed,
                                Some(pct),
                            );
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
                               anyway. Printing the raw error used to look like a
                               broken connection — but hiding it entirely left
                               nothing to diagnose, so it is always shown. */
                            let detail = out.trim();

                            /* The archive survived the failure. Remember where it
                               is before deciding what to do next. */
                            if let Some(found) = crate::direct_engine::kept_archive(&out) {
                                kept_archive = Some(found);
                            }

                            let detail_lower = detail.to_lowercase();
                            let auth_refused = detail_lower.contains("permission denied")
                                || detail_lower.contains("publickey")
                                || detail_lower.contains("authentication failed");
                            let unreachable = detail_lower.contains("connection timed out")
                                || detail_lower.contains("connection refused")
                                || detail_lower.contains("no route to host")
                                || detail_lower.contains("network is unreachable")
                                || detail_lower.contains("could not resolve")
                                || detail_lower.contains("host key verification failed");

                            let err_msg = if unreachable {
                                format!(
                                    "The source server could not open an SSH connection to the destination: {}",
                                    detail.lines().last().unwrap_or("no route").trim()
                                )
                            } else if auth_refused {
                                if task.target_pass.is_empty() {
                                    "Direct transfer was refused: the source server holds no SSH key authorized on the destination.".to_string()
                                } else {
                                    "Direct transfer was refused: the destination rejected the password when the source server presented it.".to_string()
                                }
                            } else if code == 92 {
                                format!("Direct shipping failed, but the archive is intact: {}", detail.lines().last().unwrap_or("").trim())
                            } else if code != 0 {
                                format!("Direct command exited with code {}: {}", code, detail.lines().last().unwrap_or("").trim())
                            } else {
                                format!("Direct command failed: {}", detail.lines().last().unwrap_or("").trim())
                            };

                            if !detail.is_empty() {
                                emit_log(&format!("Direct engine output:\n{}", detail), "info");
                            }
                            emit_log(&err_msg, if auth_refused || unreachable { "info" } else { "warning" });
                            // A refused credential is not a transfer failure: the
                            // relay below authenticates from this machine, which is
                            // where the saved credentials actually live. Only a real
                            // transfer error should stop an explicit direct run.
                            if task.transfer_mode != "auto" && !auth_refused && !unreachable && kept_archive.is_none() {
                                emit_error(&err_msg);
                                return;
                            }
                            emit_log(
                                "Continuing with the Stream Relay Bridge, which uses the credentials entered here — nothing else to configure.",
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

    /* 1b. The archive the direct engine already built.
       When packing succeeded and only the shipping failed, the expensive half of
       the work is done and the result is one file sitting in the source's /tmp.
       Walking 34,000 individual files through SFTP instead — one round trip
       each — is both slower and more fragile than moving that single archive.
       The extraction runs on the destination through its own SSH session, so
       nothing has to be unpacked on this computer. */
    if let Some((archive_path, archive_size)) = kept_archive.take() {
        let target_has_shell = matches!(pre_target_client, Some(RemoteClient::Sftp(_)));
        if !target_has_shell {
            emit_log(
                "The archive cannot be unpacked on the destination because this protocol has no shell; transferring the files individually instead.",
                "warning",
            );
        } else if let (Some(src), Some(target)) = (source_client.as_mut(), pre_target_client.as_mut()) {
            let archive_started = Instant::now();
            match ship_staged_archive(
                src,
                &archive_path,
                archive_size,
                target,
                &dest_base,
                &cancelled,
                &emit_log,
                &emit_progress,
            ) {
                Ok(()) => {
                    emit_progress(&archive_path, 1, 1, archive_size, archive_size, "Completed", Some(100.0));
                    emit_complete(
                        true,
                        "Transfer completed by shipping the compressed archive the source had already built",
                        1,
                        archive_size,
                        archive_started.elapsed().as_secs_f64(),
                    );
                    return;
                }
                Err(e) => {
                    if cancelled.load(Ordering::Relaxed) {
                        emit_log("Transfer cancelled by user.", "warning");
                        emit_complete(false, "Transfer cancelled by user", 0, 0, archive_started.elapsed().as_secs_f64());
                        return;
                    }
                    emit_log(
                        &format!("Shipping the archive through this computer failed too ({}). Falling back to per-file transfer.", e),
                        "warning",
                    );
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

    /* Enumerating a real website over SFTP costs one round trip *per directory*.
       A 23k-file tree therefore sits for minutes before the first byte moves —
       which reads as "the transfer is stuck". GNU `find -printf` returns the
       whole tree (type, size, path) in a single command; the SFTP walk stays as
       the fallback for servers that do not have it. */
    let scan_start = Instant::now();
    let last_scan_report = std::cell::Cell::new(Instant::now());
    let scan_progress = |found: usize| {
        if last_scan_report.get().elapsed().as_millis() >= 700 {
            last_scan_report.set(Instant::now());
            emit_status(
                &format!("Indexing source tree — {} item(s) so far…", found),
                "info",
            );
        }
    };

    let items = if task.source_is_remote {
        if let Some(ref mut src) = source_client {
            emit_status("Indexing source tree…", "info");
            let enumerated = match remote_tree_via_find(src, &task.sources, &cancelled) {
                TreeScan::Indexed(list) => {
                    emit_log(
                        &format!(
                            "Source indexed in {:.1}s in a single remote call ({} items).",
                            scan_start.elapsed().as_secs_f64(),
                            list.len()
                        ),
                        "success",
                    );
                    Ok(list)
                }
                TreeScan::Unsupported => {
                    emit_log(
                        "This server's find has no -printf support; using the SFTP directory walk instead.",
                        "info",
                    );
                    collect_remote_items_with_progress(src, &task.sources, &cancelled, &scan_progress)
                }
                TreeScan::Failed(e) => {
                    emit_log(
                        &format!("Fast indexing was not possible ({}); using the SFTP directory walk.", e),
                        "info",
                    );
                    collect_remote_items_with_progress(src, &task.sources, &cancelled, &scan_progress)
                }
            };
            match enumerated {
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

    /* The "Recursive directory transfer" checkbox was sent by the UI but never
       read, so unchecking it still copied the whole tree. A non-recursive
       transfer means the selected entries themselves: files as they are, and
       directories created empty. */
    let items: Vec<TransferItem> = if task.recursive {
        items
    } else {
        items
            .into_iter()
            .filter(|i| !i.relative_path.contains('/'))
            .collect()
    };

    if items.is_empty() {
        emit_error("Nothing to transfer: the selection produced no items.");
        return;
    }
    if !task.recursive {
        emit_log(
            "Recursive transfer is off: only the selected entries themselves are copied.",
            "info",
        );
    }

    let total_files = items.iter().filter(|i| !i.is_dir).count();
    let indexed_bytes: u64 = items.iter().filter(|i| !i.is_dir).map(|i| i.size).sum();

    emit_log(
        &format!(
            "Ready to transfer {} items ({} files, total size: {})",
            items.len(),
            total_files,
            format_size(indexed_bytes)
        ),
        "info",
    );

    /* The destination parameters travel with the job because extra workers need
       to open their own session: a connection is stateful and cannot be shared
       between threads. */
    let target_spec = crate::relay_engine::ConnectionSpec {
        protocol: task.target_protocol.clone(),
        host: task.target_host.clone(),
        port: task.target_port,
        user: task.target_user.clone(),
        pass: task.target_pass.clone(),
        auth_type: task.target_auth_type.clone(),
        key_path: task.target_key_path.clone(),
        passphrase: task.target_passphrase.clone(),
    };

    let plan = crate::relay_engine::Plan {
        items: &items,
        dest_base: &dest_base,
        source_is_remote: task.source_is_remote,
        overwrite_skip: task.overwrite == "skip",
        workers: task.workers,
    };

    let outcome = crate::relay_engine::run(
        source_client,
        source_spec.as_ref(),
        target_client,
        &target_spec,
        &plan,
        &exec_script,
        &cancelled,
        indexed_bytes,
    );

    let elapsed_total = outcome.elapsed_secs;

    if outcome.cancelled {
        emit_log("Transfer cancelled by user.", "warning");
        emit_complete(
            false,
            "Transfer cancelled by user",
            outcome.done_files,
            outcome.done_bytes,
            elapsed_total,
        );
        return;
    }

    if !outcome.failures.is_empty() {
        emit_log(
            &format!(
                "{} file(s) could not be transferred. First error: {}",
                outcome.failures.len(),
                outcome.failures[0]
            ),
            "error",
        );
    }

    let is_all_success = outcome.failed_files == 0;
    let completion_msg = if is_all_success {
        format!(
            "Transfer session completed successfully! Transferred {}/{} files ({} in {:.1}s)",
            outcome.done_files,
            total_files,
            format_size(outcome.done_bytes),
            elapsed_total
        )
    } else {
        format!(
            "Transfer finished with {} failures: {}/{} files succeeded ({} in {:.1}s)",
            outcome.failed_files,
            outcome.done_files,
            total_files,
            format_size(outcome.done_bytes),
            elapsed_total
        )
    };

    emit_log(&completion_msg, if is_all_success { "success" } else { "warning" });
    emit_complete(
        is_all_success,
        &completion_msg,
        outcome.done_files,
        outcome.done_bytes,
        elapsed_total,
    );
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
    fn find_output_maps_paths_relative_to_their_source() {
        let dirs = vec![("/home/site/public_html".to_string(), "public_html".to_string())];
        let item = parse_find_line("f", "512", "/home/site/public_html/index.php", &dirs).unwrap();
        assert_eq!(item.relative_path, "public_html/index.php");
        assert_eq!(item.size, 512);
        assert!(!item.is_dir);
        assert_eq!(item.source_full_path, "/home/site/public_html/index.php");

        // The root directory itself becomes the transfer root.
        let root = parse_find_line("d", "4096", "/home/site/public_html", &dirs).unwrap();
        assert_eq!(root.relative_path, "public_html");
        assert!(root.is_dir);

        // A path outside every source is ignored rather than mis-attributed.
        assert!(parse_find_line("f", "1", "/etc/passwd", &dirs).is_none());
        // A sibling whose name merely starts with the source name must not match.
        assert!(parse_find_line("f", "1", "/home/site/public_html_backup/x", &dirs).is_none());
        // Unknown types are skipped.
        assert!(parse_find_line("s", "0", "/home/site/public_html/sock", &dirs).is_none());
    }

    #[test]
    fn every_directory_precedes_its_children() {
        let mut items = vec![
            TransferItem { relative_path: "site/a.txt".into(), is_dir: false, size: 1, source_full_path: String::new() },
            TransferItem { relative_path: "site/a/b.txt".into(), is_dir: false, size: 1, source_full_path: String::new() },
            TransferItem { relative_path: "site/a".into(), is_dir: true, size: 0, source_full_path: String::new() },
            TransferItem { relative_path: "site".into(), is_dir: true, size: 0, source_full_path: String::new() },
            TransferItem { relative_path: "site/b/c/d.txt".into(), is_dir: false, size: 1, source_full_path: String::new() },
            TransferItem { relative_path: "site/b/c".into(), is_dir: true, size: 0, source_full_path: String::new() },
            TransferItem { relative_path: "site/b".into(), is_dir: true, size: 0, source_full_path: String::new() },
        ];
        sort_for_creation(&mut items);

        let dirs: std::collections::HashSet<&str> = items
            .iter()
            .filter(|i| i.is_dir)
            .map(|i| i.relative_path.as_str())
            .collect();

        // The invariant that matters: by the time an entry is reached, its
        // parent directory has already been created.
        for (idx, item) in items.iter().enumerate() {
            if let Some(slash) = item.relative_path.rfind('/') {
                let parent = &item.relative_path[..slash];
                let parent_idx = items
                    .iter()
                    .position(|i| i.is_dir && i.relative_path == parent)
                    .unwrap_or_else(|| panic!("parent '{}' missing for '{}'", parent, item.relative_path));
                assert!(
                    parent_idx < idx,
                    "parent '{}' must come before '{}'",
                    parent,
                    item.relative_path
                );
                assert!(dirs.contains(parent));
            }
        }
        assert_eq!(items[0].relative_path, "site");
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
            compression: "none".to_string(),
            workers: 0,
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
            compression: "none".to_string(),
            workers: 0,
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
