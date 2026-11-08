//! Draws the Project knowledge panel: section tabs with their match counts
//! and the search toggle, the floating search, each section's composer and
//! list, and an open note or plan in place of its list. Every control
//! dispatches to `KnowledgePanel`.

use chrono::{Datelike, Local, TimeZone as _, Timelike};
use gpui_kit::component::WindowExt;
use gpui_kit::component::input::{Input, Textarea};
use gpui_kit::{
    AnyElement, Context, FontWeight, InteractiveElement, IntoElement, ParentElement, Render, Role,
    SharedString, StatefulInteractiveElement, Styled, TestSupportExt, Window, div,
    prelude::FluentBuilder, px, rems,
};
use openchamber_native_core::knowledge::{self, Note, NoteSource, Plan, PlanSource, Section, Todo};
use openchamber_native_core::message_time::{LocalDate, LocalDateTime};

use crate::activation::on_activate;
use crate::chrome::{IconButton, glyph};
use crate::knowledge::{KnowledgePanel, PlanLoad};
use crate::locale::{self, Direction, Strings};
use crate::logical::LogicalStyledExt;
use crate::panel_controls::{
    LABEL_LINE, LABEL_SIZE, META_LINE, META_SIZE, MICRO_LINE, MICRO_SIZE, Variant, button,
    option_row, popup, text,
};
use crate::scheduled_tasks_view::ClockSetting;
use crate::theme::Palette;

const META: (f32, f32) = (META_SIZE, META_LINE);
const MICRO: (f32, f32) = (MICRO_SIZE, MICRO_LINE);

/// `formatKnowledgeDate`: the time for today, a short date otherwise, the
/// year only when it differs.
fn knowledge_date(at_ms: i64, strings: &Strings, cx: &gpui_kit::App) -> String {
    let Some(moment) = Local.timestamp_millis_opt(at_ms).single() else {
        return String::new();
    };
    let today = Local::now().date_naive();
    let date = moment.date_naive();
    if date == today {
        let clock = cx
            .try_global::<ClockSetting>()
            .map(|setting| setting.0)
            .unwrap_or_default();
        return strings.dates().time(
            &LocalDateTime {
                date: LocalDate {
                    year: date.year(),
                    month: date.month(),
                    day: date.day(),
                },
                hour: moment.hour(),
                minute: moment.minute(),
            },
            clock,
        );
    }
    let month = strings
        .dates()
        .months
        .get(date.month0() as usize)
        .cloned()
        .unwrap_or_default();
    if date.year() == today.year() {
        format!("{month} {}", date.day())
    } else {
        format!("{month} {}, {}", date.day(), date.year())
    }
}

fn section_look(section: Section) -> (&'static str, &'static str) {
    match section {
        Section::Notes => ("sticky-note", "rightSidebar.contextNotesTodo.tabs.notes"),
        Section::Todos => (
            "checkbox-circle",
            "rightSidebar.contextNotesTodo.tabs.todos",
        ),
        Section::Plans => ("file-text", "rightSidebar.contextNotesTodo.tabs.plans"),
    }
}

impl KnowledgePanel {
    fn render_tabs(
        &self,
        strings: &Strings,
        palette: &Palette,
        direction: Direction,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let counts = self.counts();
        let tabs: Vec<AnyElement> = Section::ALL
            .into_iter()
            .map(|section| {
                let (icon, key) = section_look(section);
                let selected = self.section == section;
                let id = format!("knowledge-tab:{}", section.as_str());
                let count = counts.of(section);
                let hover = palette.hover;
                let color = if selected {
                    palette.foreground
                } else {
                    palette.muted_foreground
                };
                on_activate(
                    div()
                        .id(SharedString::from(id.clone()))
                        .role(Role::Tab)
                        .accessibility_id(id)
                        .aria_label(SharedString::from(format!("{} {count}", strings.get(key))))
                        .aria_selected(selected)
                        .h(px(26.0))
                        .px(px(8.0))
                        .rounded(px(6.0))
                        .flex_row_from(direction)
                        .items_center()
                        .gap(px(6.0))
                        .cursor_pointer()
                        .when(selected, |this| this.bg(palette.selection))
                        .when(!selected, |this| this.hover(move |style| style.bg(hover)))
                        .child(glyph(icon, 14.0, color))
                        // The active tab keeps its label; the others show
                        // their icon and count.
                        .when(selected, |this| {
                            this.child(
                                text(strings.get(key), META, color, cx)
                                    .font_weight(FontWeight::MEDIUM),
                            )
                        })
                        .child(text(count.to_string(), MICRO, palette.muted_foreground, cx)),
                    cx,
                    move |this: &mut Self, _, cx| this.select_section(section, cx),
                )
                .test_support()
                .into_any_element()
            })
            .collect();
        let search_label = strings.format(
            "rightSidebar.contextNotesTodo.search.placeholderInProject",
            &[("project", &self.owner.label)],
        );
        div()
            .flex_shrink_0()
            .h(px(40.0))
            .px(px(8.0))
            .flex_row_from(direction)
            .items_center()
            .justify_between()
            .border_b_1()
            .border_color(palette.divider)
            .child(
                div()
                    .id("knowledge-sections")
                    .role(Role::TabList)
                    .accessibility_id("knowledge-sections")
                    .aria_label(strings.get("rightSidebar.contextNotesTodo.sections.label"))
                    .flex_row_from(direction)
                    .gap(px(2.0))
                    .children(tabs),
            )
            .child(
                IconButton::new("knowledge-search-toggle", "search", search_label)
                    .size(26.0, 16.0)
                    .tone(if self.search_open {
                        crate::chrome::ButtonTone::Active
                    } else {
                        crate::chrome::ButtonTone::Quiet
                    })
                    .render(palette, cx, |this: &mut Self, window, cx| {
                        this.toggle_search(window, cx)
                    }),
            )
            .into_any_element()
    }

