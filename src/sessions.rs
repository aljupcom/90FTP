use std::fs;
use std::path::PathBuf;
use base64::Engine;
use serde::{Deserialize, Serialize};
use crate::models::SiteProfile;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WindowState {
    pub width: f64,
    pub height: f64,
    pub is_maximized: bool,
}

impl Default for WindowState {
    fn default() -> Self {
        Self {
            width: 1280.0,
            height: 800.0,
            is_maximized: false,
        }
    }
}

pub fn get_window_state_path() -> PathBuf {
    let mut p = get_storage_dir();
    p.push("window.json");
    p
}

pub fn load_window_state() -> WindowState {
    let p = get_window_state_path();
    if p.exists() {
        if let Ok(content) = fs::read_to_string(p) {
            if let Ok(state) = serde_json::from_str::<WindowState>(&content) {
                if state.width >= 600.0 && state.height >= 400.0 {
                    return state;
                }
            }
        }
    }
    WindowState::default()
}

pub fn save_window_state(state: &WindowState) {
    let p = get_window_state_path();
    if let Ok(json) = serde_json::to_string_pretty(state) {
        let _ = fs::write(p, json);
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KnownHost {
    pub host: String,
    pub port: u16,
    pub fingerprint: String,
    pub trusted_at: String,
}

pub fn get_known_hosts_path() -> PathBuf {
    let mut p = get_storage_dir();
    p.push("known_hosts.json");
    p
}

pub fn load_known_hosts() -> Vec<KnownHost> {
    let p = get_known_hosts_path();
    if p.exists() {
        if let Ok(c) = fs::read_to_string(p) {
            if let Ok(hosts) = serde_json::from_str::<Vec<KnownHost>>(&c) {
                return hosts;
            }
        }
    }
    Vec::new()
}

pub fn is_host_trusted(host: &str, port: u16, fingerprint: &str) -> bool {
    let hosts = load_known_hosts();
    hosts.iter().any(|h| {
        (h.host == host || h.host == "*")
            && (h.port == port || h.port == 0)
            && h.fingerprint == fingerprint
    })
}

pub fn trust_host(host: &str, port: u16, fingerprint: &str) {
    let mut hosts = load_known_hosts();
    if !is_host_trusted(host, port, fingerprint) {
        hosts.push(KnownHost {
            host: host.to_string(),
            port,
            fingerprint: fingerprint.to_string(),
            trusted_at: chrono::Utc::now().to_rfc3339(),
        });
        let p = get_known_hosts_path();
        if let Ok(json) = serde_json::to_string_pretty(&hosts) {
            let _ = fs::write(p, json);
        }
    }
}

pub fn get_storage_dir() -> PathBuf {
    if let Some(appdata) = std::env::var_os("APPDATA") {
        let mut p = PathBuf::from(appdata);
        p.push("90FTP");
        let _ = fs::create_dir_all(&p);
        p
    } else {
        PathBuf::from(".")
    }
}

pub fn get_storage_path() -> PathBuf {
    let mut p = get_storage_dir();
    p.push("sites.json");
    p
}

pub fn get_storage_file_path() -> String {
    get_storage_path().to_string_lossy().to_string()
}

fn get_user_cipher_key() -> Vec<u8> {
    let user = std::env::var("USERNAME").unwrap_or_else(|_| "90ftp_user".into());
    let domain = std::env::var("USERDOMAIN").unwrap_or_else(|_| "local".into());
    let raw = format!("90FTP_SECURITY_SALT_2026_{}_{}_CRED_PROTECT", user, domain);
    let mut key = Vec::with_capacity(32);
    for (i, b) in raw.bytes().cycle().take(32).enumerate() {
        key.push(b ^ ((i as u8).wrapping_mul(37).wrapping_add(19)));
    }
    key
}

pub fn encrypt_credential(plain: &str) -> String {
    if plain.is_empty() {
        return String::new();
    }
    if plain.starts_with("enc:v1:") {
        return plain.to_string();
    }
    let key = get_user_cipher_key();
    let bytes = plain.as_bytes();
    let mut enc = Vec::with_capacity(bytes.len());
    for (i, &b) in bytes.iter().enumerate() {
        enc.push(b ^ key[i % key.len()]);
    }
    format!("enc:v1:{}", base64::engine::general_purpose::STANDARD.encode(enc))
}

pub fn decrypt_credential(cipher: &str) -> String {
    if !cipher.starts_with("enc:v1:") {
        return cipher.to_string();
    }
    let stripped = &cipher["enc:v1:".len()..];
    if let Ok(decoded) = base64::engine::general_purpose::STANDARD.decode(stripped) {
        let key = get_user_cipher_key();
        let mut dec = Vec::with_capacity(decoded.len());
        for (i, &b) in decoded.iter().enumerate() {
            dec.push(b ^ key[i % key.len()]);
        }
        if let Ok(s) = String::from_utf8(dec) {
            return s;
        }
    }
    cipher.to_string()
}

pub fn load_sites() -> Vec<SiteProfile> {
    let p = get_storage_path();
    if p.exists() {
        if let Ok(content) = fs::read_to_string(p) {
            if let Ok(mut sites) = serde_json::from_str::<Vec<SiteProfile>>(&content) {
                for site in &mut sites {
                    if !site.password.is_empty() {
                        site.password = decrypt_credential(&site.password);
                    }
                    if let Some(ref pass) = site.passphrase {
                        if !pass.is_empty() {
                            site.passphrase = Some(decrypt_credential(pass));
                        }
                    }
                }
                return sites;
            }
        }
    }
    Vec::new()
}

pub fn save_sites(sites: &[SiteProfile]) -> Result<(), String> {
    let p = get_storage_path();
    let mut encrypted_sites = sites.to_vec();
    for site in &mut encrypted_sites {
        if !site.password.is_empty() {
            site.password = encrypt_credential(&site.password);
        }
        if let Some(ref pass) = site.passphrase {
            if !pass.is_empty() {
                site.passphrase = Some(encrypt_credential(pass));
            }
        }
    }
    let json = serde_json::to_string_pretty(&encrypted_sites).map_err(|e| e.to_string())?;
    fs::write(p, json).map_err(|e| e.to_string())
}

pub fn upsert_site(site: SiteProfile) -> Result<Vec<SiteProfile>, String> {
    let mut sites = load_sites();
    if let Some(pos) = sites.iter().position(|s| s.id == site.id) {
        sites[pos] = site;
    } else {
        sites.push(site);
    }
    save_sites(&sites)?;
    Ok(sites)
}

pub fn delete_site(id: &str) -> Result<Vec<SiteProfile>, String> {
    let mut sites = load_sites();
    sites.retain(|s| s.id != id);
    save_sites(&sites)?;
    Ok(sites)
}

pub fn get_remote_history_path() -> PathBuf {
    let mut p = get_storage_dir();
    p.push("remote_history.json");
    p
}

pub fn load_remote_history() -> Vec<String> {
    let p = get_remote_history_path();
    if p.exists() {
        if let Ok(c) = fs::read_to_string(p) {
            if let Ok(paths) = serde_json::from_str::<Vec<String>>(&c) {
                if !paths.is_empty() {
                    return paths;
                }
            }
        }
    }
    vec![
        "/".to_string(),
        "/var/www".to_string(),
        "/var/www/html".to_string(),
        "/root".to_string(),
        "/home".to_string(),
    ]
}

pub fn add_remote_history(path: &str) -> Vec<String> {
    let clean = path.trim();
    if clean.is_empty() {
        return load_remote_history();
    }
    let mut paths = load_remote_history();
    paths.retain(|p| p != clean);
    paths.insert(0, clean.to_string());
    if paths.len() > 30 {
        paths.truncate(30);
    }
    let p = get_remote_history_path();
    if let Ok(json) = serde_json::to_string_pretty(&paths) {
        let _ = fs::write(p, json);
    }
    paths
}

pub fn clear_known_hosts() -> Result<(), String> {
    let p = get_known_hosts_path();
    if p.exists() {
        fs::remove_file(p).map_err(|e| e.to_string())?;
    }
    Ok(())
}

pub fn clear_remote_history() -> Result<Vec<String>, String> {
    let p = get_remote_history_path();
    if p.exists() {
        let _ = fs::remove_file(p);
    }
    Ok(load_remote_history())
}

pub fn open_storage_dir() {
    let dir = get_storage_dir();
    let _ = std::process::Command::new("explorer").arg(dir).spawn();
}


