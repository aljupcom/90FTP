use std::path::Path;
use std::sync::{Arc, Mutex};
use russh::client::{self, Config, Handle, Handler};
use russh::keys::{key::PrivateKeyWithHashAlg, PublicKeyOrCertificate};
use russh_sftp::client::{Config as SftpConfig, SftpSession};
use chrono::{DateTime, Utc};
use tokio::runtime::Runtime;
use crate::models::FileItem;

pub struct ClientHandler {
    pub host: String,
    pub port: u16,
    pub trust_host: bool,
    pub host_key_prompt: Arc<Mutex<Option<(String, String)>>>,
}

impl Handler for ClientHandler {
    type Error = russh::Error;

    fn check_server_key(
        &mut self,
        server_public_key: &PublicKeyOrCertificate,
    ) -> impl std::future::Future<Output = Result<bool, Self::Error>> + Send {
        let (key_type, fingerprint) = match server_public_key {
            PublicKeyOrCertificate::PublicKey { key: pk, hash_alg: _ } => {
                let alg = pk.algorithm().to_string();
                let fp = pk.fingerprint(russh::keys::HashAlg::Sha256).to_string();
                (alg, fp)
            }
            _ => ("ssh-ed25519/rsa".to_string(), "unknown".to_string()),
        };

        let trusted = if crate::sessions::is_host_trusted(&self.host, self.port, &fingerprint) {
            true
        } else if self.trust_host {
            crate::sessions::trust_host(&self.host, self.port, &fingerprint);
            true
        } else {
            let mut lock = self.host_key_prompt.lock().unwrap();
            *lock = Some((key_type, fingerprint));
            false
        };

        std::future::ready(Ok(trusted))
    }
}

#[allow(dead_code)]
pub struct SftpClient {
    rt: Arc<Runtime>,
    sftp: SftpSession,
    pub session: Handle<ClientHandler>,
    pub current_dir: String,
    pub host: String,
    pub username: String,
    pub resize_tx: tokio::sync::broadcast::Sender<(u32, u32)>,
    pub input_tx: tokio::sync::broadcast::Sender<Vec<u8>>,
}

impl SftpClient {
    pub fn connect(
        host: &str,
        port: u16,
        user: &str,
        auth_type: &str,
        pass: &str,
        key_path: Option<&str>,
        passphrase: Option<&str>,
        trust_host: bool,
    ) -> Result<Self, String> {
        let rt = Runtime::new().map_err(|e| format!("Failed to create runtime: {}", e))?;
        let rt = Arc::new(rt);

        let host_s = host.to_string();
        let user_s = user.to_string();
        let pass_s = pass.to_string();
        let auth_type_s = auth_type.to_string();
        let kp_s = key_path.map(|s| s.to_string());
        let pp_s = passphrase.map(|s| s.to_string());

        let prompt_holder = Arc::new(Mutex::new(None));
        let prompt_holder_clone = prompt_holder.clone();

        let res: Result<(SftpSession, Handle<ClientHandler>, String, tokio::sync::broadcast::Sender<(u32, u32)>, tokio::sync::broadcast::Sender<Vec<u8>>), String> = rt.block_on(async {
            let config = Arc::new(Config::default());
            let handler = ClientHandler {
                host: host_s.clone(),
                port,
                trust_host,
                host_key_prompt: prompt_holder_clone,
            };

            let mut session = match client::connect(config, (host_s.as_str(), port), handler).await {
                Ok(s) => s,
                Err(e) => {
                    let lock = prompt_holder.lock().unwrap();
                    if let Some((ref kt, ref fp)) = *lock {
                        return Err(format!("HOST_KEY_PROMPT|{}|{}|{}|{}", host_s, port, kt, fp));
                    }
                    return Err(format!("Connection to {}:{} failed: {}", host_s, port, e));
                }
            };

            // Authenticate
            if auth_type_s == "key" {
                if let Some(kp) = kp_s {
                    if !kp.is_empty() {
                        let key_p = Path::new(&kp);
                        if !key_p.exists() {
                            return Err(format!("Private key file does not exist: {}", kp));
                        }

                        let pass_opt = pp_s.as_deref().filter(|s| !s.is_empty());
                        let key = russh::keys::load_secret_key(&kp, pass_opt)
                            .map_err(|e| {
                                format!(
                                    "Failed to load SSH private key: {}. Note: If your key has a passphrase, make sure to enter it in Site Manager.",
                                    e
                                )
                            })?;

                        let key_with_alg = PrivateKeyWithHashAlg::new(Arc::new(key), None);
                        let auth_res = session.authenticate_publickey(&user_s, key_with_alg).await
                            .map_err(|e| format!("SSH Key authentication error: {}", e))?;

                        if !auth_res.success() {
                            return Err(format!("SSH Key authentication failed for user {}: key was rejected by server", user_s));
                        }
                    } else {
                        return Err("SSH Key path is empty".to_string());
                    }
                } else {
                    return Err("SSH Key path not specified".to_string());
                }
            } else {
                let auth_res = session.authenticate_password(&user_s, &pass_s).await
                    .map_err(|e| format!("SSH Password authentication error: {}", e))?;

                if !auth_res.success() {
                    return Err(format!("SSH Password authentication failed for user {}", user_s));
                }
            }

            let channel = session.channel_open_session().await
                .map_err(|e| format!("Failed to open SSH session channel: {}", e))?;

            channel.request_subsystem(true, "sftp").await
                .map_err(|e| format!("Failed to request SFTP subsystem: {}", e))?;

            let mut sftp_cfg = SftpConfig::default();
            sftp_cfg.request_timeout_secs = 300; // 5-minute timeout per request for large files and batch transfers
            sftp_cfg.max_concurrent_writes = 32;
            sftp_cfg.max_concurrent_reads = 32;

            let sftp = SftpSession::new_with_config(channel.into_stream(), sftp_cfg).await
                .map_err(|e| format!("Failed to initialize SFTP protocol: {}", e))?;

            let default_dir = match sftp.canonicalize(".").await {
                Ok(p) => p.replace('\\', "/"),
                Err(_) => "/".to_string(),
            };

            let (resize_tx, _) = tokio::sync::broadcast::channel::<(u32, u32)>(32);
            let (input_tx, _) = tokio::sync::broadcast::channel::<Vec<u8>>(256);

            Ok((sftp, session, default_dir, resize_tx, input_tx))
        });

        match res {
            Ok((sftp, session, default_dir, resize_tx, input_tx)) => Ok(Self {
                rt,
                sftp,
                session,
                current_dir: default_dir,
                host: host.to_string(),
                username: user.to_string(),
                resize_tx,
                input_tx,
            }),
            Err(e) => Err(e),
        }
    }

