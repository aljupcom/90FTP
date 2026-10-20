use std::sync::{Arc, Mutex};
use serde_json::json;
use crate::local_fs;
use crate::models::{IpcRequest, IpcResponse, SiteProfile};
use crate::remote::RemoteClient;
use crate::sessions;

pub type ScriptCallback = Arc<dyn Fn(String) + Send + Sync>;

pub struct AppState {
    pub remote: Arc<Mutex<Option<RemoteClient>>>,
    pub exec_script: Option<ScriptCallback>,
    pub current_connection: Arc<Mutex<Option<serde_json::Value>>>,
}

impl AppState {
    pub fn new(exec_script: Option<ScriptCallback>) -> Self {
        Self {
            remote: Arc::new(Mutex::new(None)),
            exec_script,
            current_connection: Arc::new(Mutex::new(None)),
        }
    }

    pub fn handle_request(&self, req: IpcRequest) -> IpcResponse {
        let id = &req.id;
        let action = req.action.as_str();
        let payload = req.payload;

        match action {
            "init" => {
                let initial_dir = local_fs::get_initial_dir();
                let drives = local_fs::list_drives();
                let sites = sessions::load_sites();
                let local_files = local_fs::list_dir(&initial_dir).unwrap_or_default();
                let remote_history = sessions::load_remote_history();

                IpcResponse::ok(
                    id,
                    action,
                    json!({
                        "initial_dir": initial_dir,
                        "drives": drives,
                        "sites": sites,
                        "local_files": local_files,
                        "remote_history": remote_history,
                        "app_version": format!("0.0.0.{}", env!("CARGO_PKG_VERSION").split('.').last().unwrap_or("2")),
                    }),
                )
            }

            "local_list" => {
                let path = payload["path"].as_str().unwrap_or(".");
                match local_fs::list_dir(path) {
                    Ok(files) => IpcResponse::ok(
                        id,
                        action,
                        json!({
                            "path": path,
                            "files": files,
                        }),
                    ),
                    Err(e) => IpcResponse::err(id, action, &e),
                }
            }

            "local_read" => {
                let path = payload["path"].as_str().unwrap_or("");
                match local_fs::read_file(path) {
                    Ok(content) => IpcResponse::ok(id, action, json!({ "path": path, "content": content })),
                    Err(e) => IpcResponse::err(id, action, &e),
                }
            }

            "local_write" => {
                let path = payload["path"].as_str().unwrap_or("");
                let content = payload["content"].as_str().unwrap_or("");
                match local_fs::write_file(path, content) {
                    Ok(_) => IpcResponse::ok(id, action, json!({ "status": "saved" })),
                    Err(e) => IpcResponse::err(id, action, &e),
                }
            }

            "local_create_dir" => {
                let path = payload["path"].as_str().unwrap_or("");
                match local_fs::create_directory(path) {
                    Ok(_) => IpcResponse::ok(id, action, json!({ "status": "created" })),
                    Err(e) => IpcResponse::err(id, action, &e),
                }
            }

            "local_create_file" => {
                let path = payload["path"].as_str().unwrap_or("");
                match local_fs::create_empty_file(path) {
                    Ok(_) => IpcResponse::ok(id, action, json!({ "status": "created" })),
                    Err(e) => IpcResponse::err(id, action, &e),
                }
            }

            "local_delete" => {
                let path = payload["path"].as_str().unwrap_or("");
                match local_fs::delete_path(path) {
                    Ok(_) => IpcResponse::ok(id, action, json!({ "status": "deleted" })),
                    Err(e) => IpcResponse::err(id, action, &e),
                }
            }

            "local_rename" => {
                let old_p = payload["old_path"].as_str().unwrap_or("");
                let new_p = payload["new_path"].as_str().unwrap_or("");
                match local_fs::rename_path(old_p, new_p) {
                    Ok(_) => IpcResponse::ok(id, action, json!({ "status": "renamed" })),
                    Err(e) => IpcResponse::err(id, action, &e),
                }
            }

            "get_local_shell_verbs" => {
                let path = payload["path"].as_str().unwrap_or("");
                if path.is_empty() || !std::path::Path::new(path).exists() {
                    return IpcResponse::err(id, action, "Path does not exist");
                }

                let py_script = format!(
                    "import win32com.client, json, os\n\
                    path = r'''{}'''\n\
                    parent = os.path.dirname(path)\n\
                    name = os.path.basename(path)\n\
                    shell = win32com.client.Dispatch('Shell.Application')\n\
                    folder = shell.NameSpace(parent)\n\
                    item = folder.ParseName(name)\n\
                    verbs = item.Verbs()\n\
                    res = []\n\
                    for i in range(verbs.Count):\n\
                        v = verbs.Item(i)\n\
                        clean = v.Name.replace('&', '').strip()\n\
                        if clean:\n\
                            res.append({{'index': i, 'name': clean}})\n\
                    print(json.dumps(res))",
                    path.replace("'''", "")
                );

                let output = std::process::Command::new("python")
                    .args(&["-c", &py_script])
                    .output();

                match output {
                    Ok(out) => {
                        let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
                        match serde_json::from_str::<serde_json::Value>(&stdout) {
                            Ok(verbs) => IpcResponse::ok(id, action, json!({ "path": path, "verbs": verbs })),
                            Err(_) => IpcResponse::err(id, action, "Failed to parse shell verbs JSON"),
                        }
                    }
                    Err(e) => IpcResponse::err(id, action, &format!("Failed to retrieve shell verbs: {}", e)),
                }
            }

            "invoke_local_shell_verb" => {
                let path = payload["path"].as_str().unwrap_or("");
                let verb_index = payload["index"].as_i64();
                let verb_name = payload["name"].as_str().unwrap_or("");

                if path.is_empty() || !std::path::Path::new(path).exists() {
                    return IpcResponse::err(id, action, "Path does not exist");
                }

                let py_script = if let Some(idx) = verb_index {
                    format!(
                        "import win32com.client, os\n\
                        path = r'''{}'''\n\
                        parent = os.path.dirname(path)\n\
                        name = os.path.basename(path)\n\
                        shell = win32com.client.Dispatch('Shell.Application')\n\
                        folder = shell.NameSpace(parent)\n\
                        item = folder.ParseName(name)\n\
                        verbs = item.Verbs()\n\
                        verbs.Item({}).DoIt()",
                        path.replace("'''", ""), idx
                    )
                } else {
                    format!(
                        "import win32com.client, os\n\
                        path = r'''{}'''\n\
                        verb_name = r'''{}'''\n\
                        parent = os.path.dirname(path)\n\
                        name = os.path.basename(path)\n\
                        shell = win32com.client.Dispatch('Shell.Application')\n\
                        folder = shell.NameSpace(parent)\n\
                        item = folder.ParseName(name)\n\
                        verbs = item.Verbs()\n\
                        for v in verbs:\n\
                            clean = v.Name.replace('&', '').strip()\n\
                            if clean.lower() == verb_name.lower():\n\
                                v.DoIt()\n\
                                break",
                        path.replace("'''", ""), verb_name.replace("'''", "")
                    )
                };

                let _ = std::process::Command::new("python")
                    .args(&["-c", &py_script])
                    .spawn();

                IpcResponse::ok(id, action, json!({ "invoked": true, "path": path }))
            }

            "open_in_file_explorer" => {
                let path = payload["path"].as_str().unwrap_or("");
                if !path.is_empty() {
                    let _ = std::process::Command::new("explorer")
                        .args(&["/select,", path])
                        .spawn();
                }
                IpcResponse::ok(id, action, json!({ "opened": true }))
            }

            "connect" => {
                let protocol = payload["protocol"].as_str().unwrap_or("ftp");
                let host = payload["host"].as_str().unwrap_or("");
                let port = payload["port"].as_u64().unwrap_or(0) as u16;
                let user = payload["username"].as_str().unwrap_or("");
                let pass = payload["password"].as_str().unwrap_or("");
                let auth_type = payload["auth_type"].as_str().unwrap_or("password");
                let key_path = payload["key_path"].as_str();
                let passphrase = payload["passphrase"].as_str();
                let initial_remote = payload["remote_dir"].as_str().unwrap_or("");
                let trust_host = payload["trust_host"].as_bool().unwrap_or(false);

                if host.is_empty() {
                    return IpcResponse::err(id, action, "Host cannot be empty");
                }

                match RemoteClient::connect(protocol, host, port, user, pass, auth_type, key_path, passphrase, trust_host) {
                    Ok(mut client) => {
                        let pwd = if !initial_remote.is_empty() {
                            client.change_dir(initial_remote).unwrap_or_else(|_| client.pwd())
                        } else {
                            client.pwd()
                        };

                        let history = sessions::add_remote_history(&pwd);
                        let files = client.list(None).unwrap_or_default();
                        let mut lock = self.remote.lock().unwrap();
                        *lock = Some(client);

                        let mut conn_lock = self.current_connection.lock().unwrap();
                        *conn_lock = Some(json!({
                            "protocol": protocol,
                            "host": host,
                            "port": port,
                            "username": user,
                            "password": pass,
                            "auth_type": auth_type,
                            "key_path": key_path,
                            "pwd": pwd
                        }));

                        IpcResponse::ok(
                            id,
                            action,
                            json!({
                                "connected": true,
                                "protocol": protocol,
                                "host": host,
                                "pwd": pwd,
                                "files": files,
                                "remote_history": history,
                            }),
                        )
                    }
                    Err(e) => {
                        if e.starts_with("HOST_KEY_PROMPT|") {
                            let parts: Vec<&str> = e.split('|').collect();
                            if parts.len() >= 5 {
                                return IpcResponse::ok(
                                    id,
                                    action,
                                    json!({
                                        "status": "host_key_prompt",
                                        "host": parts[1],
                                        "port": parts[2],
                                        "key_type": parts[3],
                                        "fingerprint": parts[4],
                                    }),
                                );
                            }
                        }
                        IpcResponse::err(id, action, &e)
                    }
                }
            }

            "trust_host_key" => {
                let host = payload["host"].as_str().unwrap_or("");
                let port = payload["port"].as_u64().unwrap_or(0) as u16;
                let fingerprint = payload["fingerprint"].as_str().unwrap_or("");
                crate::sessions::trust_host(host, port, fingerprint);
                IpcResponse::ok(id, action, json!({ "trusted": true }))
            }

            "disconnect" => {
                let mut lock = self.remote.lock().unwrap();
                *lock = None;
                let mut conn_lock = self.current_connection.lock().unwrap();
                *conn_lock = None;
                IpcResponse::ok(id, action, json!({ "connected": false }))
            }

            "remote_list" => {
                let path = payload["path"].as_str();
                let mut lock = self.remote.lock().unwrap();
                if let Some(client) = lock.as_mut() {
                    match client.list(path) {
                        Ok(files) => {
                            let pwd = client.pwd();
                            IpcResponse::ok(id, action, json!({ "pwd": pwd, "files": files }))
                        }
                        Err(e) => IpcResponse::err(id, action, &e),
                    }
                } else {
                    IpcResponse::err(id, action, "Not connected to any remote server")
                }
            }

            "remote_cd" => {
                let path = payload["path"].as_str().unwrap_or("/");
                let mut lock = self.remote.lock().unwrap();
                if let Some(client) = lock.as_mut() {
                    match client.change_dir(path) {
                        Ok(new_pwd) => {
                            let history = sessions::add_remote_history(&new_pwd);
                            let files = client.list(None).unwrap_or_default();
                            IpcResponse::ok(id, action, json!({ "pwd": new_pwd, "files": files, "remote_history": history }))
                        }
                        Err(e) => IpcResponse::err(id, action, &e),
                    }
                } else {
                    IpcResponse::err(id, action, "Not connected to any remote server")
                }
            }

            "get_remote_history" => {
                let history = sessions::load_remote_history();
                IpcResponse::ok(id, action, json!({ "history": history }))
            }

            "add_remote_history" => {
                let path = payload["path"].as_str().unwrap_or("/");
                let history = sessions::add_remote_history(path);
                IpcResponse::ok(id, action, json!({ "history": history }))
            }

            "remote_read" => {
                let remote_file = payload["path"].as_str().unwrap_or("");
                let mut lock = self.remote.lock().unwrap();
                if let Some(client) = lock.as_mut() {
                    match client.download_file(remote_file) {
                        Ok(bytes) => {
                            let text = String::from_utf8_lossy(&bytes).to_string();
                            IpcResponse::ok(
                                id,
                                action,
                                json!({
                                    "path": remote_file,
                                    "content": text,
                                    "size": bytes.len(),
                                }),
                            )
                        }
                        Err(e) => IpcResponse::err(id, action, &e),
                    }
                } else {
                    IpcResponse::err(id, action, "Not connected")
                }
            }

            "remote_write" => {
                let remote_file = payload["path"].as_str().unwrap_or("");
                let content = payload["content"].as_str().unwrap_or("");
                let bytes = content.as_bytes();
                let mut lock = self.remote.lock().unwrap();
                if let Some(client) = lock.as_mut() {
                    match client.upload_file(remote_file, bytes) {
                        Ok(_) => IpcResponse::ok(id, action, json!({ "status": "uploaded", "size": bytes.len() })),
                        Err(e) => IpcResponse::err(id, action, &e),
                    }
                } else {
                    IpcResponse::err(id, action, "Not connected")
                }
            }

            "remote_create_dir" => {
                let dir_path = payload["path"].as_str().unwrap_or("");
                let mut lock = self.remote.lock().unwrap();
                if let Some(client) = lock.as_mut() {
                    match client.make_dir(dir_path) {
                        Ok(_) => {
                            let files = client.list(None).unwrap_or_default();
                            IpcResponse::ok(id, action, json!({ "status": "created", "files": files }))
                        }
                        Err(e) => IpcResponse::err(id, action, &e),
                    }
                } else {
                    IpcResponse::err(id, action, "Not connected")
                }
            }

            "remote_create_file" => {
                let path = payload["path"].as_str().unwrap_or("");
                let mut lock = self.remote.lock().unwrap();
                if let Some(client) = lock.as_mut() {
                    match client.upload_file(path, b"") {
                        Ok(_) => {
                            let files = client.list(None).unwrap_or_default();
                            IpcResponse::ok(id, action, json!({ "status": "created", "files": files }))
                        }
                        Err(e) => IpcResponse::err(id, action, &e),
                    }
                } else {
                    IpcResponse::err(id, action, "Not connected")
                }
            }

            "remote_rename" => {
                let old_p = payload["old_path"].as_str().unwrap_or("");
                let new_p = payload["new_path"].as_str().unwrap_or("");
                let mut lock = self.remote.lock().unwrap();
                if let Some(client) = lock.as_mut() {
                    match client.rename(old_p, new_p) {
                        Ok(_) => {
                            let files = client.list(None).unwrap_or_default();
                            IpcResponse::ok(id, action, json!({ "status": "renamed", "files": files }))
                        }
                        Err(e) => IpcResponse::err(id, action, &e),
                    }
                } else {
                    IpcResponse::err(id, action, "Not connected")
                }
            }

            "remote_delete" => {
                let path = payload["path"].as_str().unwrap_or("");
                let is_dir = payload["is_dir"].as_bool().unwrap_or(false);
                let mut lock = self.remote.lock().unwrap();
                if let Some(client) = lock.as_mut() {
                    let res = if is_dir {
                        client.remove_dir(path)
                    } else {
                        client.remove_file(path)
                    };
                    match res {
                        Ok(_) => {
                            let files = client.list(None).unwrap_or_default();
                            IpcResponse::ok(id, action, json!({ "status": "deleted", "files": files }))
                        }
                        Err(e) => IpcResponse::err(id, action, &e),
                    }
                } else {
                    IpcResponse::err(id, action, "Not connected")
                }
            }

            "remote_chmod" => {
                let path = payload["path"].as_str().unwrap_or("");
                let mode = payload["mode"].as_str().unwrap_or("0644");
                let mut lock = self.remote.lock().unwrap();
                if let Some(client) = lock.as_mut() {
                    match client.chmod(mode, path) {
                        Ok(_) => {
                            let files = client.list(None).unwrap_or_default();
                            IpcResponse::ok(id, action, json!({ "status": "chmod_applied", "files": files }))
                        }
                        Err(e) => IpcResponse::err(id, action, &e),
                    }
                } else {
                    IpcResponse::err(id, action, "Not connected")
                }
            }

            "upload" => {
                let local_path = payload["local_path"].as_str().unwrap_or("");
                let remote_path = payload["remote_path"].as_str().unwrap_or("");

                let data = match std::fs::read(local_path) {
                    Ok(d) => d,
                    Err(e) => return IpcResponse::err(id, action, &format!("Failed to read local file: {}", e)),
                };

                let mut lock = self.remote.lock().unwrap();
                if let Some(client) = lock.as_mut() {
                    match client.upload_file(remote_path, &data) {
                        Ok(_) => {
                            let files = client.list(None).unwrap_or_default();
                            IpcResponse::ok(
                                id,
                                action,
                                json!({
                                    "status": "uploaded",
                                    "bytes": data.len(),
                                    "files": files,
                                }),
                            )
                        }
                        Err(e) => IpcResponse::err(id, action, &e),
                    }
                } else {
                    IpcResponse::err(id, action, "Not connected")
                }
            }

            "local_scan_dir_recursive" => {
                let dir_path = payload["dir_path"].as_str().unwrap_or("");
                let base = std::path::Path::new(dir_path);
                if !base.exists() || !base.is_dir() {
                    return IpcResponse::err(id, action, "Directory does not exist");
                }

                fn walk(dir: &std::path::Path, base: &std::path::Path, out: &mut Vec<serde_json::Value>) {
                    if let Ok(entries) = std::fs::read_dir(dir) {
                        for entry in entries.flatten() {
                            let p = entry.path();
                            let rel = match p.strip_prefix(base) {
                                Ok(r) => r.to_string_lossy().replace('\\', "/"),
                                Err(_) => continue,
                            };
                            let is_dir = p.is_dir();
                            let size = if is_dir { 0 } else { p.metadata().map(|m| m.len()).unwrap_or(0) };
                            out.push(serde_json::json!({
                                "rel_path": rel,
                                "abs_path": p.to_string_lossy().to_string(),
                                "is_dir": is_dir,
                                "size": size
                            }));
                            if is_dir {
                                walk(&p, base, out);
                            }
                        }
                    }
                }

                let mut items = Vec::new();
                walk(base, base, &mut items);
                IpcResponse::ok(id, action, json!({ "items": items }))
            }

            "remote_scan_dir_recursive" => {
                let remote_dir = payload["remote_dir"].as_str().unwrap_or("/");
                let mut lock = self.remote.lock().unwrap();
                if let Some(client) = lock.as_mut() {
                    fn walk_remote(client: &mut RemoteClient, current: &str, base: &str, out: &mut Vec<serde_json::Value>) -> Result<(), String> {
                        let list = client.list(Some(current))?;
                        for item in list {
                            if item.name == "." || item.name == ".." {
                                continue;
                            }
                            let item_path = if current.ends_with('/') {
                                format!("{}{}", current, item.name)
                            } else {
                                format!("{}/{}", current, item.name)
                            };

                            let rel_path = if item_path.starts_with(base) {
                                item_path[base.len()..].trim_start_matches('/').to_string()
                            } else {
                                item.name.clone()
                            };

                            out.push(serde_json::json!({
                                "rel_path": rel_path,
                                "remote_path": item_path,
                                "is_dir": item.is_dir,
                                "size": item.size
                            }));

                            if item.is_dir {
                                let _ = walk_remote(client, &item_path, base, out);
                            }
                        }
                        Ok(())
                    }

                    let mut items = Vec::new();
                    let trimmed_base = remote_dir.trim_end_matches('/');
                    match walk_remote(client, remote_dir, trimmed_base, &mut items) {
                        Ok(_) => IpcResponse::ok(id, action, json!({ "items": items })),
                        Err(e) => IpcResponse::err(id, action, &e),
                    }
                } else {
                    IpcResponse::err(id, action, "Not connected")
                }
            }

            "download" => {
                let remote_path = payload["remote_path"].as_str().unwrap_or("");
                let local_path = payload["local_path"].as_str().unwrap_or("");

                let mut lock = self.remote.lock().unwrap();
                if let Some(client) = lock.as_mut() {
                    match client.download_file(remote_path) {
                        Ok(data) => {
                            if let Err(e) = std::fs::write(local_path, &data) {
                                return IpcResponse::err(id, action, &format!("Failed to write local file: {}", e));
                            }
                            IpcResponse::ok(
                                id,
                                action,
                                json!({
                                    "status": "downloaded",
                                    "bytes": data.len(),
                                    "local_path": local_path,
                                }),
                            )
                        }
                        Err(e) => IpcResponse::err(id, action, &e),
                    }
                } else {
                    IpcResponse::err(id, action, "Not connected")
                }
            }

            "save_site" => {
                match serde_json::from_value::<SiteProfile>(payload) {
                    Ok(site) => match sessions::upsert_site(site) {
                        Ok(sites) => IpcResponse::ok(id, action, json!({ "sites": sites })),
                        Err(e) => IpcResponse::err(id, action, &e),
                    },
                    Err(e) => IpcResponse::err(id, action, &format!("Invalid site payload: {}", e)),
                }
            }

            "delete_site" => {
                let site_id = payload["id"].as_str().unwrap_or("");
                match sessions::delete_site(site_id) {
                    Ok(sites) => IpcResponse::ok(id, action, json!({ "sites": sites })),
                    Err(e) => IpcResponse::err(id, action, &e),
                }
            }

            "list_sites" => {
                let sites = sessions::load_sites();
                IpcResponse::ok(id, action, json!({ "sites": sites }))
            }

            "pick_ssh_key" => {
                let file = rfd::FileDialog::new()
                    .set_title("Select SSH Private Key File")
                    .add_filter("Private Keys (*.pem;*.ppk;*.key;id_*)", &["pem", "ppk", "key", "*"])
                    .pick_file();
                match file {
                    Some(path) => IpcResponse::ok(id, action, json!({ "path": path.to_string_lossy() })),
                    None => IpcResponse::ok(id, action, json!({ "path": null })),
                }
            }

            "get_storage_info" => {
                let path = sessions::get_storage_file_path();
                let dir = sessions::get_storage_dir();
                IpcResponse::ok(id, action, json!({
                    "path": path,
                    "dir": dir.to_string_lossy(),
                    "persistent": true,
                    "encrypted": true
                }))
            }

            "open_storage_folder" | "open_storage_dir" => {
                sessions::open_storage_dir();
                IpcResponse::ok(id, action, json!({ "opened": true }))
            }

            "clear_known_hosts" => {
                match sessions::clear_known_hosts() {
                    Ok(_) => IpcResponse::ok(id, action, json!({ "status": "cleared" })),
                    Err(e) => IpcResponse::err(id, action, &e),
                }
            }

            "clear_remote_history" => {
                match sessions::clear_remote_history() {
                    Ok(history) => IpcResponse::ok(id, action, json!({ "history": history })),
                    Err(e) => IpcResponse::err(id, action, &e),
                }
            }

            "start_update_download" => {
                let url = payload.get("url").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let version = payload.get("version").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let filename = payload.get("filename").and_then(|v| v.as_str()).unwrap_or("90FTP-Setup.exe").to_string();

                if url.is_empty() {
                    return IpcResponse::err(id, action, "Download URL is required");
                }

                let exec_script = self.exec_script.clone();
                std::thread::spawn(move || {
                    let mut temp_dir = std::env::temp_dir();
                    temp_dir.push("90FTP_Updates");
                    let _ = std::fs::create_dir_all(&temp_dir);
                    let target_path = temp_dir.join(&filename);

                    match ureq::get(&url).call() {
                        Ok(resp) => {
                            let total_bytes: u64 = resp
                                .headers()
                                .get("content-length")
                                .and_then(|v| v.to_str().ok())
                                .and_then(|s| s.parse().ok())
                                .unwrap_or(0);

                            let mut reader = resp.into_body().into_reader();
                            let mut file = match std::fs::File::create(&target_path) {
                                Ok(f) => f,
                                Err(e) => {
                                    if let Some(ref cb) = exec_script {
                                        cb(format!("window.onUpdateDownloadError({{ error: \"Failed to create file: {}\" }});", e));
                                    }
                                    return;
                                }
                            };

                            let mut buffer = [0u8; 64 * 1024];
                            let mut downloaded: u64 = 0;
                            let start_time = std::time::Instant::now();
                            let mut last_emit = std::time::Instant::now();

                            use std::io::{Read, Write};
                            loop {
                                match reader.read(&mut buffer) {
                                    Ok(0) => break,
                                    Ok(n) => {
                                        if let Err(e) = file.write_all(&buffer[..n]) {
                                            if let Some(ref cb) = exec_script {
                                                cb(format!("window.onUpdateDownloadError({{ error: \"Write error: {}\" }});", e));
                                            }
                                            return;
                                        }
                                        downloaded += n as u64;

                                        let elapsed = start_time.elapsed().as_secs_f64();
                                        let speed_mb = if elapsed > 0.05 {
                                            (downloaded as f64 / (1024.0 * 1024.0)) / elapsed
                                        } else {
                                            0.0
                                        };

                                        let percent = if total_bytes > 0 {
                                            ((downloaded as f64 / total_bytes as f64) * 100.0).min(100.0)
                                        } else {
                                            0.0
                                        };

                                        if last_emit.elapsed().as_millis() >= 80 {
                                            if let Some(ref cb) = exec_script {
                                                cb(format!(
                                                    "window.onUpdateDownloadProgress({{ percent: {:.1}, downloaded: {}, total: {}, speed: '{:.2} MB/s' }});",
                                                    percent, downloaded, total_bytes, speed_mb
                                                ));
                                            }
                                            last_emit = std::time::Instant::now();
                                        }
                                    }
                                    Err(e) => {
                                        if let Some(ref cb) = exec_script {
                                            cb(format!("window.onUpdateDownloadError({{ error: \"Download error: {}\" }});", e));
                                        }
                                        return;
                                    }
                                }
                            }

                            // Download completed successfully
                            let escaped_path = target_path.to_string_lossy().replace('\\', "\\\\");
                            if let Some(ref cb) = exec_script {
                                cb(format!(
                                    "window.onUpdateDownloadComplete({{ path: \"{}\", version: \"{}\", size: {} }});",
                                    escaped_path, version, downloaded
                                ));
                            }
                        }
                        Err(e) => {
                            if let Some(ref cb) = exec_script {
                                cb(format!("window.onUpdateDownloadError({{ error: \"Failed to connect to GitHub: {}\" }});", e));
                            }
                        }
                    }
                });

                IpcResponse::ok(id, action, json!({ "started": true }))
            }

            "launch_installer" => {
                let path = payload.get("path").and_then(|v| v.as_str()).unwrap_or("");
                if path.is_empty() {
                    return IpcResponse::err(id, action, "Installer path is required");
                }

                let p = std::path::PathBuf::from(path);
                if !p.exists() {
                    return IpcResponse::err(id, action, "Installer executable not found on disk");
                }

                match std::process::Command::new(&p).spawn() {
                    Ok(_) => {
                        std::thread::spawn(|| {
                            std::thread::sleep(std::time::Duration::from_millis(500));
                            std::process::exit(0);
                        });
                        IpcResponse::ok(id, action, json!({ "launched": true }))
                    }
                    Err(e) => IpcResponse::err(id, action, &format!("Failed to launch installer: {}", e)),
                }
            }

            "open_installer_folder" => {
                let path = payload.get("path").and_then(|v| v.as_str()).unwrap_or("");
                if !path.is_empty() {
                    let p = std::path::PathBuf::from(path);
                    if let Some(parent) = p.parent() {
                        let _ = std::process::Command::new("explorer").arg(parent).spawn();
                    }
                }
                IpcResponse::ok(id, action, json!({ "opened": true }))
            }

            "open_remote_terminal" => {
                let conn_lock = self.current_connection.lock().unwrap();
                let conn = match conn_lock.as_ref() {
                    Some(c) => c.clone(),
                    None => return IpcResponse::err(id, action, "No active connection"),
                };

                let protocol = conn["protocol"].as_str().unwrap_or("").to_lowercase();
                let host = conn["host"].as_str().unwrap_or("");
                let port = conn["port"].as_u64().unwrap_or(22);
                let username = conn["username"].as_str().unwrap_or("");
                let auth_type = conn["auth_type"].as_str().unwrap_or("password");
                let key_path = conn["key_path"].as_str().unwrap_or("");
                let pwd = payload.get("remote_path").and_then(|v| v.as_str()).unwrap_or(conn["pwd"].as_str().unwrap_or("~"));

                if protocol != "sftp" && protocol != "ssh" {
                    return IpcResponse::err(id, action, "Terminal shell is only available for SSH/SFTP sessions");
                }

                if host.is_empty() {
                    return IpcResponse::err(id, action, "Host is empty");
                }

                // Check if user has NetSarang Xshell installed
                let xshell_paths = [
                    r"C:\Program Files (x86)\NetSarang\Xshell 8\Xshell.exe",
                    r"C:\Program Files\NetSarang\Xshell 8\Xshell.exe",
                    r"C:\Program Files (x86)\NetSarang\Xshell 7\Xshell.exe",
                    r"C:\Program Files\NetSarang\Xshell 7\Xshell.exe",
                ];
                let mut xshell_exe = None;
                for xp in &xshell_paths {
                    if std::path::Path::new(xp).exists() {
                        xshell_exe = Some(xp.to_string());
                        break;
                    }
                }

                if let Some(ref xe) = xshell_exe {
                    // Launch via Xshell
                    // Format: xshell.exe -url ssh://user:pass@host:port
                    let conn_url = if !username.is_empty() {
                        format!("ssh://{}@{}:{}", username, host, port)
                    } else {
                        format!("ssh://{}:{}", host, port)
                    };

                    let launched = std::process::Command::new(xe)
                        .args(&["-url", &conn_url])
                        .spawn()
                        .is_ok();

                    if launched {
                        return IpcResponse::ok(id, action, json!({ "launched": true, "terminal": "xshell", "host": host }));
                    }
                }

                let mut ssh_args = Vec::new();
                if port != 22 && port != 0 {
                    ssh_args.push("-p".to_string());
                    ssh_args.push(port.to_string());
                }
                if auth_type == "key" && !key_path.is_empty() {
                    ssh_args.push("-i".to_string());
                    ssh_args.push(key_path.to_string());
                }

                let target = if !username.is_empty() {
                    format!("{}@{}", username, host)
                } else {
                    host.to_string()
                };
                ssh_args.push(target);

                // Priority: Windows Terminal (wt.exe) -> cmd.exe fallback
                let mut wt_args: Vec<String> = vec!["-w".to_string(), "0".to_string(), "nt".to_string(), "--title".to_string(), format!("90FTP SSH: {}", host), "ssh".to_string()];
                wt_args.extend(ssh_args.clone());

                let launched = std::process::Command::new("wt.exe")
                    .args(&wt_args)
                    .spawn()
                    .is_ok();

                if !launched {
                    let mut cmd_args = vec!["/c".to_string(), "start".to_string(), format!("90FTP SSH: {}", host), "ssh".to_string()];
                    cmd_args.extend(ssh_args);
                    if let Err(e) = std::process::Command::new("cmd.exe").args(&cmd_args).spawn() {
                        return IpcResponse::err(id, action, &format!("Failed to spawn terminal: {}", e));
                    }
                }

                IpcResponse::ok(id, action, json!({ "launched": true, "terminal": "wt", "host": host, "remote_path": pwd }))
            }

            unknown => IpcResponse::err(id, action, &format!("Unknown action: {}", unknown)),
        }
    }
}
