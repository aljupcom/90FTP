//! WebDAV (RFC 4918) over HTTP/HTTPS.
//!
//! WebDAV is the only protocol here that has no "session": every operation is
//! one HTTP request against a URL. That removes the connection state FTP and
//! SFTP carry around, and it also removes their failure modes — no passive-mode
//! data channel, no AUTH TLS ordering, no SSH key exchange. What it adds instead
//! is XML: directory listings come back as a `multistatus` document whose
//! namespace prefixes are chosen by the server (`D:`, `d:`, `lp1:`, or none),
//! so everything below matches on local tag names only.
//!
//! The surface deliberately mirrors `SftpClient` and `FtpBackend` so
//! `RemoteClient` can dispatch to it without special cases.

use std::io::{Read, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use base64::Engine;
use ureq::Agent;
use ureq::http::{Request, header};

use crate::models::FileItem;
use crate::transfer_control::{CANCELLED, CHUNK_SIZE, TransferControl};

/// Which properties we ask for. Only three are needed, and asking for less is
/// meaningfully faster on large collections.
const PROPFIND_BODY: &str = concat!(
    r#"<?xml version="1.0" encoding="utf-8"?>"#,
    r#"<propfind xmlns="DAV:"><prop>"#,
    r#"<resourcetype/><getcontentlength/><getlastmodified/>"#,
    r#"</prop></propfind>"#
);

/// Files at or below this size are buffered before the PUT so the request
/// carries a real `Content-Length`. See [`WebdavClient::put_file`].
const BUFFERED_UPLOAD_LIMIT: u64 = 4 * 1024 * 1024;

/// Upper bound for the fallback used when a server refuses a chunked PUT.
/// Buffering beyond this would risk exhausting memory on a large media file.
const BUFFERED_FALLBACK_LIMIT: u64 = 256 * 1024 * 1024;

pub struct WebdavClient {
    agent: Agent,
    /// `scheme://authority`, with no trailing slash and no path.
    origin: String,
    /// Server-side prefix the user pointed at, e.g. `/dav`. Empty when the
    /// endpoint is the server root.
    root: String,
    /// Current collection, always an absolute server path including `root`.
    current_dir: String,
    auth: String,
    host: String,
    username: String,
    secure: bool,
}

impl WebdavClient {
    /// Connects and verifies the endpoint speaks WebDAV.
    ///
    /// `host` accepts a bare host, `host:port`, or a full URL such as
    /// `https://nas.local:5006/dav`. Users copy WebDAV addresses as URLs, so
    /// rejecting the URL form would mean asking them to split it by hand.
    pub fn connect(
        host: &str,
        protocol: &str,
        port: u16,
        user: &str,
        pass: &str,
        trust_invalid_cert: bool,
    ) -> Result<Self, String> {
        let raw = host.trim();
        if raw.is_empty() {
            return Err(
                "WebDAV address is empty. Enter a host name, or a full URL such as https://nas.local:5006/dav"
                    .to_string(),
            );
        }

        let proto = protocol.trim().to_ascii_lowercase();
        let tls_by_default = matches!(proto.as_str(), "webdavs" | "davs" | "https");

        let (scheme, rest) = if let Some(r) = strip_prefix_ci(raw, "https://") {
            ("https", r)
        } else if let Some(r) = strip_prefix_ci(raw, "http://") {
            ("http", r)
        } else if tls_by_default {
            ("https", raw)
        } else {
            ("http", raw)
        };

        let (authority_raw, path_raw) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, ""),
        };
        if authority_raw.trim().is_empty() {
            return Err(format!("'{}' has no host name.", raw));
        }

        let default_port: u16 = if scheme == "https" { 443 } else { 80 };
        let authority = if authority_has_port(authority_raw) {
            authority_raw.to_string()
        } else {
            let chosen = if port != 0 { port } else { default_port };
            if chosen == default_port {
                authority_raw.to_string()
            } else {
                format!("{}:{}", authority_raw, chosen)
            }
        };

        let root = path_raw.trim_end_matches('/').to_string();
        let current_dir = if root.is_empty() {
            "/".to_string()
        } else {
            root.clone()
        };

        let mut config = Agent::config_builder()
            // 404 and 405 are answers, not transport failures: the caller needs
            // to tell "missing" from "broken" and cannot do that through an Err.
            .http_status_as_error(false)
            // PROPFIND, MKCOL and MOVE are not in the http crate's method table.
            .allow_non_standard_methods(true)
            .timeout_global(Some(Duration::from_secs(120)));

        if trust_invalid_cert {
            config = config.tls_config(
                ureq::tls::TlsConfig::builder()
                    .disable_verification(true)
                    .build(),
            );
        }

        let agent: Agent = config.build().into();

        let mut client = Self {
            agent,
            origin: format!("{}://{}", scheme, authority),
            root,
            current_dir,
            auth: format!(
                "Basic {}",
                base64::engine::general_purpose::STANDARD.encode(format!("{}:{}", user, pass))
            ),
            host: authority_raw.to_string(),
            username: user.to_string(),
            secure: scheme == "https",
        };

        // A wrong URL or a server that is not WebDAV both surface here rather
        // than as a confusing failure on the first file.
        client.ping().map_err(|e| {
            format!(
                "WebDAV endpoint {} could not be used: {}",
                client.origin, e
            )
        })?;

        Ok(client)
    }

    pub fn current_dir(&self) -> &str {
        &self.current_dir
    }

    /// Kept for parity with `FtpBackend`, whose implementations expose the same
    /// accessors even where nothing in the UI reads them yet.
    #[allow(dead_code)]
    pub fn host(&self) -> &str {
        &self.host
    }

    #[allow(dead_code)]
    pub fn username(&self) -> &str {
        &self.username
    }

    pub fn is_secure(&self) -> bool {
        self.secure
    }

    // ---------------------------------------------------------------- paths

    /// Absolute server path for `path`, resolved against the current
    /// collection and clamped so `..` can never climb above the configured root.
    fn resolve(&self, path: &str) -> String {
        let p = path.trim().replace('\\', "/");
        let joined = if p.starts_with('/') {
            p
        } else if p.is_empty() || p == "." || p == "~" {
            self.current_dir.clone()
        } else {
            format!("{}/{}", self.current_dir.trim_end_matches('/'), p)
        };
        normalize_path(&joined, &self.root)
    }

    fn url_for(&self, path: &str) -> String {
        format!("{}{}", self.origin, encode_path(path))
    }

    // ------------------------------------------------------------- requests

    fn send(
        &self,
        method: &str,
        path: &str,
        extra: &[(&str, &str)],
        body: Option<&str>,
    ) -> Result<(u16, String), String> {
        let url = self.url_for(path);
        let mut builder = Request::builder()
            .method(method)
            .uri(&url)
            .header(header::AUTHORIZATION, &self.auth);

        if body.is_some() {
            builder = builder.header(header::CONTENT_TYPE, "application/xml; charset=utf-8");
        }
        for (k, v) in extra {
            builder = builder.header(*k, *v);
        }

        let request = builder
            .body(body.unwrap_or("").as_bytes())
            .map_err(|e| format!("Could not build the {} request for {}: {}", method, url, e))?;

        let mut response = self
            .agent
            .run(request)
            .map_err(|e| format!("{} {} failed: {}", method, path, describe_transport_error(&e)))?;

        let status = response.status().as_u16();
        // Every method routed through here answers with a small XML document,
        // and the body carries the reason for a 207 or a 4xx. The cap only
        // guards against a server that streams something enormous by mistake.
        let text = response
            .body_mut()
            .with_config()
            .limit(64 * 1024)
            .read_to_string()
            .unwrap_or_default();

        Ok((status, text))
    }

    // ------------------------------------------------------------ operations

    pub fn ping(&mut self) -> Result<(), String> {
        let dir = self.current_dir.clone();
        let (status, body) = self.send("PROPFIND", &dir, &[("Depth", "0")], Some(PROPFIND_BODY))?;
        match status {
            200 | 207 => Ok(()),
            401 => Err(format!(
                "the server rejected the username or password (HTTP 401){}",
                auth_hint(&body)
            )),
            403 => Err("the account is authenticated but not allowed to read this collection (HTTP 403)".to_string()),
            404 => Err(format!("the collection '{}' does not exist (HTTP 404)", dir)),
            405 => Err("the server does not allow PROPFIND here, so it is not a WebDAV endpoint (HTTP 405)".to_string()),
            301 | 302 => Err(format!(
                "the server redirected the request (HTTP {}). Use the final URL, including https:// if the redirect points there.",
                status
            )),
            other => Err(format!(
                "unexpected answer HTTP {}{}",
                other,
                body_excerpt(&body)
            )),
        }
    }

    pub fn list(&mut self, path: Option<&str>) -> Result<Vec<FileItem>, String> {
        let dir = match path {
            Some(p) => self.resolve(p),
            None => self.current_dir.clone(),
        };

        let (status, body) = self.send("PROPFIND", &dir, &[("Depth", "1")], Some(PROPFIND_BODY))?;
        match status {
            207 => {}
            404 => return Err(format!("Directory '{}' does not exist on the server.", dir)),
            401 => return Err("Authentication failed (HTTP 401) — check the username and password.".to_string()),
            403 => return Err(format!("Access to '{}' is forbidden (HTTP 403).", dir)),
            other => {
                return Err(format!(
                    "Could not list '{}': HTTP {}{}",
                    dir,
                    other,
                    body_excerpt(&body)
                ));
            }
        }

        let entries = parse_multistatus(&body);
        if entries.is_empty() {
            return Err(format!(
                "'{}' returned no WebDAV entries. The address may point at a file instead of a collection.",
                dir
            ));
        }

        let mut items = Vec::new();
        for entry in entries {
            let entry_path = href_to_path(&entry.href);
            if entry_path.is_empty() {
                continue;
            }
            // Depth: 1 always includes the collection itself as the first
            // response; listing it as a child of itself would produce a `..`
            // that goes nowhere.
            if same_collection(&entry_path, &dir) {
                continue;
            }

            let name = entry_path
                .rsplit('/')
                .find(|s| !s.is_empty())
                .unwrap_or(&entry_path)
                .to_string();
            if name.is_empty() || name == "." || name == ".." {
                continue;
            }

            let is_dir = entry.is_collection;
            items.push(FileItem {
                name,
                path: entry_path,
                is_dir,
                size: if is_dir { 0 } else { entry.size },
                modified: format_http_date(&entry.modified),
                // WebDAV exposes no POSIX mode. Showing a plausible default is
                // better than an empty column, and the chmod action refuses
                // rather than pretending to change anything.
                permissions: if is_dir {
                    "drwxr-xr-x".to_string()
                } else {
                    "-rw-r--r--".to_string()
                },
                is_symlink: false,
            });
        }

        items.sort_by(|a, b| match (a.is_dir, b.is_dir) {
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            _ => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
        });

        Ok(items)
    }

    pub fn change_dir(&mut self, path: &str) -> Result<String, String> {
        let target = self.resolve(path);
        let item = self.stat(&target)?;
        match item {
            Some(i) if i.is_dir => {
                self.current_dir = target.clone();
                Ok(target)
            }
            Some(_) => Err(format!("'{}' is a file, not a directory.", target)),
            None => Err(format!("Directory '{}' does not exist.", target)),
        }
    }

    pub fn stat(&mut self, path: &str) -> Result<Option<FileItem>, String> {
        let target = self.resolve(path);
        let (status, body) = self.send("PROPFIND", &target, &[("Depth", "0")], Some(PROPFIND_BODY))?;

        match status {
            200 | 207 => {
                let entries = parse_multistatus(&body);
                let Some(entry) = entries.first() else {
                    return Ok(None);
                };
                let name = target
                    .rsplit('/')
                    .find(|s| !s.is_empty())
                    .unwrap_or("/")
                    .to_string();
                Ok(Some(FileItem {
                    name,
                    path: target,
                    is_dir: entry.is_collection,
                    size: if entry.is_collection { 0 } else { entry.size },
                    modified: format_http_date(&entry.modified),
                    permissions: if entry.is_collection {
                        "drwxr-xr-x".to_string()
                    } else {
                        "-rw-r--r--".to_string()
                    },
                    is_symlink: false,
                }))
            }
            404 => Ok(None),
            401 => Err("Authentication failed (HTTP 401) — check the username and password.".to_string()),
            other => Err(format!("Could not read '{}': HTTP {}", target, other)),
        }
    }

    pub fn make_dir(&mut self, path: &str) -> Result<(), String> {
        let target = self.resolve(path);
        let (status, body) = self.send("MKCOL", &target, &[], None)?;
        match status {
            // 405 is how every server says "already there"; a directory that
            // exists is the desired end state, so it is not an error.
            201 | 200 | 405 => Ok(()),
            401 => Err("Authentication failed (HTTP 401).".to_string()),
            403 => Err(format!("Not allowed to create '{}' (HTTP 403).", target)),
            409 => Err(format!(
                "The parent collection of '{}' does not exist (HTTP 409).",
                target
            )),
            other => Err(format!(
                "Could not create '{}': HTTP {}{}",
                target,
                other,
                body_excerpt(&body)
            )),
        }
    }

    pub fn make_dir_all(&mut self, dir_path: &str) -> Result<(), String> {
        let target = self.resolve(dir_path);
        if target == "/" || target.is_empty() {
            return Ok(());
        }

        let mut current = String::new();
        for segment in target.split('/').filter(|s| !s.is_empty()) {
            current.push('/');
            current.push_str(segment);
            self.make_dir(&current)?;
        }
        Ok(())
    }

    pub fn remove_file(&mut self, path: &str) -> Result<(), String> {
        self.delete(path, false)
    }

    pub fn remove_dir(&mut self, path: &str) -> Result<(), String> {
        self.delete(path, true)
    }

    fn delete(&mut self, path: &str, is_dir: bool) -> Result<(), String> {
        let target = self.resolve(path);
        let depth = [("Depth", "infinity")];
        let extra: &[(&str, &str)] = if is_dir { &depth } else { &[] };
        let (status, body) = self.send("DELETE", &target, extra, None)?;
        match status {
            200 | 204 | 404 => Ok(()),
            207 => Err(format!(
                "The server deleted only part of '{}'.{}",
                target,
                body_excerpt(&body)
            )),
            401 => Err("Authentication failed (HTTP 401).".to_string()),
            403 => Err(format!("Not allowed to delete '{}' (HTTP 403).", target)),
            423 => Err(format!("'{}' is locked by another user (HTTP 423).", target)),
            other => Err(format!("Could not delete '{}': HTTP {}", target, other)),
        }
    }

    pub fn rename(&mut self, from: &str, to: &str) -> Result<(), String> {
        let source = self.resolve(from);
        let destination = self.resolve(to);
        if source == destination {
            return Ok(());
        }

        let destination_url = self.url_for(&destination);
        let (status, body) = self.send(
            "MOVE",
            &source,
            &[("Destination", &destination_url), ("Overwrite", "T")],
            None,
        )?;

        match status {
            201 | 204 | 200 => Ok(()),
            401 => Err("Authentication failed (HTTP 401).".to_string()),
            403 => Err(format!("Not allowed to move '{}' (HTTP 403).", source)),
            409 => Err(format!(
                "The destination collection for '{}' does not exist (HTTP 409).",
                destination
            )),
            412 => Err(format!(
                "The server refused to overwrite '{}' (HTTP 412).",
                destination
            )),
            423 => Err(format!("'{}' is locked (HTTP 423).", source)),
            other => Err(format!(
                "Could not move '{}': HTTP {}{}",
                source,
                other,
                body_excerpt(&body)
            )),
        }
    }

    /// WebDAV has no permission model in the base specification.
    pub fn chmod(&mut self, _mode: u32, path: &str) -> Result<(), String> {
        Err(format!(
            "WebDAV cannot change permissions (requested for '{}'). Use SFTP or FTP for that.",
            path
        ))
    }

    // ------------------------------------------------------------ data path

    pub fn download_file(&mut self, remote_file: &str) -> Result<Vec<u8>, String> {
        let target = self.resolve(remote_file);
        let url = self.url_for(&target);
        let mut response = self
            .agent
            .get(&url)
            .header(header::AUTHORIZATION, &self.auth)
            .call()
            .map_err(|e| format!("GET {} failed: {}", target, describe_transport_error(&e)))?;

        let status = response.status().as_u16();
        if status == 404 {
            return Err(format!("'{}' does not exist on the server.", target));
        }
        if status == 401 {
            return Err("Authentication failed (HTTP 401).".to_string());
        }
        if !(200..300).contains(&status) {
            return Err(format!("Could not download '{}': HTTP {}", target, status));
        }

        let mut data = Vec::new();
        response
            .body_mut()
            .as_reader()
            .read_to_end(&mut data)
            .map_err(|e| format!("Could not read '{}': {}", target, e))?;
        Ok(data)
    }

    pub fn download_to_local_file_ctl(
        &mut self,
        remote_file: &str,
        local_path: &str,
        control: &TransferControl,
    ) -> Result<u64, String> {
        let target = self.resolve(remote_file);
        let url = self.url_for(&target);
        let mut response = self
            .agent
            .get(&url)
            .header(header::AUTHORIZATION, &self.auth)
            .call()
            .map_err(|e| format!("GET {} failed: {}", target, describe_transport_error(&e)))?;

        let status = response.status().as_u16();
        if status == 404 {
            return Err(format!("'{}' does not exist on the server.", target));
        }
        if status == 401 {
            return Err("Authentication failed (HTTP 401).".to_string());
        }
        if !(200..300).contains(&status) {
            return Err(format!("Could not download '{}': HTTP {}", target, status));
        }

        let file = std::fs::File::create(local_path)
            .map_err(|e| format!("Cannot create local file {}: {}", local_path, e))?;
        let mut writer = std::io::BufWriter::with_capacity(CHUNK_SIZE, file);
        let mut reader = response.body_mut().as_reader();
        let mut buffer = vec![0u8; CHUNK_SIZE];
        let mut total = 0u64;

        loop {
            let n = match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) => {
                    drop(writer);
                    let _ = std::fs::remove_file(local_path);
                    return Err(format!("Reading '{}' failed: {}", target, e));
                }
            };

            if let Err(e) = writer.write_all(&buffer[..n]) {
                drop(writer);
                let _ = std::fs::remove_file(local_path);
                return Err(format!("Writing {} failed: {}", local_path, e));
            }

            total += n as u64;
            if let Err(e) = control.on_chunk(n) {
                drop(writer);
                let _ = std::fs::remove_file(local_path);
                return Err(e);
            }
        }

        writer
            .flush()
            .map_err(|e| format!("Flushing {} failed: {}", local_path, e))?;
        Ok(total)
    }

    pub fn upload_file(&mut self, remote_file: &str, data: &[u8]) -> Result<(), String> {
        let target = self.resolve(remote_file);
        let url = self.url_for(&target);
        let request = Request::builder()
            .method("PUT")
            .uri(&url)
            .header(header::AUTHORIZATION, &self.auth)
            .header(header::CONTENT_LENGTH, data.len().to_string())
            .body(data)
            .map_err(|e| format!("Could not build the PUT for '{}': {}", target, e))?;

        let response = self
            .agent
            .run(request)
            .map_err(|e| format!("PUT {} failed: {}", target, describe_transport_error(&e)))?;
        let status = response.status().as_u16();
        match status {
            200 | 201 | 204 => Ok(()),
            401 => Err("Authentication failed (HTTP 401).".to_string()),
            403 => Err(format!("Not allowed to write '{}' (HTTP 403).", target)),
            409 => Err(format!(
                "The parent collection of '{}' does not exist (HTTP 409).",
                target
            )),
            507 => Err(format!("The server is out of space (HTTP 507) for '{}'.", target)),
            other => Err(format!("Could not upload '{}': HTTP {}", target, other)),
        }
    }

    pub fn upload_from_local_file_ctl(
        &mut self,
        local_path: &str,
        remote_file: &str,
        control: &TransferControl,
    ) -> Result<u64, String> {
        let target = self.resolve(remote_file);
        let size = std::fs::metadata(local_path)
            .map_err(|e| format!("Cannot read {}: {}", local_path, e))?
            .len();

        /* Buffering buys a real `Content-Length`, which some servers and most
           reverse proxies require for PUT. It also makes the transfer
           unsteerable: the bytes are already in memory when the request starts,
           so a speed limit could not be honoured and the progress bar could not
           move during the upload. A limited transfer therefore always streams,
           whatever the size. */
        if size <= BUFFERED_UPLOAD_LIMIT && !control.has_speed_limit() {
            return self.put_buffered(local_path, &target, size, control);
        }

        match self.put_streaming(local_path, &target, size, control) {
            // 411/400/501 are what a server answers when it will not accept a
            // chunked body. Rather than failing a file the user can see is
            // small enough, send it again with a length.
            Err(PutError::Refused(_)) if size <= BUFFERED_FALLBACK_LIMIT => {
                self.put_buffered(local_path, &target, size, control)
            }
            Err(PutError::Message(m)) => Err(m),
            Err(PutError::Refused(status)) => Err(format!(
                "The server refused a streamed upload of '{}' (HTTP {}) and the file is too large to send in one piece.",
                target, status
            )),
            Ok(bytes) => Ok(bytes),
        }
    }

    fn put_buffered(
        &mut self,
        local_path: &str,
        target: &str,
        size: u64,
        control: &TransferControl,
    ) -> Result<u64, String> {
        if control.is_cancelled() {
            return Err(CANCELLED.to_string());
        }

        let data = std::fs::read(local_path)
            .map_err(|e| format!("Cannot read {}: {}", local_path, e))?;
        self.upload_file(target, &data)?;

        // Reported after the fact because the whole body leaves in one write.
        control.on_chunk(data.len())?;
        Ok(size)
    }

    fn put_streaming(
        &mut self,
        local_path: &str,
        target: &str,
        size: u64,
        control: &TransferControl,
    ) -> Result<u64, PutError> {
        let file = std::fs::File::open(local_path)
            .map_err(|e| PutError::Message(format!("Cannot open {}: {}", local_path, e)))?;
        let url = self.url_for(target);

        let cancelled = Arc::new(AtomicBool::new(false));
        let mut reader = ProgressReader {
            inner: file,
            control,
            cancelled: cancelled.clone(),
        };

        let request = Request::builder()
            .method("PUT")
            .uri(&url)
            .header(header::AUTHORIZATION, &self.auth)
            .body(ureq::SendBody::from_reader(&mut reader))
            .map_err(|e| PutError::Message(format!("Could not build the PUT for '{}': {}", target, e)))?;

        let response = match self.agent.run(request) {
            Ok(r) => r,
            Err(e) => {
                if cancelled.load(Ordering::Relaxed) {
                    return Err(PutError::Message(CANCELLED.to_string()));
                }
                return Err(PutError::Message(format!(
                    "PUT {} failed: {}",
                    target,
                    describe_transport_error(&e)
                )));
            }
        };

        match response.status().as_u16() {
            200 | 201 | 204 => Ok(size),
            411 | 400 | 501 => Err(PutError::Refused(response.status().as_u16())),
            401 => Err(PutError::Message(
                "Authentication failed (HTTP 401).".to_string(),
            )),
            403 => Err(PutError::Message(format!(
                "Not allowed to write '{}' (HTTP 403).",
                target
            ))),
            409 => Err(PutError::Message(format!(
                "The parent collection of '{}' does not exist (HTTP 409).",
                target
            ))),
            507 => Err(PutError::Message(format!(
                "The server is out of space (HTTP 507) for '{}'.",
                target
            ))),
            other => Err(PutError::Message(format!(
                "Could not upload '{}': HTTP {}",
                target, other
            ))),
        }
    }
}

