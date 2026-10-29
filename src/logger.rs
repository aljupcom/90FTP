use std::fs::{create_dir_all, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

static LOG_MUTEX: Mutex<()> = Mutex::new(());
static WRITE_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Live credentials seen during this session.
///
/// Every sink that prints text (the file log, the activity log, the remote
/// transfer log) funnels through [`redact`], so a password that accidentally
/// reaches a message — an echoed shell command, a server error, a debug
/// string — is masked instead of written to disk. Registration is cheap and
/// idempotent, and the list is bounded so a long session cannot grow it.
static SECRETS: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// Shortest value worth masking as a bare substring. Below this a password is
/// more likely to collide with ordinary words in a path or an error message
/// than to be a real leak, and masking those would make logs unreadable.
const MIN_BARE_SECRET_LEN: usize = 4;
const MAX_REGISTERED_SECRETS: usize = 64;

/// Remembers a credential so [`redact`] can mask it wherever it appears.
pub fn register_secret(value: &str) {
    let trimmed = value.trim();
    if trimmed.chars().count() < MIN_BARE_SECRET_LEN {
        return;
    }
    let mut list = match SECRETS.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    if list.iter().any(|s| s == trimmed) {
        return;
    }
    if list.len() >= MAX_REGISTERED_SECRETS {
        list.remove(0);
    }
    list.push(trimmed.to_string());
}

fn registered_secrets() -> Vec<String> {
    match SECRETS.lock() {
        Ok(g) => g.clone(),
        Err(poisoned) => poisoned.into_inner().clone(),
    }
}

/// Replaces `ranges` (byte offsets) with `***`, merging overlaps first.
fn apply_ranges(text: &str, mut ranges: Vec<(usize, usize)>) -> String {
    if ranges.is_empty() {
        return text.to_string();
    }
    ranges.sort_unstable();
    let mut merged: Vec<(usize, usize)> = Vec::with_capacity(ranges.len());
    for (start, end) in ranges {
        if let Some(last) = merged.last_mut() {
            if start <= last.1 {
                if end > last.1 {
                    last.1 = end;
                }
                continue;
            }
        }
        merged.push((start, end));
    }

    let mut out = String::with_capacity(text.len());
    let mut cursor = 0usize;
    for (start, end) in merged {
        if start > cursor && start <= text.len() {
            out.push_str(&text[cursor..start]);
        }
        out.push_str("***");
        cursor = end;
    }
    if cursor < text.len() {
        out.push_str(&text[cursor..]);
    }
    out
}

fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

fn is_value_terminator(b: u8) -> bool {
    matches!(
        b,
        b' ' | b'\t' | b'\n' | b'\r' | b'"' | b'\'' | b',' | b';' | b'}' | b')' | b'|' | b'&'
    )
}

fn mask_registered_secrets(text: &str) -> String {
    let mut secrets = registered_secrets();
    if secrets.is_empty() {
        return text.to_string();
    }
    // Longest first: a secret that contains another one must be masked whole.
    secrets.sort_by_key(|s| std::cmp::Reverse(s.len()));
    let mut out = text.to_string();
    for secret in secrets {
        if secret.chars().count() >= MIN_BARE_SECRET_LEN && out.contains(&secret) {
            out = out.replace(&secret, "***");
        }
    }
    out
}

/// Drops the body of every PEM private key block.
fn mask_pem_blocks(text: &str) -> String {
    if !text.contains("PRIVATE KEY") {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut inside = false;
    for line in text.split_inclusive('\n') {
        if line.contains("BEGIN") && line.contains("PRIVATE KEY") {
            inside = true;
            out.push_str("[private key redacted]\n");
            continue;
        }
        if inside {
            if line.contains("END") && line.contains("PRIVATE KEY") {
                inside = false;
            }
            continue;
        }
        out.push_str(line);
    }
    out
}

/// Masks the argument of `sshpass -p` / `sshpass -e` style invocations.
fn mask_sshpass(text: &str) -> String {
    if !text.contains("sshpass") {
        return text.to_string();
    }
    let lower = text.to_ascii_lowercase();
    let bytes = text.as_bytes();
    let lb = lower.as_bytes();
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    let mut from = 0usize;
    while let Some(rel) = lower[from..].find("sshpass") {
        let start = from + rel;
        let mut i = start + "sshpass".len();
        while i < lb.len() && (lb[i] == b' ' || lb[i] == b'\t') {
            i += 1;
        }
        if i + 1 < lb.len() && lb[i] == b'-' && (lb[i + 1] == b'p' || lb[i + 1] == b'e') {
            let mut j = i + 2;
            while j < lb.len() && (lb[j] == b' ' || lb[j] == b'\t') {
                j += 1;
            }
            if j < lb.len() && (lb[j] == b'"' || lb[j] == b'\'') {
                let quote = lb[j];
                j += 1;
                let value_start = j;
                let mut value_end = value_start;
                while value_end < bytes.len() && bytes[value_end] != quote {
                    value_end += 1;
                }
                if value_end > value_start {
                    ranges.push((value_start, value_end));
                }
            } else {
                let value_start = j;
                let mut value_end = value_start;
                while value_end < bytes.len() && !is_value_terminator(bytes[value_end]) {
                    value_end += 1;
                }
                if value_end > value_start {
                    ranges.push((value_start, value_end));
                }
            }
        }
        from = start + "sshpass".len();
    }
    apply_ranges(text, ranges)
}

/// Masks the password portion of `scheme://user:password@host`.
fn mask_url_credentials(text: &str) -> String {
    if !text.contains("://") {
        return text.to_string();
    }
    let b = text.as_bytes();
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    let mut i = 0usize;
    while i + 3 <= b.len() {
        if &b[i..i + 3] == b"://" {
            let mut j = i + 3;
            let mut colon: Option<usize> = None;
            let mut at: Option<usize> = None;
            while j < b.len() {
                match b[j] {
                    b'@' => {
                        at = Some(j);
                        break;
                    }
                    b'/' | b' ' | b'\t' | b'\n' | b'\r' | b'"' | b'\'' => break,
                    b':' => colon = Some(j),
                    _ => {}
                }
                j += 1;
            }
            if let (Some(at_pos), Some(colon_pos)) = (at, colon) {
                if at_pos > colon_pos + 1 {
                    ranges.push((colon_pos + 1, at_pos));
                }
            }
            i = j.max(i + 3);
        } else {
            i += 1;
        }
    }
    apply_ranges(text, ranges)
}

/// Masks `password=...`, `passphrase: ...`, `token=...` style pairs.
fn mask_key_value_pairs(text: &str) -> String {
    const KEYS: [&str; 7] = [
        "password",
        "passwd",
        "passphrase",
        "secret",
        "token",
        "apikey",
        "api_key",
    ];
    let lower = text.to_ascii_lowercase();
    let bytes = text.as_bytes();
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    for key in KEYS {
        let mut from = 0usize;
        while let Some(rel) = lower[from..].find(key) {
            let start = from + rel;
            let after_key = start + key.len();
            let boundary_ok = start == 0 || !is_ident_byte(bytes[start - 1]);
            from = after_key;
            if !boundary_ok {
                continue;
            }
            let mut i = after_key;
            while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b'\t') {
                i += 1;
            }
            // JSON style: "password": "..." — the key is quoted, so the closing
            // quote sits between the key and the separator.
            if i < bytes.len() && (bytes[i] == b'"' || bytes[i] == b'\'') {
                i += 1;
                while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b'\t') {
                    i += 1;
                }
            }
            if i >= bytes.len() || (bytes[i] != b'=' && bytes[i] != b':') {
                continue;
            }
            i += 1;
            while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b'\t') {
                i += 1;
            }
            if i < bytes.len() && (bytes[i] == b'"' || bytes[i] == b'\'') {
                let quote = bytes[i];
                i += 1;
                let value_start = i;
                let mut value_end = value_start;
                while value_end < bytes.len() && bytes[value_end] != quote {
                    value_end += 1;
                }
                if value_end > value_start {
                    ranges.push((value_start, value_end));
                }
            } else {
                let value_start = i;
                let mut value_end = value_start;
                while value_end < bytes.len() && !is_value_terminator(bytes[value_end]) {
                    value_end += 1;
                }
                if value_end > value_start {
                    ranges.push((value_start, value_end));
                }
            }
        }
    }
    apply_ranges(text, ranges)
}

