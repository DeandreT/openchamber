//! The sidebar's Select sessions mode: which sessions are checked.
//!
//! Ports `useSessionMultiSelectStore.ts` and the selection helpers of
//! `sessions/sessionRowOrder.tsx`. A selection lives in one scope, the
//! project its rows belong to: checking a row of another project starts a
//! new selection there. Rows outside every project header (the zones, Chats
//! and the list by time) carry no scope and join whatever selection they
//! find. Checking a row takes the subsessions listed under it along, and a
//! range runs between two rows of the same scope. Callers resolve a row's
//! subsessions on demand, so a range only pays for the rows inside it.

use std::collections::HashSet;

use crate::ids::SessionId;
use crate::session_index::SessionIndex;

/// A session row in display order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SelectableRow {
    pub id: SessionId,
    /// The row's stable key; one session can show in several rows.
    pub row_key: String,
    /// The project whose header the row sits under; `None` for a zone,
    /// Chats or time-list row.
    pub scope: Option<String>,
}

/// The checked sessions.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SessionMultiSelect {
    enabled: bool,
    /// In the order they were checked, which bulk actions run in.
    order: Vec<SessionId>,
    selected: HashSet<SessionId>,
    scope: Option<String>,
    anchor_row_key: Option<String>,
}

impl SessionMultiSelect {
    /// Whether the mode is on.
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Turns the mode on, or off and forgets the selection.
    pub fn toggle_mode(&mut self) {
        if self.enabled {
            self.disable();
        } else {
            self.enabled = true;
        }
    }

    /// Turns the mode off and forgets the selection.
    pub fn disable(&mut self) {
        *self = Self::default();
    }

    pub fn is_selected(&self, id: &SessionId) -> bool {
        self.selected.contains(id)
    }

    pub fn len(&self) -> usize {
        self.order.len()
    }

    pub fn is_empty(&self) -> bool {
        self.order.is_empty()
    }

    /// The checked sessions, oldest check first.
    pub fn selected(&self) -> &[SessionId] {
        &self.order
    }

    /// The scope the selection lives in.
    pub fn scope(&self) -> Option<&str> {
        self.scope.as_deref()
    }

    fn insert(&mut self, id: &SessionId) {
        if self.selected.insert(id.clone()) {
            self.order.push(id.clone());
        }
    }

    fn clear(&mut self) {
        self.order.clear();
        self.selected.clear();
        self.scope = None;
        self.anchor_row_key = None;
    }

    /// Leaves a selection of another scope behind, then adopts `scope`.
    fn enter_scope(&mut self, scope: Option<&str>) {
        let Some(scope) = scope else {
            return;
        };
        if self
            .scope
            .as_deref()
            .is_some_and(|current| current != scope)
        {
            self.clear();
        }
        self.scope = Some(scope.to_owned());
    }

    /// Checks or unchecks a row with `descendants`, the subsessions under
    /// it, parents first.
    pub fn toggle(&mut self, row: &SelectableRow, descendants: &[SessionId]) {
        self.enter_scope(row.scope.as_deref());
        if self.selected.contains(&row.id) {
            let removed: HashSet<&SessionId> =
                std::iter::once(&row.id).chain(descendants).collect();
            self.order.retain(|id| !removed.contains(id));
            for id in removed {
                self.selected.remove(id);
            }
            if self.anchor_row_key.as_deref() == Some(row.row_key.as_str()) {
                self.anchor_row_key = None;
            }
        } else {
            self.insert(&row.id);
            for id in descendants {
                self.insert(id);
            }
            self.anchor_row_key = Some(row.row_key.clone());
        }
        if self.order.is_empty() {
            self.clear();
        }
    }

