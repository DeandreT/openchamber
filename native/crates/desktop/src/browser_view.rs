//! The context panel's browser surface (`BrowserPane.tsx`,
//! `BrowserToolbar.tsx`).
//!
//! Each project keeps its own tabs. A tab's page is a [`HostedWebView`] in
//! the browser profile, which every project and runtime shares, as Electron
//! shares `persist:openchamber-browser`; the desktop's own storage and
//! credentials are never part of it. A tab without an address has no view
//! yet and shows the empty state. Typed addresses pass
//! [`web_policy::normalize_browser_url`], so only http(s) loads start.

use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::{
    AnyElement, AppContext, Context, Entity, FocusHandle, Focusable, InteractiveElement,
    IntoElement, ParentElement, Render, Role, SharedString, StatefulInteractiveElement, Styled,
    Subscription, TestSupportExt, Window, div, prelude::FluentBuilder, px,
};

use crate::activation::on_activate;
use crate::baseline::CssTextExt;
use crate::chrome::{CONTROL_RADIUS, IconButton, glyph};
use crate::context_panel::{ROW_LINE, ROW_SIZE, XS_LINE, XS_SIZE};
use crate::locale::{self, Direction};
use crate::logical::LogicalStyledExt;
use crate::theme::Palette;
use crate::web_policy::{self, ViewPolicy};
use crate::webview_host::{HostElement, HostedWebView, Load, ViewSpec, WebViewEvent};
use crate::webview_platform::Profile;

const HEADER_HEIGHT: f32 = 40.0;
const TOOLBAR_HEIGHT: f32 = 32.0;
const TAB_MAX_WIDTH: f32 = 180.0;
/// Chromium's zoom levels: each step is 1.2 to the power of half a level.
const ZOOM_STEP: f64 = 0.5;
const ZOOM_MIN: f64 = -3.0;
const ZOOM_MAX: f64 = 4.0;

/// Why a tab shows a notice in place of its page.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Failure {
    LoadFailed,
    Crashed,
    Unavailable,
}

struct BrowserTab {
    id: u64,
    host: Option<Entity<HostedWebView>>,
    url: String,
    title: String,
    loading: bool,
    back: bool,
    forward: bool,
    zoom_level: f64,
    failure: Option<Failure>,
    _events: Option<Subscription>,
}

impl BrowserTab {
    fn empty(id: u64) -> Self {
        Self {
            id,
            host: None,
            url: String::new(),
            title: String::new(),
            loading: false,
            back: false,
            forward: false,
            zoom_level: 0.0,
            failure: None,
            _events: None,
        }
    }

    /// The tab's name: the page's host, else its title, else the surface's.
    fn label(&self, fallback: &SharedString) -> SharedString {
        let host = web_policy::browser_url_label(&self.url);
        if !host.is_empty() {
            return host.into();
        }
        if !self.title.is_empty() {
            return self.title.clone().into();
        }
        fallback.clone()
    }
}

pub(crate) struct BrowserPanel {
    tabs: Vec<BrowserTab>,
    selected: usize,
    next_id: u64,
    address: Entity<InputState>,
    focus: FocusHandle,
    _address_events: Subscription,
}

