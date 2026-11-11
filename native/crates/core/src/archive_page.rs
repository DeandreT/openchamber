//! What the archive page lists: its project buckets, its search and the
//! order of its rows.
//!
//! Ports the list logic of `ArchiveSessionsView` in
//! `packages/ui/src/components/views/ArchiveView.tsx`. The caller passes the
//! sessions [`crate::session_actions::archive_page`] admits, most recently
//! archived first, and says which registered project a session belongs to.
//! Buckets for deleted isolated spaces and archived worktrees are not built:
//! the native client reads neither.

use std::collections::{HashMap, HashSet};

use crate::ids::SessionId;
use crate::search_rank;
use crate::session::SessionRecord;

/// How many more rows each Show more reveals, and how many show at first.
pub const PAGE_SIZE: usize = 100;

/// The key of the bucket for sessions outside every registered project. It
/// cannot collide with a project root, which is a path.
pub const OTHER_BUCKET_KEY: &str = "\u{0}other";

/// What a bucket groups.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum BucketKind {
    /// A registered project with its worktrees.
    Project,
    /// Everything else.
    Other,
}

/// A group in the page's filter panel.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArchiveBucket<'a> {
    /// The project's root, or [`OTHER_BUCKET_KEY`].
    pub key: String,
    pub kind: BucketKind,
    pub label: String,
    /// Most recently archived first.
    pub sessions: Vec<&'a SessionRecord>,
}

/// Groups `sessions` by the registered project `project_root_of` names;
/// sessions it places nowhere share the `other_label` bucket. Projects come
/// first, by label, then the rest.
pub fn buckets<'a>(
    sessions: &[&'a SessionRecord],
    project_root_of: impl Fn(&SessionRecord) -> Option<String>,
    project_label: impl Fn(&str) -> String,
    other_label: &str,
) -> Vec<ArchiveBucket<'a>> {
    let mut by_key: HashMap<String, usize> = HashMap::new();
    let mut buckets: Vec<ArchiveBucket<'a>> = Vec::new();
    for session in sessions {
        let (key, kind) = match project_root_of(session) {
            Some(root) => (root, BucketKind::Project),
            None => (OTHER_BUCKET_KEY.to_owned(), BucketKind::Other),
        };
        let position = match by_key.get(&key) {
            Some(position) => *position,
            None => {
                let label = match kind {
                    BucketKind::Project => project_label(&key),
                    BucketKind::Other => other_label.to_owned(),
                };
                by_key.insert(key.clone(), buckets.len());
                buckets.push(ArchiveBucket {
                    key,
                    kind,
                    label,
                    sessions: Vec::new(),
                });
                buckets.len() - 1
            }
        };
        buckets[position].sessions.push(*session);
    }
    buckets.sort_by_cached_key(|bucket| (bucket.kind, bucket.label.to_lowercase()));
    buckets
}

/// `groupByParent`: subsessions of one parent sit together, at the place of
/// the first of them.
pub fn group_by_parent<'a>(sessions: &[&'a SessionRecord]) -> Vec<&'a SessionRecord> {
    let mut by_parent: HashMap<&SessionId, Vec<&'a SessionRecord>> = HashMap::new();
    for session in sessions {
        if let Some(parent) = &session.parent_id {
            by_parent.entry(parent).or_default().push(*session);
        }
    }
    let mut grouped = Vec::with_capacity(sessions.len());
    let mut placed: HashSet<&SessionId> = HashSet::new();
    for session in sessions {
        match &session.parent_id {
            None => grouped.push(*session),
            Some(parent) => {
                if placed.insert(parent) {
                    grouped.extend(by_parent.get(parent).into_iter().flatten().copied());
                }
            }
        }
    }
    grouped
}

/// The rows of the page. A query spans every archived session: an `ses_`
/// query finds the session with that ID, any other text ranks titles as the
/// pickers do. Without a query the rows are `bucket`'s, or every session's
/// when no bucket is chosen.
pub fn rows<'a>(
    sessions: &[&'a SessionRecord],
    query: &str,
    bucket: Option<&ArchiveBucket<'a>>,
) -> Vec<&'a SessionRecord> {
    let query = query.trim().to_lowercase();
    if query.starts_with("ses_") {
        return sessions
            .iter()
            .copied()
            .filter(|session| session.id.as_str().to_lowercase() == query)
            .collect();
    }
    if !query.is_empty() {
        let ranked: Vec<&'a SessionRecord> = search_rank::rank_by_query(
            sessions,
            &query,
            |session| vec![Some(session.title.as_str())],
            true,
        )
        .into_iter()
        .map(|index| sessions[index])
        .collect();
        return group_by_parent(&ranked);
    }
    match bucket {
        Some(bucket) => group_by_parent(&bucket.sessions),
        None => group_by_parent(sessions),
    }
}

