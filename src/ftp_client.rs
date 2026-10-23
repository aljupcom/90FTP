use std::io::Cursor;
use suppaftp::FtpStream;
use crate::models::FileItem;

#[allow(dead_code)]
pub struct FtpClient {
    stream: FtpStream,
    pub current_dir: String,
    pub host: String,
    pub username: String,
}

#[allow(dead_code)]
impl FtpClient {
    pub fn connect(host: &str, port: u16, user: &str, pass: &str) -> Result<Self, String> {
        let addr = format!("{}:{}", host, port);
        let mut stream = FtpStream::connect(&addr)
            .map_err(|e| format!("FTP Connection failed to {}: {}", addr, e))?;

        stream
            .login(user, pass)
            .map_err(|e| format!("FTP Login failed for user {}: {}", user, e))?;

        let current_dir = stream.pwd().unwrap_or_else(|_| "/".to_string());

        Ok(Self {
            stream,
            current_dir,
            host: host.to_string(),
            username: user.to_string(),
        })
    }

    pub fn list(&mut self, path: Option<&str>) -> Result<Vec<FileItem>, String> {
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

    pub fn change_dir(&mut self, path: &str) -> Result<String, String> {
        self.stream
            .cwd(path)
            .map_err(|e| format!("Failed to change directory to {}: {}", path, e))?;
        let new_pwd = self.stream.pwd().unwrap_or_else(|_| path.to_string());
        self.current_dir = new_pwd.clone();
        Ok(new_pwd)
    }

    pub fn cdup(&mut self) -> Result<String, String> {
        self.stream
            .cdup()
            .map_err(|e| format!("Failed to navigate up: {}", e))?;
        let new_pwd = self.stream.pwd().unwrap_or_else(|_| "/".to_string());
        self.current_dir = new_pwd.clone();
        Ok(new_pwd)
    }

    pub fn download_file(&mut self, remote_file: &str) -> Result<Vec<u8>, String> {
        let cursor = self
            .stream
            .retr_as_buffer(remote_file)
            .map_err(|e| format!("Failed to download {}: {}", remote_file, e))?;
        Ok(cursor.into_inner())
    }

    pub fn download_to_local_file(&mut self, remote_file: &str, local_path: &str) -> Result<u64, String> {
        if let Some(parent) = std::path::Path::new(local_path).parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let cursor = self
            .stream
            .retr_as_buffer(remote_file)
            .map_err(|e| format!("Failed to download {}: {}", remote_file, e))?;
        let bytes = cursor.into_inner();
        let len = bytes.len() as u64;
        std::fs::write(local_path, &bytes)
            .map_err(|e| format!("Failed to write local file {}: {}", local_path, e))?;
        Ok(len)
    }

    pub fn upload_file(&mut self, remote_file: &str, data: &[u8]) -> Result<(), String> {
        let mut cursor = Cursor::new(data);
        self.stream
            .put_file(remote_file, &mut cursor)
            .map_err(|e| format!("Failed to upload {}: {}", remote_file, e))?;
        Ok(())
    }

    pub fn upload_from_local_file(&mut self, local_path: &str, remote_file: &str) -> Result<u64, String> {
        let bytes = std::fs::read(local_path)
            .map_err(|e| format!("Failed to read local file {}: {}", local_path, e))?;
        let len = bytes.len() as u64;
        let mut cursor = Cursor::new(&bytes);
        self.stream
            .put_file(remote_file, &mut cursor)
            .map_err(|e| format!("Failed to upload {}: {}", remote_file, e))?;
        Ok(len)
    }

    pub fn stat(&mut self, remote_path: &str) -> Result<Option<FileItem>, String> {
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

    pub fn make_dir(&mut self, dir_name: &str) -> Result<(), String> {
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

    pub fn remove_file(&mut self, file_name: &str) -> Result<(), String> {
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

    pub fn remove_dir(&mut self, dir_name: &str) -> Result<(), String> {
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

    pub fn chmod(&mut self, mode: &str, file_name: &str) -> Result<(), String> {
        let cmd = format!("CHMOD {} {}", mode, file_name);
        self.stream
            .site(&cmd)
            .map_err(|e| format!("Failed to set permissions: {}", e))?;
        Ok(())
    }

    pub fn rename(&mut self, from: &str, to: &str) -> Result<(), String> {
        self.stream.rename(from, to).map_err(|e| format!("FTP rename failed: {}", e))
    }

    pub fn ping(&mut self) -> Result<(), String> {
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
}
