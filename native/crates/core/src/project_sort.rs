//! The order the sidebar lists projects in.
//!
//! Ports `sortProjectsByOrder` and `rankByLatestActivity` of
//! `packages/ui/src/components/session/sidebar/list/projectSort.ts`. The
//! registered order is the manual order: the native client keeps no separate
//! drag order, so `Manual` returns the projects as they came.
//!
//! Labels compare by their lowercased text. The web compares with
//! `localeCompare`, which also folds accents; the two agree for ASCII labels.

use std::cmp::Reverse;

/// `sidebarProjectSortOrder`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum ProjectSortOrder {
    /// The registered order.
    #[default]
    Manual,
    AToZ,
    ZToA,
    /// Newest registration first.
    DateAdded,
    /// Newest session activity first, then last opened.
    Recent,
}

impl ProjectSortOrder {
    /// Every order, as the Display mode menu lists them.
    pub const ALL: [Self; 5] = [
        Self::Manual,
        Self::AToZ,
        Self::ZToA,
        Self::DateAdded,
        Self::Recent,
    ];

    /// The value the server stores.
    pub fn as_setting(self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::AToZ => "a-z",
            Self::ZToA => "z-a",
            Self::DateAdded => "date-added",
            Self::Recent => "recent",
        }
    }

    /// Reads a stored value; anything else is no choice.
    pub fn from_setting(value: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|order| order.as_setting() == value)
    }
}

/// What a project needs to be sortable.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SortableProject<'a> {
    /// The display label; an empty or absent label sorts by `path`.
    pub label: Option<&'a str>,
    pub path: &'a str,
    /// `addedAt`, in milliseconds.
    pub added_at: Option<i64>,
    /// `lastOpenedAt`, in milliseconds.
    pub last_opened_at: Option<i64>,
    /// The newest `time.updated` among the project's sessions, when it has
    /// any. Only `Recent` reads it.
    pub latest_activity: Option<i64>,
}

fn sort_label(project: &SortableProject<'_>) -> String {
    project
        .label
        .filter(|label| !label.is_empty())
        .unwrap_or(project.path)
        .to_lowercase()
}

/// The positions of `projects` in sidebar order. Projects that tie keep their
/// incoming order.
pub fn sort_projects(projects: &[SortableProject<'_>], order: ProjectSortOrder) -> Vec<usize> {
    let mut sorted: Vec<usize> = (0..projects.len()).collect();
    match order {
        ProjectSortOrder::Manual => {}
        ProjectSortOrder::AToZ => sorted.sort_by_cached_key(|index| sort_label(&projects[*index])),
        ProjectSortOrder::ZToA => {
            sorted.sort_by_cached_key(|index| Reverse(sort_label(&projects[*index])));
        }
        ProjectSortOrder::DateAdded => {
            sorted.sort_by_key(|index| Reverse(projects[*index].added_at.unwrap_or(0)));
        }
        ProjectSortOrder::Recent => {
            // Last opened first, then the newest session activity over it:
            // projects without a session keep the last-opened order after
            // the active ones.
            sorted.sort_by_key(|index| Reverse(projects[*index].last_opened_at.unwrap_or(0)));
            sorted.sort_by_key(|index| Reverse(projects[*index].latest_activity.unwrap_or(0)));
        }
    }
    sorted
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project<'a>(label: Option<&'a str>, path: &'a str) -> SortableProject<'a> {
        SortableProject {
            label,
            path,
            ..SortableProject::default()
        }
    }

    #[test]
    fn settings_round_trip_and_reject_unknown_values() {
        for order in ProjectSortOrder::ALL {
            assert_eq!(
                ProjectSortOrder::from_setting(order.as_setting()),
                Some(order)
            );
        }
        assert_eq!(ProjectSortOrder::from_setting("newest"), None);
        assert_eq!(ProjectSortOrder::from_setting(""), None);
    }

    #[test]
    fn manual_keeps_the_registered_order() {
        let projects = [project(None, "/z"), project(None, "/a")];
        assert_eq!(
            sort_projects(&projects, ProjectSortOrder::Manual),
            vec![0, 1]
        );
    }

    #[test]
    fn names_sort_by_label_then_path_without_case() {
        let projects = [
            project(Some("beta"), "/1"),
            project(None, "/Alpha"),
            project(Some(""), "/charlie"),
            project(Some("Aardvark"), "/9"),
        ];
        // Labels: "beta", "/alpha", "/charlie", "aardvark".
        assert_eq!(
            sort_projects(&projects, ProjectSortOrder::AToZ),
            vec![1, 2, 3, 0]
        );
        assert_eq!(
            sort_projects(&projects, ProjectSortOrder::ZToA),
            vec![0, 3, 2, 1]
        );
    }

    #[test]
    fn equal_labels_keep_their_incoming_order_in_both_directions() {
        let projects = [project(Some("same"), "/1"), project(Some("Same"), "/2")];
        assert_eq!(sort_projects(&projects, ProjectSortOrder::AToZ), vec![0, 1]);
        assert_eq!(sort_projects(&projects, ProjectSortOrder::ZToA), vec![0, 1]);
    }

    #[test]
    fn date_added_lists_the_newest_first_and_undated_last() {
        let mut projects = [
            project(None, "/old"),
            project(None, "/none"),
            project(None, "/new"),
        ];
        projects[0].added_at = Some(10);
        projects[2].added_at = Some(30);
        assert_eq!(
            sort_projects(&projects, ProjectSortOrder::DateAdded),
            vec![2, 0, 1]
        );
    }

    #[test]
    fn recent_ranks_session_activity_over_last_opened() {
        let mut projects = [
            project(None, "/opened-late"),
            project(None, "/active-old"),
            project(None, "/active-new"),
            project(None, "/opened-early"),
        ];
        projects[0].last_opened_at = Some(900);
        projects[3].last_opened_at = Some(100);
        projects[1].latest_activity = Some(5);
        projects[1].last_opened_at = Some(1_000);
        projects[2].latest_activity = Some(50);
        assert_eq!(
            sort_projects(&projects, ProjectSortOrder::Recent),
            vec![2, 1, 0, 3],
            "sessions rank first, then the last-opened order breaks ties"
        );
    }
}
