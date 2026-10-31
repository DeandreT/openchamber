//! The Files panel's filesystem routes, as `createWebFilesAPI` in
//! `packages/web/src/api/files.ts` calls them: folder listings, file name
//! and content search, stat, text reads, writes and renames.
//!
//! Every request names the workspace in `x-opencode-directory`, URI-encoded
//! and marked so, as `runtimeFetch` sends it; the server resolves paths
//! against that workspace and refuses writes outside it. Paths are absolute
//! in the server's path space. The server's write is last-writer-wins; the
//! caller protects a dirty draft (`openchamber_native_core::file_draft`).

use crate::request::RequestBuilder;
use openchamber_native_core::file_draft::FileStat;
use openchamber_native_core::file_tree::{DirectoryEntry, EntryKind, ListingFailure};
use openchamber_native_core::path;
use reqwest::header::CONTENT_TYPE;
use reqwest::{Method, Response, StatusCode};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::json;

use crate::client::{ServerClient, encode_directory};
use crate::error::TransportError;

/// The most matches a content search asks for (`CONTENT_SEARCH_MAX_LIMIT`).
pub const CONTENT_SEARCH_LIMIT: usize = 500;

/// Why a filesystem request did not succeed.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum FilesError {
    #[error("the path does not exist")]
    NotFound,
    #[error("the path is not a folder")]
    NotDirectory,
    #[error("the operating system denied access")]
    PermissionDenied,
    #[error("the destination already exists")]
    AlreadyExists,
    /// Any other answer the server declined with, and its message.
    #[error("the server refused the request (HTTP {status})")]
    Refused {
        status: u16,
        message: Option<String>,
    },
    #[error(transparent)]
    Transport(#[from] TransportError),
}

impl FilesError {
    /// The server's own explanation, when it gave one.
    pub fn message(&self) -> Option<&str> {
        match self {
            Self::Refused { message, .. } => message.as_deref(),
            _ => None,
        }
    }

    /// The tree's failure for a folder listing.
    pub fn into_listing_failure(self) -> ListingFailure {
        match self {
            Self::NotFound => ListingFailure::NotFound,
            Self::NotDirectory => ListingFailure::NotDirectory,
            Self::PermissionDenied => ListingFailure::PermissionDenied,
            Self::Refused { message, .. } => ListingFailure::Failed(message),
            Self::AlreadyExists | Self::Transport(_) => ListingFailure::Failed(None),
        }
    }
}

/// A file name search hit, relative to the workspace.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileHit {
    pub path: String,
    pub relative_path: String,
}

/// One content search match. Ranges are UTF-16 offsets into `text`.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContentMatch {
    pub path: String,
    pub relative_path: String,
    pub line: u32,
    pub column: u32,
    pub text: String,
    pub ranges: Vec<(u32, u32)>,
    pub cut_start: bool,
}

/// Why a content search stopped before it searched everything.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SearchStop {
    Limit,
    Deadline,
}

/// A content search's outcome.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ContentSearch {
    Matches {
        matches: Vec<ContentMatch>,
        stopped: Option<SearchStop>,
    },
    /// The pattern is no valid regular expression; ripgrep's reason.
    InvalidPattern(String),
    /// The server has no ripgrep.
    Unavailable,
}

/// A content search request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContentQuery {
    pub query: String,
    pub regex: bool,
    pub limit: usize,
    pub include_hidden: bool,
    pub respect_gitignore: bool,
}

