//! Device-local interface memory that outlives a restart: per runtime, the
//! project action each project's button last ran
//! (`lib/projectActionSelection.ts`), each root's file tree
//! (`useFilesViewTabsStore.ts`) and the side of the new-session target picker
//! last worked on (`oc.chatInput.lastDraftTarget`); for every runtime, the
//! work status sections
//! the user folded or unfolded (`workStatusExpandedSections`). The web keeps
//! them in local storage and never sends them to the server; here they live
//! in `ui-memory.json` beside the layout file.
//!
//! The file is read once at startup and written off the UI thread, one write
//! at a time, so the last change is the one kept. A missing, oversized or
//! unreadable file starts empty; a failed write keeps the memory in this run.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use gpui_kit::{App, Global, Task};
use openchamber_native_core::chat_draft::DraftDestination;
use serde::{Deserialize, Serialize};

const FILE_NAME: &str = "ui-memory.json";
const MAX_BYTES: u64 = 4 * 1024 * 1024;
/// `MAX_ROOTS`, `MAX_EXPANDED_PATHS_PER_ROOT` and `MAX_PATH_LENGTH`.
const MAX_ROOTS: usize = 20;
const MAX_EXPANDED: usize = 500;
const MAX_PATH_LENGTH: usize = 4096;
/// `ROOT_TTL_MS`: a root untouched for 90 days is forgotten.
const ROOT_TTL_MILLIS: u64 = 90 * 24 * 60 * 60 * 1000;

/// One root's remembered file tree.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TreeMemory {
    /// Expanded folders, oldest first.
    pub(crate) expanded: Vec<String>,
    /// The file open in the editor.
    pub(crate) selected: Option<String>,
    touched_at: u64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MemoryFile {
    version: u32,
    /// Runtime, then project path, then action ID.
    #[serde(default)]
    action_picks: BTreeMap<String, BTreeMap<String, String>>,
    /// Runtime, then root directory.
    #[serde(default)]
    file_trees: BTreeMap<String, BTreeMap<String, TreeMemory>>,
    /// Runtime, then the directory of the project a new session last
    /// targeted; `null` when it targeted Chat.
    #[serde(default)]
    draft_destinations: BTreeMap<String, Option<String>>,
    /// The work status sections the user folded or unfolded, by section ID
    /// (`workStatusExpandedSections`). The web keeps one map for every
    /// runtime, so this one is not keyed by runtime either.
    #[serde(default)]
    work_status_expanded_sections: BTreeMap<String, bool>,
}

/// The application's interface memory. Tests and synthetic runs keep it in
/// memory only.
#[derive(Default)]
pub(crate) struct UiMemory {
    path: Option<PathBuf>,
    file: MemoryFile,
    saving: Option<Task<()>>,
    dirty: bool,
}

impl Global for UiMemory {}

impl UiMemory {
    /// Reads the memory kept in `directory`.
    pub(crate) fn load(directory: &Path) -> Self {
        let path = directory.join(FILE_NAME);
        let read = || -> Option<MemoryFile> {
            let file = File::open(&path).ok()?;
            let mut bytes = Vec::new();
            file.take(MAX_BYTES + 1).read_to_end(&mut bytes).ok()?;
            if bytes.len() as u64 > MAX_BYTES {
                return None;
            }
            let file: MemoryFile = serde_json::from_slice(&bytes).ok()?;
            (file.version == 1).then_some(file)
        };
        let mut file = read().unwrap_or_default();
        file.version = 1;
        let now = now_millis();
        for trees in file.file_trees.values_mut() {
            trees.retain(|root, tree| {
                sanitize(root, tree);
                now.saturating_sub(tree.touched_at) < ROOT_TTL_MILLIS
            });
        }
        Self {
            path: Some(path),
            file,
            saving: None,
            dirty: false,
        }
    }

    /// The action last picked for `project` on `runtime`.
    pub(crate) fn action_pick(&self, runtime: &str, project: &str) -> Option<&str> {
        self.file
            .action_picks
            .get(runtime)?
            .get(project)
            .map(String::as_str)
    }

