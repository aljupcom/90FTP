use std::fs;
use std::path::{Path, PathBuf};
use aes_gcm::{
    aead::{Aead, KeyInit},
    Aes256Gcm, Nonce,
};
use base64::Engine;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use crate::models::SiteProfile;

const VAULT_MAGIC: &[u8; 4] = b"90FV";
const VAULT_VERSION: u8 = 1;
const VAULT_PEPPER: &[u8] = b"90FTP_AEAD_AES256GCM_MASTER_KEY_2026_PEPPER_SECURE_AUTH";
const FILENAME_SALT: &[u8] = b"90FTP_OBFUSCATED_FILENAME_DERIVATION_SALT_V2";

/// Derives a 256-bit AES master key tied cryptographically to this user and machine.
fn get_master_vault_key() -> [u8; 32] {
    let user = std::env::var("USERNAME").unwrap_or_else(|_| "90ftp_user".to_string());
    let domain = std::env::var("USERDOMAIN").unwrap_or_else(|_| "localhost".to_string());
    let computer = std::env::var("COMPUTERNAME").unwrap_or_else(|_| "pc".to_string());
    let arch = std::env::var("PROCESSOR_IDENTIFIER").unwrap_or_else(|_| "x86_64".to_string());

    let mut hasher = Sha256::new();
    hasher.update(VAULT_PEPPER);
    hasher.update(user.as_bytes());
    hasher.update(b"|");
    hasher.update(domain.as_bytes());
    hasher.update(b"|");
    hasher.update(computer.as_bytes());
    hasher.update(b"|");
    hasher.update(arch.as_bytes());

    let result = hasher.finalize();
    let mut key = [0u8; 32];
    key.copy_from_slice(&result);
    key
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write;
        let _ = write!(s, "{:02x}", b);
    }
    s
}

/// Derives a cryptographically secure, obfuscated filename for a logical store.
/// E.g. "sites" -> "v_7b3a9e102f4a.dat"
fn get_vault_file_path(logical_store_id: &str) -> PathBuf {
    let key = get_master_vault_key();
    let mut hasher = Sha256::new();
    hasher.update(FILENAME_SALT);
    hasher.update(&key);
    hasher.update(logical_store_id.as_bytes());
    let hash = hasher.finalize();
    let filename = format!("v_{}.dat", hex_encode(&hash[0..12]));

    let mut p = get_storage_dir();
    p.push(filename);
    p
}

/// Securely wipes a file from disk before deletion by overwriting with zeroes.
fn secure_wipe_file(path: &Path) {
    if path.exists() {
        if let Ok(meta) = fs::metadata(path) {
            let len = meta.len() as usize;
            if len > 0 {
                let zeroes = vec![0u8; len];
                let _ = fs::write(path, zeroes);
            }
        }
        let _ = fs::remove_file(path);
    }
}

/// Encrypts raw data using AES-256-GCM authenticated cipher with a fresh random 12-byte Nonce.
/// Binary format: [MAGIC 4B "90FV"] + [VERSION 1B 0x01] + [NONCE 12B] + [CIPHERTEXT + 16B TAG]
fn encrypt_vault_payload(plaintext: &[u8]) -> Result<Vec<u8>, String> {
    let key = get_master_vault_key();
    let cipher = Aes256Gcm::new_from_slice(&key).map_err(|e| format!("AES init error: {:?}", e))?;

    let mut nonce_bytes = [0u8; 12];
    rand::thread_rng().fill_bytes(&mut nonce_bytes);
    let nonce = Nonce::from(nonce_bytes);

    let ciphertext = cipher
        .encrypt(&nonce, plaintext)
        .map_err(|e| format!("Encryption error: {:?}", e))?;

    let mut out = Vec::with_capacity(VAULT_MAGIC.len() + 1 + 12 + ciphertext.len());
    out.extend_from_slice(VAULT_MAGIC);
    out.push(VAULT_VERSION);
    out.extend_from_slice(&nonce_bytes);
    out.extend_from_slice(&ciphertext);
    Ok(out)
}

