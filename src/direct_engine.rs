//! Direct server-to-server transfer engine.
//!
//! `scp -r` is the obvious way to copy a tree between two servers, and it is
//! what this app used to run. For a GUI it has one fatal property: it prints
//! nothing at all until the copy is finished, so the progress bar sat at 0%
//! for the whole transfer and then jumped to 100%. There was no way to make
//! `scp` report bytes — it simply does not.
//!
//! This module replaces it with a pipeline whose byte counter is visible:
//!
//! ```text
//! tar -cf - -C <parent> <name> | dd bs=4M status=progress | ssh <target> 'tar -xf - -C <dest>'
//! ```
//!
//! `dd` writes `N bytes copied, X s, Y MB/s` to **stderr** once a second, which
//! is exactly the quantity a progress bar needs, and it never touches the
//! archive stream on stdout. `tar` preserves permissions, timestamps and
//! symlinks the way `scp -r` did, and the bytes travel from the source server
//! straight to the destination — they still never pass through this computer.
//!
//! Two shapes are produced:
//!
//! * **Stream** (compression off) — one pipeline, one pass, no temporary files.
//!   `dd` counts raw bytes, so `done / total` is an exact percentage.
//! * **Staged** (compression on) — the tree is packed into a temporary archive
//!   on the source, that single file is shipped, the destination unpacks it,
//!   and **only the temporary archive is deleted** at the end. The user's files
//!   are never touched. Shipping a known-size file is also what makes the
//!   percentage exact for the compressed case, where raw-byte accounting would
//!   overstate the progress.

use crate::remote_transfer::RemoteTransferTask;

/// Marker prefix for the progress protocol the generated script speaks.
pub const MARKER: &str = "@90FTP:";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Compression {
    None,
    Gzip,
    Zstd,
}

impl Compression {
    pub fn parse(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "gzip" | "gz" => Compression::Gzip,
            "zstd" | "zst" => Compression::Zstd,
            _ => Compression::None,
        }
    }

    pub fn suffix(self) -> &'static str {
        match self {
            Compression::None => "",
            Compression::Gzip => ".gz",
            Compression::Zstd => ".zst",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Compression::None => "no compression",
            Compression::Gzip => "gzip",
            Compression::Zstd => "zstd",
        }
    }

    /// `rsync -z` is gzip; there is no zstd transport flag, so anything other
    /// than "off" means `-z` on that engine.
    pub fn rsync_flag(self) -> &'static str {
        match self {
            Compression::None => "",
            _ => "-z",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Pack,
    Ship,
    Extract,
    Cleanup,
}

impl Phase {
    pub fn from_token(token: &str) -> Option<Self> {
        match token.trim().to_ascii_lowercase().as_str() {
            "pack" => Some(Phase::Pack),
            "ship" => Some(Phase::Ship),
            "extract" => Some(Phase::Extract),
            "cleanup" => Some(Phase::Cleanup),
            _ => None,
        }
    }
}

/// A ready-to-run remote command plus the safe version that may be logged.
#[derive(Debug, Clone)]
pub struct ShipPlan {
    pub command: String,
    pub display: String,
    /// True when a temporary archive is created on the source.
    pub staging: bool,
    pub compression: Compression,
    pub engine: &'static str,
}

/// Quotes a value for a POSIX shell, including embedded single quotes.
pub fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// Splits a source path into the directory `tar -C` should change to and the
/// member name it should archive. Relative paths are resolved against the
/// source session's working directory, because the remote shell starts in the
/// login directory, which is not necessarily the directory being browsed.
pub fn split_source(path: &str, cwd: &str) -> (String, String) {
    let mut full = path.trim().to_string();
    if full.is_empty() {
        full = ".".to_string();
    }
    if !full.starts_with('/') {
        let base = cwd.trim();
        let base = if base.is_empty() { "." } else { base };
        full = if base.ends_with('/') {
            format!("{}{}", base, full)
        } else {
            format!("{}/{}", base, full)
        };
    }

    let trimmed = full.trim_end_matches('/');
    let trimmed = if trimmed.is_empty() { "/" } else { trimmed };

    match trimmed.rfind('/') {
        Some(0) => ("/".to_string(), trimmed[1..].to_string()),
        Some(i) => (trimmed[..i].to_string(), trimmed[i + 1..].to_string()),
        None => (".".to_string(), trimmed.to_string()),
    }
}

