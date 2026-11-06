//! Sidebar previews of client-owned new-session drafts.

use gpui_kit::{App, Context, SharedString, Window};
use openchamber_native_core::draft::{Draft, DraftKey, DraftTarget};
use openchamber_native_core::sidebar_rows::SectionSpacing;

use crate::sidebar_item::SidebarItem;
use crate::workspace::{PROJECT_DRAFT_ID, Workspace};

fn preview(text: &str) -> Option<SharedString> {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(Into::into)
}

fn content_preview(draft: &Draft, cx: &App) -> Option<SharedString> {
    preview(&draft.text)
        .or_else(|| {
            draft
                .content
                .files
                .iter()
                .find_map(|file| file.name.as_deref().and_then(preview))
        })
        .or_else(|| {
            draft.content.context.iter().find_map(|item| {
                item.description
                    .as_deref()
                    .and_then(preview)
                    .or_else(|| preview(&item.text))
            })
        })
        .or_else(|| {
            (!draft.is_empty())
                .then(|| crate::locale::strings(cx).get("sessions.sidebar.session.untitled"))
        })
}

fn is_project_draft(key: &DraftKey) -> bool {
    matches!(key.target(), DraftTarget::NewSession(id) if id.as_str() == PROJECT_DRAFT_ID)
}

impl Workspace {
    pub(crate) fn restore_draft_previews(&mut self, cx: &App) {
        self.draft_previews = self
            .drafts
            .iter()
            .filter(|(key, _)| is_project_draft(key))
            .filter_map(|(key, draft)| content_preview(draft, cx).map(|label| (key.clone(), label)))
            .collect();
        for capture in self
            .starters
            .captures
            .iter()
            .filter(|capture| is_project_draft(&capture.owner))
        {
            if let Some(label) = content_preview(&capture.draft, cx) {
                self.draft_previews
                    .entry(capture.owner.clone())
                    .or_insert(label);
            }
        }
        self.rebuild_sidebar();
    }
    pub(crate) fn save_draft_preview(&mut self, key: &DraftKey, draft: &Draft, cx: &App) {
        if !is_project_draft(key) {
            return;
        }
        let label = content_preview(draft, cx).or_else(|| self.starter_draft_preview(key, cx));
        self.set_draft_preview(key, label);
    }

    fn starter_draft_preview(&self, key: &DraftKey, cx: &App) -> Option<SharedString> {
        self.starters
            .captures
            .iter()
            .find(|capture| &capture.owner == key)
            .and_then(|capture| content_preview(&capture.draft, cx))
    }

    /// A changed first line updates one row. Only adding/removing a draft
    /// changes sidebar structure, including when its project is collapsed.
    pub(crate) fn sync_draft_preview(&mut self, cx: &mut Context<Self>) {
        let Some(key) = self.composer_key.clone().filter(is_project_draft) else {
            return;
        };
        let label = self
            .drafts
            .get(&key)
            .and_then(|draft| content_preview(draft, cx))
            .or_else(|| self.starter_draft_preview(&key, cx));
        if self.set_draft_preview(&key, label) {
            cx.notify();
        }
    }

    fn set_draft_preview(&mut self, key: &DraftKey, label: Option<SharedString>) -> bool {
        let previous = self.draft_previews.get(key).cloned();
        if previous == label {
            return false;
        }
        match &label {
            Some(label) => {
                self.draft_previews.insert(key.clone(), label.clone());
            }
            None => {
                self.draft_previews.remove(key);
            }
        }
        if previous.is_some() != label.is_some() {
            self.rebuild_sidebar();
        } else if let Some(&index) = self.draft_rows.get(key)
            && let Some(SidebarItem::Draft { title, .. }) = self.sidebar_items.get_mut(index)
            && let Some(label) = label
        {
            *title = label;
            self.sidebar_list.remeasure_items(index..index + 1);
        }
        true
    }

    pub(crate) fn append_draft_rows(
        &mut self,
        items: &mut Vec<SidebarItem>,
        spacing: &mut Vec<Option<SectionSpacing>>,
    ) {
        let mut drafts: Vec<_> = self
            .draft_previews
            .iter()
            .filter(|(key, _)| key.runtime() == &self.runtime)
            .map(|(key, label)| (key.clone(), label.clone()))
            .collect();
        drafts.sort_by(|(left, _), (right, _)| left.directory().cmp(right.directory()));
        for (draft, title) in drafts {
            let project = self
                .topology
                .projects
                .iter()
                .find(|project| project.path == draft.directory());
            let placement = if let Some(project) = project {
                items.iter().position(|item| matches!(item, SidebarItem::Project { project_id, collapsed: false, .. } if project_id == &project.id)).map(|index| (index, project.id.to_string()))
            } else {
                None
            };
            let Some((index, group)) = placement else {
                continue;
            };
            let DraftTarget::NewSession(id) = draft.target() else {
                continue;
            };
            let after = spacing[index].take();
            items.insert(
                index + 1,
                SidebarItem::Draft {
                    key: format!("{group}:root:draft:{id}").into(),
                    draft,
                    title,
                },
            );
            spacing.insert(index + 1, after);
        }
        self.draft_rows = items
            .iter()
            .enumerate()
            .filter_map(|(index, item)| match item {
                SidebarItem::Draft { draft, .. } => Some((draft.clone(), index)),
                _ => None,
            })
            .collect();
    }

    pub(crate) fn open_draft(
        &mut self,
        key: &DraftKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if key.runtime() != &self.runtime || !self.draft_previews.contains_key(key) {
            return;
        }
        if self.composer_key.as_ref() == Some(key) && self.selected.is_none() {
            self.focus_composer(window, cx);
            return;
        }
        self.show_new_session(window, cx);
        self.save_composer_draft(cx);
        self.composer_key = Some(key.clone());
        self.draft_projects
            .insert(self.runtime.clone(), key.directory().to_owned());
        self.restore_composer_draft(window, cx);
        self.focus_composer(window, cx);
        self.rebuild_sidebar();
        cx.notify();
    }

    pub(crate) fn discard_draft(
        &mut self,
        key: &DraftKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if key.runtime() != &self.runtime || !is_project_draft(key) || self.close_in_progress() {
            return;
        }
        self.navigation_revision = self.navigation_revision.wrapping_add(1);
        let resources: Vec<_> = self
            .drafts
            .get(key)
            .into_iter()
            .flat_map(|draft| &draft.content.files)
            .chain(
                self.starters
                    .captures
                    .iter()
                    .filter(|capture| &capture.owner == key)
                    .flat_map(|capture| &capture.draft.content.files),
            )
            .map(|file| file.id.clone())
            .collect();
        let tickets: Vec<_> = self
            .attachment_storage
            .rows(key)
            .map(|row| row.ticket)
            .collect();
        self.attachment_input.cancel(key);
        self.cancel_attachment_inputs(key);
        self.invalidate_attachment_staging(key, None, cx);
        for ticket in tickets {
            self.cancel_attachment_input(ticket, window, cx);
        }
        self.starters.discard_captures(key);
        let _ = self.drafts.take(key);
        self.draft_previews.remove(key);
        if self.composer_key.as_ref() == Some(key) {
            self.composer
                .update(cx, |composer, cx| composer.set_value("", window, cx));
            self.focus_composer(window, cx);
        }
        self.rebuild_sidebar();
        self.queue_persistence(cx);
        for id in resources {
            self.retire_attachment(id, window, cx);
        }
        cx.notify();
    }
}
