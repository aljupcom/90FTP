use std::io::Cursor;
use suppaftp::{FtpStream, ImplFtpStream, NativeTlsConnector, NativeTlsFtpStream, TlsStream};
use crate::models::FileItem;
use crate::transfer_control::{CHUNK_SIZE, TransferControl};

/// Transport-agnostic FTP surface consumed by `RemoteClient`.
///
/// Implemented once for every TLS backend, so plain FTP (`ImplFtpStream<TcpStream>`)
/// and explicit FTPS (`ImplFtpStream<NativeTlsStream>`, i.e. AUTH TLS) share a single
/// code path. This removes the previous situation where "ftps" silently degraded to
/// a cleartext control channel.
pub trait FtpBackend: Send {
    #[allow(dead_code)]
    fn current_dir(&self) -> &str;
    #[allow(dead_code)]
    fn host(&self) -> &str;
    #[allow(dead_code)]
    fn username(&self) -> &str;
    fn is_secure(&self) -> bool;
    fn list(&mut self, path: Option<&str>) -> Result<Vec<FileItem>, String>;
    fn change_dir(&mut self, path: &str) -> Result<String, String>;
    #[allow(dead_code)]
    fn cdup(&mut self) -> Result<String, String>;
    fn download_file(&mut self, remote_file: &str) -> Result<Vec<u8>, String>;
    /// Steerable download. `control` decides when to pause, stop or slow down.
    fn download_to_local_file_ctl(
        &mut self,
        remote_file: &str,
        local_path: &str,
        control: &TransferControl,
    ) -> Result<u64, String>;
    fn upload_file(&mut self, remote_file: &str, data: &[u8]) -> Result<(), String>;
    /// Steerable upload.
    fn upload_from_local_file_ctl(
        &mut self,
        local_path: &str,
        remote_file: &str,
        control: &TransferControl,
    ) -> Result<u64, String>;

    /// Unsteerable wrappers, for callers with no queue behind them (archive
    /// staging, the server-to-server dispatcher, tests).
    fn download_to_local_file(&mut self, remote_file: &str, local_path: &str) -> Result<u64, String> {
        self.download_to_local_file_ctl(remote_file, local_path, &TransferControl::unbounded())
    }
    fn upload_from_local_file(&mut self, local_path: &str, remote_file: &str) -> Result<u64, String> {
        self.upload_from_local_file_ctl(local_path, remote_file, &TransferControl::unbounded())
    }
    fn stat(&mut self, remote_path: &str) -> Result<Option<FileItem>, String>;
    fn make_dir(&mut self, dir_name: &str) -> Result<(), String>;
    fn remove_file(&mut self, file_name: &str) -> Result<(), String>;
    fn remove_dir(&mut self, dir_name: &str) -> Result<(), String>;
    fn chmod(&mut self, mode: &str, file_name: &str) -> Result<(), String>;
    fn rename(&mut self, from: &str, to: &str) -> Result<(), String>;
    fn ping(&mut self) -> Result<(), String>;
}

#[allow(dead_code)]
pub struct FtpClient<T: TlsStream> {
    stream: ImplFtpStream<T>,
    current_dir: String,
    host: String,
    username: String,
    secure: bool,
}

impl<T: TlsStream> FtpClient<T> {
    fn from_stream(mut stream: ImplFtpStream<T>, host: &str, user: &str, secure: bool) -> Self {
        let current_dir = stream.pwd().unwrap_or_else(|_| "/".to_string());
        Self {
            stream,
            current_dir,
            host: host.to_string(),
            username: user.to_string(),
            secure,
        }
    }
}

