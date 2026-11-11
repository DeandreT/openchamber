//! Window-owned Settings navigation; page models own their edits and writes.

use gpui_kit::component::WindowExt;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::{
    App, AppContext, Context, Entity, EntityInputHandler, FocusHandle, FontWeight,
    InteractiveElement, IntoElement, ParentElement, Render, Role, ScrollHandle, SharedString,
    StatefulInteractiveElement, Styled, Subscription, Task, TestSupportExt, WeakEntity, Window,
    div, prelude::FluentBuilder, px,
};

use crate::activation::on_activate;
use crate::baseline::CssTextExt;
use crate::host_manager::HostManager;
use crate::locale;
use crate::logical::LogicalStyledExt;
use crate::preference_pages::PreferencePage;
use crate::settings_navigation::{self, Page, SearchItem};
use crate::theme::Palette;
use crate::workspace::Workspace;

pub(crate) struct Settings {
    owner: WeakEntity<Workspace>,
    hosts: Entity<HostManager>,
    pub(crate) general: Entity<crate::general_settings::GeneralSettings>,
    appearance: Entity<crate::appearance_view::AppearanceView>,
    pub(crate) chat: Entity<PreferencePage>,
    pub(crate) sessions: Entity<PreferencePage>,
    notifications: Entity<PreferencePage>,
    voice: Entity<PreferencePage>,
    usage: Entity<crate::usage_page::UsagePage>,
    shortcuts: Entity<crate::shortcuts_settings::ShortcutsPage>,
    pub(crate) behavior: Entity<crate::behavior_settings::BehaviorPage>,
    pub(crate) snippets: Entity<crate::snippets_settings::SnippetsPage>,
    pub(crate) commands: Entity<crate::commands_settings::CommandsPage>,
    pub(crate) about: Entity<crate::about_settings::AboutPage>,
    pub(crate) git: Entity<crate::git_settings::GitSettingsPage>,
    pub(crate) magic_prompts: Entity<crate::magic_prompts_settings::MagicPromptsPage>,
    page: Page,
    /// Counts openings, so a dialog that closes after a newer one opened
    /// does not hide the newer one's page.
    shows: u64,
    search: Entity<InputState>,
    active_result: usize,
    navigation_scroll: ScrollHandle,
    nav_focus: [FocusHandle; Page::ALL.len()],
    result_focus: Vec<FocusHandle>,
    _search_subscription: Subscription,
}

impl Settings {
    #[cfg(test)]
    pub(crate) fn voice_for_test(&self) -> Entity<PreferencePage> {
        self.voice.clone()
    }

