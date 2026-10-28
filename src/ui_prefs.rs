//! Plain-JSON UI preferences (interface language, and future presentation-only
//! settings).
//!
//! This module is deliberately separate from [`crate::sessions`]: that module
//! owns the encrypted credential vault, and mixing a non-secret preference file
//! into it would invite accidental coupling with the DPAPI key handling.
//!
//! Storage location: `%APPDATA%\90FTP\ui_prefs.json`, honouring the
//! `NINETYFTP_HOME` override through [`crate::sessions::get_storage_dir`].

use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

const PREFS_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UiPrefs {
    #[serde(default = "default_version")]
    pub version: u32,
    #[serde(default = "default_language")]
    pub language: String,
}

fn default_version() -> u32 {
    PREFS_VERSION
}

fn default_language() -> String {
    "en".to_string()
}

impl Default for UiPrefs {
    fn default() -> Self {
        Self {
            version: PREFS_VERSION,
            language: default_language(),
        }
    }
}

/// Whitelist mapping. Anything unrecognised collapses to English so a
/// hand-edited or corrupted file can never drive the renderer into an
/// unsupported state.
pub fn normalize_language(raw: &str) -> String {
    match raw.trim().to_ascii_lowercase().as_str() {
        "ar" | "arabic" | "ar-sa" | "ar_sa" => "ar".to_string(),
        _ => "en".to_string(),
    }
}

pub fn get_ui_prefs_path() -> PathBuf {
    let mut path = crate::sessions::get_storage_dir();
    path.push("ui_prefs.json");
    path
}

pub fn load() -> UiPrefs {
    load_from_path(&get_ui_prefs_path())
}

pub fn load_from_path(path: &Path) -> UiPrefs {
    match fs::read_to_string(path) {
        Ok(raw) => {
            let mut prefs: UiPrefs = serde_json::from_str(&raw).unwrap_or_default();
            prefs.version = PREFS_VERSION;
            prefs.language = normalize_language(&prefs.language);
            prefs
        }
        Err(_) => UiPrefs::default(),
    }
}

pub fn save(prefs: &UiPrefs) -> Result<UiPrefs, String> {
    let mut next = prefs.clone();
    next.version = PREFS_VERSION;
    next.language = normalize_language(&next.language);
    save_to_path(&get_ui_prefs_path(), &next)?;
    Ok(next)
}

/// Write through a temporary file in the same directory and rename over the
/// target. On Windows `rename` maps to `MoveFileExW(MOVEFILE_REPLACE_EXISTING)`,
/// so a reader never observes a half-written file.
pub fn save_to_path(path: &Path, prefs: &UiPrefs) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let json = serde_json::to_vec_pretty(prefs).map_err(|e| e.to_string())?;
    let tmp = path.with_extension("json.tmp");

    let mut last_error = String::new();
    for attempt in 0..3u32 {
        match fs::write(&tmp, &json).and_then(|_| fs::rename(&tmp, path)) {
            Ok(_) => return Ok(()),
            Err(err) => {
                last_error = err.to_string();
                std::thread::sleep(std::time::Duration::from_millis(20 * u64::from(attempt + 1)));
            }
        }
    }
    let _ = fs::remove_file(&tmp);
    Err(last_error)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(name: &str) -> PathBuf {
        let mut dir = std::env::temp_dir();
        dir.push(format!("90ftp_uiprefs_test_{}_{}", std::process::id(), name));
        let _ = fs::create_dir_all(&dir);
        dir.push("ui_prefs.json");
        dir
    }

    #[test]
    fn normalize_language_accepts_known_values() {
        assert_eq!(normalize_language("ar"), "ar");
        assert_eq!(normalize_language("AR"), "ar");
        assert_eq!(normalize_language(" Arabic "), "ar");
        assert_eq!(normalize_language("ar-sa"), "ar");
        assert_eq!(normalize_language("en"), "en");
    }

    #[test]
    fn normalize_language_rejects_everything_else() {
        assert_eq!(normalize_language(""), "en");
        assert_eq!(normalize_language("fr"), "en");
        assert_eq!(normalize_language("../../etc/passwd"), "en");
    }

    #[test]
    fn missing_file_falls_back_to_english() {
        let path = temp_path("missing");
        let _ = fs::remove_file(&path);
        let prefs = load_from_path(&path);
        assert_eq!(prefs.language, "en");
        assert_eq!(prefs.version, PREFS_VERSION);
    }

    #[test]
    fn round_trip_preserves_language() {
        let path = temp_path("roundtrip");
        let prefs = UiPrefs {
            version: 0,
            language: "AR".to_string(),
        };
        save_to_path(&path, &prefs).expect("save should succeed");
        let loaded = load_from_path(&path);
        assert_eq!(loaded.language, "ar");
        assert_eq!(loaded.version, PREFS_VERSION);
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn corrupted_file_falls_back_without_panicking() {
        let path = temp_path("corrupt");
        fs::write(&path, "{not json").expect("write should succeed");
        let prefs = load_from_path(&path);
        assert_eq!(prefs.language, "en");
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn unknown_fields_are_ignored() {
        let path = temp_path("unknown");
        fs::write(&path, r#"{"language":"ar","future_key":true}"#).expect("write should succeed");
        let prefs = load_from_path(&path);
        assert_eq!(prefs.language, "ar");
        let _ = fs::remove_file(&path);
    }
}
