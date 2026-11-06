//! Attachment bytes staged in the selected OpenCode server's temporary directory.
//!
//! The 2.2.0 client declares an unwrapped `GET /api/info` response and a
//! location-wrapped `POST /api/experimental/fs/write` response. Writes use raw
//! bytes. Allocation only reads server info and returns an immutable per-file
//! plan. Persist its owner, exact target and captured bytes before calling
//! `AttachmentPlan::write`. Explicit retries reuse that same plan and payload.
//!
//! The caller retains its draft ownership and recoverable bytes before dispatch.
//! A failed or cancelled write may have reached the server. This module does not
//! retry, delete files, promise retention, or fall back to workspace writes.
//! Format detection, document conversion and a durable local spool belong to
//! preparation, outside this transport.
//!
//! Integration requires `RequestBuilder::body(Vec<u8>)` to forward owned raw
//! bytes to reqwest without bypassing the authenticated `send` method.

use std::collections::hash_map::RandomState;
use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

use openchamber_native_core::path;
use reqwest::header::CONTENT_TYPE;
use reqwest::{Method, Response, StatusCode};
use serde::Deserialize;

use crate::client::{ServerClient, encode_directory};
use crate::endpoint::Endpoint;
use crate::request::RequestBuilder;

/// The shared document pipeline's ceiling for one prepared embedded image.
/// Raw input limits apply before normalization; prepared content can be larger.
/// This is a client bound, not a claim about OpenCode's upload limit.
pub const MAX_PREPARED_ATTACHMENT_BYTES: usize = 20 * 1024 * 1024;
const MAX_FILENAME_BYTES: usize = 255;
const MAX_REMOTE_PATH_BYTES: usize = 16 * 1024;
const MAX_RESPONSE_BYTES: usize = 64 * 1024;

/// Captured, prepared bytes and a validated single-component filename.
pub struct AttachmentUpload {
    filename: String,
    bytes: Vec<u8>,
}

impl AttachmentUpload {
    /// Validates without renaming or interpreting the bytes as text.
    pub fn new(filename: &str, bytes: Vec<u8>) -> Result<Self, AttachmentError> {
        validate_filename(filename)?;
        if bytes.len() > MAX_PREPARED_ATTACHMENT_BYTES {
            return Err(AttachmentError::FileTooLarge);
        }
        Ok(Self {
            filename: filename.to_owned(),
            bytes,
        })
    }

    /// The original safe filename, for association with the caller's draft slot.
    pub fn filename(&self) -> &str {
        &self.filename
    }
}

impl fmt::Debug for AttachmentUpload {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AttachmentUpload")
            .field("filename", &"<redacted>")
            .field("bytes", &self.bytes.len())
            .finish()
    }
}

/// A server path verified against the exact generated staging target.
#[derive(Clone, PartialEq, Eq)]
pub struct StagedAttachment {
    server_path: String,
}

impl StagedAttachment {
    /// An absolute path in the captured server's path space, not a local path.
    pub fn server_path(&self) -> &str {
        &self.server_path
    }
}

impl fmt::Debug for StagedAttachment {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("StagedAttachment(<redacted>)")
    }
}

/// An immutable write plan bound to one captured client, directory and payload.
/// Planning or restoring this type never writes a file.
#[must_use]
pub struct AttachmentPlan {
    client: ServerClient,
    directory: String,
    temporary_directory: String,
    server_path: String,
    file: AttachmentUpload,
}

impl AttachmentPlan {
    /// The captured endpoint. Persist its runtime association with the plan.
    pub fn endpoint(&self) -> &Endpoint {
        self.client.endpoint()
    }

    /// The exact directory whose marked header accompanies every request.
    pub fn directory(&self) -> &str {
        &self.directory
    }

    /// The selected server's temporary root retained for recovery validation.
    pub fn temporary_directory(&self) -> &str {
        &self.temporary_directory
    }

    /// The allocated absolute server target, available before any file write.
    pub fn server_path(&self) -> &str {
        &self.server_path
    }

    /// The captured safe filename.
    pub fn filename(&self) -> &str {
        self.file.filename()
    }

    /// The exact captured payload to persist in the caller's durable spool.
    pub fn bytes(&self) -> &[u8] {
        &self.file.bytes
    }

    /// Submits one explicit write to this retained target and payload.
    ///
    /// The caller must persist the plan and bytes before the first call. Each
    /// call freshly checks the temporary root and refuses a change. Neither
    /// failure nor cancellation remints the target or alters the payload. A
    /// lost response can leave the file written; the caller decides whether an
    /// explicit same-plan write is appropriate. Other files have separate plans
    /// and outcomes, with no shared mutation or retry loop.
    pub async fn write(&self) -> Result<StagedAttachment, AttachmentError> {
        let current_root = self
            .client
            .attachment_temporary_directory(&self.directory)
            .await?;
        if current_root != self.temporary_directory {
            return Err(AttachmentError::TemporaryDirectoryChanged);
        }
        let location = remote_path(&self.directory).ok_or(AttachmentError::InvalidDirectory)?;
        // reqwest needs an owned body. Keep the captured bytes for an explicit
        // repeat of this plan instead of consuming or rereading a local source.
        let response = self
            .client
            .attachment_write_request(&self.directory, &self.server_path, self.file.bytes.clone())
            .send()
            .await
            .map_err(request_error)?;
        parse_written_path(&read_body(response).await?, &location, &self.server_path)
    }
}

impl fmt::Debug for AttachmentPlan {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AttachmentPlan")
            .field("owner", &"<redacted>")
            .field("server_path", &"<redacted>")
            .field("file", &self.file)
            .finish()
    }
}

