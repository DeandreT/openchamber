//! A WebKitGTK view hosted as a native X11 child of the GPUI window.
//!
//! GPUI's X11 window hands out an XCB handle, which neither wry's child
//! path nor GTK's plug accepts, and a plain GDK child window never receives
//! a painted frame. The host instead creates a GDK window of type toplevel
//! under the GPUI window: GDK keeps the frame clock GTK paints from, and X
//! creates the window as a child (the arrangement `GtkPlug` relies on). The
//! [`HostElement`] then owns its bounds, clipping, visibility and focus every
//! frame, and the window shape gives back every area GPUI draws above the
//! page. See this crate's `DOCUMENTATION.md` for the decisions.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::rc::Rc;
use std::time::{Duration, Instant};

use gpui_kit::component::{ActiveTheme, WindowExt};
use gpui_kit::{
    AnyWindowHandle, App, AppContext, Bounds, Context, DispatchPhase, Element, ElementId, Entity,
    EventEmitter, GlobalElementId, HitboxBehavior, HitboxId, InspectorElementId, IntoElement,
    Keystroke, LayoutId, Modifiers, MouseDownEvent, Pixels, PlatformInput, Rgba, ScrollDelta,
    ScrollWheelEvent, Size, Style, TouchPhase, Window, point, px,
};
use gtk::prelude::*;
use gtk::{cairo, gdk, gio, glib};
use webkit2gtk::{
    NavigationPolicyDecisionExt, PermissionRequestExt, PolicyDecisionExt, PolicyDecisionType,
    ResponsePolicyDecisionExt, SettingsExt, URIRequestExt, UserContentManagerExt, WebContextExt,
    WebInspectorExt, WebViewExt,
};

use crate::web_policy::{self, OpenRequest, ViewPolicy, WindowOpenTarget, window_open_target};
use crate::web_shim::{RELAY_HANDLER, RELAY_WORLD};
use crate::webview_platform::{Profile, Unavailable, WebPlatform, random_id};

/// GPUI's lines per wheel notch on Linux (`SCROLL_LINES`).
const SCROLL_LINES: f32 = 3.0;
/// Room around an overlay for its shadow, which has no hitbox.
const OVERLAY_SHADOW: f32 = 12.0;
/// How long after a press in the page a submission still counts as the
/// user's gesture.
const GESTURE_WINDOW: Duration = Duration::from_secs(5);

/// What a view reports to its owner.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum WebViewEvent {
    UrlChanged(String),
    TitleChanged(String),
    /// Loading started or ended; history may have changed with it.
    LoadingChanged,
    /// A navigation the view's policy refused.
    Blocked(String),
    /// A page asked for a new tab.
    OpenTab {
        url: String,
        background: bool,
    },
    /// A message the shim's frame posted, parsed as JSON.
    Message(serde_json::Value),
    /// The user pressed inside the page.
    PageClicked,
    LoadFailed(String),
    /// The page's web process ended.
    Crashed,
    PointerOver(bool),
}

/// What a view loads first.
pub(crate) enum Load {
    Address(String),
    /// A shim document under the address `web_policy::shim_url(nonce)`.
    Shim {
        nonce: String,
        document: String,
    },
}

/// Everything a view is built from.
pub(crate) struct ViewSpec {
    pub policy: ViewPolicy,
    pub profile: Profile,
    pub load: Load,
    /// Vertical wheel turns go to GPUI: the page is as tall as its content
    /// and the list around it scrolls.
    pub forward_wheel: bool,
    pub developer_tools: bool,
}

/// A fresh shim nonce.
pub(crate) fn new_nonce() -> String {
    random_id()
}

/// Events from GTK and WebKit callbacks, which run inside the GLib pump,
/// outside any GPUI update; they only queue.
enum HostEvent {
    Page(WebViewEvent),
    /// `generation` is the modal generation when the press was queued.
    Pressed {
        generation: u64,
    },
    /// A key chord the page left unhandled.
    Key(Keystroke),
    /// A wheel turn at a position inside the view, in GDK pixels.
    Wheel {
        x: f64,
        y: f64,
        dx: f64,
        dy: f64,
        modifiers: Modifiers,
    },
}

type Queue = Rc<RefCell<VecDeque<HostEvent>>>;

fn push(queue: &Queue, event: HostEvent) {
    queue.borrow_mut().push_back(event);
}

/// Physical pixel rectangle `(x, y, width, height)`.
type Rect = (i32, i32, i32, i32);

#[derive(Clone, Debug, Default, PartialEq)]
struct Applied {
    mapped: bool,
    rect: Option<Rect>,
    /// `None` shows the whole rectangle.
    shape: Option<Vec<Rect>>,
    /// The input region's size in GDK pixels; zero takes all input away.
    input_size: Option<(i32, i32)>,
}

/// X11 keyboard focus between the GPUI window and page windows.
///
/// `gdk_window_focus` asks the window manager through `_NET_ACTIVE_WINDOW`,
/// which managers ignore for a child window, so focus moves with a plain
/// `SetInputFocus` on a connection of its own.
pub(crate) struct XFocus {
    connection: x11rb::rust_connection::RustConnection,
}

impl XFocus {
    pub(crate) fn connect() -> Result<Self, String> {
        let (connection, _) = x11rb::connect(None).map_err(|error| error.to_string())?;
        Ok(Self { connection })
    }

    fn focus(&self, window: u64) {
        use x11rb::connection::Connection;
        use x11rb::protocol::xproto::{ConnectionExt, InputFocus};
        let Ok(window) = u32::try_from(window) else {
            return;
        };
        let _ = self
            .connection
            .set_input_focus(InputFocus::PARENT, window, x11rb::CURRENT_TIME);
        let _ = self.connection.flush();
    }

    /// The window holding X keyboard focus. Shares this connection's request
    /// order, so an earlier `focus` is already applied.
    fn current(&self) -> Option<u64> {
        use x11rb::protocol::xproto::ConnectionExt;
        let reply = self.connection.get_input_focus().ok()?.reply().ok()?;
        Some(u64::from(reply.focus))
    }
}

