//! Live filesystem change detection for the two commander panes.
//!
//! The UI previously only reflected changes that it made itself; anything that
//! happened outside the app (another editor saving a file, a cron job, a
//! colleague uploading over SFTP) stayed invisible until the user pressed F5.
//!
//! Two complementary strategies live here:
//!
//! * [`NativeWatcher`] — a real OS change notification (`ReadDirectoryChangesW`
//!   on Windows, via the `notify` crate). Latency is ~0 ms instead of one poll
//!   interval, and an idle folder costs nothing: no directory is ever re-read.
//! * [`local_signature`] / [`remote_signature`] — cheap structural fingerprints
//!   (entry name + size + modification stamp). These remain the fallback for
//!   local paths the OS watcher cannot attach to (network shares, exotic
//!   filesystems) and the only option for remote directories, where there is no
//!   push channel to subscribe to.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::UNIX_EPOCH;

use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};

use crate::remote::RemoteClient;

/// Quiet period before a burst of native events is reported. Saving a file
/// through an editor usually produces several events (write, rename, attribute
/// change); collapsing them keeps the UI from refreshing three times.
pub const DEBOUNCE_MS: u64 = 120;

/// Milliseconds since the Unix epoch. Used as a cheap monotonic-ish stamp for
/// debouncing; it never needs to be wall-clock accurate.
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// An OS-level directory watcher bound to a single (non-recursive) directory.
///
/// The struct owns the platform watcher, so it must stay alive for as long as
/// events are wanted; dropping it stops delivery. Callers hold it inside their
/// own thread and call [`NativeWatcher::retarget`] when the pane navigates.
pub struct NativeWatcher {
    inner: RecommendedWatcher,
    current: Option<std::path::PathBuf>,
    /// Total number of interesting events seen. The owner compares this against
    /// its own last-seen value instead of being called back across threads.
    pub events: Arc<AtomicU64>,
    /// Timestamp of the most recent event, for debouncing.
    pub last_event_ms: Arc<AtomicU64>,
}

impl NativeWatcher {
    /// Creates the platform watcher. Returns `Err` when the OS backend cannot be
    /// initialised, in which case the caller should fall back to fingerprinting.
    pub fn new() -> Result<Self, String> {
        let events = Arc::new(AtomicU64::new(0));
        let last_event_ms = Arc::new(AtomicU64::new(0));

        let events_cb = events.clone();
        let last_cb = last_event_ms.clone();

        let inner = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            let Ok(event) = res else { return };
            // Access events (open/read/close) do not change a directory listing.
            if matches!(event.kind, EventKind::Access(_)) {
                return;
            }
            events_cb.fetch_add(1, Ordering::Relaxed);
            last_cb.store(now_ms(), Ordering::Relaxed);
        })
        .map_err(|e| format!("native watcher unavailable: {}", e))?;

        Ok(Self {
            inner,
            current: None,
            events,
            last_event_ms,
        })
    }

    /// Points the watcher at `dir`, detaching from the previous directory.
    /// Returns `false` when the path cannot be watched, so the caller can fall
    /// back to polling instead of silently going deaf.
    pub fn retarget(&mut self, dir: &str) -> bool {
        let path = std::path::PathBuf::from(dir);
        if self.current.as_ref() == Some(&path) {
            return true;
        }

        if let Some(old) = self.current.take() {
            let _ = self.inner.unwatch(&old);
        }

        match self.inner.watch(&path, RecursiveMode::NonRecursive) {
            Ok(()) => {
                self.current = Some(path);
                true
            }
            Err(_) => false,
        }
    }

    /// Detaches from the current directory, if any.
    pub fn release(&mut self) {
        if let Some(old) = self.current.take() {
            let _ = self.inner.unwatch(&old);
        }
    }
}

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

    /// End-to-end check of the OS watcher. Tolerant by design: if the platform
    /// backend cannot be created the app falls back to polling, so the test
    /// simply returns instead of failing.
    #[test]
    fn native_watcher_reports_a_file_creation() {
        let dir = scratch_dir("native");
        let Ok(mut nw) = NativeWatcher::new() else {
            return;
        };
        assert!(
            nw.retarget(&dir.to_string_lossy()),
            "a temp directory must be watchable"
        );

        let baseline = nw.events.load(Ordering::Relaxed);
        std::fs::write(dir.join("created.txt"), b"x").unwrap();

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            if nw.events.load(Ordering::Relaxed) > baseline {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }

        assert!(
            nw.events.load(Ordering::Relaxed) > baseline,
            "creating a file must raise at least one native event"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn retargeting_to_the_same_directory_is_a_no_op() {
        let dir = scratch_dir("retarget");
        let Ok(mut nw) = NativeWatcher::new() else {
            return;
        };
        let path = dir.to_string_lossy().to_string();
        assert!(nw.retarget(&path));
        assert!(nw.retarget(&path), "re-targeting the same path must stay successful");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_directory_is_unknown_not_empty() {
        let mut p = std::env::temp_dir();
        p.push("90ftp_watch_definitely_missing_dir_xyz");
        assert_eq!(local_signature(&p.to_string_lossy()), 0);
    }
}