    fn render_search(
        &self,
        strings: &Strings,
        palette: &Palette,
        direction: Direction,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let counts = self.counts();
        let nothing =
            !self.query.trim().is_empty() && counts.notes + counts.todos + counts.plans == 0;
        div()
            .px(px(8.0))
            .pt(px(8.0))
            .flex()
            .flex_col()
            .gap(px(4.0))
            .child(
                div()
                    .h(px(30.0))
                    .px(px(8.0))
                    .rounded(px(8.0))
                    .bg(palette.background)
                    .shadow(crate::elevation::control(palette))
                    .flex_row_from(direction)
                    .items_center()
                    .gap(px(6.0))
                    .child(glyph("search", 14.0, palette.muted_foreground))
                    .child(
                        div().flex_1().min_w_0().child(
                            Input::new(&self.search)
                                .appearance(false)
                                .bordered(false)
                                .text_size(rems(LABEL_SIZE))
                                .accessibility_id("knowledge-search")
                                .aria_label(strings.format(
                                    "rightSidebar.contextNotesTodo.search.placeholderInProject",
                                    &[("project", &self.owner.label)],
                                )),
                        ),
                    )
                    .child(
                        IconButton::new(
                            "knowledge-search-close",
                            "close",
                            strings.get("chat.search.close"),
                        )
                        .size(20.0, 14.0)
                        .render(
                            palette,
                            cx,
                            |this: &mut Self, window, cx| this.close_search(window, cx),
                        ),
                    ),
            )
            .when(nothing, |this| {
                this.child(
                    div()
                        .id("knowledge-search-empty")
                        .role(Role::Status)
                        .accessibility_id("knowledge-search-empty")
                        .aria_label(strings.get("chat.search.noResults"))
                        .test_support()
                        .child(text(
                            strings.get("chat.search.noResults"),
                            META,
                            palette.muted_foreground,
                            cx,
                        )),
                )
            })
            .into_any_element()
    }

    /// A composer: one field and its add button.
    #[allow(clippy::too_many_arguments)]
    fn composer(
        &self,
        id: &'static str,
        field: AnyElement,
        add: &'static str,
        add_label: SharedString,
        palette: &Palette,
        direction: Direction,
        cx: &mut Context<Self>,
        action: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    ) -> AnyElement {
        div()
            .id(id)
            .min_h(px(34.0))
            .px(px(8.0))
            .py(px(2.0))
            .rounded(px(8.0))
            .border_1()
            .border_color(palette.border)
            .flex_row_from(direction)
            .items_center()
            .gap(px(6.0))
            .child(div().flex_1().min_w_0().child(field))
            .child(button(
                add,
                Some("add"),
                None,
                add_label,
                Variant::Ghost,
                false,
                palette,
                direction,
                cx,
                action,
            ))
            .into_any_element()
    }

    fn empty_line(
        &self,
        key: &'static str,
        strings: &Strings,
        palette: &Palette,
        cx: &Context<Self>,
    ) -> AnyElement {
        let value = if self.query.trim().is_empty() {
            strings.get(key)
        } else {
            strings.format(
                "rightSidebar.contextNotesTodo.search.noResults",
                &[("query", self.query.trim())],
            )
        };
        div()
            .id("knowledge-empty")
            .role(Role::Status)
            .accessibility_id("knowledge-empty")
            .aria_label(value.clone())
            .test_support()
            .px(px(12.0))
            .py(px(12.0))
            .child(text(value, META, palette.muted_foreground, cx))
            .into_any_element()
    }