/// Input the page window selects while GPUI holds keyboard focus. Keys are
/// added only while the page owns focus: X11 sends keys to the deepest
/// window under the pointer that selects them when the focus window is an
/// ancestor, so a page that always selected keys would take typing meant for
/// GPUI whenever the pointer rested over it.
fn base_events() -> gdk::EventMask {
    gdk::EventMask::EXPOSURE_MASK
        | gdk::EventMask::STRUCTURE_MASK
        | gdk::EventMask::PROPERTY_CHANGE_MASK
        | gdk::EventMask::FOCUS_CHANGE_MASK
        | gdk::EventMask::BUTTON_PRESS_MASK
        | gdk::EventMask::BUTTON_RELEASE_MASK
        | gdk::EventMask::POINTER_MOTION_MASK
        | gdk::EventMask::ENTER_NOTIFY_MASK
        | gdk::EventMask::LEAVE_NOTIFY_MASK
        | gdk::EventMask::SCROLL_MASK
        | gdk::EventMask::SMOOTH_SCROLL_MASK
}

fn focused_events() -> gdk::EventMask {
    base_events() | gdk::EventMask::KEY_PRESS_MASK | gdk::EventMask::KEY_RELEASE_MASK
}

/// The GTK window that holds the WebKit view, realized on a native child of
/// the GPUI X11 window, with a pass-through shade above the view that
/// paints the modal scrim over the live page.
struct GtkHost {
    child: gdk::Window,
    gtk_window: gtk::Window,
    container: gtk::Box,
    gpui_xid: u64,
    shade: gtk::DrawingArea,
    modal_tint: Rc<Cell<Option<Rgba>>>,
}

impl GtkHost {
    fn new(gpui_xid: u64) -> Result<Self, String> {
        let display = gdk::Display::default().ok_or("no GDK display")?;
        let x11_display = display
            .downcast::<gdkx11::X11Display>()
            .map_err(|_| "the GDK display is not X11")?;
        let parent: gdk::Window =
            gdkx11::X11Window::foreign_new_for_display(&x11_display, gpui_xid).upcast();
        let attributes = gdk::WindowAttr {
            window_type: gdk::WindowType::Toplevel,
            wclass: gdk::WindowWindowClass::InputOutput,
            x: Some(0),
            y: Some(0),
            width: 1,
            height: 1,
            override_redirect: true,
            event_mask: base_events(),
            ..Default::default()
        };
        let child = gdk::Window::new(Some(&parent), &attributes);
        // No window manager manages the embedded window, so none answers
        // frame-sync requests.
        if let Ok(x11) = child.clone().downcast::<gdkx11::X11Window>() {
            x11.set_frame_sync_enabled(false);
        }
        let gtk_window = gtk::Window::new(gtk::WindowType::Toplevel);
        let realized = child.clone();
        gtk_window.connect_realize(move |window| {
            window.set_window(realized.clone());
            // GTK finds the widget for a window's paint and input events
            // through registration; `set_window` alone does not register.
            window.register_window(&realized);
        });
        gtk_window.set_has_window(true);
        gtk_window.realize();
        let container = gtk::Box::new(gtk::Orientation::Vertical, 0);
        let overlay = gtk::Overlay::new();
        overlay.add(&container);
        gtk_window.add(&overlay);
        let shade = gtk::DrawingArea::new();
        shade.set_halign(gtk::Align::Fill);
        shade.set_valign(gtk::Align::Fill);
        shade.set_can_focus(false);
        shade.set_focus_on_click(false);
        shade.set_sensitive(false);
        shade.set_app_paintable(true);
        // Mapping the host uses `show_all`; only modal state shows the shade.
        shade.set_no_show_all(true);
        if let Some(accessible) = shade.accessible() {
            use gtk::atk::prelude::AtkObjectExt;
            accessible.set_role(gtk::atk::Role::RedundantObject);
            accessible.set_name("");
        }
        let modal_tint = Rc::new(Cell::new(None::<Rgba>));
        let tint = modal_tint.clone();
        shade.connect_draw(move |_, cr| {
            if let Some(color) = tint.get() {
                cr.set_operator(cairo::Operator::Over);
                cr.set_source_rgba(
                    f64::from(color.r),
                    f64::from(color.g),
                    f64::from(color.b),
                    f64::from(color.a),
                );
                let _ = cr.paint();
            }
            glib::Propagation::Stop
        });
        overlay.add_overlay(&shade);
        overlay.set_overlay_pass_through(&shade, true);
        Ok(Self {
            child,
            gtk_window,
            container,
            gpui_xid,
            shade,
            modal_tint,
        })
    }

    fn xid(&self) -> u64 {
        self.child
            .clone()
            .downcast::<gdkx11::X11Window>()
            .map(|window| window.xid())
            .unwrap_or(0)
    }
}

/// The X11 window of a GPUI window, or `None` on Wayland.
fn gpui_xid(window: &Window) -> Option<u64> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    match HasWindowHandle::window_handle(window).map(|handle| handle.as_raw()) {
        Ok(RawWindowHandle::Xcb(handle)) => Some(u64::from(handle.window.get())),
        Ok(RawWindowHandle::Xlib(handle)) => Some(handle.window),
        _ => None,
    }
}

pub(crate) struct HostedWebView {
    view: webkit2gtk::WebView,
    host: GtkHost,
    window: AnyWindowHandle,
    focus: Rc<XFocus>,
    queue: Queue,
    policy: Rc<ViewPolicy>,
    /// The owner shows the view, e.g. its tab is selected.
    owner_visible: bool,
    bounds: Option<Bounds<Pixels>>,
    visible: Option<Bounds<Pixels>>,
    scale: f32,
    user_zoom: f64,
    hitbox: Option<HitboxId>,
    /// What GPUI drew above the view in its last frame.
    cutouts: Vec<Bounds<Pixels>>,
    applied: Applied,
    page_focused: bool,
    /// Bumped on each modal entry; page presses from before it are stale.
    modal_generation: Rc<Cell<u64>>,
    last_press: Rc<Cell<Option<Instant>>>,
    popups: Rc<RefCell<Vec<gtk::Window>>>,
}

impl EventEmitter<WebViewEvent> for HostedWebView {}