/// Content-free failures. No response text, filename, path or credential is kept.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum AttachmentError {
    #[error("invalid attachment directory")]
    InvalidDirectory,
    #[error("invalid attachment filename")]
    InvalidFilename,
    #[error("attachment exceeds the client size limit")]
    FileTooLarge,
    #[error("invalid server temporary directory")]
    InvalidTemporaryDirectory,
    #[error("invalid retained attachment target")]
    InvalidTarget,
    #[error("the server temporary directory changed")]
    TemporaryDirectoryChanged,
    #[error("attachment response is malformed")]
    InvalidResponse,
    #[error("attachment response exceeds its size limit")]
    ResponseTooLarge,
    #[error("attachment response names a different location")]
    LocationMismatch,
    #[error("attachment response names a different path")]
    PathMismatch,
    #[error("the server requires authentication")]
    AuthenticationRequired,
    #[error("the server answered with a redirect, which is never followed")]
    Redirected,
    #[error("the attachment request did not answer in time")]
    TimedOut,
    #[error("the attachment request failed")]
    RequestFailed,
    #[error("the server answered HTTP {status}")]
    Http { status: u16 },
}

#[derive(Deserialize)]
struct InfoResponse {
    paths: InfoPaths,
}

#[derive(Deserialize)]
struct InfoPaths {
    tmp: String,
}

#[derive(Deserialize)]
struct WriteResponse {
    location: WriteLocation,
    data: WrittenPath,
}

#[derive(Deserialize)]
struct WriteLocation {
    directory: String,
}

#[derive(Deserialize)]
struct WrittenPath {
    path: String,
}

impl ServerClient {
    /// Reads the selected server's temporary root and allocates one exact target.
    ///
    /// The directory must be the actual location being worked in. A fresh
    /// app-owned folder prevents repeated filenames from overwriting siblings.
    /// This performs no file write. Persist the returned plan and captured bytes
    /// before calling `write`; callers retain each file's independent outcome.
    pub async fn plan_attachment(
        &self,
        directory: &str,
        file: AttachmentUpload,
    ) -> Result<AttachmentPlan, AttachmentError> {
        remote_path(directory).ok_or(AttachmentError::InvalidDirectory)?;
        let temporary_directory = self.attachment_temporary_directory(directory).await?;
        let server_path = staging_target(&temporary_directory, file.filename())?;
        Ok(AttachmentPlan {
            client: self.clone(),
            directory: directory.to_owned(),
            temporary_directory,
            server_path,
            file,
        })
    }

    /// Restores a retained target with the original immutable spool bytes.
    ///
    /// The caller binds this client and directory to the original verified
    /// runtime, as for session recovery. The saved root and target must describe
    /// exactly `<tmp>/openchamber/<16-lowercase-hex>/<safe filename>`. A fresh info
    /// read must still name that root. No target is allocated and nothing is
    /// written. A changed root is an explicit refusal, with no fallback URI.
    pub async fn restore_attachment_plan(
        &self,
        directory: &str,
        temporary_directory: &str,
        server_path: &str,
        file: AttachmentUpload,
    ) -> Result<AttachmentPlan, AttachmentError> {
        remote_path(directory).ok_or(AttachmentError::InvalidDirectory)?;
        let temporary_directory =
            remote_path(temporary_directory).ok_or(AttachmentError::InvalidTemporaryDirectory)?;
        validate_retained_target(&temporary_directory, server_path, file.filename())?;
        let current_root = self.attachment_temporary_directory(directory).await?;
        if current_root != temporary_directory {
            return Err(AttachmentError::TemporaryDirectoryChanged);
        }
        Ok(AttachmentPlan {
            client: self.clone(),
            directory: directory.to_owned(),
            temporary_directory,
            server_path: server_path.to_owned(),
            file,
        })
    }

    async fn attachment_temporary_directory(
        &self,
        directory: &str,
    ) -> Result<String, AttachmentError> {
        let response = self
            .attachment_info_request(directory)
            .send()
            .await
            .map_err(request_error)?;
        parse_temporary_directory(&read_body(response).await?)
    }

    fn attachment_info_request(&self, directory: &str) -> RequestBuilder {
        self.authorized(Method::GET, self.endpoint().url("api/info"))
            .header("x-opencode-directory", encode_directory(directory))
            .header("x-opencode-directory-encoding", "uri")
    }

    fn attachment_write_request(
        &self,
        directory: &str,
        target: &str,
        bytes: Vec<u8>,
    ) -> RequestBuilder {
        let mut url = self.endpoint().url("api/experimental/fs/write");
        url.query_pairs_mut().append_pair("path", target);
        self.authorized(Method::POST, url)
            .header("x-opencode-directory", encode_directory(directory))
            .header("x-opencode-directory-encoding", "uri")
            .header(CONTENT_TYPE.as_str(), "application/octet-stream")
            .body(bytes)
    }
}

fn validate_filename(filename: &str) -> Result<(), AttachmentError> {
    if filename.is_empty()
        || filename.len() > MAX_FILENAME_BYTES
        || filename.trim() != filename
        || filename.ends_with('.')
        || filename.chars().any(|character| {
            character.is_control()
                || matches!(
                    character,
                    '/' | '\\' | ':' | '<' | '>' | '"' | '|' | '?' | '*'
                )
        })
    {
        return Err(AttachmentError::InvalidFilename);
    }
    // Windows device names are reserved even with an extension. Reject them on
    // every client because the selected server may use Windows.
    let stem = filename
        .split('.')
        .next()
        .unwrap_or_default()
        .trim_end_matches(' ')
        .to_ascii_uppercase();
    if matches!(
        stem.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
    ) || ["COM", "LPT"].iter().any(|prefix| {
        stem.strip_prefix(*prefix).is_some_and(|suffix| {
            matches!(
                suffix,
                "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
            )
        })
    }) {
        return Err(AttachmentError::InvalidFilename);
    }
    Ok(())
}

// Interpret remote paths lexically, independently of the client's OS. Reject
// relative segments before comparison; normalization never resolves them.
fn remote_path(value: &str) -> Option<String> {
    if value.len() > MAX_REMOTE_PATH_BYTES
        || value.trim() != value
        || value.chars().any(char::is_control)
        // A backslash can be a real POSIX name. Do not silently write to a
        // different root by treating it as a Windows separator.
        || (value.starts_with('/') && !value.starts_with("//") && value.contains('\\'))
        || (value.starts_with('\\') && !value.starts_with("\\\\"))
    {
        return None;
    }
    let normalized = path::normalize(value)?;
    let bytes = normalized.as_bytes();
    let drive =
        bytes.len() >= 3 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && bytes[2] == b'/';
    let remainder = if drive {
        &normalized[3..]
    } else if let Some(unc) = normalized.strip_prefix("//") {
        if unc.starts_with(['?', '.']) || unc.split('/').count() < 2 {
            return None;
        }
        unc
    } else {
        normalized.strip_prefix('/')?
    };
    if !remainder.is_empty()
        && remainder
            .split('/')
            .any(|component| matches!(component, "" | "." | ".."))
    {
        return None;
    }
    Some(normalized)
}