    pub fn list(&self, dir_path: Option<&str>) -> Result<Vec<FileItem>, String> {
        let target = dir_path.unwrap_or(&self.current_dir);
        let mut target_s = target.trim().replace('\\', "/");
        if target_s.is_empty() {
            target_s = "/".to_string();
        }
        while target_s.len() > 1 && target_s.ends_with('/') {
            target_s.pop();
        }

        let sftp_res = self.rt.block_on(async {
            let read_dir_res = self.sftp.read_dir(&target_s).await;
            let mut read_dir = match read_dir_res {
                Ok(rd) => rd,
                Err(e) => {
                    let fallback_path = format!("{}/", target_s);
                    match self.sftp.read_dir(&fallback_path).await {
                        Ok(rd) => rd,
                        Err(_) => return Err(format!("Failed to read SFTP directory {}: {}", target_s, e)),
                    }
                }
            };

            let mut items = Vec::new();

            while let Some(entry) = read_dir.next() {
                let name = entry.file_name();
                if name == "." || name == ".." || name.is_empty() {
                    continue;
                }

                let meta = entry.metadata();
                let is_dir = meta.is_dir();
                let size = meta.len();
                let is_symlink = meta.is_symlink();

                let mtime = meta.mtime.unwrap_or(0) as i64;
                let dt = DateTime::<Utc>::from_timestamp(mtime, 0).unwrap_or_default();
                let modified = dt.format("%Y-%m-%d %H:%M").to_string();

                let perms_octal = meta.permissions.unwrap_or(0) & 0o777;
                let permissions = format_octal_permissions(perms_octal, is_dir);

                let full_path = if target_s.ends_with('/') {
                    format!("{}{}", target_s, name)
                } else {
                    format!("{}/{}", target_s, name)
                };

                items.push(FileItem {
                    name,
                    path: full_path,
                    is_dir,
                    size,
                    modified,
                    permissions,
                    is_symlink,
                });
            }

            items.sort_by(|a, b| {
                if a.is_dir != b.is_dir {
                    b.is_dir.cmp(&a.is_dir)
                } else {
                    a.name.to_lowercase().cmp(&b.name.to_lowercase())
                }
            });

            Ok(items)
        });

        match sftp_res {
            Ok(items) => {
                if !items.is_empty() {
                    return Ok(items);
                }
                // If SFTP returned empty, verify if SSH ls sees files (e.g. permission/chroot edge case)
                if let Ok(ssh_items) = self.list_via_ssh_exec(&target_s) {
                    if !ssh_items.is_empty() {
                        return Ok(ssh_items);
                    }
                }
                Ok(items)
            }
            Err(sftp_err) => {
                crate::logger::log_warn(&format!("{}. Attempting SSH ls fallback...", sftp_err));
                match self.list_via_ssh_exec(&target_s) {
                    Ok(ssh_items) if !ssh_items.is_empty() => {
                        crate::logger::log_info(&format!("Successfully listed directory '{}' via SSH fallback ({} items)", target_s, ssh_items.len()));
                        Ok(ssh_items)
                    }
                    _ => {
                        crate::logger::log_error(&sftp_err);
                        Err(sftp_err)
                    }
                }
            }
        }
    }

