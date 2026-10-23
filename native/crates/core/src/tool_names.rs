//! OpenCode's built-in tool names, as `packages/ui/src/lib/opencode/tools.ts`
//! compares them.

/// Tools that write files: their results change the working tree.
const FILE_CHANGE_TOOLS: [&str; 3] = ["edit", "write", "patch"];

/// A tool name as compared: lowercased and trimmed, without a trailing
/// `:index` suffix, and reduced to the last segment of a namespaced name
/// (`opencode.session_rename` is `session_rename`).
pub fn normalize(tool: &str) -> String {
    let trimmed = tool.trim().to_lowercase();
    let without_index = match trimmed.rsplit_once(':') {
        Some((head, index))
            if !index.is_empty() && index.chars().all(|digit| digit.is_ascii_digit()) =>
        {
            head
        }
        _ => trimmed.as_str(),
    };
    if !without_index.contains('.') {
        return without_index.to_owned();
    }
    without_index
        .split('.')
        .rfind(|segment| !segment.is_empty())
        .unwrap_or(without_index)
        .to_owned()
}

/// Whether a finished call of `tool` may have changed what Git reports:
/// file edits, shell commands and Code Mode scripts (`isGitMutatingTool`).
pub fn may_change_git(tool: &str) -> bool {
    let name = normalize(tool);
    FILE_CHANGE_TOOLS.contains(&name.as_str()) || name == "shell" || name == "execute"
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