/// Decrypts AES-256-GCM authenticated vault payload.
fn decrypt_vault_payload(data: &[u8]) -> Result<Vec<u8>, String> {
    if data.len() < VAULT_MAGIC.len() + 1 + 12 + 16 {
        return Err("Vault data too short or corrupted".to_string());
    }
    if &data[0..4] != VAULT_MAGIC {
        return Err("Invalid vault magic header".to_string());
    }
    let version = data[4];
    if version != VAULT_VERSION {
        return Err(format!("Unsupported vault version: {}", version));
    }
    let nonce_bytes = &data[5..17];
    let ciphertext = &data[17..];

    let key = get_master_vault_key();
    let cipher = Aes256Gcm::new_from_slice(&key).map_err(|e| format!("AES init error: {:?}", e))?;
    let nonce = Nonce::try_from(nonce_bytes).map_err(|e| format!("Invalid nonce: {:?}", e))?;

    let decrypted = cipher
        .decrypt(&nonce, ciphertext)
        .map_err(|e| format!("Decryption failed (tampered data or wrong machine): {:?}", e))?;

    Ok(decrypted)
}

/// Writes encrypted data to the specified path.
fn write_vault_encrypted(path: &Path, plaintext: &[u8]) -> Result<(), String> {
    let encrypted = encrypt_vault_payload(plaintext)?;
    fs::write(path, encrypted).map_err(|e| e.to_string())
}

/// Reads and decrypts data from the specified path.
fn read_vault_encrypted(path: &Path) -> Result<Vec<u8>, String> {
    let raw = fs::read(path).map_err(|e| e.to_string())?;
    decrypt_vault_payload(&raw)
}

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
    get_vault_file_path("window_state_v2")
}

pub fn load_window_state() -> WindowState {
    let p = get_window_state_path();
    let legacy_p = get_storage_dir().join("window.json");

    if p.exists() {
        if let Ok(decrypted) = read_vault_encrypted(&p) {
            if let Ok(state) = serde_json::from_slice::<WindowState>(&decrypted) {
                if state.width >= 600.0 && state.height >= 400.0 {
                    return state;
                }
            }
        }
    } else if legacy_p.exists() {
        // Migrate from legacy unencrypted window.json
        if let Ok(content) = fs::read_to_string(&legacy_p) {
            if let Ok(state) = serde_json::from_str::<WindowState>(&content) {
                save_window_state(&state);
                secure_wipe_file(&legacy_p);
                return state;
            }
        }
    }

    WindowState::default()
}

