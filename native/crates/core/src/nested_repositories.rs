//! Which repository a Git surface works in when its directory holds others
//! (`hooks/useNestedGitDirectory.ts`, `lib/nestedRepositories.ts`).

/// Whether repositories under `root` are looked for: never in the home
/// directory, where every checkout of the machine would show.
pub fn searches_nested(root: &str, home: Option<&str>) -> bool {
    home.is_none_or(|home| root.trim_end_matches('/') != home.trim_end_matches('/'))
}

/// The repository a root that is no repository opens: the first one not
/// under a dot-directory (`chooseAutoNestedRepo`).
pub fn auto_pick<'a>(root: &str, repositories: &'a [String]) -> Option<&'a str> {
    let root = root.trim_end_matches('/');
    repositories
        .iter()
        .find(|repository| {
            !repository
                .strip_prefix(root)
                .unwrap_or(repository)
                .split('/')
                .any(|segment| segment.starts_with('.'))
        })
        .map(String::as_str)
}

/// What the repository picker lists, or `None` when there is no choice:
/// the root when it is a repository, then the discovered ones, keeping a
/// selected repository listed.
pub fn options(
    root: &str,
    root_is_repository: bool,
    discovered: &[String],
    selected: Option<&str>,
) -> Option<Vec<String>> {
    let selected = selected.filter(|selected| *selected != root);
    if discovered.is_empty() && selected.is_none() {
        return None;
    }
    let mut options: Vec<String> = Vec::new();
    let candidates = root_is_repository
        .then_some(root)
        .into_iter()
        .chain(discovered.iter().map(String::as_str))
        .chain(selected);
    for candidate in candidates {
        if !options.iter().any(|known| known == candidate) {
            options.push(candidate.to_owned());
        }
    }
    Some(options)
}

/// The picker's label: the root by its folder name, others relative to it.
pub fn label<'a>(root: &str, repository: &'a str) -> &'a str {
    if repository == root {
        return repository
            .rsplit('/')
            .find(|segment| !segment.is_empty())
            .unwrap_or(repository);
    }
    repository
        .strip_prefix(root.trim_end_matches('/'))
        .and_then(|rest| rest.strip_prefix('/'))
        .unwrap_or(repository)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn a_plain_root_opens_its_first_visible_repository() {
        let found = paths(&["/w/.cache/x", "/w/app", "/w/lib"]);
        assert_eq!(auto_pick("/w", &found), Some("/w/app"));
        assert_eq!(auto_pick("/w", &paths(&["/w/.git-x/y"])), None);
        assert!(!searches_nested("/home/me/", Some("/home/me")));
        assert!(searches_nested("/home/me/p", Some("/home/me")));
    }

    #[test]
    fn the_picker_lists_the_root_repository_first_and_keeps_the_selection() {
        let found = paths(&["/w/sub"]);
        assert_eq!(
            options("/w", true, &found, None),
            Some(paths(&["/w", "/w/sub"]))
        );
        assert_eq!(options("/w", false, &found, None), Some(paths(&["/w/sub"])));
        assert_eq!(options("/w", true, &[], None), None);
        assert_eq!(
            options("/w", false, &[], Some("/w/gone")),
            Some(paths(&["/w/gone"]))
        );
        assert_eq!(label("/w", "/w"), "w");
        assert_eq!(label("/w", "/w/a/b"), "a/b");
    }
}
