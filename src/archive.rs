use std::fs::File;
use std::io::Write;
use std::path::Path;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};
use crate::remote::RemoteClient;
use crate::app_state::ScriptCallback;

pub fn compress_to_zip(source_paths: &[String], dest_zip_path: &str) -> Result<usize, String> {
    if source_paths.is_empty() {
        return Err("No source files or folders selected to compress".to_string());
    }

    let dest_path = Path::new(dest_zip_path);
    if let Some(parent) = dest_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    let file = File::create(dest_path)
        .map_err(|e| format!("Failed to create destination zip file {:?}: {}", dest_path, e))?;
    let mut zip = ZipWriter::new(file);
    let options = SimpleFileOptions::default()
        .compression_method(CompressionMethod::Deflated)
        .unix_permissions(0o644);

    let mut count = 0;

    for src_str in source_paths {
        let src_path = Path::new(src_str);
        if !src_path.exists() {
            continue;
        }

        let base_name = match src_path.file_name() {
            Some(n) => n.to_string_lossy().to_string(),
            None => continue,
        };

        if src_path.is_file() {
            zip.start_file(&base_name, options)
                .map_err(|e| format!("Failed to add file {} to zip: {}", base_name, e))?;
            let mut f = File::open(src_path)
                .map_err(|e| format!("Failed to open file {:?}: {}", src_path, e))?;
            std::io::copy(&mut f, &mut zip)
                .map_err(|e| format!("Failed to write file {:?} into zip: {}", src_path, e))?;
            count += 1;
        } else if src_path.is_dir() {
            add_dir_recursive(&mut zip, src_path, &base_name, &mut count, options)?;
        }
    }

    zip.finish()
        .map_err(|e| format!("Failed to finalize zip archive: {}", e))?;

    Ok(count)
}

fn add_dir_recursive<W: Write + std::io::Seek>(
    zip: &mut ZipWriter<W>,
    current_dir: &Path,
    current_prefix: &str,
    count: &mut usize,
    options: SimpleFileOptions,
) -> Result<(), String> {
    let dir_name = if current_prefix.ends_with('/') {
        current_prefix.to_string()
    } else {
        format!("{}/", current_prefix)
    };

    let dir_options = options.unix_permissions(0o755);
    let _ = zip.add_directory(&dir_name, dir_options);

    let entries = std::fs::read_dir(current_dir)
        .map_err(|e| format!("Failed to read directory {:?}: {}", current_dir, e))?;

    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        let next_prefix = format!("{}{}", dir_name, name);

        if path.is_file() {
            zip.start_file(&next_prefix, options)
                .map_err(|e| format!("Failed to add entry {} to zip: {}", next_prefix, e))?;
            let mut f = File::open(&path)
                .map_err(|e| format!("Failed to read entry {:?}: {}", path, e))?;
            std::io::copy(&mut f, zip)
                .map_err(|e| format!("Failed to write entry {:?} into zip: {}", path, e))?;
            *count += 1;
        } else if path.is_dir() {
            add_dir_recursive(zip, &path, &next_prefix, count, options)?;
        }
    }

    Ok(())
}

pub fn extract_zip(zip_path: &str, dest_dir: &str) -> Result<usize, String> {
    let zip_file = Path::new(zip_path);
    if !zip_file.exists() {
        return Err(format!("Archive file not found: {}", zip_path));
    }

    let file = File::open(zip_file)
        .map_err(|e| format!("Failed to open zip archive {:?}: {}", zip_file, e))?;
    let mut archive = ZipArchive::new(file)
        .map_err(|e| format!("Failed to parse zip archive: {}", e))?;

    let dest = Path::new(dest_dir);
    std::fs::create_dir_all(dest)
        .map_err(|e| format!("Failed to create destination directory {:?}: {}", dest, e))?;

    let mut count = 0;

    for i in 0..archive.len() {
        let mut entry = archive.by_index(i)
            .map_err(|e| format!("Failed to read zip entry #{}: {}", i, e))?;

        // Guard against Zip Slip directory traversal vulnerabilities
        let enclosed_name = match entry.enclosed_name() {
            Some(path) => path.to_owned(),
            None => continue,
        };

        let out_path = dest.join(enclosed_name);

        if entry.is_dir() {
            std::fs::create_dir_all(&out_path)
                .map_err(|e| format!("Failed to create directory {:?}: {}", out_path, e))?;
        } else {
            if let Some(parent) = out_path.parent() {
                if !parent.exists() {
                    std::fs::create_dir_all(parent)
                        .map_err(|e| format!("Failed to create directory {:?}: {}", parent, e))?;
                }
            }

            let mut out_file = File::create(&out_path)
                .map_err(|e| format!("Failed to create extracted file {:?}: {}", out_path, e))?;
            std::io::copy(&mut entry, &mut out_file)
                .map_err(|e| format!("Failed to write extracted file {:?}: {}", out_path, e))?;
            count += 1;
        }

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Some(mode) = entry.unix_mode() {
                let _ = std::fs::set_permissions(&out_path, std::fs::Permissions::from_mode(mode));
            }
        }
    }

    Ok(count)
}