    /// The remembered tree of `root` on `runtime`.
    pub(crate) fn tree(&self, runtime: &str, root: &str) -> Option<&TreeMemory> {
        self.file.file_trees.get(runtime)?.get(root)
    }

    /// What a new session last targeted on `runtime`; `None` before the
    /// first one.
    pub(crate) fn draft_destination(&self, runtime: &str) -> Option<DraftDestination> {
        let directory = self.file.draft_destinations.get(runtime)?;
        Some(match directory {
            Some(directory) => DraftDestination::Project(directory.clone()),
            None => DraftDestination::Chat,
        })
    }

    /// Whether the user left the work status section `id` expanded; `None`
    /// until they fold or unfold it, when the section's default applies.
    pub(crate) fn work_status_section_expanded(&self, id: &str) -> Option<bool> {
        self.file.work_status_expanded_sections.get(id).copied()
    }
}

/// Remembers whether the work status section `id` is expanded.
pub(crate) fn set_work_status_section_expanded(id: &str, expanded: bool, cx: &mut App) {
    let memory = cx.default_global::<UiMemory>();
    if memory.file.work_status_expanded_sections.get(id) == Some(&expanded) {
        return;
    }
    memory
        .file
        .work_status_expanded_sections
        .insert(id.to_owned(), expanded);
    save(cx);
}

/// Remembers what a new session last targeted on `runtime`.
pub(crate) fn remember_draft_destination(
    runtime: &str,
    destination: &DraftDestination,
    cx: &mut App,
) {
    let directory = match destination {
        DraftDestination::Chat => None,
        DraftDestination::Project(directory) if directory.len() <= MAX_PATH_LENGTH => {
            Some(directory.clone())
        }
        DraftDestination::Project(_) => return,
    };
    let memory = cx.default_global::<UiMemory>();
    if memory.file.draft_destinations.get(runtime) == Some(&directory) {
        return;
    }
    memory
        .file
        .draft_destinations
        .insert(runtime.to_owned(), directory);
    save(cx);
}

/// Remembers `action` as `project`'s pick on `runtime`.
pub(crate) fn pick_action(runtime: &str, project: &str, action: &str, cx: &mut App) {
    let memory = cx.default_global::<UiMemory>();
    let picks = memory
        .file
        .action_picks
        .entry(runtime.to_owned())
        .or_default();
    if picks.get(project).map(String::as_str) == Some(action) {
        return;
    }
    picks.insert(project.to_owned(), action.to_owned());
    save(cx);
}

/// Replaces `root`'s remembered tree on `runtime`, keeping the most recently
/// touched roots.
pub(crate) fn remember_tree(
    runtime: &str,
    root: &str,
    expanded: Vec<String>,
    selected: Option<String>,
    cx: &mut App,
) {
    let memory = cx.default_global::<UiMemory>();
    let trees = memory
        .file
        .file_trees
        .entry(runtime.to_owned())
        .or_default();
    let mut tree = TreeMemory {
        expanded,
        selected,
        touched_at: now_millis(),
    };
    sanitize(root, &mut tree);
    if trees
        .get(root)
        .is_some_and(|known| known.expanded == tree.expanded && known.selected == tree.selected)
    {
        return;
    }
    trees.insert(root.to_owned(), tree);
    if trees.len() > MAX_ROOTS {
        let mut roots: Vec<(u64, String)> = trees
            .iter()
            .map(|(root, tree)| (tree.touched_at, root.clone()))
            .collect();
        roots.sort();
        for (_, root) in roots.into_iter().take(trees.len() - MAX_ROOTS) {
            trees.remove(&root);
        }
    }
    save(cx);
}

