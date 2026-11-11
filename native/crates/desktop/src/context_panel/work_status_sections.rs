//! The profile's work-status layout and the dialog that edits it.
//!
//! Content renderers remain owned by their sections. Accepted layout edits use
//! the retained GeneralSettings owner, including after this dialog closes.

use std::collections::{HashMap, HashSet};

use gpui_kit::component::WindowExt;
use gpui_kit::component::dialog::{DialogDescription, DialogTitle};
use gpui_kit::{
    AnyElement, App, AppContext, Bounds, Context, DispatchPhase, Entity, FocusHandle, FontWeight,
    InteractiveElement, IntoElement, KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent,
    ParentElement, Pixels, Point, Render, Role, SharedString, StatefulInteractiveElement, Styled,
    Subscription, TestSupportExt, WeakEntity, Window, canvas, div, prelude::FluentBuilder, px,
};
use openchamber_native_core::ids::RuntimeId;
use openchamber_native_transport::client::{ServerSettings, WorkStatusSectionList};

use crate::activation::on_activate;
use crate::baseline::CssTextExt;
use crate::chrome::glyph;
use crate::general_settings::GeneralSettings;
use crate::locale;
use crate::logical::LogicalStyledExt;
use crate::settings_controls;
use crate::theme::Palette;
use crate::workspace::Workspace;

/// `WORK_STATUS_SECTION_IDS` and `WORK_STATUS_SECTION_LABEL_KEYS` of 2.6.1.
pub(crate) const BUILT_INS: [(&str, &str); 9] = [
    ("session", "chat.workStatus.section.session"),
    ("repository", "chat.workStatus.section.project"),
    ("usage", "chat.workStatus.section.usage"),
    ("telemetry", "chat.workStatus.section.telemetry"),
    ("subagents", "chat.workStatus.section.subagents"),
    ("workflows", "chat.workStatus.section.workflows"),
    ("mcp", "chat.workStatus.section.mcp"),
    ("pinned", "chat.workStatus.section.pinned"),
    ("contextSources", "chat.workStatus.section.contextBreakdown"),
];

fn label_key(id: &str) -> Option<&'static str> {
    BUILT_INS
        .iter()
        .find(|(known, _)| *known == id)
        .map(|(_, key)| *key)
}