/// Safely quotes an argument for POSIX shells to avoid any command injection
pub fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Checks if a filename matches supported archive extensions
#[allow(dead_code)]
pub fn is_archive_filename(filename: &str) -> bool {
    let lower = filename.to_lowercase();
    lower.ends_with(".zip")
        || lower.ends_with(".tar.gz")
        || lower.ends_with(".tgz")
        || lower.ends_with(".tar.bz2")
        || lower.ends_with(".tbz2")
        || lower.ends_with(".tar.xz")
        || lower.ends_with(".txz")
        || lower.ends_with(".tar")
        || lower.ends_with(".gz")
}

pub fn remote_compress(
    client: &mut RemoteClient,
    sources: &[String],
    dest_zip: &str,
    cancelled: &std::sync::atomic::AtomicBool,
    exec_script: &Option<ScriptCallback>,
) -> Result<serde_json::Value, String> {
    if sources.is_empty() {
        return Err("No remote sources specified for compression".to_string());
    }

    let parent_dir = if let Some(idx) = dest_zip.rfind('/') {
        if idx == 0 { "/" } else { &dest_zip[..idx] }
    } else {
        "."
    };
    let archive_filename = dest_zip.rsplit('/').next().unwrap_or(dest_zip);
    let dest_lower = dest_zip.to_lowercase();
    let is_tar = dest_lower.ends_with(".tar.gz") || dest_lower.ends_with(".tgz") || dest_lower.ends_with(".tar");

    // 1. Try Native SSH server-side execution if SFTP
    let mut native_succeeded = false;
    let mut native_count = 0usize;

    if let RemoteClient::Sftp(sftp) = client {
        let check_cmd = if is_tar { "which tar || command -v tar" } else { "which zip || command -v zip" };
        if let Ok((0, tool_path)) = sftp.exec_command_clean(check_cmd) {
            let tool_trimmed = tool_path.trim();
            if !tool_trimmed.is_empty() && !tool_trimmed.contains("not found") {
                let safe_parent = shell_quote(parent_dir);
                let safe_archive_name = shell_quote(archive_filename);
                let rel_sources: Vec<String> = sources.iter().map(|s| {
                    let rel = if s.starts_with(parent_dir) {
                        s[parent_dir.len()..].trim_start_matches('/')
                    } else {
                        s.rsplit('/').next().unwrap_or(s)
                    };
                    shell_quote(rel)
                }).collect();

                let cmd = if is_tar {
                    format!("cd {} && tar -czvf {} {}", safe_parent, safe_archive_name, rel_sources.join(" "))
                } else {
                    format!("cd {} && zip -r {} {}", safe_parent, safe_archive_name, rel_sources.join(" "))
                };

                if let Some(cb) = exec_script {
                    cb(format!("window.onArchiveProgress({{ mode: 'compress', currentItem: 'Compressing on remote server (SSH)...', count: 0, total: {}, percent: 1, status: 'Running server compression...' }});", sources.len()));
                }

                if let Ok((code, output)) = sftp.exec_command_clean(&cmd) {
                    if code == 0 {
                        native_succeeded = true;
                        let line_count = output.lines().filter(|l| !l.trim().is_empty()).count();
                        native_count = line_count.max(sources.len());
                    }
                }
            }
        }
    }

    if native_succeeded {
        if let Some(cb) = exec_script {
            cb(format!("window.onArchiveProgress({{ mode: 'compress', currentItem: 'Compression complete!', count: {}, total: {}, percent: 100, status: 'Finished successfully' }});", native_count, native_count));
        }
        return Ok(serde_json::json!({
            "count": native_count,
            "dest_zip": dest_zip,
            "method": "native_ssh"
        }));
    }

    // 2. High-Performance Pure Streaming Engine Fallback (FTP/FTPS or SFTP without zip/tar)
    let temp_path = std::env::temp_dir().join(format!("90ftp_rem_comp_{}_{}.zip", std::process::id(), rand::random::<u32>()));
    let file = File::create(&temp_path)
        .map_err(|e| format!("Failed to create local temp archive: {}", e))?;
    let mut zip = ZipWriter::new(file);
    let zip_options = SimpleFileOptions::default()
        .compression_method(CompressionMethod::Deflated)
        .unix_permissions(0o644);

    let mut count = 0usize;
    for src in sources {
        if cancelled.load(std::sync::atomic::Ordering::SeqCst) {
            let _ = std::fs::remove_file(&temp_path);
            return Err("Remote compression cancelled by user".to_string());
        }
        let base_name = src.rsplit('/').next().unwrap_or(src);
        let is_dir = if let Ok(Some(item)) = client.stat(src) {
            item.is_dir
        } else {
            false
        };

        if !is_dir {
            match client.download_file(src) {
                Ok(bytes) => {
                    let _ = zip.start_file(base_name, zip_options);
                    let _ = zip.write_all(&bytes);
                    count += 1;
                    if let Some(cb) = exec_script {
                        cb(format!("window.onArchiveProgress({{ mode: 'compress', currentItem: '{}', count: {}, total: {}, percent: {}, status: 'Streaming file to archive...' }});", base_name, count, sources.len(), (count * 100) / sources.len()));
                    }
                }
                Err(e) => {
                    crate::logger::log_warn(&format!("remote_compress: skipping file {}: {}", src, e));
                }
            }
        } else {
            add_remote_dir_to_zip(client, &mut zip, zip_options, src, base_name, &mut count, cancelled, exec_script)?;
        }
    }

    zip.finish().map_err(|e| format!("Failed to finalize zip archive: {}", e))?;

    if let Some(cb) = exec_script {
        cb(format!("window.onArchiveProgress({{ mode: 'compress', currentItem: 'Uploading finished archive to remote server...', count: {}, total: {}, percent: 95, status: 'Uploading archive...' }});", count, count));
    }

    // Ensure parent directory exists before uploading
    let _ = client.make_dir_all(parent_dir);

    let temp_str = temp_path.to_string_lossy().to_string();
    let upload_res = client.upload_from_local_file(&temp_str, dest_zip);
    let _ = std::fs::remove_file(&temp_path);

    upload_res.map_err(|e| format!("Failed to upload created archive to {}: {}", dest_zip, e))?;

    if let Some(cb) = exec_script {
        cb(format!("window.onArchiveProgress({{ mode: 'compress', currentItem: 'Archive created successfully!', count: {}, total: {}, percent: 100, status: 'Completed' }});", count, count));
    }

    Ok(serde_json::json!({
        "count": count,
        "dest_zip": dest_zip,
        "method": "streaming_engine"
    }))
}

