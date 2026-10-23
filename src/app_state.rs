use std::sync::{Arc, Mutex};
use serde_json::json;
use crate::local_fs;
use crate::models::{IpcRequest, IpcResponse, SiteProfile};
use crate::remote::RemoteClient;
use crate::sessions;

pub type ScriptCallback = Arc<dyn Fn(String) + Send + Sync>;
pub type TerminalWindowCallback = Arc<dyn Fn(serde_json::Value) + Send + Sync>;

pub struct AppState {
    pub remote: Arc<Mutex<Option<RemoteClient>>>,
    pub exec_script: Option<ScriptCallback>,
    pub open_terminal_window: Option<TerminalWindowCallback>,
    pub current_connection: Arc<Mutex<Option<serde_json::Value>>>,
    pub archive_cancelled: Arc<std::sync::atomic::AtomicBool>,
}

impl AppState {
    pub fn new(
        exec_script: Option<ScriptCallback>,
        open_terminal_window: Option<TerminalWindowCallback>,
    ) -> Self {
        Self {
            remote: Arc::new(Mutex::new(None)),
            exec_script,
            open_terminal_window,
            current_connection: Arc::new(Mutex::new(None)),
            archive_cancelled: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }
}

fn matches_exclude(name: &str, rel_path: &str, patterns: &[String]) -> bool {
    let name_lower = name.to_lowercase();
    let rel_lower = rel_path.to_lowercase().replace('\\', "/");
    for p in patterns {
        let pat = p.trim().trim_end_matches('/').to_lowercase();
        if pat.is_empty() {
            continue;
        }
        if pat.starts_with("*.") {
            let ext = &pat[1..];
            if name_lower.ends_with(ext) {
                return true;
            }
        } else if pat.contains('*') {
            let parts: Vec<&str> = pat.split('*').collect();
            if parts.len() == 2 && name_lower.starts_with(parts[0]) && name_lower.ends_with(parts[1]) {
                return true;
            }
        } else {
            if name_lower == pat {
                return true;
            }
            for seg in rel_lower.split('/') {
                if seg == pat {
                    return true;
                }
            }
        }
    }
    false
}

fn compute_local_checksum(path: &str, algo: &str) -> Result<String, String> {
    use std::io::Read;
    use sha2::{Sha256, Digest};
    let mut file = std::fs::File::open(path).map_err(|e| format!("Failed to open local file: {}", e))?;
    let mut buffer = [0u8; 65536];

    if algo.eq_ignore_ascii_case("md5") {
        let mut context = md5::Context::new();
        loop {
            let n = file.read(&mut buffer).map_err(|e| format!("Read error: {}", e))?;
            if n == 0 { break; }
            context.consume(&buffer[..n]);
        }
        let digest = context.compute();
        Ok(format!("{:x}", digest))
    } else {
        let mut hasher = Sha256::new();
        loop {
            let n = file.read(&mut buffer).map_err(|e| format!("Read error: {}", e))?;
            if n == 0 { break; }
            hasher.update(&buffer[..n]);
        }
        let result = hasher.finalize();
        Ok(result.iter().map(|b| format!("{:02x}", b)).collect::<String>())
    }
}

fn compute_remote_checksum(client: &mut RemoteClient, remote_path: &str, algo: &str) -> Result<String, String> {
    let cmd = if algo.eq_ignore_ascii_case("md5") {
        format!("md5sum \"{}\" 2>/dev/null || md5 -q \"{}\" 2>/dev/null", remote_path, remote_path)
    } else {
        format!("sha256sum \"{}\" 2>/dev/null || shasum -a 256 \"{}\" 2>/dev/null", remote_path, remote_path)
    };

    if let Ok(output) = client.exec_command(&cmd) {
        let trimmed = output.trim();
        if !trimmed.is_empty() {
            if let Some(token) = trimmed.split_whitespace().next() {
                let token_clean = token.trim();
                let expected_len = if algo.eq_ignore_ascii_case("md5") { 32 } else { 64 };
                if token_clean.len() == expected_len && token_clean.chars().all(|c| c.is_ascii_hexdigit()) {
                    return Ok(token_clean.to_lowercase());
                }
            }
        }
    }

    use sha2::{Sha256, Digest};
    let data = client.download_file(remote_path)?;
    if algo.eq_ignore_ascii_case("md5") {
        let digest = md5::compute(&data);
        Ok(format!("{:x}", digest))
    } else {
        let mut hasher = Sha256::new();
        hasher.update(&data);
        let result = hasher.finalize();
        Ok(result.iter().map(|b| format!("{:02x}", b)).collect::<String>())
    }
}

impl AppState {
    pub fn reconnect_client_locked(
        &self,
        lock: &mut Option<RemoteClient>,
        target_dir: Option<&str>,
    ) -> Result<String, String> {
        let conn_info = {
            let conn_lock = self.current_connection.lock().unwrap();
            conn_lock.clone()
        };
        let info = conn_info.ok_or_else(|| "No previous connection session available to reconnect".to_string())?;
        let protocol = info["protocol"].as_str().unwrap_or("ftp");
        let host = info["host"].as_str().unwrap_or("");
        let port = info["port"].as_u64().unwrap_or(0) as u16;
        let user = info["username"].as_str().unwrap_or("");
        let pass = info["password"].as_str().unwrap_or("");
        let auth_type = info["auth_type"].as_str().unwrap_or("password");
        let key_path = info["key_path"].as_str();
        let passphrase = info["passphrase"].as_str();
        let saved_pwd = info["pwd"].as_str().unwrap_or("");

        if host.is_empty() {
            return Err("No host stored in connection state".to_string());
        }

        crate::logger::log_info(&format!("Auto-reconnecting to {}://{}:{} as {}...", protocol, host, port, user));
        let mut client = RemoteClient::connect(
            protocol,
            host,
            port,
            user,
            pass,
            auth_type,
            key_path,
            passphrase,
            true,
        )?;

        let desired_dir = target_dir.filter(|s| !s.is_empty()).unwrap_or(saved_pwd);
        let pwd = if !desired_dir.is_empty() {
            client.change_dir(desired_dir).unwrap_or_else(|_| client.pwd())
        } else {
            client.pwd()
        };

        {
            let mut conn_lock = self.current_connection.lock().unwrap();
            if let Some(ref mut c) = *conn_lock {
                c["pwd"] = json!(pwd);
            }
        }

        *lock = Some(client);
        Ok(pwd)
    }

    pub fn update_stored_pwd(&self, new_pwd: &str) {
        let mut conn_lock = self.current_connection.lock().unwrap();
        if let Some(ref mut c) = *conn_lock {
            c["pwd"] = json!(new_pwd);
        }
    }