    /// Captures valid retained editor text before preference writers are frozen.
    pub(crate) fn prepare_close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.appearance
            .update(cx, |appearance, cx| appearance.commit_size(window, cx));
        self.behavior
            .update(cx, |page, cx| page.prepare_close(window, cx));
        self.commands
            .update(cx, |page, cx| page.prepare_close(window, cx));
    }

    pub(crate) fn take_pending_write(&mut self, cx: &mut Context<Self>) -> Option<Task<()>> {
        let behavior = self
            .behavior
            .update(cx, |page, cx| page.take_pending_write(cx));
        let defaults = self
            .sessions
            .read(cx)
            .defaults
            .clone()
            .and_then(|defaults| defaults.update(cx, |page, cx| page.take_pending_write(cx)));
        let voice = self
            .voice
            .update(cx, |page, cx| page.take_voice_pending_write(cx));
        let snippets = self
            .snippets
            .update(cx, |page, cx| page.take_pending_write(cx));
        let commands = self
            .commands
            .update(cx, |page, cx| page.take_pending_write(cx));
        let magic_prompts = self
            .magic_prompts
            .update(cx, |page, cx| page.take_pending_write(cx));
        let git = self.git.update(cx, |page, cx| page.take_pending_write(cx));
        let pending: Vec<_> = [
            behavior,
            defaults,
            voice,
            snippets,
            commands,
            magic_prompts,
            git,
        ]
        .into_iter()
        .flatten()
        .collect();
        if pending.is_empty() {
            return None;
        }
        Some(cx.spawn(async move |_, _| {
            for pending in pending {
                let _ = pending.await;
            }
        }))
    }

    pub(crate) fn has_failed_writes(&self, cx: &App) -> bool {
        self.behavior.read(cx).has_failed_writes()
            || self.voice.read(cx).voice_has_failed_writes()
            || self.snippets.read(cx).has_failed_writes()
            || self.commands.read(cx).has_failed_writes()
            || self.magic_prompts.read(cx).has_failed_writes()
            || self.git.read(cx).has_failed_writes()
            || self
                .sessions
                .read(cx)
                .defaults
                .as_ref()
                .is_some_and(|defaults| defaults.read(cx).has_failed_writes())
    }

    pub(crate) fn cancel_close(&mut self, cx: &mut Context<Self>) {
        self.behavior.update(cx, |page, cx| page.cancel_close(cx));
        self.voice
            .update(cx, |page, cx| page.cancel_voice_close(cx));
        self.snippets.update(cx, |page, cx| page.cancel_close(cx));
        self.commands.update(cx, |page, cx| page.cancel_close(cx));
        self.magic_prompts
            .update(cx, |page, cx| page.cancel_close(cx));
        self.git.update(cx, |page, cx| page.cancel_close(cx));
        if let Some(defaults) = self.sessions.read(cx).defaults.clone() {
            defaults.update(cx, |page, cx| page.cancel_close(cx));
        }
    }

    pub(crate) fn take_quit_write(&mut self, cx: &mut Context<Self>) -> Option<Task<()>> {
        let behavior = self
            .behavior
            .update(cx, |page, cx| page.take_quit_write(cx));
        let defaults = self
            .sessions
            .read(cx)
            .defaults
            .clone()
            .and_then(|defaults| defaults.update(cx, |page, cx| page.take_quit_write(cx)));
        let voice = self
            .voice
            .update(cx, |page, cx| page.take_voice_quit_write(cx));
        let snippets = self
            .snippets
            .update(cx, |page, cx| page.take_quit_write(cx));
        let commands = self
            .commands
            .update(cx, |page, cx| page.take_quit_write(cx));
        let magic_prompts = self
            .magic_prompts
            .update(cx, |page, cx| page.take_quit_write(cx));
        let git = self.git.update(cx, |page, cx| page.take_quit_write(cx));
        let pending: Vec<_> = [
            behavior,
            defaults,
            voice,
            snippets,
            commands,
            magic_prompts,
            git,
        ]
        .into_iter()
        .flatten()
        .collect();
        if pending.is_empty() {
            return None;
        }
        Some(cx.background_executor().spawn(async move {
            for pending in pending {
                pending.await;
            }
        }))
    }

    pub(crate) fn general_preferences(
        &self,
        runtime: &openchamber_native_core::ids::RuntimeId,
        cx: &App,
    ) -> Option<crate::general_settings::GeneralPreferences> {
        self.general.read(cx).preferences(runtime)
    }

    fn new(
        owner: WeakEntity<Workspace>,
        hosts: Entity<HostManager>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let search = cx.new(|cx| InputState::new(window, cx));
        let subscription =
            cx.subscribe_in(&search, window, |this, _, event, window, cx| match event {
                InputEvent::Change => {
                    this.active_result = 0;
                    this.navigation_scroll
                        .set_offset(gpui_kit::point(px(0.0), px(0.0)));
                    cx.notify();
                }
                InputEvent::PressEnter { .. } if !this.composing(window, cx) => {
                    this.open_active_result(window, cx);
                }
                _ => {}
            });
        let general =
            cx.new(|cx| crate::general_settings::GeneralSettings::new(owner.clone(), window, cx));
        let appearance = cx.new(|cx| crate::appearance_view::AppearanceView::new(window, cx));
        let chat = cx.new(|cx| PreferencePage::new(owner.clone(), general.clone(), Page::Chat, cx));
        let sessions =
            cx.new(|cx| PreferencePage::new(owner.clone(), general.clone(), Page::Sessions, cx));
        let defaults =
            cx.new(|cx| crate::session_defaults::SessionDefaults::new(owner.clone(), window, cx));
        sessions.update(cx, |page, cx| page.set_defaults(defaults, cx));
        let shortcuts = cx.new(|_| crate::shortcuts_settings::ShortcutsPage::new(owner.clone()));
        let notifications = cx
            .new(|cx| PreferencePage::new(owner.clone(), general.clone(), Page::Notifications, cx));
        let voice =
            cx.new(|cx| PreferencePage::new(owner.clone(), general.clone(), Page::Voice, cx));
        voice.update(cx, |voice, cx| voice.observe_voice_target(window, cx));
        let usage = cx.new(|_| crate::usage_page::UsagePage::new(owner.clone()));
        let behavior =
            cx.new(|cx| crate::behavior_settings::BehaviorPage::new(owner.clone(), window, cx));
        let snippets = cx.new(|cx| crate::snippets_settings::SnippetsPage::new(owner.clone(), cx));
        let commands = cx.new(|cx| crate::commands_settings::CommandsPage::new(owner.clone(), cx));
        let magic_prompts =
            cx.new(|cx| crate::magic_prompts_settings::MagicPromptsPage::new(owner.clone(), cx));
        let about = cx.new(|cx| crate::about_settings::AboutPage::new(owner.clone(), cx));
        let git = cx.new(|cx| crate::git_settings::GitSettingsPage::new(owner.clone(), cx));
        Self {
            about,
            git,
            commands,
            magic_prompts,
            shortcuts,
            behavior,
            snippets,
            owner,
            hosts,
            general,
            appearance,
            chat,
            sessions,
            notifications,
            voice,
            usage,
            page: Page::default(),
            shows: 0,
            search,
            active_result: 0,
            navigation_scroll: ScrollHandle::new(),
            nav_focus: std::array::from_fn(|_| cx.focus_handle().tab_index(0).tab_stop(true)),
            result_focus: settings_navigation::SEARCH_ITEMS
                .iter()
                .map(|_| cx.focus_handle().tab_index(0).tab_stop(true))
                .collect(),
            _search_subscription: subscription,
        }
    }

    fn composing(&self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        self.search.update(cx, |input, cx| {
            input.marked_text_range(window, cx).is_some()
        })
    }

    fn select_page(&mut self, page: Page, cx: &mut Context<Self>) {
        self.page = page;
        if page == Page::Usage {
            self.usage.update(cx, |usage, cx| usage.open(cx));
        }
        // Chat and Sessions read the same server document as General.
        if matches!(
            page,
            Page::General | Page::Chat | Page::Sessions | Page::Notifications | Page::Voice
        ) {
            let general = self.general.downgrade();
            cx.defer(move |cx| {
                let _ = general.update(cx, |page, cx| page.refresh(cx));
            });
        }
        if page == Page::Sessions
            && let Some(defaults) = self.sessions.read(cx).defaults.clone()
        {
            cx.defer(move |cx| defaults.update(cx, |defaults, cx| defaults.refresh(cx)));
        }
        // OpenCode configuration pages read again on every visit.
        if page == Page::Behavior {
            let behavior = self.behavior.downgrade();
            cx.defer(move |cx| {
                let _ = behavior.update(cx, |page, cx| page.refresh(cx));
            });
        }
        if page == Page::Snippets {
            let snippets = self.snippets.downgrade();
            cx.defer(move |cx| {
                let _ = snippets.update(cx, |page, cx| page.refresh(cx));
            });
        }
        // These pages read only while they are the visible one.
        self.show_pages(Some(page), cx);
        cx.notify();
    }

    /// Tells the pages that read on visit which one is shown; `None` when the
    /// dialog closed. Deferred, because a page may be mid-update.
    fn show_pages(&self, shown: Option<Page>, cx: &mut Context<Self>) {
        let (about, git, commands, magic_prompts) = (
            self.about.downgrade(),
            self.git.downgrade(),
            self.commands.downgrade(),
            self.magic_prompts.downgrade(),
        );
        cx.defer(move |cx| {
            let _ = about.update(cx, |page, cx| {
                page.set_shown(shown == Some(Page::About), cx)
            });
            let _ = git.update(cx, |page, cx| page.set_shown(shown == Some(Page::Git), cx));
            let _ = commands.update(cx, |page, cx| {
                page.set_shown(shown == Some(Page::Commands), cx)
            });
            let _ = magic_prompts.update(cx, |page, cx| {
                page.set_shown(shown == Some(Page::MagicPrompts), cx)
            });
        });
    }

    fn open_result(&mut self, item: &SearchItem, window: &mut Window, cx: &mut Context<Self>) {
        self.select_page(item.page, cx);
        match item.page {
            Page::General => self
                .general
                .update(cx, |page, cx| page.reveal(item.id, window, cx)),
            Page::Appearance => self
                .appearance
                .update(cx, |page, cx| page.reveal(item.id, window, cx)),
            Page::Chat => self
                .chat
                .update(cx, |page, cx| page.reveal(item.id, window, cx)),
            Page::Sessions => self
                .sessions
                .update(cx, |page, cx| page.reveal(item.id, window, cx)),
            Page::Notifications => self
                .notifications
                .update(cx, |page, cx| page.reveal(item.id, window, cx)),
            Page::Voice => self
                .voice
                .update(cx, |page, cx| page.reveal(item.id, window, cx)),
            Page::Shortcuts | Page::Usage | Page::About => {}
            Page::Git => self
                .git
                .update(cx, |page, cx| page.reveal(item.id, window, cx)),
            Page::Behavior => self
                .behavior
                .update(cx, |page, cx| page.reveal(item.id, window, cx)),
            Page::Snippets => self
                .snippets
                .update(cx, |page, cx| page.reveal(item.id, window, cx)),
            Page::Commands => self
                .commands
                .update(cx, |page, cx| page.reveal(item.id, window, cx)),
            Page::MagicPrompts => self
                .magic_prompts
                .update(cx, |page, cx| page.reveal(item.id, window, cx)),
            Page::Hosts => {
                self.hosts
                    .update(cx, |page, cx| page.focus_settings(window, cx));
            }
        }
    }

    fn open_active_result(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let results =
            settings_navigation::search(&self.search.read(cx).value(), locale::strings(cx));
        if let Some(item) = results.get(self.active_result) {
            self.open_result(item, window, cx);
        }
    }

    fn search_key(
        &mut self,
        event: &gpui_kit::KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.search.read(cx).value().trim().is_empty() || self.composing(window, cx) {
            return;
        }
        let key = &event.keystroke;
        let count =
            settings_navigation::search(&self.search.read(cx).value(), locale::strings(cx)).len();
        let delta = match key.key.as_str() {
            "down" if !key.modifiers.control && !key.modifiers.alt && !key.modifiers.platform => 1,
            "n" if key.modifiers.control && !key.modifiers.alt && !key.modifiers.platform => 1,
            "up" if !key.modifiers.control && !key.modifiers.alt && !key.modifiers.platform => -1,
            "p" if key.modifiers.control && !key.modifiers.alt && !key.modifiers.platform => -1,
            "escape" => {
                self.search
                    .update(cx, |input, cx| input.set_value("", window, cx));
                cx.stop_propagation();
                return;
            }
            _ => return,
        };
        if count != 0 {
            self.active_result =
                (self.active_result as isize + delta).rem_euclid(count as isize) as usize;
            self.navigation_scroll.scroll_to_item(self.active_result);
            cx.notify();
        }
        cx.stop_propagation();
    }

    fn navigation(&self, cx: &Context<Self>) -> impl IntoElement {
        let strings = locale::strings(cx);
        let palette = cx.global::<Palette>();
        let hover = palette.hover;
        let direction = strings.direction();
        let query = self.search.read(cx).value();
        let searching = !query.trim().is_empty();
        let results = settings_navigation::search(&query, strings);
        let mut navigation = div()
            .id("settings-pages")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .track_scroll(&self.navigation_scroll);
        if searching {
            if results.is_empty() {
                navigation = navigation.child(
                    div()
                        .p(px(12.0))
                        .text_color(palette.muted_foreground)
                        .child(strings.get("settings.view.search.noResults")),
                );
            }
            for (index, item) in results.into_iter().enumerate() {
                let focus_index = settings_navigation::SEARCH_ITEMS
                    .iter()
                    .position(|candidate| candidate.id == item.id)
                    .expect("search returns registry items");
                let label = strings.get(item.title);
                let page_title = strings.get(item.page.title());
                let id: SharedString =
                    format!("settings-result-{}", item.id.replace('.', "-")).into();
                navigation = navigation.child(
                    on_activate(
                        div()
                            .id(id.clone())
                            .accessibility_id(id)
                            .role(Role::Button)
                            .aria_label(label.clone())
                            .track_focus(&self.result_focus[focus_index])
                            .tab_index(0)
                            .px(px(12.0))
                            .py(px(8.0))
                            .rounded(px(7.0))
                            .cursor_pointer()
                            .hover(move |style| style.bg(hover))
                            .when(direction == locale::Direction::RightToLeft, |row| {
                                row.text_right()
                            })
                            .when(index == self.active_result, |row| {
                                row.bg(palette.selection)
                                    .text_color(palette.selection_foreground)
                            })
                            .child(div().css_text(0.8125, 1.25, cx).child(label))
                            .child(
                                div()
                                    .css_text(0.75, 1.0, cx)
                                    .text_color(palette.muted_foreground)
                                    .child(page_title),
                            ),
                        cx,
                        move |this, window, cx| this.open_result(item, window, cx),
                    )
                    .on_action(cx.listener(
                        move |this, action: &gpui_kit::base::actions::Confirm, window, cx| {
                            if !action.secondary {
                                this.open_result(item, window, cx);
                                cx.stop_propagation();
                            }
                        },
                    ))
                    .test_support(),
                );
            }
        } else {
            for (index, page) in Page::ALL.into_iter().enumerate() {
                if index == 0 || page.group() != Page::ALL[index - 1].group() {
                    navigation = navigation.child(
                        div()
                            .px(px(12.0))
                            .pt(px(if index == 0 { 8.0 } else { 20.0 }))
                            .pb(px(8.0))
                            .css_text(0.75, 1.0, cx)
                            .font_weight(FontWeight::SEMIBOLD)
                            .when(direction == locale::Direction::RightToLeft, |heading| {
                                heading.text_right()
                            })
                            .text_color(palette.muted_foreground)
                            .child(strings.get(page.group())),
                    );
                }
                let label = strings.get(page.title());
                let id: SharedString = format!("settings-nav-{}", page.id()).into();
                navigation = navigation.child(
                    on_activate(
                        div()
                            .id(id.clone())
                            .accessibility_id(id)
                            .role(Role::Button)
                            .aria_label(label.clone())
                            .aria_selected(self.page == page)
                            .track_focus(&self.nav_focus[index])
                            .tab_index(0)
                            .px(px(12.0))
                            .py(px(8.0))
                            .rounded(px(7.0))
                            .cursor_pointer()
                            .hover(move |style| style.bg(hover))
                            .flex_row_from(direction)
                            .items_center()
                            .gap(px(8.0))
                            .when(self.page == page, |row| {
                                row.bg(palette.selection)
                                    .text_color(palette.selection_foreground)
                            })
                            .child(crate::chrome::glyph(page.icon(), 16.0, palette.foreground))
                            .child(label),
                        cx,
                        move |this, _, cx| this.select_page(page, cx),
                    )
                    .on_action(cx.listener(
                        move |this, action: &gpui_kit::base::actions::Confirm, _, cx| {
                            if !action.secondary {
                                this.select_page(page, cx);
                                cx.stop_propagation();
                            }
                        },
                    ))
                    .test_support(),
                );
            }
        }
        navigation
    }
}