enum PutError {
    /// The server will not take a chunked body; the caller may retry buffered.
    Refused(u16),
    Message(String),
}

/// Wraps the file so the transfer limiter and the progress bar see the bytes as
/// they leave, not after the request returns.
struct ProgressReader<'a> {
    inner: std::fs::File,
    control: &'a TransferControl,
    cancelled: Arc<AtomicBool>,
}

impl Read for ProgressReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        if n > 0 {
            if let Err(e) = self.control.on_chunk(n) {
                // Remember *why* the stream stopped: ureq can only report an
                // io::Error here, and a cancelled transfer must not surface as
                // a network failure.
                self.cancelled.store(true, Ordering::Relaxed);
                return Err(std::io::Error::new(std::io::ErrorKind::Interrupted, e));
            }
        }
        Ok(n)
    }
}

// ------------------------------------------------------------------ helpers

fn strip_prefix_ci<'a>(value: &'a str, prefix: &str) -> Option<&'a str> {
    if value.len() >= prefix.len() && value[..prefix.len()].eq_ignore_ascii_case(prefix) {
        Some(&value[prefix.len()..])
    } else {
        None
    }
}

/// True when the authority already carries an explicit port, including the
/// bracketed IPv6 form `[::1]:8080`.
fn authority_has_port(authority: &str) -> bool {
    match authority.rsplit_once(':') {
        Some((_, port)) => !port.is_empty() && port.chars().all(|c| c.is_ascii_digit()),
        None => false,
    }
}

