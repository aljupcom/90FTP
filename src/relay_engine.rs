//! The file-moving half of the server-to-server dispatcher.
//!
//! This module owns three things that used to be tangled inside one long loop in
//! `remote_transfer`:
//!
//! * **What "progress" means.** The old loop counted a file twice — half while
//!   downloading, half while uploading — against a total that came from the
//!   directory index. That made the percentage an estimate of an estimate: it
//!   lagged behind reality and never recovered when a file turned out to be a
//!   different size than the index claimed (a symlink reports the length of its
//!   target *path*, not of the file). Progress is now measured in bytes that
//!   actually crossed the wire, against a total that is corrected as soon as the
//!   real size of a file is known.
//! * **Which failures are worth retrying.** A permission error was retried three
//!   times, each with a fresh connection, so a website full of unwritable files
//!   spent most of its time re-authenticating. Only transport failures are
//!   retried now.
//! * **How many files move at once.** One file at a time is fine for a few large
//!   ones and terrible for a website: every small file costs two round trips per
//!   stage, and the link sits idle in between. Files are spread over a small
//!   pool of workers, each with its own connections.

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::json;

use crate::app_state::ScriptCallback;
use crate::remote::RemoteClient;
use crate::remote_transfer::TransferItem;
use crate::transfer_control::{CANCELLED, TransferControl};

/// Files at or below this size are relayed through memory instead of a temporary
/// file. A website is thousands of files of a few kilobytes each, and writing
/// each one to disk and reading it back was pure overhead.
const MEMORY_RELAY_LIMIT: u64 = 4 * 1024 * 1024;

/// How often the progress line is allowed to reach the WebView. Every emit is a
/// cross-process call, so the value is a compromise between smoothness and cost.
const PROGRESS_INTERVAL_MS: u128 = 120;

/// How many files pass between two permanent log lines.
const LOG_EVERY_FILES: usize = 50;

/// Below this many files, opening extra connections costs more than it saves.
const PARALLEL_MIN_FILES: usize = 40;

/// The pool size. Four is deliberately conservative: FTP servers commonly cap
/// concurrent sessions, and the gain past four workers is small.
const DEFAULT_WORKERS: usize = 4;

/// Everything needed to open a connection again.
///
/// The relay starts with one connection per side, used to index the tree and to
/// probe the destination. Workers cannot share those (a session is stateful), so
/// they open their own — which means the parameters have to travel with the job.
#[derive(Debug, Clone)]
pub struct ConnectionSpec {
    pub protocol: String,
    pub host: String,
    pub port: u16,
    pub user: String,
    pub pass: String,
    pub auth_type: String,
    pub key_path: Option<String>,
    pub passphrase: Option<String>,
}

impl ConnectionSpec {
    pub fn connect(&self) -> Result<RemoteClient, String> {
        RemoteClient::connect(
            &self.protocol,
            &self.host,
            self.port,
            &self.user,
            &self.pass,
            &self.auth_type,
            self.key_path.as_deref(),
            self.passphrase.as_deref(),
            true,
        )
    }
}

// ---------------------------------------------------------------- emissions

pub fn send_log(exec_script: &Option<ScriptCallback>, msg: &str, kind: &str) {
    let Some(cb) = exec_script else { return };
    let payload = json!({ "text": crate::logger::redact(msg), "kind": kind });
    if let Ok(text) = serde_json::to_string(&payload) {
        cb(format!(
            "if(window.onRemoteTransferLog) window.onRemoteTransferLog({});",
            text
        ));
    }
}

/// A line that replaces the previous one instead of appending. Used for anything
/// that updates in place, so the log does not fill with near-identical rows.
pub fn send_status(exec_script: &Option<ScriptCallback>, msg: &str, kind: &str) {
    let Some(cb) = exec_script else { return };
    let payload = json!({
        "text": crate::logger::redact(msg),
        "kind": kind,
        "replace": true
    });
    if let Ok(text) = serde_json::to_string(&payload) {
        cb(format!(
            "if(window.onRemoteTransferLog) window.onRemoteTransferLog({});",
            text
        ));
    }
}

#[allow(clippy::too_many_arguments)]
pub fn send_progress(
    exec_script: &Option<ScriptCallback>,
    percent: f64,
    current_file: &str,
    done_files: usize,
    total_files: usize,
    done_bytes: u64,
    total_bytes: u64,
    speed: &str,
) {
    let Some(cb) = exec_script else { return };
    let payload = json!({
        "percent": percent.clamp(0.0, 100.0),
        "current_file": current_file,
        "done_files": done_files,
        "total_files": total_files,
        "done_bytes": done_bytes,
        "total_bytes": total_bytes,
        "speed": speed
    });
    if let Ok(text) = serde_json::to_string(&payload) {
        cb(format!(
            "if(window.onRemoteTransferProgress) window.onRemoteTransferProgress({});",
            text
        ));
    }
}

pub fn send_complete(
    exec_script: &Option<ScriptCallback>,
    success: bool,
    msg: &str,
    count: usize,
    bytes: u64,
    secs: f64,
) {
    let Some(cb) = exec_script else { return };
    let payload = json!({
        "success": success,
        "message": msg,
        "count": count,
        "bytes": bytes,
        "elapsed_secs": secs
    });
    if let Ok(text) = serde_json::to_string(&payload) {
        cb(format!(
            "if(window.onRemoteTransferComplete) window.onRemoteTransferComplete({});",
            text
        ));
    }
}