impl HostedWebView {
    /// Builds a view for `window`, or reports why this session has none.
    pub(crate) fn build(
        spec: ViewSpec,
        window: &mut Window,
        cx: &mut App,
    ) -> Result<Entity<Self>, Unavailable> {
        let xid = gpui_xid(window).ok_or(Unavailable::NotX11)?;
        WebPlatform::ensure(cx)?;
        let host = GtkHost::new(xid).map_err(Unavailable::Failed)?;
        let context = WebPlatform::context(cx, spec.profile);
        let settings = webkit2gtk::Settings::new();
        settings.set_enable_developer_extras(spec.developer_tools);
        settings.set_enable_write_console_messages_to_stdout(false);
        settings.set_javascript_can_open_windows_automatically(false);
        settings.set_enable_back_forward_navigation_gestures(false);
        let manager = webkit2gtk::UserContentManager::new();
        if matches!(spec.load, Load::Shim { .. }) {
            // The relay lives in an isolated world of the top frame; the
            // frame's own world, and every subframe, sees no handler.
            manager.register_script_message_handler_in_world(RELAY_HANDLER, RELAY_WORLD);
            manager.add_script(&webkit2gtk::UserScript::for_world(
                &crate::web_shim::relay_script(),
                webkit2gtk::UserContentInjectedFrames::TopFrame,
                webkit2gtk::UserScriptInjectionTime::Start,
                RELAY_WORLD,
                &[],
                &[],
            ));
        }
        let view = webkit2gtk::WebView::builder()
            .web_context(&context)
            .settings(&settings)
            .user_content_manager(&manager)
            .build();
        host.container.pack_start(&view, true, true, 0);
        let queue = Queue::default();
        let policy = Rc::new(spec.policy);
        let modal_generation = Rc::new(Cell::new(0_u64));
        let last_press = Rc::new(Cell::new(None));
        let popups = Rc::new(RefCell::new(Vec::new()));
        let focus = WebPlatform::focus(cx);
        connect_messages(&manager, &queue);
        connect_navigation(&view, &policy, &queue, &popups);
        connect_page_state(&view, &queue);
        connect_input(
            &view,
            &host,
            &focus,
            &queue,
            &modal_generation,
            &last_press,
            spec.forward_wheel,
        );
        match spec.load {
            Load::Address(url) => {
                if policy.allows_load(&url) {
                    view.load_uri(&url);
                }
            }
            Load::Shim { nonce, document } => {
                WebPlatform::offer_shim(cx, &nonce, &view, document);
                view.load_uri(&web_policy::shim_url(&nonce));
            }
        }
        let hosted = Self {
            view,
            host,
            window: window.window_handle(),
            focus,
            queue,
            policy,
            owner_visible: true,
            bounds: None,
            visible: None,
            scale: window.scale_factor(),
            user_zoom: 1.0,
            hitbox: None,
            cutouts: Vec::new(),
            applied: Applied::default(),
            page_focused: false,
            modal_generation,
            last_press,
            popups,
        };
        let entity = cx.new(|_| hosted);
        WebPlatform::register(entity.downgrade(), cx);
        Ok(entity)
    }

    /// Starts a load the user asked for. Only browser views take addresses,
    /// and only http(s) or `about:blank`; returns whether the load started.
    pub(crate) fn load_url(&self, url: &str) -> bool {
        let allowed = self.policy.allows_load(url);
        if allowed {
            self.view.load_uri(url);
        }
        allowed
    }

    pub(crate) fn go_back(&self) {
        self.view.go_back();
    }

    pub(crate) fn go_forward(&self) {
        self.view.go_forward();
    }

    pub(crate) fn reload(&self) {
        self.view.reload();
    }

    pub(crate) fn stop(&self) {
        self.view.stop_loading();
    }

    pub(crate) fn can_go_back(&self) -> bool {
        self.view.can_go_back()
    }

    pub(crate) fn can_go_forward(&self) -> bool {
        self.view.can_go_forward()
    }

    pub(crate) fn is_loading(&self) -> bool {
        self.view.is_loading()
    }

    /// Opens WebKit's inspector docked in the view, when the view allows it.
    pub(crate) fn open_developer_tools(&self) {
        if let Some(inspector) = self.view.inspector() {
            inspector.show();
        }
    }

    /// The page's zoom apart from the display scale.
    pub(crate) fn set_zoom(&mut self, zoom: f64) {
        self.user_zoom = zoom;
        self.apply_zoom();
    }

    /// Shows or hides the view for its owner, such as a tab switch.
    pub(crate) fn set_owner_visible(&mut self, visible: bool) {
        if self.owner_visible != visible {
            self.owner_visible = visible;
            self.apply();
        }
    }

    /// Posts `message` into the shim's frame through the relay.
    pub(crate) fn post_message(&self, message: &serde_json::Value) {
        self.view.evaluate_javascript(
            &crate::web_shim::post_script(message),
            Some(RELAY_WORLD),
            None,
            None::<&gio::Cancellable>,
            |_| {},
        );
    }

    /// Whether the user pressed inside the page recently enough for a
    /// submission to be their gesture. GTK only sees real presses; a page
    /// cannot produce one.
    pub(crate) fn has_recent_gesture(&self) -> bool {
        self.last_press
            .get()
            .is_some_and(|pressed| pressed.elapsed() <= GESTURE_WINDOW)
    }

    /// One pump step: delivers queued events and brings the native window
    /// up to date with the last frame.
    pub(crate) fn tick(this: &Entity<Self>, cx: &mut App) {
        let window = this.read(cx).window;
        let _ = window.update(cx, |_, window, cx| {
            let inputs = this.update(cx, |host, cx| {
                let inputs = host.drain(window, cx);
                host.sync_window(window, cx);
                inputs
            });
            // Outside the update: an action may reach this view's owner.
            for input in inputs {
                match input {
                    WindowInput::Key(keystroke) => {
                        window.dispatch_keystroke(keystroke, cx);
                    }
                    WindowInput::Event(event) => {
                        window.dispatch_event(event, cx);
                    }
                }
            }
        });
    }

    fn modal_open(window: &mut Window, cx: &mut App) -> bool {
        window.has_active_dialog(cx) || window.has_active_sheet(cx)
    }