    /// The ⋯ menu of a row and its options.
    #[allow(clippy::too_many_arguments)]
    fn row_menu(
        &self,
        id: String,
        options: Vec<AnyElement>,
        strings: &Strings,
        palette: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let open = self.entry_menu.as_deref() == Some(id.as_str());
        let key = id.clone();
        div()
            .relative()
            .child(
                IconButton::new(
                    SharedString::from(format!("knowledge-more:{id}")),
                    "more-2",
                    strings.get("rightSidebar.contextNotesTodo.actions.more"),
                )
                .size(24.0, 14.0)
                .render(palette, cx, move |this: &mut Self, _, cx| {
                    this.toggle_entry_menu(&key, cx)
                }),
            )
            .when(open, |this| {
                this.child(div().absolute().top_0().right_0().child(popup(
                    "knowledge-row-menu",
                    options,
                    200.0,
                    palette,
                )))
            })
            .into_any_element()
    }

    fn render_note_card(
        &self,
        note: &Note,
        strings: &Strings,
        palette: &Palette,
        direction: Direction,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (title, rest) = knowledge::split_note_title(&note.body);
        let source = match note.source {
            NoteSource::Selection => {
                Some(strings.get("rightSidebar.contextNotesTodo.notes.source.selection"))
            }
            NoteSource::Agent => {
                Some(strings.get("rightSidebar.contextNotesTodo.notes.source.agent"))
            }
            NoteSource::Manual => None,
        };
        let id = note.id.clone();
        let delete_id = note.id.clone();
        let element_id = format!("knowledge-note:{}", note.id);
        let hover = palette.hover;
        let preview: String = rest.chars().take(280).collect();
        let label: SharedString = if title.is_empty() {
            strings.get("rightSidebar.contextNotesTodo.notes.actions.expand")
        } else {
            title.clone().into()
        };
        div()
            .relative()
            .child(
                on_activate(
                    div()
                        .id(SharedString::from(element_id.clone()))
                        .role(Role::Button)
                        .accessibility_id(element_id)
                        .aria_label(label)
                        .h(px(112.0))
                        .p(px(10.0))
                        .rounded(px(8.0))
                        .border_1()
                        .border_color(palette.border)
                        .overflow_hidden()
                        .flex()
                        .flex_col()
                        .gap(px(4.0))
                        .cursor_pointer()
                        .hover(move |style| style.bg(hover))
                        .child(
                            text(title, (LABEL_SIZE, LABEL_LINE), palette.foreground, cx)
                                .font_weight(FontWeight::MEDIUM)
                                .truncate()
                                .pr(px(24.0)),
                        )
                        .child(div().flex_1().min_h_0().overflow_hidden().child(text(
                            preview,
                            META,
                            palette.muted_foreground,
                            cx,
                        )))
                        .child(
                            div()
                                .flex_row_from(direction)
                                .gap(px(6.0))
                                .when_some(source, |this, source| {
                                    this.child(text(source, MICRO, palette.muted_foreground, cx))
                                })
                                .child(text(
                                    knowledge_date(
                                        note.updated_at.max(note.created_at),
                                        strings,
                                        cx,
                                    ),
                                    MICRO,
                                    palette.muted_foreground,
                                    cx,
                                )),
                        ),
                    cx,
                    move |this: &mut Self, window, cx| this.open_note(Some(id.clone()), window, cx),
                )
                .test_support(),
            )
            .child(
                div()
                    .absolute()
                    .top(px(6.0))
                    .right(px(6.0))
                    .child(self.row_menu(
                        note.id.clone(),
                        vec![option_row(
                            format!("knowledge-note-delete:{}", note.id),
                            strings.get("rightSidebar.contextNotesTodo.notes.actions.delete"),
                            false,
                            true,
                            palette,
                            direction,
                            cx,
                            move |this: &mut Self, window, cx| {
                                this.schedule_delete(Section::Notes, &delete_id, window, cx)
                            },
                        )],
                        strings,
                        palette,
                        cx,
                    )),
            )
            .into_any_element()
    }

    fn render_notes(
        &self,
        strings: &Strings,
        palette: &Palette,
        direction: Direction,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let mut rows = vec![
            self.composer(
                "knowledge-note-composer",
                Textarea::new(&self.note_input)
                    .appearance(false)
                    .bordered(false)
                    .text_size(rems(LABEL_SIZE))
                    .accessibility_id("knowledge-note-input")
                    .aria_label(strings.get("rightSidebar.contextNotesTodo.notes.placeholder"))
                    .into_any_element(),
                "knowledge-note-add",
                strings.get("rightSidebar.contextNotesTodo.notes.addAria"),
                palette,
                direction,
                cx,
                |this: &mut Self, window, cx| this.add_note(window, cx),
            ),
        ];
        let Some(knowledge) = &self.knowledge else {
            return rows;
        };
        let visible: Vec<&Note> = knowledge
            .notes
            .iter()
            .filter(|note| {
                !self.delete_pending(&note.id) && knowledge::note_matches(note, &self.query)
            })
            .collect();
        // With no notes at all the composer's placeholder says enough.
        if visible.is_empty() && !knowledge.notes.is_empty() {
            rows.push(self.empty_line(
                "rightSidebar.contextNotesTodo.notes.placeholder",
                strings,
                palette,
                cx,
            ));
        }
        for note in visible {
            rows.push(self.render_note_card(note, strings, palette, direction, cx));
        }
        rows
    }

