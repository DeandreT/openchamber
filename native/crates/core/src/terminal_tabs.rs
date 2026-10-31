//! Terminal tabs per working directory.
//!
//! Mirrors the tab part of `packages/ui/src/stores/useTerminalStore.ts`: each
//! directory keeps its tabs in order and one active tab; each tab names the
//! server terminal it shows and where that terminal's lifecycle stands. The
//! server owns the terminals. Tabs live in memory for the window's lifetime
//! and adopt the directory's server terminals on startup and reconnect.

use std::collections::{BTreeMap, BTreeSet};

/// A tab, unique for the life of one [`TerminalTabs`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TabId(u64);

impl TabId {
    pub fn get(self) -> u64 {
        self.0
    }
}

/// Where a tab's terminal stands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Lifecycle {
    /// The create request is in flight.
    Starting,
    Running,
    /// The process ended; the terminal stays readable until the tab closes.
    Exited {
        exit_code: Option<i64>,
        signal: Option<i64>,
    },
    /// The server refused to start it. `cwd_missing` names the one cause the
    /// user can act on: the directory no longer exists.
    StartFailed {
        cwd_missing: bool,
    },
    /// The server no longer holds the terminal, as after its idle sweep.
    Ended,
}

/// One tab.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tab {
    pub id: TabId,
    /// The server terminal, once created or adopted.
    pub session: Option<String>,
    pub lifecycle: Lifecycle,
    pub label: String,
}

/// A server terminal offered for adoption, from an authoritative list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListedTerminal {
    pub session: String,
    pub running: bool,
    /// Project-action terminals belong to the project action row and are
    /// never adopted as tabs.
    pub is_terminal: bool,
}

#[derive(Clone, Debug, Default)]
struct DirectoryTabs {
    tabs: Vec<Tab>,
    active: Option<TabId>,
}

/// Every directory's tabs.
#[derive(Clone, Debug, Default)]
pub struct TerminalTabs {
    directories: BTreeMap<String, DirectoryTabs>,
    next_id: u64,
}

impl TerminalTabs {
    pub fn new() -> Self {
        Self::default()
    }

    /// The directory's tabs in order.
    pub fn tabs(&self, directory: &str) -> &[Tab] {
        self.directories
            .get(directory)
            .map_or(&[], |entry| entry.tabs.as_slice())
    }

    pub fn active(&self, directory: &str) -> Option<&Tab> {
        let entry = self.directories.get(directory)?;
        let active = entry.active?;
        entry.tabs.iter().find(|tab| tab.id == active)
    }

    pub fn tab(&self, directory: &str, id: TabId) -> Option<&Tab> {
        self.tabs(directory).iter().find(|tab| tab.id == id)
    }

    /// The directory and tab that show `session`.
    pub fn find_session(&self, session: &str) -> Option<(&str, &Tab)> {
        self.directories.iter().find_map(|(directory, entry)| {
            entry
                .tabs
                .iter()
                .find(|tab| tab.session.as_deref() == Some(session))
                .map(|tab| (directory.as_str(), tab))
        })
    }

    /// Every server terminal a tab shows, in any directory.
    pub fn sessions(&self) -> impl Iterator<Item = &str> {
        self.directories
            .values()
            .flat_map(|entry| entry.tabs.iter())
            .filter_map(|tab| tab.session.as_deref())
    }

    /// Opens a starting tab at the end and makes it active.
    pub fn open(&mut self, directory: &str) -> TabId {
        self.next_id += 1;
        let id = TabId(self.next_id);
        let entry = self.directories.entry(directory.to_owned()).or_default();
        let label = next_default_label(&entry.tabs);
        entry.tabs.push(Tab {
            id,
            session: None,
            lifecycle: Lifecycle::Starting,
            label,
        });
        entry.active = Some(id);
        id
    }

    pub fn select(&mut self, directory: &str, id: TabId) -> bool {
        let Some(entry) = self.directories.get_mut(directory) else {
            return false;
        };
        if !entry.tabs.iter().any(|tab| tab.id == id) {
            return false;
        }
        entry.active = Some(id);
        true
    }

    /// Binds a created terminal to its tab. `false` when the tab closed while
    /// the create was in flight: the caller closes the orphaned terminal.
    pub fn started(&mut self, directory: &str, id: TabId, session: &str) -> bool {
        let Some(tab) = self.tab_mut(directory, id) else {
            return false;
        };
        if tab.lifecycle != Lifecycle::Starting {
            return false;
        }
        tab.session = Some(session.to_owned());
        tab.lifecycle = Lifecycle::Running;
        true
    }

    pub fn start_failed(&mut self, directory: &str, id: TabId, cwd_missing: bool) {
        if let Some(tab) = self.tab_mut(directory, id)
            && tab.lifecycle == Lifecycle::Starting
        {
            tab.lifecycle = Lifecycle::StartFailed { cwd_missing };
        }
    }