    fn drain(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Vec<WindowInput> {
        let mut inputs = Vec::new();
        let modal = Self::modal_open(window, cx);
        loop {
            let event = self.queue.borrow_mut().pop_front();
            let Some(event) = event else { break };
            match event {
                HostEvent::Page(event) => cx.emit(event),
                HostEvent::Pressed { generation } => {
                    // A press from before a modal, or during one, is stale.
                    if modal || generation != self.modal_generation.get() {
                        continue;
                    }
                    self.page_focused = true;
                    // GPUI keeps no focused element while the page types.
                    window.blur(cx);
                    cx.emit(WebViewEvent::PageClicked);
                }
                HostEvent::Key(keystroke) => {
                    if !modal {
                        inputs.push(WindowInput::Key(keystroke));
                    }
                }
                HostEvent::Wheel {
                    x,
                    y,
                    dx,
                    dy,
                    modifiers,
                } => {
                    let Some(bounds) = self.bounds.filter(|_| !modal) else {
                        continue;
                    };
                    let gdk_scale = f64::from(self.host.child.scale_factor().max(1));
                    let to_logical = gdk_scale / f64::from(self.scale.max(0.01));
                    let position = point(
                        bounds.origin.x + px((x * to_logical) as f32),
                        bounds.origin.y + px((y * to_logical) as f32),
                    );
                    inputs.push(WindowInput::Event(PlatformInput::ScrollWheel(
                        ScrollWheelEvent {
                            position,
                            delta: ScrollDelta::Lines(point(
                                -(dx as f32) * SCROLL_LINES,
                                -(dy as f32) * SCROLL_LINES,
                            )),
                            modifiers,
                            touch_phase: TouchPhase::default(),
                        },
                    )));
                }
            }
        }
        inputs
    }

    /// Reads what the last frame drew: whether the element was painted at
    /// all, what sits above it, and whether a dialog owns input.
    fn sync_window(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let above = self
            .hitbox
            .and_then(|hitbox| window.hitbox_bounds_above(hitbox));
        let mut changed = false;
        match above {
            None => {
                if self.bounds.is_some() {
                    self.bounds = None;
                    self.visible = None;
                    changed = true;
                }
            }
            Some(above) => {
                let viewport = window.viewport_size();
                let visible = self.visible.unwrap_or_default();
                let cutouts: Vec<_> = above
                    .into_iter()
                    // A transparent full-window layer (a dialog's backdrop, a
                    // menu's outside-click catcher) covers nothing visible;
                    // the modal tint stands in for a backdrop.
                    .filter(|bounds| {
                        bounds.size.width < viewport.width || bounds.size.height < viewport.height
                    })
                    .map(|bounds| bounds.dilate(px(OVERLAY_SHADOW)))
                    .filter(|bounds| bounds.intersects(&visible))
                    .collect();
                if cutouts != self.cutouts {
                    self.cutouts = cutouts;
                    changed = true;
                }
            }
        }
        let tint = Self::modal_open(window, cx).then(|| cx.theme().overlay.to_rgb());
        changed |= self.set_modal_tint(tint);
        // GPUI took focus back on its own, such as a shortcut focusing an
        // input: keys must reach it again.
        if self.page_focused && window.focused(cx).is_some() {
            self.release_page_focus();
        }
        changed |= self.apply();
        if changed {
            // X clipped GPUI's drawing under the old shape.
            window.refresh();
        }
    }

    fn layout(
        &mut self,
        bounds: Bounds<Pixels>,
        visible: Bounds<Pixels>,
        scale: f32,
        hitbox: HitboxId,
    ) {
        self.bounds = Some(bounds);
        self.visible = Some(visible);
        self.hitbox = Some(hitbox);
        self.scale = scale;
        self.apply_zoom();
        self.apply();
    }

    /// CSS pixels match GPUI logical pixels at user zoom 1. GDK applies its
    /// integer scale; WebKit zoom supplies only the remaining ratio.
    fn apply_zoom(&self) {
        let zoom = self.user_zoom * f64::from(self.scale)
            / f64::from(self.host.child.scale_factor().max(1));
        if (self.view.zoom_level() - zoom).abs() > 0.0001 {
            self.view.set_zoom_level(zoom);
        }
    }

    fn physical(&self, bounds: Bounds<Pixels>) -> Rect {
        physical(bounds, self.scale)
    }

    /// Brings the native window to the state layout, overlays and modal
    /// state ask for. Returns whether the X11 window changed.
    fn apply(&mut self) -> bool {
        let before = self.applied.clone();
        self.apply_native();
        let changed = before != self.applied;
        if changed {
            self.host.child.display().flush();
        }
        changed
    }

    fn apply_native(&mut self) {
        let (Some(bounds), Some(visible)) = (self.bounds, self.visible) else {
            self.set_mapped(false);
            return;
        };
        let full = self.physical(bounds);
        let shown = self.physical(visible);
        let cuts: Vec<Rect> = self
            .cutouts
            .iter()
            .map(|cut| {
                let cut = self.physical(*cut);
                (cut.0 - full.0, cut.1 - full.1, cut.2, cut.3)
            })
            .collect();
        let shape = match window_shape(full, shown, &cuts) {
            Shape::Hidden => None,
            _ if !self.owner_visible => None,
            Shape::Whole => Some(None),
            Shape::Parts(parts) => Some(Some(parts)),
        };
        let Some(shape) = shape else {
            self.set_mapped(false);
            return;
        };
        let gdk_scale = self.host.child.scale_factor().max(1);
        if self.applied.rect != Some(full) {
            self.host.child.move_resize(
                full.0 / gdk_scale,
                full.1 / gdk_scale,
                full.2 / gdk_scale,
                full.3 / gdk_scale,
            );
            self.host.gtk_window.size_allocate(&gtk::Allocation::new(
                0,
                0,
                full.2 / gdk_scale,
                full.3 / gdk_scale,
            ));
            self.applied.rect = Some(full);
        }
        self.apply_input_shape(full.2 / gdk_scale, full.3 / gdk_scale);
        if self.applied.shape != shape {
            match &shape {
                None => self.host.child.shape_combine_region(None, 0, 0),
                Some(rects) => {
                    let region = cairo::Region::create();
                    for (x, y, w, h) in rects {
                        let _ = region.union_rectangle(&cairo::RectangleInt::new(
                            x / gdk_scale,
                            y / gdk_scale,
                            w / gdk_scale,
                            h / gdk_scale,
                        ));
                    }
                    self.host.child.shape_combine_region(Some(&region), 0, 0);
                }
            }
            // GDK applies a toplevel's shape during its next paint, and a
            // shrinking shape invalidates nothing, so force that paint.
            self.host.child.invalidate_rect(None, true);
            self.applied.shape = shape;
        }
        self.set_mapped(true);
    }

    fn apply_input_shape(&mut self, width: i32, height: i32) {
        let size = if self.host.modal_tint.get().is_some() {
            (0, 0)
        } else {
            (width.max(0), height.max(0))
        };
        if self.applied.input_size == Some(size) {
            return;
        }
        let region = if size.0 == 0 || size.1 == 0 {
            cairo::Region::create()
        } else {
            cairo::Region::create_rectangle(&cairo::RectangleInt::new(0, 0, size.0, size.1))
        };
        self.host.child.input_shape_combine_region(&region, 0, 0);
        self.applied.input_size = Some(size);
    }

    /// Shades the live page and gives all native input to the GPUI modal.
    /// Removing the modal restores input without moving focus. Returns
    /// whether anything changed.
    fn set_modal_tint(&mut self, tint: Option<Rgba>) -> bool {
        let previous = self.host.modal_tint.replace(tint);
        if previous == tint {
            return false;
        }
        let entering = previous.is_none() && tint.is_some();
        // A press may still be queued, so this asks X rather than the
        // queued state. Focus held by another application stays there.
        let page_held_focus = entering && self.focus.current() == Some(self.host.xid());
        if entering {
            self.modal_generation.set(self.modal_generation.get() + 1);
            self.page_focused = false;
            self.host.child.set_events(base_events());
            self.close_page_popups();
        }
        self.apply_input_shape(self.host.child.width(), self.host.child.height());
        self.host.child.display().flush();
        if page_held_focus {
            self.focus.focus(self.host.gpui_xid);
        }
        if tint.is_some() {
            self.host.shade.show();
        } else {
            self.host.shade.hide();
        }
        self.host.shade.queue_draw();
        self.host.child.invalidate_rect(None, true);
        self.host.child.display().flush();
        true
    }

    /// Closes the page's popups, which are separate grabbing windows that
    /// neither the input shape nor the tint covers.
    ///
    /// WebKit's GTK 3 `<select>` list is a grabbing popup attached to the
    /// view that closes, ungrabs and keeps the committed selection on any
    /// button press, as for a click outside it. Its context menu is a
    /// `GtkMenu` attached to the view. A datalist dropdown hides on the
    /// view's focus-out, which modal entry causes whenever the page held
    /// focus.
    fn close_page_popups(&self) {
        let view: &gtk::Widget = self.view.upcast_ref();
        let grabbing =
            gtk::grab_get_current().and_then(|widget| widget.downcast::<gtk::Window>().ok());
        if let Some(popup) = grabbing
            && popup.window_type() == gtk::WindowType::Popup
            && popup.attached_to().as_ref() == Some(view)
            && !popup.child().is_some_and(|child| child.is::<gtk::Menu>())
        {
            let press = gdk::Event::new(gdk::EventType::ButtonPress);
            let _ = popup.emit_by_name::<bool>("button-press-event", &[&press]);
        }
        for widget in gtk::Menu::for_attach_widget(view) {
            if let Ok(menu) = widget.downcast::<gtk::Menu>()
                && menu.is_visible()
            {
                menu.deactivate();
            }
        }
    }

    fn set_mapped(&mut self, mapped: bool) {
        if self.applied.mapped == mapped {
            return;
        }
        if mapped {
            self.host.gtk_window.show_all();
            self.host.child.show();
        } else {
            self.release_page_focus();
            self.host.child.hide();
        }
        self.applied.mapped = mapped;
    }

    /// Returns X keyboard focus to the GPUI window.
    fn release_page_focus(&mut self) {
        if !self.page_focused {
            return;
        }
        self.page_focused = false;
        self.host.child.set_events(base_events());
        self.host.child.display().flush();
        self.focus.focus(self.host.gpui_xid);
    }
}

/// Input for the GPUI window that must run outside the view's update.
enum WindowInput {
    Key(Keystroke),
    Event(PlatformInput),
}

/// Physical pixels of logical `bounds`, with edges rounded so neighbours
/// share them.
fn physical(bounds: Bounds<Pixels>, scale: f32) -> Rect {
    let left = (f32::from(bounds.origin.x) * scale).round() as i32;
    let top = (f32::from(bounds.origin.y) * scale).round() as i32;
    let right = (f32::from(bounds.origin.x + bounds.size.width) * scale).round() as i32;
    let bottom = (f32::from(bounds.origin.y + bounds.size.height) * scale).round() as i32;
    (left, top, right - left, bottom - top)
}

/// What of a child window shows.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Shape {
    Hidden,
    Whole,
    /// These rectangles, relative to the window.
    Parts(Vec<Rect>),
}

