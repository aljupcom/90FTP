use crate::ftp_client::FtpClient;
use crate::models::FileItem;
use crate::sftp_client::SftpClient;

#[allow(dead_code)]
pub enum RemoteClient {
    Ftp(FtpClient),
    Sftp(SftpClient),
}

#[allow(dead_code)]
impl RemoteClient {
    pub fn connect(
        protocol: &str,
        host: &str,
        port: u16,
        user: &str,
        pass: &str,
        auth_type: &str,
        key_path: Option<&str>,
        passphrase: Option<&str>,
        trust_host: bool,
    ) -> Result<Self, String> {
        match protocol.to_lowercase().as_str() {
            "ftp" | "ftps" => {
                let actual_port = if port == 0 { 21 } else { port };
                let client = FtpClient::connect(host, actual_port, user, pass)?;
                Ok(RemoteClient::Ftp(client))
            }
            "sftp" | "ssh" => {
                let actual_port = if port == 0 { 22 } else { port };
                let client = SftpClient::connect(
                    host, actual_port, user, auth_type, pass, key_path, passphrase, trust_host,
                )?;
                Ok(RemoteClient::Sftp(client))
            }
            other => Err(format!("Unsupported protocol: {}", other)),
        }
    }

    pub fn pwd(&self) -> String {
        match self {
            RemoteClient::Ftp(c) => c.current_dir.clone(),
            RemoteClient::Sftp(c) => c.current_dir.clone(),
        }
    }

    pub fn list(&mut self, path: Option<&str>) -> Result<Vec<FileItem>, String> {
        match self {
            RemoteClient::Ftp(c) => c.list(path),
            RemoteClient::Sftp(c) => c.list(path),
        }
    }

    pub fn change_dir(&mut self, path: &str) -> Result<String, String> {
        match self {
            RemoteClient::Ftp(c) => c.change_dir(path),
            RemoteClient::Sftp(c) => c.change_dir(path),
        }
    }

    pub fn download_file(&mut self, remote_file: &str) -> Result<Vec<u8>, String> {
        match self {
            RemoteClient::Ftp(c) => c.download_file(remote_file),
            RemoteClient::Sftp(c) => c.download_file(remote_file),
        }
    }

    pub fn upload_file(&mut self, remote_file: &str, data: &[u8]) -> Result<(), String> {
        match self {
            RemoteClient::Ftp(c) => c.upload_file(remote_file, data),
            RemoteClient::Sftp(c) => c.upload_file(remote_file, data),
        }
    }

    pub fn make_dir(&mut self, dir_name: &str) -> Result<(), String> {
        match self {
            RemoteClient::Ftp(c) => c.make_dir(dir_name),
            RemoteClient::Sftp(c) => c.make_dir(dir_name),
        }
    }

    pub fn remove_file(&mut self, file_name: &str) -> Result<(), String> {
        match self {
            RemoteClient::Ftp(c) => c.remove_file(file_name),
            RemoteClient::Sftp(c) => c.remove_file(file_name),
        }
    }

    pub fn remove_dir(&mut self, dir_name: &str) -> Result<(), String> {
        match self {
            RemoteClient::Ftp(c) => c.remove_dir(dir_name),
            RemoteClient::Sftp(c) => c.remove_dir(dir_name),
        }
    }

    pub fn rename(&mut self, old_path: &str, new_path: &str) -> Result<(), String> {
        match self {
            RemoteClient::Ftp(c) => c.rename(old_path, new_path),
            RemoteClient::Sftp(c) => c.rename(old_path, new_path),
        }
    }

    pub fn chmod(&mut self, mode: &str, path_str: &str) -> Result<(), String> {
        match self {
            RemoteClient::Ftp(c) => c.chmod(mode, path_str),
            RemoteClient::Sftp(c) => {
                let octal = u32::from_str_radix(mode.trim_start_matches("0o"), 8)
                    .unwrap_or(0o644);
                c.chmod(octal, path_str)
            }
        }
    }
}