/// Plain, unencrypted FTP (RFC 959). Credentials cross the wire in cleartext —
/// only offered when the user explicitly picks the `ftp` protocol.
pub fn connect_plain(
    host: &str,
    port: u16,
    user: &str,
    pass: &str,
) -> Result<Box<dyn FtpBackend>, String> {
    let addr = format!("{}:{}", host, port);
    let mut stream =
        FtpStream::connect(&addr).map_err(|e| format!("FTP Connection failed to {}: {}", addr, e))?;

    stream
        .login(user, pass)
        .map_err(|e| format!("FTP Login failed for user {}: {}", user, e))?;

    Ok(Box::new(FtpClient::from_stream(stream, host, user, false)))
}

/// Explicit FTPS / FTPES: connects, upgrades the control channel with `AUTH TLS`,
/// negotiates `PBSZ 0` + `PROT P` (handled by `into_secure`) and only then sends
/// credentials. Certificate validation uses the platform trust store (SChannel).
pub fn connect_secure(
    host: &str,
    port: u16,
    user: &str,
    pass: &str,
) -> Result<Box<dyn FtpBackend>, String> {
    let addr = format!("{}:{}", host, port);
    let stream = NativeTlsFtpStream::connect(&addr)
        .map_err(|e| format!("FTPS Connection failed to {}: {}", addr, e))?;

    let connector = native_tls::TlsConnector::builder()
        .build()
        .map_err(|e| format!("FTPS TLS engine initialisation failed: {}", e))?;

    let mut stream = stream
        .into_secure(NativeTlsConnector::from(connector), host)
        .map_err(|e| format!("FTPS handshake (AUTH TLS) failed for {}: {}", addr, e))?;

    stream
        .login(user, pass)
        .map_err(|e| format!("FTPS Login failed for user {}: {}", user, e))?;

    Ok(Box::new(FtpClient::from_stream(stream, host, user, true)))
}

/// Implicit FTPS (usually port 990): TLS is negotiated the moment the socket
/// opens, before the control channel speaks any FTP at all — so there is no
/// `AUTH TLS` step and the server must be configured for it. Same certificate
/// validation as explicit FTPS, and no cleartext fallback either way.
pub fn connect_secure_implicit(
    host: &str,
    port: u16,
    user: &str,
    pass: &str,
) -> Result<Box<dyn FtpBackend>, String> {
    let addr = format!("{}:{}", host, port);

    let connector = native_tls::TlsConnector::builder()
        .build()
        .map_err(|e| format!("Implicit FTPS TLS engine initialisation failed: {}", e))?;

    let mut stream = NativeTlsFtpStream::connect_secure_implicit(
        addr.as_str(),
        NativeTlsConnector::from(connector),
        host,
    )
    .map_err(|e| format!("Implicit FTPS handshake failed for {}: {}", addr, e))?;

    stream
        .login(user, pass)
        .map_err(|e| format!("Implicit FTPS Login failed for user {}: {}", user, e))?;

    Ok(Box::new(FtpClient::from_stream(stream, host, user, true)))
}