/// The shape of a child window at `full` that shows only `shown` minus the
/// `cuts`, which are relative to the window.
fn window_shape(full: Rect, shown: Rect, cuts: &[Rect]) -> Shape {
    if shown.2 <= 0 || shown.3 <= 0 || full.2 <= 0 || full.3 <= 0 {
        return Shape::Hidden;
    }
    let mut keep = vec![(shown.0 - full.0, shown.1 - full.1, shown.2, shown.3)];
    for cut in cuts {
        keep = keep
            .into_iter()
            .flat_map(|rect| subtract(rect, *cut))
            .collect();
    }
    if keep.is_empty() {
        Shape::Hidden
    } else if keep == [(0, 0, full.2, full.3)] {
        Shape::Whole
    } else {
        Shape::Parts(keep)
    }
}

/// `rect` minus `cut`, as up to four rectangles.
fn subtract(rect: Rect, cut: Rect) -> Vec<Rect> {
    let (x, y, w, h) = rect;
    let (cx, cy, cw, ch) = cut;
    let left = cx.max(x);
    let top = cy.max(y);
    let right = (cx + cw).min(x + w);
    let bottom = (cy + ch).min(y + h);
    if left >= right || top >= bottom {
        return vec![rect];
    }
    let mut out = Vec::new();
    if top > y {
        out.push((x, y, w, top - y));
    }
    if bottom < y + h {
        out.push((x, bottom, w, y + h - bottom));
    }
    if left > x {
        out.push((x, top, left - x, bottom - top));
    }
    if right < x + w {
        out.push((right, top, x + w - right, bottom - top));
    }
    out
}

