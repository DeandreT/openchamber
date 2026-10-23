//! Window chrome shared by the sidebar, header and rail: sprite glyphs, quiet
//! icon buttons, the frameless window's drag region and its window controls.
//!
//! Geometry and hover treatment follow the web client's header and sidebar
//! (`packages/ui/src/components/layout/Header.tsx`,
//! `components/desktop/WindowsWindowControls.tsx`).

use gpui_kit::component::{InteractiveElementExt, window_paddings};
use gpui_kit::{
    AnyElement, Context, ElementId, Hsla, InteractiveElement, IntoElement, MouseButton,
    ParentElement, Pixels, Role, SharedString, Size, StatefulInteractiveElement, Styled, Svg,
    TestSupportExt, Window, WindowControlArea, div, prelude::FluentBuilder, px, svg,
};

use crate::activation::on_activate;
use crate::assets;
use crate::locale::Strings;
use crate::theme::Palette;
use crate::workspace::Workspace;

/// The line GPUI Kit's client-side frame draws around the content on Linux.
#[cfg(target_os = "linux")]
pub const FRAME_LINE: f32 = 1.0;
#[cfg(not(target_os = "linux"))]
pub const FRAME_LINE: f32 = 0.0;

/// The size of the application's content: the viewport without the
/// client-side frame's shadow band and line, which Kit draws on every side
/// the window is not tiled against.
pub fn content_size(window: &Window) -> Size<Pixels> {
    let viewport = window.viewport_size();
    let paddings = window_paddings(window);
    let frame = |padding: Pixels| {
        if padding > px(0.0) {
            padding + px(FRAME_LINE)
        } else {
            px(0.0)
        }
    };
    Size {
        width: viewport.width - frame(paddings.left) - frame(paddings.right),
        height: viewport.height - frame(paddings.top) - frame(paddings.bottom),
    }
}

/// The transcript's end gutter (`pe-8`) that holds the prompt navigation.
pub const NAVIGATION_GUTTER: f32 = 32.0;

/// `.chat-column`, `.chat-input-column` and `.chat-message-column` of
/// `packages/ui/src/styles/typography.css`: at most 48rem wide, centered.
pub const CHAT_COLUMN_MAX_WIDTH: f32 = 768.0;

/// The chat columns' inline padding: `clamp(1rem, 2.5vw, 1.5rem)` from a
/// 1,024 px wide viewport, `clamp(0.75rem, 2.5vw, 1rem)` below.
pub fn chat_column_padding(window: &Window) -> Pixels {
    let width = f32::from(content_size(window).width);
    let preferred = width * 0.025;
    px(if width >= 1024.0 {
        preferred.clamp(16.0, 24.0)
    } else {
        preferred.clamp(12.0, 16.0)
    })
}

/// Height of the title rows of the sidebar and the main column.
pub const TITLE_ROW_HEIGHT: f32 = 48.0;
/// `rounded-md` of the design system.
pub const CONTROL_RADIUS: f32 = 7.0;
const WINDOW_CONTROL_WIDTH: f32 = 44.0;
/// Group name for icon buttons, so their glyph follows the button's hover.
const BUTTON_GROUP: &str = "icon-button";

/// A sprite glyph of `size` logical pixels in `color`.
pub fn glyph(name: &str, size: f32, color: Hsla) -> Svg {
    svg()
        .path(assets::icon_path(name))
        .size(px(size))
        .flex_shrink_0()
        .text_color(color)
}

/// How a quiet icon button looks at rest and under the pointer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ButtonTone {
    /// Muted glyph that turns foreground on hover.
    Quiet,
    /// Quiet, plus a faint hover fill (`hover:bg-interactive-hover/50`).
    QuietFilled,
    /// Foreground glyph: the control's panel is open.
    Active,
    /// Dimmed glyph (`text-muted-foreground/70`), foreground on hover.
    Dim,
}

/// A square button around one glyph.
pub struct IconButton {
    pub id: SharedString,
    pub icon: &'static str,
    pub label: SharedString,
    /// Side of the square button.
    pub size: f32,
    pub icon_size: f32,
    pub tone: ButtonTone,
}

impl IconButton {
    /// A 24 px button with a 16 px glyph.
    pub fn new(id: impl Into<SharedString>, icon: &'static str, label: SharedString) -> Self {
        Self {
            id: id.into(),
            icon,
            label,
            size: 24.0,
            icon_size: 16.0,
            tone: ButtonTone::Quiet,
        }
    }

    pub fn size(mut self, size: f32, icon_size: f32) -> Self {
        self.size = size;
        self.icon_size = icon_size;
        self
    }

    pub fn tone(mut self, tone: ButtonTone) -> Self {
        self.tone = tone;
        self
    }