pub fn send_error(exec_script: &Option<ScriptCallback>, err: &str) {
    let Some(cb) = exec_script else { return };
    let payload = json!({ "error": crate::logger::redact(err) });
    if let Ok(text) = serde_json::to_string(&payload) {
        cb(format!(
            "if(window.onRemoteTransferError) window.onRemoteTransferError({});",
            text
        ));
    }
}

// ---------------------------------------------------------------- reporting

struct ReporterInner {
    exec_script: Option<ScriptCallback>,
    /// Raw bytes that crossed the wire, in-flight files included. A relayed file
    /// crosses twice.
    wire_done: AtomicU64,
    wire_total: AtomicU64,
    /// Bytes of files that were skipped because they already existed. They move
    /// nothing, but they still have to advance the bar.
    skipped_bytes: AtomicU64,
    /// Sum of the sizes that actually arrived, for the summary line.
    completed_bytes: AtomicU64,
    done_files: AtomicUsize,
    failed_files: AtomicUsize,
    total_files: usize,
    /// 2 for a relay (download + upload), 1 for a plain upload.
    factor: u64,
    started: Instant,
    last_emit: Mutex<Instant>,
    last_label: Mutex<String>,
}

/// Shared, cheap to clone, and safe to hand to a worker thread or a progress
/// callback.
#[derive(Clone)]
pub struct TransferReporter {
    inner: Arc<ReporterInner>,
}

impl TransferReporter {
    pub fn new(
        exec_script: &Option<ScriptCallback>,
        total_files: usize,
        indexed_bytes: u64,
        relay: bool,
    ) -> Self {
        let factor = if relay { 2 } else { 1 };
        Self {
            inner: Arc::new(ReporterInner {
                exec_script: exec_script.clone(),
                wire_done: AtomicU64::new(0),
                wire_total: AtomicU64::new(indexed_bytes.saturating_mul(factor).max(1)),
                skipped_bytes: AtomicU64::new(0),
                completed_bytes: AtomicU64::new(0),
                done_files: AtomicUsize::new(0),
                failed_files: AtomicUsize::new(0),
                total_files,
                factor,
                started: Instant::now(),
                // Start in the past so the first update renders immediately.
                last_emit: Mutex::new(Instant::now() - Duration::from_secs(1)),
                last_label: Mutex::new(String::new()),
            }),
        }
    }

    /// Records `n` bytes that just moved and, at most every
    /// [`PROGRESS_INTERVAL_MS`], pushes a new progress line.
    pub fn add_wire(&self, n: u64, label: &str) {
        if n == 0 {
            return;
        }
        self.inner.wire_done.fetch_add(n, Ordering::Relaxed);

        if let Ok(mut l) = self.inner.last_label.lock() {
            if l.as_str() != label {
                *l = label.to_string();
            }
        }
        self.render_if_due(label);
    }

    /// Books a completed file. `actual_size` is what really moved, which is not
    /// always what the index predicted.
    pub fn complete_file(&self, actual_size: u64, indexed_size: u64, label: &str) {
        self.inner
            .completed_bytes
            .fetch_add(actual_size, Ordering::Relaxed);
        self.inner.done_files.fetch_add(1, Ordering::Relaxed);
        self.adjust_total(actual_size as i64 - indexed_size as i64);
        self.render_if_due(label);
    }

    /// A file that was left alone because it already existed.
    ///
    /// It moves no bytes, so it must not touch `wire_done` — that counter feeds
    /// the displayed byte total, and inflating it would double-count the file
    /// once the in-flight term is added back. The bar advances through
    /// `skipped_bytes` instead.
    pub fn skip_file(&self, indexed_size: u64, label: &str) {
        self.inner
            .skipped_bytes
            .fetch_add(indexed_size, Ordering::Relaxed);
        self.inner.done_files.fetch_add(1, Ordering::Relaxed);
        self.render_if_due(label);
    }

    pub fn note_failure(&self) {
        self.inner.failed_files.fetch_add(1, Ordering::Relaxed);
    }

    /// Keeps the denominator honest when a file is bigger or smaller than the
    /// index said (symlinks, or a file that changed since it was listed).
    fn adjust_total(&self, delta_bytes: i64) {
        let delta = delta_bytes.saturating_mul(self.inner.factor as i64);
        if delta >= 0 {
            self.inner
                .wire_total
                .fetch_add(delta as u64, Ordering::Relaxed);
        } else {
            let _ = self.inner.wire_total.try_update(
                Ordering::Relaxed,
                Ordering::Relaxed,
                |v| Some(v.saturating_sub((-delta) as u64)),
            );
        }
    }

    fn render_if_due(&self, label: &str) {
        let Ok(mut last) = self.inner.last_emit.try_lock() else {
            // Another worker is already rendering; skipping one frame is better
            // than blocking an I/O loop on a lock.
            return;
        };
        if last.elapsed().as_millis() < PROGRESS_INTERVAL_MS {
            return;
        }
        *last = Instant::now();
        drop(last);
        self.render(label, false);
    }