/// Collapses `.`/`..` and repeated slashes, refusing to climb above `root`.
fn normalize_path(path: &str, root: &str) -> String {
    let floor: Vec<&str> = root.split('/').filter(|s| !s.is_empty()).collect();
    let mut out: Vec<String> = Vec::new();

    for segment in path.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                // Never pop into the prefix the user configured: `..` at the
                // root has to stay at the root, otherwise navigating up would
                // silently leave the share.
                if out.len() > floor.len() {
                    out.pop();
                }
            }
            other => out.push(other.to_string()),
        }
    }

    if out.is_empty() {
        "/".to_string()
    } else {
        format!("/{}", out.join("/"))
    }
}

/// Percent-encodes everything that is not safe in a URL path segment. Spaces
/// and non-ASCII names are the norm on real servers, so this cannot be skipped.
fn encode_path(path: &str) -> String {
    // Splitting on '/' keeps the separators: the empty leading segment of an
    // absolute path encodes to nothing, so the join restores the leading slash.
    let encoded = path
        .split('/')
        .map(encode_segment)
        .collect::<Vec<_>>()
        .join("/");
    if encoded.is_empty() {
        "/".to_string()
    } else {
        encoded
    }
}

fn encode_segment(segment: &str) -> String {
    let mut out = String::with_capacity(segment.len());
    for byte in segment.as_bytes() {
        match byte {
            b'A'..=b'Z'
            | b'a'..=b'z'
            | b'0'..=b'9'
            | b'-'
            | b'.'
            | b'_'
            | b'~'
            | b'!'
            | b'$'
            | b'&'
            | b'\''
            | b'('
            | b')'
            | b'*'
            | b'+'
            | b','
            | b';'
            | b'='
            | b':'
            | b'@' => out.push(*byte as char),
            other => out.push_str(&format!("%{:02X}", other)),
        }
    }
    out
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = (bytes[i + 1] as char).to_digit(16);
            let lo = (bytes[i + 2] as char).to_digit(16);
            if let (Some(hi), Some(lo)) = (hi, lo) {
                out.push((hi * 16 + lo) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

/// Turns the `href` from a PROPFIND response into an absolute server path.
///
/// Servers answer with anything from `/dav/file.txt` to a fully qualified
/// `https://host:8080/dav/file.txt`, percent-encoded, sometimes with a trailing
/// slash for collections.
fn href_to_path(href: &str) -> String {
    let trimmed = href.trim();
    if trimmed.is_empty() {
        return String::new();
    }

    let without_origin = if let Some(scheme_end) = trimmed.find("://") {
        let after = &trimmed[scheme_end + 3..];
        match after.find('/') {
            Some(i) => &after[i..],
            None => "/",
        }
    } else {
        trimmed
    };

    let decoded = percent_decode(without_origin);
    let mut path = if decoded.starts_with('/') {
        decoded
    } else {
        format!("/{}", decoded)
    };
    while path.len() > 1 && path.ends_with('/') {
        path.pop();
    }
    path
}

fn same_collection(a: &str, b: &str) -> bool {
    let trim = |s: &str| {
        let mut v = s.trim_end_matches('/').to_string();
        if v.is_empty() {
            v.push('/');
        }
        v
    };
    trim(a) == trim(b)
}

#[derive(Debug, Default, Clone, PartialEq)]
struct DavEntry {
    href: String,
    is_collection: bool,
    size: u64,
    modified: String,
}

/// Extracts the `<response>` elements of a `multistatus` document.
///
/// This is not a general XML parser. It matches on local tag names so the
/// namespace prefix a server happens to use is irrelevant, and it ignores
/// everything it does not recognise — which is what makes it work with Apache,
/// nginx, sabre/dav, Nextcloud and IIS alike.
fn parse_multistatus(xml: &str) -> Vec<DavEntry> {
    let mut entries: Vec<DavEntry> = Vec::new();
    let mut current: Option<DavEntry> = None;
    let mut capture: Option<&'static str> = None;
    let mut rest = xml;

    while let Some(lt) = rest.find('<') {
        let text = &rest[..lt];
        if let (Some(entry), Some(field)) = (current.as_mut(), capture) {
            let value = text.trim();
            if !value.is_empty() {
                match field {
                    "href" => entry.href.push_str(value),
                    "size" => entry.size = value.parse().unwrap_or(0),
                    "modified" => entry.modified.push_str(value),
                    _ => {}
                }
            }
        }

        let Some(gt) = rest[lt..].find('>') else {
            break;
        };
        let tag = &rest[lt + 1..lt + gt];
        rest = &rest[lt + gt + 1..];

        // Declarations, comments and CDATA carry no structure we need.
        if tag.starts_with('?') || tag.starts_with('!') {
            continue;
        }

        let closing = tag.starts_with('/');
        let self_closing = tag.ends_with('/');
        let name = tag
            .trim_start_matches('/')
            .trim_end_matches('/')
            .split(|c: char| c.is_whitespace())
            .next()
            .unwrap_or("");
        let local = name
            .rsplit(':')
            .next()
            .unwrap_or(name)
            .to_ascii_lowercase();

        if closing {
            match local.as_str() {
                "response" => {
                    if let Some(entry) = current.take() {
                        entries.push(entry);
                    }
                    capture = None;
                }
                "href" | "getcontentlength" | "getlastmodified" => capture = None,
                _ => {}
            }
            continue;
        }

        match local.as_str() {
            "response" => {
                let entry = DavEntry::default();
                if self_closing {
                    entries.push(entry);
                } else {
                    current = Some(entry);
                }
            }
            "collection" => {
                if let Some(entry) = current.as_mut() {
                    entry.is_collection = true;
                }
            }
            "href" => capture = if self_closing { None } else { Some("href") },
            "getcontentlength" => {
                capture = if self_closing { None } else { Some("size") }
            }
            "getlastmodified" => {
                capture = if self_closing { None } else { Some("modified") }
            }
            _ => {}
        }
    }

    // A truncated document may end without the closing tag.
    if let Some(entry) = current.take() {
        entries.push(entry);
    }

    entries
}

/// `Wed, 21 Oct 2015 07:28:00 GMT` → `2015-10-21 07:28`, the shape every other
/// backend produces, so the file table column stays consistent.
fn format_http_date(value: &str) -> String {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    if let Ok(dt) = chrono::DateTime::parse_from_rfc2822(trimmed) {
        return dt.format("%Y-%m-%d %H:%M").to_string();
    }
    // chrono's RFC 2822 parser is strict about the obsolete `GMT` zone that
    // HTTP dates always use, so fall back to the exact RFC 1123 shape.
    if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(trimmed, "%a, %d %b %Y %H:%M:%S GMT") {
        return dt.format("%Y-%m-%d %H:%M").to_string();
    }
    trimmed.to_string()
}

fn body_excerpt(body: &str) -> String {
    let text: String = body
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(200)
        .collect();
    if text.is_empty() {
        String::new()
    } else {
        format!(" — {}", text)
    }
}

fn auth_hint(body: &str) -> String {
    let lower = body.to_ascii_lowercase();
    if lower.contains("digest") {
        ". This server only offers Digest authentication, which is not supported — switch it to Basic auth, or use the SFTP/FTP protocol instead."
            .to_string()
    } else {
        String::new()
    }
}

fn describe_transport_error(error: &ureq::Error) -> String {
    let text = error.to_string();
    if text.contains("certificate") || text.contains("Certificate") || text.contains("UnknownIssuer") {
        return format!(
            "{} — the TLS certificate was rejected. If this server uses a self-signed certificate, enable 'Trust this server' for the site.",
            text
        );
    }
    text
}

#[cfg(test)]
pub(crate) mod test_server {
    //! A real WebDAV server for tests, shared by every module that needs one.

    /// Spawns `tests/webdav-server.mjs` on a free port and serves a scratch
    /// directory. Returns `None` when Node is not installed, so the suite stays
    /// green on a machine without it instead of failing for the wrong reason.
    pub(crate) struct TestServer {
        child: std::process::Child,
        pub(crate) port: u16,
        pub(crate) root: std::path::PathBuf,
    }

    impl TestServer {
        pub(crate) fn start() -> Option<Self> {
            use std::io::BufRead;

            let script = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/webdav-server.mjs");
            if !std::path::Path::new(script).exists() {
                return None;
            }

            static COUNTER: std::sync::atomic::AtomicUsize =
                std::sync::atomic::AtomicUsize::new(0);
            let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let root =
                std::env::temp_dir().join(format!("90ftp_dav_{}_{}", std::process::id(), n));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(&root).ok()?;

            let mut child = std::process::Command::new("node")
                .arg(script)
                .arg(&root)
                .arg("0")
                .arg("user")
                .arg("pass")
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::null())
                .spawn()
                .ok()?;

            let stdout = child.stdout.take()?;
            let mut reader = std::io::BufReader::new(stdout);
            let mut line = String::new();
            // A server that never announces itself is a broken harness, not a
            // client bug, so fail loudly here.
            if reader.read_line(&mut line).ok()? == 0 {
                let _ = child.kill();
                return None;
            }
            let port = line
                .trim()
                .strip_prefix("LISTENING ")
                .and_then(|p| p.parse::<u16>().ok())?;

            Some(Self { child, port, root })
        }

        /// The URL a client should be pointed at.
        pub(crate) fn url(&self) -> String {
            format!("http://127.0.0.1:{}", self.port)
        }
    }

    impl Drop for TestServer {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::webdav_client::test_server::TestServer;

    fn connect_to(server: &TestServer, pass: &str) -> Result<WebdavClient, String> {
        WebdavClient::connect(
            &format!("http://127.0.0.1:{}", server.port),
            "webdav",
            server.port,
            "user",
            pass,
            false,
        )
    }

    /// The whole surface against a real server: collections, uploads, a listing
    /// that has to decode hrefs, a move, and deletes.
    #[test]
    fn round_trips_every_method_against_a_real_server() {
        let Some(server) = TestServer::start() else {
            eprintln!("node is not available — skipping the WebDAV integration test");
            return;
        };

        let mut client = connect_to(&server, "pass").expect("the endpoint should accept us");

        let root_listing = client.list(None).expect("the root must be listable");
        assert!(root_listing.is_empty(), "the scratch directory starts empty");

        client
            .make_dir_all("/site/assets")
            .expect("nested collections are created segment by segment");

        client
            .upload_file("/site/assets/app.css", b"body{color:red}")
            .expect("PUT should create the file");

        let stat = client
            .stat("/site/assets/app.css")
            .expect("stat should succeed")
            .expect("the file exists now");
        assert!(!stat.is_dir);
        assert_eq!(stat.size, 15);

        let downloaded = client
            .download_file("/site/assets/app.css")
            .expect("GET should return the bytes");
        assert_eq!(downloaded, b"body{color:red}");

        let listing = client.list(Some("/site/assets")).expect("list the collection");
        assert_eq!(listing.len(), 1, "the collection itself must not be listed as a child");
        assert_eq!(listing[0].name, "app.css");
        assert_eq!(listing[0].path, "/site/assets/app.css");
        assert!(!listing[0].is_dir);

        // A name with a space and non-ASCII letters: both have to survive
        // percent-encoding on the way out and decoding on the way back.
        // The letters are spelled as escapes because this project keeps Arabic
        // literals confined to i18n.ar.js.
        let unicode_name = "\u{0645}\u{0644}\u{0641} \u{0646}\u{0647}\u{0627}\u{0626}\u{064A}.txt";
        let unicode_path = format!("/site/assets/{}", unicode_name);
        client
            .upload_file(&unicode_path, b"ok")
            .expect("upload a name that needs encoding");
        let listing = client.list(Some("/site/assets")).expect("list again");
        let names: Vec<&str> = listing.iter().map(|i| i.name.as_str()).collect();
        assert!(
            names.contains(&unicode_name),
            "expected the decoded name, got {:?}",
            names
        );

        client
            .rename("/site/assets/app.css", "/site/assets/main.css")
            .expect("MOVE should rename");
        assert!(
            client.stat("/site/assets/app.css").expect("stat").is_none(),
            "the old name must be gone after a move"
        );
        assert!(client.stat("/site/assets/main.css").expect("stat").is_some());

        assert_eq!(
            client.change_dir("/site").expect("chdir into an existing collection"),
            "/site"
        );
        assert_eq!(client.current_dir(), "/site");

        client
            .remove_file("/site/assets/main.css")
            .expect("DELETE a file");
        client
            .remove_dir("/site/assets")
            .expect("DELETE a collection recursively");
        assert!(client.stat("/site/assets").expect("stat").is_none());
    }

    /// The streaming path, with a file large enough to skip the buffered branch.
    #[test]
    fn streams_a_large_upload_and_download() {
        let Some(server) = TestServer::start() else {
            eprintln!("node is not available — skipping the WebDAV streaming test");
            return;
        };

        let mut client = connect_to(&server, "pass").expect("connect");
        client.make_dir_all("/big").expect("mkdir");

        let payload: Vec<u8> = (0..(BUFFERED_UPLOAD_LIMIT + 12345))
            .map(|i| (i % 251) as u8)
            .collect();
        let local = server.root.join("payload.bin");
        std::fs::write(&local, &payload).expect("write the source file");

        let control = TransferControl::unbounded();
        let sent = client
            .upload_from_local_file_ctl(
                local.to_str().unwrap(),
                "/big/payload.bin",
                &control,
            )
            .expect("streamed PUT");
        assert_eq!(sent, payload.len() as u64);

        let back = server.root.join("payload.back.bin");
        let written = client
            .download_to_local_file_ctl(
                "/big/payload.bin",
                back.to_str().unwrap(),
                &TransferControl::unbounded(),
            )
            .expect("streamed GET");
        assert_eq!(written, payload.len() as u64);
        assert_eq!(
            std::fs::read(&back).expect("read back"),
            payload,
            "the bytes must survive the round trip unchanged"
        );
    }

    #[test]
    fn a_wrong_password_is_reported_as_an_auth_failure() {
        let Some(server) = TestServer::start() else {
            eprintln!("node is not available — skipping the WebDAV auth test");
            return;
        };

        let error = match connect_to(&server, "wrong") {
            Ok(_) => panic!("a wrong password must not connect"),
            Err(e) => e,
        };
        assert!(
            error.contains("401") || error.to_lowercase().contains("username or password"),
            "expected an authentication message, got: {}",
            error
        );
    }

    #[test]
    fn parses_a_namespaced_multistatus() {
        let xml = r#"<?xml version="1.0" encoding="utf-8"?>
<D:multistatus xmlns:D="DAV:">
  <D:response>
    <D:href>/dav/</D:href>
    <D:propstat><D:prop><D:resourcetype><D:collection/></D:resourcetype></D:prop>
      <D:status>HTTP/1.1 200 OK</D:status></D:propstat>
  </D:response>
  <D:response>
    <D:href>/dav/index.html</D:href>
    <D:propstat><D:prop>
      <D:resourcetype/>
      <D:getcontentlength>1024</D:getcontentlength>
      <D:getlastmodified>Wed, 21 Oct 2015 07:28:00 GMT</D:getlastmodified>
    </D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat>
  </D:response>
</D:multistatus>"#;

        let entries = parse_multistatus(xml);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].href, "/dav/");
        assert!(entries[0].is_collection);
        assert_eq!(entries[1].href, "/dav/index.html");
        assert!(!entries[1].is_collection);
        assert_eq!(entries[1].size, 1024);
        assert_eq!(entries[1].modified, "Wed, 21 Oct 2015 07:28:00 GMT");
    }

    /// sabre/dav (Nextcloud) uses `d:` and puts the status before the prop.
    #[test]
    fn parses_lowercase_prefixes_and_reordered_props() {
        let xml = r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:" xmlns:s="http://sabredav.org/ns">
 <d:response>
  <d:href>/remote.php/dav/files/user/</d:href>
  <d:propstat>
   <d:status>HTTP/1.1 200 OK</d:status>
   <d:prop><d:resourcetype><d:collection/></d:resourcetype></d:prop>
  </d:propstat>
 </d:response>
 <d:response>
  <d:href>/remote.php/dav/files/user/My%20Site/</d:href>
  <d:propstat>
   <d:prop><d:resourcetype><d:collection/></d:resourcetype></d:prop>
  </d:propstat>
 </d:response>
</d:multistatus>"#;

        let entries = parse_multistatus(xml);
        assert_eq!(entries.len(), 2);
        assert!(entries[1].is_collection);
        assert_eq!(href_to_path(&entries[1].href), "/remote.php/dav/files/user/My Site");
    }

    /// Apache answers with absolute URLs and no namespace prefix at all.
    #[test]
    fn parses_absolute_hrefs_without_a_prefix() {
        let xml = r#"<multistatus xmlns="DAV:">
<response><href>https://nas.local:5006/dav/report%20final.pdf</href>
<propstat><prop><resourcetype/><getcontentlength>77</getcontentlength></prop></propstat></response>
</multistatus>"#;

        let entries = parse_multistatus(xml);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].size, 77);
        assert_eq!(
            href_to_path(&entries[0].href),
            "/dav/report final.pdf",
            "the origin must be dropped and the space decoded"
        );
    }

    #[test]
    fn a_missing_length_does_not_break_the_entry() {
        let xml = r#"<D:multistatus xmlns:D="DAV:"><D:response>
<D:href>/dav/empty.txt</D:href><D:propstat><D:prop><D:resourcetype/></D:prop></D:propstat>
</D:response></D:multistatus>"#;
        let entries = parse_multistatus(xml);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].size, 0);
        assert!(!entries[0].is_collection);
    }

    #[test]
    fn resolves_relative_paths_against_the_current_collection() {
        let client_path = |path: &str, current: &str, root: &str| {
            let joined = if path.starts_with('/') {
                path.to_string()
            } else if path.is_empty() || path == "." {
                current.to_string()
            } else {
                format!("{}/{}", current.trim_end_matches('/'), path)
            };
            normalize_path(&joined, root)
        };

        assert_eq!(client_path("index.html", "/dav", "/dav"), "/dav/index.html");
        assert_eq!(client_path("../etc", "/dav/sub", "/dav"), "/dav/etc");
        // `..` at the root must stay inside the configured share.
        assert_eq!(client_path("..", "/dav", "/dav"), "/dav");
        assert_eq!(client_path("/", "/dav/sub", "/dav"), "/");
        assert_eq!(client_path("//a//b/", "/dav", "/dav"), "/a/b");
    }

    #[test]
    fn encodes_spaces_and_non_ascii_names() {
        assert_eq!(encode_path("/dav/a b.txt"), "/dav/a%20b.txt");
        // U+0645 U+0644 U+0641 — three two-byte characters, each percent-encoded.
        let arabic = "\u{0645}\u{0644}\u{0641}.txt";
        assert_eq!(
            encode_path(&format!("/dav/{}", arabic)),
            "/dav/%D9%85%D9%84%D9%81.txt"
        );
        assert_eq!(encode_path("/"), "/");
        // A literal percent must be encoded, or the server decodes it twice.
        assert_eq!(encode_path("/100%.txt"), "/100%25.txt");
    }

    #[test]
    fn detects_an_explicit_port_including_ipv6() {
        assert!(authority_has_port("nas.local:5006"));
        assert!(authority_has_port("[::1]:8080"));
        assert!(!authority_has_port("nas.local"));
        assert!(!authority_has_port("[::1]"));
    }

    #[test]
    fn normalises_http_dates() {
        assert_eq!(
            format_http_date("Wed, 21 Oct 2015 07:28:00 GMT"),
            "2015-10-21 07:28"
        );
        assert_eq!(format_http_date(""), "");
        // An unexpected format is passed through rather than blanked.
        assert_eq!(format_http_date("not a date"), "not a date");
    }
}