impl<T: TlsStream + Send + 'static> FtpBackend for FtpClient<T> {
    fn current_dir(&self) -> &str {
        &self.current_dir
    }

    fn host(&self) -> &str {
        &self.host
    }

    fn username(&self) -> &str {
        &self.username
    }

    fn is_secure(&self) -> bool {
        self.secure
    }

    fn list(&mut self, path: Option<&str>) -> Result<Vec<FileItem>, String> {
        let target_dir = path.unwrap_or(&self.current_dir);
        let lines = self
            .stream
            .list(Some(target_dir))
            .map_err(|e| format!("Failed to list directory: {}", e))?;

        let mut items = Vec::new();

        for line in lines {
            let line_trimmed = line.trim();
            if line_trimmed.is_empty() {
                continue;
            }

            // Standard Unix format: drwxr-xr-x 2 user group 4096 Oct 18 12:00 filename
            // Windows format: 10-18-26 12:00PM <DIR> foldername or size filename
            if let Some(item) = parse_ftp_list_line(line_trimmed, target_dir) {
                if item.name != "." && item.name != ".." {
                    items.push(item);
                }
            }
        }

        // Sort directories first, then alphabetical
        items.sort_by(|a, b| {
            if a.is_dir != b.is_dir {
                b.is_dir.cmp(&a.is_dir)
            } else {
                a.name.to_lowercase().cmp(&b.name.to_lowercase())
            }
        });

        Ok(items)
    }

    fn change_dir(&mut self, path: &str) -> Result<String, String> {
        self.stream
            .cwd(path)
            .map_err(|e| format!("Failed to change directory to {}: {}", path, e))?;
        let new_pwd = self.stream.pwd().unwrap_or_else(|_| path.to_string());
        self.current_dir = new_pwd.clone();
        Ok(new_pwd)
    }

    fn cdup(&mut self) -> Result<String, String> {
        self.stream
            .cdup()
            .map_err(|e| format!("Failed to navigate up: {}", e))?;
        let new_pwd = self.stream.pwd().unwrap_or_else(|_| "/".to_string());
        self.current_dir = new_pwd.clone();
        Ok(new_pwd)
    }

    fn download_file(&mut self, remote_file: &str) -> Result<Vec<u8>, String> {
        let cursor = self
            .stream
            .retr_as_buffer(remote_file)
            .map_err(|e| format!("Failed to download {}: {}", remote_file, e))?;
        Ok(cursor.into_inner())
    }

    fn download_to_local_file_ctl(
        &mut self,
        remote_file: &str,
        local_path: &str,
        control: &TransferControl,
    ) -> Result<u64, String> {
        use std::io::{Read, Write};

        if let Some(parent) = std::path::Path::new(local_path).parent() {
            let _ = std::fs::create_dir_all(parent);
        }

        // Stream straight to disk. Buffering the whole payload first would need
        // as much RAM as the file is large, which fails outright on multi-GB
        // files and stalls the app for the duration of the download.
        let mut local_f = std::fs::File::create(local_path)
            .map_err(|e| format!("Failed to create local file {}: {}", local_path, e))?;

        let mut stream = match self.stream.retr_as_stream(remote_file) {
            Ok(s) => s,
            Err(e) => {
                // The destination was created above, so it must be removed here
                // too: otherwise a missing remote file leaves an empty local file
                // that looks like a successful zero-byte download.
                drop(local_f);
                let _ = std::fs::remove_file(local_path);
                return Err(format!("Failed to download {}: {}", remote_file, e));
            }
        };

        let mut total = 0u64;
        let mut chunk = vec![0u8; CHUNK_SIZE];
        let copied = loop {
            match stream.read(&mut chunk) {
                Ok(0) => break Ok(()),
                Ok(n) => {
                    if let Err(e) = local_f.write_all(&chunk[..n]) {
                        break Err(format!("Failed writing to local file {}: {}", local_path, e));
                    }
                    total += n as u64;
                    // Checked after the write so a pause never loses bytes that
                    // were already read off the socket.
                    if let Err(e) = control.on_chunk(n) {
                        break Err(e);
                    }
                }
                Err(e) => break Err(format!("Failed reading remote file {}: {}", remote_file, e)),
            }
        };

        // `finish` closes the data connection and reads the server's completion
        // reply. It must run even when the copy failed: skipping it leaves the
        // control channel out of step for the next command on this session.
        let finished = stream
            .finish()
            .map_err(|e| format!("Transfer of {} did not complete: {}", remote_file, e));

        if let Err(e) = copied.and(finished) {
            // A truncated file that looks complete is worse than no file, so the
            // partial output is removed rather than left behind.
            drop(local_f);
            let _ = std::fs::remove_file(local_path);
            return Err(e);
        }

        local_f
            .flush()
            .map_err(|e| format!("Failed to flush local file {}: {}", local_path, e))?;
        Ok(total)
    }

    fn upload_file(&mut self, remote_file: &str, data: &[u8]) -> Result<(), String> {
        let mut cursor = Cursor::new(data);
        self.stream
            .put_file(remote_file, &mut cursor)
            .map_err(|e| format!("Failed to upload {}: {}", remote_file, e))?;
        Ok(())
    }

    fn upload_from_local_file_ctl(
        &mut self,
        local_path: &str,
        remote_file: &str,
        control: &TransferControl,
    ) -> Result<u64, String> {
        use std::io::{Read, Write};

        // Streamed for the same reason as downloads: reading the file into a Vec
        // first caps the transfer size at available RAM.
        let mut local_f = std::fs::File::open(local_path)
            .map_err(|e| format!("Failed to open local file {}: {}", local_path, e))?;

        let mut stream = self
            .stream
            .put_with_stream(remote_file)
            .map_err(|e| format!("Failed to create remote file {}: {}", remote_file, e))?;

        let mut total = 0u64;
        let mut chunk = vec![0u8; CHUNK_SIZE];
        let copied = loop {
            match local_f.read(&mut chunk) {
                Ok(0) => break Ok(()),
                Ok(n) => {
                    if let Err(e) = stream.write_all(&chunk[..n]) {
                        break Err(format!("Failed uploading to {}: {}", remote_file, e));
                    }
                    total += n as u64;
                    if let Err(e) = control.on_chunk(n) {
                        break Err(e);
                    }
                }
                Err(e) => break Err(format!("Failed reading local file {}: {}", local_path, e)),
            }
        };

        let finished = stream
            .finish()
            .map_err(|e| format!("Upload of {} did not complete: {}", remote_file, e));

        copied.and(finished)?;
        Ok(total)
    }

    fn stat(&mut self, remote_path: &str) -> Result<Option<FileItem>, String> {
        let parent: String = if let Some(idx) = remote_path.rfind('/') {
            if idx == 0 { "/".to_string() } else { remote_path[..idx].to_string() }
        } else {
            self.current_dir.clone()
        };
        let target_name = remote_path.rsplit('/').next().unwrap_or(remote_path);
        let items = self.list(Some(&parent))?;
        for item in items {
            if item.name.eq_ignore_ascii_case(target_name) {
                return Ok(Some(item));
            }
        }
        Ok(None)
    }

    fn make_dir(&mut self, dir_name: &str) -> Result<(), String> {
        match self.stream.mkdir(dir_name) {
            Ok(_) => Ok(()),
            Err(e) => {
                let err_str = e.to_string();
                if err_str.contains("exists") || err_str.contains("File exists") || err_str.contains("550") {
                    Ok(())
                } else {
                    Err(format!("Failed to create folder {}: {}", dir_name, err_str))
                }
            }
        }
    }

    fn remove_file(&mut self, file_name: &str) -> Result<(), String> {
        if self.stream.rm(file_name).is_ok() {
            return Ok(());
        }
        // Fallback: if full path fails on chrooted FTP servers, try relative basename from parent dir
        if let Some((parent, base)) = file_name.rsplit_once('/') {
            if !base.is_empty() {
                let target_parent = if parent.is_empty() { "/" } else { parent };
                let prev_dir = self.current_dir.clone();
                if self.stream.cwd(target_parent).is_ok() {
                    let res = self.stream.rm(base);
                    let _ = self.stream.cwd(&prev_dir);
                    if res.is_ok() {
                        return Ok(());
                    }
                }
            }
        }
        // Fallback: if item is actually an empty directory or directory symlink
        if self.stream.rmdir(file_name).is_ok() {
            return Ok(());
        }
        self.stream
            .rm(file_name)
            .map_err(|e| format!("Failed to delete file {}: {}", file_name, e))
    }

    fn remove_dir(&mut self, dir_name: &str) -> Result<(), String> {
        // 1. Fast path for empty directory
        if self.stream.rmdir(dir_name).is_ok() {
            return Ok(());
        }

        // 2. Recursively delete contents (including hidden dotfiles via -a listing if supported)
        let mut children = self.list(Some(dir_name)).unwrap_or_default();
        let list_a_arg = format!("-a {}", dir_name);
        if let Ok(extra_lines) = self.stream.list(Some(&list_a_arg)) {
            for line in extra_lines {
                if let Some(item) = parse_ftp_list_line(&line, dir_name) {
                    if item.name != "." && item.name != ".." && !children.iter().any(|c| c.name == item.name) {
                        children.push(item);
                    }
                }
            }
        }

        for child in children {
            if child.name == "." || child.name == ".." {
                continue;
            }
            if child.is_dir && !child.is_symlink {
                let _ = self.remove_dir(&child.path);
            } else {
                let _ = self.remove_file(&child.path);
            }
        }

        if self.stream.rmdir(dir_name).is_ok() {
            return Ok(());
        }

        // 3. Relative basename fallback
        if let Some((parent, base)) = dir_name.rsplit_once('/') {
            if !base.is_empty() {
                let target_parent = if parent.is_empty() { "/" } else { parent };
                let prev_dir = self.current_dir.clone();
                if self.stream.cwd(target_parent).is_ok() {
                    let res = self.stream.rmdir(base);
                    let _ = self.stream.cwd(&prev_dir);
                    if res.is_ok() {
                        return Ok(());
                    }
                }
            }
        }

        // 4. Fallback if directory entry is actually a symlink or file
        if self.stream.rm(dir_name).is_ok() {
            return Ok(());
        }

        self.stream
            .rmdir(dir_name)
            .map_err(|e| format!("Failed to delete folder {}: {}", dir_name, e))
    }

    fn chmod(&mut self, mode: &str, file_name: &str) -> Result<(), String> {
        let cmd = format!("CHMOD {} {}", mode, file_name);
        self.stream
            .site(&cmd)
            .map_err(|e| format!("Failed to set permissions: {}", e))?;
        Ok(())
    }

    fn rename(&mut self, from: &str, to: &str) -> Result<(), String> {
        self.stream.rename(from, to).map_err(|e| format!("FTP rename failed: {}", e))
    }

    fn ping(&mut self) -> Result<(), String> {
        self.stream.noop().map_err(|e| format!("FTP keepalive failed: {}", e))
    }
}

