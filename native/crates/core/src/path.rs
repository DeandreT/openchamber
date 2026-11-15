//! Lexical directory normalization shared by ownership and routing.
//!
//! Mirrors `packages/ui/src/lib/pathNormalization.ts`. Normalization is purely
//! lexical: it never touches the filesystem, so canonical aliases (case
//! folding, symlinks) come from the server.

/// Normalizes a directory for comparison.
///
/// Converts backslashes to forward slashes, uppercases a leading lowercase
/// drive letter and trims trailing slashes while preserving `/` and Windows
/// drive roots such as `C:/`. Returns `None` for empty, whitespace-only and
/// slash-only input other than `/`.
pub fn normalize(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }

    let mut replaced = trimmed.replace('\\', "/");
    let bytes = replaced.as_bytes();
    if bytes.len() >= 2 && bytes[0].is_ascii_lowercase() && bytes[1] == b':' {
        replaced.replace_range(0..1, &replaced[0..1].to_ascii_uppercase());
    }

    if replaced == "/" {
        return Some(replaced);
    }
    // A Windows drive root is absolute; removing its slash would turn it into a
    // drive-relative path whose meaning depends on that drive's current folder.
    if is_drive_root(&replaced) {
        let end = replaced.trim_end_matches('/').len();
        replaced.truncate(end + 1);
        return Some(replaced);
    }
    let stripped = if replaced.len() > 1 {
        replaced.trim_end_matches('/')
    } else {
        replaced.as_str()
    };
    if stripped.is_empty() {
        None
    } else {
        Some(stripped.to_owned())
    }
}

/// Normalizes an optional directory; `None` and unusable values both yield `None`.
pub fn normalize_optional(value: Option<&str>) -> Option<String> {
    value.and_then(normalize)
}

/// The lexical parent of an already-normalized directory.
///
/// Filesystem roots (`/`, drive roots, drive prefixes and UNC share roots)
/// have no parent.
pub fn parent(directory: &str) -> Option<String> {
    if directory == "/"
        || is_drive_root(directory)
        || is_drive_prefix(directory)
        || is_unc_share_root(directory)
    {
        return None;
    }
    let separator = directory.rfind('/')?;
    if separator == 0 {
        return Some("/".to_owned());
    }
    let parent = &directory[..separator];
    if is_drive_prefix(parent) {
        Some(format!("{parent}/"))
    } else {
        Some(parent.to_owned())
    }
}

/// Whether `directory` equals `root` or lies lexically inside it.
///
/// Paths containing `.` or `..` segments never match, so a crafted relative
/// segment cannot escape a root.
pub fn is_within(directory: &str, root: &str) -> bool {
    let has_relative_segment = directory.split('/').any(|part| part == "." || part == "..");
    if has_relative_segment {
        return false;
    }
    directory == root
        || directory
            .strip_prefix(root)
            .is_some_and(|rest| rest.starts_with('/'))
}

/// Strips an optional `//?/` device prefix.
fn strip_device_prefix(value: &str) -> &str {
    value.strip_prefix("//?/").unwrap_or(value)
}

/// Matches `X:` optionally preceded by `//?/`.
fn is_drive_prefix(value: &str) -> bool {
    let bytes = strip_device_prefix(value).as_bytes();
    bytes.len() == 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':'
}

/// Matches `X:/`, `X://` and so on, optionally preceded by `//?/`.
fn is_drive_root(value: &str) -> bool {
    let rest = strip_device_prefix(value);
    let bytes = rest.as_bytes();
    bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && bytes[2..].iter().all(|byte| *byte == b'/')
}

/// Matches `//server/share` and `//?/UNC/server/share`.
fn is_unc_share_root(value: &str) -> bool {
    let Some(rest) = value.strip_prefix("//") else {
        return false;
    };
    let unc = rest
        .get(..4)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("?/UN"))
        && rest
            .get(4..6)
            .is_some_and(|suffix| suffix.eq_ignore_ascii_case("C/"));
    if unc {
        let segments: Vec<&str> = rest[6..].split('/').collect();
        return segments.len() == 2 && segments.iter().all(|segment| !segment.is_empty());
    }
    let mut segments = rest.splitn(2, '/');
    let (Some(server), Some(share)) = (segments.next(), segments.next()) else {
        return false;
    };
    !server.is_empty() && !server.starts_with('?') && !share.is_empty() && !share.contains('/')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_separators_drive_letters_and_trailing_slashes() {
        assert_eq!(normalize("c:\\Users\\me\\").as_deref(), Some("C:/Users/me"));
        assert_eq!(normalize("/home/me///").as_deref(), Some("/home/me"));
        assert_eq!(normalize("  /tmp  ").as_deref(), Some("/tmp"));
        assert_eq!(normalize("abc:def").as_deref(), Some("abc:def"));
    }

    #[test]
    fn preserves_roots() {
        assert_eq!(normalize("/").as_deref(), Some("/"));
        assert_eq!(normalize("c:\\").as_deref(), Some("C:/"));
        assert_eq!(normalize("C://").as_deref(), Some("C:/"));
        assert_eq!(normalize("//?/C:/").as_deref(), Some("//?/C:/"));
    }

    #[test]
    fn rejects_unusable_values() {
        assert_eq!(normalize(""), None);
        assert_eq!(normalize("   "), None);
        assert_eq!(normalize("\\\\"), None);
        assert_eq!(normalize("///"), None);
    }

    #[test]
    fn walks_lexical_parents_to_the_root() {
        assert_eq!(parent("/home/me").as_deref(), Some("/home"));
        assert_eq!(parent("/home").as_deref(), Some("/"));
        assert_eq!(parent("/"), None);
        assert_eq!(parent("C:/projects").as_deref(), Some("C:/"));
        assert_eq!(parent("C:/"), None);
        assert_eq!(parent("C:"), None);
        assert_eq!(parent("//server/share"), None);
        assert_eq!(
            parent("//server/share/dir").as_deref(),
            Some("//server/share")
        );
        assert_eq!(parent("//?/UNC/server/share"), None);
        assert_eq!(parent("relative"), None);
    }

    #[test]
    fn containment_rejects_relative_segments_and_sibling_prefixes() {
        assert!(is_within("/a/b", "/a"));
        assert!(is_within("/a", "/a"));
        assert!(!is_within("/ab", "/a"));
        assert!(!is_within("/a/../b", "/a"));
        assert!(!is_within("/a/./b", "/a"));
    }
}