/// `parentTitleAbove`: the parent whose title shows above row `index`, the
/// first of that parent's subsessions in a run.
pub fn parent_above<'r>(rows: &[&'r SessionRecord], index: usize) -> Option<&'r SessionId> {
    let parent = rows.get(index)?.parent_id.as_ref()?;
    let previous = index
        .checked_sub(1)
        .and_then(|previous| rows.get(previous))
        .and_then(|row| row.parent_id.as_ref());
    (previous != Some(parent)).then_some(parent)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(id: &str, title: &str, directory: &str, parent: Option<&str>) -> SessionRecord {
        let mut record = SessionRecord::new(id, directory);
        record.title = title.to_owned();
        record.parent_id = parent.map(SessionId::from);
        record
    }

    fn ids(rows: &[&SessionRecord]) -> Vec<String> {
        rows.iter().map(|row| row.id.to_string()).collect()
    }

    fn root_of(session: &SessionRecord) -> Option<String> {
        let directory = session.directory.as_deref()?;
        ["/work/beta", "/work/Alpha"]
            .into_iter()
            .find(|root| directory.starts_with(root))
            .map(str::to_owned)
    }

    fn label_of(root: &str) -> String {
        root.rsplit('/').next().unwrap_or(root).to_owned()
    }

    #[test]
    fn projects_come_first_by_label_and_the_rest_share_one_bucket() {
        let records = [
            session("s1", "One", "/work/beta", None),
            session("s2", "Two", "/elsewhere", None),
            session("s3", "Three", "/work/Alpha/tree", None),
            session("s4", "Four", "/work/beta/sub", None),
            session("s5", "Five", "/gone", None),
        ];
        let sessions: Vec<&SessionRecord> = records.iter().collect();
        let buckets = buckets(&sessions, root_of, label_of, "Other");
        let summary: Vec<(&str, BucketKind, Vec<String>)> = buckets
            .iter()
            .map(|bucket| (bucket.label.as_str(), bucket.kind, ids(&bucket.sessions)))
            .collect();
        assert_eq!(
            summary,
            [
                ("Alpha", BucketKind::Project, vec!["s3".to_owned()]),
                (
                    "beta",
                    BucketKind::Project,
                    vec!["s1".to_owned(), "s4".to_owned()]
                ),
                (
                    "Other",
                    BucketKind::Other,
                    vec!["s2".to_owned(), "s5".to_owned()]
                ),
            ]
        );
        assert_eq!(buckets[2].key, OTHER_BUCKET_KEY);
        assert_eq!(buckets[1].key, "/work/beta");
    }

    #[test]
    fn subsessions_gather_at_the_first_of_them() {
        let records = [
            session("c1", "Child one", "/p", Some("parent")),
            session("r1", "Root", "/p", None),
            session("c2", "Child two", "/p", Some("parent")),
            session("d1", "Other child", "/p", Some("other")),
        ];
        let sessions: Vec<&SessionRecord> = records.iter().collect();
        let grouped = group_by_parent(&sessions);
        assert_eq!(ids(&grouped), ["c1", "c2", "r1", "d1"]);
        let parents: Vec<Option<&str>> = (0..grouped.len())
            .map(|index| parent_above(&grouped, index).map(SessionId::as_str))
            .collect();
        assert_eq!(parents, [Some("parent"), None, None, Some("other")]);
        assert_eq!(parent_above(&grouped, 9), None);
    }

    #[test]
    fn a_query_spans_every_bucket_and_an_id_query_finds_one_session() {
        let records = [
            session("ses_ABC", "Relay limits", "/work/beta", None),
            session("ses_def", "Docs", "/elsewhere", None),
            session("ses_ghi", "Relay retries", "/work/Alpha", None),
        ];
        let sessions: Vec<&SessionRecord> = records.iter().collect();
        let buckets = buckets(&sessions, root_of, label_of, "Other");
        let alpha = &buckets[0];
        assert_eq!(ids(&rows(&sessions, "", Some(alpha))), ["ses_ghi"]);
        assert_eq!(
            ids(&rows(&sessions, "  ", None)),
            ["ses_ABC", "ses_def", "ses_ghi"]
        );
        assert_eq!(
            ids(&rows(&sessions, " Relay ", Some(alpha))),
            ["ses_ABC", "ses_ghi"],
            "a search ignores the chosen bucket"
        );
        assert_eq!(ids(&rows(&sessions, "SES_abc", None)), ["ses_ABC"]);
        assert!(rows(&sessions, "ses_ab", None).is_empty());
        assert!(rows(&sessions, "zzzzzz", None).is_empty());
    }
}
