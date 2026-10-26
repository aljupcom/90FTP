//! Live filesystem change detection for the two commander panes.
//!
//! The UI previously only reflected changes that it made itself; anything that
//! happened outside the app (another editor saving a file, a cron job, a
//! colleague uploading over SFTP) stayed invisible until the user pressed F5.
//!
//! This module produces a cheap structural fingerprint of a directory
//! (entry name + size + modification stamp) so a single background thread can
//! notice external mutations and push an event to the WebView. Polling is used
//! deliberately: it works identically for local disks and remote servers, it
//! needs no extra dependency, and it never misses a change that happened while
//! the app was busy.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::time::UNIX_EPOCH;

use crate::remote::RemoteClient;

/// Fingerprints a local directory. Returns 0 when the directory cannot be read,
/// which the caller treats as "unknown" rather than "empty".
pub fn local_signature(dir: &str) -> u64 {
    if dir.trim().is_empty() {
        return 0;
    }

    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return 0,
    };

    let mut rows: Vec<(String, bool, u64, u128)> = Vec::new();

    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        let meta = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        let is_dir = meta.is_dir();
        let size = if is_dir { 0 } else { meta.len() };
        let modified = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        rows.push((name, is_dir, size, modified));
    }

    // Sorting keeps the fingerprint stable regardless of read_dir ordering.
    rows.sort_by(|a, b| a.0.cmp(&b.0));

    let mut hasher = DefaultHasher::new();
    rows.hash(&mut hasher);
    hasher.finish()
}

/// Fingerprints a remote directory using the same idea. `modified` from the
/// server listing is a string, but it changes whenever a file is rewritten, so
/// it is a sufficient change signal.
pub fn remote_signature(client: &mut RemoteClient, dir: &str) -> Result<u64, String> {
    if dir.trim().is_empty() {
        return Ok(0);
    }

    let files = client.list(Some(dir))?;

    let mut rows: Vec<(String, bool, u64, String)> = files
        .into_iter()
        .map(|f| (f.name, f.is_dir, f.size, f.modified))
        .collect();
    rows.sort_by(|a, b| a.0.cmp(&b.0));

    let mut hasher = DefaultHasher::new();
    rows.hash(&mut hasher);
    Ok(hasher.finish())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch_dir(tag: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("90ftp_watch_{}_{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn signature_reacts_to_create_modify_and_delete() {
        let dir = scratch_dir("change");
        let path = dir.to_string_lossy().to_string();

        let empty = local_signature(&path);
        assert_ne!(empty, 0, "an existing directory must produce a real fingerprint");

        std::fs::write(dir.join("a.txt"), b"hello").unwrap();
        let after_create = local_signature(&path);
        assert_ne!(empty, after_create, "creating a file must change the fingerprint");

        std::fs::write(dir.join("a.txt"), b"hello world").unwrap();
        let after_modify = local_signature(&path);
        assert_ne!(after_create, after_modify, "modifying a file must change the fingerprint");

        std::fs::remove_file(dir.join("a.txt")).unwrap();
        assert_ne!(after_modify, local_signature(&path), "deleting a file must change the fingerprint");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_directory_is_unknown_not_empty() {
        let mut p = std::env::temp_dir();
        p.push("90ftp_watch_definitely_missing_dir_xyz");
        assert_eq!(local_signature(&p.to_string_lossy()), 0);
    }
}
