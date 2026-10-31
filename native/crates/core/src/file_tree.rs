//! The Files panel's directory tree: one snapshot per listed folder, the
//! expanded and selected paths, and the visible rows in display order.
//!
//! Ports the tree rules of `FilesView.tsx` and `files/directoryRequests.ts`:
//! folders before files, then names in `localeCompare` order; dotfiles shown
//! unless hidden; `node_modules` left out unless gitignored entries are shown;
//! one read in flight per folder, which a refresh after a mutation replaces
//! and a background poll never does. A failed read keeps the folder's last
//! snapshot and reports its failure on the folder's row.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};

/// Folder names the tree leaves out until gitignored entries are shown.
const DEFAULT_IGNORED_NAMES: &[&str] = &["node_modules"];

/// What a directory entry is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum EntryKind {
    Directory,
    File,
}

/// One entry of a folder listing, its path in the server's path space.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirectoryEntry {
    pub name: String,
    pub path: String,
    pub kind: EntryKind,
}

/// Why a folder could not be listed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ListingFailure {
    NotFound,
    NotDirectory,
    PermissionDenied,
    /// Any other refusal or a failed request, with the server's message when
    /// it sent one.
    Failed(Option<String>),
}

/// Who asks for a folder listing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ListingCause {
    /// The folder came into view: expanded, or the tree opened.
    Reveal,
    /// A change this client made; it replaces a read in flight.
    Mutation,
    /// The periodic check; it never replaces a read in flight.
    Poll,
}

/// A read the caller performs and hands back to [`FileTree::complete`].
#[derive(Clone, Debug, PartialEq, Eq)]
#[must_use = "a ticket names a read the caller must perform"]
pub struct ListingTicket {
    pub path: String,
    /// Whether the read leaves out gitignored entries.
    pub respect_gitignore: bool,
    sequence: u64,
    scope: u64,
}

/// One visible row of the tree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TreeRow {
    pub path: String,
    pub name: String,
    /// Zero for the root's own entries.
    pub depth: usize,
    pub kind: EntryKind,
    pub expanded: bool,
    /// A folder whose first listing is still on its way.
    pub loading: bool,
    /// The folder's last read failed; its previous entries stay listed.
    pub failure: Option<ListingFailure>,
    /// Position among its siblings, from 1, and the sibling count.
    pub position: usize,
    pub siblings: usize,
}

#[derive(Debug, Default)]
struct Folder {
    /// The last listing, sorted; `None` before one succeeded.
    entries: Option<Vec<DirectoryEntry>>,
    failure: Option<ListingFailure>,
    /// The sequence of the read in flight.
    in_flight: Option<u64>,
}

/// The tree under one root folder.
#[derive(Debug)]
pub struct FileTree {
    root: String,
    show_hidden: bool,
    show_gitignored: bool,
    folders: HashMap<String, Folder>,
    expanded: HashSet<String>,
    selected: Option<String>,
    next_sequence: u64,
    /// Bumps when the filter changes, so reads started before it are dropped.
    scope: u64,
}

impl FileTree {
    /// A tree of `root` that shows dotfiles and hides gitignored entries, as
    /// the web's defaults do.
    pub fn new(root: impl Into<String>) -> Self {
        Self {
            root: root.into(),
            show_hidden: true,
            show_gitignored: false,
            folders: HashMap::new(),
            expanded: HashSet::new(),
            selected: None,
            next_sequence: 0,
            scope: 0,
        }
    }

    pub fn root(&self) -> &str {
        &self.root
    }

    pub fn show_gitignored(&self) -> bool {
        self.show_gitignored
    }

    pub fn selected(&self) -> Option<&str> {
        self.selected.as_deref()
    }

    pub fn is_expanded(&self, path: &str) -> bool {
        self.expanded.contains(path)
    }

    /// Whether the root's listing is still on its way.
    pub fn root_loading(&self) -> bool {
        self.folders
            .get(&self.root)
            .is_none_or(|folder| folder.entries.is_none() && folder.failure.is_none())
    }

    /// The root's failure while it has nothing to show.
    pub fn root_failure(&self) -> Option<&ListingFailure> {
        self.folders
            .get(&self.root)
            .filter(|folder| folder.entries.is_none())
            .and_then(|folder| folder.failure.as_ref())
    }

