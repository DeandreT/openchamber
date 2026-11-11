//! The Changes walkthrough. The server owns generation and hunk identity;
//! this view retains the last complete read through errors and detachments.

use std::time::Duration;

use gpui_kit::{
    AnyElement, Context, FontWeight, InteractiveElement, IntoElement, ParentElement, Role,
    SharedString, StatefulInteractiveElement, Styled, Task, div, prelude::FluentBuilder, px,
};
use openchamber_native_transport::git::PathDiff;
use openchamber_native_transport::walkthrough::{self, Options, Read, Scope, Source, Stage};

use super::{DiffPane, DiffView};
use crate::baseline::CssTextExt;
use crate::context_panel::{ROW_LINE, ROW_SIZE, XS_LINE, XS_SIZE};
use crate::files_view::{Notice, empty_state, status_line, text_button};
use crate::locale::{self, Direction};
use crate::logical::LogicalStyledExt;
use crate::theme::Palette;

pub(super) struct WalkthroughPanel {
    pub open: bool,
    result: Option<Read>,
    loading: bool,
    generating: bool,
    stage: Option<Stage>,
    notice: Option<Notice>,
    selected: usize,
    pane: DiffPane,
    request: Option<Task<()>>,
    progress: Option<Task<()>>,
    cancel: Option<Task<()>>,
}

impl WalkthroughPanel {
    pub fn new(cx: &mut gpui_kit::App) -> Self {
        Self {
            open: false,
            result: None,
            loading: false,
            generating: false,
            stage: None,
            notice: None,
            selected: 0,
            pane: DiffPane::new(cx),
            request: None,
            progress: None,
            cancel: None,
        }
    }

    fn choices(&self) -> Vec<(String, String, Vec<String>)> {
        let Some(result) = &self.result else {
            return Vec::new();
        };
        let mut choices = Vec::new();
        if let Some(walkthrough) = &result.walkthrough {
            for chapter in &walkthrough.chapters {
                for stop in &chapter.stops {
                    choices.push((stop.id.clone(), stop.title.clone(), stop.hunk_ids.clone()));
                }
            }
        }
        for id in &result.uncovered_hunk_ids {
            if let Some(hunk) = result.hunks.iter().find(|hunk| &hunk.id == id) {
                choices.push((hunk.id.clone(), hunk.path.clone(), vec![hunk.id.clone()]));
            }
        }
        choices
    }

    fn select(&mut self, index: usize, cx: &mut gpui_kit::App) {
        self.selected = index;
        let choices = self.choices();
        let Some((_, _, ids)) = choices.get(index) else {
            self.pane.clear(cx);
            return;
        };
        let Some(result) = &self.result else { return };
        let patches = ids
            .iter()
            .filter_map(|id| result.hunks.iter().find(|hunk| &hunk.id == id))
            .map(|hunk| PathDiff::Patch(hunk.patch.clone()))
            .collect();
        self.pane.apply(Ok(patches), cx);
    }
}

impl DiffView {
    fn walkthrough_source(&self) -> Source {
        Source::WorkingTree {
            scope: if self.change_scope == openchamber_native_core::git_changes::ChangeScope::Staged
            {
                Scope::Staged
            } else {
                Scope::Working
            },
        }
    }

    fn walkthrough_options(&self, cx: &gpui_kit::App) -> Options {
        let model = self
            .scope
            .owner
            .as_ref()
            .and_then(|owner| owner.upgrade())
            .and_then(|owner| owner.read(cx).send_selection())
            .and_then(|(_, selected)| selected.model)
            .filter(|model| model.provider_id != "openchamber");
        Options {
            provider_id: model.as_ref().map(|model| model.provider_id.clone()),
            model_id: model.map(|model| model.model_id),
            language: locale::strings(cx).locale().to_owned(),
            ..Options::default()
        }
    }

    pub(super) fn toggle_walkthrough(&mut self, cx: &mut Context<Self>) {
        self.walkthrough.open = !self.walkthrough.open;
        if self.walkthrough.open {
            self.load_walkthrough(cx);
        } else {
            self.refresh(cx);
        }
        cx.notify();
    }