    /// Checks every row from the last checked row to `to_row_key`, both
    /// included, among the rows of `to`'s scope. Without a usable anchor it
    /// checks `to` alone. `descendants` is asked only for rows in the range.
    pub fn select_range(
        &mut self,
        rows: &[SelectableRow],
        to_row_key: &str,
        descendants: impl Fn(&SelectableRow) -> Vec<SessionId>,
    ) {
        let Some(to) = rows.iter().find(|row| row.row_key == to_row_key) else {
            return;
        };
        let scope = to.scope.clone();
        let scoped: Vec<&SelectableRow> = rows
            .iter()
            .filter(|row| scope.is_none() || row.scope == scope)
            .collect();
        let from = self
            .anchor_row_key
            .as_deref()
            .filter(|anchor| scoped.iter().any(|row| row.row_key == *anchor))
            .unwrap_or(to_row_key)
            .to_owned();
        let position = |key: &str| scoped.iter().position(|row| row.row_key == key);
        let (Some(start), Some(end)) = (position(&from), position(to_row_key)) else {
            return;
        };
        self.enter_scope(scope.as_deref());
        for row in &scoped[start.min(end)..=start.max(end)] {
            self.insert(&row.id);
            for id in descendants(row) {
                self.insert(&id);
            }
        }
        self.anchor_row_key = Some(from);
    }

    /// Checks every row of the selection's scope, or of the first row's
    /// scope while nothing is checked. Returns whether there was a row.
    pub fn select_all(
        &mut self,
        rows: &[SelectableRow],
        descendants: impl Fn(&SelectableRow) -> Vec<SessionId>,
    ) -> bool {
        let Some(first) = rows.first() else {
            return false;
        };
        let scope = self.scope.clone().or_else(|| first.scope.clone());
        let chosen: Vec<&SelectableRow> = rows
            .iter()
            .filter(|row| scope.is_none() || row.scope == scope)
            .collect();
        if chosen.is_empty() {
            return false;
        }
        self.clear();
        self.scope = scope;
        for row in chosen {
            self.insert(&row.id);
            for id in descendants(row) {
                self.insert(&id);
            }
        }
        true
    }

    /// Unchecks sessions an action moved or a server removed.
    pub fn remove_many<'a>(&mut self, ids: impl IntoIterator<Item = &'a SessionId>) {
        let mut changed = false;
        for id in ids {
            changed |= self.selected.remove(id);
        }
        if !changed {
            return;
        }
        self.order.retain(|id| self.selected.contains(id));
        if self.order.is_empty() {
            self.clear();
        }
    }

    /// Unchecks sessions the index no longer holds. Callers run it whenever
    /// the index changes, so the count and [`Self::all_archived`] never read
    /// a session that is gone.
    pub fn retain_known(&mut self, index: &SessionIndex) {
        if self.order.is_empty() {
            return;
        }
        let gone: Vec<SessionId> = self
            .order
            .iter()
            .filter(|id| index.get(id).is_none())
            .cloned()
            .collect();
        self.remove_many(&gone);
    }

    /// `deriveSessionRowSelectionArchived`: something is checked and every
    /// checked session is archived, so the selection restores or deletes.
    pub fn all_archived(&self, index: &SessionIndex) -> bool {
        !self.order.is_empty()
            && self
                .order
                .iter()
                .all(|id| index.get(id).is_some_and(|session| session.is_archived()))
    }
}