    fn render_open_note(
        &self,
        id: &str,
        strings: &Strings,
        palette: &Palette,
        direction: Direction,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let note = self
            .knowledge
            .as_ref()
            .and_then(|knowledge| knowledge.notes.iter().find(|note| note.id == id))
            .cloned();
        let delete_id = id.to_owned();
        vec![
            div()
                .flex_row_from(direction)
                .items_center()
                .justify_between()
                .child(
                    div()
                        .flex_row_from(direction)
                        .items_center()
                        .gap(px(6.0))
                        .child(
                            IconButton::new(
                                "knowledge-back",
                                "arrow-left-s",
                                strings.get("rightSidebar.contextNotesTodo.notes.actions.back"),
                            )
                            .size(24.0, 16.0)
                            .render(
                                palette,
                                cx,
                                |this: &mut Self, _, cx| this.close_entry(cx),
                            ),
                        )
                        .when_some(note.as_ref(), |this, note| {
                            this.child(text(
                                knowledge_date(note.updated_at.max(note.created_at), strings, cx),
                                MICRO,
                                palette.muted_foreground,
                                cx,
                            ))
                        }),
                )
                .child(self.row_menu(
                    id.to_owned(),
                    vec![option_row(
                        "knowledge-open-note-delete",
                        strings.get("rightSidebar.contextNotesTodo.notes.actions.delete"),
                        false,
                        true,
                        palette,
                        direction,
                        cx,
                        move |this: &mut Self, window, cx| {
                            this.schedule_delete(Section::Notes, &delete_id, window, cx)
                        },
                    )],
                    strings,
                    palette,
                    cx,
                ))
                .into_any_element(),
            div()
                .id("knowledge-note-editor-frame")
                .role(Role::Group)
                .accessibility_id("knowledge-note-editor-frame")
                .child(
                    Textarea::new(&self.note_editor)
                        .appearance(false)
                        .bordered(false)
                        .text_size(rems(LABEL_SIZE))
                        .accessibility_id("knowledge-note-editor")
                        .aria_label(strings.get("rightSidebar.contextNotesTodo.notes.editorLabel")),
                )
                .into_any_element(),
        ]
    }

