use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use crate::ftp_client::{self, FtpBackend};
use crate::models::FileItem;
use crate::sftp_client::SftpClient;

#[allow(dead_code)]
pub enum RemoteClient {
    Ftp(Box<dyn FtpBackend>),
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
            "ftp" => {
                let actual_port = if port == 0 { 21 } else { port };
                let client = ftp_client::connect_plain(host, actual_port, user, pass)?;
                Ok(RemoteClient::Ftp(client))
            }
            "ftps" | "ftpes" => {
                // Explicit FTPS (AUTH TLS). Never falls back to cleartext: if the
                // handshake fails the connection fails loudly instead of silently
                // sending the password over an unencrypted channel.
                let actual_port = if port == 0 { 21 } else { port };
                let client = ftp_client::connect_secure(host, actual_port, user, pass)?;
                Ok(RemoteClient::Ftp(client))
            }
            "ftpsi" | "ftps-implicit" | "ftps_implicit" | "ftps-implicit-990" => {
                // Implicit FTPS: TLS starts on the very first byte, so the
                // default port is 990 and the server must expect it.
                let actual_port = if port == 0 { 990 } else { port };
                let client = ftp_client::connect_secure_implicit(host, actual_port, user, pass)?;
                Ok(RemoteClient::Ftp(client))
            }
            "sftp" | "ssh" | "scp" => {
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
            RemoteClient::Ftp(c) => c.current_dir().to_string(),
            RemoteClient::Sftp(c) => c.current_dir.clone(),
        }
    }

    /// True when the control channel is encrypted (SFTP, or FTPS after AUTH TLS).
    pub fn is_encrypted(&self) -> bool {
        match self {
            RemoteClient::Ftp(c) => c.is_secure(),
            RemoteClient::Sftp(_) => true,
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

    pub fn download_to_local_file(&mut self, remote_file: &str, local_path: &str) -> Result<u64, String> {
        match self {
            RemoteClient::Ftp(c) => c.download_to_local_file(remote_file, local_path),
            RemoteClient::Sftp(c) => c.download_to_local_file(remote_file, local_path),
        }
    }

    /// Steerable download, for the transfer queue.
    pub fn download_to_local_file_ctl(
        &mut self,
        remote_file: &str,
        local_path: &str,
        control: &crate::transfer_control::TransferControl,
    ) -> Result<u64, String> {
        match self {
            RemoteClient::Ftp(c) => c.download_to_local_file_ctl(remote_file, local_path, control),
            RemoteClient::Sftp(c) => c.download_to_local_file_ctl(remote_file, local_path, control),
        }
    }

    pub fn upload_file(&mut self, remote_file: &str, data: &[u8]) -> Result<(), String> {
        match self {
            RemoteClient::Ftp(c) => c.upload_file(remote_file, data),
            RemoteClient::Sftp(c) => c.upload_file(remote_file, data),
        }
    }

    pub fn upload_from_local_file(&mut self, local_path: &str, remote_file: &str) -> Result<u64, String> {
        match self {
            RemoteClient::Ftp(c) => c.upload_from_local_file(local_path, remote_file),
            RemoteClient::Sftp(c) => c.upload_from_local_file(local_path, remote_file),
        }
    }

    /// Steerable upload, for the transfer queue.
    pub fn upload_from_local_file_ctl(
        &mut self,
        local_path: &str,
        remote_file: &str,
        control: &crate::transfer_control::TransferControl,
    ) -> Result<u64, String> {
        match self {
            RemoteClient::Ftp(c) => c.upload_from_local_file_ctl(local_path, remote_file, control),
            RemoteClient::Sftp(c) => c.upload_from_local_file_ctl(local_path, remote_file, control),
        }
    }

    pub fn stat(&mut self, path: &str) -> Result<Option<FileItem>, String> {
        match self {
            RemoteClient::Ftp(c) => c.stat(path),
            RemoteClient::Sftp(c) => c.stat(path),
        }
    }

    pub fn make_dir(&mut self, dir_name: &str) -> Result<(), String> {
        match self {
            RemoteClient::Ftp(c) => c.make_dir(dir_name),
            RemoteClient::Sftp(c) => c.make_dir(dir_name),
        }
    }

    pub fn make_dir_all(&mut self, dir_path: &str) -> Result<(), String> {
        let clean = dir_path.trim().replace('\\', "/");
        if clean.is_empty() || clean == "." || clean == "~" || clean == "/" {
            return Ok(());
        }
        let parts: Vec<&str> = clean.split('/').filter(|s| !s.is_empty() && *s != "." && *s != "~").collect();
        let mut cur = if clean.starts_with('/') { String::from("/") } else { String::new() };

        for part in parts {
            if !cur.ends_with('/') && !cur.is_empty() {
                cur.push('/');
            }
            cur.push_str(part);
            let _ = self.make_dir(&cur);
        }
        Ok(())
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

    pub fn exec_command(&self, cmd: &str) -> Result<String, String> {
        match self {
            RemoteClient::Ftp(_) => Err("Terminal command execution is not supported on standard FTP/FTPS. Please connect with SSH/SFTP.".to_string()),
            RemoteClient::Sftp(c) => c.exec_command(cmd),
        }
    }

    pub fn exec_command_clean(&self, cmd: &str) -> Result<(i32, String), String> {
        match self {
            RemoteClient::Ftp(_) => Err("Terminal command execution is not supported on standard FTP/FTPS. Please connect with SSH/SFTP.".to_string()),
            RemoteClient::Sftp(c) => c.exec_command_clean(cmd),
        }
    }

    pub fn exec_command_streaming<F>(&self, cmd: &str, cols: u32, rows: u32, on_chunk: F) -> Result<String, String>
    where
        F: FnMut(&str) + Send + 'static,
    {
        match self {
            RemoteClient::Ftp(_) => Err("Terminal command execution is not supported on standard FTP/FTPS. Please connect with SSH/SFTP.".to_string()),
            RemoteClient::Sftp(c) => c.exec_command_streaming(cmd, cols, rows, on_chunk),
        }
    }

    pub fn exec_command_stream_pty<F>(
        &self,
        cmd: &str,
        cancelled: Option<Arc<AtomicBool>>,
        on_chunk: F,
    ) -> Result<(i32, String), String>
    where
        F: FnMut(&str) + Send,
    {
        match self {
            RemoteClient::Ftp(_) => Err("Terminal command execution is not supported on standard FTP/FTPS. Please connect with SSH/SFTP.".to_string()),
            RemoteClient::Sftp(c) => c.exec_command_stream_pty(cmd, cancelled, on_chunk),
        }
    }

    pub fn resize_terminal(&self, cols: u32, rows: u32) {
        if let RemoteClient::Sftp(c) = self {
            c.resize_pty(cols, rows);
        }
    }

    pub fn send_terminal_input(&self, data: &[u8]) {
        if let RemoteClient::Sftp(c) = self {
            c.send_terminal_input(data);
        }
    }

    pub fn ping(&mut self) -> Result<(), String> {
        match self {
            RemoteClient::Ftp(c) => c.ping(),
            RemoteClient::Sftp(c) => c.ping(),
        }
    }
}