    pub fn with_remote_retry<T, F>(&self, mut op: F) -> Result<(T, bool), String>
    where
        F: FnMut(&mut RemoteClient) -> Result<T, String>,
    {
        let mut lock = self.remote.lock().unwrap();
        let mut reconnected = false;
        if lock.is_none() {
            self.reconnect_client_locked(&mut lock, None)?;
            reconnected = true;
        }
        let client = lock.as_mut().ok_or_else(|| "Not connected to any remote server".to_string())?;
        match op(client) {
            Ok(val) => Ok((val, reconnected)),
            Err(first_err) => {
                if !reconnected && client.ping().is_err() {
                    if self.reconnect_client_locked(&mut lock, None).is_ok() {
                        if let Some(client2) = lock.as_mut() {
                            return op(client2).map(|v| (v, true));
                        }
                    }
                }
                Err(first_err)
            }
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
                let drives_info = local_fs::list_drives_info();
                let desktop_dir = local_fs::get_desktop_dir();
                let sites = sessions::load_sites();
                let local_files = local_fs::list_dir(&initial_dir).unwrap_or_default();
                let remote_history = sessions::load_remote_history();

                IpcResponse::ok(
                    id,
                    action,
                    json!({
                        "initial_dir": initial_dir,
                        "drives": drives,
                        "drives_info": drives_info,
                        "desktop_dir": desktop_dir,
                        "sites": sites,
                        "local_files": local_files,
                        "remote_history": remote_history,
                        "app_version": format!("0.0.0.{}", env!("CARGO_PKG_VERSION").split('.').last().unwrap_or("2")),
                    }),
                )
            }

            "get_local_drives" => {
                let drives_info = local_fs::list_drives_info();
                let desktop_dir = local_fs::get_desktop_dir();
                IpcResponse::ok(id, action, json!({
                    "drives_info": drives_info,
                    "desktop_dir": desktop_dir,
                }))
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

            "local_move_batch" => {
                let is_copy = payload["copy"].as_bool().unwrap_or(false);
                let items = payload["items"].as_array().cloned().unwrap_or_default();
                let mut moves = Vec::new();
                let mut errors = Vec::new();

                fn copy_dir_recursive(src: &std::path::Path, dst: &std::path::Path) -> Result<(), String> {
                    if !dst.exists() {
                        std::fs::create_dir_all(dst).map_err(|e| format!("Failed to create dir {:?}: {}", dst, e))?;
                    }
                    let entries = std::fs::read_dir(src).map_err(|e| format!("Failed to read dir {:?}: {}", src, e))?;
                    for entry in entries.flatten() {
                        let p = entry.path();
                        let target = dst.join(entry.file_name());
                        if p.is_dir() {
                            copy_dir_recursive(&p, &target)?;
                        } else {
                            std::fs::copy(&p, &target).map_err(|e| format!("Failed to copy {:?} to {:?}: {}", p, target, e))?;
                        }
                    }
                    Ok(())
                }

                for item in items {
                    let old_p = item["old_path"].as_str().unwrap_or("").trim();
                    let new_p = item["new_path"].as_str().unwrap_or("").trim();
                    if old_p.is_empty() || new_p.is_empty() || old_p.eq_ignore_ascii_case(new_p) {
                        continue;
                    }
                    let res = if is_copy {
                        let sp = std::path::Path::new(old_p);
                        let dp = std::path::Path::new(new_p);
                        if sp.is_dir() {
                            copy_dir_recursive(sp, dp)
                        } else {
                            if let Some(parent) = dp.parent() {
                                let _ = std::fs::create_dir_all(parent);
                            }
                            std::fs::copy(sp, dp).map(|_| ()).map_err(|e| format!("Failed to copy: {}", e))
                        }
                    } else {
                        local_fs::rename_path(old_p, new_p)
                    };

                    match res {
                        Ok(_) => moves.push(json!({ "from": old_p, "to": new_p })),
                        Err(e) => errors.push(format!("{}: {}", old_p, e)),
                    }
                }

                if moves.is_empty() && !errors.is_empty() {
                    IpcResponse::err(id, action, &errors.join("; "))
                } else {
                    IpcResponse::ok(id, action, json!({
                        "status": if is_copy { "copied" } else { "moved" },
                        "moved_count": moves.len(),
                        "moves": moves,
                        "errors": errors,
                    }))
                }
            }

            "local_compress" => {
                let sources: Vec<String> = payload.get("sources")
                    .and_then(|v| v.as_array())
                    .map(|arr| arr.iter().filter_map(|x| x.as_str()).map(|s| s.to_string()).collect())
                    .unwrap_or_default();
                let dest_zip = payload.get("dest_zip").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
                let level = payload.get("level").and_then(|v| v.as_str());

                if sources.is_empty() {
                    return IpcResponse::err(id, action, "No source files or folders selected to compress");
                }
                if dest_zip.is_empty() {
                    return IpcResponse::err(id, action, "Destination zip path cannot be empty");
                }

                match crate::archive::compress_to_zip(&sources, &dest_zip, level) {
                    Ok(count) => {
                        crate::logger::log_info(&format!("Successfully compressed {} items to {:?}", count, dest_zip));
                        IpcResponse::ok(id, action, json!({
                            "count": count,
                            "dest_zip": dest_zip,
                        }))
                    }
                    Err(e) => {
                        crate::logger::log_error(&format!("Failed to compress to {:?}: {}", dest_zip, e));
                        IpcResponse::err(id, action, &e)
                    }
                }
            }

            "local_extract" => {
                let archive_path = payload.get("archive_path").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
                let dest_dir = payload.get("dest_dir").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();

                if archive_path.is_empty() {
                    return IpcResponse::err(id, action, "Archive path cannot be empty");
                }
                if dest_dir.is_empty() {
                    return IpcResponse::err(id, action, "Destination folder cannot be empty");
                }

                match crate::archive::extract_zip(&archive_path, &dest_dir) {
                    Ok(count) => {
                        crate::logger::log_info(&format!("Successfully extracted {} items from {:?} to {:?}", count, archive_path, dest_dir));
                        IpcResponse::ok(id, action, json!({
                            "count": count,
                            "dest_dir": dest_dir,
                        }))
                    }
                    Err(e) => {
                        crate::logger::log_error(&format!("Failed to extract {:?}: {}", archive_path, e));
                        IpcResponse::err(id, action, &e)
                    }
                }
            }

            "remote_compress" => {
                let sources: Vec<String> = payload.get("sources")
                    .and_then(|v| v.as_array())
                    .map(|arr| arr.iter().filter_map(|x| x.as_str()).map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect())
                    .unwrap_or_default();
                let dest_zip = payload.get("dest_zip").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
                let level = payload.get("level").and_then(|v| v.as_str());

                if sources.is_empty() {
                    return IpcResponse::err(id, action, "No remote files or folders selected to compress");
                }
                if dest_zip.is_empty() {
                    return IpcResponse::err(id, action, "Destination archive path cannot be empty");
                }

                self.archive_cancelled.store(false, std::sync::atomic::Ordering::SeqCst);
                let exec_script = self.exec_script.clone();
                let cancelled = self.archive_cancelled.clone();

                match self.with_remote_retry(|client| {
                    crate::archive::remote_compress(client, &sources, &dest_zip, level, &cancelled, &exec_script)
                }) {
                    Ok((mut val, reconnected)) => {
                        if reconnected {
                            val["reconnected"] = json!(true);
                        }
                        crate::logger::log_info(&format!("remote_compress succeeded for {:?}", dest_zip));
                        IpcResponse::ok(id, action, val)
                    }
                    Err(e) => {
                        crate::logger::log_error(&format!("remote_compress failed for {:?}: {}", dest_zip, e));
                        IpcResponse::err(id, action, &e)
                    }
                }
            }

            "remote_extract" => {
                let archive_path = payload.get("archive_path").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
                let dest_dir = payload.get("dest_dir").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
                let overwrite_mode = payload.get("overwrite_mode").and_then(|v| v.as_str()).unwrap_or("overwrite").trim().to_lowercase();

                if archive_path.is_empty() {
                    return IpcResponse::err(id, action, "Archive path cannot be empty");
                }
                if dest_dir.is_empty() {
                    return IpcResponse::err(id, action, "Destination folder cannot be empty");
                }

                self.archive_cancelled.store(false, std::sync::atomic::Ordering::SeqCst);
                let exec_script = self.exec_script.clone();
                let cancelled = self.archive_cancelled.clone();

                match self.with_remote_retry(|client| {
                    crate::archive::remote_extract(client, &archive_path, &dest_dir, &overwrite_mode, &cancelled, &exec_script)
                }) {
                    Ok((mut val, reconnected)) => {
                        if reconnected {
                            val["reconnected"] = json!(true);
                        }
                        crate::logger::log_info(&format!("remote_extract succeeded from {:?} to {:?}", archive_path, dest_dir));
                        IpcResponse::ok(id, action, val)
                    }
                    Err(e) => {
                        crate::logger::log_error(&format!("remote_extract failed for {:?}: {}", archive_path, e));
                        IpcResponse::err(id, action, &e)
                    }
                }
            }

            "cancel_archive" => {
                self.archive_cancelled.store(true, std::sync::atomic::Ordering::SeqCst);
                IpcResponse::ok(id, action, json!({ "cancelled": true }))
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

                let mut cmd = std::process::Command::new("python");
                cmd.args(&["-c", &py_script]);
                #[cfg(target_os = "windows")]
                {
                    use std::os::windows::process::CommandExt;
                    cmd.creation_flags(0x08000000);
                }
                let output = cmd.output();

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

                let mut cmd = std::process::Command::new("python");
                cmd.args(&["-c", &py_script]);
                #[cfg(target_os = "windows")]
                {
                    use std::os::windows::process::CommandExt;
                    cmd.creation_flags(0x08000000);
                }
                let _ = cmd.spawn();

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

                // Always close and clear any previous active connection before connecting to a new server
                {
                    let mut lock = self.remote.lock().unwrap();
                    *lock = None;
                    let mut conn_lock = self.current_connection.lock().unwrap();
                    *conn_lock = None;
                }

                match RemoteClient::connect(protocol, host, port, user, pass, auth_type, key_path, passphrase, trust_host) {
                    Ok(mut client) => {
                        let pwd = if !initial_remote.is_empty() {
                            client.change_dir(initial_remote).unwrap_or_else(|_| client.pwd())
                        } else {
                            client.pwd()
                        };

                        let history = sessions::add_remote_history(&pwd);
                        let (files, list_err) = match client.list(None) {
                            Ok(f) => (f, None),
                            Err(e) => {
                                crate::logger::log_error(&format!("Listing remote directory '{}' failed on connect: {}", pwd, e));
                                (Vec::new(), Some(e))
                            }
                        };
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
                            "passphrase": passphrase,
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
                                "list_error": list_err,
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
                let wipe = payload["wipe_credentials"].as_bool().unwrap_or(false);
                let mut lock = self.remote.lock().unwrap();
                *lock = None;
                if wipe {
                    let mut conn_lock = self.current_connection.lock().unwrap();
                    *conn_lock = None;
                }
                IpcResponse::ok(id, action, json!({ "connected": false }))
            }

            "reconnect" => {
                let target_path = payload["path"].as_str();
                let mut lock = self.remote.lock().unwrap();
                match self.reconnect_client_locked(&mut lock, target_path) {
                    Ok(pwd) => {
                        let history = sessions::add_remote_history(&pwd);
                        let client = lock.as_mut().unwrap();
                        let (files, list_err) = match client.list(None) {
                            Ok(f) => (f, None),
                            Err(e) => (Vec::new(), Some(e)),
                        };
                        let conn_info = self.current_connection.lock().unwrap().clone().unwrap_or_else(|| json!({}));
                        IpcResponse::ok(
                            id,
                            action,
                            json!({
                                "connected": true,
                                "reconnected": true,
                                "protocol": conn_info["protocol"].as_str().unwrap_or("sftp"),
                                "host": conn_info["host"].as_str().unwrap_or(""),
                                "pwd": pwd,
                                "files": files,
                                "list_error": list_err,
                                "remote_history": history,
                            }),
                        )
                    }
                    Err(e) => IpcResponse::err(id, action, &e),
                }
            }

            "ping_keepalive" => {
                let auto_reconnect = payload["auto_reconnect"].as_bool().unwrap_or(true);
                let mut lock = self.remote.lock().unwrap();
                if let Some(client) = lock.as_mut() {
                    match client.ping() {
                        Ok(_) => IpcResponse::ok(id, action, json!({ "alive": true, "reconnected": false })),
                        Err(ping_err) => {
                            if auto_reconnect {
                                match self.reconnect_client_locked(&mut lock, None) {
                                    Ok(pwd) => {
                                        let files = lock.as_mut().and_then(|c| c.list(None).ok()).unwrap_or_default();
                                        IpcResponse::ok(id, action, json!({
                                            "alive": true,
                                            "reconnected": true,
                                            "pwd": pwd,
                                            "files": files
                                        }))
                                    }
                                    Err(e) => {
                                        *lock = None;
                                        IpcResponse::err(id, action, &format!("Keepalive failed ({}) and reconnect failed: {}", ping_err, e))
                                    }
                                }
                            } else {
                                *lock = None;
                                IpcResponse::err(id, action, &ping_err)
                            }
                        }
                    }
                } else if auto_reconnect {
                    match self.reconnect_client_locked(&mut lock, None) {
                        Ok(pwd) => {
                            let files = lock.as_mut().and_then(|c| c.list(None).ok()).unwrap_or_default();
                            IpcResponse::ok(id, action, json!({
                                "alive": true,
                                "reconnected": true,
                                "pwd": pwd,
                                "files": files
                            }))
                        }
                        Err(e) => IpcResponse::err(id, action, &e),
                    }
                } else {
                    IpcResponse::err(id, action, "Not connected")
                }
            }

            "remote_list" => {
                let path = payload["path"].as_str();
                let mut lock = self.remote.lock().unwrap();
                let mut reconnected = false;
                if lock.is_none() {
                    if self.reconnect_client_locked(&mut lock, path).is_ok() {
                        reconnected = true;
                    }
                }
                if let Some(client) = lock.as_mut() {
                    match client.list(path) {
                        Ok(files) => {
                            let pwd = client.pwd();
                            IpcResponse::ok(id, action, json!({ "pwd": pwd, "files": files, "reconnected": reconnected }))
                        }
                        Err(first_err) => {
                            let is_dead = client.ping().is_err();
                            if is_dead && self.reconnect_client_locked(&mut lock, path).is_ok() {
                                if let Some(client2) = lock.as_mut() {
                                    match client2.list(path) {
                                        Ok(files) => {
                                            let pwd = client2.pwd();
                                            return IpcResponse::ok(id, action, json!({ "pwd": pwd, "files": files, "reconnected": true }));
                                        }
                                        Err(e2) => return IpcResponse::err(id, action, &e2),
                                    }
                                }
                            }
                            IpcResponse::err(id, action, &first_err)
                        }
                    }
                } else {
                    IpcResponse::err(id, action, "Not connected to any remote server")
                }
            }

            "remote_cd" => {
                let path = payload["path"].as_str().unwrap_or("/");
                let mut lock = self.remote.lock().unwrap();
                let mut reconnected = false;
                if lock.is_none() {
                    if self.reconnect_client_locked(&mut lock, Some(path)).is_ok() {
                        reconnected = true;
                    }
                }
                if let Some(client) = lock.as_mut() {
                    let cd_res = client.change_dir(path);
                    let final_cd = match cd_res {
                        Ok(p) => Ok(p),
                        Err(first_err) => {
                            let is_dead = client.ping().is_err();
                            if is_dead && self.reconnect_client_locked(&mut lock, Some(path)).is_ok() {
                                reconnected = true;
                                lock.as_mut().unwrap().change_dir(path)
                            } else {
                                Err(first_err)
                            }
                        }
                    };
                    match final_cd {
                        Ok(new_pwd) => {
                            self.update_stored_pwd(&new_pwd);
                            let history = sessions::add_remote_history(&new_pwd);
                            let client_ref = lock.as_mut().unwrap();
                            let (files, list_err) = match client_ref.list(None) {
                                Ok(f) => (f, None),
                                Err(e) => {
                                    crate::logger::log_error(&format!("remote_cd: listing directory '{}' failed: {}", new_pwd, e));
                                    (Vec::new(), Some(e))
                                }
                            };
                            IpcResponse::ok(id, action, json!({
                                "pwd": new_pwd,
                                "files": files,
                                "list_error": list_err,
                                "remote_history": history,
                                "reconnected": reconnected
                            }))
                        }
                        Err(e) => {
                            crate::logger::log_error(&format!("remote_cd to '{}' failed: {}", path, e));
                            IpcResponse::err(id, action, &e)
                        }
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
                match self.with_remote_retry(|client| client.download_file(remote_file)) {
                    Ok((bytes, reconnected)) => {
                        let text = String::from_utf8_lossy(&bytes).to_string();
                        IpcResponse::ok(
                            id,
                            action,
                            json!({
                                "path": remote_file,
                                "content": text,
                                "size": bytes.len(),
                                "reconnected": reconnected,
                            }),
                        )
                    }
                    Err(e) => IpcResponse::err(id, action, &e),
                }
            }

            "remote_write" => {
                let remote_file = payload["path"].as_str().unwrap_or("");
                let content = payload["content"].as_str().unwrap_or("");
                let bytes = content.as_bytes();
                match self.with_remote_retry(|client| client.upload_file(remote_file, bytes)) {
                    Ok((_, reconnected)) => IpcResponse::ok(id, action, json!({ "status": "uploaded", "size": bytes.len(), "reconnected": reconnected })),
                    Err(e) => IpcResponse::err(id, action, &e),
                }
            }

            "remote_create_dir" => {
                let dir_path = payload["path"].as_str().unwrap_or("");
                match self.with_remote_retry(|client| {
                    client.make_dir(dir_path)?;
                    let files = client.list(None).unwrap_or_else(|e| {
                        crate::logger::log_error(&format!("Listing remote directory failed after create_dir: {}", e));
                        Vec::new()
                    });
                    Ok(files)
                }) {
                    Ok((files, reconnected)) => IpcResponse::ok(id, action, json!({ "status": "created", "files": files, "reconnected": reconnected })),
                    Err(e) => IpcResponse::err(id, action, &e),
                }
            }

            "remote_create_file" => {
                let path = payload["path"].as_str().unwrap_or("");
                match self.with_remote_retry(|client| {
                    client.upload_file(path, b"")?;
                    let files = client.list(None).unwrap_or_else(|e| {
                        crate::logger::log_error(&format!("Listing remote directory failed after create_file: {}", e));
                        Vec::new()
                    });
                    Ok(files)
                }) {
                    Ok((files, reconnected)) => IpcResponse::ok(id, action, json!({ "status": "created", "files": files, "reconnected": reconnected })),
                    Err(e) => IpcResponse::err(id, action, &e),
                }
            }

            "remote_rename" => {
                let old_p = payload["old_path"].as_str().unwrap_or("");
                let new_p = payload["new_path"].as_str().unwrap_or("");
                match self.with_remote_retry(|client| {
                    client.rename(old_p, new_p)?;
                    let files = client.list(None).unwrap_or_else(|e| {
                        crate::logger::log_error(&format!("Listing remote directory failed after rename: {}", e));
                        Vec::new()
                    });
                    Ok(files)
                }) {
                    Ok((files, reconnected)) => IpcResponse::ok(id, action, json!({ "status": "renamed", "files": files, "reconnected": reconnected })),
                    Err(e) => IpcResponse::err(id, action, &e),
                }
            }

            "remote_move_batch" => {
                let is_copy = payload["copy"].as_bool().unwrap_or(false);
                let items = payload["items"].as_array().cloned().unwrap_or_default();

                match self.with_remote_retry(|client| {
                    let mut moves = Vec::new();
                    let mut errors = Vec::new();

                    for item in &items {
                        let old_p = item["old_path"].as_str().unwrap_or("").trim();
                        let new_p = item["new_path"].as_str().unwrap_or("").trim();
                        if old_p.is_empty() || new_p.is_empty() || old_p == new_p {
                            continue;
                        }

                        if let Some(idx) = new_p.rfind('/') {
                            if idx > 0 {
                                let parent = &new_p[..idx];
                                let _ = client.make_dir_all(parent);
                            }
                        }

                        let op_res = if is_copy {
                            let sq_old = format!("'{}'", old_p.replace('\'', "'\\''"));
                            let sq_new = format!("'{}'", new_p.replace('\'', "'\\''"));
                            match client.exec_command_clean(&format!("cp -rf {} {}", sq_old, sq_new)) {
                                Ok((0, _)) => Ok(()),
                                _ => {
                                    // Fallback for regular files via SFTP/FTP buffer
                                    match client.download_file(old_p) {
                                        Ok(bytes) => client.upload_file(new_p, &bytes),
                                        Err(e) => Err(e),
                                    }
                                }
                            }
                        } else {
                            client.rename(old_p, new_p)
                        };

                        match op_res {
                            Ok(_) => moves.push(json!({ "from": old_p, "to": new_p })),
                            Err(e) => errors.push(format!("{}: {}", old_p, e)),
                        }
                    }

                    if moves.is_empty() && !errors.is_empty() {
                        return Err(errors.join("; "));
                    }

                    let files = client.list(None).unwrap_or_default();
                    Ok((moves, errors, files))
                }) {
                    Ok(((moves, errors, files), reconnected)) => IpcResponse::ok(id, action, json!({
                        "status": if is_copy { "copied" } else { "moved" },
                        "moved_count": moves.len(),
                        "moves": moves,
                        "errors": errors,
                        "files": files,
                        "reconnected": reconnected,
                    })),
                    Err(e) => IpcResponse::err(id, action, &e),
                }
            }

            "remote_delete" => {
                let path = payload["path"].as_str().unwrap_or("");
                let is_dir = payload["is_dir"].as_bool().unwrap_or(false);
                match self.with_remote_retry(|client| {
                    if is_dir {
                        client.remove_dir(path)?;
                    } else {
                        client.remove_file(path)?;
                    }
                    let files = client.list(None).unwrap_or_else(|e| {
                        crate::logger::log_error(&format!("Listing remote directory failed after delete: {}", e));
                        Vec::new()
                    });
                    Ok(files)
                }) {
                    Ok((files, reconnected)) => IpcResponse::ok(id, action, json!({ "status": "deleted", "files": files, "reconnected": reconnected })),
                    Err(e) => IpcResponse::err(id, action, &e),
                }
            }

            "remote_chmod" => {
                let path = payload["path"].as_str().unwrap_or("");
                let mode = payload["mode"].as_str().unwrap_or("0644");
                match self.with_remote_retry(|client| {
                    client.chmod(mode, path)?;
                    let files = client.list(None).unwrap_or_else(|e| {
                        crate::logger::log_error(&format!("Listing remote directory failed after chmod: {}", e));
                        Vec::new()
                    });
                    Ok(files)
                }) {
                    Ok((files, reconnected)) => IpcResponse::ok(id, action, json!({ "status": "chmod_applied", "files": files, "reconnected": reconnected })),
                    Err(e) => IpcResponse::err(id, action, &e),
                }
            }

            "upload" => {
                let local_path = payload["local_path"].as_str().unwrap_or("");
                let remote_path = payload["remote_path"].as_str().unwrap_or("");
                let skip_list = payload["skip_list"].as_bool().unwrap_or(false);

                match self.with_remote_retry(|client| {
                    let bytes = client.upload_from_local_file(local_path, remote_path)?;
                    let files = if skip_list {
                        Vec::new()
                    } else {
                        client.list(None).unwrap_or_default()
                    };
                    Ok((bytes, files))
                }) {
                    Ok(((bytes, files), reconnected)) => {
                        IpcResponse::ok(
                            id,
                            action,
                            json!({
                                "status": "uploaded",
                                "bytes": bytes,
                                "files": files,
                                "reconnected": reconnected,
                            }),
                        )
                    }
                    Err(e) => {
                        crate::logger::log_error(&format!("Upload error for '{}' -> '{}': {}", local_path, remote_path, e));
                        IpcResponse::err(id, action, &e)
                    }
                }
            }

            "local_scan_dir_recursive" => {
                let dir_path = payload["dir_path"].as_str().unwrap_or("");
                let exclude_patterns: Vec<String> = payload["exclude_patterns"]
                    .as_array()
                    .map(|arr| arr.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect())
                    .unwrap_or_default();

                let base = std::path::Path::new(dir_path);
                if !base.exists() || !base.is_dir() {
                    return IpcResponse::err(id, action, "Directory does not exist");
                }

                fn walk(
                    dir: &std::path::Path,
                    base: &std::path::Path,
                    patterns: &[String],
                    out: &mut Vec<serde_json::Value>,
                    skipped_count: &mut usize,
                ) {
                    if let Ok(entries) = std::fs::read_dir(dir) {
                        for entry in entries.flatten() {
                            let p = entry.path();
                            let rel = match p.strip_prefix(base) {
                                Ok(r) => r.to_string_lossy().replace('\\', "/"),
                                Err(_) => continue,
                            };
                            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
                            if !patterns.is_empty() && matches_exclude(name, &rel, patterns) {
                                *skipped_count += 1;
                                continue;
                            }

                            let is_dir = p.is_dir();
                            let size = if is_dir { 0 } else { p.metadata().map(|m| m.len()).unwrap_or(0) };
                            out.push(serde_json::json!({
                                "rel_path": rel,
                                "abs_path": p.to_string_lossy().to_string(),
                                "is_dir": is_dir,
                                "size": size
                            }));
                            if is_dir {
                                walk(&p, base, patterns, out, skipped_count);
                            }
                        }
                    }
                }

                let mut items = Vec::new();
                let mut skipped = 0usize;
                walk(base, base, &exclude_patterns, &mut items, &mut skipped);
                IpcResponse::ok(id, action, json!({ "items": items, "skipped_count": skipped }))
            }

            "remote_scan_dir_recursive" => {
                let remote_dir = payload["remote_dir"].as_str().unwrap_or("/");
                let exclude_patterns: Vec<String> = payload["exclude_patterns"]
                    .as_array()
                    .map(|arr| arr.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect())
                    .unwrap_or_default();

                let mut lock = self.remote.lock().unwrap();
                if let Some(client) = lock.as_mut() {
                    fn walk_remote(
                        client: &mut RemoteClient,
                        current: &str,
                        base: &str,
                        patterns: &[String],
                        out: &mut Vec<serde_json::Value>,
                        skipped_count: &mut usize,
                    ) -> Result<(), String> {
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

                            if !patterns.is_empty() && matches_exclude(&item.name, &rel_path, patterns) {
                                *skipped_count += 1;
                                continue;
                            }

                            out.push(serde_json::json!({
                                "rel_path": rel_path,
                                "remote_path": item_path,
                                "is_dir": item.is_dir,
                                "size": item.size
                            }));

                            if item.is_dir {
                                let _ = walk_remote(client, &item_path, base, patterns, out, skipped_count);
                            }
                        }
                        Ok(())
                    }

                    let mut items = Vec::new();
                    let mut skipped = 0usize;
                    let trimmed_base = remote_dir.trim_end_matches('/');
                    match walk_remote(client, remote_dir, trimmed_base, &exclude_patterns, &mut items, &mut skipped) {
                        Ok(_) => IpcResponse::ok(id, action, json!({ "items": items, "skipped_count": skipped })),
                        Err(e) => IpcResponse::err(id, action, &e),
                    }
                } else {
                    IpcResponse::err(id, action, "Not connected")
                }
            }

            "download" => {
                let remote_path = payload["remote_path"].as_str().unwrap_or("");
                let local_path = payload["local_path"].as_str().unwrap_or("");

                match self.with_remote_retry(|client| client.download_to_local_file(remote_path, local_path)) {
                    Ok((bytes, reconnected)) => {
                        IpcResponse::ok(
                            id,
                            action,
                            json!({
                                "status": "downloaded",
                                "bytes": bytes,
                                "local_path": local_path,
                                "reconnected": reconnected,
                            }),
                        )
                    }
                    Err(e) => {
                        crate::logger::log_error(&format!("Download error for '{}' -> '{}': {}", remote_path, local_path, e));
                        IpcResponse::err(id, action, &e)
                    }
                }
            }

            "local_stat" => {
                let path = payload["path"].as_str().unwrap_or("");
                let p = std::path::Path::new(path);
                if !p.exists() {
                    IpcResponse::ok(id, action, json!({ "exists": false }))
                } else {
                    let is_dir = p.is_dir();
                    let size = if is_dir { 0 } else { p.metadata().map(|m| m.len()).unwrap_or(0) };
                    let modified = p.metadata()
                        .and_then(|m| m.modified())
                        .map(|t| {
                            let dt: chrono::DateTime<chrono::Utc> = t.into();
                            dt.format("%Y-%m-%d %H:%M").to_string()
                        })
                        .unwrap_or_else(|_| "-".to_string());
                    let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("").to_string();
                    IpcResponse::ok(id, action, json!({
                        "exists": true,
                        "name": name,
                        "path": path,
                        "is_dir": is_dir,
                        "size": size,
                        "modified": modified
                    }))
                }
            }

            "remote_stat" => {
                let path = payload["path"].as_str().unwrap_or("");
                match self.with_remote_retry(|client| client.stat(path)) {
                    Ok((Some(item), reconnected)) => IpcResponse::ok(id, action, json!({
                        "exists": true,
                        "item": item,
                        "reconnected": reconnected
                    })),
                    Ok((None, reconnected)) => IpcResponse::ok(id, action, json!({ "exists": false, "reconnected": reconnected })),
                    Err(e) => IpcResponse::err(id, action, &e),
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

            "terminal_init" => {
                let remote_lock = self.remote.lock().unwrap();
                let is_connected = remote_lock.is_some();
                let cwd = if let Some(ref r) = *remote_lock {
                    r.pwd()
                } else {
                    crate::local_fs::get_initial_dir()
                };

                let conn_lock = self.current_connection.lock().unwrap();
                let (host, user, port) = match conn_lock.as_ref() {
                    Some(c) => (
                        c["host"].as_str().unwrap_or("").to_string(),
                        c["username"].as_str().unwrap_or("").to_string(),
                        c["port"].as_u64().unwrap_or(22),
                    ),
                    None => ("localhost".to_string(), "local".to_string(), 0),
                };

                IpcResponse::ok(id, action, json!({
                    "connected": is_connected,
                    "mode": if is_connected { "remote" } else { "local" },
                    "host": host,
                    "username": user,
                    "port": port,
                    "cwd": cwd,
                }))
            }

            "terminal_exec" => {
                let cmd_raw = payload["command"].as_str().unwrap_or("").trim().to_string();
                let cwd = payload["cwd"].as_str().unwrap_or("").trim().to_string();
                let req_mode = payload.get("mode").and_then(|v| v.as_str()).unwrap_or("auto").to_string();
                let session_id = payload.get("session_id").and_then(|v| v.as_str()).unwrap_or("sess_1").to_string();
                let cols = payload.get("cols").and_then(|v| v.as_u64()).unwrap_or(100) as u32;
                let rows = payload.get("rows").and_then(|v| v.as_u64()).unwrap_or(30) as u32;

                if cmd_raw.is_empty() {
                    return IpcResponse::ok(id, action, json!({
                        "output": "",
                        "cwd": cwd,
                        "mode": req_mode,
                    }));
                }

                if cmd_raw == "clear" {
                    return IpcResponse::ok(id, action, json!({
                        "clear": true,
                        "output": "",
                        "cwd": cwd,
                    }));
                }

                let is_remote = {
                    let lock = self.remote.lock().unwrap();
                    (req_mode == "remote" || req_mode == "auto") && lock.is_some()
                };

                let exec_cb = self.exec_script.clone();
                let sess_id = session_id.clone();

                if is_remote {
                    let remote_arc = self.remote.clone();
                    std::thread::spawn(move || {
                        let remote_lock = remote_arc.lock().unwrap();
                        if let Some(ref remote) = *remote_lock {
                            let safe_cwd = if cwd.is_empty() { "~" } else { &cwd };
                            let wrapped_cmd = format!(
                                "cd \"{}\" 2>/dev/null; {}; printf \"\\n__90FTP_PWD__%s\\n\" \"$(pwd)\"",
                                safe_cwd, cmd_raw
                            );

                            let cb_clone = exec_cb.clone();
                            let s_id = sess_id.clone();
                            let on_chunk = move |chunk: &str| {
                                if let Some(ref cb) = cb_clone {
                                    let json_str = serde_json::to_string(chunk).unwrap_or_default();
                                    cb(format!("if(window.onTerminalStream)window.onTerminalStream('{}', {});", s_id, json_str));
                                }
                            };

                            match remote.exec_command_streaming(&wrapped_cmd, cols, rows, on_chunk) {
                                Ok(all_out) => {
                                    let mut new_cwd = cwd.clone();
                                    if let Some(pos) = all_out.rfind("__90FTP_PWD__") {
                                        let post = &all_out[pos + "__90FTP_PWD__".len()..];
                                        let detected = post.lines().next().unwrap_or("").trim();
                                        if !detected.is_empty() {
                                            new_cwd = detected.to_string();
                                        }
                                    }
                                    if let Some(ref cb) = exec_cb {
                                        let json_cwd = serde_json::to_string(&new_cwd).unwrap_or_default();
                                        cb(format!("if(window.onTerminalDone)window.onTerminalDone('{}', {}, 0);", sess_id, json_cwd));
                                    }
                                }
                                Err(e) => {
                                    if let Some(ref cb) = exec_cb {
                                        let err_str = format!("\x1b[31mRemote error: {}\x1b[0m\n", e);
                                        let json_err = serde_json::to_string(&err_str).unwrap_or_default();
                                        cb(format!("if(window.onTerminalStream)window.onTerminalStream('{}', {});", sess_id, json_err));
                                        let json_cwd = serde_json::to_string(&cwd).unwrap_or_default();
                                        cb(format!("if(window.onTerminalDone)window.onTerminalDone('{}', {}, 1);", sess_id, json_cwd));
                                    }
                                }
                            }
                        }
                    });

                    IpcResponse::ok(id, action, json!({
                        "started": true,
                        "session_id": session_id,
                        "mode": "remote",
                    }))
                } else {
                    // Local Terminal execution (PowerShell)
                    std::thread::spawn(move || {
                        let run_dir = if !cwd.is_empty() && std::path::Path::new(&cwd).exists() {
                            cwd.clone()
                        } else {
                            crate::local_fs::get_initial_dir()
                        };

                        let full_ps = format!(
                            "Set-Location -LiteralPath '{}'; {}; Write-Output \"`n__90FTP_PWD__$((Get-Location).Path)\"",
                            run_dir.replace("'", "''"),
                            cmd_raw
                        );

                        use std::io::Read;
                        use std::process::{Command, Stdio};

                        let mut cmd = Command::new("powershell.exe");
                        cmd.args(["-NoProfile", "-NonInteractive", "-Command", &full_ps])
                            .stdout(Stdio::piped())
                            .stderr(Stdio::piped());
                        #[cfg(target_os = "windows")]
                        {
                            use std::os::windows::process::CommandExt;
                            cmd.creation_flags(0x08000000);
                        }
                        let mut child = match cmd.spawn()
                        {
                            Ok(c) => c,
                            Err(e) => {
                                if let Some(ref cb) = exec_cb {
                                    let err_str = format!("\x1b[31mFailed to start PowerShell: {}\x1b[0m\n", e);
                                    let json_err = serde_json::to_string(&err_str).unwrap_or_default();
                                    cb(format!("if(window.onTerminalStream)window.onTerminalStream('{}', {});", sess_id, json_err));
                                    let json_cwd = serde_json::to_string(&run_dir).unwrap_or_default();
                                    cb(format!("if(window.onTerminalDone)window.onTerminalDone('{}', {}, 1);", sess_id, json_cwd));
                                }
                                return;
                            }
                        };

                        let mut all_output = String::new();
                        let mut buffer = [0u8; 4096];

                        if let Some(mut stdout) = child.stdout.take() {
                            while let Ok(n) = stdout.read(&mut buffer) {
                                if n == 0 { break; }
                                let text = String::from_utf8_lossy(&buffer[..n]).to_string();
                                all_output.push_str(&text);
                                if let Some(ref cb) = exec_cb {
                                    let json_chunk = serde_json::to_string(&text).unwrap_or_default();
                                    cb(format!("if(window.onTerminalStream)window.onTerminalStream('{}', {});", sess_id, json_chunk));
                                }
                            }
                        }

                        if let Some(mut stderr) = child.stderr.take() {
                            while let Ok(n) = stderr.read(&mut buffer) {
                                if n == 0 { break; }
                                let text = String::from_utf8_lossy(&buffer[..n]).to_string();
                                all_output.push_str(&text);
                                if let Some(ref cb) = exec_cb {
                                    let json_chunk = serde_json::to_string(&text).unwrap_or_default();
                                    cb(format!("if(window.onTerminalStream)window.onTerminalStream('{}', {});", sess_id, json_chunk));
                                }
                            }
                        }

                        let _ = child.wait();

                        let mut new_cwd = run_dir.clone();
                        if let Some(pos) = all_output.rfind("__90FTP_PWD__") {
                            let post = &all_output[pos + "__90FTP_PWD__".len()..];
                            let detected = post.lines().next().unwrap_or("").trim();
                            if !detected.is_empty() {
                                new_cwd = detected.to_string();
                            }
                        }

                        if let Some(ref cb) = exec_cb {
                            let json_cwd = serde_json::to_string(&new_cwd).unwrap_or_default();
                            cb(format!("if(window.onTerminalDone)window.onTerminalDone('{}', {}, 0);", sess_id, json_cwd));
                        }
                    });

                    IpcResponse::ok(id, action, json!({
                        "started": true,
                        "session_id": session_id,
                        "mode": "local",
                    }))
                }
            }

            "terminal_resize" => {
                let cols = payload.get("cols").and_then(|v| v.as_u64()).unwrap_or(100) as u32;
                let rows = payload.get("rows").and_then(|v| v.as_u64()).unwrap_or(30) as u32;
                let lock = self.remote.lock().unwrap();
                if let Some(ref remote) = *lock {
                    remote.resize_terminal(cols, rows);
                }
                IpcResponse::ok(id, action, json!({
                    "resized": true,
                    "cols": cols,
                    "rows": rows,
                }))
            }

            "terminal_input" => {
                let data = payload.get("data").and_then(|v| v.as_str()).unwrap_or("");
                if !data.is_empty() {
                    let lock = self.remote.lock().unwrap();
                    if let Some(ref remote) = *lock {
                        remote.send_terminal_input(data.as_bytes());
                    }
                }
                IpcResponse::ok(id, action, json!({ "sent": true }))
            }

            "open_native_terminal" => {
                let req_cwd = payload.get("cwd").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
                let req_host = payload.get("host").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
                let req_user = payload.get("user").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
                let req_mode = payload.get("mode").and_then(|v| v.as_str()).unwrap_or("auto").to_string();
                let session_id = payload.get("session_id").and_then(|v| v.as_str()).unwrap_or("native_1").to_string();

                let (actual_host, actual_user, conn_pwd, is_connected) = {
                    let conn = self.current_connection.lock().unwrap();
                    if let Some(ref c) = *conn {
                        let h = c.get("host").and_then(|v| v.as_str()).unwrap_or("").to_string();
                        let u = c.get("username").or_else(|| c.get("user")).and_then(|v| v.as_str()).unwrap_or("").to_string();
                        let p = c.get("pwd").and_then(|v| v.as_str()).unwrap_or("").to_string();
                        (h, u, p, true)
                    } else {
                        (String::new(), String::new(), String::new(), false)
                    }
                };

                let remote_pwd = {
                    let rem = self.remote.lock().unwrap();
                    if let Some(ref client) = *rem {
                        client.pwd()
                    } else {
                        String::new()
                    }
                };

                let final_host = if !actual_host.is_empty() {
                    actual_host
                } else if !req_host.is_empty() && req_host != "remote-host" {
                    req_host
                } else if is_connected {
                    "connected-server".to_string()
                } else {
                    "LocalPC".to_string()
                };

                let final_user = if !actual_user.is_empty() {
                    actual_user
                } else if !req_user.is_empty() && req_user != "root" {
                    req_user
                } else if is_connected {
                    "root".to_string()
                } else {
                    "PowerShell".to_string()
                };

                let final_mode = if is_connected && (req_mode == "remote" || req_mode == "auto") {
                    "remote".to_string()
                } else if req_mode == "local" || !is_connected {
                    "local".to_string()
                } else {
                    "remote".to_string()
                };

                let final_cwd = if !req_cwd.is_empty() && req_cwd != "~" && req_cwd != "." {
                    req_cwd
                } else if !remote_pwd.is_empty() {
                    remote_pwd
                } else if !conn_pwd.is_empty() {
                    conn_pwd
                } else if final_mode == "remote" {
                    "~".to_string()
                } else {
                    std::env::current_dir().map(|p| p.to_string_lossy().to_string()).unwrap_or_else(|_| ".".to_string())
                };

                let params = json!({
                    "cwd": final_cwd,
                    "host": final_host,
                    "user": final_user,
                    "mode": final_mode,
                    "session_id": session_id,
                });

                if let Some(ref cb) = self.open_terminal_window {
                    cb(params.clone());
                }

                IpcResponse::ok(id, action, params)
            }

            "open_remote_terminal" => {
                let remote_lock = self.remote.lock().unwrap();
                let is_connected = remote_lock.is_some();
                let pwd = if let Some(ref r) = *remote_lock {
                    payload.get("remote_path").and_then(|v| v.as_str()).unwrap_or(&r.pwd()).to_string()
                } else {
                    payload.get("remote_path").and_then(|v| v.as_str()).unwrap_or("").to_string()
                };

                let conn_lock = self.current_connection.lock().unwrap();
                let (host, user) = match conn_lock.as_ref() {
                    Some(c) => (
                        c["host"].as_str().unwrap_or("").to_string(),
                        c["username"].as_str().unwrap_or("").to_string(),
                    ),
                    None => ("localhost".to_string(), "local".to_string()),
                };

                IpcResponse::ok(id, action, json!({
                    "opened": true,
                    "embedded": true,
                    "connected": is_connected,
                    "host": host,
                    "username": user,
                    "remote_path": pwd,
                }))
            }

            "calculate_checksum" => {
                let target = payload["target"].as_str().unwrap_or("local");
                let file_path = payload["path"].as_str().unwrap_or("");
                let algorithm = payload["algorithm"].as_str().unwrap_or("sha256");

                if file_path.is_empty() {
                    return IpcResponse::err(id, action, "File path is required");
                }

                if target == "local" {
                    match compute_local_checksum(file_path, algorithm) {
                        Ok(hash) => {
                            let size = std::fs::metadata(file_path).map(|m| m.len()).unwrap_or(0);
                            let name = std::path::Path::new(file_path)
                                .file_name()
                                .and_then(|n| n.to_str())
                                .unwrap_or(file_path);
                            IpcResponse::ok(id, action, json!({
                                "target": "local",
                                "name": name,
                                "path": file_path,
                                "algorithm": algorithm.to_lowercase(),
                                "hash": hash,
                                "size": size
                            }))
                        }
                        Err(e) => IpcResponse::err(id, action, &e),
                    }
                } else {
                    let mut lock = self.remote.lock().unwrap();
                    if let Some(client) = lock.as_mut() {
                        match compute_remote_checksum(client, file_path, algorithm) {
                            Ok(hash) => {
                                let name = file_path.rsplit('/').next().unwrap_or(file_path);
                                IpcResponse::ok(id, action, json!({
                                    "target": "remote",
                                    "name": name,
                                    "path": file_path,
                                    "algorithm": algorithm.to_lowercase(),
                                    "hash": hash
                                }))
                            }
                            Err(e) => IpcResponse::err(id, action, &e),
                        }
                    } else {
                        IpcResponse::err(id, action, "Not connected")
                    }
                }
            }

            "remote_find_files" => {
                let search_path = payload["path"].as_str().unwrap_or("/").trim();
                let query = payload["query"].as_str().unwrap_or("").trim();
                let recursive = payload["recursive"].as_bool().unwrap_or(true);
                let file_type = payload["file_type"].as_str().unwrap_or("all");
                let max_results = payload["max_results"].as_u64().unwrap_or(300) as usize;

                if query.is_empty() {
                    return IpcResponse::err(id, action, "Search query cannot be empty");
                }

                let mut lock = self.remote.lock().unwrap();
                if let Some(client) = lock.as_mut() {
                    let mut results = Vec::new();
                    let query_pattern = if query.contains('*') {
                        query.to_string()
                    } else {
                        format!("*{}*", query)
                    };

                    let type_flag = match file_type {
                        "files" => "-type f ",
                        "dirs" => "-type d ",
                        _ => "",
                    };

                    let depth_flag = if recursive { "-maxdepth 8 " } else { "-maxdepth 1 " };
                    let find_cmd = format!(
                        "find \"{}\" {} {} -name \"{}\" -printf \"%y|%s|%T@|%p\\n\" 2>/dev/null | head -n {}",
                        search_path, depth_flag, type_flag, query_pattern, max_results
                    );

                    let mut parsed_via_ssh = false;
                    if let Ok(output) = client.exec_command(&find_cmd) {
                        let lines: Vec<&str> = output.lines().filter(|l| !l.trim().is_empty()).collect();
                        if !lines.is_empty() {
                            for line in lines {
                                let parts: Vec<&str> = line.splitn(4, '|').collect();
                                if parts.len() == 4 {
                                    let is_dir = parts[0] == "d";
                                    let size = parts[1].parse::<u64>().unwrap_or(0);
                                    let mtime_secs = parts[2].split('.').next().unwrap_or("0").parse::<i64>().unwrap_or(0);
                                    let full_path = parts[3].to_string();
                                    let name = full_path.rsplit('/').next().unwrap_or(&full_path).to_string();
                                    let dir_path = if let Some(idx) = full_path.rfind('/') {
                                        if idx == 0 { "/".to_string() } else { full_path[..idx].to_string() }
                                    } else {
                                        "/".to_string()
                                    };

                                    results.push(serde_json::json!({
                                        "name": name,
                                        "path": full_path,
                                        "dir": dir_path,
                                        "is_dir": is_dir,
                                        "size": size,
                                        "mtime": mtime_secs,
                                    }));
                                }
                            }
                            if !results.is_empty() {
                                parsed_via_ssh = true;
                            }
                        }
                    }

                    if !parsed_via_ssh {
                        let simple_cmd = format!(
                            "find \"{}\" {} {} -name \"{}\" 2>/dev/null | head -n {}",
                            search_path, depth_flag, type_flag, query_pattern, max_results
                        );
                        if let Ok(output) = client.exec_command(&simple_cmd) {
                            for line in output.lines().filter(|l| !l.trim().is_empty()) {
                                let full_path = line.trim().to_string();
                                let name = full_path.rsplit('/').next().unwrap_or(&full_path).to_string();
                                let dir_path = if let Some(idx) = full_path.rfind('/') {
                                    if idx == 0 { "/".to_string() } else { full_path[..idx].to_string() }
                                } else {
                                    "/".to_string()
                                };
                                results.push(serde_json::json!({
                                    "name": name,
                                    "path": full_path,
                                    "dir": dir_path,
                                    "is_dir": false,
                                    "size": 0,
                                    "mtime": 0,
                                }));
                            }
                            if !results.is_empty() {
                                parsed_via_ssh = true;
                            }
                        }
                    }

                    if !parsed_via_ssh {
                        fn walk_find(
                            client: &mut RemoteClient,
                            curr: &str,
                            q: &str,
                            ftype: &str,
                            rec: bool,
                            max: usize,
                            depth: usize,
                            out: &mut Vec<serde_json::Value>,
                        ) {
                            if depth > 6 || out.len() >= max { return; }
                            if let Ok(items) = client.list(Some(curr)) {
                                for it in items {
                                    if it.name == "." || it.name == ".." { continue; }
                                    let full = if curr.ends_with('/') {
                                        format!("{}{}", curr, it.name)
                                    } else {
                                        format!("{}/{}", curr, it.name)
                                    };
                                    let name_lower = it.name.to_lowercase();
                                    let q_lower = q.to_lowercase();
                                    let matches = if q_lower.starts_with('*') && q_lower.ends_with('*') {
                                        name_lower.contains(&q_lower[1..q_lower.len()-1])
                                    } else if q_lower.starts_with('*') {
                                        name_lower.ends_with(&q_lower[1..])
                                    } else if q_lower.ends_with('*') {
                                        name_lower.starts_with(&q_lower[..q_lower.len()-1])
                                    } else {
                                        name_lower.contains(&q_lower)
                                    };

                                    let type_ok = match ftype {
                                        "files" => !it.is_dir,
                                        "dirs" => it.is_dir,
                                        _ => true,
                                    };

                                    if matches && type_ok {
                                        out.push(serde_json::json!({
                                            "name": it.name,
                                            "path": full,
                                            "dir": curr,
                                            "is_dir": it.is_dir,
                                            "size": it.size,
                                            "mtime": it.modified.clone(),
                                        }));
                                    }

                                    if it.is_dir && rec && out.len() < max {
                                        walk_find(client, &full, q, ftype, rec, max, depth + 1, out);
                                    }
                                }
                            }
                        }
                        walk_find(client, search_path, query, file_type, recursive, max_results, 0, &mut results);
                    }

                    IpcResponse::ok(id, action, json!({
                        "results": results,
                        "count": results.len(),
                        "query": query,
                        "path": search_path
                    }))
                } else {
                    IpcResponse::err(id, action, "Not connected")
                }
            }

            "get_error_logs" => {
                let logs = crate::logger::read_recent_errors(300);
                let path = crate::logger::get_log_file_path().to_string_lossy().to_string();
                IpcResponse::ok(id, action, json!({
                    "logs": logs,
                    "path": path,
                }))
            }

            unknown => IpcResponse::err(id, action, &format!("Unknown action: {}", unknown)),
        }
    }
}