    fn render_todo(
        &self,
        todo: &Todo,
        strings: &Strings,
        palette: &Palette,
        direction: Direction,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let expanded = self.expanded.contains(&todo.id);
        let menu_open = self.todo_menu.as_deref() == Some(todo.id.as_str());
        let id = todo.id.clone();
        let (check_id, expand_id, send_id, current_id, new_id, delete_id) = (
            id.clone(),
            id.clone(),
            id.clone(),
            id.clone(),
            id.clone(),
            id.clone(),
        );
        let sending = self.sending.as_deref() == Some(todo.id.as_str());
        let element_id = format!("knowledge-todo:{}", todo.id);
        div()
            .id(SharedString::from(element_id.clone()))
            .role(Role::ListItem)
            .accessibility_id(element_id)
            .aria_label(SharedString::from(todo.text.clone()))
            .test_support()
            .px(px(10.0))
            .py(px(6.0))
            .flex_row_from(direction)
            .when(expanded, |this| this.items_start())
            .when(!expanded, |this| this.items_center())
            .gap(px(6.0))
            .child(
                on_activate(
                    div()
                        .id(SharedString::from(format!("knowledge-todo-check:{}", todo.id)))
                        .role(Role::CheckBox)
                        .accessibility_id(format!("knowledge-todo-check:{}", todo.id))
                        .aria_label(strings.format(
                            "rightSidebar.contextNotesTodo.todo.actions.markComplete",
                            &[("text", &todo.text)],
                        ))
                        .aria_toggled(if todo.completed {
                            gpui_kit::Toggled::True
                        } else {
                            gpui_kit::Toggled::False
                        })
                        .cursor_pointer()
                        .child(glyph(
                            if todo.completed { "checkbox" } else { "checkbox-blank" },
                            16.0,
                            if todo.completed { palette.primary } else { palette.muted_foreground },
                        )),
                    cx,
                    move |this: &mut Self, window, cx| this.toggle_todo(&check_id, window, cx),
                )
                .test_support(),
            )
            .child(
                on_activate(
                    div()
                        .id(SharedString::from(format!("knowledge-todo-text:{}", todo.id)))
                        .role(Role::Button)
                        .accessibility_id(format!("knowledge-todo-text:{}", todo.id))
                        .aria_label(strings.format(
                            if expanded {
                                "rightSidebar.contextNotesTodo.todo.actions.collapse"
                            } else {
                                "rightSidebar.contextNotesTodo.todo.actions.expand"
                            },
                            &[("text", &todo.text)],
                        ))
                        .flex_1()
                        .min_w_0()
                        .cursor_pointer()
                        .child(
                            text(
                                todo.text.clone(),
                                (LABEL_SIZE, LABEL_LINE),
                                if todo.completed { palette.muted_foreground } else { palette.foreground },
                                cx,
                            )
                            .when(todo.completed, |this| this.line_through())
                            .when(!expanded, |this| this.truncate()),
                        ),
                    cx,
                    move |this: &mut Self, _, cx| this.toggle_expanded(&expand_id, cx),
                )
                .test_support(),
            )
            .child(
                div()
                    .relative()
                    .child(button(
                        format!("knowledge-todo-send:{}", todo.id),
                        Some("send-plane"),
                        None,
                        strings.format("rightSidebar.contextNotesTodo.todo.actions.send", &[("text", &todo.text)]),
                        Variant::Ghost,
                        sending,
                        palette,
                        direction,
                        cx,
                        move |this: &mut Self, _, cx| this.toggle_todo_menu(&send_id, cx),
                    ))
                    .when(menu_open, |this| {
                        this.child(div().absolute().top_0().right_0().child(popup(
                            "knowledge-todo-send-menu",
                            vec![
                                option_row(
                                    format!("knowledge-todo-send-current:{id}"),
                                    strings.get("rightSidebar.contextNotesTodo.todo.sendMenu.currentSession"),
                                    false,
                                    false,
                                    palette,
                                    direction,
                                    cx,
                                    move |this: &mut Self, _, cx| this.send_todo_to_current(&current_id, cx),
                                ),
                                option_row(
                                    format!("knowledge-todo-send-new:{id}"),
                                    strings.get("rightSidebar.contextNotesTodo.todo.sendMenu.newSession"),
                                    false,
                                    false,
                                    palette,
                                    direction,
                                    cx,
                                    move |this: &mut Self, _, cx| this.send_todo_to_new(&new_id, cx),
                                ),
                            ],
                            224.0,
                            palette,
                        )))
                    }),
            )
            .child(self.row_menu(
                todo.id.clone(),
                vec![option_row(
                    format!("knowledge-todo-delete:{}", todo.id),
                    strings.get("rightSidebar.contextNotesTodo.todo.menu.delete"),
                    false,
                    true,
                    palette,
                    direction,
                    cx,
                    move |this: &mut Self, window, cx| this.delete_todo(&delete_id, window, cx),
                )],
                strings,
                palette,
                cx,
            ))
            .into_any_element()
    }

    fn render_todos(
        &self,
        strings: &Strings,
        palette: &Palette,
        direction: Direction,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let mut rows = vec![
            self.composer(
                "knowledge-todo-composer",
                Input::new(&self.todo_input)
                    .appearance(false)
                    .bordered(false)
                    .text_size(rems(LABEL_SIZE))
                    .accessibility_id("knowledge-todo-input")
                    .aria_label(strings.get("rightSidebar.contextNotesTodo.todo.inputPlaceholder"))
                    .into_any_element(),
                "knowledge-todo-add",
                strings.get("rightSidebar.contextNotesTodo.todo.addAria"),
                palette,
                direction,
                cx,
                |this: &mut Self, window, cx| this.add_todo(window, cx),
            ),
        ];
        let todos = self
            .knowledge
            .as_ref()
            .map(|knowledge| knowledge::display_order(&knowledge.todos))
            .unwrap_or_default();
        let visible: Vec<&Todo> = todos
            .iter()
            .filter(|todo| knowledge::todo_matches(todo, &self.query))
            .collect();
        let list = if visible.is_empty() {
            vec![self.empty_line(
                "rightSidebar.contextNotesTodo.todo.empty",
                strings,
                palette,
                cx,
            )]
        } else {
            visible
                .into_iter()
                .map(|todo| self.render_todo(todo, strings, palette, direction, cx))
                .collect()
        };
        rows.push(
            div()
                .id("knowledge-todos")
                .role(Role::List)
                .accessibility_id("knowledge-todos")
                .rounded(px(8.0))
                .border_1()
                .border_color(palette.border)
                .flex()
                .flex_col()
                .children(list)
                .into_any_element(),
        );
        if todos.iter().any(|todo| todo.completed) {
            rows.push(button(
                "knowledge-todo-clear",
                None,
                Some(strings.get("rightSidebar.contextNotesTodo.todo.clearCompleted")),
                strings.get("rightSidebar.contextNotesTodo.todo.clearCompleted"),
                Variant::Ghost,
                false,
                palette,
                direction,
                cx,
                |this: &mut Self, window, cx| this.clear_completed(window, cx),
            ));
        }
        rows
    }

