use std::path::Path;
use std::sync::{Arc, Mutex};
use russh::client::{self, Config, Handle, Handler};
use russh::keys::{key::PrivateKeyWithHashAlg, PublicKeyOrCertificate};
use russh_sftp::client::SftpSession;
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

        let res: Result<(SftpSession, Handle<ClientHandler>, String), String> = rt.block_on(async {
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

            let sftp = SftpSession::new(channel.into_stream()).await
                .map_err(|e| format!("Failed to initialize SFTP protocol: {}", e))?;

            let default_dir = match sftp.canonicalize(".").await {
                Ok(p) => p.replace('\\', "/"),
                Err(_) => "/".to_string(),
            };

            Ok((sftp, session, default_dir))
        });

        match res {
            Ok((sftp, session, default_dir)) => Ok(Self {
                rt,
                sftp,
                session,
                current_dir: default_dir,
                host: host.to_string(),
                username: user.to_string(),
            }),
            Err(e) => Err(e),
        }
    }

    pub fn list(&self, dir_path: Option<&str>) -> Result<Vec<FileItem>, String> {
        let target = dir_path.unwrap_or(&self.current_dir);
        let target_s = target.to_string();

        self.rt.block_on(async {
            let mut read_dir = self.sftp.read_dir(&target_s).await
                .map_err(|e| format!("Failed to read SFTP directory {}: {}", target_s, e))?;

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
        })
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
            let data = self.sftp.read(&file_s).await
                .map_err(|e| format!("Failed to read remote file {}: {}", file_s, e))?;
            Ok(data)
        })
    }

    pub fn upload_file(&self, remote_file: &str, data: &[u8]) -> Result<(), String> {
        let file_s = remote_file.to_string();
        let bytes = data.to_vec();
        self.rt.block_on(async {
            self.sftp.write(&file_s, &bytes).await
                .map_err(|e| format!("Failed to write remote file data: {}", e))?;
            Ok(())
        })
    }

    pub fn make_dir(&self, dir_path: &str) -> Result<(), String> {
        let dir_s = dir_path.to_string();
        self.rt.block_on(async {
            self.sftp.create_dir(&dir_s).await
                .map_err(|e| format!("Failed to create folder: {}", e))?;
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

    pub fn exec_command(&self, cmd: &str) -> Result<String, String> {
        let cmd_s = cmd.to_string();
        self.rt.block_on(async {
            let mut channel = self.session.channel_open_session().await
                .map_err(|e| format!("Failed to open SSH channel: {}", e))?;

            channel.exec(true, cmd_s).await
                .map_err(|e| format!("Failed to execute command: {}", e))?;

            let mut stdout = Vec::new();
            let mut stderr = Vec::new();

            let run_res = tokio::time::timeout(tokio::time::Duration::from_secs(25), async {
                while let Some(msg) = channel.wait().await {
                    match msg {
                        russh::ChannelMsg::Data { ref data } => {
                            stdout.extend_from_slice(data);
                        }
                        russh::ChannelMsg::ExtendedData { ref data, ext: _ } => {
                            stderr.extend_from_slice(data);
                        }
                        russh::ChannelMsg::Eof | russh::ChannelMsg::Close => {
                            break;
                        }
                        _ => {}
                    }
                }
            }).await;

            if run_res.is_err() {
                let _ = channel.close().await;
                stderr.extend_from_slice(b"\n[Execution timed out after 25s]");
            }

            let out_str = String::from_utf8_lossy(&stdout).to_string();
            let err_str = String::from_utf8_lossy(&stderr).to_string();
            if !err_str.is_empty() && out_str.is_empty() {
                Ok(err_str)
            } else if !err_str.is_empty() {
                Ok(format!("{}{}", out_str, err_str))
            } else {
                Ok(out_str)
            }
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