fn staging_target(temporary_directory: &str, filename: &str) -> Result<String, AttachmentError> {
    let target = format!(
        "{}/openchamber/{}/{filename}",
        temporary_directory.trim_end_matches('/'),
        fresh_folder()
    );
    if target.len() > MAX_REMOTE_PATH_BYTES {
        return Err(AttachmentError::InvalidTemporaryDirectory);
    }
    Ok(target)
}

fn validate_retained_target(
    temporary_directory: &str,
    target: &str,
    filename: &str,
) -> Result<(), AttachmentError> {
    validate_filename(filename)?;
    if target.ends_with(['/', '\\']) {
        return Err(AttachmentError::InvalidTarget);
    }
    let target = remote_path(target).ok_or(AttachmentError::InvalidTarget)?;
    let prefix = format!("{}/openchamber/", temporary_directory.trim_end_matches('/'));
    let relative = target
        .strip_prefix(&prefix)
        .ok_or(AttachmentError::InvalidTarget)?;
    let (folder, name) = relative
        .split_once('/')
        .ok_or(AttachmentError::InvalidTarget)?;
    if folder.len() != 16
        || !folder
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        || name != filename
    {
        return Err(AttachmentError::InvalidTarget);
    }
    Ok(())
}

// The native install ID uses these standard-library random hasher keys too.
// This is a collision-resistant ownership identifier, not a credential or an
// assertion that the server's filesystem has no symlinks.
fn fresh_folder() -> String {
    use std::hash::{BuildHasher, Hasher};

    let mut hasher = RandomState::new().build_hasher();
    hasher.write_u128(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or_default(),
    );
    format!("{:016x}", hasher.finish())
}

fn parse_temporary_directory(bytes: &[u8]) -> Result<String, AttachmentError> {
    let info: InfoResponse = decode(bytes)?;
    remote_path(&info.paths.tmp).ok_or(AttachmentError::InvalidTemporaryDirectory)
}

fn parse_written_path(
    bytes: &[u8],
    directory: &str,
    target: &str,
) -> Result<StagedAttachment, AttachmentError> {
    let written: WriteResponse = decode(bytes)?;
    if remote_path(&written.location.directory).as_deref() != Some(directory) {
        return Err(AttachmentError::LocationMismatch);
    }
    let expected = remote_path(target).ok_or(AttachmentError::InvalidTarget)?;
    if written.data.path.ends_with(['/', '\\'])
        || remote_path(&written.data.path).as_deref() != Some(expected.as_str())
    {
        return Err(AttachmentError::PathMismatch);
    }
    Ok(StagedAttachment {
        server_path: target.to_owned(),
    })
}

fn decode<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> Result<T, AttachmentError> {
    if bytes.len() > MAX_RESPONSE_BYTES {
        return Err(AttachmentError::ResponseTooLarge);
    }
    serde_json::from_slice(bytes).map_err(|_| AttachmentError::InvalidResponse)
}

fn request_error(error: reqwest::Error) -> AttachmentError {
    if error.is_timeout() {
        AttachmentError::TimedOut
    } else {
        AttachmentError::RequestFailed
    }
}