/// A GDK key as a GPUI keystroke, for chords only: a page never passes plain
/// typing on, so single-key shortcuts of the app stay out of reach.
fn keystroke(event: &gdk::EventKey) -> Option<Keystroke> {
    let state = event.state();
    let modifiers = Modifiers {
        control: state.contains(gdk::ModifierType::CONTROL_MASK),
        alt: state.contains(gdk::ModifierType::MOD1_MASK),
        shift: state.contains(gdk::ModifierType::SHIFT_MASK),
        platform: state.contains(gdk::ModifierType::SUPER_MASK),
        function: false,
    };
    let keyval = event.keyval();
    let name = keyval.to_lower().name()?.to_string();
    let key = match name.as_str() {
        "Return" | "KP_Enter" => "enter".to_owned(),
        "Escape" => "escape".to_owned(),
        "Tab" | "ISO_Left_Tab" => "tab".to_owned(),
        "BackSpace" => "backspace".to_owned(),
        "Delete" => "delete".to_owned(),
        "Left" => "left".to_owned(),
        "Right" => "right".to_owned(),
        "Up" => "up".to_owned(),
        "Down" => "down".to_owned(),
        "Home" => "home".to_owned(),
        "End" => "end".to_owned(),
        "Page_Up" => "pageup".to_owned(),
        "Page_Down" => "pagedown".to_owned(),
        "space" => "space".to_owned(),
        "comma" => ",".to_owned(),
        "period" => ".".to_owned(),
        "slash" => "/".to_owned(),
        "backslash" => "\\".to_owned(),
        "semicolon" => ";".to_owned(),
        "apostrophe" => "'".to_owned(),
        "bracketleft" => "[".to_owned(),
        "bracketright" => "]".to_owned(),
        "minus" => "-".to_owned(),
        "equal" => "=".to_owned(),
        "plus" => "+".to_owned(),
        "grave" => "`".to_owned(),
        function if function.starts_with('F') && function[1..].parse::<u8>().is_ok() => {
            function.to_ascii_lowercase()
        }
        single if single.chars().count() == 1 => single.to_owned(),
        _ => return None,
    };
    let chord = modifiers.control
        || modifiers.alt
        || modifiers.platform
        || key.starts_with('f') && key.len() > 1;
    chord.then_some(Keystroke {
        modifiers,
        key,
        key_char: None,
    })
}

fn connect_messages(manager: &webkit2gtk::UserContentManager, queue: &Queue) {
    let queue = queue.clone();
    manager.connect_script_message_received(Some(RELAY_HANDLER), move |_, result| {
        let Some(text) = result.js_value().map(|value| value.to_string()) else {
            return;
        };
        if let Ok(message) = serde_json::from_str(&text) {
            push(&queue, HostEvent::Page(WebViewEvent::Message(message)));
        }
    });
}

/// How a page's new-window request was made.
fn open_request(action: &mut webkit2gtk::NavigationAction) -> OpenRequest {
    let modified = action.mouse_button() == 2
        || action.modifiers()
            & (gdk::ModifierType::CONTROL_MASK.bits() | gdk::ModifierType::SUPER_MASK.bits())
            != 0;
    match action.navigation_type() {
        webkit2gtk::NavigationType::LinkClicked if modified => OpenRequest::BackgroundTab,
        webkit2gtk::NavigationType::LinkClicked => OpenRequest::ForegroundTab,
        webkit2gtk::NavigationType::FormSubmitted | webkit2gtk::NavigationType::FormResubmitted => {
            OpenRequest::FormPost
        }
        _ => OpenRequest::Script,
    }
}

fn request_uri(
    decision: &webkit2gtk::NavigationPolicyDecision,
) -> Option<(String, webkit2gtk::NavigationAction)> {
    let action = decision.navigation_action()?;
    let uri = action.request()?.uri()?.to_string();
    Some((uri, action))
}

