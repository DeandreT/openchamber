//! About readouts are scoped to the server this dialog was opened against.

use gpui_kit::component::WindowExt;
use gpui_kit::{
    AppContext, Context, Entity, InteractiveElement, IntoElement, ParentElement, Render, Role,
    StatefulInteractiveElement, Styled, Task, TestSupportExt, WeakEntity, Window, div, px, svg,
};

use crate::activation::on_activate;
use crate::baseline::CssTextExt;
use crate::locale;
use crate::logical::LogicalStyledExt;
use crate::theme::Palette;
use crate::workspace::Workspace;

enum Version {
    Loading,
    Known(String),
    Unknown,
}

struct About {
    owner: WeakEntity<Workspace>,
    generation: Option<u64>,
    app: Version,
    engine: Version,
    _task: Option<Task<()>>,
    links: [gpui_kit::FocusHandle; 3],
}

impl Workspace {
    pub(crate) fn open_about(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let owner = cx.weak_entity();
        let generation = self
            .connection
            .as_ref()
            .map(|connection| connection.generation);
        let reading = self.connection.as_ref().and_then(|connection| {
            connection.client.clone().map(|client| {
                connection
                    .network
                    .run(async move { client.system_versions().await })
            })
        });
        let about = cx.new(|cx: &mut Context<About>| {
            let mut about = About {
                owner,
                generation,
                app: Version::Known(env!("CARGO_PKG_VERSION").to_owned()),
                engine: Version::Unknown,
                _task: None,
                links: std::array::from_fn(|_| cx.focus_handle().tab_index(0).tab_stop(true)),
            };
            if let Some(reading) = reading {
                about.app = Version::Loading;
                about.engine = Version::Loading;
                about._task = Some(cx.spawn(async move |this, cx| {
                    let result = reading.await;
                    let _ = this.update(cx, |this, cx| {
                        let current = this.owner.upgrade().is_some_and(|owner| {
                            owner
                                .read(cx)
                                .connection
                                .as_ref()
                                .map(|connection| connection.generation)
                                == this.generation
                        });
                        let (app, engine) = result
                            .map(|versions| (versions.openchamber.ok(), versions.opencode.ok()))
                            .unwrap_or_default();
                        this.app = if current {
                            Version::Known(
                                app.unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_owned()),
                            )
                        } else {
                            Version::Unknown
                        };
                        this.engine = if current {
                            engine.map_or(Version::Unknown, Version::Known)
                        } else {
                            Version::Unknown
                        };
                        cx.notify();
                    });
                }));
            }
            about
        });
        show(about, window, cx);
    }
}

fn show(about: Entity<About>, window: &mut Window, cx: &mut gpui_kit::App) {
    window.open_dialog(cx, move |dialog, window, _| {
        let height = px(310.0);
        dialog
            .w(px(320.0))
            .h(height)
            .p(px(24.0))
            .margin_top((window.viewport_size().height - height) / 2.0)
            .child(about.clone())
    });
}

