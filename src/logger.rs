use std::fs::{create_dir_all, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

static LOG_MUTEX: Mutex<()> = Mutex::new(());
static WRITE_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Cap for a single log file. Beyond this the file is rotated to `.log.1`
/// (one generation is kept) so a long-running install cannot fill the disk.
const MAX_LOG_BYTES: u64 = 2 * 1024 * 1024;

fn get_app_dir() -> PathBuf {
    if let Some(appdata) = std::env::var_os("APPDATA") {
        let dir = PathBuf::from(appdata).join("90FTP").join("logs");
        let _ = create_dir_all(&dir);
        dir
    } else {
        PathBuf::from(".")
    }
}

pub fn get_log_file_path() -> PathBuf {
    get_app_dir().join("90ftp_errors.log")
}

pub fn init_logger() {
    let log_path = get_log_file_path();

    // Write session start marker
    log_info(&format!("=== 90FTP Session Started. Log target: {:?} ===", log_path));

    // Install global panic hook to capture crashes and unhandled panics
    std::panic::set_hook(Box::new(|panic_info| {
        let location = panic_info.location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "unknown location".to_string());

        let payload = if let Some(s) = panic_info.payload().downcast_ref::<&str>() {
            (*s).to_string()
        } else if let Some(s) = panic_info.payload().downcast_ref::<String>() {
            s.clone()
        } else {
            "Box<Any> panic payload".to_string()
        };

        let msg = format!("CRITICAL UNHANDLED PANIC at [{}]: {}\nBacktrace:\n{:?}", location, payload, std::backtrace::Backtrace::capture());
        log_error(&msg);
    }));
}

fn rotate_if_needed(path: &Path) {
    if let Ok(meta) = std::fs::metadata(path) {
        if meta.len() > MAX_LOG_BYTES {
            let mut backup = path.to_path_buf();
            backup.set_extension("log.1");
            let _ = std::fs::rename(path, backup);
        }
    }
}

pub fn log_message(level: &str, msg: &str) {
    // Never block here: the panic hook calls into this function, and a panic that
    // happens while the log lock is held used to freeze the whole process.
    let _guard = LOG_MUTEX.try_lock().ok();

    let now = chrono::Local::now();
    let timestamp = now.format("%Y-%m-%d %H:%M:%S%.3f").to_string();
    let line = format!("[{}] [{}] {}\n", timestamp, level, msg);

    // Rotate every so often instead of on every single line (avoids a stat call
    // per message while still bounding file growth).
    if WRITE_COUNTER.fetch_add(1, Ordering::Relaxed) % 128 == 0 {
        rotate_if_needed(&get_log_file_path());
    }

    // Write to appdata log
    let app_log_path = get_log_file_path();
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&app_log_path) {
        let _ = f.write_all(line.as_bytes());
    }

    // Mirror into the working directory for developer inspection, debug builds only.
    // In a release install the CWD is usually Program Files, where the write fails.
    #[cfg(debug_assertions)]
    {
        let local_path = PathBuf::from("90ftp_errors.log");
        if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&local_path) {
            let _ = f.write_all(line.as_bytes());
        }
    }

    eprintln!("[{}] {}", level, msg);
}

pub fn log_error(msg: &str) {
    log_message("ERROR", msg);
}

#[allow(dead_code)]
pub fn log_warn(msg: &str) {
    log_message("WARN", msg);
}

pub fn log_info(msg: &str) {
    log_message("INFO", msg);
}

pub fn read_recent_errors(max_lines: usize) -> Result<String, String> {
    let _guard = LOG_MUTEX.try_lock().ok();
    let app_log_path = get_log_file_path();
    let path_to_read = if app_log_path.exists() {
        app_log_path
    } else {
        PathBuf::from("90ftp_errors.log")
    };

    if !path_to_read.exists() {
        return Ok("No error logs recorded yet.".to_string());
    }

    let content = std::fs::read_to_string(&path_to_read)
        .map_err(|e| format!("Failed to read error log at {:?}: {}", path_to_read, e))?;

    let lines: Vec<&str> = content.lines().collect();
    if lines.len() <= max_lines {
        Ok(content)
    } else {
        let start = lines.len() - max_lines;
        Ok(lines[start..].join("\n"))
    }
}