pub fn save_window_state(state: &WindowState) {
    let p = get_window_state_path();
    if let Ok(json) = serde_json::to_vec(state) {
        let _ = write_vault_encrypted(&p, &json);
    }
    // Clean up any legacy unencrypted file
    let legacy_p = get_storage_dir().join("window.json");
    secure_wipe_file(&legacy_p);
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KnownHost {
    pub host: String,
    pub port: u16,
    pub fingerprint: String,
    pub trusted_at: String,
}

pub fn get_known_hosts_path() -> PathBuf {
    get_vault_file_path("known_hosts_v2")
}

pub fn load_known_hosts() -> Vec<KnownHost> {
    let p = get_known_hosts_path();
    let legacy_p = get_storage_dir().join("known_hosts.json");

    if p.exists() {
        if let Ok(decrypted) = read_vault_encrypted(&p) {
            if let Ok(hosts) = serde_json::from_slice::<Vec<KnownHost>>(&decrypted) {
                return hosts;
            }
        }
    } else if legacy_p.exists() {
        // Migrate from legacy unencrypted known_hosts.json
        if let Ok(c) = fs::read_to_string(&legacy_p) {
            if let Ok(hosts) = serde_json::from_str::<Vec<KnownHost>>(&c) {
                if let Ok(json) = serde_json::to_vec(&hosts) {
                    let _ = write_vault_encrypted(&p, &json);
                }
                secure_wipe_file(&legacy_p);
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
        if let Ok(json) = serde_json::to_vec(&hosts) {
            let _ = write_vault_encrypted(&p, &json);
        }
        let legacy_p = get_storage_dir().join("known_hosts.json");
        secure_wipe_file(&legacy_p);
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
    get_vault_file_path("sites_profiles_v2")
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
    let legacy_p = get_storage_dir().join("sites.json");

    if p.exists() {
        if let Ok(decrypted) = read_vault_encrypted(&p) {
            if let Ok(mut sites) = serde_json::from_slice::<Vec<SiteProfile>>(&decrypted) {
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
    } else if legacy_p.exists() {
        // Migrate legacy sites.json into encrypted AES-256-GCM vault
        if let Ok(content) = fs::read_to_string(&legacy_p) {
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
                // Save immediately into new encrypted vault format
                let _ = save_sites(&sites);
                // Securely shred unencrypted legacy sites.json
                secure_wipe_file(&legacy_p);
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

    // Entire JSON structure (host, username, ports, keys, paths, and passwords) is AES-256-GCM encrypted
    let json_bytes = serde_json::to_vec(&encrypted_sites).map_err(|e| e.to_string())?;
    write_vault_encrypted(&p, &json_bytes)?;

    // Ensure any legacy unencrypted sites.json is completely wiped
    let legacy_p = get_storage_dir().join("sites.json");
    secure_wipe_file(&legacy_p);

    Ok(())
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
    get_vault_file_path("remote_history_v2")
}

pub fn load_remote_history() -> Vec<String> {
    let p = get_remote_history_path();
    let legacy_p = get_storage_dir().join("remote_history.json");

    if p.exists() {
        if let Ok(decrypted) = read_vault_encrypted(&p) {
            if let Ok(paths) = serde_json::from_slice::<Vec<String>>(&decrypted) {
                if !paths.is_empty() {
                    return paths;
                }
            }
        }
    } else if legacy_p.exists() {
        // Migrate legacy remote_history.json
        if let Ok(c) = fs::read_to_string(&legacy_p) {
            if let Ok(paths) = serde_json::from_str::<Vec<String>>(&c) {
                if let Ok(json) = serde_json::to_vec(&paths) {
                    let _ = write_vault_encrypted(&p, &json);
                }
                secure_wipe_file(&legacy_p);
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
    if let Ok(json) = serde_json::to_vec(&paths) {
        let _ = write_vault_encrypted(&p, &json);
    }
    let legacy_p = get_storage_dir().join("remote_history.json");
    secure_wipe_file(&legacy_p);

    paths
}

pub fn clear_known_hosts() -> Result<(), String> {
    let p = get_known_hosts_path();
    secure_wipe_file(&p);
    let legacy_p = get_storage_dir().join("known_hosts.json");
    secure_wipe_file(&legacy_p);
    Ok(())
}

pub fn clear_remote_history() -> Result<Vec<String>, String> {
    let p = get_remote_history_path();
    secure_wipe_file(&p);
    let legacy_p = get_storage_dir().join("remote_history.json");
    secure_wipe_file(&legacy_p);
    Ok(load_remote_history())
}

pub fn open_storage_dir() {
    let dir = get_storage_dir();
    let _ = std::process::Command::new("explorer").arg(dir).spawn();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_vault_roundtrip() {
        let sample = b"Sensitive site credentials: host=84.46.254.56, user=root";
        let encrypted = encrypt_vault_payload(sample).expect("Encryption failed");
        assert_ne!(&encrypted[..], sample);
        assert_eq!(&encrypted[0..4], b"90FV");
        let decrypted = decrypt_vault_payload(&encrypted).expect("Decryption failed");
        assert_eq!(decrypted, sample);
    }

    #[test]
    fn test_obfuscated_filenames() {
        let sites_path = get_vault_file_path("sites_profiles_v2");
        let history_path = get_vault_file_path("remote_history_v2");
        let hosts_path = get_vault_file_path("known_hosts_v2");

        let sites_name = sites_path.file_name().unwrap().to_str().unwrap();
        let history_name = history_path.file_name().unwrap().to_str().unwrap();
        let hosts_name = hosts_path.file_name().unwrap().to_str().unwrap();

        assert!(sites_name.starts_with("v_") && sites_name.ends_with(".dat"));
        assert!(history_name.starts_with("v_") && history_name.ends_with(".dat"));
        assert!(hosts_name.starts_with("v_") && hosts_name.ends_with(".dat"));

        assert_ne!(sites_name, history_name);
        assert_ne!(sites_name, hosts_name);
        assert!(!sites_name.contains("site"));
        assert!(!history_name.contains("history"));
        assert!(!hosts_name.contains("hosts"));
    }

    #[test]
    fn test_migration_and_shredding() {
        let sites = load_sites();
        let hosts = load_known_hosts();
        let history = load_remote_history();
        let _window = load_window_state();

        println!("Loaded sites count: {}", sites.len());
        println!("Loaded hosts count: {}", hosts.len());
        println!("Loaded history count: {}", history.len());

        // Verify that the vault files exist
        assert!(get_storage_path().exists());
        assert!(get_known_hosts_path().exists());
        assert!(get_remote_history_path().exists());
        assert!(get_window_state_path().exists());

        // Verify that the legacy unencrypted json files are gone!
        assert!(!get_storage_dir().join("sites.json").exists());
        assert!(!get_storage_dir().join("known_hosts.json").exists());
        assert!(!get_storage_dir().join("remote_history.json").exists());
        assert!(!get_storage_dir().join("window.json").exists());
    }
}

