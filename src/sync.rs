//! Two-way directory synchronisation planning.
//!
//! The planner is deliberately pure: it takes two snapshots (relative path →
//! size + modification time) and returns the list of actions a sync would
//! perform. Keeping it free of I/O means the whole decision table — including
//! the awkward cases (conflicts, extraneous files, one-sided updates) — can be
//! unit-tested without a server or a filesystem.
//!
//! `sync_plan` in `app_state` builds the snapshots and returns this plan to the
//! UI as a dry-run preview; `sync_apply` executes the accepted subset.

use std::collections::{BTreeMap, BTreeSet};

/// What a single path needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncAction {
    /// Exists only locally.
    UploadNew,
    /// Exists only remotely.
    DownloadNew,
    /// Exists on both sides, local copy is newer or larger.
    UploadUpdate,
    /// Exists on both sides, remote copy is newer.
    DownloadUpdate,
    /// Identical by the chosen comparison.
    Unchanged,
    /// Both sides changed in a way the direction does not resolve.
    Conflict,
    /// Present locally but absent remotely; only produced when deletions are on.
    DeleteLocal,
    /// Present remotely but absent locally; only produced when deletions are on.
    DeleteRemote,
}

impl SyncAction {
    pub fn as_str(self) -> &'static str {
        match self {
            SyncAction::UploadNew => "upload_new",
            SyncAction::DownloadNew => "download_new",
            SyncAction::UploadUpdate => "upload_update",
            SyncAction::DownloadUpdate => "download_update",
            SyncAction::Unchanged => "unchanged",
            SyncAction::Conflict => "conflict",
            SyncAction::DeleteLocal => "delete_local",
            SyncAction::DeleteRemote => "delete_remote",
        }
    }

    /// True when applying the plan would move bytes or delete something.
    pub fn is_effective(self) -> bool {
        !matches!(self, SyncAction::Unchanged)
    }
}

/// Metadata for one side of a path. `mtime` is seconds since the epoch; `None`
/// when the source could not supply one (some FTP servers omit it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileMeta {
    pub size: u64,
    pub mtime: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// Local is the source of truth: only uploads (plus optional local deletions).
    Upload,
    /// Remote is the source of truth: only downloads.
    Download,
    /// Newest side wins per path; ties become conflicts.
    Both,
}