    fn render(&self, label: &str, finished: bool) {
        let wire_done = self.inner.wire_done.load(Ordering::Relaxed);
        let skipped = self.inner.skipped_bytes.load(Ordering::Relaxed);
        let wire_total = self.inner.wire_total.load(Ordering::Relaxed).max(1);
        let factor = self.inner.factor;
        let elapsed = self.inner.started.elapsed().as_secs_f64();

        // Skipped files contribute their size to the denominator's progress even
        // though nothing moved for them.
        let credited = wire_done.saturating_add(skipped.saturating_mul(factor));
        let percent = if finished {
            100.0
        } else {
            (credited as f64 / wire_total as f64 * 100.0).clamp(0.0, 99.9)
        };

        let shown_bytes = wire_done / factor + skipped;
        let speed = if elapsed > 0.05 {
            (wire_done as f64 / factor as f64) / (1024.0 * 1024.0) / elapsed
        } else {
            0.0
        };

        send_progress(
            &self.inner.exec_script,
            percent,
            label,
            self.inner.done_files.load(Ordering::Relaxed),
            self.inner.total_files,
            shown_bytes,
            wire_total / factor,
            &format!("{:.2} MB/s", speed),
        );
    }

    pub fn finish(&self) {
        let label = self
            .inner
            .last_label
            .lock()
            .map(|l| l.clone())
            .unwrap_or_default();
        self.render(&label, true);
    }

    pub fn total_files(&self) -> usize {
        self.inner.total_files
    }

    pub fn done_files(&self) -> usize {
        self.inner.done_files.load(Ordering::Relaxed)
    }

    pub fn failed_files(&self) -> usize {
        self.inner.failed_files.load(Ordering::Relaxed)
    }

    pub fn file_bytes(&self) -> u64 {
        self.inner.completed_bytes.load(Ordering::Relaxed)
            + self.inner.skipped_bytes.load(Ordering::Relaxed)
    }

    pub fn elapsed_secs(&self) -> f64 {
        self.inner.started.elapsed().as_secs_f64()
    }
}

// ------------------------------------------------------------- error policy

/// Whether trying the same file again could plausibly succeed.
///
/// Retrying a refused permission or a missing file only multiplies the delay by
/// three and re-authenticates twice for nothing.
pub fn is_retryable(message: &str) -> bool {
    let m = message.to_lowercase();

    if m.contains(&CANCELLED.to_lowercase()) {
        return false;
    }

    const PERMANENT: [&str; 14] = [
        "permission denied",
        "access is denied",
        "access denied",
        "no such file",
        "not found",
        "does not exist",
        "not a directory",
        "authentication failed",
        "http 401",
        "http 403",
        "http 507",
        "read-only file system",
        "cannot change permissions",
        "not supported",
    ];
    !PERMANENT.iter().any(|needle| m.contains(needle))
}

/// Whether the failure looks like the connection died rather than the request
/// being rejected, in which case the worker should reconnect before retrying.
pub fn looks_like_a_dead_connection(message: &str) -> bool {
    let m = message.to_lowercase();
    [
        "broken pipe",
        "connection reset",
        "connection closed",
        "connection aborted",
        "connection refused",
        "unexpected eof",
        "timed out",
        "timeout",
        "channel closed",
        "session closed",
        "disconnected",
        "network",
    ]
    .iter()
    .any(|needle| m.contains(needle))
}

// ---------------------------------------------------------------- one file

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

fn unique_temp_path() -> std::path::PathBuf {
    let n = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("90ftp_relay_{}_{}.tmp", std::process::id(), n))
}

/// Wraps a progress callback so it reports the *delta* to the shared reporter.
///
/// `TransferControl` reports cumulative bytes per file; the reporter needs
/// increments, otherwise every callback would add the whole running total again.
fn wire_counter(
    reporter: &TransferReporter,
    label: &str,
    seen: Arc<AtomicU64>,
) -> Arc<dyn Fn(u64, u64) + Send + Sync> {
    let reporter = reporter.clone();
    let label = label.to_string();
    Arc::new(move |done, _total| {
        let previous = seen.swap(done, Ordering::Relaxed);
        if done > previous {
            reporter.add_wire(done - previous, &label);
        }
    })
}