fn connect_navigation(
    view: &webkit2gtk::WebView,
    policy: &Rc<ViewPolicy>,
    queue: &Queue,
    popups: &Rc<RefCell<Vec<gtk::Window>>>,
) {
    let decide_policy = policy.clone();
    let decide_queue = queue.clone();
    view.connect_decide_policy(move |view, decision, kind| {
        match kind {
            PolicyDecisionType::NavigationAction => {
                let Some(navigation) =
                    decision.downcast_ref::<webkit2gtk::NavigationPolicyDecision>()
                else {
                    return false;
                };
                let Some((uri, _)) = request_uri(navigation) else {
                    decision.ignore();
                    return true;
                };
                if !decide_policy.allows(&uri) {
                    decision.ignore();
                    push(&decide_queue, HostEvent::Page(WebViewEvent::Blocked(uri)));
                    return true;
                }
                false
            }
            PolicyDecisionType::NewWindowAction => {
                let Some(navigation) =
                    decision.downcast_ref::<webkit2gtk::NavigationPolicyDecision>()
                else {
                    decision.ignore();
                    return true;
                };
                let Some((uri, mut action)) = request_uri(navigation) else {
                    decision.ignore();
                    return true;
                };
                let target = if matches!(*decide_policy, ViewPolicy::Browser) {
                    window_open_target(&uri, open_request(&mut action))
                } else {
                    // A sign-in popup, and every shim, keeps its pages in
                    // place; shims open nothing at all.
                    match *decide_policy {
                        ViewPolicy::Popup if decide_policy.allows_load(&uri) => {
                            WindowOpenTarget::InPlace
                        }
                        _ => WindowOpenTarget::Deny,
                    }
                };
                match target {
                    WindowOpenTarget::Popup => {
                        decision.use_();
                        return true;
                    }
                    WindowOpenTarget::Tab | WindowOpenTarget::BackgroundTab => {
                        push(
                            &decide_queue,
                            HostEvent::Page(WebViewEvent::OpenTab {
                                url: uri,
                                background: target == WindowOpenTarget::BackgroundTab,
                            }),
                        );
                    }
                    WindowOpenTarget::InPlace => view.load_uri(&uri),
                    WindowOpenTarget::Deny => {
                        push(&decide_queue, HostEvent::Page(WebViewEvent::Blocked(uri)));
                    }
                }
                decision.ignore();
                true
            }
            PolicyDecisionType::Response => {
                // Nothing is downloaded: a response WebKit cannot show is dropped.
                let supported = decision
                    .downcast_ref::<webkit2gtk::ResponsePolicyDecision>()
                    .is_none_or(|response| response.is_mime_type_supported());
                if !supported {
                    decision.ignore();
                    return true;
                }
                false
            }
            _ => false,
        }
    });
    // Device, location, notification and media access is denied, as the
    // desktop shell denies it: a silent grant is the outcome that must not
    // happen, and nothing asks yet.
    view.connect_permission_request(|_, request| {
        request.deny();
        true
    });
    // A self-signed certificate is accepted on loopback only, for dev servers.
    let tls_policy = policy.clone();
    view.connect_load_failed_with_tls_errors(move |view, failing, certificate, errors| {
        let loopback_https =
            failing.starts_with("https://") && web_policy::is_loopback_url(failing);
        if !matches!(*tls_policy, ViewPolicy::Browser)
            || !loopback_https
            || errors != gio::TlsCertificateFlags::UNKNOWN_CA
        {
            return false;
        }
        let Some(host) = openchamber_native_transport::Url::parse(failing)
            .ok()
            .and_then(|url| url.host_str().map(str::to_owned))
        else {
            return false;
        };
        if let Some(context) = view.context() {
            context.allow_tls_certificate_for_host(certificate, &host);
        }
        view.load_uri(failing);
        true
    });
    let popups = popups.clone();
    view.connect_create(move |view, _| {
        let popup = webkit2gtk::WebView::builder().related_view(view).build();
        if let Some(settings) = WebViewExt::settings(view) {
            popup.set_settings(&settings);
        }
        let window = gtk::Window::new(gtk::WindowType::Toplevel);
        window.set_default_size(520, 680);
        window.add(&popup);
        let shown = window.clone();
        popup.connect_ready_to_show(move |_| shown.show_all());
        let closing = window.clone();
        popup.connect_close(move |_| closing.close());
        let titled = window.clone();
        popup.connect_title_notify(move |popup| {
            titled.set_title(popup.title().as_deref().unwrap_or_default());
        });
        let popup_policy = Rc::new(ViewPolicy::Popup);
        connect_navigation(&popup, &popup_policy, &Queue::default(), &Rc::default());
        popup.connect_permission_request(|_, request| {
            request.deny();
            true
        });
        popups.borrow_mut().push(window);
        Some(popup.upcast())
    });
}

fn connect_page_state(view: &webkit2gtk::WebView, queue: &Queue) {
    let urls = queue.clone();
    view.connect_uri_notify(move |view| {
        let url = view.uri().map(String::from).unwrap_or_default();
        push(&urls, HostEvent::Page(WebViewEvent::UrlChanged(url)));
    });
    let titles = queue.clone();
    view.connect_title_notify(move |view| {
        let title = view.title().map(String::from).unwrap_or_default();
        push(&titles, HostEvent::Page(WebViewEvent::TitleChanged(title)));
    });
    let loads = queue.clone();
    view.connect_load_changed(move |_, _| {
        push(&loads, HostEvent::Page(WebViewEvent::LoadingChanged));
    });
    let failures = queue.clone();
    view.connect_load_failed(move |_, _, uri, error| {
        // A stopped load and a refused navigation are not failures.
        let cancelled = error.matches(webkit2gtk::NetworkError::Cancelled)
            || error.matches(webkit2gtk::PolicyError::FrameLoadInterruptedByPolicyChange);
        if !cancelled {
            push(
                &failures,
                HostEvent::Page(WebViewEvent::LoadFailed(uri.to_owned())),
            );
        }
        false
    });
    let crashes = queue.clone();
    view.connect_web_process_terminated(move |_, _| {
        push(&crashes, HostEvent::Page(WebViewEvent::Crashed));
    });
}