impl Direction {
    pub fn parse(value: &str) -> Direction {
        match value.trim().to_ascii_lowercase().as_str() {
            "upload" | "push" | "local-to-remote" => Direction::Upload,
            "download" | "pull" | "remote-to-local" => Direction::Download,
            _ => Direction::Both,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct SyncOptions {
    pub direction: Direction,
    /// Also delete files that exist on only one side. Off by default: a dry-run
    /// that proposes deletions is surprising enough without it being implicit.
    pub delete_extraneous: bool,
    /// Treat equal size as equal even when timestamps differ. Useful on servers
    /// that do not preserve modification times.
    pub size_only: bool,
}

impl Default for SyncOptions {
    fn default() -> Self {
        Self {
            direction: Direction::Both,
            delete_extraneous: false,
            size_only: false,
        }
    }
}

#[derive(Debug, Clone)]
pub struct SyncEntry {
    pub rel_path: String,
    pub action: SyncAction,
    pub local: Option<FileMeta>,
    pub remote: Option<FileMeta>,
}

/// True only when both sides actually report a timestamp and agree. Two missing
/// timestamps are *not* evidence of equality — treating them as equal would let
/// a genuinely different file slip through as "unchanged".
fn same_timestamp(local: FileMeta, remote: FileMeta) -> bool {
    matches!((local.mtime, remote.mtime), (Some(a), Some(b)) if a == b)
}

/// Why an update was chosen, for the UI to explain itself.
fn newer_side(local: FileMeta, remote: FileMeta, opts: &SyncOptions) -> SyncAction {
    if local.size == remote.size && (opts.size_only || same_timestamp(local, remote)) {
        return SyncAction::Unchanged;
    }

    match (local.mtime, remote.mtime) {
        (Some(l), Some(r)) if l > r => SyncAction::UploadUpdate,
        (Some(l), Some(r)) if r > l => SyncAction::DownloadUpdate,
        // One side has no timestamp: fall back to size, then to direction.
        _ => {
            if local.size > remote.size {
                SyncAction::UploadUpdate
            } else if remote.size > local.size {
                SyncAction::DownloadUpdate
            } else {
                // Same size, different stamp, no usable ordering.
                SyncAction::Conflict
            }
        }
    }
}

/// Builds the action list for two snapshots.
///
/// Paths are relative and use `/` as the separator, so the same function serves
/// the local (Windows) and remote (POSIX) sides.
pub fn build_plan(
    local: &BTreeMap<String, FileMeta>,
    remote: &BTreeMap<String, FileMeta>,
    opts: SyncOptions,
) -> Vec<SyncEntry> {
    let mut paths: BTreeSet<&String> = BTreeSet::new();
    paths.extend(local.keys());
    paths.extend(remote.keys());

    let mut entries = Vec::with_capacity(paths.len());

    for rel in paths {
        let l = local.get(rel).copied();
        let r = remote.get(rel).copied();

        let action = match (l, r) {
            (Some(_), Some(_)) => {
                let (l, r) = (l.unwrap(), r.unwrap());
                if l.size == r.size && (opts.size_only || same_timestamp(l, r)) {
                    SyncAction::Unchanged
                } else {
                    match opts.direction {
                        Direction::Upload => SyncAction::UploadUpdate,
                        Direction::Download => SyncAction::DownloadUpdate,
                        Direction::Both => newer_side(l, r, &opts),
                    }
                }
            }
            (Some(_), None) => {
                if opts.delete_extraneous {
                    match opts.direction {
                        // Remote is the source of truth: the local extra goes away.
                        Direction::Download => SyncAction::DeleteLocal,
                        _ => SyncAction::UploadNew,
                    }
                } else {
                    SyncAction::UploadNew
                }
            }
            (None, Some(_)) => {
                if opts.delete_extraneous {
                    match opts.direction {
                        Direction::Upload => SyncAction::DeleteRemote,
                        _ => SyncAction::DownloadNew,
                    }
                } else {
                    SyncAction::DownloadNew
                }
            }
            (None, None) => continue,
        };

        entries.push(SyncEntry {
            rel_path: rel.clone(),
            action,
            local: l,
            remote: r,
        });
    }

    entries
}

/// Counts per action, for the summary line in the preview.
pub fn summarise(entries: &[SyncEntry]) -> BTreeMap<&'static str, usize> {
    let mut counts: BTreeMap<&'static str, usize> = BTreeMap::new();
    for entry in entries {
        if entry.action == SyncAction::Unchanged {
            continue;
        }
        *counts.entry(entry.action.as_str()).or_insert(0) += 1;
    }
    counts
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(size: u64, mtime: i64) -> FileMeta {
        FileMeta {
            size,
            mtime: Some(mtime),
        }
    }

    fn map(pairs: &[(&str, FileMeta)]) -> BTreeMap<String, FileMeta> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), *v))
            .collect()
    }

    fn opts(direction: Direction, delete: bool) -> SyncOptions {
        SyncOptions {
            direction,
            delete_extraneous: delete,
            size_only: false,
        }
    }

    #[test]
    fn identical_files_are_unchanged() {
        let local = map(&[("a.txt", meta(10, 100))]);
        let remote = map(&[("a.txt", meta(10, 100))]);
        let plan = build_plan(&local, &remote, opts(Direction::Both, false));
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].action, SyncAction::Unchanged);
        assert!(!plan[0].action.is_effective());
    }

    #[test]
    fn one_sided_files_are_new_transfers() {
        let local = map(&[("only_local.txt", meta(5, 100))]);
        let remote = map(&[("only_remote.txt", meta(7, 100))]);
        let plan = build_plan(&local, &remote, opts(Direction::Both, false));

        let by_path = |p: &str| plan.iter().find(|e| e.rel_path == p).unwrap().action;
        assert_eq!(by_path("only_local.txt"), SyncAction::UploadNew);
        assert_eq!(by_path("only_remote.txt"), SyncAction::DownloadNew);
    }

    #[test]
    fn direction_upload_never_downloads_an_update() {
        let local = map(&[("a.txt", meta(10, 100))]);
        let remote = map(&[("a.txt", meta(10, 999))]); // remote is newer
        let plan = build_plan(&local, &remote, opts(Direction::Upload, false));
        assert_eq!(plan[0].action, SyncAction::UploadUpdate);
    }

    #[test]
    fn direction_download_never_uploads_an_update() {
        let local = map(&[("a.txt", meta(10, 999))]); // local is newer
        let remote = map(&[("a.txt", meta(10, 100))]);
        let plan = build_plan(&local, &remote, opts(Direction::Download, false));
        assert_eq!(plan[0].action, SyncAction::DownloadUpdate);
    }

    #[test]
    fn newest_wins_in_both_directions() {
        let local = map(&[("newer_local.txt", meta(1, 200)), ("newer_remote.txt", meta(1, 100))]);
        let remote = map(&[("newer_local.txt", meta(1, 100)), ("newer_remote.txt", meta(1, 200))]);
        let plan = build_plan(&local, &remote, opts(Direction::Both, false));

        let by_path = |p: &str| plan.iter().find(|e| e.rel_path == p).unwrap().action;
        assert_eq!(by_path("newer_local.txt"), SyncAction::UploadUpdate);
        assert_eq!(by_path("newer_remote.txt"), SyncAction::DownloadUpdate);
    }

    #[test]
    fn equal_size_with_no_timestamps_is_a_conflict_not_a_guess() {
        let local = map(&[("a.bin", FileMeta { size: 4, mtime: None })]);
        let remote = map(&[("a.bin", FileMeta { size: 4, mtime: None })]);
        let plan = build_plan(&local, &remote, opts(Direction::Both, false));
        assert_eq!(plan[0].action, SyncAction::Conflict);
    }

    #[test]
    fn deletions_are_only_proposed_when_explicitly_enabled() {
        let local = map(&[("local_extra.txt", meta(1, 1))]);
        let remote = map(&[("remote_extra.txt", meta(1, 1))]);

        let without = build_plan(&local, &remote, opts(Direction::Both, false));
        assert!(without.iter().all(|e| !matches!(
            e.action,
            SyncAction::DeleteLocal | SyncAction::DeleteRemote
        )));

        let with = build_plan(&local, &remote, opts(Direction::Both, true));
        let by_path = |p: &str| with.iter().find(|e| e.rel_path == p).unwrap().action;
        assert_eq!(by_path("local_extra.txt"), SyncAction::UploadNew);
        assert_eq!(by_path("remote_extra.txt"), SyncAction::DownloadNew);
    }

    #[test]
    fn pull_with_deletions_removes_local_extras() {
        let local = map(&[("extra.txt", meta(1, 1))]);
        let remote = BTreeMap::new();
        let plan = build_plan(&local, &remote, opts(Direction::Download, true));
        assert_eq!(plan[0].action, SyncAction::DeleteLocal);
    }

    #[test]
    fn push_with_deletions_removes_remote_extras() {
        let local = BTreeMap::new();
        let remote = map(&[("extra.txt", meta(1, 1))]);
        let plan = build_plan(&local, &remote, opts(Direction::Upload, true));
        assert_eq!(plan[0].action, SyncAction::DeleteRemote);
    }

    #[test]
    fn nested_paths_are_compared_independently() {
        let local = map(&[("dir/a.txt", meta(1, 1)), ("dir/sub/b.txt", meta(2, 2))]);
        let remote = map(&[("dir/a.txt", meta(1, 1))]);
        let plan = build_plan(&local, &remote, opts(Direction::Both, false));

        let by_path = |p: &str| plan.iter().find(|e| e.rel_path == p).unwrap().action;
        assert_eq!(by_path("dir/a.txt"), SyncAction::Unchanged);
        assert_eq!(by_path("dir/sub/b.txt"), SyncAction::UploadNew);
    }

    #[test]
    fn summary_ignores_unchanged_entries() {
        let local = map(&[("a.txt", meta(1, 1)), ("b.txt", meta(2, 2))]);
        let remote = map(&[("a.txt", meta(1, 1))]);
        let plan = build_plan(&local, &remote, opts(Direction::Both, false));
        let counts = summarise(&plan);
        assert_eq!(counts.get("upload_new"), Some(&1));
        assert_eq!(counts.get("unchanged"), None);
    }

    #[test]
    fn size_only_mode_treats_matching_sizes_as_equal() {
        let local = map(&[("a.txt", meta(10, 100))]);
        let remote = map(&[("a.txt", meta(10, 500))]);
        let plan = build_plan(
            &local,
            &remote,
            SyncOptions {
                direction: Direction::Both,
                delete_extraneous: false,
                size_only: true,
            },
        );
        assert_eq!(plan[0].action, SyncAction::Unchanged);
    }
}