fn add_remote_dir_to_zip(
    client: &mut RemoteClient,
    zip: &mut ZipWriter<File>,
    zip_options: SimpleFileOptions,
    remote_dir: &str,
    prefix: &str,
    count: &mut usize,
    cancelled: &std::sync::atomic::AtomicBool,
    exec_script: &Option<ScriptCallback>,
) -> Result<(), String> {
    if cancelled.load(std::sync::atomic::Ordering::SeqCst) {
        return Ok(());
    }
    let dir_name = if prefix.ends_with('/') { prefix.to_string() } else { format!("{}/", prefix) };
    let _ = zip.add_directory(&dir_name, zip_options);

    let entries = client.list(Some(remote_dir))?;
    for item in entries {
        if item.name == "." || item.name == ".." { continue; }
        let item_remote = format!("{}/{}", remote_dir.trim_end_matches('/'), item.name);
        let item_prefix = format!("{}{}", dir_name, item.name);

        if item.is_dir {
            add_remote_dir_to_zip(client, zip, zip_options, &item_remote, &item_prefix, count, cancelled, exec_script)?;
        } else {
            if let Ok(bytes) = client.download_file(&item_remote) {
                let _ = zip.start_file(&item_prefix, zip_options);
                let _ = zip.write_all(&bytes);
                *count += 1;
                if let Some(cb) = exec_script {
                    cb(format!("window.onArchiveProgress({{ mode: 'compress', currentItem: '{}', count: {}, total: -1, percent: -1, status: 'Streaming folder...' }});", item.name, *count));
                }
            }
        }
    }
    Ok(())
}

