//! The sidebar's session search.
//!
//! Ports `matchesRankQuery` with `SIDEBAR_SEARCH` from
//! `packages/ui/src/lib/search/fuzzySearch.ts` and the node filter of
//! `useSessionGrouping.ts`: every word of the query must start a word of the
//! session's title or directory; a query starting with `ses_` finds one
//! session by its exact ID. A matching session keeps its whole subtree; a
//! session that does not match stays when one of its descendants does.

use crate::grouping::SessionNode;

/// The query's lowercase words.
fn tokens(query: &str) -> Vec<String> {
    query
        .trim()
        .to_lowercase()
        .split_whitespace()
        .map(str::to_owned)
        .collect()
}

/// `compactText`: the ASCII letters and digits only.
fn compact(text: &str) -> String {
    text.chars()
        .filter(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit())
        .collect()
}

/// `matchesTokenAtWordStart`: the token starts at the field's start or after
/// a character that is no letter or digit, in any script; punctuation inside
/// the token may be left out from a word start on.
fn starts_a_word(token: &str, field: &str) -> bool {
    let compact_token = compact(token);
    let mut previous: Option<char> = None;
    for (index, ch) in field.char_indices() {
        let boundary = previous.is_none_or(|before| !before.is_alphanumeric());
        previous = Some(ch);
        if !boundary {
            continue;
        }
        let rest = &field[index..];
        if rest.starts_with(token) {
            return true;
        }
        if token.chars().count() > 1
            && !compact_token.is_empty()
            && compact(rest).starts_with(&compact_token)
        {
            return true;
        }
    }
    false
}

/// Whether every word of `query` starts a word of `text`.
pub fn matches(text: &str, query: &str) -> bool {
    let field = text.to_lowercase();
    tokens(query)
        .iter()
        .all(|token| starts_a_word(token, &field))
}

/// What a query looks for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SearchQuery {
    /// Words of a title or path; `untitled` is the title an untitled
    /// session is searched by.
    Words { query: String, untitled: String },
    /// One session by its exact ID.
    Id(String),
}

impl SearchQuery {
    /// `None` for a query with nothing in it.
    pub fn parse(query: &str, untitled: &str) -> Option<Self> {
        let normalized = query.trim().to_lowercase();
        if normalized.is_empty() {
            return None;
        }
        Some(if normalized.starts_with("ses_") {
            Self::Id(normalized)
        } else {
            Self::Words {
                query: query.to_owned(),
                untitled: untitled.to_owned(),
            }
        })
    }
}

/// `filterSessionNodesForSearch`.
pub fn filter_nodes<'a>(nodes: &[SessionNode<'a>], query: &SearchQuery) -> Vec<SessionNode<'a>> {
    nodes
        .iter()
        .filter_map(|node| {
            let session = node.session;
            let hit = match query {
                SearchQuery::Id(id) => {
                    if session.is_archived() {
                        return None;
                    }
                    session.id.as_str().to_lowercase() == *id
                }
                SearchQuery::Words { query, untitled } => {
                    let title = if session.title.trim().is_empty() {
                        untitled.as_str()
                    } else {
                        session.title.trim()
                    };
                    let directory = crate::path::normalize_optional(session.directory.as_deref())
                        .unwrap_or_default();
                    matches(&format!("{title} {directory}"), query)
                }
            };
            if hit {
                return Some(node.clone());
            }
            let children = filter_nodes(&node.children, query);
            (!children.is_empty()).then_some(SessionNode { session, children })
        })
        .collect()
}

/// The sessions a filtered list shows, for the match count.
pub fn count(nodes: &[SessionNode<'_>]) -> usize {
    nodes.iter().map(|node| 1 + count(&node.children)).sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::SessionRecord;

    #[test]
    fn words_match_at_word_starts_only() {
        assert!(matches("Fix login", "fix"));
        assert!(matches("bugfix/fix-relay", "fix"));
        assert!(!matches("prefix", "fix"));
        assert!(matches(
            "openchamber/sidebar-custom-sections",
            "sidebarcustom"
        ));
        assert!(matches("Relay limits /home/u/app", "app relay"));
        assert!(!matches("Relay limits", "relay quota"));
        assert!(
            matches("Ошибка входа", "вход"),
            "a word start in any script"
        );
    }

    #[test]
    fn a_parent_stays_for_a_matching_child_and_a_match_keeps_its_subtree() {
        let mut parent = SessionRecord::new("p", "/p");
        parent.title = "Planning".to_owned();
        let mut child = SessionRecord::new("c", "/p");
        child.title = "Relay fix".to_owned();
        let mut other = SessionRecord::new("o", "/p");
        other.title = "Docs".to_owned();
        let nodes = vec![
            SessionNode {
                session: &parent,
                children: vec![SessionNode {
                    session: &child,
                    children: Vec::new(),
                }],
            },
            SessionNode {
                session: &other,
                children: Vec::new(),
            },
        ];
        let relay = SearchQuery::parse("relay", "Untitled").expect("a query");
        let found = filter_nodes(&nodes, &relay);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].session.id.as_str(), "p");
        assert_eq!(count(&found), 2);
        let planning = filter_nodes(
            &nodes,
            &SearchQuery::parse("plan", "Untitled").expect("a query"),
        );
        assert_eq!(count(&planning), 2, "a match keeps its subtree");
        let by_id = filter_nodes(
            &nodes,
            &SearchQuery::parse(" o ", "Untitled").expect("a query"),
        );
        assert!(
            by_id.is_empty(),
            "no title or path has a word starting with o"
        );
        assert_eq!(SearchQuery::parse("  ", "Untitled"), None);
    }
}