fn strip_ansi_codes(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            if chars.peek() == Some(&'[') {
                chars.next();
                while let Some(&nc) = chars.peek() {
                    chars.next();
                    if ('@'..='~').contains(&nc) {
                        break;
                    }
                }
            }
        } else if c != '\r' {
            out.push(c);
        }
    }
    out
}

fn clean_symlink_name(raw_name: &str, is_symlink: bool) -> String {
    if is_symlink {
        if let Some((link_name, _target)) = raw_name.split_once(" -> ") {
            return link_name.trim().to_string();
        }
    }
    raw_name.to_string()
}

// Parses both Unix-style (8-col ISO date & 9-col standard date) and DOS/Windows-style FTP/SSH LIST output
pub(crate) fn parse_ftp_list_line(line: &str, base_path: &str) -> Option<FileItem> {
    let clean_line = strip_ansi_codes(line);
    let trimmed = clean_line.trim();
    if trimmed.is_empty() || trimmed.starts_with("total ") {
        return None;
    }

    let parts: Vec<&str> = trimmed.split_whitespace().collect();
    if parts.is_empty() {
        return None;
    }

    let first = parts[0];
    let looks_like_unix_perms = first.len() >= 9
        && matches!(
            first.chars().next(),
            Some('-' | 'd' | 'l' | 'c' | 'b' | 'p' | 's')
        )
        && first.chars().skip(1).take(3).all(|c| matches!(c, 'r' | 'w' | 'x' | 's' | 'S' | 't' | 'T' | '-'));

    if looks_like_unix_perms {
        let perms = first;
        let is_dir = perms.starts_with('d');
        let is_symlink = perms.starts_with('l');

        // Case A: 8-column ISO format from `ls -la --time-style=+%Y-%m-%d\ %H:%M`
        // perms links owner group size YYYY-MM-DD HH:MM name...
        if parts.len() >= 8 && parts[5].len() == 10 && parts[5].chars().nth(4) == Some('-') && parts[6].contains(':') {
            let size = parts[4].parse::<u64>().unwrap_or(0);
            let date_str = format!("{} {}", parts[5], parts[6]);
            // Extract exact filename preserving multiple spaces after parts[6]
            let raw_name = extract_nth_token_remainder(trimmed, 7).unwrap_or_else(|| parts[7..].join(" "));
            let name = clean_symlink_name(&raw_name, is_symlink);
            if name.is_empty() {
                return None;
            }
            let full_path = if base_path.ends_with('/') {
                format!("{}{}", base_path, name)
            } else {
                format!("{}/{}", base_path, name)
            };
            return Some(FileItem {
                name,
                path: full_path,
                is_dir,
                size,
                modified: date_str,
                permissions: perms.to_string(),
                is_symlink,
            });
        }

        // Case B: 9-column standard Unix format:
        // perms links owner group size Month Day Time/Year name...
        if parts.len() >= 9 {
            let size = parts[4].parse::<u64>().unwrap_or(0);
            let date_str = format!("{} {} {}", parts[5], parts[6], parts[7]);
            let raw_name = extract_nth_token_remainder(trimmed, 8).unwrap_or_else(|| parts[8..].join(" "));
            let name = clean_symlink_name(&raw_name, is_symlink);
            if name.is_empty() {
                return None;
            }
            let full_path = if base_path.ends_with('/') {
                format!("{}{}", base_path, name)
            } else {
                format!("{}/{}", base_path, name)
            };
            return Some(FileItem {
                name,
                path: full_path,
                is_dir,
                size,
                modified: date_str,
                permissions: perms.to_string(),
                is_symlink,
            });
        }

        return None;
    }

    // DOS format: 05-22-26 12:00PM <DIR> images
    if parts.len() >= 4 {
        let date_str = format!("{} {}", parts[0], parts[1]);
        let is_dir = parts[2].eq_ignore_ascii_case("<DIR>");
        let size = if is_dir { 0 } else { parts[2].parse::<u64>().unwrap_or(0) };
        let name = extract_nth_token_remainder(trimmed, 3).unwrap_or_else(|| parts[3..].join(" "));
        let full_path = if base_path.ends_with('/') {
            format!("{}{}", base_path, name)
        } else {
            format!("{}/{}", base_path, name)
        };

        return Some(FileItem {
            name,
            path: full_path,
            is_dir,
            size,
            modified: date_str,
            permissions: if is_dir { "drwxr-xr-x".to_string() } else { "-rw-r--r--".to_string() },
            is_symlink: false,
        });
    }

    None
}