pub fn remote_extract(
    client: &mut RemoteClient,
    archive_path: &str,
    dest_dir: &str,
    overwrite_mode: &str,
    cancelled: &std::sync::atomic::AtomicBool,
    exec_script: &Option<ScriptCallback>,
) -> Result<serde_json::Value, String> {
    let _ = client.make_dir(dest_dir);

    let arch_lower = archive_path.to_lowercase();
    let is_tar = arch_lower.ends_with(".tar.gz")
        || arch_lower.ends_with(".tgz")
        || arch_lower.ends_with(".tar.bz2")
        || arch_lower.ends_with(".tbz2")
        || arch_lower.ends_with(".tar.xz")
        || arch_lower.ends_with(".tar");

    let mut native_success = false;
    let mut extracted_count = 0usize;
    let mut skipped_count = 0usize;

    // 1. Try Native SSH Extraction
    if let RemoteClient::Sftp(sftp) = client {
        let check_cmd = if is_tar { "which tar || command -v tar" } else { "which unzip || command -v unzip" };
        if let Ok((code, out)) = sftp.exec_command_clean(check_cmd) {
            if code == 0 && !out.trim().is_empty() {
                let safe_archive = shell_quote(archive_path);
                let safe_dest = shell_quote(dest_dir);

                let cmd = if is_tar {
                    let ov_flag = match overwrite_mode {
                        "skip" => "--skip-old-files",
                        "newer" => "--keep-newer-files",
                        _ => "--overwrite",
                    };
                    format!("mkdir -p {} && tar -xvf {} -C {} {}", safe_dest, safe_archive, safe_dest, ov_flag)
                } else {
                    let ov_flag = match overwrite_mode {
                        "skip" => "-n",
                        "newer" => "-u",
                        _ => "-o",
                    };
                    format!("mkdir -p {} && unzip {} {} -d {}", safe_dest, ov_flag, safe_archive, safe_dest)
                };

                if let Some(cb) = exec_script {
                    cb("window.onArchiveProgress({ mode: 'extract', currentItem: 'Extracting natively on remote server...', count: 0, skipped: 0, total: -1, percent: 1, status: 'Running native SSH unzip...' });".to_string());
                }

                if let Ok((exit_code, out)) = sftp.exec_command_clean(&cmd) {
                    if exit_code == 0 {
                        native_success = true;
                        extracted_count = out.lines().filter(|l| !l.trim().is_empty()).count();
                    } else {
                        crate::logger::log_warn(&format!("Native remote extraction failed (exit {}): {}", exit_code, out));
                    }
                }
            }
        }
    }

    if native_success {
        if let Some(cb) = exec_script {
            cb(format!("window.onArchiveProgress({{ mode: 'extract', currentItem: 'Extraction completed successfully!', count: {}, skipped: 0, total: {}, percent: 100, status: 'Finished successfully' }});", extracted_count, extracted_count));
        }
        return Ok(serde_json::json!({
            "count": extracted_count,
            "skipped": 0,
            "dest_dir": dest_dir,
            "method": "native_ssh"
        }));
    }

    // 2. High-Performance Pure Streaming Engine Fallback (FTP/FTPS or SFTP without unzip)
    let temp_archive_path = std::env::temp_dir().join(format!("90ftp_rem_ext_{}_{}.zip", std::process::id(), rand::random::<u32>()));
    let temp_str = temp_archive_path.to_string_lossy().to_string();

    if let Some(cb) = exec_script {
        cb("window.onArchiveProgress({ mode: 'extract', currentItem: 'Downloading remote archive for extraction...', count: 0, skipped: 0, total: -1, percent: 5, status: 'Streaming archive...' });".to_string());
    }

    if let Err(e) = client.download_to_local_file(archive_path, &temp_str) {
        let _ = std::fs::remove_file(&temp_archive_path);
        return Err(format!("Failed to stream remote archive for extraction: {}", e));
    }

    let file = File::open(&temp_archive_path)
        .map_err(|e| {
            let _ = std::fs::remove_file(&temp_archive_path);
            format!("Failed to open temp archive: {}", e)
        })?;

    let mut zip = ZipArchive::new(file)
        .map_err(|e| {
            let _ = std::fs::remove_file(&temp_archive_path);
            format!("Failed to parse zip archive: {}", e)
        })?;

    let total_entries = zip.len();

    for i in 0..total_entries {
        if cancelled.load(std::sync::atomic::Ordering::SeqCst) {
            let _ = std::fs::remove_file(&temp_archive_path);
            return Err("Remote extraction cancelled by user".to_string());
        }

        let mut entry = match zip.by_index(i) {
            Ok(e) => e,
            Err(_) => continue,
        };

        // Guard against Zip Slip directory traversal vulnerabilities
        let enclosed_name = match entry.enclosed_name() {
            Some(path) => path.to_owned(),
            None => continue,
        };

        let rel_str = enclosed_name.to_string_lossy().replace('\\', "/");
        let full_remote = format!("{}/{}", dest_dir.trim_end_matches('/'), rel_str);
        let percent = if total_entries > 0 { ((i + 1) * 100) / total_entries } else { 100 };

        if entry.is_dir() {
            let _ = client.make_dir_all(&full_remote);
            continue;
        }

        // Collision handling: Overwrite vs Skip vs Newer
        if overwrite_mode == "skip" {
            if let Ok(Some(_)) = client.stat(&full_remote) {
                skipped_count += 1;
                if let Some(cb) = exec_script {
                    cb(format!("window.onArchiveProgress({{ mode: 'extract', currentItem: '{}', count: {}, skipped: {}, total: {}, percent: {}, status: 'Skipped existing file' }});", rel_str, extracted_count, skipped_count, total_entries, percent));
                }
                continue;
            }
        } else if overwrite_mode == "newer" {
            if let Ok(Some(existing)) = client.stat(&full_remote) {
                if existing.size == entry.size() {
                    skipped_count += 1;
                    if let Some(cb) = exec_script {
                        cb(format!("window.onArchiveProgress({{ mode: 'extract', currentItem: '{}', count: {}, skipped: {}, total: {}, percent: {}, status: 'Skipped (identical size)' }});", rel_str, extracted_count, skipped_count, total_entries, percent));
                    }
                    continue;
                }
            }
        }

        // Ensure parent directory exists on remote
        if let Some(idx) = full_remote.rfind('/') {
            if idx > 0 {
                let parent = &full_remote[..idx];
                let _ = client.make_dir_all(parent);
            }
        }

        let mut buf = Vec::with_capacity(entry.size() as usize);
        if std::io::copy(&mut entry, &mut buf).is_ok() {
            if client.upload_file(&full_remote, &buf).is_ok() {
                extracted_count += 1;
                if let Some(cb) = exec_script {
                    cb(format!("window.onArchiveProgress({{ mode: 'extract', currentItem: '{}', count: {}, skipped: {}, total: {}, percent: {}, status: 'Extracted file' }});", rel_str, extracted_count, skipped_count, total_entries, percent));
                }
            }
        }
    }

    let _ = std::fs::remove_file(&temp_archive_path);

    if let Some(cb) = exec_script {
        cb(format!("window.onArchiveProgress({{ mode: 'extract', currentItem: 'Extraction complete!', count: {}, skipped: {}, total: {}, percent: 100, status: 'Completed successfully' }});", extracted_count, skipped_count, total_entries));
    }

    Ok(serde_json::json!({
        "count": extracted_count,
        "skipped": skipped_count,
        "dest_dir": dest_dir,
        "method": "streaming_engine"
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_zip_roundtrip() {
        let temp_dir = std::env::temp_dir().join(format!("90ftp_zip_test_{}", rand::random::<u32>()));
        let src_dir = temp_dir.join("src");
        let dest_dir = temp_dir.join("dest");
        let zip_file = temp_dir.join("test_archive.zip");

        std::fs::create_dir_all(&src_dir).unwrap();
        let sub_dir = src_dir.join("nested");
        std::fs::create_dir_all(&sub_dir).unwrap();

        std::fs::write(src_dir.join("hello.txt"), "Hello, 90FTP!").unwrap();
        std::fs::write(sub_dir.join("inner.json"), "{\"status\": \"ok\"}").unwrap();

        let sources = vec![
            src_dir.join("hello.txt").to_string_lossy().to_string(),
            sub_dir.to_string_lossy().to_string(),
        ];

        let count = compress_to_zip(&sources, &zip_file.to_string_lossy()).unwrap();
        assert!(count >= 2);
        assert!(zip_file.exists());

        let extracted_count = extract_zip(&zip_file.to_string_lossy(), &dest_dir.to_string_lossy()).unwrap();
        assert!(extracted_count >= 2);

        let read_hello = std::fs::read_to_string(dest_dir.join("hello.txt")).unwrap();
        assert_eq!(read_hello, "Hello, 90FTP!");

        let read_inner = std::fs::read_to_string(dest_dir.join("nested").join("inner.json")).unwrap();
        assert_eq!(read_inner, "{\"status\": \"ok\"}");

        let _ = std::fs::remove_dir_all(&temp_dir);
    }
}