impl Workspace {
    /// Retains the shared preference owner even when Settings is not visible.
    pub(crate) fn ensure_settings_model(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<Settings> {
        let hosts = self.settings_host_manager(window, cx);
        let owner = cx.weak_entity();
        self.settings
            .get_or_insert_with(|| cx.new(|cx| Settings::new(owner, hosts, window, cx)))
            .clone()
    }

    pub(crate) fn open_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.close_in_progress() {
            return;
        }
        self.settings_host_manager(window, cx)
            .update(cx, |hosts, cx| hosts.prepare_settings(cx));
        self.show_settings(None, window, cx);
    }

    pub(crate) fn show_settings(
        &mut self,
        page: Option<Page>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.close_in_progress() {
            return;
        }
        let settings = self.ensure_settings_model(window, cx);
        settings.update(cx, |settings, cx| {
            settings.shows += 1;
            settings
                .search
                .update(cx, |input, cx| input.set_value("", window, cx));
            settings.select_page(page.unwrap_or(settings.page), cx);
        });
        show(settings.clone(), window, cx);
        settings.update(cx, |settings, cx| {
            settings
                .search
                .update(cx, |input, cx| input.focus(window, cx))
        });
    }
}

fn show(settings: Entity<Settings>, window: &mut Window, cx: &mut App) {
    window.close_dialog(cx);
    let owner = settings.read(cx).owner.clone();
    let shows = settings.read(cx).shows;
    let closing = settings.downgrade();
    window.open_dialog(cx, move |dialog, window, _| {
        let viewport = window.viewport_size();
        let height = viewport.height * 0.85;
        let owner = owner.clone();
        let closing = closing.clone();
        dialog
            .w(viewport.width * 0.9)
            .h(height)
            .margin_top((viewport.height - height) / 2.0)
            .p_0()
            .on_ok(|_, _, _| false)
            .on_close(move |_, window, cx| {
                let _ = owner.update(cx, |owner, cx| owner.focus_composer(window, cx));
                // A closed dialog shows no page, so none of them reads.
                let _ = closing.update(cx, |settings, cx| {
                    if settings.shows == shows {
                        settings.show_pages(None, cx);
                    }
                });
            })
            .content({
                let settings = settings.clone();
                move |content, _, _| {
                    content
                        .size_full()
                        .min_h_0()
                        .overflow_hidden()
                        .child(settings.clone())
                }
            })
    });
}