fn extract_nth_token_remainder(s: &str, token_index: usize) -> Option<String> {
    let mut in_ws = true;
    let mut current_token = 0usize;
    for (byte_idx, ch) in s.char_indices() {
        if ch.is_whitespace() {
            in_ws = true;
        } else if in_ws {
            if current_token == token_index {
                return Some(s[byte_idx..].to_string());
            }
            current_token += 1;
            in_ws = false;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_ftp_list_line_8col_iso_hidden_and_symlinks() {
        let dot_line = "drwxr-xr-x 2 root root 4096 2026-10-23 06:40 .";
        let item = parse_ftp_list_line(dot_line, "/home/backup").expect("should parse .");
        assert_eq!(item.name, ".");
        assert!(item.is_dir);

        let hidden_line = "-rw-r--r-- 1 root root 512 2026-10-23 06:40 .htaccess";
        let h_item = parse_ftp_list_line(hidden_line, "/home/backup").expect("should parse .htaccess");
        assert_eq!(h_item.name, ".htaccess");
        assert_eq!(h_item.path, "/home/backup/.htaccess");
        assert!(!h_item.is_dir);

        let sym_line = "lrwxrwxrwx 1 root root 12 2026-10-23 06:40 .env_link -> /etc/secret/.env";
        let s_item = parse_ftp_list_line(sym_line, "/home/backup").expect("should parse symlink");
        assert_eq!(s_item.name, ".env_link");
        assert_eq!(s_item.path, "/home/backup/.env_link");
        assert!(s_item.is_symlink);
    }

    /// End-to-end check of the streamed transfer path against a real server.
    ///
    /// Ignored by default because it needs a live FTP server on 127.0.0.1:2121
    /// (user/pass `test`/`test`). Run it with:
    ///
    /// ```text
    /// python -m pyftpdlib -p 2121 -u test -P test -d <dir>
    /// cargo test --bin ninety_ftp -- --ignored ftp_streaming
    /// ```
    ///
    /// The payload is deliberately several buffers long and not a multiple of
    /// the buffer size, so a loop that drops the tail or miscounts the last
    /// partial chunk fails here rather than in production.
    #[test]
    #[ignore = "requires an FTP server on 127.0.0.1:2121"]
    fn ftp_streaming_roundtrip_preserves_bytes_and_size() {
        const SIZE: usize = 5 * 1024 * 1024 + 1234;

        let dir = std::env::temp_dir().join(format!("90ftp_ftpxfer_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // Deterministic but non-repeating-enough content to catch offset bugs.
        let payload: Vec<u8> = (0..SIZE).map(|i| (i % 251) as u8).collect();
        let src = dir.join("payload.bin");
        let dst = dir.join("roundtrip.bin");
        std::fs::write(&src, &payload).unwrap();

        let mut client = connect_plain("127.0.0.1", 2121, "test", "test")
            .expect("the test FTP server must be running on 127.0.0.1:2121");

        let uploaded = client
            .upload_from_local_file(&src.to_string_lossy(), "payload.bin")
            .expect("streamed upload should succeed");
        assert_eq!(uploaded, SIZE as u64, "upload must report every byte");

        // The control channel must still be usable after the transfer, which is
        // only true if the stream was finished properly.
        let remote = client
            .stat("payload.bin")
            .expect("stat should work after a finished transfer")
            .expect("the uploaded file should exist");
        assert_eq!(remote.size, SIZE as u64, "remote size must match the source");

        let downloaded = client
            .download_to_local_file("payload.bin", &dst.to_string_lossy())
            .expect("streamed download should succeed");
        assert_eq!(downloaded, SIZE as u64, "download must report every byte");
        assert_eq!(std::fs::read(&dst).unwrap(), payload, "round-trip must be byte-exact");

        // A second transfer on the same session proves `finish()` left the
        // control connection in step rather than one reply behind.
        let again = client
            .download_to_local_file("payload.bin", &dir.join("second.bin").to_string_lossy())
            .expect("a second transfer on the same session should succeed");
        assert_eq!(again, SIZE as u64);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A failed download must not leave a truncated file behind, because a
    /// partial file looks exactly like a complete one to the user.
    #[test]
    #[ignore = "requires an FTP server on 127.0.0.1:2121"]
    fn failed_download_removes_the_partial_file() {
        let dir = std::env::temp_dir().join(format!("90ftp_ftpfail_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let dst = dir.join("never.bin");

        let mut client = connect_plain("127.0.0.1", 2121, "test", "test")
            .expect("the test FTP server must be running on 127.0.0.1:2121");

        let result = client.download_to_local_file("does-not-exist.bin", &dst.to_string_lossy());
        assert!(result.is_err(), "downloading a missing file must fail");
        assert!(!dst.exists(), "no partial file should survive a failed download");

        // And the session must remain usable.
        assert!(client.ping().is_ok(), "the session should still be healthy");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