    fn list_via_ssh_exec(&self, target_dir: &str) -> Result<Vec<FileItem>, String> {
        let cmd = format!("ls -la --time-style=+%Y-%m-%d\\ %H:%M \"{}\" 2>/dev/null", target_dir);
        let output = self.exec_command(&cmd)?;
        let mut items = Vec::new();

        for line in output.lines() {
            let line_trimmed = line.trim();
            if line_trimmed.is_empty() || line_trimmed.starts_with("total ") {
                continue;
            }
            if let Some(item) = crate::ftp_client::parse_ftp_list_line(line_trimmed, target_dir) {
                if item.name != "." && item.name != ".." {
                    items.push(item);
                }
            }
        }

        if items.is_empty() {
            let simple_cmd = format!("ls -la \"{}\" 2>/dev/null", target_dir);
            if let Ok(simple_out) = self.exec_command(&simple_cmd) {
                for line in simple_out.lines() {
                    let line_trimmed = line.trim();
                    if line_trimmed.is_empty() || line_trimmed.starts_with("total ") {
                        continue;
                    }
                    if let Some(item) = crate::ftp_client::parse_ftp_list_line(line_trimmed, target_dir) {
                        if item.name != "." && item.name != ".." {
                            items.push(item);
                        }
                    }
                }
            }
        }

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
        let path_s = path.to_string();
        self.rt.block_on(async {
            let resolved = self.sftp.canonicalize(&path_s).await
                .map_err(|e| format!("Directory does not exist: {}", e))?;
            let new_pwd = resolved.replace('\\', "/");
            self.current_dir = new_pwd.clone();
            Ok(new_pwd)
        })
    }

    pub fn download_file(&self, remote_file: &str) -> Result<Vec<u8>, String> {
        let file_s = remote_file.to_string();
        self.rt.block_on(async {
            use tokio::io::AsyncReadExt;
            let mut file = self.sftp.open(&file_s).await
                .map_err(|e| format!("Failed to open remote file {}: {}", file_s, e))?;
            let mut buffer = Vec::new();
            file.read_to_end(&mut buffer).await
                .map_err(|e| format!("Failed to read remote file {}: {}", file_s, e))?;
            let _ = file.close().await;
            Ok(buffer)
        })
    }

    pub fn upload_file(&self, remote_file: &str, data: &[u8]) -> Result<(), String> {
        let file_s = remote_file.to_string();
        let bytes = data.to_vec();
        self.rt.block_on(async {
            use tokio::io::AsyncWriteExt;
            let mut file = self.sftp.create(&file_s).await
                .map_err(|e| format!("Failed to create remote file {}: {}", file_s, e))?;

            // Stream in 64 KiB chunks for smooth TCP flow and reliable ACKs
            const CHUNK_SIZE: usize = 64 * 1024;
            for chunk in bytes.chunks(CHUNK_SIZE) {
                file.write_all(chunk).await
                    .map_err(|e| format!("Failed to write remote file data {}: {}", file_s, e))?;
            }
            file.flush().await
                .map_err(|e| format!("Failed to flush remote file {}: {}", file_s, e))?;
            file.close().await
                .map_err(|e| format!("Failed to close remote file {}: {}", file_s, e))?;
            Ok(())
        })
    }

    pub fn make_dir(&self, dir_path: &str) -> Result<(), String> {
        let dir_s = dir_path.to_string();
        self.rt.block_on(async {
            // If folder already exists, treat as success without error
            if let Ok(true) = self.sftp.try_exists(&dir_s).await {
                return Ok(());
            }
            self.sftp.create_dir(&dir_s).await
                .map_err(|e| format!("Failed to create folder {}: {}", dir_s, e))?;
            Ok(())
        })
    }

    pub fn remove_file(&self, file_path: &str) -> Result<(), String> {
        let path_s = file_path.to_string();
        self.rt.block_on(async {
            self.sftp.remove_file(&path_s).await
                .map_err(|e| format!("Failed to delete file: {}", e))?;
            Ok(())
        })
    }

    pub fn remove_dir(&self, dir_path: &str) -> Result<(), String> {
        let dir_s = dir_path.to_string();
        self.rt.block_on(async {
            self.sftp.remove_dir(&dir_s).await
                .map_err(|e| format!("Failed to delete folder: {}", e))?;
            Ok(())
        })
    }