/// What a bulk archive or delete reaches: every checked session the index
/// still holds with the sessions `reach` adds below it, each once, in check
/// order. A session that left the index is skipped; the others still run.
pub fn bulk_targets(
    index: &SessionIndex,
    selected: &[SessionId],
    reach: impl Fn(&SessionIndex, &SessionId) -> Vec<SessionId>,
) -> Vec<SessionId> {
    let mut seen: HashSet<SessionId> = HashSet::new();
    let mut targets = Vec::new();
    for id in selected {
        if index.get(id).is_none() {
            continue;
        }
        for target in reach(index, id) {
            if seen.insert(target.clone()) {
                targets.push(target);
            }
        }
    }
    targets
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;
    use crate::session::SessionRecord;

    fn row(id: &str, scope: Option<&str>) -> SelectableRow {
        SelectableRow {
            id: SessionId::from(id),
            row_key: format!("row:{id}"),
            scope: scope.map(str::to_owned),
        }
    }

    fn sessions(ids: &[&str]) -> Vec<SessionId> {
        ids.iter().copied().map(SessionId::from).collect()
    }

    /// `b` carries `b1`; no other row has subsessions.
    fn under(row: &SelectableRow) -> Vec<SessionId> {
        if row.id.as_str() == "b" {
            sessions(&["b1"])
        } else {
            Vec::new()
        }
    }

    fn ids(select: &SessionMultiSelect) -> Vec<&str> {
        select.selected().iter().map(SessionId::as_str).collect()
    }

    fn index(records: Vec<SessionRecord>) -> SessionIndex {
        let mut index = SessionIndex::new();
        let ticket = index.begin_load();
        let _ = index.complete_load(ticket, records);
        index
    }

    #[test]
    fn leaving_the_mode_forgets_the_selection() {
        let mut select = SessionMultiSelect::default();
        select.toggle_mode();
        assert!(select.is_enabled());
        select.toggle(&row("a", Some("p")), &[]);
        select.toggle_mode();
        assert!(!select.is_enabled());
        assert!(select.is_empty());
        assert_eq!(select.scope(), None);
    }

    #[test]
    fn a_row_takes_its_subsessions_along_both_ways() {
        let mut select = SessionMultiSelect::default();
        let parent = row("a", Some("p"));
        let children = sessions(&["a1", "a2"]);
        select.toggle(&parent, &children);
        assert_eq!(ids(&select), ["a", "a1", "a2"]);
        select.toggle(&row("b", Some("p")), &[]);
        select.toggle(&parent, &children);
        assert_eq!(ids(&select), ["b"]);
        select.toggle(&row("b", Some("p")), &[]);
        assert!(select.is_empty());
        assert_eq!(select.scope(), None, "an emptied selection has no scope");
    }

    #[test]
    fn another_project_starts_a_new_selection() {
        let mut select = SessionMultiSelect::default();
        select.toggle(&row("a", Some("p")), &[]);
        select.toggle(&row("b", Some("q")), &[]);
        assert_eq!(ids(&select), ["b"]);
        assert_eq!(select.scope(), Some("q"));
    }

    #[test]
    fn rows_without_a_scope_join_the_selection_and_set_none() {
        // Zone, Chats and time-list rows: one selection across projects.
        let mut select = SessionMultiSelect::default();
        select.toggle(&row("a", None), &[]);
        select.toggle(&row("b", None), &[]);
        assert_eq!(ids(&select), ["a", "b"]);
        assert_eq!(select.scope(), None);
        // A project row adopts its scope without dropping them, and a
        // scopeless row after it stays in.
        select.toggle(&row("c", Some("p")), &[]);
        select.toggle(&row("d", None), &[]);
        assert_eq!(ids(&select), ["a", "b", "c", "d"]);
        assert_eq!(select.scope(), Some("p"));
    }

    #[test]
    fn a_range_runs_between_the_anchor_and_the_row_inside_one_scope() {
        let rows = [
            row("a", Some("p")),
            row("x", Some("q")),
            row("b", Some("p")),
            row("c", Some("p")),
            row("d", Some("p")),
        ];
        let mut select = SessionMultiSelect::default();
        select.toggle(&rows[0], &[]);
        select.select_range(&rows, "row:c", under);
        assert_eq!(ids(&select), ["a", "b", "b1", "c"]);
        // Upward from the same anchor.
        let mut select = SessionMultiSelect::default();
        select.toggle(&rows[4], &[]);
        select.select_range(&rows, "row:b", under);
        assert_eq!(ids(&select), ["d", "b", "b1", "c"]);
    }

    #[test]
    fn a_range_over_scopeless_rows_spans_every_row() {
        let rows = [row("a", None), row("x", None), row("b", None)];
        let mut select = SessionMultiSelect::default();
        select.toggle(&rows[0], &[]);
        select.select_range(&rows, "row:b", under);
        assert_eq!(ids(&select), ["a", "x", "b", "b1"]);
    }

    #[test]
    fn a_range_resolves_subsessions_only_inside_it() {
        let rows = [
            row("a", Some("p")),
            row("b", Some("p")),
            row("c", Some("p")),
            row("d", Some("p")),
            row("x", Some("q")),
        ];
        let asked: RefCell<Vec<String>> = RefCell::new(Vec::new());
        let mut select = SessionMultiSelect::default();
        select.toggle(&rows[1], &[]);
        select.select_range(&rows, "row:c", |row| {
            asked.borrow_mut().push(row.id.to_string());
            Vec::new()
        });
        assert_eq!(*asked.borrow(), ["b", "c"]);
    }

    #[test]
    fn a_range_without_a_usable_anchor_checks_the_row_alone() {
        let rows = [row("a", Some("p")), row("x", Some("q"))];
        let mut select = SessionMultiSelect::default();
        select.select_range(&rows, "row:a", under);
        assert_eq!(ids(&select), ["a"]);
        // The anchor is in another scope, so the range starts over there.
        select.select_range(&rows, "row:x", under);
        assert_eq!(ids(&select), ["x"]);
        assert_eq!(select.scope(), Some("q"));
        select.select_range(&rows, "row:missing", under);
        assert_eq!(ids(&select), ["x"]);
    }

    #[test]
    fn select_all_stays_in_the_scope() {
        let rows = [
            row("b", Some("p")),
            row("x", Some("q")),
            row("c", Some("p")),
        ];
        let mut select = SessionMultiSelect::default();
        assert!(select.select_all(&rows, under));
        assert_eq!(ids(&select), ["b", "b1", "c"]);
        select.disable();
        select.toggle(&rows[1], &[]);
        assert!(select.select_all(&rows, under));
        assert_eq!(ids(&select), ["x"]);
        assert!(!SessionMultiSelect::default().select_all(&[], under));
    }

    #[test]
    fn removing_sessions_keeps_the_rest_checked() {
        let mut select = SessionMultiSelect::default();
        select.toggle(&row("a", Some("p")), &[]);
        select.toggle(&row("b", Some("p")), &[]);
        select.remove_many(&[SessionId::from("a"), SessionId::from("missing")]);
        assert_eq!(ids(&select), ["b"]);
        assert_eq!(select.scope(), Some("p"));
        select.remove_many(&[SessionId::from("b")]);
        assert!(select.is_empty());
        assert_eq!(select.scope(), None);
    }

    #[test]
    fn a_session_the_server_removed_leaves_the_selection_and_its_archive_state() {
        let mut archived = SessionRecord::new("old", "/p");
        archived.time.archived = Some(5);
        let gone = SessionRecord::new("gone", "/p");
        let mut select = SessionMultiSelect::default();
        select.toggle(&row("old", Some("p")), &[]);
        select.toggle(&row("gone", Some("p")), &[]);
        let before = index(vec![archived.clone(), gone]);
        assert!(
            !select.all_archived(&before),
            "one checked session is active"
        );
        // The active one is deleted elsewhere: only the archived one remains.
        let after = index(vec![archived]);
        select.retain_known(&after);
        assert_eq!(ids(&select), ["old"]);
        assert_eq!(select.len(), 1);
        assert!(select.all_archived(&after));
        // The last one goes too: nothing is checked and no scope lingers.
        select.retain_known(&index(Vec::new()));
        assert!(select.is_empty());
        assert_eq!(select.scope(), None);
        assert!(!select.all_archived(&index(Vec::new())));
    }

    #[test]
    fn bulk_targets_skip_missing_sessions_and_list_each_target_once() {
        let mut child = SessionRecord::new("a1", "/p");
        child.parent_id = Some(SessionId::from("a"));
        let index = index(vec![
            SessionRecord::new("a", "/p"),
            child,
            SessionRecord::new("b", "/p"),
        ]);
        let selected = sessions(&["a", "gone", "a1", "b"]);
        let targets = bulk_targets(&index, &selected, crate::session_actions::archive_targets);
        let targets: Vec<&str> = targets.iter().map(SessionId::as_str).collect();
        assert_eq!(targets, ["a", "a1", "b"]);
    }
}