impl Render for Settings {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let strings = locale::strings(cx).clone();
        let palette = cx.global::<Palette>().clone();
        let placeholder = strings.get("settings.view.search.placeholder");
        if self.search.read(cx).presentation().placeholder() != &placeholder {
            self.search.update(cx, |input, cx| {
                input.set_placeholder(placeholder, window, cx)
            });
        }
        let direction = strings.direction();
        let page = match self.page {
            Page::General => self.general.clone().into_any_element(),
            Page::Appearance => self.appearance.clone().into_any_element(),
            Page::Chat => self.chat.clone().into_any_element(),
            Page::Sessions => self.sessions.clone().into_any_element(),
            Page::Shortcuts => self.shortcuts.clone().into_any_element(),
            Page::Notifications => self.notifications.clone().into_any_element(),
            Page::Voice => self.voice.clone().into_any_element(),
            Page::Usage => self.usage.clone().into_any_element(),
            Page::Behavior => self.behavior.clone().into_any_element(),
            Page::Snippets => self.snippets.clone().into_any_element(),
            Page::Commands => self.commands.clone().into_any_element(),
            Page::About => self.about.clone().into_any_element(),
            Page::Git => self.git.clone().into_any_element(),
            Page::MagicPrompts => self.magic_prompts.clone().into_any_element(),
            Page::Hosts => {
                let scroll = self.hosts.read(cx).settings_scroll.clone();
                div()
                    .id("settings-hosts-content")
                    .size_full()
                    .overflow_y_scroll()
                    .track_scroll(&scroll)
                    .px(px(48.0))
                    .py(px(32.0))
                    .child(
                        div()
                            .max_w(px(840.0))
                            .mx_auto()
                            .child(page_header(Page::Hosts, cx))
                            .child(self.hosts.clone()),
                    )
                    .into_any_element()
            }
        };
        div()
            .id("settings-dialog")
            .accessibility_id("settings-dialog")
            .role(Role::Group)
            .aria_label(strings.get("settings.window.description"))
            .size_full()
            .flex_row_from(direction)
            .overflow_hidden()
            .rounded(px(10.0))
            .css_text(0.875, 1.25, cx)
            .bg(palette.background)
            .text_color(palette.foreground)
            .child(
                div()
                    .id("settings-navigation")
                    .accessibility_id("settings-navigation")
                    .role(Role::Navigation)
                    .w(px(256.0))
                    .flex_shrink_0()
                    .h_full()
                    .p(px(16.0))
                    .flex()
                    .flex_col()
                    .gap(px(8.0))
                    .bg(palette.elevated)
                    .child(
                        div()
                            .when(direction == locale::Direction::RightToLeft, |row| {
                                row.pr(px(32.0))
                            })
                            .capture_key_down(cx.listener(Self::search_key))
                            .child(
                                Input::new(&self.search)
                                    .focus_bordered(false)
                                    .id("settings-search")
                                    .accessibility_id("settings-search")
                                    .aria_label(strings.get("settings.view.search.aria")),
                            ),
                    )
                    .child(self.navigation(cx)),
            )
            .child(
                div()
                    .id("settings-content")
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .child(page),
            )
    }
}

pub(crate) fn page_header(page: Page, cx: &App) -> gpui_kit::AnyElement {
    let strings = locale::strings(cx);
    div()
        .mb(px(32.0))
        .when(
            strings.direction() == locale::Direction::RightToLeft,
            |header| header.text_right(),
        )
        .child(
            div()
                .id("settings-current-page")
                .accessibility_id("settings-current-page")
                .role(Role::Heading)
                .aria_label(strings.get(page.title()))
                .css_text(1.125, 1.5, cx)
                .font_weight(FontWeight::SEMIBOLD)
                .child(strings.get(page.title())),
        )
        .child(
            div()
                .mt(px(4.0))
                .css_text(0.875, 1.25, cx)
                .text_color(cx.global::<Palette>().muted_foreground)
                .children(page.description().map(|key| strings.get(key))),
        )
        .into_any_element()
}