/// Masks every credential this module knows how to recognise.
///
/// Call this on anything bound for a log sink. It is intentionally lossy:
/// over-masking a value that merely looks like a secret is acceptable, leaking
/// one is not.
pub fn redact(text: &str) -> String {
    if text.is_empty() {
        return String::new();
    }
    let mut out = mask_pem_blocks(text);
    out = mask_sshpass(&out);
    out = mask_url_credentials(&out);
    out = mask_key_value_pairs(&out);
    out = mask_registered_secrets(&out);
    out
}

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

    // Nothing reaches the file before the credentials are masked: this is the
    // single choke point every log line passes through.
    let msg = redact(msg);

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masks_key_value_credentials() {
        assert_eq!(redact("password=hunter2"), "password=***");
        assert_eq!(redact("passphrase: 'my secret'"), "passphrase: '***'");
        assert_eq!(redact("token: abc123;"), "token: ***;");
        assert_eq!(
            redact("{\"password\":\"p@ss\",\"host\":\"example.com\"}"),
            "{\"password\":\"***\",\"host\":\"example.com\"}"
        );
    }

    #[test]
    fn masks_url_credentials() {
        assert_eq!(
            redact("sftp://aljup:secret99@example.com:22/pub"),
            "sftp://aljup:***@example.com:22/pub"
        );
        // No password in the URL: nothing to mask.
        assert_eq!(
            redact("https://example.com/a:b"),
            "https://example.com/a:b"
        );
    }

    #[test]
    fn masks_sshpass_argument() {
        assert_eq!(
            redact("sshpass -p 'hunter2' scp -P 22 a b"),
            "sshpass -p '***' scp -P 22 a b"
        );
        assert_eq!(
            redact("sshpass -p hunter2 scp -P 22 a b"),
            "sshpass -p *** scp -P 22 a b"
        );
    }

    #[test]
    fn masks_pem_private_key_body() {
        let text = "before\n-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaC1rZXk\n-----END OPENSSH PRIVATE KEY-----\nafter";
        let out = redact(text);
        assert!(!out.contains("b3BlbnNzaC1rZXk"));
        assert!(out.contains("before"));
        assert!(out.contains("after"));
    }

    #[test]
    fn leaves_ordinary_paths_alone() {
        assert_eq!(
            redact("Transferred /var/www/html/index.php (12.4 KB)"),
            "Transferred /var/www/html/index.php (12.4 KB)"
        );
    }

    #[test]
    fn masks_registered_secret_anywhere() {
        register_secret("Zq7-unique-secret-value");
        let out = redact("server said: bad password Zq7-unique-secret-value for user");
        assert!(!out.contains("Zq7-unique-secret-value"));
        assert!(out.contains("***"));
    }
}
