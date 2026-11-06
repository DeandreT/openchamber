//! Decodes advertised Linux clipboard file lists without accessing the files.

use std::path::PathBuf;

use gpui::ExternalPaths;
use smallvec::SmallVec;
use url::Url;

pub(super) const URI_LIST_MIME_TYPE: &str = "text/uri-list";
pub(super) const GNOME_COPIED_FILES_MIME_TYPE: &str = "x-special/gnome-copied-files";

// Bounds clipboard metadata, independently of file sizes. The entry count matches
// the native application's MAX_ENTRIES admission bound.
pub(super) const MAX_FILE_LIST_BYTES: usize = 16 * 1024 * 1024;
pub(super) const MAX_FILE_LIST_ENTRIES: usize = 4096;

/// Returns local absolute paths only for a supported, nonempty file-list payload.
///
/// As in the X11 and Wayland drop handlers, invalid URIs are skipped individually.
/// Invalid UTF-8, an invalid GNOME operation, or an exceeded resource bound rejects
/// the entire payload. Percent-encoded non-UTF-8 Unix paths remain lossless.
pub(super) fn parse_file_list(mime_type: &str, bytes: &[u8]) -> Option<ExternalPaths> {
    if bytes.len() > MAX_FILE_LIST_BYTES {
        return None;
    }
    let mut lines = std::str::from_utf8(bytes).ok()?.lines();
    match mime_type {
        URI_LIST_MIME_TYPE => {}
        GNOME_COPIED_FILES_MIME_TYPE => match lines.next()? {
            "copy" | "cut" => {}
            _ => return None,
        },
        _ => return None,
    }

    let mut paths: SmallVec<[PathBuf; 2]> = SmallVec::new();
    let mut entries = 0;
    for line in lines {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        entries += 1;
        if entries > MAX_FILE_LIST_ENTRIES {
            return None;
        }
        if let Some(path) = local_file_path(line) {
            paths.push(path);
        }
    }
    if paths.is_empty() {
        None
    } else {
        Some(ExternalPaths(paths))
    }
}