/// Moves a single file, returning the number of bytes that actually arrived.
pub fn transfer_one_file(
    source: Option<&mut RemoteClient>,
    target: &mut RemoteClient,
    item: &TransferItem,
    target_path: &str,
    reporter: &TransferReporter,
    cancelled: &Arc<AtomicBool>,
) -> Result<u64, String> {
    let label = item.relative_path.clone();

    let Some(src) = source else {
        // Local file to remote target: one phase, no staging.
        let seen = Arc::new(AtomicU64::new(0));
        let control = TransferControl::with_progress(
            item.size,
            wire_counter(reporter, &label, seen.clone()),
        );

        let uploaded = target
            .upload_from_local_file_ctl(&item.source_full_path, target_path, &control)
            .map_err(|e| format!("Upload failed for {}: {}", target_path, e))?;

        let counted = seen.load(Ordering::Relaxed);
        if uploaded > counted {
            reporter.add_wire(uploaded - counted, &label);
        }
        return Ok(uploaded);
    };

    /* Small files skip the disk entirely. Writing a 2 KB file to a temp path,
       closing it, reopening it for the upload and deleting it is four syscalls
       and two copies for data that fits in a buffer. */
    if item.size > 0 && item.size <= MEMORY_RELAY_LIMIT {
        if cancelled.load(Ordering::Relaxed) {
            return Err(CANCELLED.to_string());
        }
        let data = src.download_file(&item.source_full_path)?;
        reporter.add_wire(data.len() as u64, &label);

        if cancelled.load(Ordering::Relaxed) {
            return Err(CANCELLED.to_string());
        }
        target.upload_file(target_path, &data)?;
        reporter.add_wire(data.len() as u64, &label);
        return Ok(data.len() as u64);
    }

    let temp_path = unique_temp_path();
    let temp_str = temp_path.to_string_lossy().to_string();

    let download = {
        let seen = Arc::new(AtomicU64::new(0));
        let control = TransferControl::with_progress(
            item.size,
            wire_counter(reporter, &label, seen.clone()),
        );
        src.download_to_local_file_ctl(&item.source_full_path, &temp_str, &control)
            .map(|bytes| (bytes, seen))
    };

    let (downloaded, seen) = match download {
        Ok(v) => v,
        Err(e) => {
            let _ = std::fs::remove_file(&temp_path);
            return Err(format!(
                "Download failed for {}: {}",
                item.source_full_path, e
            ));
        }
    };

    // The callback may have been throttled on the final chunk; count it now so
    // the totals are exact rather than nearly exact.
    let counted = seen.load(Ordering::Relaxed);
    if downloaded > counted {
        reporter.add_wire(downloaded - counted, &label);
    }

    let effective = if item.size > 0 { item.size } else { downloaded };
    let upload = {
        let seen = Arc::new(AtomicU64::new(0));
        let control = TransferControl::with_progress(
            effective,
            wire_counter(reporter, &label, seen.clone()),
        );
        target
            .upload_from_local_file_ctl(&temp_str, target_path, &control)
            .map(|bytes| (bytes, seen))
    };

    let _ = std::fs::remove_file(&temp_path);

    match upload {
        Ok((uploaded, seen)) => {
            let counted = seen.load(Ordering::Relaxed);
            if uploaded > counted {
                reporter.add_wire(uploaded - counted, &label);
            }
            Ok(uploaded.max(downloaded))
        }
        Err(e) => Err(format!("Upload failed for {}: {}", target_path, e)),
    }
}

// ---------------------------------------------------------------- the run

pub struct RunOutcome {
    pub done_files: usize,
    pub done_bytes: u64,
    pub failed_files: usize,
    pub cancelled: bool,
    pub failures: Vec<String>,
    pub elapsed_secs: f64,
}

pub struct Plan<'a> {
    pub items: &'a [TransferItem],
    pub dest_base: &'a str,
    pub source_is_remote: bool,
    pub overwrite_skip: bool,
}

fn target_path_for(dest_base: &str, item: &TransferItem) -> String {
    if dest_base == "." || dest_base.is_empty() || dest_base == "~" {
        item.relative_path.clone()
    } else {
        // `/` trims to an empty prefix, so the join still yields an absolute path.
        format!("{}/{}", dest_base.trim_end_matches('/'), item.relative_path)
    }
}

/// Creates every directory the transfer needs, parents first.
///
/// Doing this once up front is what makes the parallel pass safe: no worker can
/// upload into a collection another worker has not created yet.
fn create_directories(
    target: &mut RemoteClient,
    plan: &Plan,
    exec_script: &Option<ScriptCallback>,
    cancelled: &Arc<AtomicBool>,
) -> Result<(), String> {
    let mut created = 0usize;
    for item in plan.items.iter().filter(|i| i.is_dir) {
        if cancelled.load(Ordering::Relaxed) {
            return Err(CANCELLED.to_string());
        }
        let path = target_path_for(plan.dest_base, item);
        match target.make_dir_all(&path) {
            Ok(()) => created += 1,
            Err(e) => send_log(
                exec_script,
                &format!("Directory create note ({}): {}", path, e),
                "info",
            ),
        }
    }

    if created > 0 {
        send_log(
            exec_script,
            &format!("Prepared {} destination folders.", created),
            "info",
        );
    }
    Ok(())
}

fn worker_count(file_count: usize) -> usize {
    if file_count < PARALLEL_MIN_FILES {
        return 1;
    }
    match std::env::var("90FTP_RELAY_WORKERS")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
    {
        Some(n) => n.clamp(1, 16),
        None => DEFAULT_WORKERS.min(file_count),
    }
}