    /// Asks for a listing of `path`, or `None` when the cause does not
    /// warrant one: a reveal of a folder already listed or being read, or a
    /// poll while a read runs.
    pub fn request(&mut self, path: &str, cause: ListingCause) -> Option<ListingTicket> {
        let folder = self.folders.entry(path.to_owned()).or_default();
        match cause {
            ListingCause::Reveal if folder.entries.is_some() || folder.in_flight.is_some() => {
                return None;
            }
            ListingCause::Poll if folder.in_flight.is_some() => return None,
            _ => {}
        }
        self.next_sequence += 1;
        folder.in_flight = Some(self.next_sequence);
        Some(ListingTicket {
            path: path.to_owned(),
            respect_gitignore: !self.show_gitignored,
            sequence: self.next_sequence,
            scope: self.scope,
        })
    }

    /// Applies a finished read. Returns whether it changed what the tree
    /// shows; a superseded read or one from before a filter change changes
    /// nothing.
    pub fn complete(
        &mut self,
        ticket: ListingTicket,
        result: Result<Vec<DirectoryEntry>, ListingFailure>,
    ) -> bool {
        if ticket.scope != self.scope {
            return false;
        }
        let Some(folder) = self.folders.get_mut(&ticket.path) else {
            return false;
        };
        if folder.in_flight != Some(ticket.sequence) {
            return false;
        }
        folder.in_flight = None;
        match result {
            Ok(mut entries) => {
                entries.sort_by(compare_entries);
                let failure = folder.failure.take();
                if folder.entries.as_ref() == Some(&entries) && failure.is_none() {
                    return false;
                }
                folder.entries = Some(entries);
                true
            }
            Err(failure) => {
                folder.failure = Some(failure);
                true
            }
        }
    }

    /// Expands or folds `path`; expanding a folder never listed asks for it.
    pub fn toggle(&mut self, path: &str) -> Option<ListingTicket> {
        if self.expanded.remove(path) {
            return None;
        }
        self.expanded.insert(path.to_owned());
        self.request(path, ListingCause::Reveal)
    }

    /// Folds `path` and every folder under it, as the web's collapse does.
    pub fn collapse(&mut self, path: &str) {
        self.expanded
            .retain(|expanded| expanded != path && !is_under(expanded, path));
    }

    pub fn select(&mut self, path: Option<String>) {
        self.selected = path;
    }

    /// Shows or hides gitignored entries. Every listing is read again under
    /// the new filter; expanded folders stay expanded.
    pub fn set_show_gitignored(&mut self, show: bool) -> Vec<ListingTicket> {
        if self.show_gitignored == show {
            return Vec::new();
        }
        self.show_gitignored = show;
        self.scope += 1;
        for folder in self.folders.values_mut() {
            folder.in_flight = None;
        }
        self.refresh_all(ListingCause::Mutation)
    }

    /// Listings for the root and every expanded folder that was listed or
    /// asked for, as a refresh or a poll reads them.
    pub fn refresh_all(&mut self, cause: ListingCause) -> Vec<ListingTicket> {
        let mut paths = vec![self.root.clone()];
        let mut expanded: Vec<String> = self
            .expanded
            .iter()
            .filter(|path| self.folders.contains_key(*path))
            .cloned()
            .collect();
        expanded.sort();
        paths.extend(expanded);
        paths
            .iter()
            .filter_map(|path| self.request(path, cause))
            .collect()
    }

    /// The listed entry count of `path`, so a poll can skip huge folders.
    pub fn entry_count(&self, path: &str) -> Option<usize> {
        self.folders
            .get(path)
            .and_then(|folder| folder.entries.as_ref())
            .map(Vec::len)
    }

    /// The visible rows, depth first in display order.
    pub fn rows(&self) -> Vec<TreeRow> {
        let mut rows = Vec::new();
        self.push_rows(&self.root, 0, &mut rows);
        rows
    }