    fn render_plan_row(
        &self,
        plan: &Plan,
        strings: &Strings,
        palette: &Palette,
        direction: Direction,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = plan.id.clone();
        let (move_id, delete_id) = (id.clone(), id.clone());
        let shared = plan.source == PlanSource::Shared;
        let hover = palette.hover;
        let element_id = format!("knowledge-plan:{}", plan.id);
        div()
            .px(px(4.0))
            .py(px(4.0))
            .flex_row_from(direction)
            .items_center()
            .gap(px(4.0))
            .child(
                on_activate(
                    div()
                        .id(SharedString::from(element_id.clone()))
                        .role(Role::Button)
                        .accessibility_id(element_id)
                        .aria_label(SharedString::from(plan.title.clone()))
                        .flex_1()
                        .min_w_0()
                        .px(px(6.0))
                        .py(px(4.0))
                        .rounded(px(6.0))
                        .flex_row_from(direction)
                        .items_center()
                        .gap(px(8.0))
                        .cursor_pointer()
                        .hover(move |style| style.bg(hover))
                        .child(
                            text(
                                plan.title.clone(),
                                (LABEL_SIZE, LABEL_LINE),
                                palette.foreground,
                                cx,
                            )
                            .min_w_0()
                            .truncate(),
                        )
                        .when(shared, |this| {
                            this.child(
                                text(
                                    strings.get("rightSidebar.contextNotesTodo.plans.sharedBadge"),
                                    MICRO,
                                    palette.muted_foreground,
                                    cx,
                                )
                                .px(px(4.0))
                                .rounded(px(4.0))
                                .bg(palette.subtle),
                            )
                        })
                        .when_some(plan.progress, |this, progress| {
                            let element_id = format!("knowledge-plan-progress:{}", plan.id);
                            this.child(if progress.is_done() {
                                div()
                                    .id(SharedString::from(element_id.clone()))
                                    .accessibility_id(element_id)
                                    .aria_label(
                                        strings.get(
                                            "rightSidebar.contextNotesTodo.plans.progress.done",
                                        ),
                                    )
                                    .child(
                                        text(
                                            strings.get(
                                                "rightSidebar.contextNotesTodo.plans.progress.done",
                                            ),
                                            MICRO,
                                            palette.status_success,
                                            cx,
                                        )
                                        .px(px(4.0)),
                                    )
                            } else {
                                let title = strings.format(
                                    "rightSidebar.contextNotesTodo.plans.progress.title",
                                    &[
                                        ("done", &progress.done.to_string()),
                                        ("total", &progress.total.to_string()),
                                    ],
                                );
                                div()
                                    .id(SharedString::from(element_id.clone()))
                                    .accessibility_id(element_id)
                                    .aria_label(title)
                                    .child(text(
                                        format!("{}/{}", progress.done, progress.total),
                                        MICRO,
                                        palette.muted_foreground,
                                        cx,
                                    ))
                            })
                        })
                        .child(div().flex_1())
                        .child(text(
                            knowledge_date(plan.created_at, strings, cx),
                            MICRO,
                            palette.muted_foreground,
                            cx,
                        )),
                    cx,
                    move |this: &mut Self, window, cx| this.open_plan(Some(id.clone()), window, cx),
                )
                .test_support(),
            )
            .when_some(plan.session_id.clone(), |this, session| {
                this.child(
                    IconButton::new(
                        SharedString::from(format!("knowledge-plan-session:{}", plan.id)),
                        "chat-3",
                        strings.get("rightSidebar.contextNotesTodo.plans.session.open"),
                    )
                    .size(24.0, 14.0)
                    .render(palette, cx, move |this: &mut Self, _, cx| {
                        this.open_session(&session, cx)
                    }),
                )
            })
            .child(self.row_menu(
                plan.id.clone(),
                vec![
                    option_row(
                        format!("knowledge-plan-move:{}", plan.id),
                        strings.get(if shared {
                            "rightSidebar.contextNotesTodo.plans.menu.makePersonal"
                        } else {
                            "rightSidebar.contextNotesTodo.plans.menu.share"
                        }),
                        false,
                        false,
                        palette,
                        direction,
                        cx,
                        move |this: &mut Self, window, cx| this.move_plan(&move_id, window, cx),
                    ),
                    option_row(
                        format!("knowledge-plan-delete:{}", plan.id),
                        strings.get("rightSidebar.contextNotesTodo.plans.deletePlan"),
                        false,
                        true,
                        palette,
                        direction,
                        cx,
                        move |this: &mut Self, window, cx| {
                            this.schedule_delete(Section::Plans, &delete_id, window, cx)
                        },
                    ),
                ],
                strings,
                palette,
                cx,
            ))
            .into_any_element()
    }