    pub(super) fn load_walkthrough(&mut self, cx: &mut Context<Self>) {
        if self.walkthrough.generating {
            return;
        }
        let Some(client) = self.scope.client(cx) else {
            return;
        };
        let source = self.walkthrough_source();
        let options = self.walkthrough_options(cx);
        let directory = self.scope.directory.clone();
        let task = self
            .scope
            .run(async move { client.read_walkthrough(&directory, &source, &options).await });
        self.walkthrough.loading = true;
        self.walkthrough.notice = None;
        self.walkthrough.request = Some(cx.spawn(async move |this, cx| {
            let Some(result) = task.await else { return };
            let _ = this.update(cx, |this, cx| {
                this.walkthrough.request = None;
                this.walkthrough.loading = false;
                match result {
                    Ok(read) => {
                        let generating = read.generating;
                        this.walkthrough.result = Some(read);
                        this.walkthrough.select(0, cx);
                        if generating {
                            this.generate_walkthrough(false, cx);
                        }
                    }
                    Err(error) => this.walkthrough_error(error, cx),
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn walkthrough_error(&mut self, error: walkthrough::Error, cx: &gpui_kit::App) {
        let title = error
            .code()
            .map(blocked_title)
            .map(|key| locale::strings(cx).get(key))
            .unwrap_or_else(|| locale::strings(cx).get("diffView.state.failedToLoadDiff"));
        self.walkthrough.notice = Some(Notice::error(title, error.message()));
    }

    fn generate_walkthrough(&mut self, force: bool, cx: &mut Context<Self>) {
        if self.walkthrough.generating {
            return;
        }
        let Some(client) = self.scope.client(cx) else {
            return;
        };
        let source = self.walkthrough_source();
        let options = self.walkthrough_options(cx);
        let directory = self.scope.directory.clone();
        let task = self.scope.run(async move {
            client
                .generate_walkthrough(&directory, &source, &options, force)
                .await
        });
        self.walkthrough.generating = true;
        self.walkthrough.loading = false;
        self.walkthrough.stage = Some(Stage::Collecting);
        self.walkthrough.notice = None;
        self.walkthrough.request = Some(cx.spawn(async move |this, cx| {
            let Some(result) = task.await else { return };
            let _ = this.update(cx, |this, cx| {
                this.walkthrough.request = None;
                this.walkthrough.progress = None;
                this.walkthrough.generating = false;
                this.walkthrough.stage = None;
                match result {
                    Ok(read) => {
                        this.walkthrough.result = Some(read);
                        this.walkthrough.select(0, cx);
                    }
                    Err(error) => this.walkthrough_error(error, cx),
                }
                cx.notify();
            });
        }));
        self.walkthrough.progress = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(1)).await;
                let visible = this
                    .read_with(cx, |this, cx| {
                        this.walkthrough.open
                            && this
                                .scope
                                .owner
                                .as_ref()
                                .and_then(|owner| owner.upgrade())
                                .is_none_or(|owner| {
                                    let owner = owner.read(cx);
                                    owner.runtime == this.scope.runtime
                                        && owner.panels.open
                                            == Some(crate::files_view::PanelSurface::Diff)
                                        && owner.panel_directory().as_deref()
                                            == Some(&this.scope.directory)
                                })
                    })
                    .unwrap_or(false);
                if !visible {
                    continue;
                }
                let request = this
                    .update(cx, |this, cx| {
                        if !this.walkthrough.generating {
                            return None;
                        }
                        let client = this.scope.client(cx)?;
                        let source = this.walkthrough_source();
                        let directory = this.scope.directory.clone();
                        Some(this.scope.run(async move {
                            client.walkthrough_progress(&directory, &source).await
                        }))
                    })
                    .ok()
                    .flatten();
                let Some(request) = request else { return };
                let result = request.await;
                let _ = this.update(cx, |this, cx| {
                    if let Some(Ok(stage)) = result {
                        this.walkthrough.stage = stage;
                        cx.notify();
                    }
                });
            }
        }));
        cx.notify();
    }

    fn cancel_walkthrough(&mut self, cx: &mut Context<Self>) {
        if self.walkthrough.cancel.is_some() {
            return;
        }
        let Some(client) = self.scope.client(cx) else {
            return;
        };
        let source = self.walkthrough_source();
        let directory = self.scope.directory.clone();
        let task = self
            .scope
            .run(async move { client.cancel_walkthrough(&directory, &source).await });
        self.walkthrough.cancel = Some(cx.spawn(async move |this, cx| {
            let Some(result) = task.await else { return };
            let _ = this.update(cx, |this, cx| {
                this.walkthrough.cancel = None;
                match result {
                    Ok(()) => {
                        this.walkthrough.request = None;
                        this.walkthrough.progress = None;
                        this.walkthrough.generating = false;
                        this.walkthrough.stage = None;
                        this.load_walkthrough(cx);
                    }
                    Err(error) => this.walkthrough_error(error, cx),
                }
                cx.notify();
            });
        }));
    }

    fn walkthrough_step(&mut self, delta: isize, cx: &mut Context<Self>) {
        let len = self.walkthrough.choices().len();
        if len == 0 {
            return;
        }
        let index = self
            .walkthrough
            .selected
            .saturating_add_signed(delta)
            .min(len - 1);
        self.walkthrough.select(index, cx);
        cx.notify();
    }

    pub(super) fn render_walkthrough(
        &self,
        palette: &Palette,
        direction: Direction,
        cx: &Context<Self>,
    ) -> AnyElement {
        let state = &self.walkthrough;
        let strings = locale::strings(cx);
        let text = |value: SharedString| {
            div()
                .css_text(ROW_SIZE, ROW_LINE, cx)
                .text_color(palette.foreground)
                .child(value)
        };
        let secondary = |value: SharedString| {
            div()
                .css_text(XS_SIZE, XS_LINE, cx)
                .text_color(palette.muted_foreground)
                .child(value)
        };
        let ready = state
            .result
            .as_ref()
            .and_then(|read| read.readiness.as_ref());
        let narrative = state
            .result
            .as_ref()
            .and_then(|read| read.walkthrough.as_ref());
        let buttons = div()
            .flex_row_from(direction)
            .items_center()
            .gap(px(6.0))
            .when(
                !state.generating && ready.is_none_or(|ready| ready.ready),
                |row| {
                    row.child(text_button(
                        "walkthrough-generate",
                        strings.get(if narrative.is_some() {
                            "walkthrough.action.regenerate"
                        } else {
                            "walkthrough.action.generate"
                        }),
                        None,
                        palette,
                        cx,
                        |this: &mut Self, _, cx| this.generate_walkthrough(true, cx),
                    ))
                },
            )
            .when(state.generating, |row| {
                row.child(text_button(
                    "walkthrough-cancel",
                    strings.get("walkthrough.action.cancel"),
                    None,
                    palette,
                    cx,
                    |this: &mut Self, _, cx| this.cancel_walkthrough(cx),
                ))
            })
            .child(text_button(
                "walkthrough-refresh",
                strings.get("walkthrough.action.refresh"),
                None,
                palette,
                cx,
                |this: &mut Self, _, cx| this.load_walkthrough(cx),
            ));
        let mut toc = div()
            .id("walkthrough-chapters")
            .flex()
            .flex_col()
            .gap(px(4.0));
        let mut index = 0;
        if let Some(narrative) = narrative {
            for chapter in &narrative.chapters {
                toc = toc
                    .child(text(chapter.title.clone().into()).font_weight(FontWeight::SEMIBOLD))
                    .child(secondary(chapter.blurb.clone().into()));
                for stop in &chapter.stops {
                    let choice = index;
                    index += 1;
                    toc = toc.child(text_button(
                        SharedString::from(format!("walkthrough-stop:{}", stop.id)),
                        stop.title.clone().into(),
                        Some(state.selected == choice),
                        palette,
                        cx,
                        move |this: &mut Self, _, cx| {
                            this.walkthrough.select(choice, cx);
                            cx.notify();
                        },
                    ));
                }
            }
        }
        if let Some(read) = &state.result
            && !read.uncovered_hunk_ids.is_empty()
            && narrative.is_some()
        {
            toc = toc.child(secondary(strings.format(
                "walkthrough.uncovered.title",
                &[("count", &read.uncovered_hunk_ids.len().to_string())],
            )));
            for id in &read.uncovered_hunk_ids {
                if let Some(hunk) = read.hunks.iter().find(|hunk| &hunk.id == id) {
                    let choice = index;
                    index += 1;
                    toc = toc.child(text_button(
                        SharedString::from(format!("walkthrough-hunk:{}", hunk.id)),
                        hunk.path.clone().into(),
                        Some(state.selected == choice),
                        palette,
                        cx,
                        move |this: &mut Self, _, cx| {
                            this.walkthrough.select(choice, cx);
                            cx.notify();
                        },
                    ));
                }
            }
        }
        let choices = state.choices();
        let selected = choices.get(state.selected).and_then(|(id, _, _)| {
            narrative.and_then(|narrative| {
                narrative
                    .chapters
                    .iter()
                    .flat_map(|chapter| &chapter.stops)
                    .find(|stop| &stop.id == id)
            })
        });
        div()
            .id("walkthrough-panel")
            .role(Role::Group)
            .accessibility_id("walkthrough-panel")
            .aria_label(strings.get("walkthrough.action.open"))
            .size_full()
            .flex()
            .flex_col()
            .gap(px(8.0))
            .p(px(12.0))
            .child(buttons)
            .children(state.notice.as_ref().map(|notice| {
                status_line("walkthrough-notice", Some(notice), palette, direction, cx)
            }))
            .when(state.loading, |body| {
                body.child(secondary(strings.get("diffView.state.loadingDiff")))
            })
            .when(state.generating, |body| {
                body.child(
                    div()
                        .id("walkthrough-progress")
                        .role(Role::Status)
                        .accessibility_id("walkthrough-progress")
                        .aria_label(strings.get(stage_key(state.stage)))
                        .child(secondary(strings.get(stage_key(state.stage)))),
                )
            })
            .when_some(ready.filter(|ready| !ready.ready), |body, ready| {
                body.child(secondary(
                    strings.get(blocked_title(ready.reason.as_deref().unwrap_or("no-model"))),
                ))
            })
            .when(
                narrative.is_none()
                    && !state.loading
                    && !state.generating
                    && ready.is_none_or(|ready| ready.ready),
                |body| {
                    body.child(text(strings.get("walkthrough.empty.title")))
                        .child(secondary(strings.get("walkthrough.empty.description")))
                },
            )
            .when_some(narrative, |body, narrative| {
                body.child(text(narrative.title.clone().into()))
                    .child(secondary(narrative.focus.clone().into()))
            })
            .when_some(
                state.result.as_ref().filter(|read| read.is_stale),
                |body, read| {
                    body.child(secondary(strings.format(
                        "walkthrough.stale.banner",
                        &[("count", &read.stale_stop_ids.len().to_string())],
                    )))
                },
            )
            .when(narrative.is_some(), |body| {
                body.child(
                    div()
                        .id("walkthrough-toc-scroll")
                        .max_h(px(180.0))
                        .overflow_y_scroll()
                        .child(toc),
                )
                .when_some(selected, |body, stop| {
                    let mut body = body
                        .child(text(stop.title.clone().into()))
                        .child(secondary(stop.prose.clone().into()));
                    for observation in &stop.observations {
                        body = body.child(secondary(
                            format!(
                                "{}: {}",
                                strings
                                    .get(&format!("walkthrough.observation.{}", observation.kind)),
                                observation.note
                            )
                            .into(),
                        ));
                    }
                    body
                })
                .child(div().flex_1().min_h(px(100.0)).child(
                    if choices.get(state.selected).is_some_and(|(_, _, ids)| {
                        state.result.as_ref().is_some_and(|read| {
                            ids.iter()
                                .all(|id| read.hunks.iter().all(|hunk| &hunk.id != id))
                        })
                    }) {
                        empty_state(strings.get("walkthrough.stop.noCode"), palette, cx)
                    } else {
                        state.pane.render(self.wrap, palette, cx)
                    },
                ))
                .child(
                    div()
                        .flex_row_from(direction)
                        .gap(px(8.0))
                        .child(text_button(
                            "walkthrough-previous",
                            strings.get("walkthrough.action.previous"),
                            None,
                            palette,
                            cx,
                            |this: &mut Self, _, cx| this.walkthrough_step(-1, cx),
                        ))
                        .child(text_button(
                            "walkthrough-next",
                            strings.get("walkthrough.action.next"),
                            None,
                            palette,
                            cx,
                            |this: &mut Self, _, cx| this.walkthrough_step(1, cx),
                        )),
                )
            })
            .into_any_element()
    }
}

fn stage_key(stage: Option<Stage>) -> &'static str {
    match stage {
        Some(Stage::Collecting) => "walkthrough.stage.collecting",
        Some(Stage::Assembling) => "walkthrough.stage.assembling",
        _ => "walkthrough.stage.asking",
    }
}

fn blocked_title(reason: &str) -> &'static str {
    match reason {
        "empty-diff" => "walkthrough.blocked.emptyDiff.title",
        "only-generated" => "walkthrough.blocked.onlyGenerated.title",
        "context-too-small" => "walkthrough.blocked.contextTooSmall.title",
        "structured-output-unsupported" => "walkthrough.blocked.structuredOutput.title",
        "output-exhausted" => "walkthrough.blocked.outputExhausted.title",
        "server-unsupported" => "walkthrough.blocked.serverUnsupported.title",
        _ => "walkthrough.blocked.noModel.title",
    }
}
