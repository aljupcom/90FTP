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
        let mut cursor = self
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
        let parent = if let Some(idx) = remote_path.rfind('/') {
            if idx == 0 { "/" } else { &remote_path[..idx] }
        } else {
            &self.current_dir
        };
        let target_name = remote_path.rsplit('/').next().unwrap_or(remote_path);
        let items = self.list(Some(parent))?;
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
        self.stream
            .rm(file_name)
            .map_err(|e| format!("Failed to delete file {}: {}", file_name, e))
    }

    pub fn remove_dir(&mut self, dir_name: &str) -> Result<(), String> {
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
}

// Parses both Unix-style and DOS/Windows-style FTP LIST output
pub(crate) fn parse_ftp_list_line(line: &str, base_path: &str) -> Option<FileItem> {
    // Unix: -rw-r--r-- 1 owner group 12345 May 22 12:00 index.html
    let parts: Vec<&str> = line.split_whitespace().collect();
    if parts.len() >= 9 {
        let perms = parts[0];
        let is_dir = perms.starts_with('d');
        let size = parts[4].parse::<u64>().unwrap_or(0);
        let date_str = format!("{} {} {}", parts[5], parts[6], parts[7]);
        let name = parts[8..].join(" ");
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
            is_symlink: perms.starts_with('l'),
        });
    }

    // DOS format: 05-22-26 12:00PM <DIR> images
    if parts.len() >= 4 {
        let date_str = format!("{} {}", parts[0], parts[1]);
        let is_dir = parts[2].eq_ignore_ascii_case("<DIR>");
        let size = if is_dir { 0 } else { parts[2].parse::<u64>().unwrap_or(0) };
        let name = parts[3..].join(" ");
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