    /// The button with `action` on pointer and accessibility activation.
    pub fn render(
        self,
        palette: &Palette,
        cx: &Context<Workspace>,
        action: impl Fn(&mut Workspace, &mut Window, &mut Context<Workspace>) + 'static,
    ) -> AnyElement {
        let rest = match self.tone {
            ButtonTone::Quiet | ButtonTone::QuietFilled => palette.muted_foreground,
            ButtonTone::Active => palette.foreground,
            ButtonTone::Dim => palette.muted_foreground.opacity(0.7),
        };
        let hover_fill = palette.hover.opacity(0.5);
        let foreground = palette.foreground;
        let button = div()
            .id(ElementId::from(self.id.clone()))
            .group(BUTTON_GROUP)
            .role(Role::Button)
            .accessibility_id(self.id)
            .aria_label(self.label)
            .size(px(self.size))
            .flex_shrink_0()
            .rounded(px(CONTROL_RADIUS))
            .flex()
            .items_center()
            .justify_center()
            .cursor_pointer()
            .when(self.tone == ButtonTone::QuietFilled, |this| {
                this.hover(move |style| style.bg(hover_fill))
            })
            // A press here is the button's, never the start of a window move.
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(
                glyph(self.icon, self.icon_size, rest)
                    .group_hover(BUTTON_GROUP, move |style| style.text_color(foreground)),
            );
        on_activate(button, cx, action)
            .test_support()
            .into_any_element()
    }
}

/// A title row that moves the frameless window: drag to move, double-click
/// to maximize, secondary click for the window menu.
pub fn drag_region(id: &'static str, cx: &Context<Workspace>) -> gpui_kit::Stateful<gpui_kit::Div> {
    div()
        .id(id)
        .window_control_area(WindowControlArea::Drag)
        .on_double_click(|_, window, _| window.zoom_window())
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(|this, _, _, _| this.title_drag_armed = true),
        )
        .on_mouse_up(
            MouseButton::Left,
            cx.listener(|this, _, _, _| this.title_drag_armed = false),
        )
        .on_mouse_down_out(cx.listener(|this, _, _, _| this.title_drag_armed = false))
        .on_mouse_move(cx.listener(|this, _, window, _| {
            if std::mem::take(&mut this.title_drag_armed) {
                window.start_window_move();
            }
        }))
        .on_mouse_down(MouseButton::Right, |event, window, _| {
            window.show_window_menu(event.position)
        })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WindowAction {
    Minimize,
    Maximize,
    Close,
}

/// Minimize, maximize or restore, and close, in that physical order in every
/// language, 44 by 48 pixels each.
pub fn window_controls(
    window: &Window,
    strings: &Strings,
    palette: &Palette,
    cx: &Context<Workspace>,
) -> impl IntoElement {
    let maximized = window.is_maximized();
    let controls = [
        (
            WindowAction::Minimize,
            "subtract",
            16.0,
            strings.get("header.windowControls.minimize"),
        ),
        if maximized {
            (
                WindowAction::Maximize,
                "fullscreen-exit",
                14.0,
                strings.get("header.windowControls.restore"),
            )
        } else {
            (
                WindowAction::Maximize,
                "checkbox-blank",
                14.0,
                strings.get("header.windowControls.maximize"),
            )
        },
        (
            WindowAction::Close,
            "close",
            16.0,
            strings.get("header.windowControls.close"),
        ),
    ];
    div()
        .id("window-controls")
        .role(Role::Group)
        .aria_label(strings.get("header.windowControls.groupAria"))
        .flex_shrink_0()
        .h_full()
        .flex()
        .children(controls.map(|(action, icon, icon_size, label)| {
            let (hover_fill, hover_glyph, area) = match action {
                WindowAction::Close => (
                    palette.status_error,
                    palette.status_error_foreground,
                    WindowControlArea::Close,
                ),
                WindowAction::Minimize => {
                    (palette.hover, palette.foreground, WindowControlArea::Min)
                }
                WindowAction::Maximize => {
                    (palette.hover, palette.foreground, WindowControlArea::Max)
                }
            };
            let id: SharedString = match action {
                WindowAction::Minimize => "window-minimize".into(),
                WindowAction::Maximize => "window-maximize".into(),
                WindowAction::Close => "window-close".into(),
            };
            let button = div()
                .id(ElementId::from(id.clone()))
                .group(BUTTON_GROUP)
                .role(Role::Button)
                .accessibility_id(id)
                .aria_label(label)
                .w(px(WINDOW_CONTROL_WIDTH))
                .h_full()
                .flex()
                .items_center()
                .justify_center()
                .window_control_area(area)
                .hover(move |style| style.bg(hover_fill))
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .child(
                    glyph(icon, icon_size, palette.muted_foreground)
                        .group_hover(BUTTON_GROUP, move |style| style.text_color(hover_glyph)),
                );
            on_activate(button, cx, move |_, window, _| match action {
                WindowAction::Minimize => window.minimize_window(),
                WindowAction::Maximize => window.zoom_window(),
                WindowAction::Close => window.remove_window(),
            })
            .test_support()
        }))
}
