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
    pub active_tab_id: Arc<Mutex<String>>,
    pub tab_sessions: Arc<Mutex<std::collections::HashMap<String, (Option<RemoteClient>, Option<serde_json::Value>)>>>,
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
            active_tab_id: Arc::new(Mutex::new("rtab_1".to_string())),
            tab_sessions: Arc::new(Mutex::new(std::collections::HashMap::new())),
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

            "local_delete_batch" => {
                let items = payload["items"].as_array().cloned().unwrap_or_default();
                let mut deleted = Vec::new();
                let mut errors = Vec::new();
                for item in items {
                    let path = item["path"].as_str().unwrap_or_else(|| item.as_str().unwrap_or(""));
                    if path.is_empty() {
                        continue;
                    }
                    match local_fs::delete_path(path) {
                        Ok(_) => deleted.push(path.to_string()),
                        Err(e) => errors.push(format!("{}: {}", path, e)),
                    }
                }
                if deleted.is_empty() && !errors.is_empty() {
                    IpcResponse::err(id, action, &errors.join("\n"))
                } else {
                    IpcResponse::ok(id, action, json!({
                        "status": "deleted",
                        "deleted_count": deleted.len(),
                        "deleted": deleted,
                        "errors": errors,
                    }))
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

            "switch_remote_tab" => {
                let new_tab_id = payload["tab_id"].as_str().unwrap_or("rtab_1").to_string();
                let target_path = payload["path"].as_str().unwrap_or("");
                let mut active_id_lock = self.active_tab_id.lock().unwrap();
                let old_tab_id = active_id_lock.clone();
                if old_tab_id != new_tab_id {
                    let old_client = self.remote.lock().unwrap().take();
                    let old_conn = self.current_connection.lock().unwrap().take();
                    let mut sessions_lock = self.tab_sessions.lock().unwrap();
                    sessions_lock.insert(old_tab_id, (old_client, old_conn));
                    let (new_client, new_conn) = sessions_lock.remove(&new_tab_id).unwrap_or((None, None));
                    *self.remote.lock().unwrap() = new_client;
                    *self.current_connection.lock().unwrap() = new_conn;
                    *active_id_lock = new_tab_id.clone();
                }
                let mut remote_lock = self.remote.lock().unwrap();
                let is_connected = remote_lock.is_some();
                if is_connected && !target_path.is_empty() {
                    if let Some(client) = remote_lock.as_mut() {
                        let _ = client.change_dir(target_path);
                    }
                }
                let conn_info = self.current_connection.lock().unwrap().clone().unwrap_or_else(|| json!(null));
                IpcResponse::ok(
                    id,
                    action,
                    json!({
                        "tab_id": new_tab_id,
                        "connected": is_connected,
                        "connection": conn_info,
                    }),
                )
            }

            "close_remote_tab" => {
                let tab_id = payload["tab_id"].as_str().unwrap_or("");
                if !tab_id.is_empty() {
                    let active_id = self.active_tab_id.lock().unwrap().clone();
                    if active_id == tab_id {
                        *self.remote.lock().unwrap() = None;
                        *self.current_connection.lock().unwrap() = None;
                    } else {
                        self.tab_sessions.lock().unwrap().remove(tab_id);
                    }
                }
                IpcResponse::ok(id, action, json!({ "closed": true, "tab_id": tab_id }))
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
                let current_dir = payload["current_dir"].as_str().filter(|s| !s.is_empty());
                match self.with_remote_retry(|client| {
                    if is_dir {
                        client.remove_dir(path)?;
                    } else {
                        client.remove_file(path)?;
                    }
                    let files = client.list(current_dir).unwrap_or_else(|e| {
                        crate::logger::log_error(&format!("Listing remote directory failed after delete: {}", e));
                        Vec::new()
                    });
                    Ok(files)
                }) {
                    Ok((files, reconnected)) => IpcResponse::ok(id, action, json!({ "status": "deleted", "files": files, "reconnected": reconnected })),
                    Err(e) => IpcResponse::err(id, action, &e),
                }
            }

            "remote_delete_batch" => {
                let items = payload["items"].as_array().cloned().unwrap_or_default();
                let current_dir = payload["current_dir"].as_str().filter(|s| !s.is_empty()).map(|s| s.to_string());
                match self.with_remote_retry(|client| {
                    let mut deleted = Vec::new();
                    let mut errors = Vec::new();
                    for item in &items {
                        let path = item["path"].as_str().unwrap_or("");
                        let is_dir = item["is_dir"].as_bool().unwrap_or(false);
                        if path.is_empty() {
                            continue;
                        }
                        let res = if is_dir {
                            client.remove_dir(path)
                        } else {
                            client.remove_file(path)
                        };
                        match res {
                            Ok(_) => deleted.push(path.to_string()),
                            Err(e) => errors.push(format!("{}: {}", path, e)),
                        }
                    }
                    if deleted.is_empty() && !errors.is_empty() {
                        return Err(errors.join("\n"));
                    }
                    let files = client.list(current_dir.as_deref()).unwrap_or_else(|e| {
                        crate::logger::log_error(&format!("Listing remote directory failed after batch delete: {}", e));
                        Vec::new()
                    });
                    Ok((deleted, errors, files))
                }) {
                    Ok(((deleted, errors, files), reconnected)) => IpcResponse::ok(id, action, json!({
                        "status": "deleted",
                        "deleted_count": deleted.len(),
                        "deleted": deleted,
                        "errors": errors,
                        "files": files,
                        "reconnected": reconnected
                    })),
                    Err(e) => IpcResponse::err(id, action, &e),
                }
            }

            "remote_chmod" => {
                let single_path = payload["path"].as_str().unwrap_or("");
                let mode = payload["mode"].as_str().unwrap_or("0644").trim();
                let recursive = payload["recursive"].as_bool().unwrap_or(false);
                let current_dir = payload["current_dir"].as_str().filter(|s| !s.is_empty()).map(|s| s.to_string());
                let paths: Vec<String> = if let Some(arr) = payload["paths"].as_array() {
                    arr.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).filter(|s| !s.is_empty()).collect()
                } else if !single_path.is_empty() {
                    vec![single_path.to_string()]
                } else {
                    Vec::new()
                };

                if paths.is_empty() {
                    return IpcResponse::err(id, action, "No target path specified for CHMOD");
                }

                match self.with_remote_retry(|client| {
                    for p in &paths {
                        if recursive {
                            let esc_p = p.replace('\'', "'\\''");
                            let clean_mode = mode.trim_start_matches("0o");
                            let ssh_cmd = format!("chmod -R {} -- '{}' 2>/dev/null", clean_mode, esc_p);
                            if client.exec_command(&ssh_cmd).is_err() {
                                client.chmod(mode, p)?;
                            } else {
                                let _ = client.chmod(mode, p);
                            }
                        } else {
                            client.chmod(mode, p)?;
                        }
                    }
                    let files = client.list(current_dir.as_deref()).unwrap_or_else(|e| {
                        crate::logger::log_error(&format!("Listing remote directory failed after chmod: {}", e));
                        Vec::new()
                    });
                    Ok(files)
                }) {
                    Ok((files, reconnected)) => IpcResponse::ok(id, action, json!({
                        "status": "chmod_applied",
                        "count": paths.len(),
                        "files": files,
                        "reconnected": reconnected
                    })),
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

            "local_find_files" => {
                let search_path = payload["path"].as_str().unwrap_or("").trim();
                let query = payload["query"].as_str().unwrap_or("").trim();
                let recursive = payload["recursive"].as_bool().unwrap_or(true);
                let file_type = payload["file_type"].as_str().unwrap_or("all");
                let max_results = payload["max_results"].as_u64().unwrap_or(300) as usize;

                if query.is_empty() {
                    return IpcResponse::err(id, action, "Search query cannot be empty");
                }
                let root = if search_path.is_empty() {
                    crate::local_fs::get_initial_dir()
                } else {
                    search_path.to_string()
                };

                fn walk_local_find(
                    dir: &std::path::Path,
                    q_lower: &str,
                    ftype: &str,
                    rec: bool,
                    max: usize,
                    depth: usize,
                    out: &mut Vec<serde_json::Value>,
                ) {
                    if depth > 10 || out.len() >= max {
                        return;
                    }
                    let entries = match std::fs::read_dir(dir) {
                        Ok(rd) => rd,
                        Err(_) => return,
                    };
                    for entry in entries.flatten() {
                        if out.len() >= max {
                            break;
                        }
                        let path = entry.path();
                        let name = entry.file_name().to_string_lossy().to_string();
                        if name == "." || name == ".." {
                            continue;
                        }
                        let meta = match std::fs::symlink_metadata(&path) {
                            Ok(m) => m,
                            Err(_) => continue,
                        };
                        let is_dir = meta.is_dir();
                        let name_lower = name.to_lowercase();
                        let matches = if q_lower.starts_with('*') && q_lower.ends_with('*') && q_lower.len() > 2 {
                            name_lower.contains(&q_lower[1..q_lower.len() - 1])
                        } else if q_lower.starts_with('*') && q_lower.len() > 1 {
                            name_lower.ends_with(&q_lower[1..])
                        } else if q_lower.ends_with('*') && q_lower.len() > 1 {
                            name_lower.starts_with(&q_lower[..q_lower.len() - 1])
                        } else {
                            name_lower.contains(q_lower)
                        };

                        let type_ok = match ftype {
                            "files" => !is_dir,
                            "dirs" => is_dir,
                            _ => true,
                        };

                        if matches && type_ok {
                            let full_str = path.to_string_lossy().to_string();
                            let parent_str = path.parent().map(|p| p.to_string_lossy().to_string()).unwrap_or_default();
                            out.push(json!({
                                "name": name,
                                "path": full_str,
                                "dir": parent_str,
                                "is_dir": is_dir,
                                "size": if is_dir { 0 } else { meta.len() },
                            }));
                        }

                        if is_dir && !meta.file_type().is_symlink() && rec && out.len() < max {
                            walk_local_find(&path, q_lower, ftype, rec, max, depth + 1, out);
                        }
                    }
                }

                let mut results = Vec::new();
                walk_local_find(
                    std::path::Path::new(&root),
                    &query.to_lowercase(),
                    file_type,
                    recursive,
                    max_results,
                    0,
                    &mut results,
                );

                IpcResponse::ok(id, action, json!({
                    "results": results,
                    "count": results.len(),
                    "query": query,
                    "path": root,
                }))
            }

            "local_folder_size" => {
                let path_str = payload["path"].as_str().unwrap_or("").trim();
                if path_str.is_empty() {
                    return IpcResponse::err(id, action, "Folder path is required");
                }
                fn calc_dir_stats(p: &std::path::Path, depth: usize) -> (u64, u64, u64) {
                    if depth > 16 {
                        return (0, 0, 0);
                    }
                    let mut bytes = 0u64;
                    let mut files = 0u64;
                    let mut dirs = 0u64;
                    if let Ok(rd) = std::fs::read_dir(p) {
                        for entry in rd.flatten() {
                            if let Ok(meta) = entry.metadata() {
                                if meta.is_dir() {
                                    dirs += 1;
                                    let (b, f, d) = calc_dir_stats(&entry.path(), depth + 1);
                                    bytes += b;
                                    files += f;
                                    dirs += d;
                                } else {
                                    files += 1;
                                    bytes += meta.len();
                                }
                            }
                        }
                    }
                    (bytes, files, dirs)
                }
                let (bytes, files, dirs) = calc_dir_stats(std::path::Path::new(path_str), 0);
                IpcResponse::ok(id, action, json!({
                    "path": path_str,
                    "size": bytes,
                    "files": files,
                    "dirs": dirs
                }))
            }

            "remote_folder_size" => {
                let path_str = payload["path"].as_str().unwrap_or("").trim();
                if path_str.is_empty() {
                    return IpcResponse::err(id, action, "Remote folder path is required");
                }
                match self.with_remote_retry(|client| {
                    let esc = path_str.replace('\'', "'\\''");
                    let cmd = format!("du -sb -- '{}' 2>/dev/null || du -sk -- '{}' 2>/dev/null", esc, esc);
                    if let Ok(out) = client.exec_command(&cmd) {
                        if let Some(first_line) = out.lines().find(|l| !l.trim().is_empty()) {
                            if let Some(num_tok) = first_line.split_whitespace().next() {
                                if let Ok(val) = num_tok.parse::<u64>() {
                                    // If du -sb succeeded it's bytes; if du -sk fallback was used on BSD/busybox, check if du -sb was supported
                                    return Ok(val);
                                }
                            }
                        }
                    }
                    // Fallback: walk up to 4 levels via SFTP/FTP list
                    fn walk_remote_size(c: &mut RemoteClient, dir: &str, depth: usize) -> u64 {
                        if depth > 4 {
                            return 0;
                        }
                        let mut sum = 0u64;
                        if let Ok(items) = c.list(Some(dir)) {
                            for it in items {
                                if it.name == "." || it.name == ".." {
                                    continue;
                                }
                                if it.is_dir && !it.is_symlink {
                                    sum += walk_remote_size(c, &it.path, depth + 1);
                                } else {
                                    sum += it.size;
                                }
                            }
                        }
                        sum
                    }
                    Ok(walk_remote_size(client, path_str, 0))
                }) {
                    Ok((size, reconnected)) => IpcResponse::ok(id, action, json!({
                        "path": path_str,
                        "size": size,
                        "reconnected": reconnected
                    })),
                    Err(e) => IpcResponse::err(id, action, &e),
                }
            }

            "network_probe" => {
                use std::io::Read;
                use std::net::{TcpStream, ToSocketAddrs};
                use std::time::{Duration, Instant};

                let host_raw = payload["host"].as_str().unwrap_or("").trim();
                let port = payload["port"].as_u64().unwrap_or(22) as u16;
                let samples = payload["samples"].as_u64().unwrap_or(4).clamp(1, 8) as usize;

                if host_raw.is_empty() {
                    return IpcResponse::err(id, action, "Host or IP address is required");
                }
                let clean_host = host_raw
                    .trim_start_matches("sftp://")
                    .trim_start_matches("ftp://")
                    .trim_start_matches("ftps://")
                    .split('/')
                    .next()
                    .unwrap_or(host_raw)
                    .split(':')
                    .next()
                    .unwrap_or(host_raw)
                    .trim();

                let dns_start = Instant::now();
                let addr_str = format!("{}:{}", clean_host, port);
                let resolved_addrs: Vec<_> = match addr_str.to_socket_addrs() {
                    Ok(iter) => iter.collect(),
                    Err(e) => {
                        return IpcResponse::err(id, action, &format!("DNS resolution failed for '{}': {}", clean_host, e));
                    }
                };
                let dns_ms = dns_start.elapsed().as_secs_f64() * 1000.0;
                if resolved_addrs.is_empty() {
                    return IpcResponse::err(id, action, &format!("Could not resolve any IP addresses for '{}'", clean_host));
                }
                let target_addr = resolved_addrs[0];
                let ip_list: Vec<String> = resolved_addrs.iter().map(|a| a.ip().to_string()).collect();

                let mut latencies_ms = Vec::new();
                let mut banner = String::new();
                let mut reachable = false;
                let mut last_err = String::new();

                for i in 0..samples {
                    let t0 = Instant::now();
                    match TcpStream::connect_timeout(&target_addr, Duration::from_millis(3500)) {
                        Ok(mut stream) => {
                            let rtt = t0.elapsed().as_secs_f64() * 1000.0;
                            latencies_ms.push((rtt * 100.0).round() / 100.0);
                            reachable = true;
                            if i == 0 && banner.is_empty() {
                                let _ = stream.set_read_timeout(Some(Duration::from_millis(450)));
                                let mut buf = [0u8; 256];
                                if let Ok(n) = stream.read(&mut buf) {
                                    if n > 0 {
                                        banner = String::from_utf8_lossy(&buf[..n])
                                            .lines()
                                            .next()
                                            .unwrap_or("")
                                            .trim()
                                            .to_string();
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            last_err = e.to_string();
                        }
                    }
                }

                let (min_ms, max_ms, avg_ms, jitter_ms) = if !latencies_ms.is_empty() {
                    let min = latencies_ms.iter().cloned().fold(f64::INFINITY, f64::min);
                    let max = latencies_ms.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
                    let sum: f64 = latencies_ms.iter().sum();
                    let avg = sum / (latencies_ms.len() as f64);
                    let jitter = max - min;
                    (
                        (min * 100.0).round() / 100.0,
                        (max * 100.0).round() / 100.0,
                        (avg * 100.0).round() / 100.0,
                        (jitter * 100.0).round() / 100.0,
                    )
                } else {
                    (0.0, 0.0, 0.0, 0.0)
                };

                IpcResponse::ok(id, action, json!({
                    "host": clean_host,
                    "port": port,
                    "ip": target_addr.ip().to_string(),
                    "ips": ip_list,
                    "dns_ms": (dns_ms * 100.0).round() / 100.0,
                    "reachable": reachable,
                    "samples_sent": samples,
                    "samples_ok": latencies_ms.len(),
                    "latencies_ms": latencies_ms,
                    "min_ms": min_ms,
                    "max_ms": max_ms,
                    "avg_ms": avg_ms,
                    "jitter_ms": jitter_ms,
                    "banner": banner,
                    "error": last_err,
                }))
            }

            "ssh_keygen" => {
                use ssh_key::{Algorithm, LineEnding, PrivateKey, rand_core::OsRng};

                let key_name_raw = payload["name"].as_str().unwrap_or("id_ed25519_90ftp").trim();
                let comment = payload["comment"].as_str().unwrap_or("90ftp-key").trim();
                let algo_str = payload["algorithm"].as_str().unwrap_or("ed25519").to_lowercase();

                let safe_name: String = key_name_raw
                    .chars()
                    .map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.' { c } else { '_' })
                    .collect();
                let safe_name = if safe_name.is_empty() { "id_ed25519_90ftp".to_string() } else { safe_name };

                let mut keys_dir = crate::sessions::get_storage_dir();
                keys_dir.push("ssh_keys");
                let _ = std::fs::create_dir_all(&keys_dir);

                let priv_path = keys_dir.join(&safe_name);
                let pub_path = keys_dir.join(format!("{}.pub", safe_name));

                let algorithm = if algo_str.contains("rsa") {
                    Algorithm::Rsa { hash: None }
                } else {
                    Algorithm::Ed25519
                };

                let mut priv_key = match PrivateKey::random(&mut OsRng, algorithm) {
                    Ok(k) => k,
                    Err(e) => return IpcResponse::err(id, action, &format!("Failed to generate SSH keypair: {}", e)),
                };
                priv_key.set_comment(comment);

                let openssh_priv = match priv_key.to_openssh(LineEnding::LF) {
                    Ok(s) => s.to_string(),
                    Err(e) => return IpcResponse::err(id, action, &format!("Failed to encode private key: {}", e)),
                };
                let mut pub_key = priv_key.public_key().clone();
                pub_key.set_comment(comment);
                let openssh_pub = match pub_key.to_openssh() {
                    Ok(s) => s,
                    Err(e) => return IpcResponse::err(id, action, &format!("Failed to encode public key: {}", e)),
                };
                let fingerprint = pub_key.fingerprint(ssh_key::HashAlg::Sha256).to_string();

                if let Err(e) = std::fs::write(&priv_path, &openssh_priv) {
                    return IpcResponse::err(id, action, &format!("Failed to write private key: {}", e));
                }
                let _ = std::fs::write(&pub_path, format!("{}\n", openssh_pub));

                IpcResponse::ok(id, action, json!({
                    "name": safe_name,
                    "algorithm": if algo_str.contains("rsa") { "RSA" } else { "Ed25519" },
                    "comment": comment,
                    "fingerprint": fingerprint,
                    "public_key": openssh_pub,
                    "private_key_path": priv_path.to_string_lossy().to_string(),
                    "public_key_path": pub_path.to_string_lossy().to_string(),
                }))
            }

            "ssh_list_keys" => {
                let mut keys_dir = crate::sessions::get_storage_dir();
                keys_dir.push("ssh_keys");
                let _ = std::fs::create_dir_all(&keys_dir);

                let mut list = Vec::new();
                if let Ok(rd) = std::fs::read_dir(&keys_dir) {
                    for entry in rd.flatten() {
                        let p = entry.path();
                        if p.extension().and_then(|e| e.to_str()) == Some("pub") {
                            let pub_content = std::fs::read_to_string(&p).unwrap_or_default().trim().to_string();
                            let stem = p.file_stem().and_then(|s| s.to_str()).unwrap_or("key").to_string();
                            let priv_p = keys_dir.join(&stem);
                            list.push(json!({
                                "name": stem,
                                "public_key": pub_content,
                                "private_key_path": priv_p.to_string_lossy().to_string(),
                                "public_key_path": p.to_string_lossy().to_string(),
                            }));
                        }
                    }
                }
                IpcResponse::ok(id, action, json!({
                    "keys": list,
                    "keys_dir": keys_dir.to_string_lossy().to_string(),
                }))
            }

            "ssh_deploy_key" => {
                let pub_key = payload["public_key"].as_str().unwrap_or("").trim();
                if pub_key.is_empty() {
                    return IpcResponse::err(id, action, "Public key content is required");
                }
                match self.with_remote_retry(|client| {
                    let esc_key = pub_key.replace('\'', "'\\''");
                    let cmd = format!(
                        "mkdir -p ~/.ssh && chmod 700 ~/.ssh && touch ~/.ssh/authorized_keys && chmod 600 ~/.ssh/authorized_keys && (grep -qxF '{}' ~/.ssh/authorized_keys || echo '{}' >> ~/.ssh/authorized_keys)",
                        esc_key, esc_key
                    );
                    client.exec_command(&cmd)?;
                    Ok(())
                }) {
                    Ok((_, reconnected)) => IpcResponse::ok(id, action, json!({
                        "deployed": true,
                        "reconnected": reconnected
                    })),
                    Err(e) => IpcResponse::err(id, action, &e),
                }
            }

            "remote_server_health" => {
                match self.with_remote_retry(|client| {
                    let cmd = "echo '===UNAME==='; uname -srn 2>/dev/null; echo '===UPTIME==='; (uptime -p 2>/dev/null || uptime 2>/dev/null); echo '===LOAD==='; cat /proc/loadavg 2>/dev/null; echo '===MEM==='; free -m 2>/dev/null; echo '===DISK==='; df -hP 2>/dev/null";
                    let out = client.exec_command(cmd)?;
                    Ok(out)
                }) {
                    Ok((raw_output, reconnected)) => {
                        let mut section = "";
                        let mut uname = String::new();
                        let mut uptime = String::new();
                        let mut loadavg = String::new();
                        let mut mem_total_mb: u64 = 0;
                        let mut mem_used_mb: u64 = 0;
                        let mut disks = Vec::new();

                        for line in raw_output.lines() {
                            let t = line.trim();
                            if t.is_empty() {
                                continue;
                            }
                            match t {
                                "===UNAME===" => { section = "uname"; continue; }
                                "===UPTIME===" => { section = "uptime"; continue; }
                                "===LOAD===" => { section = "load"; continue; }
                                "===MEM===" => { section = "mem"; continue; }
                                "===DISK===" => { section = "disk"; continue; }
                                _ => {}
                            }
                            match section {
                                "uname" if uname.is_empty() => uname = t.to_string(),
                                "uptime" if uptime.is_empty() => uptime = t.to_string(),
                                "load" if loadavg.is_empty() => loadavg = t.to_string(),
                                "mem" => {
                                    if t.starts_with("Mem:") {
                                        let parts: Vec<&str> = t.split_whitespace().collect();
                                        if parts.len() >= 3 {
                                            mem_total_mb = parts[1].parse::<u64>().unwrap_or(0);
                                            mem_used_mb = parts[2].parse::<u64>().unwrap_or(0);
                                        }
                                    }
                                }
                                "disk" => {
                                    if !t.starts_with("Filesystem") {
                                        let parts: Vec<&str> = t.split_whitespace().collect();
                                        if parts.len() >= 6 {
                                            disks.push(json!({
                                                "filesystem": parts[0],
                                                "size": parts[1],
                                                "used": parts[2],
                                                "avail": parts[3],
                                                "use_percent": parts[4],
                                                "mount": parts[5..].join(" "),
                                            }));
                                        }
                                    }
                                }
                                _ => {}
                            }
                        }

                        IpcResponse::ok(id, action, json!({
                            "uname": uname,
                            "uptime": uptime,
                            "loadavg": loadavg,
                            "mem_total_mb": mem_total_mb,
                            "mem_used_mb": mem_used_mb,
                            "disks": disks,
                            "raw": raw_output,
                            "reconnected": reconnected
                        }))
                    }
                    Err(e) => IpcResponse::err(id, action, &e),
                }
            }

            "remote_grep_content" => {
                let path = payload["path"].as_str().unwrap_or("/").trim();
                let pattern = payload["pattern"].as_str().unwrap_or("").trim();
                let file_mask = payload["file_mask"].as_str().unwrap_or("").trim();
                let case_sensitive = payload["case_sensitive"].as_bool().unwrap_or(false);
                let max_results = payload["max_results"].as_u64().unwrap_or(200) as usize;

                if pattern.is_empty() {
                    return IpcResponse::err(id, action, "Search text pattern is required");
                }

                match self.with_remote_retry(|client| {
                    let esc_path = path.replace('\'', "'\\''");
                    let esc_pat = pattern.replace('\'', "'\\''");
                    let case_flag = if case_sensitive { "" } else { "-i" };
                    let mut include_flags = String::new();
                    if !file_mask.is_empty() {
                        for m in file_mask.split(|c| c == ',' || c == ';' || c == ' ').map(|s| s.trim()).filter(|s| !s.is_empty()) {
                            let safe_m = m.replace('\'', "");
                            include_flags.push_str(&format!(" --include='{}'", safe_m));
                        }
                    }
                    let cmd = format!(
                        "grep -rnI {} {} --exclude-dir='.git' --exclude-dir='node_modules' --exclude-dir='vendor' -- '{}' '{}' 2>/dev/null | head -n {}",
                        case_flag, include_flags, esc_pat, esc_path, max_results
                    );
                    let raw = client.exec_command(&cmd)?;
                    let mut matches = Vec::new();
                    for line in raw.lines() {
                        let mut parts = line.splitn(3, ':');
                        if let (Some(f_path), Some(line_no), Some(snippet)) = (parts.next(), parts.next(), parts.next()) {
                            if let Ok(ln) = line_no.parse::<u64>() {
                                let name = f_path.rsplit('/').next().unwrap_or(f_path).to_string();
                                let dir = match f_path.rfind('/') {
                                    Some(0) => "/".to_string(),
                                    Some(idx) => f_path[..idx].to_string(),
                                    None => "/".to_string(),
                                };
                                matches.push(json!({
                                    "path": f_path,
                                    "name": name,
                                    "dir": dir,
                                    "line": ln,
                                    "snippet": snippet.chars().take(240).collect::<String>()
                                }));
                            }
                        }
                    }
                    Ok(matches)
                }) {
                    Ok((matches, reconnected)) => IpcResponse::ok(id, action, json!({
                        "matches": matches,
                        "reconnected": reconnected
                    })),
                    Err(e) => IpcResponse::err(id, action, &e),
                }
            }

            "local_grep_content" => {
                let root_path = payload["path"].as_str().unwrap_or("").trim();
                let pattern = payload["pattern"].as_str().unwrap_or("").trim();
                let file_mask = payload["file_mask"].as_str().unwrap_or("").trim();
                let case_sensitive = payload["case_sensitive"].as_bool().unwrap_or(false);
                let max_results = payload["max_results"].as_u64().unwrap_or(200) as usize;

                if root_path.is_empty() || pattern.is_empty() {
                    return IpcResponse::err(id, action, "Both search path and text pattern are required");
                }

                let pat_cmp = if case_sensitive { pattern.to_string() } else { pattern.to_lowercase() };
                let masks: Vec<String> = file_mask
                    .split(|c| c == ',' || c == ';' || c == ' ')
                    .map(|s| s.trim().to_lowercase())
                    .filter(|s| !s.is_empty())
                    .collect();

                let mut matches = Vec::new();
                let mut stack = vec![std::path::PathBuf::from(root_path)];
                let mut scanned_files = 0usize;

                while let Some(dir) = stack.pop() {
                    if matches.len() >= max_results || scanned_files > 4000 {
                        break;
                    }
                    let entries = match std::fs::read_dir(&dir) {
                        Ok(rd) => rd,
                        Err(_) => continue,
                    };
                    for entry in entries.flatten() {
                        if matches.len() >= max_results {
                            break;
                        }
                        let p = entry.path();
                        let name = entry.file_name().to_string_lossy().to_string();
                        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
                        if is_dir {
                            if name != ".git" && name != "node_modules" && name != "target" && name != "vendor" && name != "__pycache__" {
                                stack.push(p);
                            }
                            continue;
                        }
                        if !masks.is_empty() {
                            let n_low = name.to_lowercase();
                            let matched_mask = masks.iter().any(|m| {
                                if let Some(ext) = m.strip_prefix("*.") {
                                    n_low.ends_with(&format!(".{}", ext))
                                } else {
                                    n_low.contains(m)
                                }
                            });
                            if !matched_mask {
                                continue;
                            }
                        }
                        let meta = match entry.metadata() {
                            Ok(m) => m,
                            Err(_) => continue,
                        };
                        if meta.len() > 2 * 1024 * 1024 {
                            continue; // Skip files > 2MB during fast grep
                        }
                        scanned_files += 1;
                        if let Ok(content) = std::fs::read_to_string(&p) {
                            let full_p = p.to_string_lossy().to_string();
                            let parent_dir = p.parent().map(|d| d.to_string_lossy().to_string()).unwrap_or_default();
                            for (idx, line_str) in content.lines().enumerate() {
                                let hit = if case_sensitive {
                                    line_str.contains(&pat_cmp)
                                } else {
                                    line_str.to_lowercase().contains(&pat_cmp)
                                };
                                if hit {
                                    matches.push(json!({
                                        "path": full_p,
                                        "name": name,
                                        "dir": parent_dir,
                                        "line": idx + 1,
                                        "snippet": line_str.trim().chars().take(240).collect::<String>()
                                    }));
                                    if matches.len() >= max_results {
                                        break;
                                    }
                                }
                            }
                        }
                    }
                }

                IpcResponse::ok(id, action, json!({
                    "matches": matches,
                    "scanned_files": scanned_files
                }))
            }

            "local_batch_rename" => {
                let current_dir = payload["current_dir"].as_str().unwrap_or("");
                let ops = payload["ops"].as_array().cloned().unwrap_or_default();
                let mut renamed = 0usize;
                let mut errors = Vec::new();
                for op in ops {
                    let old_p = op["old_path"].as_str().unwrap_or("");
                    let new_p = op["new_path"].as_str().unwrap_or("");
                    if !old_p.is_empty() && !new_p.is_empty() && old_p != new_p {
                        match std::fs::rename(old_p, new_p) {
                            Ok(_) => renamed += 1,
                            Err(e) => errors.push(format!("{}: {}", old_p, e)),
                        }
                    }
                }
                let files = if !current_dir.is_empty() {
                    crate::local_fs::list_dir(current_dir).unwrap_or_default()
                } else {
                    Vec::new()
                };
                IpcResponse::ok(id, action, json!({
                    "renamed": renamed,
                    "errors": errors,
                    "files": files
                }))
            }

            "remote_batch_rename" => {
                let current_dir = payload["current_dir"].as_str().unwrap_or("/").to_string();
                let ops = payload["ops"].as_array().cloned().unwrap_or_default();
                match self.with_remote_retry(|client| {
                    let mut renamed = 0usize;
                    let mut errors = Vec::new();
                    for op in &ops {
                        let old_p = op["old_path"].as_str().unwrap_or("");
                        let new_p = op["new_path"].as_str().unwrap_or("");
                        if !old_p.is_empty() && !new_p.is_empty() && old_p != new_p {
                            match client.rename(old_p, new_p) {
                                Ok(_) => renamed += 1,
                                Err(e) => errors.push(format!("{}: {}", old_p, e)),
                            }
                        }
                    }
                    let files = client.list(Some(&current_dir)).unwrap_or_default();
                    Ok((renamed, errors, files))
                }) {
                    Ok(((renamed, errors, files), reconnected)) => IpcResponse::ok(id, action, json!({
                        "renamed": renamed,
                        "errors": errors,
                        "files": files,
                        "reconnected": reconnected
                    })),
                    Err(e) => IpcResponse::err(id, action, &e),
                }
            }

            "remote_process_manager" => {
                match self.with_remote_retry(|client| {
                    let cmd = "echo '===PROCS==='; ps aux --sort=-%cpu 2>/dev/null | head -n 40; echo '===PORTS==='; (ss -tulnp 2>/dev/null || netstat -tulnp 2>/dev/null) | head -n 45; echo '===SERVICES==='; systemctl list-units --type=service --state=running,failed --no-pager --no-legend 2>/dev/null | head -n 45";
                    let raw = client.exec_command(cmd)?;
                    Ok(raw)
                }) {
                    Ok((raw, reconnected)) => {
                        let mut section = "";
                        let mut processes = Vec::new();
                        let mut ports = Vec::new();
                        let mut services = Vec::new();

                        for line in raw.lines() {
                            let t = line.trim();
                            if t.is_empty() { continue; }
                            match t {
                                "===PROCS===" => { section = "procs"; continue; }
                                "===PORTS===" => { section = "ports"; continue; }
                                "===SERVICES===" => { section = "services"; continue; }
                                _ => {}
                            }
                            match section {
                                "procs" => {
                                    if t.starts_with("USER") { continue; }
                                    let parts: Vec<&str> = t.split_whitespace().collect();
                                    if parts.len() >= 11 {
                                        processes.push(json!({
                                            "user": parts[0],
                                            "pid": parts[1],
                                            "cpu": parts[2],
                                            "mem": parts[3],
                                            "rss": parts[5],
                                            "stat": parts[7],
                                            "time": parts[9],
                                            "command": parts[10..].join(" ")
                                        }));
                                    }
                                }
                                "ports" => {
                                    if t.starts_with("Netid") || t.starts_with("Active") || t.starts_with("Proto") {
                                        continue;
                                    }
                                    let parts: Vec<&str> = t.split_whitespace().collect();
                                    if parts.len() >= 5 {
                                        ports.push(json!({
                                            "proto": parts[0],
                                            "state": parts[1],
                                            "local_addr": parts.get(4).copied().unwrap_or(parts[3]),
                                            "process": parts.last().copied().unwrap_or("-")
                                        }));
                                    }
                                }
                                "services" => {
                                    let clean = t.trim_start_matches('●').trim();
                                    let parts: Vec<&str> = clean.split_whitespace().collect();
                                    if parts.len() >= 4 {
                                        services.push(json!({
                                            "unit": parts[0],
                                            "load": parts[1],
                                            "active": parts[2],
                                            "sub": parts[3],
                                            "description": if parts.len() > 4 { parts[4..].join(" ") } else { String::new() }
                                        }));
                                    }
                                }
                                _ => {}
                            }
                        }

                        IpcResponse::ok(id, action, json!({
                            "processes": processes,
                            "ports": ports,
                            "services": services,
                            "reconnected": reconnected
                        }))
                    }
                    Err(e) => IpcResponse::err(id, action, &e),
                }
            }

            "remote_kill_process" => {
                let pid = payload["pid"].as_str().unwrap_or("").trim();
                let signal = payload["signal"].as_str().unwrap_or("15").trim();
                if pid.is_empty() || !pid.chars().all(|c| c.is_ascii_digit()) {
                    return IpcResponse::err(id, action, "Valid numeric PID is required");
                }
                let sig_clean = if signal == "9" || signal.eq_ignore_ascii_case("SIGKILL") { "9" } else { "15" };
                match self.with_remote_retry(|client| {
                    let cmd = format!("kill -{} {} 2>&1 || sudo -n kill -{} {} 2>&1", sig_clean, pid, sig_clean, pid);
                    let out = client.exec_command(&cmd)?;
                    Ok(out)
                }) {
                    Ok((out, reconnected)) => IpcResponse::ok(id, action, json!({
                        "pid": pid,
                        "signal": sig_clean,
                        "output": out.trim(),
                        "reconnected": reconnected
                    })),
                    Err(e) => IpcResponse::err(id, action, &e),
                }
            }

            "remote_service_action" => {
                let unit = payload["unit"].as_str().unwrap_or("").trim();
                let op = payload["op"].as_str().unwrap_or("restart").trim();
                if unit.is_empty() || !unit.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_' || c == '@') {
                    return IpcResponse::err(id, action, "Valid systemd unit name is required");
                }
                let safe_op = match op {
                    "start" | "stop" | "restart" | "reload" | "status" => op,
                    _ => "restart",
                };
                match self.with_remote_retry(|client| {
                    let cmd = format!("systemctl {} {} 2>&1 || sudo -n systemctl {} {} 2>&1", safe_op, unit, safe_op, unit);
                    let out = client.exec_command(&cmd)?;
                    Ok(out)
                }) {
                    Ok((out, reconnected)) => IpcResponse::ok(id, action, json!({
                        "unit": unit,
                        "op": safe_op,
                        "output": out.trim(),
                        "reconnected": reconnected
                    })),
                    Err(e) => IpcResponse::err(id, action, &e),
                }
            }

            "local_file_inspect" => {
                let path = payload["path"].as_str().unwrap_or("").trim();
                let mode = payload["mode"].as_str().unwrap_or("tail");
                let lines_count = payload["lines"].as_u64().unwrap_or(120) as usize;
                let max_bytes = payload["max_bytes"].as_u64().unwrap_or(4096) as usize;

                if path.is_empty() {
                    return IpcResponse::err(id, action, "File path is required");
                }

                let meta = match std::fs::metadata(path) {
                    Ok(m) => m,
                    Err(e) => return IpcResponse::err(id, action, &format!("Failed to stat file: {}", e)),
                };

                if mode == "hex" {
                    use std::io::Read;
                    let mut f = match std::fs::File::open(path) {
                        Ok(file) => file,
                        Err(e) => return IpcResponse::err(id, action, &format!("Failed to open file: {}", e)),
                    };
                    let mut buf = vec![0u8; max_bytes.min(16384)];
                    let n = f.read(&mut buf).unwrap_or(0);
                    buf.truncate(n);
                    let hex_dump = format_hex_dump(&buf);
                    IpcResponse::ok(id, action, json!({
                        "path": path,
                        "size": meta.len(),
                        "mode": "hex",
                        "content": hex_dump
                    }))
                } else {
                    let raw = std::fs::read_to_string(path).unwrap_or_else(|_| {
                        let bytes = std::fs::read(path).unwrap_or_default();
                        String::from_utf8_lossy(&bytes).to_string()
                    });
                    let all_lines: Vec<&str> = raw.lines().collect();
                    let start = all_lines.len().saturating_sub(lines_count);
                    let tail_str = all_lines[start..].join("\n");
                    IpcResponse::ok(id, action, json!({
                        "path": path,
                        "size": meta.len(),
                        "total_lines": all_lines.len(),
                        "mode": "tail",
                        "content": tail_str
                    }))
                }
            }

            "remote_file_inspect" => {
                let path = payload["path"].as_str().unwrap_or("").trim();
                let mode = payload["mode"].as_str().unwrap_or("tail").to_string();
                let lines_count = payload["lines"].as_u64().unwrap_or(120) as usize;
                let max_bytes = payload["max_bytes"].as_u64().unwrap_or(4096) as usize;

                if path.is_empty() {
                    return IpcResponse::err(id, action, "Remote file path is required");
                }

                match self.with_remote_retry(|client| {
                    let esc_p = path.replace('\'', "'\\''");
                    if mode == "hex" {
                        let data = client.download_file(path)?;
                        let slice = &data[..data.len().min(max_bytes.min(16384))];
                        Ok((format_hex_dump(slice), data.len() as u64))
                    } else {
                        let cmd = format!("tail -n {} '{}' 2>/dev/null", lines_count, esc_p);
                        if let Ok(out) = client.exec_command(&cmd) {
                            if !out.is_empty() {
                                return Ok((out, 0u64));
                            }
                        }
                        let data = client.download_file(path)?;
                        let raw = String::from_utf8_lossy(&data);
                        let all_lines: Vec<&str> = raw.lines().collect();
                        let start = all_lines.len().saturating_sub(lines_count);
                        Ok((all_lines[start..].join("\n"), data.len() as u64))
                    }
                }) {
                    Ok(((content, size), reconnected)) => IpcResponse::ok(id, action, json!({
                        "path": path,
                        "size": size,
                        "mode": mode,
                        "content": content,
                        "reconnected": reconnected
                    })),
                    Err(e) => IpcResponse::err(id, action, &e),
                }
            }

            unknown => IpcResponse::err(id, action, &format!("Unknown action: {}", unknown)),
        }
    }
}

fn format_hex_dump(bytes: &[u8]) -> String {
    let mut out = String::new();
    for (chunk_idx, chunk) in bytes.chunks(16).enumerate() {
        let offset = chunk_idx * 16;
        out.push_str(&format!("{:08X}  ", offset));
        for i in 0..16 {
            if i < chunk.len() {
                out.push_str(&format!("{:02X} ", chunk[i]));
            } else {
                out.push_str("   ");
            }
            if i == 7 {
                out.push(' ');
            }
        }
        out.push_str(" |");
        for &b in chunk {
            let c = if (0x20..=0x7E).contains(&b) { b as char } else { '.' };
            out.push(c);
        }
        out.push_str("|\n");
    }
    if out.is_empty() {
        "(Empty file - 0 bytes)".to_string()
    } else {
        out
    }
}