impl Render for About {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let strings = locale::strings(cx);
        let palette = cx.global::<Palette>();
        let foreground = palette.foreground;
        let value = |version: &Version| match version {
            Version::Loading => strings.get("common.loading").to_string(),
            Version::Known(value) => value.clone(),
            Version::Unknown => strings
                .get("settings.openchamber.about.state.unknown")
                .to_string(),
        };
        let link = |index: usize,
                    id: &'static str,
                    icon: &'static str,
                    label: &'static str,
                    url: &'static str| {
            on_activate(
                div()
                    .id(id)
                    .test_support()
                    .accessibility_id(id)
                    .aria_label(label)
                    .role(Role::Link)
                    .track_focus(&self.links[index])
                    .tab_index(0)
                    .h(px(24.0))
                    .px(px(6.0))
                    .whitespace_nowrap()
                    .text_color(palette.muted_foreground)
                    .cursor_pointer()
                    .hover(move |style| style.text_color(foreground))
                    .child(
                        div()
                            .flex_row_from(strings.direction())
                            .items_center()
                            .gap(px(6.0))
                            .child(crate::chrome::glyph(icon, 16.0, palette.muted_foreground))
                            .child(label),
                    ),
                cx,
                move |_, window, cx| crate::markdown_links::open_user_link(url, window, cx),
            )
            .on_action(cx.listener(
                move |_, action: &gpui_kit::base::actions::Confirm, window, cx| {
                    if !action.secondary {
                        crate::markdown_links::open_user_link(url, window, cx);
                        cx.stop_propagation();
                    }
                },
            ))
        };
        div()
            .id("about-dialog")
            .accessibility_id("about-dialog")
            .role(Role::Group)
            .aria_label(strings.get("sessions.sidebar.footer.actions.aboutOpenChamber"))
            .size_full()
            .flex()
            .flex_col()
            .items_center()
            .text_center()
            .gap(px(16.0))
            .css_text(0.8125, 1.25, cx)
            .text_color(palette.foreground)
            .child(
                div()
                    .id("about-logo")
                    .accessibility_id("about-logo")
                    .role(Role::Image)
                    .aria_label(strings.get("openChamberLogo.aria.logo"))
                    .size(px(64.0))
                    .child(
                        svg()
                            .path(crate::assets::APP_LOGO)
                            .size(px(64.0))
                            .text_color(palette.foreground),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(4.0))
                    .child(
                        div()
                            .id("about-title")
                            .role(Role::Heading)
                            .css_text(1.125, 1.75, cx)
                            .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                            .child("OpenChamber"),
                    )
                    .child(
                        div()
                            .css_text(0.8125, 1.25, cx)
                            .text_color(palette.muted_foreground)
                            .child(
                                div()
                                    .id("about-app-version")
                                    .accessibility_id("about-app-version")
                                    .role(Role::Paragraph)
                                    .h(px(20.0))
                                    .aria_label(strings.format(
                                        "aboutDialog.openChamberVersionLabel",
                                        &[("version", &value(&self.app))],
                                    ))
                                    .child(strings.format(
                                        "aboutDialog.openChamberVersionLabel",
                                        &[("version", &value(&self.app))],
                                    )),
                            )
                            .child(
                                div()
                                    .id("about-engine-version")
                                    .accessibility_id("about-engine-version")
                                    .role(Role::Paragraph)
                                    .h(px(20.0))
                                    .aria_label(strings.format(
                                        "aboutDialog.openCodeVersionLabel",
                                        &[("version", &value(&self.engine))],
                                    ))
                                    .child(strings.format(
                                        "aboutDialog.openCodeVersionLabel",
                                        &[("version", &value(&self.engine))],
                                    )),
                            ),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap(px(4.0))
                    .child(
                        div()
                            .flex()
                            .gap(px(8.0))
                            .child(link(
                                0,
                                "about-github",
                                "github-fill",
                                "GitHub",
                                "https://github.com/openchamber/openchamber",
                            ))
                            .child(link(
                                1,
                                "about-discord",
                                "discord-fill",
                                "Discord",
                                "https://discord.gg/ZYRSdnwwKA",
                            )),
                    )
                    .child(link(
                        2,
                        "about-social",
                        "twitter-xfill",
                        "@openchamber_dev",
                        "https://x.com/openchamber_dev",
                    )),
            )
            .child(
                div()
                    .w_full()
                    .css_text(0.8125, 1.25, cx)
                    .text_color(palette.muted_foreground)
                    .child(strings.get("aboutDialog.footerNote")),
            )
    }
}

#[cfg(test)]
mod tests {
    use gpui_kit::test::TestWindowExt;
    use gpui_kit::{AppContext, ElementId, TestAppContext};

    #[gpui_kit::test]
    fn about_links_are_tab_stops_and_enter_opens_the_expected_url(cx: &mut TestAppContext) {
        let (window, workspace) = crate::workspace_tests::open(cx);
        cx.update_window(window, |_, window, cx| {
            workspace.update(cx, |workspace, cx| workspace.open_about(window, cx))
        })
        .expect("window");
        cx.update_window(window, |_, window, cx| {
            for _ in 0..6 {
                window.press("tab", cx);
                window.render_frame(cx);
                if window
                    .find(ElementId::Name("about-github".into()))
                    .focused()
                    == Some(true)
                {
                    break;
                }
            }
            assert_eq!(
                window
                    .find(ElementId::Name("about-github".into()))
                    .focused(),
                Some(true)
            );
        })
        .expect("window");
        for (id, url) in [
            ("about-github", "https://github.com/openchamber/openchamber"),
            ("about-discord", "https://discord.gg/ZYRSdnwwKA"),
            ("about-social", "https://x.com/openchamber_dev"),
        ] {
            cx.update_window(window, |_, window, cx| {
                window.render_frame(cx);
                assert_eq!(
                    window.find(ElementId::Name(id.into())).focused(),
                    Some(true),
                    "Tab reaches {id}"
                );
                window.press("enter", cx);
            })
            .expect("window");
            assert_eq!(cx.opened_url().as_deref(), Some(url));
            cx.update_window(window, |_, window, cx| window.press("tab", cx))
                .expect("window");
        }
    }
}