fn nonce() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{:x}", nanos & 0xffff_ffff)
}

fn member_args(task: &RemoteTransferTask, source_cwd: &str) -> String {
    task.sources
        .iter()
        .map(|s| {
            let (parent, name) = split_source(s, source_cwd);
            format!("-C {} {}", shell_quote(&parent), shell_quote(&name))
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn normalized_dest(task: &RemoteTransferTask) -> String {
    let trimmed = task.target_path.trim();
    if trimmed.is_empty() || trimmed == "." {
        "./".to_string()
    } else if trimmed.ends_with('/') {
        trimmed.to_string()
    } else {
        format!("{}/", trimmed)
    }
}

/// `ssh` options shared by every direct command.
///
/// `accept-new` still trusts the key on first contact (there is no way to
/// pre-seed a remote-to-remote host key) but refuses a changed key, which is
/// what stops a silent MITM on re-connect.
///
/// When authenticating with a password via `sshpass` we MUST NOT pass
/// `-o BatchMode=yes`: OpenSSH disables password prompts completely in
/// BatchMode, so `scp`/`ssh` fail immediately with
/// `Permission denied (publickey,password)`. Disabling pubkey probing instead
/// makes `sshpass` receive the password prompt immediately.
pub fn ssh_options(has_password: bool) -> &'static str {
    if has_password {
        "-o StrictHostKeyChecking=accept-new -o PubkeyAuthentication=no -o PreferredAuthentications=password,keyboard-interactive -o ConnectTimeout=20"
    } else {
        "-o BatchMode=yes -o StrictHostKeyChecking=accept-new -o ConnectTimeout=20"
    }
}

/// `ssh [sshpass] <host> <remote command>`.
fn ssh_invocation(task: &RemoteTransferTask, has_password: bool, remote_cmd: &str) -> String {
    let target = shell_quote(&format!("{}@{}", task.target_user, task.target_host));
    let body = format!(
        "ssh -p {} {} {} {}",
        task.target_port,
        ssh_options(has_password),
        target,
        shell_quote(remote_cmd)
    );
    if has_password {
        format!("sshpass -f \"$__p\" {}", body)
    } else {
        body
    }
}

/// Opens the password file when one is needed. The password never goes on a
/// command line (`sshpass -p` exposes it to `ps` for every local user) and
/// never reaches the UI log.
fn password_preamble(task: &RemoteTransferTask) -> Option<String> {
    if task.target_pass.is_empty() {
        return None;
    }
    let escaped = task.target_pass.replace('\'', "'\\''");
    Some(format!(
        "umask 077\n__p=$(mktemp /tmp/.90ftp.XXXXXX) || exit 90\nprintf '%s' '{}' > \"$__p\"\n",
        escaped
    ))
}

pub fn build(task: &RemoteTransferTask, source_cwd: &str) -> ShipPlan {
    let compression = Compression::parse(&task.compression);
    if task.transfer_mode == "direct_rsync" {
        return build_rsync(task, compression);
    }
    build_stream(task, source_cwd, compression)
}

/// A plan that only packs the tree on the source server.
///
/// Used when the two servers cannot talk to each other but the user asked for
/// compression: the archive is built where the files already are, and this
/// computer then carries that single file instead of tens of thousands of them.
/// The archive is deliberately left behind (`KEEP`) so the caller can ship it.
pub fn build_pack_only(
    task: &RemoteTransferTask,
    source_cwd: &str,
    compression: Compression,
) -> Option<ShipPlan> {
    if compression == Compression::None {
        return None;
    }
    let (create_flag, _) = match compression {
        Compression::Gzip => ("-czf".to_string(), "-xzf".to_string()),
        Compression::Zstd => ("--zstd -cf".to_string(), "--zstd -xf".to_string()),
        Compression::None => return None,
    };
    let id = nonce();
    let local_archive = format!("/tmp/.90ftp-{}.tar{}", id, compression.suffix());
    let members = member_args(task, source_cwd);

    let command = format!(
        "__a={archive}\n\
__keep=\n\
trap '[ -n \"$__keep\" ] || rm -f \"$__a\"' EXIT INT TERM\n\
echo \"{m}PHASE pack\"\n\
( while [ -e \"$__a\" ]; do printf '{m}PACK %s\\n' \"$(wc -c < \"$__a\" 2>/dev/null || echo 0)\"; sleep 1; done ) & __w=$!\n\
tar {create_flag} \"$__a\" {members}\n\
__rc=$?\n\
kill \"$__w\" 2>/dev/null; wait \"$__w\" 2>/dev/null\n\
if [ $__rc -ne 0 ]; then echo \"{m}ERROR pack $__rc\"; exit $__rc; fi\n\
__sz=$(wc -c < \"$__a\")\n\
echo \"{m}ARCHIVE $__a $__sz\"\n\
__keep=1\n\
echo \"{m}KEEP $__a $__sz\"\n\
exit 0",
        archive = shell_quote(&local_archive),
        m = MARKER,
        create_flag = create_flag,
        members = members,
    );

    Some(ShipPlan {
        command,
        display: format!("tar {} <temp-archive> {}  (pack only)", create_flag, members),
        staging: true,
        compression,
        engine: "tar-pack",
    })
}

fn build_rsync(task: &RemoteTransferTask, compression: Compression) -> ShipPlan {
    let sources = task
        .sources
        .iter()
        .map(|s| shell_quote(s.trim()))
        .collect::<Vec<_>>()
        .join(" ");
    let destination = shell_quote(&format!(
        "{}@{}:{}",
        task.target_user,
        task.target_host,
        normalized_dest(task)
    ));
    let ssh_cmd = shell_quote(&format!(
        "ssh -p {} {}",
        task.target_port,
        ssh_options(!task.target_pass.is_empty())
    ));

    /* `--no-inc-recursive` makes rsync compute the whole file list before it
       starts, so `--info=progress2` reports a stable total. Without it the
       percentage climbs and then collapses as new files are discovered. */
    let rsync = format!(
        "rsync -a --info=progress2 --no-inc-recursive {} -e {} {} {}",
        compression.rsync_flag(),
        ssh_cmd,
        sources,
        destination
    )
    .replace("  ", " ");

    let has_password = !task.target_pass.is_empty();
    let display = if has_password {
        format!("sshpass -f <secure-temp-file> {}", rsync)
    } else {
        rsync.clone()
    };

    let command = match password_preamble(task) {
        Some(pre) => format!(
            "{}trap 'rm -f \"$__p\"' EXIT INT TERM\necho \"{}PHASE ship 0\"\n{}\n__rc=$?\necho \"{}DONE $__rc\"\nexit $__rc",
            pre, MARKER, rsync, MARKER
        ),
        None => format!("echo \"{}PHASE ship 0\"\n{}\n__rc=$?\necho \"{}DONE $__rc\"\nexit $__rc", MARKER, rsync, MARKER),
    };

    ShipPlan {
        command,
        display,
        staging: false,
        compression,
        engine: "rsync",
    }
}

fn build_stream(task: &RemoteTransferTask, source_cwd: &str, compression: Compression) -> ShipPlan {
    let has_password = !task.target_pass.is_empty();
    let dest = normalized_dest(task);
    let members = member_args(task, source_cwd);
    let preamble = password_preamble(task);

    if compression == Compression::None {
        let remote = format!("mkdir -p {} && tar -xf - -C {}", shell_quote(&dest), shell_quote(&dest));
        let transport = ssh_invocation(task, has_password, &remote);
        let pipeline = format!(
            "tar -cf - {} | dd bs=4M status=progress | {}",
            members, transport
        );

        let display_pipeline = format!(
            "tar -cf - {} | dd bs=4M status=progress | {}",
            members,
            ssh_invocation_display(task, has_password, "tar -xf - -C <dest>")
        );

        let command = match preamble {
            Some(pre) => format!(
                "{}trap 'rm -f \"$__p\"' EXIT INT TERM\necho \"{}PHASE ship 0\"\n{}\n__rc=$?\nrm -f \"$__p\"\necho \"{}DONE $__rc\"\nexit $__rc",
                pre, MARKER, pipeline, MARKER
            ),
            None => format!(
                "echo \"{}PHASE ship 0\"\n{}\n__rc=$?\necho \"{}DONE $__rc\"\nexit $__rc",
                MARKER, pipeline, MARKER
            ),
        };

        return ShipPlan {
            command,
            display: display_pipeline,
            staging: false,
            compression,
            engine: "tar-stream",
        };
    }

    // Staged: pack → ship → extract → delete the temporary archive only.
    let id = nonce();
    let local_archive = format!("/tmp/.90ftp-{}.tar{}", id, compression.suffix());
    let remote_archive = format!("/tmp/.90ftp-{}.tar{}", id, compression.suffix());

    /* gzip rides on tar's own `-z`; zstd needs the long form on both ends
       (`-xzstdf` is not a thing), and only if the server actually has it. */
    let (create_flag, extract_flag) = match compression {
        Compression::Gzip => ("-czf".to_string(), "-xzf".to_string()),
        Compression::Zstd => ("--zstd -cf".to_string(), "--zstd -xf".to_string()),
        Compression::None => ("-cf".to_string(), "-xf".to_string()),
    };

    let extract_remote = format!(
        "mkdir -p {dest} && tar {extract_flag} {arch} -C {dest}; __r=$?; rm -f {arch}; exit $__r",
        extract_flag = extract_flag,
        dest = shell_quote(&dest),
        arch = shell_quote(&remote_archive),
    );

    let transport = ssh_invocation(task, has_password, &extract_remote);
    let ship_remote = ssh_invocation(
        task,
        has_password,
        &format!("cat > {}", shell_quote(&remote_archive)),
    );

    let body = format!(
        "__a={archive}\n\
__keep=\n\
trap '[ -n \"$__keep\" ] || rm -f \"$__a\"; rm -f \"$__p\"' EXIT INT TERM\n\
echo \"{m}PHASE pack\"\n\
( while [ -e \"$__a\" ]; do printf '{m}PACK %s\\n' \"$(wc -c < \"$__a\" 2>/dev/null || echo 0)\"; sleep 1; done ) & __w=$!\n\
tar {create_flag} \"$__a\" {members}\n\
__rc=$?\n\
kill \"$__w\" 2>/dev/null; wait \"$__w\" 2>/dev/null\n\
if [ $__rc -ne 0 ]; then echo \"{m}ERROR pack $__rc\"; exit $__rc; fi\n\
__sz=$(wc -c < \"$__a\")\n\
echo \"{m}ARCHIVE $__a $__sz\"\n\
echo \"{m}PHASE ship $__sz\"\n\
dd if=\"$__a\" bs=4M status=progress | {ship}\n\
__rc=$?\n\
if [ $__rc -ne 0 ]; then __keep=1; echo \"{m}KEEP $__a $__sz\"; echo \"{m}ERROR ship $__rc\"; exit 92; fi\n\
echo \"{m}PHASE extract\"\n\
{transport}\n\
__rc=$?\n\
rm -f \"$__a\"\n\
echo \"{m}PHASE cleanup\"\n\
if [ $__rc -ne 0 ]; then echo \"{m}ERROR extract $__rc\"; fi\n\
echo \"{m}DONE $__rc\"\n\
exit $__rc",
        archive = shell_quote(&local_archive),
        m = MARKER,
        create_flag = create_flag,
        members = members,
        ship = ship_remote,
        transport = transport,
    );

    let command = match preamble {
        Some(pre) => format!("{}{}", pre, body),
        None => body,
    };

    let display = format!(
        "tar {} <temp-archive> {} && dd | ssh … 'tar -xf' && rm -f <temp-archive>",
        create_flag, members
    );

    ShipPlan {
        command,
        display,
        staging: true,
        compression,
        engine: "tar-staged",
    }
}

fn ssh_invocation_display(task: &RemoteTransferTask, has_password: bool, remote_cmd: &str) -> String {
    let prefix = if has_password { "sshpass -f <secure-temp-file> " } else { "" };
    format!(
        "{}ssh -p {} {} {}@{} {}",
        prefix,
        task.target_port,
        ssh_options(has_password),
        task.target_user,
        task.target_host,
        shell_quote(remote_cmd)
    )
}

/// Reads `@90FTP:KEEP <path> <size>` out of the command output.
///
/// The marker is printed only when the archive was built successfully and the
/// shipping step failed, which is the one case where the archive is still worth
/// reusing: the packing time is already spent and the file is complete.
pub fn kept_archive(output: &str) -> Option<(String, u64)> {
    let needle = format!("{}KEEP ", MARKER);
    let mut found = None;
    for line in output.split(['\n', '\r']) {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix(needle.as_str()) {
            let mut parts = rest.split_whitespace();
            if let Some(path) = parts.next() {
                let size = parts
                    .next()
                    .and_then(|s| s.parse::<u64>().ok())
                    .unwrap_or(0);
                found = Some((path.to_string(), size));
            }
        }
    }
    found
}

/// The `tar` flags needed to unpack an archive the engine built earlier, read
/// back from the file name so the two halves cannot drift apart.
pub fn extract_flag_for(path: &str) -> &'static str {
    if path.ends_with(".zst") {
        "--zstd -xf"
    } else if path.ends_with(".gz") {
        "-xzf"
    } else {
        "-xf"
    }
}

/// Extracts the byte count from a `dd status=progress` line.///
/// The format is `123456789 bytes (123 MB, 118 MiB) copied, 12.3 s, 10.0 MB/s`
/// and it is rewritten in place with `\r`, so a chunk may hold several updates.
pub fn parse_dd_bytes(chunk: &str) -> Option<u64> {
    let mut last = None;
    for line in chunk.split(['\n', '\r']) {
        let tokens: Vec<&str> = line.split_whitespace().collect();
        if tokens.len() >= 2 && tokens[1] == "bytes" {
            if let Ok(value) = tokens[0].replace(',', "").parse::<u64>() {
                last = Some(value);
            }
        }
    }
    last
}

/// Collects `@90FTP:` marker lines that may be split across chunks.
#[derive(Default)]
pub struct MarkerBuffer {
    carry: String,
}

impl MarkerBuffer {
    pub fn feed(&mut self, chunk: &str) -> Vec<String> {
        let mut buffer = std::mem::take(&mut self.carry);
        buffer.push_str(chunk);

        let mut out = Vec::new();
        loop {
            let Some(start) = buffer.find(MARKER) else {
                buffer.clear();
                break;
            };
            let rest = &buffer[start + MARKER.len()..];
            match rest.find('\n') {
                Some(end) => {
                    out.push(rest[..end].trim_end_matches('\r').to_string());
                    buffer = rest[end + 1..].to_string();
                }
                None => {
                    // Keep the incomplete marker for the next chunk, but never
                    // let the buffer grow without bound.
                    buffer = buffer[start..].to_string();
                    if buffer.len() > 256 {
                        buffer.clear();
                    }
                    break;
                }
            }
        }

        self.carry = buffer;
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(mode: &str, compression: &str) -> RemoteTransferTask {
        RemoteTransferTask {
            source_is_remote: true,
            sources: vec!["/var/www/site".to_string()],
            transfer_mode: mode.to_string(),
            compression: compression.to_string(),
            target_protocol: "sftp".to_string(),
            target_host: "example.com".to_string(),
            target_port: 22,
            target_user: "deploy".to_string(),
            target_pass: String::new(),
            target_auth_type: "key".to_string(),
            target_key_path: None,
            target_passphrase: None,
            target_path: "/backup".to_string(),
            recursive: true,
            overwrite: "overwrite".to_string(),
            workers: 0,
        }
    }

    #[test]
    fn shell_quote_escapes_single_quotes() {
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
    }

    #[test]
    fn split_source_separates_parent_and_name() {
        assert_eq!(
            split_source("/var/www/site", "/"),
            ("/var/www".to_string(), "site".to_string())
        );
        assert_eq!(
            split_source("/var/www/site/", "/"),
            ("/var/www".to_string(), "site".to_string())
        );
        assert_eq!(
            split_source("/site", "/"),
            ("/".to_string(), "site".to_string())
        );
    }

    #[test]
    fn split_source_resolves_relative_paths_against_the_session_cwd() {
        assert_eq!(
            split_source("site", "/var/www"),
            ("/var/www".to_string(), "site".to_string())
        );
        assert_eq!(
            split_source("site", "/"),
            ("/".to_string(), "site".to_string())
        );
    }

    #[test]
    fn stream_plan_pipes_tar_through_dd_and_reports_the_phase() {
        let plan = build(&task("direct_scp", "none"), "/var/www");
        assert!(!plan.staging);
        assert!(plan.command.contains("tar -cf - -C '/var/www' 'site'"));
        assert!(plan.command.contains("dd bs=4M status=progress"));
        assert!(plan.command.contains("tar -xf - -C"));
        assert!(plan.command.contains("@90FTP:PHASE ship"));
        // The password is absent, so no temporary credential file is created.
        assert!(!plan.command.contains("sshpass"));
    }

    #[test]
    fn staged_plan_creates_a_temporary_archive_and_deletes_only_that() {
        let plan = build(&task("direct_scp", "gzip"), "/var/www");
        assert!(plan.staging);
        assert!(plan.command.contains("tar -czf"));
        assert!(plan.command.contains("@90FTP:PHASE pack"));
        assert!(plan.command.contains("@90FTP:PHASE ship"));
        assert!(plan.command.contains("@90FTP:PHASE extract"));
        // The archive is removed from both ends; nothing removes the sources.
        assert!(plan.command.contains("rm -f \"$__a\""));
        assert!(plan.command.contains("/tmp/.90ftp-"));
        assert!(plan.command.matches("rm -f").count() >= 2);
        assert!(!plan.command.contains("rm -rf"));
        assert!(!plan.command.contains("/var/www/site'"));
    }

    #[test]
    fn password_never_appears_in_the_display_command() {
        let mut t = task("direct_scp", "none");
        t.target_pass = "s3cr3t".to_string();
        let plan = build(&t, "/var/www");
        assert!(!plan.display.contains("s3cr3t"));
        assert!(plan.command.contains("s3cr3t")); // written to the 0600 file only
        assert!(plan.command.contains("mktemp /tmp/.90ftp.XXXXXX"));
    }

    #[test]
    fn rsync_plan_uses_progress2_and_a_stable_total() {
        let plan = build(&task("direct_rsync", "gzip"), "/var/www");
        assert_eq!(plan.engine, "rsync");
        assert!(plan.command.contains("--info=progress2"));
        assert!(plan.command.contains("--no-inc-recursive"));
        assert!(plan.command.contains("-z"));
    }

    #[test]
    fn zstd_uses_the_long_flag_on_both_ends() {
        let plan = build(&task("direct_scp", "zstd"), "/var/www");
        assert!(plan.command.contains("tar --zstd -cf"));
        assert!(plan.command.contains("tar --zstd -xf"));
    }

    #[test]
    fn parses_dd_progress_updates() {
        let chunk = "  524288000 bytes (524 MB, 500 MiB) copied, 5.0 s, 105 MB/s";
        assert_eq!(parse_dd_bytes(chunk), Some(524_288_000));
        // Several in-place updates separated by \r, the last one wins.
        let chunk = "1048576 bytes (1.0 MB) copied, 0.1 s, 10 MB/s\r2097152 bytes (2.1 MB) copied, 0.2 s, 10 MB/s";
        assert_eq!(parse_dd_bytes(chunk), Some(2_097_152));
        assert_eq!(parse_dd_bytes("records in"), None);
    }

    #[test]
    fn a_failed_ship_keeps_the_archive_and_reports_where_it_is() {
        let plan = build(&task("direct_scp", "zstd"), "/var/www");
        // The trap must not delete the archive when the keep flag is set.
        assert!(plan.command.contains("__keep="));
        assert!(plan.command.contains("KEEP $__a $__sz"));
        assert!(plan.command.contains("exit 92"));
    }

    #[test]
    fn reads_the_kept_archive_out_of_the_output() {
        let output = "@90FTP:PHASE ship 4096\n\
ssh: connect to host example.com port 22: Connection timed out\n\
@90FTP:KEEP /tmp/.90ftp-abc.tar.zst 4096\n\
@90FTP:ERROR ship 255\n";
        assert_eq!(
            kept_archive(output),
            Some(("/tmp/.90ftp-abc.tar.zst".to_string(), 4096))
        );
        assert_eq!(kept_archive("nothing here"), None);
    }

    #[test]
    fn extract_flags_follow_the_archive_suffix() {
        assert_eq!(extract_flag_for("/tmp/a.tar.zst"), "--zstd -xf");
        assert_eq!(extract_flag_for("/tmp/a.tar.gz"), "-xzf");
        assert_eq!(extract_flag_for("/tmp/a.tar"), "-xf");
    }

    #[test]
    fn pack_only_leaves_the_archive_and_never_tries_to_ship_it() {
        let plan = build_pack_only(&task("direct_scp", "gzip"), "/var/www", Compression::Gzip)
            .expect("a gzip plan");
        assert!(plan.command.contains("tar -czf"));
        assert!(plan.command.contains("KEEP $__a $__sz"));
        // No transport at all: this plan stops after packing.
        assert!(!plan.command.contains("dd if="));
        assert!(!plan.command.contains("ssh "));
        assert_shell_parses(&plan.command);

        assert!(build_pack_only(&task("direct_scp", "none"), "/var/www", Compression::None).is_none());
    }

    #[test]
    fn marker_buffer_reassembles_markers_split_across_chunks() {        let mut buffer = MarkerBuffer::default();
        assert!(buffer.feed("@90FTP:PHA").is_empty());
        let events = buffer.feed("SE ship 4096\n@90FTP:DONE 0\n");
        assert_eq!(events, vec!["PHASE ship 4096".to_string(), "DONE 0".to_string()]);
    }

    /// Runs `sh -n` over the generated script so a quoting mistake is caught
    /// here instead of on a customer's server. Skipped when no POSIX shell is
    /// available (a plain Windows CI box).
    fn assert_shell_parses(script: &str) {
        use std::io::Write;
        let dir = std::env::temp_dir().join(format!("90ftp-syntax-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("script.sh");
        let mut file = match std::fs::File::create(&path) {
            Ok(f) => f,
            Err(_) => return,
        };
        let _ = file.write_all(script.as_bytes());
        drop(file);

        let result = std::process::Command::new("sh").arg("-n").arg(&path).output();
        let _ = std::fs::remove_dir_all(&dir);
        match result {
            Ok(output) => {
                assert!(
                    output.status.success(),
                    "generated script is not valid POSIX shell: {}\n---\n{}",
                    String::from_utf8_lossy(&output.stderr),
                    script
                );
            }
            Err(_) => {}
        }
    }

    #[test]
    fn every_generated_script_is_valid_shell() {
        for mode in ["auto", "direct_scp", "direct_rsync"] {
            for compression in ["none", "gzip", "zstd"] {
                for password in [false, true] {
                    let mut t = task(mode, compression);
                    if password {
                        t.target_pass = "p@ss'word".to_string();
                    }
                    let plan = build(&t, "/var/www");
                    if std::env::var("FTP90_DUMP").is_ok() {
                        eprintln!("\n=== {mode} / {compression} / password={password} ===\n{}\n", plan.command);
                    }
                    assert_shell_parses(&plan.command);
                }
            }
        }
    }
}