fn valid_id(id: &str) -> bool {
    if label_key(id).is_some() {
        return true;
    }
    let Some(extension) = id.strip_prefix("ext:") else {
        return false;
    };
    extension
        .as_bytes()
        .first()
        .is_some_and(u8::is_ascii_lowercase)
        && extension
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

fn distinct_ids(ids: &[String]) -> Vec<String> {
    let mut result = Vec::new();
    let mut seen = HashSet::new();
    for id in ids {
        if valid_id(id) && seen.insert(id.as_str()) {
            result.push(id.clone());
        }
    }
    result
}

/// Preserve valid saved slots, including unavailable extensions, and append
/// every missing built-in in its canonical order.
pub(crate) fn sanitize_order(ids: &[String]) -> Vec<String> {
    let mut result = distinct_ids(ids);
    for (id, _) in BUILT_INS {
        if !result.iter().any(|known| known == id) {
            result.push(id.to_owned());
        }
    }
    result
}

/// A missing explicit marker denotes the old automatic telemetry default.
pub(crate) fn sanitize_hidden(ids: &[String], explicit: bool) -> Vec<String> {
    distinct_ids(ids)
        .into_iter()
        .filter(|id| explicit || id != "telemetry")
        .collect()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Layout {
    order: Vec<String>,
    hidden: Vec<String>,
}

impl Layout {
    pub(crate) fn from_settings(settings: Option<&ServerSettings>) -> Self {
        Self::from_fields(
            settings
                .and_then(|settings| settings.work_status_section_order.as_ref())
                .and_then(WorkStatusSectionList::last_valid_entries),
            settings
                .and_then(|settings| settings.work_status_hidden_sections.as_ref())
                .and_then(WorkStatusSectionList::last_valid_entries),
            settings.map(|settings| {
                matches!(
                    settings.work_status_hidden_sections,
                    Some(WorkStatusSectionList::MalformedWithPrior(_))
                ) || settings.work_status_hidden_sections_explicit == Some(true)
            }),
        )
    }

    pub(crate) fn from_fields(
        order: Option<&[String]>,
        hidden: Option<&[String]>,
        explicit: Option<bool>,
    ) -> Self {
        Self {
            order: sanitize_order(order.unwrap_or_default()),
            hidden: sanitize_hidden(hidden.unwrap_or_default(), explicit == Some(true)),
        }
    }

    pub(crate) fn for_workspace(workspace: &Workspace, cx: &App) -> Self {
        let settings = workspace.send_catalog.settings.as_ref();
        if let Some(model) = &workspace.settings {
            return model
                .read(cx)
                .general
                .read(cx)
                .work_status_layout(&workspace.runtime, settings);
        }
        Self::from_settings(settings)
    }

    /// Owner lists are normalized when a valid field is adopted. A missing or
    /// malformed later field cannot reapply migration to that known choice.
    pub(crate) fn with_known_lists(
        mut self,
        order: Option<&[String]>,
        hidden: Option<&[String]>,
    ) -> Self {
        if let Some(order) = order {
            self.order = sanitize_order(order);
        }
        if let Some(hidden) = hidden {
            self.hidden = sanitize_hidden(hidden, true);
        }
        self
    }

    fn builtin_order(&self) -> Vec<String> {
        self.order
            .iter()
            .filter(|id| label_key(id).is_some())
            .cloned()
            .collect()
    }

    pub(crate) fn visible_builtin_order(&self) -> impl Iterator<Item = &str> {
        self.order
            .iter()
            .filter(|id| label_key(id).is_some() && !self.hidden.contains(id))
            .map(String::as_str)
    }

    pub(crate) fn all_hidden(&self) -> bool {
        BUILT_INS
            .iter()
            .all(|(id, _)| self.hidden.iter().any(|hidden| hidden == id))
    }

    fn all_visible(&self) -> bool {
        BUILT_INS
            .iter()
            .all(|(id, _)| !self.hidden.iter().any(|hidden| hidden == id))
    }
}

/// Keep the last validated presentation inside the existing settings record.
/// The retained variant still records that this successful document was partial.
pub(crate) fn retain_validated_layout(
    incoming: &mut ServerSettings,
    previous: Option<&ServerSettings>,
) {
    let Some(previous) = previous else { return };
    if matches!(
        incoming.work_status_section_order,
        Some(WorkStatusSectionList::Malformed)
    ) && let Some(prior) = previous
        .work_status_section_order
        .as_ref()
        .and_then(WorkStatusSectionList::last_valid_entries)
    {
        incoming.work_status_section_order =
            Some(WorkStatusSectionList::MalformedWithPrior(prior.to_vec()));
    }
    if matches!(
        incoming.work_status_hidden_sections,
        Some(WorkStatusSectionList::Malformed)
    ) && let Some(prior) = previous
        .work_status_hidden_sections
        .as_ref()
        .and_then(WorkStatusSectionList::last_valid_entries)
    {
        let normalized = sanitize_hidden(
            prior,
            matches!(
                previous.work_status_hidden_sections,
                Some(WorkStatusSectionList::MalformedWithPrior(_))
            ) || previous.work_status_hidden_sections_explicit == Some(true),
        );
        incoming.work_status_hidden_sections =
            Some(WorkStatusSectionList::MalformedWithPrior(normalized));
    }
}

#[derive(Clone, PartialEq, Eq)]
struct Scope {
    runtime: RuntimeId,
    generation: Option<u64>,
}

#[derive(Clone, Copy)]
enum DragInput {
    Keyboard,
    Pointer {
        start: Point<Pixels>,
        offset: Pixels,
        active: bool,
    },
}

struct Drag {
    scope: Scope,
    id: String,
    saved_order: Vec<String>,
    original: Vec<String>,
    preview: Vec<String>,
    bounds: HashMap<String, Bounds<Pixels>>,
    input: DragInput,
}

impl Drag {
    fn active(&self) -> bool {
        match self.input {
            DragInput::Keyboard => true,
            DragInput::Pointer { active, .. } => active,
        }
    }

    fn move_to(&mut self, target: usize) {
        let Some(from) = self.preview.iter().position(|id| id == &self.id) else {
            return;
        };
        let id = self.preview.remove(from);
        self.preview.insert(target.min(self.preview.len()), id);
    }

    fn offset(&self, id: &str) -> Pixels {
        if id == self.id
            && let DragInput::Pointer { offset, .. } = self.input
        {
            return offset;
        }
        let Some(position) = self.preview.iter().position(|entry| entry == id) else {
            return px(0.0);
        };
        let Some(slot) = self
            .original
            .get(position)
            .and_then(|slot| self.bounds.get(slot))
        else {
            return px(0.0);
        };
        self.bounds
            .get(id)
            .map_or(px(0.0), |bounds| slot.origin.y - bounds.origin.y)
    }
}

struct SectionsDialog {
    owner: WeakEntity<Workspace>,
    general: Entity<GeneralSettings>,
    handles: HashMap<SharedString, FocusHandle>,
    bounds: HashMap<String, Bounds<Pixels>>,
    drag: Option<Drag>,
    announcement: SharedString,
    _subscriptions: Vec<Subscription>,
}

impl Workspace {
    pub(crate) fn open_work_status_sections(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.close_in_progress() {
            open(self, window, cx);
        }
    }
}

fn open(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Entity<SectionsDialog> {
    let return_focus = window.focused(cx);
    let settings = workspace.ensure_settings_model(window, cx);
    let general = settings.read(cx).general.clone();
    let owner = cx.weak_entity();
    let dialog = cx.new(|cx: &mut Context<SectionsDialog>| {
        let mut subscriptions = vec![cx.observe(&general, |this, _, cx| {
            this.reconcile_drag(cx);
            cx.notify();
        })];
        subscriptions.push(cx.observe(
            &owner.upgrade().expect("opening workspace is retained"),
            |this, _, cx| {
                this.reconcile_drag(cx);
                cx.notify();
            },
        ));
        SectionsDialog {
            owner,
            general,
            handles: HashMap::new(),
            bounds: HashMap::new(),
            drag: None,
            announcement: SharedString::default(),
            _subscriptions: subscriptions,
        }
    });
    let view = dialog.clone();
    window.open_dialog(cx, move |dialog, _, cx| {
        let return_focus = return_focus.clone();
        dialog
            .w(px(448.0))
            .p(px(24.0))
            .keyboard(!view.read(cx).drag.as_ref().is_some_and(Drag::active))
            .on_ok(|_, _, _| false)
            .on_close(move |_, window, cx| {
                if let Some(focus) = &return_focus {
                    window.focus(focus, cx);
                }
            })
            .child(view.clone())
    });
    dialog
}

impl SectionsDialog {
    fn current(&self, cx: &App) -> Option<(Scope, Layout)> {
        let owner = self.owner.upgrade()?;
        let workspace = owner.read(cx);
        Some((
            Scope {
                runtime: workspace.runtime.clone(),
                generation: workspace
                    .connection
                    .as_ref()
                    .map(|connection| connection.generation),
            },
            Layout::for_workspace(workspace, cx),
        ))
    }

    fn owns_scope(&self, scope: &Scope, cx: &App) -> bool {
        self.owner.upgrade().is_some_and(|owner| {
            let workspace = owner.read(cx);
            workspace.runtime == scope.runtime
                && workspace
                    .connection
                    .as_ref()
                    .map(|connection| connection.generation)
                    == scope.generation
        })
    }

    fn set_visible(&mut self, scope: &Scope, id: &str, visible: bool, cx: &mut Context<Self>) {
        if !self.visibility_editable(scope, cx) || label_key(id).is_none() {
            return;
        }
        let Some((_, mut layout)) = self.current(cx) else {
            return;
        };
        layout.hidden.retain(|hidden| hidden != id);
        if !visible {
            layout.hidden.push(id.to_owned());
        }
        self.general.update(cx, |general, cx| {
            general.set_work_status_hidden(&scope.runtime, &layout.hidden, cx)
        });
    }

    fn visibility_editable(&self, scope: &Scope, cx: &App) -> bool {
        self.owns_scope(scope, cx)
            && self
                .general
                .read(cx)
                .work_status_visibility_editable(&scope.runtime, scope.generation)
    }

    fn order_editable(&self, scope: &Scope, cx: &App) -> bool {
        self.owns_scope(scope, cx)
            && self
                .general
                .read(cx)
                .work_status_order_editable(&scope.runtime, scope.generation)
    }

    fn show_all(&mut self, scope: &Scope, cx: &mut Context<Self>) {
        if self.visibility_editable(scope, cx) {
            self.general.update(cx, |general, cx| {
                general.set_work_status_hidden(&scope.runtime, &[], cx)
            });
        }
    }

    fn begin(&mut self, scope: &Scope, id: &str, input: DragInput, cx: &mut Context<Self>) {
        if !self.order_editable(scope, cx) {
            return;
        }
        let Some((_, layout)) = self.current(cx) else {
            return;
        };
        let order = layout.builtin_order();
        if !order.iter().any(|entry| entry == id) {
            return;
        }
        self.drag = Some(Drag {
            scope: scope.clone(),
            id: id.to_owned(),
            saved_order: layout.order,
            original: order.clone(),
            preview: order,
            bounds: self.bounds.clone(),
            input,
        });
        if self.drag.as_ref().is_some_and(Drag::active) {
            self.announce_position(cx);
        }
        cx.notify();
    }

    fn reconcile_drag(&mut self, cx: &mut Context<Self>) {
        if self.drag.as_ref().is_some_and(|drag| {
            !self.order_editable(&drag.scope, cx)
                || self.current(cx).is_none_or(|(scope, layout)| {
                    scope != drag.scope || layout.order != drag.saved_order
                })
        }) {
            self.cancel(cx);
        }
    }

    fn cancel(&mut self, cx: &mut Context<Self>) {
        if self.drag.take().is_some() {
            self.announcement = locale::strings(cx).get("chat.workStatus.sections.dragCancelled");
            cx.notify();
        }
    }

    fn announce_position(&mut self, cx: &mut Context<Self>) {
        let Some(drag) = &self.drag else { return };
        let Some(key) = label_key(&drag.id) else {
            return;
        };
        let position = drag
            .preview
            .iter()
            .position(|id| id == &drag.id)
            .map_or(1, |position| position + 1);
        let strings = locale::strings(cx);
        self.announcement = strings.format(
            "chat.workStatus.sections.position",
            &[
                ("label", strings.get(key).as_ref()),
                ("position", &position.to_string()),
                ("count", &drag.preview.len().to_string()),
            ],
        );
    }

    fn commit(&mut self, cx: &mut Context<Self>) {
        self.reconcile_drag(cx);
        if self.drag.as_ref().is_some_and(Drag::active) {
            self.announce_position(cx);
        }
        let Some(drag) = self.drag.take() else { return };
        if drag.active() && drag.preview != drag.original && self.order_editable(&drag.scope, cx) {
            let mut order = drag.preview;
            // Unloaded/paused extension sections retain a saved slot at the
            // end, exactly as handleDragEnd does with its unavailable ids.
            order.extend(
                drag.saved_order
                    .into_iter()
                    .filter(|id| label_key(id).is_none()),
            );
            self.general.update(cx, |general, cx| {
                general.set_work_status_order(&drag.scope.runtime, &order, cx)
            });
        }
        cx.notify();
    }

    fn pointer_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.reconcile_drag(cx);
        let Some(drag) = &mut self.drag else { return };
        let DragInput::Pointer {
            start,
            offset,
            active,
        } = &mut drag.input
        else {
            return;
        };
        let x = f32::from(event.position.x - start.x);
        let y = f32::from(event.position.y - start.y);
        if !*active && x * x + y * y <= 64.0 {
            return;
        }
        *active = true;
        *offset = event.position.y - start.y;
        let drag_center = drag
            .bounds
            .get(&drag.id)
            .map(|bounds| bounds.origin.y + bounds.size.height / 2.0 + *offset);
        // Frozen pre-drag slot geometry avoids oscillating as siblings move.
        let target = drag
            .original
            .iter()
            .enumerate()
            .filter_map(|(index, id)| {
                let bounds = drag.bounds.get(id)?;
                let center = bounds.origin.y + bounds.size.height / 2.0;
                Some((index, f32::from(center - drag_center?).abs()))
            })
            .min_by(|left, right| left.1.total_cmp(&right.1))
            .map(|(index, _)| index);
        if let Some(target) = target {
            drag.move_to(target);
        }
        self.announce_position(cx);
        cx.notify();
    }

    fn reorder_key(
        &mut self,
        scope: &Scope,
        id: &str,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if event.keystroke.modifiers.control
            || event.keystroke.modifiers.alt
            || event.keystroke.modifiers.platform
            || event.keystroke.modifiers.shift
        {
            return;
        }
        self.reconcile_drag(cx);
        match event.keystroke.key.as_str() {
            "space" | "enter" => {
                if self.drag.as_ref().is_some_and(|drag| drag.id == id) {
                    self.commit(cx);
                } else {
                    self.begin(scope, id, DragInput::Keyboard, cx);
                }
            }
            "up" | "down" if self.drag.as_ref().is_some_and(|drag| drag.id == id) => {
                let drag = self
                    .drag
                    .as_mut()
                    .expect("the active keyboard drag was checked");
                let current = drag
                    .preview
                    .iter()
                    .position(|entry| entry == id)
                    .expect("the active id is in its preview");
                let target = if event.keystroke.key == "up" {
                    current.saturating_sub(1)
                } else {
                    (current + 1).min(drag.preview.len() - 1)
                };
                drag.move_to(target);
                self.announce_position(cx);
                cx.notify();
            }
            _ => return,
        }
        window.prevent_default();
        cx.stop_propagation();
    }

    fn escape(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if event.keystroke.key == "escape" && self.drag.as_ref().is_some_and(Drag::active) {
            self.cancel(cx);
            window.prevent_default();
            cx.stop_propagation();
        }
    }

    fn row(
        &mut self,
        scope: &Scope,
        layout: &Layout,
        id: &str,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let strings = locale::strings(cx);
        let palette = cx.global::<Palette>().clone();
        let label = strings.get(label_key(id).expect("only canonical built-ins have rows"));
        let description = strings.get("chat.workStatus.sections.dragInstructions");
        let reorder_label = strings.format(
            "chat.workStatus.sections.reorder",
            &[("label", label.as_ref())],
        );
        let direction = strings.direction();
        let reorder_enabled = self.order_editable(scope, cx);
        let visibility_enabled = self.visibility_editable(scope, cx);
        let handle_id: SharedString = format!("work-status-reorder-{id}").into();
        let handle_focus = settings_controls::focus_handle(&mut self.handles, &handle_id, cx);
        let checkbox_id: SharedString = format!("work-status-visible-{id}").into();
        let checkbox_focus = settings_controls::focus_handle(&mut self.handles, &checkbox_id, cx);
        let handle = div()
            .id(handle_id.clone())
            .test_support()
            .accessibility_id(handle_id)
            .role(Role::Button)
            .aria_label(reorder_label)
            .aria_description(description)
            .track_focus(&handle_focus)
            .tab_index(0)
            .size(px(32.0))
            .flex_shrink_0()
            .rounded(px(7.0))
            .flex()
            .items_center()
            .justify_center()
            .cursor_grab()
            .child(glyph("draggable", 16.0, palette.muted_foreground));
        let handle = if reorder_enabled {
            let (pointer_scope, pointer_id) = (scope.clone(), id.to_owned());
            let (key_scope, key_id) = (scope.clone(), id.to_owned());
            let pointer_focus = handle_focus.clone();
            handle
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                        window.focus(&pointer_focus, cx);
                        this.begin(
                            &pointer_scope,
                            &pointer_id,
                            DragInput::Pointer {
                                start: event.position,
                                offset: px(0.0),
                                active: false,
                            },
                            cx,
                        );
                        cx.stop_propagation();
                    }),
                )
                .on_key_down(cx.listener(move |this, event, window, cx| {
                    this.reorder_key(&key_scope, &key_id, event, window, cx)
                }))
        } else {
            handle
                .opacity(0.5)
                .a11y_synthetic_children(|builder| builder.parent_node().set_disabled())
        };
        let (checkbox_scope, checkbox_section) = (scope.clone(), id.to_owned());
        let checkbox = settings_controls::checkbox_row(
            checkbox_id,
            label.clone(),
            label,
            !layout.hidden.iter().any(|hidden| hidden == id),
            visibility_enabled,
            &checkbox_focus,
            None,
            cx,
            move |this, checked, _, cx| {
                this.set_visible(&checkbox_scope, &checkbox_section, checked, cx)
            },
        );
        let weak = cx.weak_entity();
        let measured_id = id.to_owned();
        let offset = self
            .drag
            .as_ref()
            .filter(|drag| drag.active())
            .map_or(px(0.0), |drag| drag.offset(id));
        div()
            .id(SharedString::from(format!("work-status-section-row-{id}")))
            .relative()
            .top(offset)
            .flex_row_from(direction)
            .items_center()
            .gap(px(8.0))
            .when(
                self.drag
                    .as_ref()
                    .is_some_and(|drag| drag.active() && drag.id == id),
                |row| row.opacity(0.6),
            )
            .child(handle)
            .child(div().flex_1().min_w_0().child(checkbox))
            .child(
                canvas(
                    move |bounds, _, cx| {
                        let _ = weak.update(cx, |this, _| {
                            this.bounds.insert(measured_id.clone(), bounds);
                        });
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full(),
            )
            .into_any_element()
    }
}

impl Render for SectionsDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let strings = locale::strings(cx).clone();
        let palette = cx.global::<Palette>().clone();
        let direction = strings.direction();
        let mut body = div()
            .id("work-status-sections-dialog")
            .accessibility_id("work-status-sections-dialog")
            .role(Role::Group)
            .aria_label(strings.get("chat.workStatus.sections.dialogTitle"))
            .flex()
            .flex_col()
            .min_w_0()
            .css_text(0.875, 1.25, cx)
            .text_color(palette.foreground)
            .when(direction == locale::Direction::RightToLeft, |body| {
                body.text_right()
            })
            .capture_key_down(cx.listener(Self::escape))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    if this
                        .drag
                        .as_ref()
                        .is_some_and(|drag| matches!(drag.input, DragInput::Pointer { .. }))
                    {
                        this.commit(cx);
                    }
                }),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    if this
                        .drag
                        .as_ref()
                        .is_some_and(|drag| matches!(drag.input, DragInput::Pointer { .. }))
                    {
                        this.commit(cx);
                    }
                }),
            )
            .child(
                DialogTitle::new()
                    .font_weight(FontWeight::SEMIBOLD)
                    .css_text(1.125, 1.75, cx)
                    .child(strings.get("chat.workStatus.sections.dialogTitle")),
            )
            .child(
                DialogDescription::new()
                    .mt(px(6.0))
                    .mb(px(16.0))
                    .text_color(palette.muted_foreground)
                    .child(strings.get("chat.workStatus.sections.dialogDescription")),
            );
        if let Some((scope, layout)) = self.current(cx) {
            for id in layout.builtin_order() {
                body = body.child(self.row(&scope, &layout, &id, cx));
            }
            if !layout.all_visible() {
                let enabled = self.visibility_editable(&scope, cx);
                let foreground = palette.foreground;
                let show_all = on_activate(
                    div()
                        .id("work-status-sections-show-all")
                        .accessibility_id("work-status-sections-show-all")
                        .role(Role::Button)
                        .aria_label(strings.get("chat.workStatus.sections.showAll"))
                        .tab_index(0)
                        .cursor_pointer()
                        .css_text(0.75, 1.0, cx)
                        .text_color(palette.muted_foreground)
                        .hover(move |style| style.text_color(foreground))
                        .child(strings.get("chat.workStatus.sections.showAll")),
                    cx,
                    move |this, _, cx| {
                        if enabled {
                            this.show_all(&scope, cx);
                        }
                    },
                );
                body = body.child(
                    div()
                        .mt(px(12.0))
                        .pt(px(12.0))
                        .border_t_1()
                        .border_color(palette.border)
                        .flex_row_from(direction)
                        .items_center()
                        .justify_between()
                        .child(div().flex_1().when(layout.all_hidden(), |warning| {
                            warning
                                .css_text(0.75, 1.0, cx)
                                .text_color(palette.status_error)
                                .child(strings.get("chat.workStatus.sections.noneWarning"))
                        }))
                        .child(show_all.test_support()),
                );
            }
        }
        let announcement = self.announcement.clone();
        let pointer_owner = cx.weak_entity();
        body.child(
            // A drag remains owned when the pointer leaves the dialog. The
            // element-level move callback only receives hovered moves.
            canvas(
                |_, _, _| {},
                move |_, _, window, _| {
                    let pointer_owner = pointer_owner.clone();
                    window.on_mouse_event(move |event: &MouseMoveEvent, phase, window, cx| {
                        if phase == DispatchPhase::Capture {
                            let _ = pointer_owner.update(cx, |this, cx| {
                                if this.drag.as_ref().is_some_and(|drag| {
                                    matches!(drag.input, DragInput::Pointer { .. })
                                }) {
                                    this.pointer_move(event, window, cx);
                                    cx.stop_propagation();
                                }
                            });
                        }
                    });
                },
            )
            .absolute()
            .size_full(),
        )
        .child(
            div()
                .id("work-status-reorder-announcement")
                .role(Role::Status)
                .aria_label(announcement.clone())
                .a11y_synthetic_children(|builder| {
                    builder
                        .parent_node()
                        .set_live(gpui_kit::accesskit::Live::Polite)
                })
                .absolute()
                .size(px(1.0))
                .overflow_hidden()
                .child(announcement),
        )
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write as _};
    use std::net::{TcpListener, TcpStream};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::Duration;

    use gpui_kit::test::TestWindowExt;
    use gpui_kit::{AnyWindowHandle, TestAppContext};
    use serde_json::{Value, json};

    use crate::general_settings::{Choice, FOLLOW_UP, PROMPT_NAVIGATOR};
    use crate::settings_controls::tests::ResponseGate;
    use crate::settings_navigation::Page;
    use crate::settings_test_server::{self, Server};

    use super::*;

    struct Profile {
        server: Server,
        saved: Arc<Mutex<Value>>,
        fail_reads: Arc<AtomicBool>,
        fail_writes: Arc<AtomicBool>,
    }

    impl Profile {
        fn new(document: Value, held_write: Option<Arc<ResponseGate>>) -> Self {
            let saved = Arc::new(Mutex::new(document));
            let fail_reads = Arc::new(AtomicBool::new(false));
            let fail_writes = Arc::new(AtomicBool::new(false));
            let (stored, reads, writes) = (saved.clone(), fail_reads.clone(), fail_writes.clone());
            let hold_once = AtomicBool::new(held_write.is_some());
            let server = Server::new(move |request| {
                if request.is("GET", "/api/config/settings") {
                    return if reads.load(Ordering::Acquire) {
                        (503, json!({}))
                    } else {
                        (200, stored.lock().expect("profile").clone())
                    };
                }
                if request.is("PUT", "/api/config/settings") {
                    if hold_once.swap(false, Ordering::AcqRel)
                        && let Some(gate) = &held_write
                    {
                        gate.hold();
                    }
                    if writes.load(Ordering::Acquire) {
                        return (503, json!({}));
                    }
                    let mut profile = stored.lock().expect("profile");
                    profile.as_object_mut().expect("profile object").extend(
                        request
                            .body
                            .as_object()
                            .expect("partial settings write")
                            .clone(),
                    );
                    return (200, profile.clone());
                }
                (200, json!({"shells": []}))
            });
            Self {
                server,
                saved,
                fail_reads,
                fail_writes,
            }
        }

        fn saved(&self) -> Value {
            self.saved.lock().expect("profile").clone()
        }
    }

    struct HeldResponse {
        method: &'static str,
        path: &'static str,
        gate: Arc<ResponseGate>,
        used: bool,
    }

    /// Separate connections can finish out of order. Each held response keeps
    /// the document it actually read, so a later request cannot rewrite it.
    struct LayoutServer {
        endpoint: openchamber_native_transport::endpoint::Endpoint,
        document: Arc<Mutex<Value>>,
        requests: Arc<Mutex<Vec<settings_test_server::Request>>>,
        gates: Arc<Mutex<Vec<HeldResponse>>>,
        fail_writes: Arc<AtomicBool>,
        fail_home_once: Arc<AtomicBool>,
        fail_next_settings_read: Arc<AtomicBool>,
        stop: Arc<AtomicBool>,
        worker: Option<thread::JoinHandle<()>>,
    }

    impl LayoutServer {
        fn new(document: Value) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").expect("owned layout server");
            let endpoint = openchamber_native_transport::endpoint::Endpoint::parse(&format!(
                "http://{}",
                listener.local_addr().expect("address")
            ))
            .expect("endpoint");
            let document = Arc::new(Mutex::new(document));
            let requests = Arc::new(Mutex::new(Vec::new()));
            let gates: Arc<Mutex<Vec<HeldResponse>>> = Arc::new(Mutex::new(Vec::new()));
            let fail_writes = Arc::new(AtomicBool::new(false));
            let fail_home_once = Arc::new(AtomicBool::new(false));
            let fail_next_settings_read = Arc::new(AtomicBool::new(false));
            let stop = Arc::new(AtomicBool::new(false));
            let (saved, recorded, held, failed, home, reject_read, stopped) = (
                document.clone(),
                requests.clone(),
                gates.clone(),
                fail_writes.clone(),
                fail_home_once.clone(),
                fail_next_settings_read.clone(),
                stop.clone(),
            );
            let worker = thread::spawn(move || {
                let mut connections = Vec::new();
                for stream in listener.incoming() {
                    if stopped.load(Ordering::Acquire) {
                        break;
                    }
                    let Ok(mut stream) = stream else { continue };
                    let (saved, recorded, held, failed, home, reject_read) = (
                        saved.clone(),
                        recorded.clone(),
                        held.clone(),
                        failed.clone(),
                        home.clone(),
                        reject_read.clone(),
                    );
                    connections.push(thread::spawn(move || {
                        let _ = stream.set_read_timeout(Some(Duration::from_secs(3)));
                        let Some(request) = Self::read_request(&mut stream) else { return };
                        let (status, body) = if request.is("GET", "/api/config/settings") {
                            (if reject_read.swap(false, Ordering::AcqRel) { 503 } else { 200 }, saved.lock().expect("document").clone())
                        } else if request.is("PUT", "/api/config/settings") {
                            if failed.load(Ordering::Acquire) { (503, json!({})) }
                            else {
                                let mut saved = saved.lock().expect("document");
                                saved.as_object_mut().expect("settings object").extend(request.body.as_object().expect("partial write").clone());
                                (200, saved.clone())
                            }
                        } else if request.is("GET", "/api/fs/home") {
                            (if home.swap(false, Ordering::AcqRel) { 503 } else { 200 }, json!({}))
                        } else { (200, json!({"shells": []})) };
                        let gate = {
                            let mut held = held.lock().expect("gates");
                            held.iter_mut().find(|entry| !entry.used && request.is(entry.method, entry.path)).map(|entry| {
                                entry.used = true;
                                entry.gate.clone()
                            })
                        };
                        recorded.lock().expect("requests").push(request);
                        if let Some(gate) = gate { gate.hold(); }
                        let body = body.to_string();
                        let _ = write!(stream, "HTTP/1.1 {status} Result\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                    }));
                }
                for connection in connections {
                    connection.join().expect("layout request worker");
                }
            });
            Self {
                endpoint,
                document,
                requests,
                gates,
                fail_writes,
                fail_home_once,
                fail_next_settings_read,
                stop,
                worker: Some(worker),
            }
        }

        fn read_request(stream: &mut TcpStream) -> Option<settings_test_server::Request> {
            let mut headers = Vec::new();
            while !headers.ends_with(b"\r\n\r\n") {
                if headers.len() >= 32768 {
                    return None;
                }
                let mut byte = [0];
                stream.read_exact(&mut byte).ok()?;
                headers.push(byte[0]);
            }
            let headers = String::from_utf8(headers).ok()?;
            let mut line = headers.lines().next()?.split_whitespace();
            let method = line.next()?.to_owned();
            let target = line.next()?.to_owned();
            let length: usize = headers
                .lines()
                .filter_map(|line| line.split_once(':'))
                .find(|(key, _)| key.eq_ignore_ascii_case("content-length"))
                .and_then(|(_, value)| value.trim().parse().ok())
                .unwrap_or(0);
            if length > 1024 * 1024 {
                return None;
            }
            let mut body = vec![0; length];
            stream.read_exact(&mut body).ok()?;
            Some(settings_test_server::Request {
                method,
                target,
                body: if body.is_empty() {
                    Value::Null
                } else {
                    serde_json::from_slice(&body).ok()?
                },
            })
        }

        fn hold(&self, method: &'static str, path: &'static str) -> Arc<ResponseGate> {
            let gate = ResponseGate::new();
            self.gates.lock().expect("gates").push(HeldResponse {
                method,
                path,
                gate: gate.clone(),
                used: false,
            });
            gate
        }

        fn saved(&self) -> Value {
            self.document.lock().expect("document").clone()
        }

        fn open(&self, cx: &mut TestAppContext) -> (AnyWindowHandle, Entity<Workspace>) {
            use crate::connection::{Connection, ServerConfig};
            use openchamber_native_transport::client::ServerClient;
            use openchamber_native_transport::credential::Credential;

            let (window, workspace) = crate::workspace_tests::open(cx);
            workspace.update(cx, |workspace, _| {
                let mut connection =
                    Connection::new(ServerConfig::for_endpoint(self.endpoint.clone()))
                        .expect("connection");
                connection.client = Some(
                    ServerClient::new(self.endpoint.clone(), Credential::None).expect("client"),
                );
                workspace.runtime = RuntimeId::from(self.endpoint.as_str());
                workspace.connection = Some(connection);
            });
            refresh_topology(window, &workspace, cx);
            settings_test_server::settle(window, cx);
            (window, workspace)
        }
    }

    impl Drop for LayoutServer {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Release);
            for held in self.gates.lock().expect("gates").iter() {
                held.gate.release();
            }
            let address = self
                .endpoint
                .as_str()
                .trim_start_matches("http://")
                .trim_end_matches('/');
            let _ = TcpStream::connect(address);
            if let Some(worker) = self.worker.take() {
                worker.join().expect("layout server shutdown");
            }
        }
    }

    fn refresh_topology(
        window: AnyWindowHandle,
        workspace: &Entity<Workspace>,
        cx: &mut TestAppContext,
    ) {
        cx.update_window(window, |_, window, cx| {
            workspace.update(cx, |workspace, cx| workspace.load_topology(window, cx))
        })
        .expect("topology request");
    }

    fn panel_layout(workspace: &Entity<Workspace>, cx: &TestAppContext) -> Layout {
        cx.read(|cx| Layout::for_workspace(workspace.read(cx), cx))
    }

    fn general_owner(
        workspace: &Entity<Workspace>,
        cx: &TestAppContext,
    ) -> Entity<GeneralSettings> {
        cx.read(|cx| {
            workspace
                .read(cx)
                .settings
                .as_ref()
                .expect("retained Settings owner")
                .read(cx)
                .general
                .clone()
        })
    }

    #[gpui_kit::test]
    fn superseded_first_general_read_cannot_authorize_lists_and_retry_recovers(
        cx: &mut TestAppContext,
    ) {
        for fail_newer in [false, true] {
            let server = LayoutServer::new(json!({
                "workStatusHiddenSections": [], "workStatusHiddenSectionsExplicit": true,
                "promptNavigatorEnabled": false, "unrelated": {"keep": 9},
            }));
            let (window, workspace) = server.open(cx);
            assert!(cx.read(|cx| workspace.read(cx).settings.is_none()));
            let older = server.hold("GET", "/api/config/settings");
            click(window, "work-status-sections", cx);
            older.entered(cx);
            let general = general_owner(&workspace, cx);
            server.document.lock().expect("unread extension state")["workStatusHiddenSections"] =
                json!({"unread": ["ext:paused"]});
            server
                .fail_next_settings_read
                .store(fail_newer, Ordering::Release);
            let newer = server.hold("GET", "/api/config/settings");
            refresh_topology(window, &workspace, cx);
            newer.entered(cx);
            older.release();
            settings_test_server::settle(window, cx);
            assert_eq!(
                cx.read(|cx| general.read(cx).current_choice(PROMPT_NAVIGATOR)),
                Choice::Boolean(false),
                "unrelated fields of the completed General document remain available"
            );
            click(window, "work-status-visible-session", cx);
            let runtime = cx.read(|cx| workspace.read(cx).runtime.clone());
            general.update(cx, |general, cx| {
                general.set_work_status_hidden(&runtime, &[], cx);
                general.set_work_status_order(
                    &runtime,
                    &["mcp".to_owned(), "session".to_owned()],
                    cx,
                );
            });
            settings_test_server::settle(window, cx);
            assert!(
                server
                    .requests
                    .lock()
                    .expect("requests")
                    .iter()
                    .all(|request| request.method != "PUT"),
                "a superseded first General response grants neither checkbox nor hidden/order API authority"
            );
            assert_eq!(
                server.saved()["workStatusHiddenSections"],
                json!({"unread": ["ext:paused"]})
            );
            newer.release();
            settings_test_server::settle(window, cx);
            assert_eq!(
                cx.read(|cx| general.read(cx).status(false)),
                (
                    Some("settings.openchamber.desktopNetwork.error.loadFailed"),
                    true
                ),
                "malformed or failed superseding reads leave the existing load error and Retry usable"
            );
            server.document.lock().expect("corrected extension list")["workStatusHiddenSections"] =
                json!(["ext:paused"]);
            general.update(cx, |general, cx| general.retry(cx));
            settings_test_server::settle(window, cx);
            click(window, "work-status-visible-session", cx);
            assert_eq!(
                server.saved()["workStatusHiddenSections"],
                json!(["ext:paused", "session"]),
                "Retry establishes a real baseline before re-enabling edits"
            );
            assert_eq!(server.saved()["unrelated"], json!({"keep": 9}));
        }
    }

    #[gpui_kit::test]
    fn ownerless_deletion_snapshot_cannot_replace_newer_topology_or_retained_failure(
        cx: &mut TestAppContext,
    ) {
        for (malformed, reset_record) in [(false, false), (true, false), (false, true)] {
            let server = LayoutServer::new(json!({
                "workStatusSectionOrder": ["session", "repository"],
                "workStatusHiddenSections": ["ext:prior"], "workStatusHiddenSectionsExplicit": true,
            }));
            let (window, workspace) = server.open(cx);
            assert!(cx.read(|cx| workspace.read(cx).settings.is_none()));
            let older = server.hold("PUT", "/api/config/settings");
            cx.update_window(window, |_, window, cx| {
                workspace.update(cx, |workspace, cx| {
                    workspace.toggle_deletion_dialog(window, cx)
                })
            })
            .expect("ownerless merged PUT");
            older.entered(cx);
            {
                let mut document = server.document.lock().expect("new topology snapshot");
                document["workStatusHiddenSections"] = if malformed {
                    json!({"unread": ["ext:new"]})
                } else {
                    json!(["ext:new"])
                };
                document["workStatusSectionOrder"] = json!(["mcp", "session"]);
            }
            if reset_record {
                workspace.update(cx, |workspace, _| workspace.reset_send_catalog());
            }
            refresh_topology(window, &workspace, cx);
            settings_test_server::settle(window, cx);
            if reset_record {
                // A second read makes the new record's ticket numerically
                // equal to the old PUT's ticket; record identity must differ.
                refresh_topology(window, &workspace, cx);
                settings_test_server::settle(window, cx);
            }
            let newest = panel_layout(&workspace, cx);
            assert_eq!(newest.order[0], "mcp");
            assert_eq!(
                newest.hidden,
                if malformed {
                    vec!["ext:prior"]
                } else {
                    vec!["ext:new"]
                }
            );
            older.release();
            settings_test_server::settle(window, cx);
            assert_eq!(
                panel_layout(&workspace, cx),
                newest,
                "ownerless snapshots share read-start ordering and settings-record identity"
            );
            cx.read(|cx| {
                let workspace = workspace.read(cx);
                assert!(
                    workspace.settings.is_none(),
                    "ordering does not eagerly instantiate Settings"
                );
                let held = workspace
                    .send_catalog
                    .settings
                    .as_ref()
                    .expect("held document");
                if malformed {
                    assert!(
                        matches!(
                            held.work_status_hidden_sections,
                            Some(WorkStatusSectionList::MalformedWithPrior(_))
                        ),
                        "an older valid PUT cannot clear the newer malformed-with-prior provenance"
                    );
                    assert!(
                        held.work_status_hidden_sections
                            .as_ref()
                            .expect("field")
                            .entries()
                            .is_none()
                    );
                } else {
                    assert_eq!(
                        held.work_status_hidden_sections,
                        Some(WorkStatusSectionList::Valid(vec!["ext:new".to_owned()]))
                    );
                }
            });
        }
    }

    #[gpui_kit::test]
    fn topology_and_deletion_snapshot_refresh_settled_owner_before_checkbox_write(
        cx: &mut TestAppContext,
    ) {
        let server = LayoutServer::new(
            json!({"workStatusHiddenSections": [], "workStatusHiddenSectionsExplicit": true, "terminalShell": "fish", "unrelated": 7}),
        );
        let (window, workspace) = server.open(cx);
        click(window, "work-status-sections", cx);
        server.document.lock().expect("external client")["workStatusHiddenSections"] =
            json!(["ext:paused"]);
        refresh_topology(window, &workspace, cx);
        settings_test_server::settle(window, cx);
        assert_eq!(panel_layout(&workspace, cx).hidden, ["ext:paused"]);
        click(window, "work-status-visible-session", cx);
        assert_eq!(
            server.saved()["workStatusHiddenSections"],
            json!(["ext:paused", "session"]),
            "a settled owner adopts the external topology result before deriving a checkbox edit"
        );

        {
            let mut document = server.document.lock().expect("external client");
            document["workStatusHiddenSections"] = json!(["ext:put"]);
            document["workStatusSectionOrder"] = json!(["mcp", "session"]);
        }
        cx.update_window(window, |_, window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.toggle_deletion_dialog(window, cx)
            })
        })
        .expect("actual deletion preference save");
        settings_test_server::settle(window, cx);
        assert_eq!(panel_layout(&workspace, cx).order[0], "mcp");
        assert_eq!(panel_layout(&workspace, cx).hidden, ["ext:put"]);
        click(window, "work-status-visible-session", cx);
        assert_eq!(
            server.saved()["workStatusHiddenSections"],
            json!(["ext:put", "session"]),
            "the merged PUT response also refreshes settled layout fields"
        );
        assert_eq!(server.saved()["terminalShell"], json!("fish"));
        assert_eq!(server.saved()["unrelated"], json!(7));
    }

    #[gpui_kit::test]
    fn topology_without_settings_retains_valid_layout_and_malformed_provenance(
        cx: &mut TestAppContext,
    ) {
        let server = LayoutServer::new(json!({
            "workStatusSectionOrder": ["mcp", "ext:paused", "session"],
            "workStatusHiddenSections": ["session", "telemetry", "ext:paused"],
            "workStatusHiddenSectionsExplicit": true,
        }));
        let (window, workspace) = server.open(cx);
        assert!(
            cx.read(|cx| workspace.read(cx).settings.is_none()),
            "topology does not eagerly create Settings"
        );
        let prior = panel_layout(&workspace, cx);
        *server.document.lock().expect("malformed backend document") = json!({
            "workStatusSectionOrder": null, "workStatusHiddenSections": {"unread": ["ext:paused"]},
            "terminalShell": "fish", "promptNavigatorEnabled": false,
        });
        refresh_topology(window, &workspace, cx);
        settings_test_server::settle(window, cx);
        assert_eq!(
            panel_layout(&workspace, cx),
            prior,
            "the real card keeps its validated order and hidden fields before Settings exists"
        );
        cx.read(|cx| {
            let workspace = workspace.read(cx);
            assert!(workspace.settings.is_none());
            let held = workspace
                .send_catalog
                .settings
                .as_ref()
                .expect("successful document");
            assert!(matches!(
                held.work_status_hidden_sections,
                Some(WorkStatusSectionList::MalformedWithPrior(_))
            ));
            assert!(
                held.work_status_hidden_sections
                    .as_ref()
                    .expect("field")
                    .entries()
                    .is_none(),
                "a display fallback grants no new list authority"
            );
            assert_eq!(
                held.work_status_hidden_sections_explicit, None,
                "a missing marker remains missing"
            );
            assert_eq!(held.terminal_shell.as_deref(), Some("fish"));
            assert_eq!(held.prompt_navigator_enabled, Some(false));
        });
        click(window, "work-status-sections", cx);
        let general = general_owner(&workspace, cx);
        assert_eq!(
            cx.read(|cx| general.read(cx).status(false).0),
            Some("settings.openchamber.desktopNetwork.error.loadFailed")
        );
        click(window, "work-status-visible-session", cx);
        assert!(
            server
                .requests
                .lock()
                .expect("requests")
                .iter()
                .all(|request| request.method != "PUT"),
            "a fresh owner cannot convert retained malformed presentation into a valid empty mutation baseline"
        );
        assert_eq!(panel_layout(&workspace, cx), prior);

        server.document.lock().expect("corrected independent order")["workStatusSectionOrder"] =
            json!(["repository", "session"]);
        refresh_topology(window, &workspace, cx);
        settings_test_server::settle(window, cx);
        assert_eq!(panel_layout(&workspace, cx).order[0], "repository");
        assert_eq!(panel_layout(&workspace, cx).hidden, prior.hidden);
        assert_eq!(
            cx.read(|cx| general.read(cx).status(false).0),
            Some("settings.openchamber.desktopNetwork.error.loadFailed"),
            "adopting order does not erase hidden-field failure"
        );
        {
            let mut document = server.document.lock().expect("corrected hidden list");
            document["workStatusHiddenSections"] = json!(["ext:paused"]);
            document["workStatusHiddenSectionsExplicit"] = json!(true);
        }
        general.update(cx, |general, cx| general.retry(cx));
        settings_test_server::settle(window, cx);
        click(window, "work-status-visible-session", cx);
        assert_eq!(
            server.saved()["workStatusHiddenSections"],
            json!(["ext:paused", "session"])
        );
        assert_eq!(server.saved()["terminalShell"], json!("fish"));
    }

    #[gpui_kit::test]
    fn topology_and_general_reads_share_start_order_without_stale_data_or_failure_flags(
        cx: &mut TestAppContext,
    ) {
        for general_first in [false, true] {
            for stale in [
                None,
                Some(json!([])),
                Some(json!(null)),
                Some(json!({"unread": true})),
            ] {
                let server = LayoutServer::new(
                    json!({"workStatusHiddenSections": [], "workStatusHiddenSectionsExplicit": true}),
                );
                let (window, workspace) = server.open(cx);
                click(window, "work-status-sections", cx);
                let general = general_owner(&workspace, cx);
                {
                    let mut document = server.document.lock().expect("older snapshot");
                    document["promptNavigatorEnabled"] = json!(false);
                    if let Some(stale) = stale {
                        document["workStatusHiddenSections"] = stale;
                    } else {
                        document
                            .as_object_mut()
                            .expect("document")
                            .remove("workStatusHiddenSections");
                    }
                }
                let held = server.hold("GET", "/api/config/settings");
                if general_first {
                    general.update(cx, |general, cx| general.refresh(cx));
                } else {
                    refresh_topology(window, &workspace, cx);
                }
                held.entered(cx);
                server.document.lock().expect("newer external snapshot")["workStatusHiddenSections"] =
                    json!(["ext:new"]);
                if general_first {
                    refresh_topology(window, &workspace, cx);
                } else {
                    general.update(cx, |general, cx| general.refresh(cx));
                }
                settings_test_server::settle(window, cx);
                assert_eq!(panel_layout(&workspace, cx).hidden, ["ext:new"]);
                held.release();
                settings_test_server::settle(window, cx);
                assert_eq!(
                    panel_layout(&workspace, cx).hidden,
                    ["ext:new"],
                    "an older response cannot replace the newer read from the other owning stream"
                );
                assert_eq!(
                    cx.read(|cx| general.read(cx).status(false).0),
                    None,
                    "stale missing or malformed fields cannot re-mark the newer valid layout failed"
                );
                assert_eq!(
                    cx.read(|cx| general.read(cx).current_choice(PROMPT_NAVIGATOR)),
                    Choice::Boolean(false),
                    "layout sequencing does not discard other General fields"
                );
                click(window, "work-status-visible-session", cx);
                assert_eq!(
                    server.saved()["workStatusHiddenSections"],
                    json!(["ext:new", "session"])
                );
            }
        }
    }

    #[gpui_kit::test]
    fn newest_partial_document_keeps_its_failure_until_a_valid_empty_list_arrives(
        cx: &mut TestAppContext,
    ) {
        let server = LayoutServer::new(
            json!({"workStatusHiddenSections": ["ext:prior"], "workStatusHiddenSectionsExplicit": true}),
        );
        let (window, workspace) = server.open(cx);
        click(window, "work-status-sections", cx);
        let general = general_owner(&workspace, cx);
        server.document.lock().expect("older valid list")["workStatusHiddenSections"] = json!([]);
        let held = server.hold("GET", "/api/config/settings");
        general.update(cx, |general, cx| general.refresh(cx));
        held.entered(cx);
        {
            let mut document = server.document.lock().expect("new partial document");
            document["workStatusHiddenSections"] = json!({"unread": ["ext:prior"]});
            document["workStatusSectionOrder"] = json!(["repository", "session"]);
        }
        refresh_topology(window, &workspace, cx);
        settings_test_server::settle(window, cx);
        held.release();
        settings_test_server::settle(window, cx);
        assert_eq!(panel_layout(&workspace, cx).hidden, ["ext:prior"]);
        assert_eq!(panel_layout(&workspace, cx).order[0], "repository");
        assert_eq!(
            cx.read(|cx| general.read(cx).status(false).0),
            Some("settings.openchamber.desktopNetwork.error.loadFailed"),
            "an older valid response cannot erase the newer malformed field's failure"
        );
        server.document.lock().expect("valid empty list")["workStatusHiddenSections"] = json!([]);
        refresh_topology(window, &workspace, cx);
        settings_test_server::settle(window, cx);
        assert!(
            panel_layout(&workspace, cx).hidden.is_empty(),
            "a new successful empty array remains authoritative"
        );
        assert_eq!(cx.read(|cx| general.read(cx).status(false).0), None);
    }

    #[gpui_kit::test]
    fn deletion_snapshot_older_than_general_read_cannot_replace_new_layout(
        cx: &mut TestAppContext,
    ) {
        let server = LayoutServer::new(
            json!({"workStatusHiddenSections": [], "workStatusHiddenSectionsExplicit": true}),
        );
        let (window, workspace) = server.open(cx);
        click(window, "work-status-sections", cx);
        let general = general_owner(&workspace, cx);
        let held = server.hold("PUT", "/api/config/settings");
        cx.update_window(window, |_, window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.toggle_deletion_dialog(window, cx)
            })
        })
        .expect("actual merged PUT");
        held.entered(cx);
        server.document.lock().expect("external update")["workStatusHiddenSections"] =
            json!(["ext:new"]);
        general.update(cx, |general, cx| general.refresh(cx));
        settings_test_server::settle(window, cx);
        held.release();
        settings_test_server::settle(window, cx);
        assert_eq!(panel_layout(&workspace, cx).hidden, ["ext:new"]);
        click(window, "work-status-visible-session", cx);
        assert_eq!(
            server.saved()["workStatusHiddenSections"],
            json!(["ext:new", "session"])
        );
    }

    #[gpui_kit::test]
    fn topology_read_before_or_during_local_write_preserves_accepted_revision(
        cx: &mut TestAppContext,
    ) {
        for write_first in [false, true] {
            let server = LayoutServer::new(
                json!({"workStatusHiddenSections": ["ext:base"], "workStatusHiddenSectionsExplicit": true}),
            );
            let (window, workspace) = server.open(cx);
            click(window, "work-status-sections", cx);
            let read = server.hold("GET", "/api/config/settings");
            let write = write_first.then(|| server.hold("PUT", "/api/config/settings"));
            if write_first {
                click(window, "work-status-visible-session", cx);
                write.as_ref().expect("held write").entered(cx);
                server.document.lock().expect("stale backend read")["workStatusHiddenSections"] =
                    json!(["ext:base"]);
            }
            refresh_topology(window, &workspace, cx);
            read.entered(cx);
            if !write_first {
                click(window, "work-status-visible-session", cx);
            }
            if let Some(write) = write {
                write.release();
            }
            settings_test_server::settle(window, cx);
            read.release();
            settings_test_server::settle(window, cx);
            assert_eq!(
                panel_layout(&workspace, cx).hidden,
                ["ext:base", "session"],
                "local revisions and unsettled-at-start choices survive a topology answer"
            );
            click(window, "work-status-visible-mcp", cx);
            assert_eq!(
                server.saved()["workStatusHiddenSections"],
                json!(["ext:base", "session", "mcp"])
            );
        }
    }

    #[gpui_kit::test]
    fn failed_local_layout_keeps_precedence_over_successful_topology_read(cx: &mut TestAppContext) {
        let server = LayoutServer::new(
            json!({"workStatusHiddenSections": ["ext:base"], "workStatusHiddenSectionsExplicit": true}),
        );
        let (window, workspace) = server.open(cx);
        click(window, "work-status-sections", cx);
        server.fail_writes.store(true, Ordering::Release);
        click(window, "work-status-visible-session", cx);
        let general = general_owner(&workspace, cx);
        refresh_topology(window, &workspace, cx);
        settings_test_server::settle(window, cx);
        assert_eq!(panel_layout(&workspace, cx).hidden, ["ext:base", "session"]);
        assert_eq!(
            cx.read(|cx| general.read(cx).status(false).0),
            Some("settings.common.status.saveFailed")
        );
        server.fail_writes.store(false, Ordering::Release);
        general.update(cx, |general, cx| general.retry(cx));
        settings_test_server::settle(window, cx);
        assert_eq!(
            server.saved()["workStatusHiddenSections"],
            json!(["ext:base", "session"])
        );
    }

    #[gpui_kit::test]
    fn owner_created_after_topology_read_start_keeps_its_fresh_choices(cx: &mut TestAppContext) {
        let server = LayoutServer::new(
            json!({"workStatusHiddenSections": [], "workStatusHiddenSectionsExplicit": true}),
        );
        let (window, workspace) = server.open(cx);
        assert!(cx.read(|cx| workspace.read(cx).settings.is_none()));
        let held = server.hold("GET", "/api/config/settings");
        refresh_topology(window, &workspace, cx);
        held.entered(cx);
        server.document.lock().expect("external update")["workStatusHiddenSections"] =
            json!(["ext:fresh-owner"]);
        click(window, "work-status-sections", cx);
        assert_eq!(panel_layout(&workspace, cx).hidden, ["ext:fresh-owner"]);
        held.release();
        settings_test_server::settle(window, cx);
        assert_eq!(panel_layout(&workspace, cx).hidden, ["ext:fresh-owner"]);
        click(window, "work-status-visible-session", cx);
        assert_eq!(
            server.saved()["workStatusHiddenSections"],
            json!(["ext:fresh-owner", "session"])
        );
    }

    #[gpui_kit::test]
    fn captured_deletion_snapshot_cannot_adopt_after_runtime_or_generation_change(
        cx: &mut TestAppContext,
    ) {
        for change_runtime in [false, true] {
            let server = LayoutServer::new(
                json!({"workStatusHiddenSections": [], "workStatusHiddenSectionsExplicit": true}),
            );
            let (window, workspace) = server.open(cx);
            click(window, "work-status-sections", cx);
            let general = general_owner(&workspace, cx);
            let held = server.hold("PUT", "/api/config/settings");
            cx.update_window(window, |_, window, cx| {
                workspace.update(cx, |workspace, cx| {
                    workspace.toggle_deletion_dialog(window, cx)
                })
            })
            .expect("captured deletion snapshot");
            held.entered(cx);
            server.document.lock().expect("new scope document")["workStatusHiddenSections"] =
                json!(["ext:new-scope"]);
            workspace.update(cx, |workspace, cx| {
                workspace
                    .connection
                    .as_mut()
                    .expect("connection")
                    .generation += 1;
                if change_runtime {
                    workspace.runtime = RuntimeId::from("new-layout-profile");
                }
                cx.notify();
            });
            general.update(cx, |general, cx| general.refresh(cx));
            settings_test_server::settle(window, cx);
            held.release();
            settings_test_server::settle(window, cx);
            assert_eq!(
                panel_layout(&workspace, cx).hidden,
                ["ext:new-scope"],
                "an old runtime/generation snapshot has no publication or owner-adoption authority"
            );
            click(window, "work-status-visible-session", cx);
            assert_eq!(
                server.saved()["workStatusHiddenSections"],
                json!(["ext:new-scope", "session"])
            );
        }
    }

    #[gpui_kit::test]
    fn first_malformed_topology_list_has_no_retained_baseline_or_mutation_authority(
        cx: &mut TestAppContext,
    ) {
        let server = LayoutServer::new(
            json!({"workStatusHiddenSections": null, "workStatusHiddenSectionsExplicit": true}),
        );
        let (window, workspace) = server.open(cx);
        cx.read(|cx| {
            let held = workspace
                .read(cx)
                .send_catalog
                .settings
                .as_ref()
                .expect("document");
            assert_eq!(
                held.work_status_hidden_sections,
                Some(WorkStatusSectionList::Malformed)
            );
            assert!(
                held.work_status_hidden_sections
                    .as_ref()
                    .expect("field")
                    .last_valid_entries()
                    .is_none()
            );
        });
        click(window, "work-status-sections", cx);
        click(window, "work-status-visible-session", cx);
        assert_eq!(server.saved()["workStatusHiddenSections"], Value::Null);
        assert!(
            server
                .requests
                .lock()
                .expect("requests")
                .iter()
                .all(|request| request.method != "PUT")
        );
    }

    #[gpui_kit::test]
    fn roots_only_topology_retry_does_not_invalidate_real_general_layout_read(
        cx: &mut TestAppContext,
    ) {
        let server = LayoutServer::new(
            json!({"workStatusHiddenSections": [], "workStatusHiddenSectionsExplicit": true}),
        );
        let (window, workspace) = server.open(cx);
        click(window, "work-status-sections", cx);
        let general = general_owner(&workspace, cx);
        server.fail_home_once.store(true, Ordering::Release);
        refresh_topology(window, &workspace, cx);
        settings_test_server::settle(window, cx);
        let home_reads = server
            .requests
            .lock()
            .expect("requests")
            .iter()
            .filter(|request| request.is("GET", "/api/fs/home"))
            .count();
        let settings_reads = server
            .requests
            .lock()
            .expect("requests")
            .iter()
            .filter(|request| request.is("GET", "/api/config/settings"))
            .count();
        server.document.lock().expect("new layout")["workStatusHiddenSections"] =
            json!(["ext:after-roots"]);
        let held = server.hold("GET", "/api/config/settings");
        general.update(cx, |general, cx| general.refresh(cx));
        held.entered(cx);
        cx.executor().advance_clock(Duration::from_secs(2));
        settings_test_server::settle(window, cx);
        assert_eq!(
            server
                .requests
                .lock()
                .expect("requests")
                .iter()
                .filter(|request| request.is("GET", "/api/fs/home"))
                .count(),
            home_reads + 1,
            "the roots retry actually runs while the General response is held"
        );
        assert_eq!(
            server
                .requests
                .lock()
                .expect("requests")
                .iter()
                .filter(|request| request.is("GET", "/api/config/settings"))
                .count(),
            settings_reads + 1,
            "the retry reads only the failed roots component"
        );
        held.release();
        settings_test_server::settle(window, cx);
        assert_eq!(
            panel_layout(&workspace, cx).hidden,
            ["ext:after-roots"],
            "an attempt with need_settings=false cannot supersede the real layout read"
        );
    }

    #[gpui_kit::test]
    fn deletion_toggle_without_a_client_cannot_start_a_layout_read(cx: &mut TestAppContext) {
        let server = LayoutServer::new(
            json!({"workStatusHiddenSections": [], "workStatusHiddenSectionsExplicit": true}),
        );
        let (window, workspace) = server.open(cx);
        click(window, "work-status-sections", cx);
        let general = general_owner(&workspace, cx);
        server.document.lock().expect("new layout")["workStatusHiddenSections"] =
            json!(["ext:held-read"]);
        let held = server.hold("GET", "/api/config/settings");
        general.update(cx, |general, cx| general.refresh(cx));
        held.entered(cx);
        cx.update_window(window, |_, window, cx| {
            workspace.update(cx, |workspace, cx| {
                let client = workspace
                    .connection
                    .as_mut()
                    .expect("connection")
                    .client
                    .take();
                workspace.toggle_deletion_dialog(window, cx);
                workspace.connection.as_mut().expect("connection").client = client;
            })
        })
        .expect("no-client toggle attempt");
        held.release();
        settings_test_server::settle(window, cx);
        assert_eq!(panel_layout(&workspace, cx).hidden, ["ext:held-read"]);
        assert!(
            server
                .requests
                .lock()
                .expect("requests")
                .iter()
                .all(|request| request.method != "PUT"),
            "no request was dispatched and no read ticket was advanced"
        );
    }

    fn show(
        profile: &Profile,
        cx: &mut TestAppContext,
    ) -> (AnyWindowHandle, Entity<Workspace>, Entity<SectionsDialog>) {
        let (window, workspace) = settings_test_server::open(&profile.server, Page::General, cx);
        let dialog = reopen(window, &workspace, cx);
        (window, workspace, dialog)
    }

    fn reopen(
        window: AnyWindowHandle,
        workspace: &Entity<Workspace>,
        cx: &mut TestAppContext,
    ) -> Entity<SectionsDialog> {
        let dialog = cx
            .update_window(window, |_, window, cx| {
                window.close_dialog(cx);
                workspace.update(cx, |workspace, cx| open(workspace, window, cx))
            })
            .expect("test window");
        settings_test_server::settle(window, cx);
        dialog
    }

    fn click(window: AnyWindowHandle, id: &'static str, cx: &mut TestAppContext) {
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            window.click(id, cx);
        })
        .expect("test window");
        settings_test_server::settle(window, cx);
    }

    fn layout(dialog: &Entity<SectionsDialog>, cx: &TestAppContext) -> Layout {
        cx.read(|cx| dialog.read(cx).current(cx).expect("current owner").1)
    }

    #[gpui_kit::test]
    fn manager_visibility_order_cancel_show_all_and_reopen_use_real_profile_writes(
        cx: &mut TestAppContext,
    ) {
        let profile = Profile::new(
            json!({
                "workStatusSectionOrder": ["ext:paused", "telemetry", "session"],
                "workStatusHiddenSections": ["telemetry", "mcp", "ext:paused"],
                "workStatusHiddenSectionsExplicit": false,
                "terminalShell": "fish", "unrelated": {"keep": true},
            }),
            None,
        );
        let (window, workspace, dialog) = show(&profile, cx);
        let initial = layout(&dialog, cx);
        assert_eq!(
            initial.builtin_order().len(),
            9,
            "the manager retains every canonical built-in"
        );
        assert!(
            !initial.hidden.iter().any(|id| id == "telemetry"),
            "legacy implicit telemetry hiding is cleared"
        );
        assert!(
            initial.hidden.iter().any(|id| id == "ext:paused"),
            "an unloaded extension's preference survives"
        );
        click(window, "work-status-visible-session", cx);
        let writes = profile.server.writes();
        assert_eq!(writes.len(), 1, "visibility and its marker dispatch once");
        assert_eq!(
            writes[0].body,
            json!({
                "workStatusHiddenSections": ["mcp", "ext:paused", "session"],
                "workStatusHiddenSectionsExplicit": true,
            })
        );

        cx.update_window(window, |_, window, cx| {
            window.click("work-status-reorder-contextSources", cx);
            window.press("space", cx);
            window.press("up", cx);
            window.press("escape", cx);
        })
        .expect("keyboard drag");
        settings_test_server::settle(window, cx);
        assert_eq!(
            profile.server.writes().len(),
            1,
            "canceling a drag sends nothing"
        );
        assert_eq!(
            layout(&dialog, cx).order,
            initial.order,
            "cancel restores the exact saved slots"
        );
        assert!(cx.read(|cx| dialog.read(cx).drag.is_none()));

        cx.update_window(window, |_, window, cx| {
            window.press("space", cx);
            for _ in 0..8 {
                window.press("up", cx);
            }
            window.press("space", cx);
        })
        .expect("committed keyboard drag");
        settings_test_server::settle(window, cx);
        let reordered = layout(&dialog, cx);
        assert_eq!(
            reordered.order.first().map(String::as_str),
            Some("contextSources")
        );
        assert_eq!(
            reordered.order.last().map(String::as_str),
            Some("ext:paused"),
            "unavailable extension slots remain at the saved tail after reorder"
        );
        assert_eq!(
            profile.server.writes().len(),
            2,
            "keyboard drop commits exactly once"
        );

        let reopened = reopen(window, &workspace, cx);
        assert_eq!(
            layout(&reopened, cx),
            reordered,
            "closing the dialog retains the preference owner"
        );
        let scope = cx.read(|cx| reopened.read(cx).current(cx).expect("owner").0);
        reopened.update(cx, |dialog, cx| {
            for (id, _) in BUILT_INS {
                dialog.set_visible(&scope, id, false, cx);
            }
        });
        settings_test_server::settle(window, cx);
        assert!(layout(&reopened, cx).all_hidden());
        click(window, "work-status-sections-show-all", cx);
        assert_eq!(
            profile.saved()["workStatusHiddenSections"],
            json!([]),
            "Show all also clears unavailable extension hidden ids"
        );
        assert!(layout(&reopened, cx).all_visible());
        assert_eq!(profile.saved()["terminalShell"], json!("fish"));
        assert_eq!(profile.saved()["unrelated"], json!({"keep": true}));

        let (_, _, fresh) = show(&profile, cx);
        assert_eq!(
            layout(&fresh, cx),
            layout(&reopened, cx),
            "a newly created workspace round-trips the server's saved layout"
        );
    }

    #[gpui_kit::test]
    fn real_pointer_reorder_uses_measured_handles_and_keeps_profile_owner_in_rtl(
        cx: &mut TestAppContext,
    ) {
        for language in ["en", "ar"] {
            let profile = Profile::new(
                json!({
                    "workStatusSectionOrder": ["session", "ext:paused", "repository"],
                    "unrelated": language,
                }),
                None,
            );
            let (window, _, dialog) = show(&profile, cx);
            cx.update_window(window, |_, window, cx| {
                cx.set_global(locale::Strings::load(language));
                window.render_frame(cx);
                window.drag_to(
                    "work-status-reorder-contextSources",
                    "work-status-reorder-session",
                    cx,
                );
            })
            .expect("measured semantic handle drag");
            settings_test_server::settle(window, cx);
            let saved = profile.saved();
            assert_eq!(saved["workStatusSectionOrder"][0], json!("contextSources"));
            assert_eq!(saved["workStatusSectionOrder"][9], json!("ext:paused"));
            assert_eq!(
                profile.server.writes().len(),
                1,
                "a real pointer drop writes once"
            );
            assert_eq!(saved["unrelated"], json!(language));
            assert_eq!(
                layout(&dialog, cx).order.first().map(String::as_str),
                Some("contextSources")
            );
            cx.update_window(window, |_, window, cx| {
                assert_eq!(
                    window.find("work-status-reorder-contextSources").focused(),
                    Some(true),
                    "the moved handle retains keyboard focus"
                );
                window.close_all_dialogs(cx);
            })
            .expect("retained drag focus");
        }
    }

    #[gpui_kit::test]
    fn failed_layout_read_and_write_keep_live_choices_until_existing_owner_retry(
        cx: &mut TestAppContext,
    ) {
        let profile = Profile::new(
            json!({"workStatusHiddenSections": [], "workStatusHiddenSectionsExplicit": true, "unrelated": 7}),
            None,
        );
        let (window, _, dialog) = show(&profile, cx);
        profile.fail_writes.store(true, Ordering::Release);
        let general = cx.read(|cx| dialog.read(cx).general.clone());
        general.update(cx, |general, cx| {
            general.change(PROMPT_NAVIGATOR, Choice::Boolean(false), cx)
        });
        settings_test_server::settle(window, cx);
        click(window, "work-status-visible-usage", cx);
        assert!(layout(&dialog, cx).hidden.iter().any(|id| id == "usage"));
        assert_eq!(profile.saved()["workStatusHiddenSections"], json!([]));
        assert_eq!(
            cx.read(|cx| general.read(cx).status(false).0),
            Some("settings.common.status.saveFailed")
        );

        profile.fail_reads.store(true, Ordering::Release);
        general.update(cx, |general, cx| general.refresh(cx));
        settings_test_server::settle(window, cx);
        assert!(
            layout(&dialog, cx).hidden.iter().any(|id| id == "usage"),
            "a failed read cannot clear a failed live edit"
        );
        profile.fail_writes.store(false, Ordering::Release);
        general.update(cx, |general, cx| {
            general.change(
                FOLLOW_UP,
                Choice::FollowUp(openchamber_native_core::message_queue::FollowUpBehavior::Steer),
                cx,
            )
        });
        settings_test_server::settle(window, cx);
        assert_eq!(
            profile.server.writes().last().expect("unrelated save").body,
            json!({"followUpBehavior": "steer"}),
            "unrelated edits do not retry failed layout or preference fields"
        );
        assert!(cx.read(|cx| general.read(cx).has_failed_writes()));
        profile.fail_reads.store(false, Ordering::Release);
        general.update(cx, |general, cx| general.retry(cx));
        settings_test_server::settle(window, cx);
        assert_eq!(
            profile.saved()["workStatusHiddenSections"],
            json!(["usage"])
        );
        assert_eq!(
            profile.saved()["workStatusHiddenSectionsExplicit"],
            json!(true)
        );
        assert_eq!(profile.saved()["promptNavigatorEnabled"], json!(false));
        assert_eq!(profile.saved()["unrelated"], json!(7));
        assert!(!cx.read(|cx| general.read(cx).has_failed_writes()));
    }

    #[gpui_kit::test]
    fn a_failed_first_read_cannot_authorize_a_default_empty_list_write(cx: &mut TestAppContext) {
        let profile = Profile::new(
            json!({
                "workStatusHiddenSections": ["ext:paused", "mcp"],
                "workStatusHiddenSectionsExplicit": true,
            }),
            None,
        );
        profile.fail_reads.store(true, Ordering::Release);
        let (window, _, dialog) = show(&profile, cx);
        let (scope, general) = cx.read(|cx| {
            let dialog = dialog.read(cx);
            (
                dialog.current(cx).expect("captured runtime").0,
                dialog.general.clone(),
            )
        });
        general.update(cx, |general, cx| {
            general.set_work_status_hidden(&scope.runtime, &[], cx)
        });
        settings_test_server::settle(window, cx);
        assert!(
            profile.server.writes().is_empty(),
            "the owning mutation API rejects an unread collection"
        );
        assert_eq!(
            profile.saved()["workStatusHiddenSections"],
            json!(["ext:paused", "mcp"])
        );
        assert!(!cx.read(|cx| {
            general
                .read(cx)
                .work_status_editable(&scope.runtime, scope.generation)
        }));
        profile.fail_reads.store(false, Ordering::Release);
        general.update(cx, |general, cx| general.retry(cx));
        settings_test_server::settle(window, cx);
        click(window, "work-status-visible-session", cx);
        assert_eq!(
            profile.saved()["workStatusHiddenSections"],
            json!(["ext:paused", "mcp", "session"]),
            "retry reads the retained extension ids before accepting a visibility edit"
        );
    }

    #[gpui_kit::test]
    fn malformed_hidden_list_is_not_an_empty_baseline_for_toggle_or_unrelated_order_write(
        cx: &mut TestAppContext,
    ) {
        let unread = json!({"unread": ["ext:paused", "mcp"]});
        let profile = Profile::new(
            json!({
                "workStatusHiddenSections": unread.clone(),
                "workStatusHiddenSectionsExplicit": true,
                "promptNavigatorEnabled": false,
                "unrelated": {"keep": 9},
            }),
            None,
        );
        let (window, _, dialog) = show(&profile, cx);
        let (scope, general) = cx.read(|cx| {
            let dialog = dialog.read(cx);
            (dialog.current(cx).expect("owner").0, dialog.general.clone())
        });
        assert_eq!(
            cx.read(|cx| general.read(cx).current_choice(PROMPT_NAVIGATOR)),
            Choice::Boolean(false),
            "a malformed layout list does not invalidate unrelated successful fields"
        );
        assert_eq!(
            cx.read(|cx| general.read(cx).status(false).0),
            Some("settings.openchamber.desktopNetwork.error.loadFailed")
        );
        click(window, "work-status-visible-session", cx);
        general.update(cx, |general, cx| {
            general.set_work_status_hidden(&scope.runtime, &[], cx)
        });
        settings_test_server::settle(window, cx);
        assert!(
            profile.server.writes().is_empty(),
            "both the real disabled checkbox and owning API reject an unread malformed list"
        );

        cx.update_window(window, |_, window, cx| {
            window.click("work-status-reorder-contextSources", cx);
            window.press("space", cx);
            window.press("up", cx);
            window.press("space", cx);
        })
        .expect("independently valid order control");
        settings_test_server::settle(window, cx);
        let writes = profile.server.writes();
        assert_eq!(writes.len(), 1);
        assert!(writes[0].body.get("workStatusSectionOrder").is_some());
        assert!(
            writes[0].body.get("workStatusHiddenSections").is_none(),
            "an order edit cannot replace the unknown hidden field"
        );
        assert_eq!(profile.saved()["workStatusHiddenSections"], unread);
        assert_eq!(profile.saved()["promptNavigatorEnabled"], json!(false));
        assert_eq!(profile.saved()["unrelated"], json!({"keep": 9}));

        profile.saved.lock().expect("server correction")["workStatusHiddenSections"] =
            json!(["ext:paused", "mcp"]);
        general.update(cx, |general, cx| general.retry(cx));
        settings_test_server::settle(window, cx);
        click(window, "work-status-visible-session", cx);
        assert_eq!(
            profile.saved()["workStatusHiddenSections"],
            json!(["ext:paused", "mcp", "session"]),
            "retry establishes the real collection before editing it"
        );
        let prior = layout(&dialog, cx);
        profile.saved.lock().expect("later malformed response")["workStatusHiddenSections"] =
            json!("malformed");
        general.update(cx, |general, cx| general.refresh(cx));
        settings_test_server::settle(window, cx);
        assert_eq!(
            layout(&dialog, cx),
            prior,
            "a later malformed field preserves the established layout"
        );
    }

    #[gpui_kit::test]
    fn actual_panel_triggers_reopen_and_restore_keyboard_focus_in_both_directions(
        cx: &mut TestAppContext,
    ) {
        for language in ["en", "ar"] {
            let profile = Profile::new(
                json!({
                    "workStatusHiddenSections": BUILT_INS.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
                    "workStatusHiddenSectionsExplicit": true,
                }),
                None,
            );
            let (window, _) = settings_test_server::open(&profile.server, Page::General, cx);
            cx.update_window(window, |_, window, cx| {
                cx.set_global(locale::Strings::load(language));
                window.close_all_dialogs(cx);
                window.render_frame(cx);
                window.click("work-status-sections-reopen", cx);
                window.render_frame(cx);
                assert!(
                    window.has_active_dialog(cx),
                    "the real all-hidden recovery opens the manager"
                );
                window.press("escape", cx);
                window.render_frame(cx);
                assert!(
                    !window.has_active_dialog(cx),
                    "Escape dismisses the idle manager"
                );
                window.press("space", cx);
                window.render_frame(cx);
                assert!(
                    window.has_active_dialog(cx),
                    "dismissal returns focus to the actual recovery trigger"
                );
                window.press("escape", cx);
                window.render_frame(cx);
                window.click("work-status-sections", cx);
                window.render_frame(cx);
                assert!(
                    window.has_active_dialog(cx),
                    "the existing equalizer opens the same manager"
                );
                window.press("escape", cx);
                window.render_frame(cx);
                window.press("space", cx);
                window.render_frame(cx);
                assert!(
                    window.has_active_dialog(cx),
                    "the equalizer also retains trigger focus"
                );
                window.close_all_dialogs(cx);
            })
            .expect("real trigger and keyboard dispatch");
            assert!(
                profile.server.writes().is_empty(),
                "opening and dismissing never changes the profile"
            );
        }
    }

    #[gpui_kit::test]
    fn captured_runtime_layout_writes_coalesce_and_drain_after_dialog_closes(
        cx: &mut TestAppContext,
    ) {
        let gate = ResponseGate::new();
        let first = Profile::new(json!({"unrelated": "first"}), Some(gate.clone()));
        let second = Profile::new(
            json!({"workStatusHiddenSections": [], "workStatusHiddenSectionsExplicit": true, "unrelated": "second"}),
            None,
        );
        let (window, workspace, dialog) = show(&first, cx);
        let original_scope = cx.read(|cx| dialog.read(cx).current(cx).expect("first owner").0);
        dialog.update(cx, |dialog, cx| {
            dialog.set_visible(&original_scope, "mcp", false, cx)
        });
        gate.entered(cx);
        dialog.update(cx, |dialog, cx| {
            dialog.set_visible(&original_scope, "telemetry", false, cx);
            dialog.set_visible(&original_scope, "contextSources", false, cx);
        });
        crate::settings_controls::tests::retarget(&workspace, &second.server, cx);
        settings_test_server::settle(window, cx);
        let second_scope = cx.read(|cx| dialog.read(cx).current(cx).expect("second owner").0);
        assert_ne!(original_scope.runtime, second_scope.runtime);
        dialog.update(cx, |dialog, cx| {
            dialog.set_visible(&original_scope, "session", false, cx);
            dialog.set_visible(&second_scope, "pinned", false, cx);
        });
        settings_test_server::settle(window, cx);
        let general = cx.read(|cx| dialog.read(cx).general.clone());
        let waiting = general.update(cx, |general, cx| {
            general
                .take_pending_write(cx)
                .expect("accepted captured writes still need a close drain")
        });
        cx.update_window(window, |_, window, cx| window.close_dialog(cx))
            .expect("close dialog");
        drop(dialog);
        gate.release();
        cx.executor().allow_parking();
        cx.foreground_executor().block_test(async move {
            waiting.await.expect("captured close drain");
        });
        assert_eq!(
            first.server.writes().len(),
            2,
            "the held write is followed by one coalesced layout"
        );
        assert_eq!(
            first.saved()["workStatusHiddenSections"],
            json!(["mcp", "telemetry", "contextSources"])
        );
        assert_eq!(
            second.saved()["workStatusHiddenSections"],
            json!(["pinned"]),
            "stale dialog actions cannot edit the new runtime"
        );
        assert_eq!(first.saved()["unrelated"], json!("first"));
        assert_eq!(second.saved()["unrelated"], json!("second"));
    }

    #[gpui_kit::test]
    fn quit_transfers_accepted_layout_writes_with_the_existing_executor_owner(
        cx: &mut TestAppContext,
    ) {
        let gate = ResponseGate::new();
        let profile = Profile::new(json!({"unrelated": "retained"}), Some(gate.clone()));
        let (window, _, dialog) = show(&profile, cx);
        let scope = cx.read(|cx| dialog.read(cx).current(cx).expect("layout owner").0);
        dialog.update(cx, |dialog, cx| {
            dialog.set_visible(&scope, "mcp", false, cx)
        });
        gate.entered(cx);
        dialog.update(cx, |dialog, cx| {
            dialog.set_visible(&scope, "telemetry", false, cx)
        });
        let general = cx.read(|cx| dialog.read(cx).general.clone());
        cx.update_window(window, |_, window, cx| window.close_dialog(cx))
            .expect("close dialog");
        drop(dialog);
        cx.update(|cx| {
            cx.on_app_quit(move |cx| {
                let pending = general.update(cx, |general, cx| general.take_quit_write(cx));
                gate.release();
                async move {
                    if let Some(pending) = pending {
                        pending.await;
                    }
                }
            })
            .detach();
        });
        cx.executor().allow_parking();
        cx.quit();
        assert_eq!(
            profile.server.writes().len(),
            2,
            "quit drains the accepted request and its newest queued layout"
        );
        assert_eq!(
            profile.saved()["workStatusHiddenSections"],
            json!(["mcp", "telemetry"])
        );
        assert_eq!(
            profile.saved()["workStatusHiddenSectionsExplicit"],
            json!(true)
        );
        assert_eq!(profile.saved()["unrelated"], json!("retained"));
    }
}
