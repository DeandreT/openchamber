//! OpenCode's built-in tool names, as `packages/ui/src/lib/opencode/tools.ts`
//! compares them.

use std::borrow::Cow;

/// Tools that write files: their results change the working tree.
const FILE_CHANGE_TOOLS: [&str; 3] = ["edit", "write", "patch"];

/// A tool name as compared: lowercased and trimmed, without a trailing
/// `:index` suffix, and reduced to the last segment of a namespaced name
/// (`opencode.session_rename` is `session_rename`). Borrows when the name
/// needs no lowercasing, so rows can compare names while they draw.
pub fn normalize(tool: &str) -> Cow<'_, str> {
    let trimmed = tool.trim();
    let lowered: Cow<'_, str> = if trimmed.chars().any(char::is_uppercase) {
        Cow::Owned(trimmed.to_lowercase())
    } else {
        Cow::Borrowed(trimmed)
    };
    let without_index = match lowered.rfind(':') {
        Some(colon)
            if colon + 1 < lowered.len()
                && lowered[colon + 1..]
                    .bytes()
                    .all(|byte| byte.is_ascii_digit()) =>
        {
            cut(lowered, colon)
        }
        _ => lowered,
    };
    let last_segment = |text: &str| -> usize {
        text.split('.')
            .rfind(|segment| !segment.is_empty())
            .map_or(0, str::len)
    };
    if !without_index.contains('.') {
        return without_index;
    }
    let length = last_segment(&without_index);
    if length == 0 {
        return without_index;
    }
    // The last non-empty segment ends at the last non-dot character.
    let end = without_index.trim_end_matches('.').len();
    match without_index {
        Cow::Borrowed(text) => Cow::Borrowed(&text[end - length..end]),
        Cow::Owned(text) => Cow::Owned(text[end - length..end].to_owned()),
    }
}

fn cut(value: Cow<'_, str>, end: usize) -> Cow<'_, str> {
    match value {
        Cow::Borrowed(text) => Cow::Borrowed(&text[..end]),
        Cow::Owned(mut text) => {
            text.truncate(end);
            Cow::Owned(text)
        }
    }
}

/// Whether a finished call of `tool` may have changed what Git reports:
/// file edits, shell commands and Code Mode scripts (`isGitMutatingTool`).
pub fn may_change_git(tool: &str) -> bool {
    let name = normalize(tool);
    FILE_CHANGE_TOOLS.contains(&name.as_ref()) || name == "shell" || name == "execute"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_compare_as_the_web_compares_them() {
        assert_eq!(normalize("  Shell "), "shell");
        assert_eq!(normalize("edit:2"), "edit");
        assert_eq!(normalize("opencode.session_rename"), "session_rename");
        assert_eq!(normalize("mcp.server.tool:1"), "tool");
        assert_eq!(normalize("a:b"), "a:b", "only a numeric suffix is an index");
    }

    #[test]
    fn edits_commands_and_scripts_change_git() {
        for tool in [
            "edit",
            "write",
            "patch",
            "shell",
            "execute",
            "opencode.write:3",
        ] {
            assert!(may_change_git(tool), "{tool}");
        }
        for tool in ["read", "grep", "subagent", "websearch"] {
            assert!(!may_change_git(tool), "{tool}");
        }
    }
}