    /// Records an exit. Answers the tab that showed the terminal.
    pub fn exited(
        &mut self,
        session: &str,
        exit_code: Option<i64>,
        signal: Option<i64>,
    ) -> Option<(String, TabId)> {
        let (directory, tab) = self.session_tab_mut(session)?;
        tab.lifecycle = Lifecycle::Exited { exit_code, signal };
        Some((directory, tab.id))
    }

    /// The terminal's process was replaced; its tab runs again.
    pub fn restarted(&mut self, session: &str) {
        if let Some((_, tab)) = self.session_tab_mut(session) {
            tab.lifecycle = Lifecycle::Running;
        }
    }

    /// The server dropped the terminal (closed, killed, reaped).
    pub fn ended(&mut self, session: &str) {
        if let Some((_, tab)) = self.session_tab_mut(session)
            && !matches!(tab.lifecycle, Lifecycle::Exited { .. })
        {
            tab.lifecycle = Lifecycle::Ended;
        }
    }

    /// Removes the tab, activating its neighbor. Answers the server terminal
    /// the caller closes. A tab still starting answers nothing; its create
    /// completion finds no tab and closes the terminal itself.
    pub fn close(&mut self, directory: &str, id: TabId) -> Option<String> {
        let entry = self.directories.get_mut(directory)?;
        let index = entry.tabs.iter().position(|tab| tab.id == id)?;
        let tab = entry.tabs.remove(index);
        if entry.active == Some(id) {
            let neighbor = index.min(entry.tabs.len().saturating_sub(1));
            entry.active = entry.tabs.get(neighbor).map(|tab| tab.id);
        }
        if entry.tabs.is_empty() {
            self.directories.remove(directory);
        }
        tab.session
    }

    /// Reconciles the directory with an authoritative list of its server
    /// terminals: unshown interactive terminals become tabs, and a tab whose
    /// terminal the server no longer holds ends. Answers the adopted
    /// sessions, which the caller attaches. A failed list read must not reach
    /// this method: an empty list ends every tab.
    pub fn adopt(&mut self, directory: &str, listed: &[ListedTerminal]) -> Vec<String> {
        let shown: BTreeSet<String> = self.sessions().map(str::to_owned).collect();
        let present: BTreeSet<&str> = listed.iter().map(|entry| entry.session.as_str()).collect();
        let mut adopted = Vec::new();
        for entry in listed {
            if !entry.is_terminal || shown.contains(&entry.session) {
                continue;
            }
            self.next_id += 1;
            let id = TabId(self.next_id);
            let tabs = self.directories.entry(directory.to_owned()).or_default();
            let label = next_default_label(&tabs.tabs);
            tabs.tabs.push(Tab {
                id,
                session: Some(entry.session.clone()),
                lifecycle: if entry.running {
                    Lifecycle::Running
                } else {
                    Lifecycle::Exited {
                        exit_code: None,
                        signal: None,
                    }
                },
                label,
            });
            tabs.active.get_or_insert(id);
            adopted.push(entry.session.clone());
        }
        if let Some(entry) = self.directories.get_mut(directory) {
            for tab in &mut entry.tabs {
                let gone = tab
                    .session
                    .as_deref()
                    .is_some_and(|session| !present.contains(session));
                if gone && matches!(tab.lifecycle, Lifecycle::Running) {
                    tab.lifecycle = Lifecycle::Ended;
                }
            }
        }
        adopted
    }

    fn tab_mut(&mut self, directory: &str, id: TabId) -> Option<&mut Tab> {
        self.directories
            .get_mut(directory)?
            .tabs
            .iter_mut()
            .find(|tab| tab.id == id)
    }

    fn session_tab_mut(&mut self, session: &str) -> Option<(String, &mut Tab)> {
        self.directories.iter_mut().find_map(|(directory, entry)| {
            entry
                .tabs
                .iter_mut()
                .find(|tab| tab.session.as_deref() == Some(session))
                .map(|tab| (directory.clone(), tab))
        })
    }
}