    fn push_rows(&self, path: &str, depth: usize, rows: &mut Vec<TreeRow>) {
        let Some(entries) = self
            .folders
            .get(path)
            .and_then(|folder| folder.entries.as_ref())
        else {
            return;
        };
        let visible: Vec<&DirectoryEntry> = entries
            .iter()
            .filter(|entry| self.is_visible(entry))
            .collect();
        let siblings = visible.len();
        for (index, entry) in visible.into_iter().enumerate() {
            let expanded =
                entry.kind == EntryKind::Directory && self.expanded.contains(&entry.path);
            let folder = self.folders.get(&entry.path);
            rows.push(TreeRow {
                path: entry.path.clone(),
                name: entry.name.clone(),
                depth,
                kind: entry.kind,
                expanded,
                loading: expanded
                    && folder
                        .is_none_or(|folder| folder.entries.is_none() && folder.failure.is_none()),
                failure: folder.and_then(|folder| folder.failure.clone()),
                position: index + 1,
                siblings,
            });
            if expanded {
                self.push_rows(&entry.path, depth + 1, rows);
            }
        }
    }

    fn is_visible(&self, entry: &DirectoryEntry) -> bool {
        if !self.show_hidden && entry.name.starts_with('.') {
            return false;
        }
        self.show_gitignored || !DEFAULT_IGNORED_NAMES.contains(&entry.name.as_str())
    }
}

/// Whether `path` lies below `folder`, by whole segments.
fn is_under(path: &str, folder: &str) -> bool {
    path.strip_prefix(folder)
        .is_some_and(|rest| rest.starts_with('/'))
}