fn connect_input(
    view: &webkit2gtk::WebView,
    host: &GtkHost,
    focus: &Rc<XFocus>,
    queue: &Queue,
    modal_generation: &Rc<Cell<u64>>,
    last_press: &Rc<Cell<Option<Instant>>>,
    forward_wheel: bool,
) {
    // The input shape routes new pointer events to GPUI while a modal is
    // open. Input GTK already queued is dropped as well; releases and motion
    // still reach the page, so a press that began before the modal ends
    // there. GTK drops a release whose press it discarded.
    let modal_tint = host.modal_tint.clone();
    view.connect_event(move |_, event| {
        let blocked = modal_tint.get().is_some()
            && matches!(
                event.event_type(),
                gdk::EventType::ButtonPress
                    | gdk::EventType::DoubleButtonPress
                    | gdk::EventType::TripleButtonPress
                    | gdk::EventType::KeyPress
                    | gdk::EventType::Scroll
                    | gdk::EventType::TouchBegin
            );
        if blocked {
            glib::Propagation::Stop
        } else {
            glib::Propagation::Proceed
        }
    });
    // A press in the page moves keyboard focus to the page's X window.
    {
        let queue = queue.clone();
        let child = host.child.clone();
        let focus = focus.clone();
        let xid = host.xid();
        let generation = modal_generation.clone();
        let last_press = last_press.clone();
        view.connect_button_press_event(move |_, _| {
            last_press.set(Some(Instant::now()));
            child.set_events(focused_events());
            child.display().flush();
            focus.focus(xid);
            push(
                &queue,
                HostEvent::Pressed {
                    generation: generation.get(),
                },
            );
            glib::Propagation::Proceed
        });
    }
    // GTK's default handler gives the key to WebKit first; WebKit hands back
    // keys the page left unhandled, which this after-handler receives. A
    // page's `preventDefault` keeps its own shortcut.
    {
        let queue = queue.clone();
        let modal_tint = host.modal_tint.clone();
        host.gtk_window
            .connect_local("key-press-event", true, move |values| {
                if modal_tint.get().is_some() {
                    return Some(true.to_value());
                }
                let handled = values
                    .get(1)
                    .and_then(|value| value.get::<gdk::Event>().ok())
                    .and_then(|event| event.downcast::<gdk::EventKey>().ok())
                    .and_then(|event| keystroke(&event))
                    .map(|keystroke| push(&queue, HostEvent::Key(keystroke)))
                    .is_some();
                Some(handled.to_value())
            });
    }
    if forward_wheel {
        let queue = queue.clone();
        view.connect_scroll_event(move |_, event| {
            let (dx, dy) = match event.direction() {
                gdk::ScrollDirection::Up => (0.0, -1.0),
                gdk::ScrollDirection::Down => (0.0, 1.0),
                gdk::ScrollDirection::Left => (-1.0, 0.0),
                gdk::ScrollDirection::Right => (1.0, 0.0),
                _ => event.delta(),
            };
            // A sideways turn stays in the page, which may scroll that way.
            if dx.abs() > dy.abs() {
                return glib::Propagation::Proceed;
            }
            let (x, y) = event.position();
            let state = event.state();
            push(
                &queue,
                HostEvent::Wheel {
                    x,
                    y,
                    dx,
                    dy,
                    modifiers: Modifiers {
                        control: state.contains(gdk::ModifierType::CONTROL_MASK),
                        alt: state.contains(gdk::ModifierType::MOD1_MASK),
                        shift: state.contains(gdk::ModifierType::SHIFT_MASK),
                        platform: state.contains(gdk::ModifierType::SUPER_MASK),
                        function: false,
                    },
                },
            );
            glib::Propagation::Stop
        });
    }
    let over = queue.clone();
    view.connect_enter_notify_event(move |_, _| {
        push(&over, HostEvent::Page(WebViewEvent::PointerOver(true)));
        glib::Propagation::Proceed
    });
    let left = queue.clone();
    view.connect_leave_notify_event(move |_, _| {
        push(&left, HostEvent::Page(WebViewEvent::PointerOver(false)));
        glib::Propagation::Proceed
    });
}

impl Drop for HostedWebView {
    fn drop(&mut self) {
        self.release_page_focus();
        // Dropping a view does not end its web process until the app exits.
        self.view.terminate_web_process();
        for popup in self.popups.borrow_mut().drain(..) {
            popup.close();
        }
        self.host.child.hide();
        self.host.gtk_window.close();
    }
}

/// Lays out the native view where it sits in GPUI and records the frame
/// that painted it.
pub(crate) struct HostElement {
    host: Entity<HostedWebView>,
}

impl HostElement {
    pub(crate) fn new(host: Entity<HostedWebView>) -> Self {
        Self { host }
    }
}

impl IntoElement for HostElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for HostElement {
    type RequestLayoutState = ();
    type PrepaintState = Option<gpui_kit::Hitbox>;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let style = Style {
            size: Size::full(),
            ..Default::default()
        };
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let visible = bounds.intersect(&window.content_mask().bounds);
        let scale = window.scale_factor();
        let hitbox = window.insert_hitbox(visible, HitboxBehavior::Normal);
        let id = hitbox.id;
        self.host
            .update(cx, |host, _| host.layout(bounds, visible, scale, id));
        Some(hitbox)
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        hitbox: &mut Self::PrepaintState,
        window: &mut Window,
        _: &mut App,
    ) {
        let Some(hitbox) = hitbox.clone() else {
            return;
        };
        let host = self.host.clone();
        // GPUI sees only presses outside the native view: X11 delivers the
        // ones inside it to WebKit. Any press GPUI receives takes focus back.
        window.on_mouse_event(move |event: &MouseDownEvent, phase, _, cx| {
            if phase != DispatchPhase::Bubble || hitbox.bounds.contains(&event.position) {
                return;
            }
            host.update(cx, |host, _| host.release_page_focus());
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shapes_clip_to_the_visible_part() {
        assert_eq!(
            window_shape((0, 0, 100, 50), (0, 0, 100, 50), &[]),
            Shape::Whole
        );
        assert_eq!(
            window_shape((0, 0, 100, 50), (0, 20, 100, 30), &[]),
            Shape::Parts(vec![(0, 20, 100, 30)])
        );
        assert_eq!(
            window_shape((0, 0, 100, 50), (0, 0, 0, 50), &[]),
            Shape::Hidden
        );
    }

    #[test]
    fn overlays_are_cut_out_of_the_page() {
        let Shape::Parts(shape) =
            window_shape((0, 0, 100, 100), (0, 0, 100, 100), &[(10, 10, 20, 20)])
        else {
            panic!("a cut page keeps its other parts");
        };
        let area: i32 = shape.iter().map(|(_, _, w, h)| w * h).sum();
        assert_eq!(area, 100 * 100 - 20 * 20);
        for rect in &shape {
            let overlaps =
                rect.0 < 30 && rect.0 + rect.2 > 10 && rect.1 < 30 && rect.1 + rect.3 > 10;
            assert!(!overlaps, "{rect:?} overlaps the cut");
        }
        // An overlay covering the whole page hides the window.
        assert_eq!(
            window_shape((0, 0, 100, 100), (0, 0, 100, 100), &[(-5, -5, 200, 200)]),
            Shape::Hidden
        );
    }

    #[test]
    fn subtraction_leaves_disjoint_cuts_alone() {
        assert_eq!(
            subtract((0, 0, 10, 10), (20, 20, 5, 5)),
            vec![(0, 0, 10, 10)]
        );
    }

    #[test]
    fn physical_edges_round_consistently() {
        let bounds = Bounds::new(
            point(px(10.3), px(5.0)),
            gpui_kit::size(px(100.0), px(20.0)),
        );
        assert_eq!(physical(bounds, 1.25), (13, 6, 125, 25));
    }
}