impl BrowserPanel {
    pub(crate) fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let address = cx.new(|cx| InputState::new(window, cx));
        let address_events = cx.subscribe_in(&address, window, |this, input, event, window, cx| {
            if let InputEvent::PressEnter { .. } = event {
                let typed = input.read(cx).value().to_string();
                this.submit(&typed, window, cx);
            }
        });
        Self {
            tabs: vec![BrowserTab::empty(0)],
            selected: 0,
            next_id: 1,
            address,
            focus: cx.focus_handle(),
            _address_events: address_events,
        }
    }

    fn current(&self) -> &BrowserTab {
        &self.tabs[self.selected]
    }

    /// Loads what the user typed into the selected tab.
    fn submit(&mut self, typed: &str, window: &mut Window, cx: &mut Context<Self>) {
        let url = web_policy::normalize_browser_url(typed);
        self.set_address(&url, window, cx);
        let index = self.selected;
        self.load(index, &url, window, cx);
    }

    fn load(&mut self, index: usize, url: &str, window: &mut Window, cx: &mut Context<Self>) {
        let tab = &mut self.tabs[index];
        tab.failure = None;
        match &tab.host {
            Some(host) => {
                host.read(cx).load_url(url);
            }
            None => {
                if url == web_policy::BLANK_URL {
                    cx.notify();
                    return;
                }
                let spec = ViewSpec {
                    policy: ViewPolicy::Browser,
                    profile: Profile::Browser,
                    load: Load::Address(url.to_owned()),
                    forward_wheel: false,
                    developer_tools: true,
                };
                match HostedWebView::build(spec, window, cx) {
                    Ok(host) => {
                        let id = tab.id;
                        tab._events = Some(cx.subscribe_in(
                            &host,
                            window,
                            move |this, _, event, window, cx| {
                                this.page_event(id, event, window, cx);
                            },
                        ));
                        tab.url = url.to_owned();
                        tab.host = Some(host);
                    }
                    Err(error) => {
                        tracing::warn!(%error, "a browser tab has no page");
                        tab.failure = Some(Failure::Unavailable);
                    }
                }
            }
        }
        cx.notify();
    }

    fn page_event(
        &mut self,
        id: u64,
        event: &WebViewEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(index) = self.tabs.iter().position(|tab| tab.id == id) else {
            return;
        };
        let selected = index == self.selected;
        match event {
            WebViewEvent::UrlChanged(url) => {
                self.tabs[index].url = url.clone();
                if selected && !self.address.read(cx).focus_handle(cx).is_focused(window) {
                    let shown = if url == web_policy::BLANK_URL {
                        ""
                    } else {
                        url.as_str()
                    };
                    self.set_address(shown, window, cx);
                }
            }
            WebViewEvent::TitleChanged(title) => self.tabs[index].title = title.clone(),
            WebViewEvent::LoadingChanged => {
                let tab = &mut self.tabs[index];
                if let Some(host) = &tab.host {
                    let host = host.read(cx);
                    tab.loading = host.is_loading();
                    tab.back = host.can_go_back();
                    tab.forward = host.can_go_forward();
                }
                if tab.loading && tab.failure == Some(Failure::LoadFailed) {
                    tab.failure = None;
                    self.show_page(index, cx);
                }
            }
            WebViewEvent::LoadFailed(_) => {
                self.tabs[index].failure = Some(Failure::LoadFailed);
                self.show_page(index, cx);
            }
            WebViewEvent::Crashed => {
                self.tabs[index].failure = Some(Failure::Crashed);
                self.show_page(index, cx);
            }
            WebViewEvent::OpenTab { url, background } => {
                self.open_tab(index + 1, url, !background, window, cx);
            }
            WebViewEvent::Blocked(url) => {
                tracing::info!(%url, "the browser refused a navigation");
            }
            WebViewEvent::Message(_) | WebViewEvent::PageClicked | WebViewEvent::PointerOver(_) => {
            }
        }
        cx.notify();
    }

    fn set_address(&self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        let text = text.to_owned();
        self.address
            .update(cx, |input, cx| input.set_value(text, window, cx));
    }

    /// A new tab at `index`, loading `url`; selected unless opened behind.
    fn open_tab(
        &mut self,
        index: usize,
        url: &str,
        select: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let id = self.next_id;
        self.next_id += 1;
        let index = index.min(self.tabs.len());
        self.tabs.insert(index, BrowserTab::empty(id));
        if index <= self.selected && !select {
            self.selected += 1;
        }
        self.load(index, url, window, cx);
        if select {
            self.select(index, window, cx);
        } else {
            self.show_page(index, cx);
        }
    }

    fn new_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let id = self.next_id;
        self.next_id += 1;
        self.tabs.push(BrowserTab::empty(id));
        let index = self.tabs.len() - 1;
        self.select(index, window, cx);
        self.address.update(cx, |input, cx| input.focus(window, cx));
    }

    fn select(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.selected = index;
        for index in 0..self.tabs.len() {
            self.show_page(index, cx);
        }
        let url = self.current().url.clone();
        let shown = if url == web_policy::BLANK_URL {
            String::new()
        } else {
            url
        };
        self.set_address(&shown, window, cx);
        cx.notify();
    }

    /// Maps the selected tab's page and unmaps the rest at once, rather
    /// than when the next frame no longer paints them.
    fn show_page(&self, index: usize, cx: &mut Context<Self>) {
        let tab = &self.tabs[index];
        let shown = index == self.selected && tab.failure.is_none();
        if let Some(host) = &tab.host {
            host.update(cx, |host, _| host.set_owner_visible(shown));
        }
    }

    /// Closes a tab. The last tab is emptied, so the surface always has one.
    fn close_tab(&mut self, id: u64, window: &mut Window, cx: &mut Context<Self>) {
        let Some(index) = self.tabs.iter().position(|tab| tab.id == id) else {
            return;
        };
        self.tabs.remove(index);
        if self.tabs.is_empty() {
            let id = self.next_id;
            self.next_id += 1;
            self.tabs.push(BrowserTab::empty(id));
        }
        if index < self.selected || self.selected >= self.tabs.len() {
            self.selected = self.selected.saturating_sub(1);
        }
        let selected = self.selected;
        self.select(selected, window, cx);
    }

    fn with_host(&self, cx: &mut Context<Self>, action: impl FnOnce(&mut HostedWebView)) {
        if let Some(host) = &self.current().host {
            host.update(cx, |host, _| action(host));
        }
    }

    fn zoom_by(&mut self, steps: f64, cx: &mut Context<Self>) {
        let index = self.selected;
        let level = if steps == 0.0 {
            0.0
        } else {
            (self.tabs[index].zoom_level + steps * ZOOM_STEP).clamp(ZOOM_MIN, ZOOM_MAX)
        };
        self.tabs[index].zoom_level = level;
        self.with_host(cx, |host| host.set_zoom(1.2_f64.powf(level)));
        cx.notify();
    }

    fn zoom_percent(&self) -> u32 {
        (1.2_f64.powf(self.current().zoom_level) * 100.0).round() as u32
    }

    fn open_external(&mut self, cx: &mut Context<Self>) {
        let url = self.current().url.clone();
        if ViewPolicy::Browser.allows_load(&url) && url != web_policy::BLANK_URL {
            cx.open_url(&url);
        }
    }

    fn render_tabs(
        &self,
        palette: &Palette,
        direction: Direction,
        cx: &Context<Self>,
    ) -> AnyElement {
        let strings = locale::strings(cx);
        let fallback = strings.get("contextPanel.mode.browser");
        let tabs = self.tabs.iter().enumerate().map(|(index, tab)| {
            let label = tab.label(&fallback);
            let selected = index == self.selected;
            let id = tab.id;
            let close_label = strings.format(
                "contextPanel.tab.closeTabAria",
                &[("label", label.as_ref())],
            );
            let body = on_activate(
                div()
                    .id(("browser-tab", id))
                    .role(Role::Tab)
                    .accessibility_id(SharedString::from(format!("browser-tab-{id}")))
                    .aria_label(label.clone())
                    .aria_selected(selected)
                    .h(px(28.0))
                    .max_w(px(TAB_MAX_WIDTH))
                    .min_w_0()
                    .padding_start(direction, px(8.0))
                    .padding_end(direction, px(4.0))
                    .rounded(px(CONTROL_RADIUS))
                    .flex_row_from(direction)
                    .items_center()
                    .gap(px(4.0))
                    .cursor_pointer()
                    .when(selected, |this| this.bg(palette.active))
                    .child(glyph("global", 14.0, palette.muted_foreground))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .css_text(XS_SIZE, XS_LINE, cx)
                            .text_color(if selected {
                                palette.foreground
                            } else {
                                palette.muted_foreground
                            })
                            .child(label),
                    )
                    .child(
                        IconButton::new(
                            SharedString::from(format!("browser-tab-close-{id}")),
                            "close",
                            close_label,
                        )
                        .size(18.0, 12.0)
                        .render(
                            palette,
                            cx,
                            move |this: &mut Self, window, cx| this.close_tab(id, window, cx),
                        ),
                    ),
                cx,
                move |this: &mut Self, window, cx| {
                    if let Some(index) = this.tabs.iter().position(|tab| tab.id == id) {
                        this.select(index, window, cx);
                    }
                },
            )
            .test_support();
            body.into_any_element()
        });
        div()
            .id("browser-tabs")
            .role(Role::TabList)
            .accessibility_id("browser-tabs")
            .aria_label(fallback.clone())
            .h(px(HEADER_HEIGHT))
            .px(px(6.0))
            .flex_row_from(direction)
            .items_center()
            .gap(px(2.0))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .flex_row_from(direction)
                    .items_center()
                    .gap(px(2.0))
                    .children(tabs),
            )
            .child(
                IconButton::new(
                    "browser-new-tab",
                    "add",
                    strings.get("contextPanel.browser.newTab"),
                )
                .size(28.0, 14.0)
                .render(palette, cx, |this: &mut Self, window, cx| {
                    this.new_tab(window, cx)
                }),
            )
            .into_any_element()
    }

    fn render_toolbar(
        &self,
        palette: &Palette,
        direction: Direction,
        cx: &Context<Self>,
    ) -> AnyElement {
        let strings = locale::strings(cx);
        let tab = self.current();
        let has_page = tab.host.is_some();
        let button = |id: &'static str,
                      icon: &'static str,
                      key: &str,
                      enabled: bool,
                      action: fn(&mut Self, &mut Window, &mut Context<Self>)| {
            let button = IconButton::new(id, icon, strings.get(key)).size(24.0, 14.0);
            if enabled {
                button.render(palette, cx, move |this: &mut Self, window, cx| {
                    action(this, window, cx)
                })
            } else {
                div()
                    .opacity(0.4)
                    .child(button.render(palette, cx, |_: &mut Self, _, _| {}))
                    .into_any_element()
            }
        };
        let percent = self.zoom_percent();
        div()
            .id("browser-toolbar")
            .role(Role::Toolbar)
            .accessibility_id("browser-toolbar")
            .h(px(TOOLBAR_HEIGHT))
            .px(px(8.0))
            .flex_row_from(direction)
            .items_center()
            .gap(px(4.0))
            .border_b_1()
            .border_color(palette.border)
            .child(button(
                "browser-back",
                "arrow-left",
                "contextPanel.browser.back",
                tab.back,
                |this, _, cx| this.with_host(cx, |host| host.go_back()),
            ))
            .child(button(
                "browser-forward",
                "arrow-right",
                "contextPanel.browser.forward",
                tab.forward,
                |this, _, cx| this.with_host(cx, |host| host.go_forward()),
            ))
            .child(button(
                "browser-reload",
                "refresh",
                if tab.loading {
                    "contextPanel.browser.stop"
                } else {
                    "contextPanel.browser.reload"
                },
                has_page,
                |this, _, cx| {
                    let loading = this.current().loading;
                    this.with_host(cx, |host| if loading { host.stop() } else { host.reload() });
                },
            ))
            .child(
                div()
                    .id("browser-address")
                    .role(Role::ComboBox)
                    .accessibility_id("browser-address")
                    .aria_label(strings.get("contextPanel.browser.addressAria"))
                    .flex_1()
                    .min_w_0()
                    .h(px(24.0))
                    .px(px(10.0))
                    .rounded_full()
                    .bg(palette.elevated)
                    .flex()
                    .items_center()
                    .child(
                        Input::new(&self.address)
                            .appearance(false)
                            .bordered(false)
                            .text_size(gpui_kit::rems(XS_SIZE)),
                    ),
            )
            .child(button(
                "browser-zoom-out",
                "subtract",
                "contextPanel.browser.zoomOut",
                has_page,
                |this, _, cx| this.zoom_by(-1.0, cx),
            ))
            .when(percent != 100, |this| {
                this.child(
                    on_activate(
                        div()
                            .id("browser-zoom-reset")
                            .role(Role::Button)
                            .accessibility_id("browser-zoom-reset")
                            .aria_label(strings.get("contextPanel.browser.zoomReset"))
                            .px(px(6.0))
                            .h(px(24.0))
                            .rounded(px(CONTROL_RADIUS))
                            .flex()
                            .items_center()
                            .cursor_pointer()
                            .css_text(XS_SIZE, XS_LINE, cx)
                            .text_color(palette.muted_foreground)
                            .child(SharedString::from(format!("{percent}%"))),
                        cx,
                        |this: &mut Self, _, cx| this.zoom_by(0.0, cx),
                    )
                    .test_support(),
                )
            })
            .child(button(
                "browser-zoom-in",
                "add",
                "contextPanel.browser.zoomIn",
                has_page,
                |this, _, cx| this.zoom_by(1.0, cx),
            ))
            .child(button(
                "browser-devtools",
                "terminal-box",
                "contextPanel.browser.devTools",
                has_page,
                |this, _, cx| this.with_host(cx, |host| host.open_developer_tools()),
            ))
            .child(button(
                "browser-open-external",
                "external-link",
                "contextPanel.browser.openExternal",
                has_page && tab.url != web_policy::BLANK_URL,
                |this, _, cx| this.open_external(cx),
            ))
            .into_any_element()
    }

    fn render_body(&self, palette: &Palette, cx: &Context<Self>) -> AnyElement {
        let strings = locale::strings(cx);
        let tab = self.current();
        let notice = |title: &str, hint: &str| {
            div()
                .size_full()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap(px(4.0))
                .p(px(16.0))
                .child(
                    div()
                        .css_text(ROW_SIZE, ROW_LINE, cx)
                        .text_color(palette.foreground)
                        .child(strings.get(title)),
                )
                .child(
                    div()
                        .css_text(XS_SIZE, XS_LINE, cx)
                        .text_color(palette.muted_foreground)
                        .child(strings.get(hint)),
                )
                .into_any_element()
        };
        match (tab.failure, &tab.host) {
            (Some(Failure::Crashed), _) => notice(
                "contextPanel.browser.crashed",
                "contextPanel.browser.crashedHint",
            ),
            (Some(_), _) => notice(
                "contextPanel.browser.loadFailed",
                "contextPanel.browser.loadFailedUnknown",
            ),
            (None, Some(host)) => div()
                .id("browser-page")
                .role(Role::Document)
                .accessibility_id("browser-page")
                .aria_label(strings.get("contextPanel.browser.frameTitle"))
                .size_full()
                .child(HostElement::new(host.clone()))
                .into_any_element(),
            (None, None) => notice(
                "contextPanel.browser.empty",
                "contextPanel.browser.emptyHint",
            ),
        }
    }
}

impl Focusable for BrowserPanel {
    fn focus_handle(&self, _: &gpui_kit::App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for BrowserPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = cx.global::<Palette>().clone();
        let direction = locale::strings(cx).direction();
        div()
            .id("browser-panel")
            .role(Role::Group)
            .accessibility_id("browser-panel")
            .aria_label(locale::strings(cx).get("contextPanel.mode.browser"))
            .track_focus(&self.focus)
            .size_full()
            .flex()
            .flex_col()
            .child(self.render_tabs(&palette, direction, cx))
            .child(self.render_toolbar(&palette, direction, cx))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .relative()
                    .child(self.render_body(&palette, cx)),
            )
    }
}