/// Runs the transfer, serially or over a worker pool.
#[allow(clippy::too_many_arguments)]
pub fn run(
    source: Option<RemoteClient>,
    source_spec: Option<&ConnectionSpec>,
    target: RemoteClient,
    target_spec: &ConnectionSpec,
    plan: &Plan,
    exec_script: &Option<ScriptCallback>,
    cancelled: &Arc<AtomicBool>,
    indexed_bytes: u64,
) -> RunOutcome {
    let mut target = target;
    let files: Vec<&TransferItem> = plan.items.iter().filter(|i| !i.is_dir).collect();
    let reporter = TransferReporter::new(
        exec_script,
        files.len(),
        indexed_bytes,
        plan.source_is_remote,
    );

    if let Err(e) = create_directories(&mut target, plan, exec_script, cancelled) {
        return RunOutcome {
            done_files: 0,
            done_bytes: 0,
            failed_files: 0,
            cancelled: true,
            failures: vec![e],
            elapsed_secs: reporter.elapsed_secs(),
        };
    }

    let mut workers = worker_count(files.len());
    // Extra connections are only possible when the caller handed us what it took
    // to open them.
    if plan.source_is_remote && source_spec.is_none() {
        workers = 1;
    }

    let failures = if workers > 1 {
        send_log(
            exec_script,
            &format!(
                "Moving {} files over {} parallel connections.",
                files.len(),
                workers
            ),
            "info",
        );
        run_parallel(
            source,
            source_spec,
            target,
            target_spec,
            &files,
            plan,
            &reporter,
            exec_script,
            cancelled,
            workers,
        )
    } else {
        run_serial(
            source,
            target,
            &files,
            plan,
            &reporter,
            exec_script,
            cancelled,
        );
        Vec::new()
    };

    reporter.finish();

    RunOutcome {
        done_files: reporter.done_files(),
        done_bytes: reporter.file_bytes(),
        failed_files: reporter.failed_files(),
        cancelled: cancelled.load(Ordering::Relaxed),
        failures,
        elapsed_secs: reporter.elapsed_secs(),
    }
}