/// Folders first, then names as `localeCompare` orders them for ASCII:
/// letters without regard to case, and a lowercase letter before its
/// uppercase form.
fn compare_entries(a: &DirectoryEntry, b: &DirectoryEntry) -> Ordering {
    let kind = |entry: &DirectoryEntry| match entry.kind {
        EntryKind::Directory => 0,
        EntryKind::File => 1,
    };
    kind(a)
        .cmp(&kind(b))
        .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        .then_with(|| b.name.cmp(&a.name))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(path: &str, kind: EntryKind) -> DirectoryEntry {
        DirectoryEntry {
            name: path.rsplit('/').next().expect("a name").to_owned(),
            path: path.to_owned(),
            kind,
        }
    }

    fn names(tree: &FileTree) -> Vec<String> {
        tree.rows()
            .iter()
            .map(|row| format!("{}{}", "  ".repeat(row.depth), row.name))
            .collect()
    }

    fn listed(tree: &mut FileTree, path: &str, entries: Vec<DirectoryEntry>) {
        let ticket = tree
            .request(path, ListingCause::Mutation)
            .expect("a mutation always reads");
        assert!(tree.complete(ticket, Ok(entries)));
    }

    #[test]
    fn folders_come_first_then_names_in_locale_order() {
        let mut tree = FileTree::new("/p");
        let ticket = tree
            .request("/p", ListingCause::Reveal)
            .expect("the root is read");
        assert!(ticket.respect_gitignore, "gitignored entries start hidden");
        assert!(tree.root_loading());
        tree.complete(
            ticket,
            Ok(vec![
                entry("/p/b.txt", EntryKind::File),
                entry("/p/A.txt", EntryKind::File),
                entry("/p/a.txt", EntryKind::File),
                entry("/p/src", EntryKind::Directory),
                entry("/p/.git", EntryKind::Directory),
                entry("/p/node_modules", EntryKind::Directory),
            ]),
        );
        assert_eq!(names(&tree), [".git", "src", "a.txt", "A.txt", "b.txt"]);
        let rows = tree.rows();
        assert_eq!((rows[1].position, rows[1].siblings), (2, 5));
    }

    #[test]
    fn expanding_reads_once_and_children_follow_their_folder() {
        let mut tree = FileTree::new("/p");
        listed(
            &mut tree,
            "/p",
            vec![
                entry("/p/src", EntryKind::Directory),
                entry("/p/z.txt", EntryKind::File),
            ],
        );
        let ticket = tree.toggle("/p/src").expect("an unlisted folder is read");
        assert!(tree.rows()[0].loading);
        assert_eq!(tree.request("/p/src", ListingCause::Reveal), None);
        tree.complete(ticket, Ok(vec![entry("/p/src/app.txt", EntryKind::File)]));
        assert_eq!(names(&tree), ["src", "  app.txt", "z.txt"]);
        assert_eq!(tree.toggle("/p/src"), None, "folding reads nothing");
        assert_eq!(names(&tree), ["src", "z.txt"]);
        assert_eq!(
            tree.toggle("/p/src"),
            None,
            "a listed folder opens from its snapshot"
        );
        assert_eq!(names(&tree), ["src", "  app.txt", "z.txt"]);
    }

    #[test]
    fn a_mutation_supersedes_a_read_and_a_poll_never_does() {
        let mut tree = FileTree::new("/p");
        let first = tree.request("/p", ListingCause::Reveal).expect("read");
        assert_eq!(tree.request("/p", ListingCause::Poll), None);
        let second = tree
            .request("/p", ListingCause::Mutation)
            .expect("replaces");
        assert!(
            !tree.complete(first, Ok(vec![entry("/p/old", EntryKind::File)])),
            "the superseded answer is dropped"
        );
        assert!(tree.complete(second, Ok(vec![entry("/p/new", EntryKind::File)])));
        assert_eq!(names(&tree), ["new"]);
    }

    #[test]
    fn a_failed_read_keeps_the_snapshot_and_reports_on_the_row() {
        let mut tree = FileTree::new("/p");
        listed(&mut tree, "/p", vec![entry("/p/src", EntryKind::Directory)]);
        let ticket = tree.toggle("/p/src").expect("read");
        tree.complete(ticket, Ok(vec![entry("/p/src/a", EntryKind::File)]));
        let again = tree.request("/p/src", ListingCause::Poll).expect("poll");
        assert!(tree.complete(again, Err(ListingFailure::PermissionDenied)));
        let rows = tree.rows();
        assert_eq!(rows[0].failure, Some(ListingFailure::PermissionDenied));
        assert_eq!(names(&tree), ["src", "  a"], "the entries stay");
        let recovered = tree.request("/p/src", ListingCause::Poll).expect("poll");
        tree.complete(recovered, Ok(vec![entry("/p/src/a", EntryKind::File)]));
        assert_eq!(tree.rows()[0].failure, None, "a good read clears it");
    }

    #[test]
    fn a_failed_first_root_read_is_not_an_empty_folder() {
        let mut tree = FileTree::new("/p");
        let ticket = tree.request("/p", ListingCause::Reveal).expect("read");
        tree.complete(ticket, Err(ListingFailure::NotFound));
        assert!(!tree.root_loading());
        assert_eq!(tree.root_failure(), Some(&ListingFailure::NotFound));
        assert!(tree.rows().is_empty());
        listed(&mut tree, "/p", Vec::new());
        assert_eq!(tree.root_failure(), None, "an empty listing is a success");
    }

    #[test]
    fn showing_gitignored_entries_drops_older_reads_and_reads_again() {
        let mut tree = FileTree::new("/p");
        listed(
            &mut tree,
            "/p",
            vec![
                entry("/p/src", EntryKind::Directory),
                entry("/p/node_modules", EntryKind::Directory),
            ],
        );
        let ticket = tree.toggle("/p/src").expect("read");
        let tickets = tree.set_show_gitignored(true);
        assert_eq!(
            tickets
                .iter()
                .map(|ticket| (ticket.path.as_str(), ticket.respect_gitignore))
                .collect::<Vec<_>>(),
            [("/p", false), ("/p/src", false)]
        );
        assert!(
            !tree.complete(ticket, Ok(Vec::new())),
            "read under the old filter"
        );
        assert_eq!(names(&tree), ["node_modules", "src"]);
        assert!(tree.set_show_gitignored(true).is_empty());
    }

    #[test]
    fn collapse_folds_the_subtree() {
        let mut tree = FileTree::new("/p");
        listed(&mut tree, "/p", vec![entry("/p/a", EntryKind::Directory)]);
        let _ = tree.toggle("/p/a");
        let _ = tree.toggle("/p/a/b");
        let _ = tree.toggle("/p/ab");
        tree.collapse("/p/a");
        assert!(!tree.is_expanded("/p/a/b"));
        assert!(
            tree.is_expanded("/p/ab"),
            "a sibling with a shared prefix stays"
        );
    }
}