async fn read_body(mut response: Response) -> Result<Vec<u8>, AttachmentError> {
    let status = response.status();
    if status == StatusCode::UNAUTHORIZED {
        return Err(AttachmentError::AuthenticationRequired);
    }
    if status.is_redirection() {
        return Err(AttachmentError::Redirected);
    }
    if status != StatusCode::OK {
        return Err(AttachmentError::Http {
            status: status.as_u16(),
        });
    }
    if !response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .is_some_and(|value| value.trim().eq_ignore_ascii_case("application/json"))
    {
        return Err(AttachmentError::InvalidResponse);
    }
    if response
        .content_length()
        .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
    {
        return Err(AttachmentError::ResponseTooLarge);
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(request_error)? {
        if chunk.len() > MAX_RESPONSE_BYTES - body.len() {
            return Err(AttachmentError::ResponseTooLarge);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

#[cfg(test)]
mod tests {
    use reqwest::Url;
    use serde_json::json;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    use crate::credential::Credential;
    use crate::endpoint::Endpoint;

    use super::*;

    const DIRECTORY: &str = "/home/synthetic/my project/ünï";
    const TARGET: &str = "/tmp/opencode/openchamber/0123456789abcdef/hello.png";

    fn client() -> ServerClient {
        ServerClient::new(
            Endpoint::parse("https://example.invalid/prefix/").expect("endpoint"),
            Credential::ClientToken("oc_client_attachment_fixture".to_owned()),
        )
        .expect("client")
    }

    fn receipt(directory: &str, target: &str) -> Vec<u8> {
        serde_json::to_vec(&json!({
            "location": { "directory": directory }, "data": { "path": target }
        }))
        .expect("JSON")
    }

    #[test]
    fn requests_match_the_pinned_official_client_with_exact_binary_bytes() {
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("../tests/fixtures/transport.json"))
                .expect("official client fixture");
        let client = ServerClient::new(
            Endpoint::parse(fixture["baseUrl"].as_str().expect("fixture URL"))
                .expect("fixture endpoint"),
            Credential::None,
        )
        .expect("client");
        let directory = fixture["scopedDirectory"]
            .as_str()
            .expect("fixture directory");
        let requests = fixture["requests"].as_array().expect("fixture requests");
        for (name, actual) in [
            (
                "attachment temporary directory",
                client
                    .attachment_info_request(directory)
                    .build()
                    .expect("info request"),
            ),
            (
                "write attachment bytes",
                client
                    .attachment_write_request(
                        directory,
                        "/tmp/opencode/openchamber/0123456789abcdef/diagram ü.png",
                        vec![0, 255, 7],
                    )
                    .build()
                    .expect("write request"),
            ),
        ] {
            let expected = requests
                .iter()
                .find(|request| request["name"] == name)
                .expect("official client recorded this operation");
            assert_eq!(
                actual.method().as_str(),
                expected["method"].as_str().expect("method")
            );
            assert_eq!(
                actual.url().path(),
                expected["path"].as_str().expect("path")
            );
            let mut query: Vec<_> = actual
                .url()
                .query_pairs()
                .map(|(key, value)| vec![key.into_owned(), value.into_owned()])
                .collect();
            query.sort();
            assert_eq!(
                json!(query),
                expected["query"],
                "preserve the exact target query"
            );
            for (header, field) in [
                ("x-opencode-directory", "directoryHeader"),
                ("content-type", "contentType"),
            ] {
                let actual = actual
                    .headers()
                    .get(header)
                    .map(|value| value.to_str().expect("header"));
                assert_eq!(actual, expected[field].as_str(), "preserve {header}");
            }
            let bytes: Option<Vec<u8>> = expected
                .get("bodyBytes")
                .map(|value| serde_json::from_value(value.clone()).expect("fixture binary body"));
            assert_eq!(
                actual
                    .body()
                    .map(|body| body.as_bytes().expect("owned bytes")),
                bytes.as_deref(),
                "do not encode binary bytes as JSON or text"
            );
        }
    }

    #[test]
    fn safe_filenames_are_single_portable_components_without_silent_renaming() {
        for filename in ["hello.png", "résumé.pdf", ".env", "my code.rs", "100%.txt"] {
            let upload = AttachmentUpload::new(filename, vec![]).expect("safe filename");
            assert_eq!(upload.filename(), filename, "preserve the captured name");
        }
        for filename in [
            "",
            " ",
            ".",
            "..",
            "../x",
            "x/../y",
            "/tmp/x",
            "x\\y",
            "C:x",
            "x:y",
            " x",
            "x ",
            "x.",
            "x\n",
            "x\0",
            "x?",
            "x*",
            "x|",
            "x<",
            "x>",
            "x\"",
            "NUL",
            "con.txt",
            "CON .txt",
            "LPT9.txt",
            "COM1",
            "COM¹.txt",
            "CONIN$",
        ] {
            assert!(
                matches!(
                    AttachmentUpload::new(filename, vec![]),
                    Err(AttachmentError::InvalidFilename)
                ),
                "reject unsafe filename {filename:?}"
            );
        }
        assert!(AttachmentUpload::new(&"a".repeat(MAX_FILENAME_BYTES), vec![]).is_ok());
        assert!(AttachmentUpload::new(&"é".repeat(128), vec![]).is_err());
    }

    #[test]
    fn upload_bytes_are_bounded_and_remain_binary() {
        let exact = AttachmentUpload::new("empty.txt", vec![0xff; MAX_PREPARED_ATTACHMENT_BYTES])
            .expect("the established input ceiling is inclusive");
        assert_eq!(exact.bytes.len(), MAX_PREPARED_ATTACHMENT_BYTES);
        let expanded = AttachmentUpload::new("normalized.png", vec![0xff; 12 * 1024 * 1024])
            .expect("prepared images can exceed the raw input limit");
        assert_eq!(expanded.bytes.len(), 12 * 1024 * 1024);
        assert!(matches!(
            AttachmentUpload::new("large.txt", vec![0; MAX_PREPARED_ATTACHMENT_BYTES + 1]),
            Err(AttachmentError::FileTooLarge)
        ));
    }

    #[test]
    fn info_uses_the_unwrapped_installed_2_2_contract_and_only_absolute_tmp_paths() {
        for (raw, expected) in [
            ("/tmp/opencode/", "/tmp/opencode"),
            ("C:\\Temp\\OpenCode\\", "C:/Temp/OpenCode"),
            ("\\\\server\\share\\tmp", "//server/share/tmp"),
            ("/", "/"),
        ] {
            let body = serde_json::to_vec(&json!({"paths": {"tmp": raw}})).expect("JSON");
            assert_eq!(
                parse_temporary_directory(&body).expect("server tmp"),
                expected
            );
        }
        for body in [
            br#"{"data":{"paths":{"tmp":"/tmp"}}}"#.as_slice(),
            br#"{"paths":{}}"#,
            br#"{"paths":{"tmp":null}}"#,
            br#"{"paths":{"tmp":17}}"#,
            br#"{"paths":{"tmp":"/tmp"}"#,
            b"not JSON",
        ] {
            assert_eq!(
                parse_temporary_directory(body),
                Err(AttachmentError::InvalidResponse)
            );
        }
        for raw in [
            "",
            "relative",
            "C:relative",
            "/a/../b",
            "/a/./b",
            "/a//b",
            " /tmp",
            "/tmp\n",
            "//server",
            "//?/C:/tmp",
            "//./device",
            "/tmp/a\\b",
            "\\Temp",
        ] {
            let body = serde_json::to_vec(&json!({"paths":{"tmp":raw}})).expect("JSON");
            assert_eq!(
                parse_temporary_directory(&body),
                Err(AttachmentError::InvalidTemporaryDirectory)
            );
        }
    }

    #[test]
    fn receipts_require_exact_safe_target_and_the_captured_location() {
        let staged = parse_written_path(&receipt(DIRECTORY, TARGET), DIRECTORY, TARGET)
            .expect("exact receipt");
        assert_eq!(staged.server_path(), TARGET);
        for target in [
            "/tmp/elsewhere/hello.png",
            "/tmp/opencode/openchamber/0123456789abcdef/../hello.png",
            "/tmp/opencode/openchamber/0123456789abcdef/hello.png.evil",
            "/tmp/opencode/openchamber/0123456789abcdef/hello.png/",
            "relative",
            "",
        ] {
            assert_eq!(
                parse_written_path(&receipt(DIRECTORY, target), DIRECTORY, TARGET),
                Err(AttachmentError::PathMismatch)
            );
        }
        assert_eq!(
            parse_written_path(&receipt("/unrelated", TARGET), DIRECTORY, TARGET),
            Err(AttachmentError::LocationMismatch)
        );
        for body in [
            b"{}".as_slice(),
            b"{\"path\":\"/tmp/x\"}",
            b"{\"data\":{\"path\":7}}",
            b"{\"location\":{\"directory\":\"/repo\"},\"data\":{}}",
            b"{\"data\":",
        ] {
            assert_eq!(
                parse_written_path(body, DIRECTORY, TARGET),
                Err(AttachmentError::InvalidResponse)
            );
        }
        let windows_target = "C:/Temp/openchamber/0123456789abcdef/a.png";
        assert!(
            parse_written_path(
                &receipt("c:\\project", &windows_target.replace('/', "\\")),
                "C:/project",
                windows_target,
            )
            .is_ok(),
            "Windows separator spelling preserves the exact target identity"
        );
    }

    #[test]
    fn retained_targets_require_the_owned_root_one_folder_and_the_exact_filename() {
        for (root, target) in [
            ("/tmp/opencode", TARGET),
            ("/", "/openchamber/0123456789abcdef/hello.png"),
            (
                "C:/Temp",
                "c:\\Temp\\openchamber\\0123456789abcdef\\hello.png",
            ),
            (
                "//server/share/tmp",
                "\\\\server\\share\\tmp\\openchamber\\0123456789abcdef\\hello.png",
            ),
        ] {
            assert_eq!(validate_retained_target(root, target, "hello.png"), Ok(()));
        }
        for target in [
            "/workspace/hello.png",
            "/tmp/opencode-other/openchamber/0123456789abcdef/hello.png",
            "/tmp/opencode/openchamber/short/hello.png",
            "/tmp/opencode/openchamber/0123456789ABCDEf/hello.png",
            "/tmp/opencode/openchamber/0123456789abcdef/other.png",
            "/tmp/opencode/openchamber/0123456789abcdef/nested/hello.png",
            "/tmp/opencode/openchamber/0123456789abcdef/../hello.png",
            "/tmp/opencode/openchamber/0123456789abcdef/hello.png/",
            "relative/openchamber/0123456789abcdef/hello.png",
        ] {
            assert_eq!(
                validate_retained_target("/tmp/opencode", target, "hello.png"),
                Err(AttachmentError::InvalidTarget),
                "saved paths cannot grant arbitrary write authority"
            );
        }
        for target in [
            "C:\\Workspace\\openchamber\\0123456789abcdef\\hello.png",
            "C:\\Temp\\openchamber\\0123456789abcdef\\..\\hello.png",
            "C:\\Temp\\openchamber\\0123456789abcdef\\hello.png:stream",
            "C:Temp\\openchamber\\0123456789abcdef\\hello.png",
        ] {
            assert_eq!(
                validate_retained_target("C:/Temp", target, "hello.png"),
                Err(AttachmentError::InvalidTarget)
            );
        }
    }

    #[test]
    fn response_parsing_is_bounded_and_diagnostics_redact_content() {
        assert_eq!(
            parse_temporary_directory(&vec![b' '; MAX_RESPONSE_BYTES + 1]),
            Err(AttachmentError::ResponseTooLarge)
        );
        assert_eq!(
            parse_written_path(&vec![b' '; MAX_RESPONSE_BYTES + 1], DIRECTORY, TARGET),
            Err(AttachmentError::ResponseTooLarge)
        );
        assert_eq!(
            staging_target(
                &format!("/{}", "a".repeat(MAX_REMOTE_PATH_BYTES - 1)),
                "a.txt"
            ),
            Err(AttachmentError::InvalidTemporaryDirectory)
        );
        let upload = AttachmentUpload::new("private-name.txt", b"private contents".to_vec())
            .expect("upload");
        let upload_debug = format!("{upload:?}");
        let staged = StagedAttachment {
            server_path: "/private/server/path".to_owned(),
        };
        let plan = AttachmentPlan {
            client: client(),
            directory: "/private/owner".to_owned(),
            temporary_directory: "/private/tmp".to_owned(),
            server_path: "/private/server/path".to_owned(),
            file: upload,
        };
        for output in [upload_debug, format!("{staged:?}"), format!("{plan:?}")] {
            assert!(
                !output.contains("private"),
                "Debug contains no filename, path or bytes"
            );
        }
        let error = parse_temporary_directory(br#"{"paths":{"tmp":"private response"}}"#)
            .expect_err("relative path");
        assert!(!format!("{error:?} {error}").contains("private"));
    }

    #[test]
    fn requests_preserve_prefix_query_raw_body_directory_and_sensitive_auth() {
        let client = client();
        let info = client
            .attachment_info_request(DIRECTORY)
            .build()
            .expect("info request");
        let binary = vec![0, 0xff, b'\n', b'"'];
        let target = "/tmp/space & ü/openchamber/0123456789abcdef/100%.txt";
        let write = client
            .attachment_write_request(DIRECTORY, target, binary.clone())
            .build()
            .expect("write request");
        assert_eq!(info.method(), Method::GET);
        assert_eq!(info.url().path(), "/prefix/api/info");
        assert!(info.url().query().is_none());
        assert!(info.body().is_none());
        assert_eq!(write.method(), Method::POST);
        assert_eq!(write.url().path(), "/prefix/api/experimental/fs/write");
        assert_eq!(write.url().query_pairs().count(), 1);
        let (key, value) = write.url().query_pairs().next().expect("path query");
        assert_eq!(key, "path");
        assert_eq!(value, target);
        assert_eq!(write.headers()[CONTENT_TYPE], "application/octet-stream");
        assert_eq!(
            write.body().and_then(reqwest::Body::as_bytes),
            Some(binary.as_slice())
        );
        for request in [info, write] {
            assert_eq!(
                request.headers()["x-opencode-directory"]
                    .to_str()
                    .expect("directory header"),
                encode_directory(DIRECTORY)
            );
            assert_eq!(request.headers()["x-opencode-directory-encoding"], "uri");
            assert_eq!(request.headers()["accept"], "application/json");
            let auth = &request.headers()["authorization"];
            assert!(auth.is_sensitive());
            assert_eq!(auth, "Bearer oc_client_attachment_fixture");
            assert!(!request.url().as_str().contains("oc_client_"));
        }
    }

    #[derive(Debug)]
    struct Recorded {
        target: String,
        headers: String,
        body: Vec<u8>,
    }

    enum Reply {
        Json { status: u16, body: String },
        EchoPath,
        ChunkedOversized,
        Truncated,
    }

    fn info() -> Reply {
        Reply::Json {
            status: 200,
            body: json!({"paths":{"tmp":"/tmp/opencode"}}).to_string(),
        }
    }

    async fn scripted_client(
        replies: Vec<Reply>,
    ) -> (ServerClient, tokio::task::JoinHandle<Vec<Recorded>>) {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("loopback bind");
        let endpoint = Endpoint::parse(&format!(
            "http://{}/prefix/",
            listener.local_addr().expect("address")
        ))
        .expect("endpoint");
        let client = ServerClient::new(
            endpoint,
            Credential::ClientToken("oc_client_attachment_fixture".to_owned()),
        )
        .expect("client");
        let task = tokio::spawn(async move {
            let mut recorded = Vec::new();
            for reply in replies {
                let (mut socket, _) = listener.accept().await.expect("request");
                let request = read_request(&mut socket).await;
                match reply {
                    Reply::Json { status, body } => {
                        write_response(&mut socket, status, &body).await;
                    }
                    Reply::EchoPath => {
                        let url =
                            Url::parse(&format!("http://fixture{}", request.target)).expect("URL");
                        let target = url
                            .query_pairs()
                            .find(|(key, _)| key == "path")
                            .expect("path query")
                            .1;
                        let body = String::from_utf8(receipt(DIRECTORY, &target)).expect("UTF-8");
                        write_response(&mut socket, 200, &body).await;
                    }
                    Reply::ChunkedOversized => {
                        socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n").await.expect("headers");
                        // No Content-Length: the streaming bound must still apply.
                        let body = " ".repeat(MAX_RESPONSE_BYTES + 1);
                        let chunk = format!("{:x}\r\n{body}\r\n0\r\n\r\n", body.len());
                        let _ = socket.write_all(chunk.as_bytes()).await;
                    }
                    Reply::Truncated => {
                        socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 100\r\nConnection: close\r\n\r\n{\"paths\":").await.expect("partial response");
                    }
                }
                recorded.push(request);
            }
            recorded
        });
        (client, task)
    }

    async fn read_request(socket: &mut TcpStream) -> Recorded {
        let mut bytes = Vec::new();
        let header_end = loop {
            if let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
                break end + 4;
            }
            let mut chunk = [0; 4096];
            let count = socket.read(&mut chunk).await.expect("request headers");
            assert_ne!(count, 0, "request headers must be complete");
            bytes.extend_from_slice(&chunk[..count]);
        };
        let headers = String::from_utf8(bytes[..header_end].to_vec()).expect("ASCII headers");
        let length: usize = headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse().expect("length"))
            })
            .unwrap_or(0);
        while bytes.len() - header_end < length {
            let mut chunk = [0; 4096];
            let count = socket.read(&mut chunk).await.expect("request body");
            assert_ne!(count, 0, "request body must be complete");
            bytes.extend_from_slice(&chunk[..count]);
        }
        Recorded {
            target: headers
                .split_whitespace()
                .nth(1)
                .expect("request target")
                .to_owned(),
            body: bytes[header_end..header_end + length].to_vec(),
            headers,
        }
    }

    async fn write_response(socket: &mut TcpStream, status: u16, body: &str) {
        let response = format!(
            "HTTP/1.1 {status} Reply\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        socket
            .write_all(response.as_bytes())
            .await
            .expect("response");
    }

    fn planned_write_replies(writes: Vec<Reply>) -> Vec<Reply> {
        let mut replies: Vec<Reply> = (0..writes.len()).map(|_| info()).collect();
        for write in writes {
            replies.push(info());
            replies.push(write);
        }
        replies
    }

    async fn planned_files(
        client: &ServerClient,
        filename: &str,
        count: usize,
    ) -> Vec<AttachmentPlan> {
        let mut plans = Vec::new();
        for index in 0..count {
            let file = AttachmentUpload::new(filename, vec![index as u8]).expect("upload");
            plans.push(client.plan_attachment(DIRECTORY, file).await.expect("plan"));
        }
        plans
    }

    #[tokio::test]
    async fn allocation_exposes_the_durable_descriptor_before_any_file_write() {
        let (client, server) = scripted_client(vec![info()]).await;
        let file = AttachmentUpload::new("hello.png", vec![0, 0xff, 7]).expect("upload");
        let plan = client.plan_attachment(DIRECTORY, file).await.expect("plan");
        assert_eq!(plan.endpoint(), client.endpoint());
        assert_eq!(plan.directory(), DIRECTORY);
        assert_eq!(plan.temporary_directory(), "/tmp/opencode");
        assert_eq!(plan.filename(), "hello.png");
        assert_eq!(plan.bytes(), [0, 0xff, 7]);
        assert_eq!(
            validate_retained_target(
                plan.temporary_directory(),
                plan.server_path(),
                plan.filename()
            ),
            Ok(())
        );
        let requests = server.await.expect("server");
        assert_eq!(requests.len(), 1, "allocation cannot write a file");
        assert!(requests[0].headers.starts_with("GET /prefix/api/info "));
        assert!(requests[0].body.is_empty());
    }

    #[tokio::test]
    async fn restored_plan_repeats_the_exact_target_and_captured_bytes() {
        let (client, server) = scripted_client(vec![
            info(),
            info(),
            Reply::Json {
                status: 200,
                body: "{}".to_owned(),
            },
            info(),
            Reply::EchoPath,
        ])
        .await;
        let file = AttachmentUpload::new("hello.png", vec![0, 0xff, 7]).expect("spool bytes");
        let plan = client
            .restore_attachment_plan(DIRECTORY, "/tmp/opencode", TARGET, file)
            .await
            .expect("restored plan");
        let endpoint = client.endpoint().clone();
        drop(client);
        assert_eq!(
            plan.endpoint(),
            &endpoint,
            "the original client remains captured"
        );
        for attempt in 0..2 {
            let result = plan.write().await;
            if attempt == 0 {
                assert_eq!(result, Err(AttachmentError::InvalidResponse));
            } else {
                assert_eq!(
                    result.expect("explicit same-plan retry").server_path(),
                    TARGET
                );
            }
            assert_eq!(plan.server_path(), TARGET, "write never remints a target");
            assert_eq!(
                plan.bytes(),
                [0, 0xff, 7],
                "write retains the immutable payload"
            );
        }
        let requests = server.await.expect("server");
        assert_eq!(
            requests.len(),
            5,
            "restore plus 2 root checks and 2 explicit writes"
        );
        let writes: Vec<&Recorded> = requests
            .iter()
            .filter(|request| {
                request
                    .target
                    .starts_with("/prefix/api/experimental/fs/write?")
            })
            .collect();
        assert_eq!(writes.len(), 2);
        assert_eq!(
            writes[0].target, writes[1].target,
            "repeat uses identical query bytes"
        );
        for write in writes {
            assert_eq!(write.body, [0, 0xff, 7]);
            assert!(
                write
                    .headers
                    .starts_with("POST /prefix/api/experimental/fs/write?")
            );
            assert!(write.headers.contains(&encode_directory(DIRECTORY)));
            let url = Url::parse(&format!("http://fixture{}", write.target)).expect("URL");
            assert_eq!(url.query_pairs().next().expect("path").1, TARGET);
        }
    }

    #[tokio::test]
    async fn restore_refuses_changed_or_caller_invented_temporary_roots() {
        for saved_root in ["/tmp/opencode", "/workspace"] {
            let (client, server) = scripted_client(vec![Reply::Json {
                status: 200,
                body: json!({"paths":{"tmp":"/tmp/current"}}).to_string(),
            }])
            .await;
            let target = format!("{saved_root}/openchamber/0123456789abcdef/hello.png");
            let file = AttachmentUpload::new("hello.png", vec![1]).expect("spool bytes");
            assert!(matches!(
                client
                    .restore_attachment_plan(DIRECTORY, saved_root, &target, file)
                    .await,
                Err(AttachmentError::TemporaryDirectoryChanged)
            ));
            assert_eq!(
                server.await.expect("server").len(),
                1,
                "restore only reads info"
            );
        }
    }

    #[tokio::test]
    async fn restored_windows_targets_keep_the_saved_spelling_and_real_directory_headers() {
        for (root, target) in [
            (
                "C:\\Temp",
                "c:\\Temp\\openchamber\\0123456789abcdef\\hello.png",
            ),
            (
                "\\\\server\\share\\tmp",
                "\\\\server\\share\\tmp\\openchamber\\0123456789abcdef\\hello.png",
            ),
        ] {
            let directory = "C:\\my project";
            let body = json!({"paths":{"tmp":root}}).to_string();
            let written = String::from_utf8(receipt(directory, target)).expect("UTF-8");
            let (client, server) = scripted_client(vec![
                Reply::Json {
                    status: 200,
                    body: body.clone(),
                },
                Reply::Json { status: 200, body },
                Reply::Json {
                    status: 200,
                    body: written,
                },
            ])
            .await;
            let file = AttachmentUpload::new("hello.png", vec![0xff]).expect("spool bytes");
            let plan = client
                .restore_attachment_plan(directory, root, target, file)
                .await
                .expect("restored plan");
            assert_eq!(
                plan.server_path(),
                target,
                "recovery does not rewrite the saved target"
            );
            assert_eq!(plan.write().await.expect("write").server_path(), target);
            let requests = server.await.expect("server");
            assert_eq!(requests.len(), 3);
            for request in &requests {
                assert!(request.headers.contains(&encode_directory(directory)));
            }
            let url = Url::parse(&format!("http://fixture{}", requests[2].target)).expect("URL");
            assert_eq!(url.query_pairs().next().expect("path").1, target);
        }
    }

    #[tokio::test]
    async fn write_refuses_a_root_change_after_the_plan_was_retained() {
        let (client, server) = scripted_client(vec![
            info(),
            Reply::Json {
                status: 200,
                body: json!({"paths":{"tmp":"/tmp/replaced"}}).to_string(),
            },
        ])
        .await;
        let file = AttachmentUpload::new("hello.png", vec![1]).expect("upload");
        let plan = client.plan_attachment(DIRECTORY, file).await.expect("plan");
        let target = plan.server_path().to_owned();
        assert_eq!(
            plan.write().await,
            Err(AttachmentError::TemporaryDirectoryChanged)
        );
        assert_eq!(
            plan.server_path(),
            target,
            "root refusal leaves the saved target intact"
        );
        assert_eq!(plan.bytes(), [1]);
        let requests = server.await.expect("server");
        assert_eq!(requests.len(), 2);
        assert!(
            requests
                .iter()
                .all(|request| request.headers.starts_with("GET "))
        );
    }

    #[tokio::test]
    async fn invalid_retained_targets_are_refused_before_network_io() {
        let client = client();
        for target in [
            "/workspace/hello.png",
            "/tmp/opencode/openchamber/short/hello.png",
            "../x",
        ] {
            let file = AttachmentUpload::new("hello.png", vec![1]).expect("spool bytes");
            assert!(matches!(
                client
                    .restore_attachment_plan(DIRECTORY, "/tmp/opencode", target, file)
                    .await,
                Err(AttachmentError::InvalidTarget)
            ));
        }
    }

    #[tokio::test]
    async fn failed_write_keeps_successful_siblings_and_repeated_names_get_distinct_targets() {
        let (client, server) = scripted_client(planned_write_replies(vec![
            Reply::EchoPath,
            Reply::Json {
                status: 200,
                body: "{}".to_owned(),
            },
            Reply::EchoPath,
        ]))
        .await;
        let plans = planned_files(&client, "same.txt", 3).await;
        let mut results = Vec::new();
        for plan in &plans {
            results.push(plan.write().await);
        }
        assert_eq!(results.len(), 3);
        assert!(results[0].is_ok());
        assert_eq!(results[1], Err(AttachmentError::InvalidResponse));
        assert!(results[2].is_ok());
        let requests = server.await.expect("server");
        assert_eq!(
            requests.len(),
            9,
            "allocation, root check and one write per file"
        );
        assert!(
            requests[..3]
                .iter()
                .all(|request| { request.target == "/prefix/api/info" && request.body.is_empty() }),
            "all targets exist before the first write"
        );
        let mut paths = Vec::new();
        let writes: Vec<&Recorded> = requests
            .iter()
            .filter(|request| {
                request
                    .target
                    .starts_with("/prefix/api/experimental/fs/write?")
            })
            .collect();
        assert_eq!(writes.len(), 3);
        for (index, request) in writes.iter().enumerate() {
            assert_eq!(
                request.body,
                [index as u8],
                "raw captured bytes reach the write"
            );
            assert!(
                request
                    .headers
                    .to_ascii_lowercase()
                    .contains("authorization: bearer oc_client_attachment_fixture")
            );
            assert!(request.headers.contains(&encode_directory(DIRECTORY)));
            let url = Url::parse(&format!("http://fixture{}", request.target)).expect("URL");
            let target = url
                .query_pairs()
                .find(|(key, _)| key == "path")
                .expect("path query")
                .1
                .into_owned();
            let folder = target
                .strip_prefix("/tmp/opencode/openchamber/")
                .expect("server tmp root")
                .strip_suffix("/same.txt")
                .expect("captured filename");
            assert_eq!(folder.len(), 16);
            assert!(folder.bytes().all(|byte| byte.is_ascii_hexdigit()));
            assert!(
                !paths.contains(&target),
                "repeated names never reuse a target"
            );
            assert_eq!(
                target,
                plans[index].server_path(),
                "write uses the allocated target"
            );
            paths.push(target);
        }
    }

    #[tokio::test]
    async fn info_failure_never_dispatches_a_write_and_401_uses_the_existing_watch() {
        for body in ["{}", "private server response"] {
            let (client, server) = scripted_client(vec![Reply::Json {
                status: 401,
                body: body.to_owned(),
            }])
            .await;
            let watch = client.authentication_failures();
            let file = AttachmentUpload::new("a.txt", vec![1]).expect("upload");
            assert!(matches!(
                client.plan_attachment(DIRECTORY, file).await,
                Err(AttachmentError::AuthenticationRequired)
            ));
            assert_eq!(
                *watch.borrow(),
                1,
                "401 signals before feature body parsing"
            );
            assert_eq!(server.await.expect("server").len(), 1);
        }
        let (client, server) = scripted_client(vec![Reply::Json {
            status: 200,
            body: "{}".to_owned(),
        }])
        .await;
        assert!(matches!(
            client
                .plan_attachment(
                    DIRECTORY,
                    AttachmentUpload::new("a.txt", vec![]).expect("upload")
                )
                .await,
            Err(AttachmentError::InvalidResponse)
        ));
        assert_eq!(server.await.expect("server").len(), 1);
    }

    #[tokio::test]
    async fn failed_first_or_last_write_preserves_independent_successes() {
        for failed_index in [0, 2] {
            let mut replies = Vec::new();
            for index in 0..3 {
                replies.push(if index == failed_index {
                    Reply::Json {
                        status: 400,
                        body: "private refusal".to_owned(),
                    }
                } else {
                    Reply::EchoPath
                });
            }
            let (client, server) = scripted_client(planned_write_replies(replies)).await;
            let plans = planned_files(&client, "a.txt", 3).await;
            let mut results = Vec::new();
            for plan in &plans {
                results.push(plan.write().await);
            }
            for (index, result) in results.iter().enumerate() {
                if index == failed_index {
                    assert_eq!(*result, Err(AttachmentError::Http { status: 400 }));
                } else {
                    assert!(result.is_ok(), "unrelated file still stages");
                }
            }
            assert_eq!(
                server.await.expect("server").len(),
                9,
                "no retries or skipped siblings"
            );
        }
    }

    #[tokio::test]
    async fn write_401_signals_once_and_preserves_other_file_results() {
        let (client, server) = scripted_client(planned_write_replies(vec![
            Reply::EchoPath,
            Reply::Json {
                status: 401,
                body: "not JSON".to_owned(),
            },
            Reply::EchoPath,
        ]))
        .await;
        let watch = client.authentication_failures();
        let plans = planned_files(&client, "a.txt", 3).await;
        let mut results = Vec::new();
        for plan in &plans {
            results.push(plan.write().await);
        }
        assert!(results[0].is_ok());
        assert_eq!(results[1], Err(AttachmentError::AuthenticationRequired));
        assert!(results[2].is_ok());
        assert_eq!(*watch.borrow(), 1);
        assert_eq!(server.await.expect("server").len(), 9);
    }

    #[tokio::test]
    async fn oversized_and_truncated_info_are_failed_reads() {
        for reply in [
            Reply::Json {
                status: 200,
                body: " ".repeat(MAX_RESPONSE_BYTES + 1),
            },
            Reply::ChunkedOversized,
            Reply::Truncated,
        ] {
            let (client, server) = scripted_client(vec![reply]).await;
            let result = client
                .plan_attachment(
                    DIRECTORY,
                    AttachmentUpload::new("a.txt", vec![]).expect("upload"),
                )
                .await;
            assert!(matches!(
                result,
                Err(AttachmentError::ResponseTooLarge | AttachmentError::RequestFailed)
            ));
            assert_eq!(
                server.await.expect("server").len(),
                1,
                "failed info never authorizes a write"
            );
        }
    }

    #[tokio::test]
    async fn invalid_locations_never_request_a_default_directory() {
        let client = client();
        for directory in ["", "relative", "/a/../b", " /tmp"] {
            let file = AttachmentUpload::new("a.txt", vec![]).expect("upload");
            assert!(matches!(
                client.plan_attachment(directory, file).await,
                Err(AttachmentError::InvalidDirectory)
            ));
        }
    }
}