/// Whether a stat found the file.
#[derive(Clone, Debug, PartialEq)]
pub enum StatRead {
    Found { path: String, stat: FileStat },
    Missing,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ListBody {
    entries: Vec<EntryBody>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct EntryBody {
    name: String,
    path: String,
    is_directory: bool,
}

#[derive(Deserialize)]
struct FindBody {
    data: Vec<FindEntry>,
}

#[derive(Deserialize)]
struct FindEntry {
    path: String,
}

#[derive(Deserialize)]
struct ContentBody {
    matches: Vec<ContentMatch>,
    stopped: Option<SearchStop>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct StatBody {
    path: String,
    exists: Option<bool>,
    size: Option<u64>,
    mtime_ms: Option<f64>,
}

#[derive(Deserialize)]
struct SuccessBody {
    success: bool,
}

#[derive(Default, Deserialize)]
struct RefusalBody {
    error: Option<String>,
    reason: Option<String>,
    code: Option<String>,
}

/// A declined answer's status and body.
#[derive(Debug)]
pub(crate) struct Refusal {
    pub status: StatusCode,
    pub message: Option<String>,
    /// The filesystem routes' `reason` or the Git routes' `code`.
    pub reason: Option<String>,
}

impl ServerClient {
    fn files_request(
        &self,
        method: Method,
        route: &str,
        directory: &str,
        query: &[(&str, &str)],
    ) -> RequestBuilder {
        let mut url = self.endpoint().url(route);
        if !query.is_empty() {
            url.query_pairs_mut().extend_pairs(query);
        }
        self.authorized(method, url)
            .header("x-opencode-directory", encode_directory(directory))
            .header("x-opencode-directory-encoding", "uri")
    }

    /// The entries of the folder `path`, gitignored ones left out when
    /// `respect_gitignore` is set.
    pub async fn list_directory(
        &self,
        directory: &str,
        path: &str,
        respect_gitignore: bool,
    ) -> Result<Vec<DirectoryEntry>, FilesError> {
        let mut query = vec![("path", path)];
        if respect_gitignore {
            query.push(("respectGitignore", "true"));
        }
        let request = self.files_request(Method::GET, "api/fs/list", directory, &query);
        let body: ListBody = files_json(request).await?;
        Ok(body
            .entries
            .into_iter()
            .map(|entry| DirectoryEntry {
                name: entry.name,
                path: entry.path,
                kind: if entry.is_directory {
                    EntryKind::Directory
                } else {
                    EntryKind::File
                },
            })
            .collect())
    }

    /// Checks a file or directory through its unfiltered parent listing.
    /// The file-stat endpoint rejects directories and cannot reconcile a
    /// directory rename. A failed read never establishes that a path is absent.
    pub async fn path_kind(
        &self,
        directory: &str,
        target: &str,
    ) -> Result<Option<EntryKind>, FilesError> {
        let parent = path::parent(target).ok_or_else(|| {
            TransportError::Malformed("path observation requires a parent directory".to_owned())
        })?;
        let entries = match self.list_directory(directory, &parent, false).await {
            Ok(entries) => entries,
            Err(FilesError::NotFound) => return Ok(None),
            Err(error) => return Err(error),
        };
        let mut found = entries.iter().filter(|entry| entry.path == target);
        let kind = found.next().map(|entry| entry.kind);
        if found.next().is_some() {
            return Err(TransportError::Malformed(
                "path observation returned duplicate entries".to_owned(),
            )
            .into());
        }
        Ok(kind)
    }

    /// Fuzzy file name search through OpenCode's file index (`file.find`).
    pub async fn find_files(
        &self,
        directory: &str,
        query: &str,
        limit: usize,
    ) -> Result<Vec<FileHit>, FilesError> {
        let limit = limit.to_string();
        let request = self.files_request(
            Method::GET,
            "api/fs/find",
            directory,
            &[("query", query), ("type", "file"), ("limit", &limit)],
        );
        let body: FindBody = files_json(request).await?;
        let root = directory.trim_end_matches('/');
        Ok(body
            .data
            .into_iter()
            .map(|entry| FileHit {
                path: format!("{root}/{}", entry.path.trim_start_matches('/')),
                relative_path: entry.path,
            })
            .collect())
    }

    /// Searches file contents with ripgrep on the server. Dropping the
    /// future closes the request, which ends the search.
    pub async fn search_contents(
        &self,
        directory: &str,
        query: &ContentQuery,
    ) -> Result<ContentSearch, FilesError> {
        let limit = query.limit.clamp(1, CONTENT_SEARCH_LIMIT).to_string();
        let mut pairs = vec![
            ("query", query.query.as_str()),
            ("regex", if query.regex { "true" } else { "false" }),
            ("limit", limit.as_str()),
        ];
        if query.include_hidden {
            pairs.push(("includeHidden", "true"));
        }
        if !query.respect_gitignore {
            pairs.push(("respectGitignore", "false"));
        }
        let request = self.files_request(Method::GET, "api/fs/find-in-files", directory, &pairs);
        let response = request.send().await.map_err(TransportError::from_request)?;
        match read_answer(response).await? {
            Ok(bytes) => {
                let body: ContentBody = parse(&bytes)?;
                Ok(ContentSearch::Matches {
                    matches: body.matches,
                    stopped: body.stopped,
                })
            }
            Err(refusal) => match (refusal.status.as_u16(), refusal.reason.as_deref()) {
                (400, Some("invalid-pattern")) => Ok(ContentSearch::InvalidPattern(
                    refusal.message.unwrap_or_default(),
                )),
                (501, Some("search-unavailable")) => Ok(ContentSearch::Unavailable),
                _ => Err(files_error(refusal)),
            },
        }
    }

    /// The size and modification time of the file `path`; a missing file is
    /// `Missing`, never an error. `outside` reads a file outside the
    /// workspace, which the editor opens read-only.
    pub async fn stat_file(
        &self,
        directory: &str,
        path: &str,
        outside: bool,
    ) -> Result<StatRead, FilesError> {
        let mut query = vec![("path", path), ("optional", "true")];
        if outside {
            query.push(("allowOutsideWorkspace", "true"));
        }
        let request = self.files_request(Method::GET, "api/fs/stat", directory, &query);
        let body: StatBody = files_json(request).await?;
        if body.exists == Some(false) {
            return Ok(StatRead::Missing);
        }
        let size = body
            .size
            .ok_or_else(|| TransportError::Malformed("a stat answer has no size".to_owned()))?;
        Ok(StatRead::Found {
            path: body.path,
            stat: FileStat {
                size,
                mtime_ms: body.mtime_ms,
            },
        })
    }

    /// The text of the file `path`, decoded as UTF-8 by the server.
    pub async fn read_file(
        &self,
        directory: &str,
        path: &str,
        outside: bool,
    ) -> Result<String, FilesError> {
        let mut query = vec![("path", path)];
        if outside {
            query.push(("allowOutsideWorkspace", "true"));
        }
        let request = self.files_request(Method::GET, "api/fs/read", directory, &query);
        let response = request.send().await.map_err(TransportError::from_request)?;
        let is_text = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.starts_with("text/plain"));
        match read_answer(response).await? {
            Ok(bytes) if is_text => String::from_utf8(bytes).map_err(|_| {
                TransportError::Malformed("the file text is not UTF-8".to_owned()).into()
            }),
            Ok(_) => {
                Err(TransportError::Malformed("the file answer is not text".to_owned()).into())
            }
            Err(refusal) => Err(files_error(refusal)),
        }
    }

    /// Writes `content` to `path`, replacing the file atomically.
    pub async fn write_file(
        &self,
        directory: &str,
        path: &str,
        content: &str,
    ) -> Result<(), FilesError> {
        let request = self
            .files_request(Method::POST, "api/fs/write", directory, &[])
            .json(&json!({ "path": path, "content": content }));
        expect_success(request).await
    }

    /// Renames or moves `old_path` to `new_path`; an existing destination is
    /// `AlreadyExists` and nothing is replaced.
    pub async fn rename_path(
        &self,
        directory: &str,
        old_path: &str,
        new_path: &str,
    ) -> Result<(), FilesError> {
        let request = self
            .files_request(Method::POST, "api/fs/rename", directory, &[])
            .json(&json!({ "oldPath": old_path, "newPath": new_path }));
        expect_success(request).await
    }
}

async fn files_json<T: DeserializeOwned>(request: RequestBuilder) -> Result<T, FilesError> {
    let response = request.send().await.map_err(TransportError::from_request)?;
    match read_answer(response).await? {
        Ok(bytes) => Ok(parse(&bytes)?),
        Err(refusal) => Err(files_error(refusal)),
    }
}

async fn expect_success(request: RequestBuilder) -> Result<(), FilesError> {
    let body: SuccessBody = files_json(request).await?;
    if body.success {
        Ok(())
    } else {
        Err(TransportError::Malformed("the server did not confirm the change".to_owned()).into())
    }
}

fn files_error(refusal: Refusal) -> FilesError {
    match (refusal.status.as_u16(), refusal.reason.as_deref()) {
        (_, Some("not-found")) | (404, _) => FilesError::NotFound,
        (_, Some("not-directory")) => FilesError::NotDirectory,
        (_, Some("os-permission")) => FilesError::PermissionDenied,
        (_, Some("already-exists")) | (409, _) => FilesError::AlreadyExists,
        (status, _) => FilesError::Refused {
            status,
            message: refusal.message,
        },
    }
}

pub(crate) fn parse<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, TransportError> {
    serde_json::from_slice(bytes).map_err(|error| TransportError::Malformed(error.to_string()))
}

/// Reads an answer: the body of a success, or what the server said when it
/// declined. Redirects and missing authentication stay transport errors; a
/// 403 the server explained in JSON is its refusal, since the filesystem
/// routes answer denied access that way.
pub(crate) async fn read_answer(
    response: Response,
) -> Result<Result<Vec<u8>, Refusal>, TransportError> {
    let status = response.status();
    if status.is_redirection() {
        return Err(TransportError::Redirected);
    }
    if status == StatusCode::UNAUTHORIZED {
        return Err(TransportError::AuthenticationRequired);
    }
    let is_json = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("application/json"));
    let bytes = response
        .bytes()
        .await
        .map_err(TransportError::from_request)?
        .to_vec();
    if status.is_success() {
        return Ok(Ok(bytes));
    }
    let body: Option<RefusalBody> = is_json
        .then(|| serde_json::from_slice(&bytes).ok())
        .flatten();
    if status == StatusCode::FORBIDDEN && body.as_ref().is_none_or(|body| body.error.is_none()) {
        return Err(TransportError::AuthenticationRequired);
    }
    let body = body.unwrap_or_default();
    Ok(Err(Refusal {
        status,
        message: body.error,
        reason: body.reason.or(body.code),
    }))
}