/// Paths outside the root, over-long ones and duplicates are dropped; the
/// newest expansions are the ones kept.
fn sanitize(root: &str, tree: &mut TreeMemory) {
    let within = |path: &str| {
        path.len() <= MAX_PATH_LENGTH
            && (path == root
                || path
                    .strip_prefix(root)
                    .is_some_and(|rest| rest.starts_with('/') || root.ends_with('/')))
    };
    let mut seen = std::collections::HashSet::new();
    let mut kept: Vec<String> = tree
        .expanded
        .iter()
        .rev()
        .filter(|path| within(path) && seen.insert(path.as_str()))
        .take(MAX_EXPANDED)
        .cloned()
        .collect();
    kept.reverse();
    tree.expanded = kept;
    if tree.selected.as_deref().is_some_and(|path| !within(path)) {
        tree.selected = None;
    }
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or_default()
}

fn save(cx: &mut App) {
    let memory = cx.default_global::<UiMemory>();
    let Some(path) = memory.path.clone() else {
        return;
    };
    if memory.saving.is_some() {
        memory.dirty = true;
        return;
    }
    memory.dirty = false;
    let bytes = match serde_json::to_vec(&memory.file) {
        Ok(bytes) => bytes,
        Err(error) => {
            tracing::warn!(%error, "the interface memory could not be encoded");
            return;
        }
    };
    let write = cx
        .background_executor()
        .spawn(async move { write_file(&path, &bytes) });
    let task = cx.spawn(async move |cx| {
        if let Err(error) = write.await {
            tracing::warn!(%error, "the interface memory could not be saved");
        }
        cx.update(|cx| {
            let memory = cx.default_global::<UiMemory>();
            memory.saving = None;
            if memory.dirty {
                save(cx);
            }
        });
    });
    cx.default_global::<UiMemory>().saving = Some(task);
}