/// "Terminal", then "Terminal N" one above the highest default label open,
/// so labels stay unique after closes (`nextDefaultTabLabel`).
fn next_default_label(tabs: &[Tab]) -> String {
    let highest = tabs
        .iter()
        .filter_map(|tab| {
            let rest = tab.label.strip_prefix("Terminal")?;
            if rest.is_empty() {
                return Some(1);
            }
            rest.strip_prefix(' ')?.parse::<u64>().ok()
        })
        .max()
        .unwrap_or(0);
    if highest == 0 {
        "Terminal".to_owned()
    } else {
        format!("Terminal {}", highest + 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn listed(session: &str, running: bool) -> ListedTerminal {
        ListedTerminal {
            session: session.to_owned(),
            running,
            is_terminal: true,
        }
    }

    #[test]
    fn labels_stay_unique_after_closes() {
        let mut tabs = TerminalTabs::new();
        let first = tabs.open("/w");
        let second = tabs.open("/w");
        assert_eq!(tabs.tab("/w", first).expect("tab").label, "Terminal");
        assert_eq!(tabs.tab("/w", second).expect("tab").label, "Terminal 2");
        tabs.close("/w", first);
        let third = tabs.open("/w");
        assert_eq!(tabs.tab("/w", third).expect("tab").label, "Terminal 3");
    }

    #[test]
    fn a_create_finishing_after_its_tab_closed_is_orphaned() {
        let mut tabs = TerminalTabs::new();
        let id = tabs.open("/w");
        assert_eq!(tabs.close("/w", id), None, "nothing to close yet");
        assert!(!tabs.started("/w", id, "t1"), "the caller closes t1");
        assert!(tabs.tabs("/w").is_empty());
    }

    #[test]
    fn lifecycle_follows_create_exit_restart_and_end() {
        let mut tabs = TerminalTabs::new();
        let id = tabs.open("/w");
        assert!(tabs.started("/w", id, "t1"));
        assert_eq!(
            tabs.active("/w").expect("active").lifecycle,
            Lifecycle::Running
        );
        assert_eq!(
            tabs.exited("t1", Some(3), None),
            Some(("/w".to_owned(), id))
        );
        assert_eq!(
            tabs.tab("/w", id).expect("tab").lifecycle,
            Lifecycle::Exited {
                exit_code: Some(3),
                signal: None
            }
        );
        tabs.ended("t1");
        assert!(
            matches!(
                tabs.tab("/w", id).expect("tab").lifecycle,
                Lifecycle::Exited { .. }
            ),
            "an exit stays readable when the server forgets the terminal"
        );
        tabs.restarted("t1");
        assert_eq!(
            tabs.tab("/w", id).expect("tab").lifecycle,
            Lifecycle::Running
        );
        assert_eq!(tabs.close("/w", id), Some("t1".to_owned()));
        assert_eq!(tabs.exited("t1", None, None), None);

        let failed = tabs.open("/gone");
        tabs.start_failed("/gone", failed, true);
        assert_eq!(
            tabs.tab("/gone", failed).expect("tab").lifecycle,
            Lifecycle::StartFailed { cwd_missing: true }
        );
    }

    #[test]
    fn closing_the_active_tab_activates_its_neighbor() {
        let mut tabs = TerminalTabs::new();
        let a = tabs.open("/w");
        let b = tabs.open("/w");
        let c = tabs.open("/w");
        tabs.select("/w", b);
        tabs.close("/w", b);
        assert_eq!(tabs.active("/w").map(|tab| tab.id), Some(c));
        tabs.close("/w", c);
        assert_eq!(tabs.active("/w").map(|tab| tab.id), Some(a));
        tabs.close("/w", a);
        assert!(tabs.active("/w").is_none());
    }

    #[test]
    fn adoption_adds_unshown_terminals_once_and_ends_vanished_ones() {
        let mut tabs = TerminalTabs::new();
        let own = tabs.open("/w");
        tabs.started("/w", own, "mine");
        let mut action = listed("action", true);
        action.is_terminal = false;
        let adopted = tabs.adopt(
            "/w",
            &[
                listed("mine", true),
                listed("other", true),
                listed("done", false),
                action,
            ],
        );
        assert_eq!(adopted, vec!["other".to_owned(), "done".to_owned()]);
        assert_eq!(tabs.tabs("/w").len(), 3, "project actions are not adopted");
        assert_eq!(
            tabs.active("/w").map(|tab| tab.id),
            Some(own),
            "the active tab stays"
        );
        assert!(
            tabs.adopt("/w", &[listed("mine", true), listed("other", true)])
                .is_empty()
        );

        // "mine" vanished from an authoritative list.
        tabs.adopt("/w", &[listed("other", true), listed("done", false)]);
        assert_eq!(
            tabs.tab("/w", own).expect("tab").lifecycle,
            Lifecycle::Ended
        );
        let (_, other) = tabs.find_session("other").expect("adopted");
        assert_eq!(other.lifecycle, Lifecycle::Running);
        assert_eq!(
            tabs.sessions().collect::<Vec<_>>(),
            vec!["mine", "other", "done"]
        );
    }

    #[test]
    fn adoption_into_an_empty_directory_activates_the_first_terminal() {
        let mut tabs = TerminalTabs::new();
        tabs.adopt("/w", &[listed("a", true), listed("b", true)]);
        assert_eq!(
            tabs.active("/w").and_then(|tab| tab.session.as_deref()),
            Some("a")
        );
        // A starting tab is not ended by a list that predates its create.
        let starting = tabs.open("/w");
        tabs.adopt("/w", &[listed("a", true), listed("b", true)]);
        assert_eq!(
            tabs.tab("/w", starting).expect("tab").lifecycle,
            Lifecycle::Starting
        );
    }
}