    pub fn rename(&self, old_path: &str, new_path: &str) -> Result<(), String> {
        let old_s = old_path.to_string();
        let new_s = new_path.to_string();
        self.rt.block_on(async {
            self.sftp.rename(&old_s, &new_s).await
                .map_err(|e| format!("Failed to rename: {}", e))?;
            Ok(())
        })
    }

    pub fn chmod(&self, mode_octal: u32, path_str: &str) -> Result<(), String> {
        let path_s = path_str.to_string();
        self.rt.block_on(async {
            let mut meta = self.sftp.metadata(&path_s).await
                .map_err(|e| format!("Failed to stat file: {}", e))?;
            if let Some(ref mut perms) = meta.permissions {
                *perms = (*perms & !0o777) | (mode_octal & 0o777);
            }
            self.sftp.set_metadata(&path_s, meta).await
                .map_err(|e| format!("Failed to change permissions: {}", e))?;
            Ok(())
        })
    }

    pub fn resize_pty(&self, cols: u32, rows: u32) {
        let _ = self.resize_tx.send((cols, rows));
    }

    pub fn send_terminal_input(&self, data: &[u8]) {
        let _ = self.input_tx.send(data.to_vec());
    }

    pub fn exec_command(&self, cmd: &str) -> Result<String, String> {
        self.exec_command_streaming(cmd, 100, 30, |_| {})
    }

    pub fn exec_command_streaming<F>(&self, cmd: &str, cols: u32, rows: u32, mut on_chunk: F) -> Result<String, String>
    where
        F: FnMut(&str) + Send + 'static,
    {
        let cmd_s = cmd.to_string();
        let pty_cols = if cols == 0 { 100 } else { cols.max(20).min(500) };
        let pty_rows = if rows == 0 { 30 } else { rows.max(5).min(200) };
        let mut resize_rx = self.resize_tx.subscribe();
        let mut input_rx = self.input_tx.subscribe();

        self.rt.block_on(async {
            let mut channel = self.session.channel_open_session().await
                .map_err(|e| format!("Failed to open SSH channel: {}", e))?;

            let _ = channel.request_pty(false, "xterm-256color", pty_cols, pty_rows, 0, 0, &[]).await;

            channel.exec(true, cmd_s).await
                .map_err(|e| format!("Failed to execute command: {}", e))?;

            let mut all_output = String::new();

            // Stream chunks as they arrive from remote server with a generous 10-minute idle timeout
            loop {
                tokio::select! {
                    wait_res = tokio::time::timeout(tokio::time::Duration::from_secs(600), channel.wait()) => {
                        match wait_res {
                            Ok(Some(msg)) => {
                                match msg {
                                    russh::ChannelMsg::Data { ref data } => {
                                        let text = String::from_utf8_lossy(data).to_string();
                                        on_chunk(&text);
                                        all_output.push_str(&text);
                                    }
                                    russh::ChannelMsg::ExtendedData { ref data, ext: _ } => {
                                        let text = String::from_utf8_lossy(data).to_string();
                                        on_chunk(&text);
                                        all_output.push_str(&text);
                                    }
                                    russh::ChannelMsg::ExitStatus { .. } | russh::ChannelMsg::Eof | russh::ChannelMsg::Close => {
                                        break;
                                    }
                                    _ => {}
                                }
                            }
                            _ => break,
                        }
                    }
                    recv_res = resize_rx.recv() => {
                        if let Ok((new_cols, new_rows)) = recv_res {
                            let c = new_cols.max(20).min(500);
                            let r = new_rows.max(5).min(200);
                            let _ = channel.window_change(c, r, 0, 0).await;
                        }
                    }
                    recv_input = input_rx.recv() => {
                        if let Ok(bytes) = recv_input {
                            let _ = channel.data(&bytes[..]).await;
                        }
                    }
                }
            }

            Ok(all_output)
        })
    }
}

fn format_octal_permissions(mode: u32, is_dir: bool) -> String {
    let mut s = String::new();
    s.push(if is_dir { 'd' } else { '-' });

    s.push(if mode & 0o400 != 0 { 'r' } else { '-' });
    s.push(if mode & 0o200 != 0 { 'w' } else { '-' });
    s.push(if mode & 0o100 != 0 { 'x' } else { '-' });

    s.push(if mode & 0o040 != 0 { 'r' } else { '-' });
    s.push(if mode & 0o020 != 0 { 'w' } else { '-' });
    s.push(if mode & 0o010 != 0 { 'x' } else { '-' });

    s.push(if mode & 0o004 != 0 { 'r' } else { '-' });
    s.push(if mode & 0o002 != 0 { 'w' } else { '-' });
    s.push(if mode & 0o001 != 0 { 'x' } else { '-' });

    s
}