fn write_file(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| std::io::Error::other("the memory file has no folder"))?;
    let mut directories = fs::DirBuilder::new();
    directories.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        directories.mode(0o700);
    }
    directories.create(parent)?;
    let temporary = parent.join(format!("{FILE_NAME}.partial-{}", std::process::id()));
    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let result = (|| {
        let mut output = options.open(&temporary)?;
        output.write_all(bytes)?;
        output.sync_all()?;
        fs::rename(&temporary, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_kit::TestAppContext;

    fn scratch(name: &str) -> PathBuf {
        let directory = std::env::temp_dir().join(format!(
            "openchamber-ui-memory-{name}-{}-{}",
            std::process::id(),
            now_millis()
        ));
        let _ = fs::remove_dir_all(&directory);
        directory
    }

    #[gpui_kit::test]
    fn the_last_draft_destination_survives_a_restart(cx: &mut TestAppContext) {
        let directory = scratch("draft-destination");
        cx.update(|cx| {
            cx.set_global(UiMemory::load(&directory));
            remember_draft_destination("chat", &DraftDestination::Chat, cx);
            remember_draft_destination("work", &DraftDestination::Project("/p".into()), cx);
        });
        cx.run_until_parked();
        let loaded = UiMemory::load(&directory);
        assert_eq!(
            loaded.draft_destination("chat"),
            Some(DraftDestination::Chat)
        );
        assert_eq!(
            loaded.draft_destination("work"),
            Some(DraftDestination::Project("/p".into()))
        );
        // A runtime that never opened a draft has no last side.
        assert_eq!(loaded.draft_destination("other"), None);
        let _ = fs::remove_dir_all(&directory);
    }

    #[gpui_kit::test]
    fn picks_and_trees_survive_a_restart(cx: &mut TestAppContext) {
        let directory = scratch("restart");
        cx.update(|cx| {
            cx.set_global(UiMemory::load(&directory));
            pick_action("rt", "/p", "dev", cx);
            remember_tree(
                "rt",
                "/p",
                vec![
                    "/p/src".into(),
                    "/elsewhere".into(),
                    "/p/src".into(),
                    "/p/docs".into(),
                ],
                Some("/p/src/main.rs".into()),
                cx,
            );
        });
        cx.run_until_parked();
        let loaded = UiMemory::load(&directory);
        assert_eq!(loaded.action_pick("rt", "/p"), Some("dev"));
        assert_eq!(
            loaded.action_pick("other", "/p"),
            None,
            "picks are per runtime"
        );
        let tree = loaded.tree("rt", "/p").expect("remembered");
        assert_eq!(
            tree.expanded,
            vec!["/p/src".to_owned(), "/p/docs".to_owned()],
            "outside paths and repeats are dropped"
        );
        assert_eq!(tree.selected.as_deref(), Some("/p/src/main.rs"));
        let _ = fs::remove_dir_all(&directory);
    }

    #[gpui_kit::test]
    fn folded_work_status_sections_survive_a_restart(cx: &mut TestAppContext) {
        let directory = scratch("sections");
        cx.update(|cx| {
            cx.set_global(UiMemory::load(&directory));
            assert_eq!(
                cx.global::<UiMemory>()
                    .work_status_section_expanded("subagents"),
                None,
                "an untouched section keeps its default"
            );
            set_work_status_section_expanded("subagents", false, cx);
        });
        cx.run_until_parked();
        let loaded = UiMemory::load(&directory);
        assert_eq!(
            loaded.work_status_section_expanded("subagents"),
            Some(false)
        );
        assert_eq!(loaded.work_status_section_expanded("telemetry"), None);
        let _ = fs::remove_dir_all(&directory);
    }

    #[test]
    fn a_file_without_section_states_still_loads() {
        let directory = scratch("legacy");
        fs::create_dir_all(&directory).expect("scratch folder");
        fs::write(
            directory.join(FILE_NAME),
            br#"{"version":1,"actionPicks":{"rt":{"/p":"dev"}},"fileTrees":{}}"#,
        )
        .expect("legacy file");
        let loaded = UiMemory::load(&directory);
        assert_eq!(loaded.action_pick("rt", "/p"), Some("dev"));
        assert_eq!(loaded.work_status_section_expanded("subagents"), None);
        let _ = fs::remove_dir_all(&directory);
    }

    #[gpui_kit::test]
    fn each_section_keeps_its_own_state_across_a_restart(cx: &mut TestAppContext) {
        let directory = scratch("section-states");
        cx.update(|cx| {
            cx.set_global(UiMemory::load(&directory));
            assert_eq!(
                cx.global::<UiMemory>()
                    .work_status_section_expanded("usage"),
                None,
                "an untouched section keeps its own default"
            );
            set_work_status_section_expanded("usage", true, cx);
            set_work_status_section_expanded("mcp", false, cx);
        });
        cx.run_until_parked();
        let loaded = UiMemory::load(&directory);
        assert_eq!(loaded.work_status_section_expanded("usage"), Some(true));
        assert_eq!(loaded.work_status_section_expanded("mcp"), Some(false));
        assert_eq!(loaded.work_status_section_expanded("pinned"), None);
        assert_eq!(loaded.action_pick("rt", "/p"), None);
        let _ = fs::remove_dir_all(&directory);
    }

    #[test]
    fn an_unreadable_file_starts_empty() {
        let directory = scratch("broken");
        fs::create_dir_all(&directory).expect("scratch folder");
        fs::write(directory.join(FILE_NAME), b"{not json").expect("broken file");
        let loaded = UiMemory::load(&directory);
        assert_eq!(loaded.action_pick("rt", "/p"), None);
        let _ = fs::remove_dir_all(&directory);
    }

    #[gpui_kit::test]
    fn the_oldest_roots_are_forgotten_past_the_limit(cx: &mut TestAppContext) {
        cx.update(|cx| {
            for index in 0..=MAX_ROOTS {
                remember_tree(
                    "rt",
                    &format!("/r{index}"),
                    vec![format!("/r{index}/a")],
                    None,
                    cx,
                );
            }
            let memory = cx.global::<UiMemory>();
            assert_eq!(memory.file.file_trees["rt"].len(), MAX_ROOTS);
        });
    }
}