    fn render_plans(
        &self,
        strings: &Strings,
        palette: &Palette,
        direction: Direction,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let mut rows = vec![
            self.composer(
                "knowledge-plan-composer",
                Input::new(&self.plan_input)
                    .appearance(false)
                    .bordered(false)
                    .text_size(rems(LABEL_SIZE))
                    .accessibility_id("knowledge-plan-input")
                    .aria_label(strings.get("rightSidebar.contextNotesTodo.plans.newPlaceholder"))
                    .into_any_element(),
                "knowledge-plan-add",
                strings.get("rightSidebar.contextNotesTodo.plans.create"),
                palette,
                direction,
                cx,
                |this: &mut Self, window, cx| this.add_plan(window, cx),
            ),
        ];
        let plans: Vec<Plan> = self
            .knowledge
            .as_ref()
            .map(|knowledge| {
                knowledge
                    .plans
                    .iter()
                    .filter(|plan| {
                        !self.delete_pending(&plan.id) && knowledge::plan_matches(plan, &self.query)
                    })
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        let list = if plans.is_empty() {
            vec![self.empty_line(
                "rightSidebar.contextNotesTodo.plans.empty",
                strings,
                palette,
                cx,
            )]
        } else {
            plans
                .iter()
                .map(|plan| self.render_plan_row(plan, strings, palette, direction, cx))
                .collect()
        };
        rows.push(
            div()
                .id("knowledge-plans")
                .role(Role::List)
                .accessibility_id("knowledge-plans")
                .rounded(px(8.0))
                .border_1()
                .border_color(palette.border)
                .flex()
                .flex_col()
                .children(list)
                .into_any_element(),
        );
        rows
    }

    fn render_open_plan(
        &self,
        id: &str,
        strings: &Strings,
        palette: &Palette,
        direction: Direction,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let title = match &self.plan {
            Some(PlanLoad::Ready(document)) => document.plan.title.clone(),
            _ => self
                .knowledge
                .as_ref()
                .and_then(|knowledge| knowledge.plans.iter().find(|plan| plan.id == id))
                .map(|plan| plan.title.clone())
                .unwrap_or_default(),
        };
        let ready = matches!(self.plan, Some(PlanLoad::Ready(_)));
        let sending = self.sending.as_deref() == Some(id);
        let mut rows = vec![
            div()
                .flex_row_from(direction)
                .flex_wrap()
                .items_center()
                .gap(px(6.0))
                .child(
                    IconButton::new(
                        "knowledge-back",
                        "arrow-left-s",
                        strings.get("rightSidebar.contextNotesTodo.plans.actions.back"),
                    )
                    .size(24.0, 16.0)
                    .render(palette, cx, |this: &mut Self, _, cx| this.close_entry(cx)),
                )
                .child(
                    text(title, (LABEL_SIZE, LABEL_LINE), palette.foreground, cx)
                        .font_weight(FontWeight::MEDIUM)
                        .flex_1()
                        .min_w_0()
                        .truncate(),
                )
                .child(button(
                    "knowledge-plan-improve",
                    None,
                    Some(strings.get("planView.actions.improve")),
                    strings.get("planView.actions.improve"),
                    Variant::Outline,
                    !ready || sending,
                    palette,
                    direction,
                    cx,
                    |this: &mut Self, _, cx| this.send_plan(false, cx),
                ))
                .child(button(
                    "knowledge-plan-implement",
                    Some("play"),
                    Some(strings.get("planView.actions.implement")),
                    strings.get("planView.actions.implement"),
                    Variant::Primary,
                    !ready || sending,
                    palette,
                    direction,
                    cx,
                    |this: &mut Self, _, cx| this.send_plan(true, cx),
                ))
                .into_any_element(),
        ];
        rows.push(match &self.plan {
            Some(PlanLoad::Ready(_)) => div()
                .id("knowledge-plan-editor-frame")
                .role(Role::Group)
                .accessibility_id("knowledge-plan-editor-frame")
                .child(
                    Textarea::new(&self.plan_editor)
                        .appearance(false)
                        .bordered(false)
                        .text_size(rems(META_SIZE))
                        .accessibility_id("knowledge-plan-editor")
                        .aria_label(strings.get("rightSidebar.contextNotesTodo.tabs.plans")),
                )
                .into_any_element(),
            Some(PlanLoad::Missing) => self.state_line(
                "knowledge-plan-state",
                strings.get("rightSidebar.contextNotesTodo.toast.updatePlanFailed"),
                true,
                palette,
                cx,
            ),
            Some(PlanLoad::Failed(error)) => self.state_line(
                "knowledge-plan-state",
                if error.is_empty() {
                    strings.get("rightSidebar.contextNotesTodo.toast.loadNotesFailed")
                } else {
                    error.clone().into()
                },
                true,
                palette,
                cx,
            ),
            _ => self.state_line(
                "knowledge-plan-state",
                strings.get("common.loading"),
                false,
                palette,
                cx,
            ),
        });
        rows
    }

    fn state_line(
        &self,
        id: &'static str,
        value: SharedString,
        error: bool,
        palette: &Palette,
        cx: &Context<Self>,
    ) -> AnyElement {
        div()
            .id(id)
            .role(if error { Role::Alert } else { Role::Status })
            .accessibility_id(id)
            .aria_label(value.clone())
            .test_support()
            .p(px(12.0))
            .child(text(
                value,
                META,
                if error {
                    palette.status_error
                } else {
                    palette.muted_foreground
                },
                cx,
            ))
            .into_any_element()
    }
}

impl Render for KnowledgePanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let strings = locale::strings(cx).clone();
        let direction = strings.direction();
        let palette = cx.global::<Palette>().clone();
        if let Some(body) = self.take_adopted_note() {
            self.note_editor
                .update(cx, |input, cx| input.set_value(body, window, cx));
        }
        if self.take_load_failure() {
            window.push_notification(
                gpui_kit::component::notification::Notification::error(
                    strings.get("rightSidebar.contextNotesTodo.toast.loadNotesFailed"),
                ),
                cx,
            );
        }
        let placeholders = [
            (
                &self.note_input,
                "rightSidebar.contextNotesTodo.notes.placeholder",
            ),
            (
                &self.plan_editor,
                "rightSidebar.contextNotesTodo.tabs.plans",
            ),
        ];
        for (input, key) in placeholders {
            let placeholder = strings.get(key);
            if input.read(cx).presentation().placeholder() != &placeholder {
                input.update(cx, |input, cx| {
                    input.set_placeholder(placeholder, window, cx)
                });
            }
        }
        for (input, key) in [
            (
                &self.todo_input,
                "rightSidebar.contextNotesTodo.todo.inputPlaceholder",
            ),
            (
                &self.plan_input,
                "rightSidebar.contextNotesTodo.plans.newPlaceholder",
            ),
        ] {
            let placeholder = strings.get(key);
            if input.read(cx).presentation().placeholder() != &placeholder {
                input.update(cx, |input, cx| {
                    input.set_placeholder(placeholder, window, cx)
                });
            }
        }
        let page = cx.entity().downgrade();
        cx.defer(move |cx| {
            let _ = page.update(cx, |page, cx| page.refresh_if_stale(cx));
        });
        let open = self
            .open
            .clone()
            .filter(|(section, _)| *section == self.section);
        let body: Vec<AnyElement> = if self.knowledge.is_none() {
            match &self.load_error {
                Some(error) => vec![
                    self.state_line(
                        "knowledge-failed",
                        if error.is_empty() {
                            strings.get("rightSidebar.contextNotesTodo.toast.loadNotesFailed")
                        } else {
                            error.clone().into()
                        },
                        true,
                        &palette,
                        cx,
                    ),
                    button(
                        "knowledge-retry",
                        Some("refresh"),
                        Some(strings.get("sessions.scheduledTasks.editor.targetRetry")),
                        strings.get("sessions.scheduledTasks.editor.targetRetry"),
                        Variant::Outline,
                        false,
                        &palette,
                        direction,
                        cx,
                        |this: &mut Self, _, cx| this.reload(cx),
                    ),
                ],
                None => vec![self.state_line(
                    "knowledge-loading",
                    strings.get("common.loading"),
                    false,
                    &palette,
                    cx,
                )],
            }
        } else {
            match (self.section, open) {
                (Section::Notes, Some((_, id))) => {
                    self.render_open_note(&id, &strings, &palette, direction, cx)
                }
                (Section::Plans, Some((_, id))) => {
                    self.render_open_plan(&id, &strings, &palette, direction, cx)
                }
                (Section::Notes, _) => self.render_notes(&strings, &palette, direction, cx),
                (Section::Todos, _) => self.render_todos(&strings, &palette, direction, cx),
                (Section::Plans, _) => self.render_plans(&strings, &palette, direction, cx),
            }
        };
        let entry_open = self
            .open
            .as_ref()
            .is_some_and(|(section, _)| *section == self.section);
        div()
            .id("knowledge-panel")
            .role(Role::Region)
            .accessibility_id("knowledge-panel")
            .test_support()
            .aria_label(strings.get("contextRail.surface.notes"))
            .size_full()
            .flex()
            .flex_col()
            .child(self.render_tabs(&strings, &palette, direction, cx))
            .when(self.search_open && !entry_open, |this| {
                this.child(self.render_search(&strings, &palette, direction, cx))
            })
            .child(
                div()
                    .id("knowledge-body")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .p(px(8.0))
                    .flex()
                    .flex_col()
                    .gap(px(8.0))
                    .children(body),
            )
    }
}
