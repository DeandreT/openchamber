//! Native Open VSX browser and explicit local-file import. The appearance owner
//! retains accepted writes; closing this browser only cancels reads and intake.

use std::collections::HashSet;
use std::io::Read;
use std::sync::Arc;
use std::time::Duration;

use gpui_kit::component::{
    WindowExt,
    input::{Input, InputEvent, InputState},
    notification::Notification,
};
use gpui_kit::{
    App, AppContext, Context, Entity, FocusHandle, Focusable, Image, InteractiveElement,
    IntoElement, ParentElement, PathPromptOptions, Render, Role, SharedString,
    StatefulInteractiveElement, Styled, Subscription, Task, Window, div, img, px,
};
use openchamber_native_transport::themes::{MAX_THEME_BYTES, ThemeExtension, ThemeSource};
use serde_json::Value;

use crate::activation::on_activate;
use crate::appearance::AppearanceState;
use crate::baseline::CssTextExt;
use crate::chrome::glyph;
use crate::locale;
use crate::logical::LogicalStyledExt;
use crate::network::NetworkTask;
use crate::settings_controls::ServerTarget;
use crate::theme::{self, Palette};
use crate::theme_import::import_vscode_theme;

use super::schematic;

struct Variant {
    name: String,
    definition: Option<Value>,
    image: Option<Arc<Image>>,
    selected: bool,
    saved: bool,
}

enum Page {
    Search {
        loading: bool,
        failed: bool,
        results: Vec<ThemeExtension>,
    },
    Package {
        extension: ThemeExtension,
        failed: bool,
    },
    Variants {
        extension: ThemeExtension,
        variants: Vec<Variant>,
    },
}

struct Catalog {
    owner: Entity<AppearanceState>,
    target: ServerTarget,
    generation: u64,
    search: Entity<InputState>,
    page: Page,
    read: Option<Task<()>>,
    intake: Option<Task<()>>,
    saving: bool,
    file_busy: bool,
    partial: bool,
    subscriptions: Vec<Subscription>,
}

pub(super) fn open(
    owner: Entity<AppearanceState>,
    return_focus: FocusHandle,
    window: &mut Window,
    cx: &mut App,
) {
    let Some(target) = owner.read(cx).theme_target() else {
        return;
    };
    let generation = owner.read(cx).theme_generation();
    let catalog = cx.new(|cx: &mut Context<Catalog>| {
        let search = cx.new(|cx| InputState::new(window, cx));
        let mut state = Catalog {
            owner: owner.clone(),
            target,
            generation,
            search: search.clone(),
            page: Page::Search {
                loading: true,
                failed: false,
                results: Vec::new(),
            },
            read: None,
            intake: None,
            saving: false,
            file_busy: false,
            partial: false,
            subscriptions: Vec::new(),
        };
        state
            .subscriptions
            .push(cx.subscribe(&search, |this, _, event, cx| {
                if matches!(event, InputEvent::Change) {
                    this.search(cx);
                }
            }));
        state
            .subscriptions
            .push(cx.observe_in(&owner, window, |this, owner, window, cx| {
                if owner.read(cx).theme_generation() != this.generation {
                    window.close_dialog(cx);
                }
            }));
        state.search(cx);
        state
    });
    let focus = catalog.read(cx).search.focus_handle(cx);
    window.open_dialog(cx, move |dialog, window, _| {
        dialog
            .w(px(672.0).min(window.viewport_size().width - px(32.0)))
            .max_h(window.viewport_size().height - px(32.0))
            .p(px(24.0))
            .close_button(false)
            .on_ok(|_, _, _| false)
            .on_close({
                let focus = return_focus.clone();
                move |_, window, cx| {
                    window.focus(&focus, cx);
                }
            })
            .child(catalog.clone())
    });
    window.focus(&focus, cx);
}

impl Catalog {
    fn current(&self, cx: &App) -> bool {
        self.owner.read(cx).theme_generation() == self.generation
    }