fn run_serial(
    source: Option<RemoteClient>,
    mut target: RemoteClient,
    files: &[&TransferItem],
    plan: &Plan,
    reporter: &TransferReporter,
    exec_script: &Option<ScriptCallback>,
    cancelled: &Arc<AtomicBool>,
) {
    let mut source = source;

    for item in files {
        if cancelled.load(Ordering::Relaxed) {
            send_log(exec_script, "Transfer cancelled by user.", "warning");
            return;
        }

        let target_path = target_path_for(plan.dest_base, item);

        if plan.overwrite_skip {
            if let Ok(Some(_)) = target.stat(&target_path) {
                send_log(
                    exec_script,
                    &format!("↷ Skipped (already exists): {}", item.relative_path),
                    "info",
                );
                reporter.skip_file(item.size, &item.relative_path);
                continue;
            }
        }

        let mut last_error = String::new();
        let mut moved = 0u64;
        let mut ok = false;

        for attempt in 1..=3 {
            if cancelled.load(Ordering::Relaxed) {
                break;
            }
            if attempt > 1 {
                send_log(
                    exec_script,
                    &format!(
                        "↻ Retrying (attempt {}/3): {}",
                        attempt, item.relative_path
                    ),
                    "warning",
                );
                std::thread::sleep(Duration::from_millis(150 * attempt as u64));
            }

            match transfer_one_file(
                source.as_mut(),
                &mut target,
                item,
                &target_path,
                reporter,
                cancelled,
            ) {
                Ok(bytes) => {
                    moved = bytes;
                    ok = true;
                    break;
                }
                Err(e) => {
                    last_error = e;
                    if !is_retryable(&last_error) {
                        break;
                    }
                }
            }
        }

        if cancelled.load(Ordering::Relaxed) {
            send_log(exec_script, "Transfer cancelled by user.", "warning");
            return;
        }

        if ok {
            reporter.complete_file(moved, item.size, &item.relative_path);
            /* One permanent line per file would be 18k cross-process calls and
               18k DOM rows for a website. A line every 50 files keeps the log
               readable without hiding progress. */
            let done = reporter.done_files();
            if done % LOG_EVERY_FILES == 0 || done == reporter.total_files() {
                send_log(
                    exec_script,
                    &format!(
                        "✓ {} of {} files transferred ({})",
                        done,
                        reporter.total_files(),
                        crate::remote_transfer::format_size(reporter.file_bytes())
                    ),
                    "success",
                );
            }
        } else {
            reporter.note_failure();
            send_log(
                exec_script,
                &format!("✕ Failed to transfer {}: {}", item.relative_path, last_error),
                "error",
            );
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn run_parallel(
    owned_source: Option<RemoteClient>,
    source_spec: Option<&ConnectionSpec>,
    owned_target: RemoteClient,
    target_spec: &ConnectionSpec,
    files: &[&TransferItem],
    plan: &Plan,
    reporter: &TransferReporter,
    exec_script: &Option<ScriptCallback>,
    cancelled: &Arc<AtomicBool>,
    workers: usize,
) -> Vec<String> {
    let cursor = AtomicUsize::new(0);
    let failures: Mutex<Vec<String>> = Mutex::new(Vec::new());

    // Worker 0 inherits the connections that already indexed the tree, so the
    // pool costs `workers - 1` handshakes rather than `workers`.
    let mut slots: Vec<(Option<RemoteClient>, Option<RemoteClient>)> = Vec::new();
    slots.push((owned_source, Some(owned_target)));
    for _ in 1..workers {
        slots.push((None, None));
    }
    let slots = Mutex::new(slots);

    std::thread::scope(|scope| {
        for index in 0..workers {
            let cursor = &cursor;
            let failures = &failures;
            let slots = &slots;
            let reporter = reporter.clone();

            scope.spawn(move || {
                let inherited = slots
                    .lock()
                    .ok()
                    .and_then(|mut guard| {
                        guard
                            .get_mut(index)
                            .map(|slot| (slot.0.take(), slot.1.take()))
                    })
                    .unwrap_or((None, None));

                let mut source = inherited.0;
                let mut target = inherited.1;

                /* A worker that cannot open its own connections simply stops:
                   the cursor hands its remaining files to the others, so the
                   transfer finishes with fewer workers instead of failing. */
                if plan.source_is_remote && source.is_none() {
                    source = source_spec.and_then(|s| s.connect().ok());
                    if source.is_none() {
                        return;
                    }
                }
                if target.is_none() {
                    target = target_spec.connect().ok();
                    if target.is_none() {
                        return;
                    }
                }
                let Some(mut target) = target else { return };

                loop {
                    if cancelled.load(Ordering::Relaxed) {
                        return;
                    }
                    let Some(item) = files.get(cursor.fetch_add(1, Ordering::Relaxed)) else {
                        return;
                    };

                    let target_path = target_path_for(plan.dest_base, item);

                    if plan.overwrite_skip {
                        if let Ok(Some(_)) = target.stat(&target_path) {
                            reporter.skip_file(item.size, &item.relative_path);
                            continue;
                        }
                    }

                    let mut last_error = String::new();
                    let mut moved = 0u64;
                    let mut ok = false;

                    for attempt in 1..=3 {
                        if cancelled.load(Ordering::Relaxed) {
                            return;
                        }

                        match transfer_one_file(
                            source.as_mut(),
                            &mut target,
                            item,
                            &target_path,
                            &reporter,
                            cancelled,
                        ) {
                            Ok(bytes) => {
                                moved = bytes;
                                ok = true;
                                break;
                            }
                            Err(e) => {
                                last_error = e;
                                if !is_retryable(&last_error) {
                                    break;
                                }
                                if looks_like_a_dead_connection(&last_error) {
                                    if let Some(spec) = source_spec {
                                        if let Ok(fresh) = spec.connect() {
                                            source = Some(fresh);
                                        }
                                    }
                                    if let Ok(fresh) = target_spec.connect() {
                                        target = fresh;
                                    }
                                }
                                std::thread::sleep(Duration::from_millis(150 * attempt as u64));
                            }
                        }
                    }

                    if ok {
                        reporter.complete_file(moved, item.size, &item.relative_path);
                        let done = reporter.done_files();
                        if done % LOG_EVERY_FILES == 0 || done == reporter.total_files() {
                            send_log(
                                exec_script,
                                &format!(
                                    "✓ {} of {} files transferred ({})",
                                    done,
                                    reporter.total_files(),
                                    crate::remote_transfer::format_size(reporter.file_bytes())
                                ),
                                "success",
                            );
                        }
                    } else {
                        reporter.note_failure();
                        send_log(
                            exec_script,
                            &format!(
                                "✕ Failed to transfer {}: {}",
                                item.relative_path, last_error
                            ),
                            "error",
                        );
                        if let Ok(mut list) = failures.lock() {
                            if list.len() < 200 {
                                list.push(format!("{}: {}", item.relative_path, last_error));
                            }
                        }
                    }
                }
            });
        }
    });

    failures.into_inner().unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(path: &str, size: u64) -> TransferItem {
        TransferItem {
            relative_path: path.to_string(),
            is_dir: false,
            size,
            source_full_path: format!("/src/{}", path),
        }
    }

    #[test]
    fn permission_errors_are_not_retried() {
        assert!(!is_retryable(
            "Download failed: Permission denied (os error 13)"
        ));
        assert!(!is_retryable("Upload failed: HTTP 403"));
        assert!(!is_retryable("Upload failed: HTTP 401"));
        assert!(!is_retryable(
            "The parent collection of '/x' does not exist (HTTP 409)"
        ));
        assert!(!is_retryable("Transfer cancelled"));
    }

    #[test]
    fn transport_errors_are_retried() {
        assert!(is_retryable("Connection reset by peer"));
        assert!(is_retryable("write failed: Broken pipe"));
        assert!(is_retryable("operation timed out"));
    }

    #[test]
    fn dead_connections_are_told_apart_from_rejections() {
        assert!(looks_like_a_dead_connection("Connection reset by peer"));
        assert!(looks_like_a_dead_connection("session closed"));
        assert!(!looks_like_a_dead_connection("Permission denied"));
        assert!(!looks_like_a_dead_connection("HTTP 507"));
    }

    /// A cancelled transfer must never be reported as a retryable failure: the
    /// retry loop would restart the file the user just stopped.
    #[test]
    fn a_cancellation_is_never_retryable() {
        assert!(!is_retryable(CANCELLED));
        assert!(!is_retryable(&format!("Download failed: {}", CANCELLED)));
    }

    #[test]
    fn the_target_path_joins_without_doubling_separators() {
        let entry = item("site/css/app.css", 10);
        assert_eq!(
            target_path_for("/var/www/html/", &entry),
            "/var/www/html/site/css/app.css"
        );
        assert_eq!(target_path_for(".", &entry), "site/css/app.css");
        assert_eq!(target_path_for("/", &entry), "/site/css/app.css");
    }

    /// The percentage has to stay honest when a file turns out to be bigger than
    /// the index predicted, which is exactly what a symlink looks like.
    #[test]
    fn the_total_grows_when_a_file_is_larger_than_indexed() {
        // Indexed as 100 bytes over a relay: 200 wire bytes expected.
        let reporter = TransferReporter::new(&None, 1, 100, true);
        reporter.add_wire(100, "a");
        // The file really held 400 bytes, so the denominator grows by the
        // difference counted twice, not by the whole size.
        reporter.complete_file(400, 100, "a");

        let inner = &reporter.inner;
        assert_eq!(inner.wire_total.load(Ordering::Relaxed), 800);
        assert_eq!(reporter.file_bytes(), 400);
    }

    #[test]
    fn a_smaller_file_shrinks_the_total_without_underflowing() {
        let reporter = TransferReporter::new(&None, 1, 1000, true);
        reporter.complete_file(10, 1000, "a");
        assert_eq!(reporter.inner.wire_total.load(Ordering::Relaxed), 20);
    }

    #[test]
    fn skipped_files_still_advance_the_bar() {
        let reporter = TransferReporter::new(&None, 2, 200, true);
        reporter.skip_file(100, "a");
        reporter.complete_file(100, 100, "b");
        assert_eq!(reporter.done_files(), 2);
        assert_eq!(reporter.file_bytes(), 200);
        // A skipped file moves no bytes, so the wire counter must stay at zero:
        // the displayed total adds the skipped size back exactly once.
        assert_eq!(reporter.inner.wire_done.load(Ordering::Relaxed), 0);
        assert_eq!(reporter.inner.skipped_bytes.load(Ordering::Relaxed), 100);
    }

    #[test]
    fn the_worker_pool_stays_serial_for_small_jobs() {
        assert_eq!(worker_count(0), 1);
        assert_eq!(worker_count(PARALLEL_MIN_FILES - 1), 1);
        assert!(worker_count(PARALLEL_MIN_FILES * 2) > 1);
    }

    // ------------------------------------------------------- end to end

    /// Runs the real engine against a real WebDAV server: directory creation,
    /// path joining, uploads and the reporter's counters.
    #[test]
    fn uploads_a_local_folder_to_a_real_server() {
        use crate::webdav_client::test_server::TestServer;

        let Some(server) = TestServer::start() else {
            eprintln!("node is not available — skipping the relay integration test");
            return;
        };

        let source_dir = std::env::temp_dir().join(format!("90ftp_src_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&source_dir);
        std::fs::create_dir_all(source_dir.join("css")).expect("create the source tree");
        std::fs::write(source_dir.join("index.html"), b"<h1>hi</h1>").expect("write");
        std::fs::write(source_dir.join("css/app.css"), b"body{}").expect("write");

        let cancelled = Arc::new(AtomicBool::new(false));
        let items = crate::remote_transfer::collect_local_items(
            &[source_dir.to_string_lossy().to_string()],
            &cancelled,
        )
        .expect("index the local folder");
        let total: u64 = items.iter().filter(|i| !i.is_dir).map(|i| i.size).sum();

        let spec = ConnectionSpec {
            protocol: "webdav".to_string(),
            host: "127.0.0.1".to_string(),
            port: server.port,
            user: "user".to_string(),
            pass: "pass".to_string(),
            auth_type: "password".to_string(),
            key_path: None,
            passphrase: None,
        };

        let target = spec.connect().expect("connect to the target");
        let plan = Plan {
            items: &items,
            dest_base: "/site",
            source_is_remote: false,
            overwrite_skip: false,
        };

        let outcome = run(
            None, None, target, &spec, &plan, &None, &cancelled, total,
        );

        assert_eq!(outcome.failed_files, 0, "failures: {:?}", outcome.failures);
        assert_eq!(outcome.done_files, 2);
        // "<h1>hi</h1>" is 11 bytes, "body{}" is 6.
        assert_eq!(outcome.done_bytes, 17);
        assert!(!outcome.cancelled);

        let mut check = crate::webdav_client::WebdavClient::connect(
            &server.url(),
            "webdav",
            server.port,
            "user",
            "pass",
            false,
        )
        .expect("verify with a fresh client");

        // Copying a folder copies the folder itself, so the destination path is
        // the relative path the index produced — not a flattened file name.
        let html_item = items
            .iter()
            .find(|i| i.relative_path.ends_with("index.html"))
            .expect("the index must contain index.html");
        let expected_html_path = format!("/site/{}", html_item.relative_path);

        let html = check
            .download_file(&expected_html_path)
            .expect("the file must exist at the joined path");
        assert_eq!(html, b"<h1>hi</h1>");

        let listing = check.list(Some("/site")).expect("list the destination");
        let top_folder = html_item
            .relative_path
            .split('/')
            .next()
            .expect("the relative path has a first segment");
        assert!(
            listing.iter().any(|i| i.name == top_folder && i.is_dir),
            "the copied folder must exist: {:?}",
            listing.iter().map(|i| i.name.as_str()).collect::<Vec<_>>()
        );

        let inner = check
            .list(Some(&format!("/site/{}", top_folder)))
            .expect("list the copied folder");
        assert!(
            inner.iter().any(|i| i.name == "css" && i.is_dir),
            "the nested subfolder must have been created: {:?}",
            inner.iter().map(|i| i.name.as_str()).collect::<Vec<_>>()
        );

        let _ = std::fs::remove_dir_all(&source_dir);
    }

    /// The worker pool has to move every file exactly once, and a pool that
    /// fails to open its extra connections must still finish.
    #[test]
    fn the_worker_pool_moves_every_file_once() {
        use crate::webdav_client::test_server::TestServer;
        use crate::webdav_client::WebdavClient;

        let Some(server) = TestServer::start() else {
            eprintln!("node is not available — skipping the parallel relay test");
            return;
        };

        let source_dir = std::env::temp_dir().join(format!("90ftp_par_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&source_dir);
        std::fs::create_dir_all(&source_dir).expect("create the source tree");
        const FILE_COUNT: usize = 60;
        for n in 0..FILE_COUNT {
            std::fs::write(source_dir.join(format!("f{:03}.txt", n)), format!("{}", n))
                .expect("write");
        }

        let cancelled = Arc::new(AtomicBool::new(false));
        let items = crate::remote_transfer::collect_local_items(
            &[source_dir.to_string_lossy().to_string()],
            &cancelled,
        )
        .expect("index the local folder");
        let total: u64 = items.iter().filter(|i| !i.is_dir).map(|i| i.size).sum();

        let spec = ConnectionSpec {
            protocol: "webdav".to_string(),
            host: "127.0.0.1".to_string(),
            port: server.port,
            user: "user".to_string(),
            pass: "pass".to_string(),
            auth_type: "password".to_string(),
            key_path: None,
            passphrase: None,
        };

        assert!(
            worker_count(FILE_COUNT) > 1,
            "this test only proves something when the pool is used"
        );

        let target = spec.connect().expect("connect to the target");
        let plan = Plan {
            items: &items,
            dest_base: "/par",
            source_is_remote: false,
            overwrite_skip: false,
        };

        let outcome = run(None, None, target, &spec, &plan, &None, &cancelled, total);
        assert_eq!(outcome.failed_files, 0, "failures: {:?}", outcome.failures);
        assert_eq!(outcome.done_files, FILE_COUNT);

        let mut check = WebdavClient::connect(
            &server.url(),
            "webdav",
            server.port,
            "user",
            "pass",
            false,
        )
        .expect("verify with a fresh client");
        let folder = format!("/par/{}", items[0].relative_path.split('/').next().unwrap());
        let listing = check.list(Some(&folder)).expect("list the destination");
        assert_eq!(
            listing.iter().filter(|i| !i.is_dir).count(),
            FILE_COUNT,
            "every file must have arrived exactly once"
        );

        for n in [0usize, 17, FILE_COUNT - 1] {
            let body = check
                .download_file(&format!("{}/f{:03}.txt", folder, n))
                .unwrap_or_else(|e| panic!("f{:03}.txt is missing: {}", n, e));
            assert_eq!(body, format!("{}", n).as_bytes());
        }

        let _ = std::fs::remove_dir_all(&source_dir);
    }

    /// Server to server, through the in-memory fast path, with no source spec —
    /// which is also the proof that a missing spec degrades to a serial run
    /// instead of failing.
    #[test]
    fn relays_between_two_real_servers() {
        use crate::webdav_client::test_server::TestServer;
        use crate::webdav_client::WebdavClient;

        let (Some(source_server), Some(target_server)) =
            (TestServer::start(), TestServer::start())
        else {
            eprintln!("node is not available — skipping the relay integration test");
            return;
        };

        let mut source_client = WebdavClient::connect(
            &source_server.url(),
            "webdav",
            source_server.port,
            "user",
            "pass",
            false,
        )
        .expect("connect to the source");
        source_client.make_dir_all("/www/assets").expect("mkdir");
        source_client
            .upload_file("/www/assets/a.js", b"alert(1)")
            .expect("seed a file");
        source_client
            .upload_file("/www/assets/b.css", b"body{}")
            .expect("seed a file");

        let cancelled = Arc::new(AtomicBool::new(false));
        let mut source = RemoteClient::Webdav(source_client);
        let items = crate::remote_transfer::collect_remote_items(
            &mut source,
            &["/www".to_string()],
            &cancelled,
        )
        .expect("index the source tree");
        assert_eq!(items.iter().filter(|i| !i.is_dir).count(), 2);
        let total: u64 = items.iter().filter(|i| !i.is_dir).map(|i| i.size).sum();

        let spec = ConnectionSpec {
            protocol: "webdav".to_string(),
            host: "127.0.0.1".to_string(),
            port: target_server.port,
            user: "user".to_string(),
            pass: "pass".to_string(),
            auth_type: "password".to_string(),
            key_path: None,
            passphrase: None,
        };
        let target = spec.connect().expect("connect to the target");

        let plan = Plan {
            items: &items,
            dest_base: "/backup",
            source_is_remote: true,
            overwrite_skip: false,
        };

        let outcome = run(
            Some(source),
            None,
            target,
            &spec,
            &plan,
            &None,
            &cancelled,
            total,
        );

        assert_eq!(outcome.failed_files, 0, "failures: {:?}", outcome.failures);
        assert_eq!(outcome.done_files, 2);
        assert_eq!(outcome.done_bytes, total);

        let mut check = WebdavClient::connect(
            &target_server.url(),
            "webdav",
            target_server.port,
            "user",
            "pass",
            false,
        )
        .expect("verify with a fresh client");
        assert_eq!(
            check
                .download_file("/backup/www/assets/a.js")
                .expect("the relayed file must exist"),
            b"alert(1)"
        );
        assert_eq!(
            check
                .download_file("/backup/www/assets/b.css")
                .expect("the relayed file must exist"),
            b"body{}"
        );
    }
}