fn local_file_path(uri: &str) -> Option<PathBuf> {
    let (scheme, rest) = uri.split_once(':')?;
    if !scheme.eq_ignore_ascii_case("file") || !rest.starts_with('/') {
        // Url accepts file:relative as an absolute file URL. A copied file must
        // supply its absolute path rather than relying on that normalization.
        return None;
    }
    if let Some(authority_and_path) = rest.strip_prefix("//") {
        // An authority without an explicit path would be normalized to `/`.
        if !authority_and_path.contains('/') {
            return None;
        }
    }
    let mut bytes = uri.bytes();
    while let Some(byte) = bytes.next() {
        // Url tolerates malformed escapes and strips some raw whitespace. Reject
        // these before conversion so they cannot name a different local file.
        if byte.is_ascii_control() || byte.is_ascii_whitespace() || byte == b'\\' {
            return None;
        }
        if byte == b'%'
            && !(bytes.next()?.is_ascii_hexdigit() && bytes.next()?.is_ascii_hexdigit())
        {
            return None;
        }
    }
    let url = Url::parse(uri).ok()?;
    if url.query().is_some() || url.fragment().is_some() {
        return None;
    }
    // to_file_path rejects non-local authorities on Unix and decodes URI bytes
    // directly into the Unix OsString, including percent-encoded non-UTF-8.
    let path = url.to_file_path().ok()?;
    use std::os::unix::ffi::OsStrExt as _;
    if !path.is_absolute() || path.as_os_str().as_bytes().contains(&0) {
        return None;
    }
    Some(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uri_list_decodes_comments_crlf_percent_and_unicode() {
        let paths = parse_file_list(
            URI_LIST_MIME_TYPE,
            concat!(
                "# file manager selection\r\n\r\n",
                "file:///tmp/a%20b%23c%25.txt\r\n",
                "file://localhost/tmp/%E9%9B%AA.txt\r\n",
                "file:///tmp/雪.txt\n",
                "FILE:/tmp/absolute\n",
            )
            .as_bytes(),
        )
        .expect("a URI list with local files must decode");
        assert_eq!(
            paths.paths(),
            &[
                PathBuf::from("/tmp/a b#c%.txt"),
                PathBuf::from("/tmp/雪.txt"),
                PathBuf::from("/tmp/雪.txt"),
                PathBuf::from("/tmp/absolute"),
            ],
            "URI decoding must preserve file names and input order"
        );
    }

    #[test]
    fn gnome_copy_and_cut_decode_the_same_paths() {
        for operation in ["copy", "cut"] {
            for eol in ["\n", "\r\n"] {
                let bytes = format!("{operation}{eol}# comment{eol}file:///tmp/a{eol}file:///tmp/b{eol}");
                let paths = parse_file_list(GNOME_COPIED_FILES_MIME_TYPE, bytes.as_bytes())
                    .expect("both GNOME operations must expose files without moving them");
                assert_eq!(
                    paths.paths(),
                    &[PathBuf::from("/tmp/a"), PathBuf::from("/tmp/b")]
                );
            }
        }
        for bytes in [
            b"move\nfile:///tmp/a".as_slice(),
            b"file:///tmp/a",
            b"copy",
            b"cut\n",
        ] {
            assert!(
                parse_file_list(GNOME_COPIED_FILES_MIME_TYPE, bytes).is_none(),
                "an invalid or empty GNOME payload must not expose files"
            );
        }
    }

    #[test]
    fn invalid_entries_do_not_hide_valid_siblings() {
        let bytes = concat!(
            "https://example.com/a\nfile://remotehost/tmp/a\nrelative.txt\n/tmp/a\n",
            "file:relative\nfile://\nfile://localhost\n",
            "file:///tmp/bad%\nfile:///tmp/bad%GG\nfile:///tmp/nul%00\n",
            "file:///tmp/a?query\nfile:///tmp/a#fragment\nfile:///tmp/raw space\n",
            "file:///tmp/raw\\slash\nfile:///tmp/ta\tb\nfile:///tmp/good\n",
        );
        let paths = parse_file_list(URI_LIST_MIME_TYPE, bytes.as_bytes())
            .expect("a valid local sibling must survive invalid entries");
        assert_eq!(paths.paths(), &[PathBuf::from("/tmp/good")]);
    }

    #[test]
    fn empty_wrong_mime_and_unusable_payloads_have_no_paths() {
        for bytes in [
            b"".as_slice(),
            b"# comment\r\n\r\n",
            b"https://example.com/a",
            b"file://remotehost/tmp/a",
            b"file:relative",
            b"file://",
            b"file://localhost",
            b"file:///tmp/a\n\xff",
        ] {
            assert!(
                parse_file_list(URI_LIST_MIME_TYPE, bytes).is_none(),
                "empty or unusable lists must not become clipboard strings"
            );
        }
        for mime in ["text/plain", "UTF8_STRING", "text/uri-list;charset=utf-8"] {
            assert!(
                parse_file_list(mime, b"file:///tmp/a").is_none(),
                "only an actual supported file-list MIME may expose paths"
            );
        }
    }

    #[test]
    fn unix_paths_round_trip_through_the_existing_url_converter() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt as _;

        for path in [
            PathBuf::from("/tmp/space # percent% 雪\n.txt"),
            PathBuf::from(OsString::from_vec(b"/tmp/non-utf8-\xff.txt".to_vec())),
        ] {
            let uri = Url::from_file_path(&path).expect("absolute Unix paths must encode");
            let paths = parse_file_list(URI_LIST_MIME_TYPE, uri.as_str().as_bytes())
                .expect("encoded Unix paths must decode");
            assert_eq!(
                paths.paths(),
                &[path],
                "URI conversion must preserve Unix path bytes"
            );
        }
    }

    #[test]
    fn entry_limit_rejects_the_whole_list_without_truncation() {
        let mut bytes = "file:///tmp/a\n".repeat(MAX_FILE_LIST_ENTRIES).into_bytes();
        assert_eq!(
            parse_file_list(URI_LIST_MIME_TYPE, &bytes)
                .expect("a list at the entry bound must decode")
                .paths()
                .len(),
            MAX_FILE_LIST_ENTRIES
        );
        bytes.extend_from_slice(b"https://example.com/over-limit\n");
        assert!(
            parse_file_list(URI_LIST_MIME_TYPE, &bytes).is_none(),
            "invalid and remote entries must also count toward the input bound"
        );
    }

    #[test]
    fn byte_limit_counts_comments_and_rejects_the_whole_payload() {
        let uri = b"\nfile:///tmp/a\n";
        let mut bytes = vec![b'#'; MAX_FILE_LIST_BYTES - uri.len()];
        bytes.extend_from_slice(uri);
        assert!(parse_file_list(URI_LIST_MIME_TYPE, &bytes).is_some());
        bytes.push(b'\n');
        assert!(
            parse_file_list(URI_LIST_MIME_TYPE, &bytes).is_none(),
            "bytes beyond the metadata bound must not yield a truncated selection"
        );
    }
}