    fn search(&mut self, cx: &mut Context<Self>) {
        if !matches!(self.page, Page::Search { .. }) || !self.current(cx) {
            return;
        }
        let query: String = self
            .search
            .read(cx)
            .value()
            .trim()
            .chars()
            .take(160)
            .collect();
        let target = self.target.clone();
        self.page = Page::Search {
            loading: true,
            failed: false,
            results: Vec::new(),
        };
        self.read = Some(cx.spawn(async move |this, cx| {
            if !query.is_empty() {
                cx.background_executor()
                    .timer(Duration::from_millis(350))
                    .await;
            }
            let work = NetworkTask::spawn_on(&target.network, async move {
                target.client.search_theme_catalog(&query).await
            });
            let result = work.await;
            let _ = this.update(cx, |this, cx| {
                if !this.current(cx) {
                    return;
                }
                this.page = match result {
                    Some(Ok(results)) => Page::Search {
                        loading: false,
                        failed: false,
                        results,
                    },
                    _ => Page::Search {
                        loading: false,
                        failed: true,
                        results: Vec::new(),
                    },
                };
                this.read = None;
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn load(&mut self, extension: ThemeExtension, cx: &mut Context<Self>) {
        if self.saving || self.file_busy || !self.current(cx) {
            return;
        }
        self.partial = false;
        self.page = Page::Package {
            extension: extension.clone(),
            failed: false,
        };
        let target = self.target.clone();
        self.read = Some(cx.spawn(async move |this, cx| {
            let request = extension.clone();
            let work = NetworkTask::spawn_on(&target.network, async move {
                target.client.theme_package(&request).await
            });
            let result = work.await;
            let variants = match result {
                Some(Ok(sources)) => {
                    let author = extension.namespace.clone();
                    Some(
                        cx.background_executor()
                            .spawn(async move { variants(sources, &author) })
                            .await,
                    )
                }
                _ => None,
            };
            let _ = this.update(cx, |this, cx| {
                if !this.current(cx) {
                    return;
                }
                this.page = match variants {
                    Some(variants) => Page::Variants {
                        extension,
                        variants,
                    },
                    None => Page::Package {
                        extension,
                        failed: true,
                    },
                };
                this.read = None;
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn install(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.saving || self.file_busy || !self.current(cx) {
            return;
        }
        let Page::Variants { variants, .. } = &self.page else {
            return;
        };
        let selected: Vec<_> = variants
            .iter()
            .enumerate()
            .filter(|(_, v)| v.selected && !v.saved)
            .filter_map(|(i, v)| v.definition.clone().map(|definition| (i, definition)))
            .collect();
        if selected.is_empty() {
            return;
        }
        self.saving = true;
        self.partial = false;
        self.intake = Some(cx.spawn_in(window, async move |this, cx| {
            let mut failed = false;
            for (index, definition) in selected {
                let Ok(Some(receipt)) = this.update(cx, |this, cx| {
                    this.current(cx).then(|| {
                        this.owner
                            .update(cx, |owner, cx| owner.import_theme(definition, false, cx))
                    })
                }) else {
                    return;
                };
                let success = matches!(receipt.await, Ok(Ok(_)));
                failed |= !success;
                if this
                    .update(cx, |this, cx| {
                        if !this.current(cx) {
                            return;
                        }
                        if success && let Page::Variants { variants, .. } = &mut this.page {
                            variants[index].saved = true;
                            variants[index].selected = false;
                        }
                        cx.notify();
                    })
                    .is_err()
                {
                    return;
                }
            }
            let _ = this.update_in(cx, |this, window, cx| {
                if !this.current(cx) {
                    return;
                }
                this.saving = false;
                this.partial = failed;
                let text = locale::strings(cx).get(if failed {
                    "settings.themeImport.partialError"
                } else {
                    "settings.themeImport.complete"
                });
                window.push_notification(
                    if failed {
                        Notification::error(text)
                    } else {
                        Notification::success(text)
                    },
                    cx,
                );
                this.intake = None;
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn pick_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.saving || self.file_busy || !self.current(cx) {
            return;
        }
        self.file_busy = true;
        let answer = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some(locale::strings(cx).get("settings.themeImport.chooseFile")),
        });
        self.intake = Some(cx.spawn_in(window, async move |this, cx| {
            let result = match answer.await {
                Ok(Ok(Some(paths))) => {
                    let path = paths.into_iter().next();
                    Some(
                        cx.background_executor()
                            .spawn(async move {
                                let path = path.ok_or("invalid".to_owned())?;
                                let name = path
                                    .file_name()
                                    .and_then(|name| name.to_str())
                                    .ok_or("invalid".to_owned())?;
                                let file =
                                    std::fs::File::open(&path).map_err(|_| "invalid".to_owned())?;
                                let metadata = file.metadata().map_err(|_| "invalid".to_owned())?;
                                if !metadata.is_file() {
                                    return Err("invalid".into());
                                }
                                if metadata.len() > MAX_THEME_BYTES as u64 {
                                    return Err("size".into());
                                }
                                let mut bytes = Vec::new();
                                file.take((MAX_THEME_BYTES + 1) as u64)
                                    .read_to_end(&mut bytes)
                                    .map_err(|_| "invalid".to_owned())?;
                                if bytes.len() > MAX_THEME_BYTES {
                                    return Err("size".into());
                                }
                                let text =
                                    String::from_utf8(bytes).map_err(|_| "invalid".to_owned())?;
                                import_vscode_theme(&text, name)
                                    .map_err(|error| error.code().to_owned())
                            })
                            .await,
                    )
                }
                Ok(Ok(None)) => None,
                _ => Some(Err("invalid".into())),
            };
            let result = if let Some(Ok(definition)) = result {
                let Ok(Some(receipt)) = this.update(cx, |this, cx| {
                    this.current(cx).then(|| {
                        this.owner
                            .update(cx, |owner, cx| owner.import_theme(definition, true, cx))
                    })
                }) else {
                    return;
                };
                Some(receipt.await.unwrap_or_else(|_| Err("save".into())))
            } else {
                result.map(|result| Err(result.expect_err("file error")))
            };
            let _ = this.update_in(cx, |this, window, cx| {
                if !this.current(cx) {
                    return;
                }
                this.file_busy = false;
                this.intake = None;
                match result {
                    Some(Ok(theme)) => {
                        let text = locale::strings(cx).format(
                            "settings.themeImport.success",
                            &[("name", &theme.full_name)],
                        );
                        window.push_notification(Notification::success(text), cx);
                        window.close_dialog(cx);
                    }
                    Some(Err(code)) => {
                        let key = if code == "unsupported" {
                            "settings.appearance.native.error.themeColor".to_owned()
                        } else {
                            format!("settings.themeImport.error.{code}")
                        };
                        window.push_notification(
                            Notification::error(locale::strings(cx).get(&key)),
                            cx,
                        );
                    }
                    None => {}
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn button(
        &self,
        id: &'static str,
        label: SharedString,
        enabled: bool,
        cx: &Context<Self>,
        action: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    ) -> gpui_kit::AnyElement {
        on_activate(
            gpui_kit::base::Button::new(id)
                .accessibility_id(id)
                .accessibility_label(label.clone())
                .disabled(!enabled)
                .h(px(28.0))
                .px(px(8.0))
                .child(label),
            cx,
            move |this, window, cx| {
                if enabled {
                    action(this, window, cx);
                }
            },
        )
        .into_any_element()
    }
}

impl Render for Catalog {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let strings = locale::strings(cx).clone();
        let p = cx.global::<Palette>().clone();
        let direction = strings.direction();
        let busy = self.saving || self.file_busy;
        let mut header = div().flex_row_from(direction).items_center().gap(px(4.0));
        match &self.page {
            Page::Search { .. } => {
                let placeholder = strings.get("settings.themeImport.search");
                self.search.update(cx, |input, cx| {
                    input.set_placeholder(placeholder.clone(), window, cx)
                });
                header = header
                    .child(
                        Input::new(&self.search)
                            .id("theme-catalog-search")
                            .aria_label(placeholder)
                            .w(px(288.0))
                            .h(px(28.0))
                            .focus_bordered(false),
                    )
                    .child(crate::settings_controls::info_hint(
                        "theme-catalog-hint",
                        strings.get("settings.themeImport.catalogHint"),
                        cx,
                    ));
            }
            Page::Package { extension, .. } | Page::Variants { extension, .. } => {
                header = header
                    .child(self.button(
                        "theme-catalog-back",
                        strings.get("settings.themeImport.back"),
                        !busy,
                        cx,
                        |this, _, cx| {
                            this.page = Page::Search {
                                loading: true,
                                failed: false,
                                results: Vec::new(),
                            };
                            this.partial = false;
                            this.search(cx);
                        },
                    ))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .child(extension.label.clone()),
                    );
            }
        }
        if let Page::Variants { variants, .. } = &self.page {
            let selectable: Vec<_> = variants
                .iter()
                .filter(|v| v.definition.is_some() && !v.saved)
                .collect();
            let all = !selectable.is_empty() && selectable.iter().all(|v| v.selected);
            if selectable.len() > 1 {
                header = header.child(self.button(
                    "theme-catalog-select-all",
                    strings.get(if all {
                        "settings.themeImport.deselectAll"
                    } else {
                        "settings.themeImport.selectAll"
                    }),
                    !busy,
                    cx,
                    move |this, _, cx| {
                        if let Page::Variants { variants, .. } = &mut this.page {
                            for v in variants {
                                if v.definition.is_some() && !v.saved {
                                    v.selected = !all;
                                }
                            }
                        }
                        cx.notify();
                    },
                ));
            }
        }
        header = header.child(div().flex_1()).child(self.button(
            "theme-catalog-close",
            strings.get("dialog.common.actions.close"),
            true,
            cx,
            |_, window, cx| window.close_dialog(cx),
        ));
        let mut body = div()
            .id("theme-catalog-results")
            .h(gpui_kit::rems(31.0))
            .max_h(window.viewport_size().height - px(210.0))
            .overflow_y_scroll();
        let message = match &self.page {
            Page::Search { loading: true, .. } | Page::Package { failed: false, .. } => {
                Some(("common.loading", false))
            }
            Page::Search { failed: true, .. } | Page::Package { failed: true, .. } => {
                Some(("settings.themeImport.catalogError", true))
            }
            Page::Search { results, .. } if results.is_empty() => {
                Some(("settings.themeImport.empty", false))
            }
            Page::Variants { variants, .. } if !variants.iter().any(|v| v.definition.is_some()) => {
                Some(("settings.themeImport.noReadableThemes", true))
            }
            _ => None,
        };
        if let Some((key, error)) = message {
            body = body.child(
                div()
                    .id("theme-catalog-message")
                    .role(if error { Role::Alert } else { Role::Status })
                    .py(px(24.0))
                    .text_center()
                    .text_color(if error { p.error } else { p.muted_foreground })
                    .child(strings.get(key)),
            );
        } else {
            match &self.page {
                Page::Search { results, .. } => {
                    for extension in results {
                        let extension = extension.clone();
                        let icon = extension.icon.clone().map_or_else(
                            || glyph("palette", 36.0, p.muted_foreground).into_any_element(),
                            |url| {
                                img(SharedString::from(url))
                                    .size(px(36.0))
                                    .into_any_element()
                            },
                        );
                        body = body.child(on_activate(
                            gpui_kit::base::Button::new(SharedString::from(format!(
                                "theme-package-{}.{}",
                                extension.namespace, extension.name
                            )))
                            .accessibility_label(extension.label.clone())
                            .w_full()
                            .h(px(56.0))
                            .px(px(8.0))
                            .disabled(busy)
                            .child(
                                div()
                                    .w_full()
                                    .flex_row_from(direction)
                                    .items_center()
                                    .gap(px(8.0))
                                    .child(icon)
                                    .child(
                                        div()
                                            .min_w_0()
                                            .flex_1()
                                            .child(div().truncate().child(extension.label.clone()))
                                            .child(
                                                div()
                                                    .truncate()
                                                    .text_color(p.muted_foreground)
                                                    .child(extension.namespace.clone()),
                                            ),
                                    ),
                            ),
                            cx,
                            move |this, _, cx| this.load(extension.clone(), cx),
                        ));
                    }
                }
                Page::Variants { variants, .. } => {
                    let mut grid = div().flex_row_from(direction).flex_wrap().gap(px(8.0));
                    for (index, variant) in variants.iter().enumerate() {
                        let interactive = variant.definition.is_some() && !variant.saved && !busy;
                        let checked = variant.selected || variant.saved;
                        let card = gpui_kit::base::Button::new(SharedString::from(format!(
                            "theme-variant-{index}"
                        )))
                        .accessibility_label(variant.name.clone())
                        .role(Role::CheckBox)
                        .aria_toggled(if checked {
                            gpui_kit::Toggled::True
                        } else {
                            gpui_kit::Toggled::False
                        })
                        .disabled(!interactive)
                        .w(gpui_kit::relative(0.31))
                        .h_auto()
                        .p(px(6.0))
                        .rounded(px(12.0))
                        .child(
                            div()
                                .w_full()
                                .flex()
                                .flex_col()
                                .gap(px(6.0))
                                .children(variant.image.as_ref().map(|image| {
                                    img(image.clone())
                                        .w_full()
                                        .aspect_ratio(1.6)
                                        .rounded(px(8.0))
                                }))
                                .child(
                                    div()
                                        .flex_row_from(direction)
                                        .gap(px(8.0))
                                        .items_center()
                                        .child(glyph(
                                            if checked { "check" } else { "palette" },
                                            14.0,
                                            if variant.saved {
                                                p.success
                                            } else {
                                                p.muted_foreground
                                            },
                                        ))
                                        .child(
                                            div()
                                                .min_w_0()
                                                .flex_1()
                                                .truncate()
                                                .child(variant.name.clone()),
                                        ),
                                )
                                .children(variant.saved.then(|| {
                                    div()
                                        .text_color(p.success)
                                        .child(strings.get("settings.themeImport.installed"))
                                }))
                                .children(variant.definition.is_none().then(|| {
                                    div()
                                        .text_color(p.error)
                                        .child(strings.get("settings.themeImport.error.invalid"))
                                })),
                        );
                        grid = grid.child(on_activate(card, cx, move |this, _, cx| {
                            if interactive && let Page::Variants { variants, .. } = &mut this.page {
                                variants[index].selected = !variants[index].selected;
                                cx.notify();
                            }
                        }));
                    }
                    body = body.child(grid);
                }
                _ => {}
            }
        }
        let mut footer = div()
            .flex_row_from(direction)
            .items_center()
            .justify_between()
            .child(self.button(
                "theme-catalog-file",
                strings.get("settings.themeImport.chooseFile"),
                !busy,
                cx,
                |this, window, cx| this.pick_file(window, cx),
            ));
        if let Page::Variants { variants, .. } = &self.page {
            let selected = variants
                .iter()
                .any(|v| v.selected && !v.saved && v.definition.is_some());
            footer = footer.child(self.button(
                "theme-catalog-import",
                strings.get(if self.saving {
                    "settings.themeImport.busy"
                } else {
                    "settings.themeImport.importSelected"
                }),
                selected && !busy,
                cx,
                |this, window, cx| this.install(window, cx),
            ));
        }
        div()
            .css_text(0.875, 1.25, cx)
            .text_color(p.foreground)
            .flex()
            .flex_col()
            .gap(px(12.0))
            .child(header)
            .child(body)
            .child(
                div()
                    .id("theme-catalog-import-status")
                    .h(px(20.0))
                    .role(Role::Alert)
                    .truncate()
                    .text_color(p.error)
                    .children(
                        self.partial
                            .then(|| strings.get("settings.themeImport.partialError")),
                    ),
            )
            .child(footer)
    }
}

fn variants(sources: Vec<ThemeSource>, author: &str) -> Vec<Variant> {
    let mut seen_palettes = HashSet::new();
    let mut seen_names = HashSet::new();
    sources
        .into_iter()
        .filter_map(|source| {
            let ready = (!source.error)
                .then(|| import_vscode_theme(&source.text, &source.path).ok())
                .flatten()
                .and_then(|mut definition| {
                    definition["metadata"]["name"] = Value::String(source.name.clone());
                    definition["metadata"]["author"] = Value::String(author.to_owned());
                    theme::from_definition(&definition)
                        .ok()
                        .map(|theme| (definition, theme))
                });
            if let Some((definition, theme)) = ready {
                let key = format!(
                    "{}:{}",
                    definition["metadata"]["variant"], definition["colors"]
                );
                let name = format!(
                    "{}:{}",
                    definition["metadata"]["variant"],
                    crate::theme_import::catalog_base_name(&source.name)
                );
                if seen_palettes.contains(&key) || seen_names.contains(&name) {
                    return None;
                }
                seen_palettes.insert(key);
                seen_names.insert(name);
                Some(Variant {
                    name: source.name,
                    image: Some(schematic::image(&theme.palette, None)),
                    definition: Some(definition),
                    selected: true,
                    saved: false,
                })
            } else {
                Some(Variant {
                    name: source.name,
                    image: None,
                    definition: None,
                    selected: false,
                    saved: false,
                })
            }
        })
        .collect()
}
