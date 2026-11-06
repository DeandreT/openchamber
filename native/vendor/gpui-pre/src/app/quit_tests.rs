//! Quit preflight regressions using the deterministic scheduler and real window/entity updates.

use std::cell::{Cell, RefCell};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use anyhow::Result;
use futures::channel::oneshot;

use super::{Application, GpuiMode};
use crate::{
    Action, ActivityGuard, AnyWindowHandle, AppContext, BackgroundExecutor, ClipboardItem,
    Context, CursorStyle, Empty, ForegroundExecutor, IntoElement, Keymap, Menu, MenuItem,
    PathPromptOptions, Platform, PlatformDisplay, PlatformKeyboardLayout, PlatformKeyboardMapper,
    PlatformTextSystem, PlatformWindow, QuitMode, Render, Subscription, Task, TestAppContext,
    ThermalState, Window, WindowAppearance, WindowHandle, WindowOptions, WindowParams,
};

type QuitCallback = Box<dyn FnMut() -> bool>;

enum QuitCallbackTiming {
    AfterLoop,
    Immediate,
}

// Keep TestPlatform's deterministic windows and executors, replacing only quit handling.
struct QuitPlatform {
    inner: Rc<dyn Platform>,
    callback: RefCell<Option<QuitCallback>>,
    quit_calls: Cell<usize>,
    stopped: Cell<bool>,
    callback_timing: QuitCallbackTiming,
}

impl QuitPlatform {
    fn request_os_quit(&self) -> bool {
        let mut callback = self
            .callback
            .borrow_mut()
            .take()
            .expect("quit callback is installed");
        let allowed = callback();
        self.callback.borrow_mut().replace(callback);
        if allowed {
            self.stopped.set(true);
        }
        allowed
    }
}

macro_rules! forward_platform {
    ($(fn $name:ident(&self $(, $arg:ident: $ty:ty)*) $(-> $result:ty)?;)*) => {
        $(fn $name(&self $(, $arg: $ty)*) $(-> $result)? {
            self.inner.$name($($arg),*)
        })*
    };
}

impl Platform for QuitPlatform {
    fn quit(&self) {
        self.quit_calls.set(self.quit_calls.get() + 1);
        if matches!(self.callback_timing, QuitCallbackTiming::Immediate) {
            self.request_os_quit();
        } else {
            // Linux stops its loop here and invokes on_quit only after the loop returns.
            self.stopped.set(true);
        }
    }

    fn on_quit(&self, callback: QuitCallback) {
        self.callback.borrow_mut().replace(callback);
    }

    forward_platform! {
        fn background_executor(&self) -> BackgroundExecutor;
        fn foreground_executor(&self) -> ForegroundExecutor;
        fn text_system(&self) -> Arc<dyn PlatformTextSystem>;
        fn run(&self, callback: Box<dyn FnOnce()>);
        fn restart(&self, path: Option<PathBuf>, arguments: Vec<OsString>);
        fn activate(&self, ignoring_other_apps: bool);
        fn hide(&self);
        fn hide_other_apps(&self);
        fn unhide_other_apps(&self);
        fn displays(&self) -> Vec<Rc<dyn PlatformDisplay>>;
        fn primary_display(&self) -> Option<Rc<dyn PlatformDisplay>>;
        fn on_displays_changed(&self, callback: Box<dyn FnMut()>);
        fn active_window(&self) -> Option<AnyWindowHandle>;
        fn open_window(&self, handle: AnyWindowHandle, options: WindowParams) -> Result<Box<dyn PlatformWindow>>;
        fn window_appearance(&self) -> WindowAppearance;
        fn open_url(&self, url: &str);
        fn on_open_urls(&self, callback: Box<dyn FnMut(Vec<String>)>);
        fn register_url_scheme(&self, url: &str) -> Task<Result<()>>;
        fn prompt_for_paths(&self, options: PathPromptOptions) -> oneshot::Receiver<Result<Option<Vec<PathBuf>>>>;
        fn prompt_for_new_path(&self, directory: &Path, suggested_name: Option<&str>) -> oneshot::Receiver<Result<Option<PathBuf>>>;
        fn can_select_mixed_files_and_dirs(&self) -> bool;
        fn reveal_path(&self, path: &Path);
        fn open_with_system(&self, path: &Path);
        fn on_reopen(&self, callback: Box<dyn FnMut()>);
        fn on_system_sleep(&self, callback: Box<dyn FnMut()>);
        fn on_system_wake(&self, callback: Box<dyn FnMut()>);
        fn set_menus(&self, menus: Vec<Menu>, keymap: &Keymap);
        fn set_dock_menu(&self, menu: Vec<MenuItem>, keymap: &Keymap);
        fn on_app_menu_action(&self, callback: Box<dyn FnMut(&dyn Action)>);
        fn on_will_open_app_menu(&self, callback: Box<dyn FnMut()>);
        fn on_validate_app_menu_command(&self, callback: Box<dyn FnMut(&dyn Action) -> bool>);
        fn thermal_state(&self) -> ThermalState;
        fn on_thermal_state_change(&self, callback: Box<dyn FnMut()>);
        fn prevent_idle_sleep(&self, reason: &str) -> Task<Result<ActivityGuard>>;
        fn app_path(&self) -> Result<PathBuf>;
        fn path_for_auxiliary_executable(&self, name: &str) -> Result<PathBuf>;
        fn set_cursor_style(&self, style: CursorStyle);
        fn hide_cursor_until_mouse_moves(&self);
        fn is_cursor_visible(&self) -> bool;
        fn should_auto_hide_scrollbars(&self) -> bool;
        fn read_from_clipboard(&self) -> Option<ClipboardItem>;
        fn write_to_clipboard(&self, item: ClipboardItem);
        fn write_credentials(&self, url: &str, username: &str, password: &[u8]) -> Task<Result<()>>;
        fn read_credentials(&self, url: &str) -> Task<Result<Option<(String, Vec<u8>)>>>;
        fn delete_credentials(&self, url: &str) -> Task<Result<()>>;
        fn keyboard_layout(&self) -> Box<dyn PlatformKeyboardLayout>;
        fn keyboard_mapper(&self) -> Rc<dyn PlatformKeyboardMapper>;
        fn on_keyboard_layout_change(&self, callback: Box<dyn FnMut()>);
    }

    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    forward_platform! {
        fn read_from_primary(&self) -> Option<ClipboardItem>;
        fn write_to_primary(&self, item: ClipboardItem);
    }

    #[cfg(target_os = "macos")]
    forward_platform! {
        fn read_from_find_pasteboard(&self) -> Option<ClipboardItem>;
        fn write_to_find_pasteboard(&self, item: ClipboardItem);
    }
}

fn context(callback_timing: QuitCallbackTiming) -> (TestAppContext, Rc<QuitPlatform>) {
    let mut cx = TestAppContext::single();
    let platform = Rc::new(QuitPlatform {
        inner: cx.app.borrow().platform.clone(),
        callback: RefCell::new(None),
        quit_calls: Cell::new(0),
        stopped: Cell::new(false),
        callback_timing,
    });
    cx.app = Application::with_platform(platform.clone()).0;
    cx.app.borrow_mut().mode = GpuiMode::test();
    cx.update(|cx| cx.set_quit_mode(QuitMode::Explicit));
    (cx, platform)
}

#[test]
fn default_quit_reaches_platform_and_final_cleanup_once() {
    let (mut cx, platform) = context(QuitCallbackTiming::AfterLoop);
    let window = cx.add_window(|_, _| Empty);
    let finalized = Rc::new(Cell::new(0));
    let _finalizer = cx.update(|cx| {
        let finalized = finalized.clone();
        cx.on_app_quit(move |_| {
            let finalized = finalized.clone();
            async move { finalized.set(finalized.get() + 1) }
        })
    });

    cx.read(|cx| cx.quit());
    cx.run_until_parked();
    assert_eq!(platform.quit_calls.get(), 1);
    assert!(platform.stopped.get());
    assert!(window.read_with(&cx, |_, _| ()).is_ok());
    assert_eq!(finalized.get(), 0);

    assert!(platform.request_os_quit());
    assert!(window.read_with(&cx, |_, _| ()).is_err());
    assert!(platform.request_os_quit());
    assert_eq!(finalized.get(), 1, "final I/O cleanup must run once");
}

#[test]
fn stopped_loop_finishes_cleanup_once_after_a_late_window_and_veto() {
    let (mut cx, platform) = context(QuitCallbackTiming::AfterLoop);
    let original = cx.add_window(|_, _| Empty);
    let finalized = Rc::new(Cell::new(0));
    let _finalizer = cx.update(|cx| {
        let finalized = finalized.clone();
        cx.on_app_quit(move |_| {
            let finalized = finalized.clone();
            async move { finalized.set(finalized.get() + 1) }
        })
    });

    cx.read(|cx| cx.quit());
    cx.run_until_parked();
    assert_eq!(platform.quit_calls.get(), 1);
    assert!(platform.stopped.get(), "loop stop precedes the final callback");
    assert_eq!(finalized.get(), 0);

    // Linux may still drain idle work after Platform::quit has signalled its loop stop.
    let late = cx.add_window(|_, _| Empty);
    let veto_calls = Rc::new(Cell::new(0));
    let _veto = cx.update(|cx| {
        let veto_calls = veto_calls.clone();
        cx.on_request_quit(move |_| {
            veto_calls.set(veto_calls.get() + 1);
            false
        })
    });
    assert!(platform.request_os_quit(), "post-loop cleanup cannot be vetoed");
    assert!(original.read_with(&cx, |_, _| ()).is_err());
    assert!(late.read_with(&cx, |_, _| ()).is_err());
    assert_eq!(finalized.get(), 1, "cleanup completes before callback return");
    assert!(platform.request_os_quit());
    cx.read(|cx| cx.quit());
    cx.run_until_parked();
    assert_eq!(platform.quit_calls.get(), 1, "committed quit is coalesced");
    assert_eq!(finalized.get(), 1, "repeated final callbacks run cleanup once");
    assert_eq!(veto_calls.get(), 0);
}

#[test]
fn veto_retains_all_observers_until_subscription_is_dropped() {
    let (cx, platform) = context(QuitCallbackTiming::AfterLoop);
    let calls = Rc::new(Cell::new(0));
    let veto = cx.update(|cx| cx.on_request_quit(|_| false));
    let _consent = cx.update(|cx| {
        let calls = calls.clone();
        cx.on_request_quit(move |_| {
            calls.set(calls.get() + 1);
            true
        })
    });

    for expected in 1..=2 {
        cx.read(|cx| cx.quit());
        cx.run_until_parked();
        assert_eq!(calls.get(), expected, "a veto must not skip other observers");
        assert_eq!(platform.quit_calls.get(), 0);
        assert!(!platform.stopped.get());
    }

    drop(veto);
    cx.read(|cx| cx.quit());
    cx.run_until_parked();
    assert_eq!(calls.get(), 3);
    assert_eq!(platform.quit_calls.get(), 1);
}

struct DelayedClose {
    completion: Option<oneshot::Receiver<bool>>,
    pending: bool,
    failed: bool,
    attempts: usize,
    completed: usize,
    _guard: Subscription,
}

impl DelayedClose {
    fn request_close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.pending {
            return;
        }
        let Some(completion) = self.completion.take() else {
            return;
        };
        self.pending = true;
        self.failed = false;
        self.attempts += 1;
        cx.spawn_in(window, async move |entity, cx| {
            let success = completion.await.expect("test releases the drain explicitly");
            entity
                .update_in(cx, |entity, window, _| {
                    entity.pending = false;
                    entity.failed = !success;
                    if success {
                        entity.completed += 1;
                        window.remove_window();
                    }
                })
                .expect("the veto keeps the window and entity available to foreground work");
        })
        .detach();
    }
}

impl Render for DelayedClose {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        Empty
    }
}

fn delayed_window(
    cx: &mut TestAppContext,
    completion: oneshot::Receiver<bool>,
) -> WindowHandle<DelayedClose> {
    cx.add_window(move |window, cx| DelayedClose {
        completion: Some(completion),
        pending: false,
        failed: false,
        attempts: 0,
        completed: 0,
        _guard: cx.on_request_quit_in(window, |entity, window, cx| {
            entity.request_close(window, cx);
            false
        }),
    })
}

fn quit_after_last_window(cx: &TestAppContext) -> Subscription {
    cx.update(|cx| {
        cx.on_window_closed(|cx, _| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
    })
}

#[test]
fn veto_preserves_foreground_drain_and_coalesces_pending_requests() {
    let (mut cx, platform) = context(QuitCallbackTiming::AfterLoop);
    let (release, completion) = oneshot::channel();
    let window = delayed_window(&mut cx, completion);
    // Retain the entity and its guard after close to prove the window identity is checked.
    let entity = window.entity(&cx).expect("window has a root");
    let _closed = quit_after_last_window(&cx);

    window
        .update(&mut cx, |_, _, cx| {
            cx.quit();
            cx.quit();
        })
        .expect("quit can be requested while its guarded entity and window are leased");
    cx.run_until_parked();
    cx.read(|cx| cx.quit());
    cx.run_until_parked();
    assert_eq!(entity.read_with(&cx, |entity, _| entity.attempts), 1);
    assert!(entity.read_with(&cx, |entity, _| entity.pending));
    assert!(window.read_with(&cx, |_, _| ()).is_ok());
    assert!(!platform.stopped.get());
    assert_eq!(platform.quit_calls.get(), 0);

    release.send(true).expect("drain is still waiting");
    cx.run_until_parked();
    assert_eq!(entity.read_with(&cx, |entity, _| entity.completed), 1);
    assert!(window.read_with(&cx, |_, _| ()).is_err());
    assert_eq!(platform.quit_calls.get(), 1);
    assert!(platform.request_os_quit());
}

#[test]
fn failed_drain_keeps_window_open_and_explicit_retry_can_quit() {
    let (mut cx, platform) = context(QuitCallbackTiming::AfterLoop);
    let (release, completion) = oneshot::channel();
    let window = delayed_window(&mut cx, completion);
    let _closed = quit_after_last_window(&cx);

    cx.read(|cx| cx.quit());
    cx.run_until_parked();
    release.send(false).expect("drain is waiting");
    cx.run_until_parked();
    assert!(
        window
            .read_with(&cx, |entity, _| entity.failed)
            .expect("failed close stays open")
    );
    assert_eq!(platform.quit_calls.get(), 0);
    assert!(!platform.stopped.get());

    cx.read(|cx| cx.quit());
    cx.run_until_parked();
    assert_eq!(
        platform.quit_calls.get(),
        0,
        "failure remains vetoed without a retry"
    );
    let (release_retry, retry) = oneshot::channel();
    window
        .update(&mut cx, |entity, _, _| entity.completion = Some(retry))
        .expect("failed window can accept a retry");
    cx.read(|cx| cx.quit());
    cx.run_until_parked();
    assert_eq!(
        window
            .read_with(&cx, |entity, _| entity.attempts)
            .expect("retry stays open"),
        2
    );
    release_retry.send(true).expect("retry drain is waiting");
    cx.run_until_parked();
    assert!(window.read_with(&cx, |_, _| ()).is_err());
    assert_eq!(platform.quit_calls.get(), 1);
}

#[test]
fn synchronous_last_window_close_rechecks_after_guard_returns() {
    let (mut cx, platform) = context(QuitCallbackTiming::Immediate);
    let window = cx.add_window(|_, _| Empty);
    let calls = Rc::new(Cell::new(0));
    let _guard = window
        .update(&mut cx, |_, window, cx| {
            let calls = calls.clone();
            cx.on_request_quit_in(window, move |_, window, _| {
                calls.set(calls.get() + 1);
                window.remove_window();
                false
            })
        })
        .expect("window is open");
    let _closed = quit_after_last_window(&cx);
    cx.update(|cx| cx.set_quit_mode(QuitMode::LastWindowClosed));

    cx.read(|cx| cx.quit());
    cx.run_until_parked();
    assert_eq!(calls.get(), 1, "the guard must not be entered recursively");
    assert!(window.read_with(&cx, |_, _| ()).is_err());
    assert_eq!(
        platform.quit_calls.get(),
        1,
        "duplicate close notifications are coalesced"
    );
    assert!(
        platform.stopped.get(),
        "synchronous platform callback can borrow the app"
    );
}

#[test]
fn nested_quit_without_closing_does_not_spin() {
    let (cx, platform) = context(QuitCallbackTiming::AfterLoop);
    let calls = Rc::new(Cell::new(0));
    let _guard = cx.update(|cx| {
        let calls = calls.clone();
        cx.on_request_quit(move |cx| {
            calls.set(calls.get() + 1);
            cx.quit();
            false
        })
    });

    cx.read(|cx| cx.quit());
    cx.run_until_parked();
    assert_eq!(calls.get(), 1);
    assert_eq!(platform.quit_calls.get(), 0);
}

#[test]
fn os_quit_is_vetoed_before_shutdown_and_can_finish_after_foreground_drain() {
    let (mut cx, platform) = context(QuitCallbackTiming::Immediate);
    let (release, completion) = oneshot::channel();
    let window = delayed_window(&mut cx, completion);
    let _closed = quit_after_last_window(&cx);

    assert!(!platform.request_os_quit());
    cx.run_until_parked();
    assert!(window.read_with(&cx, |_, _| ()).is_ok());
    assert!(!platform.stopped.get());
    release.send(true).expect("OS quit left foreground preparation alive");
    cx.run_until_parked();
    assert_eq!(platform.quit_calls.get(), 1);
    assert!(platform.stopped.get());
}

#[test]
fn os_quit_still_returns_false_when_app_is_borrowed() {
    let (cx, platform) = context(QuitCallbackTiming::Immediate);
    cx.update(|_| assert!(!platform.request_os_quit()));
    assert!(!platform.stopped.get());
    cx.read(|cx| cx.quit());
    cx.run_until_parked();
    assert!(platform.stopped.get());
}

#[test]
fn released_entity_does_not_veto_a_live_window() {
    let (mut cx, platform) = context(QuitCallbackTiming::AfterLoop);
    let window = cx.add_window(|_, _| Empty);
    let mut guard = None;
    let owner = window
        .update(&mut cx, |_, window, cx| {
            cx.new(|cx| {
                guard = Some(cx.on_request_quit_in(window, |_, _, _| false));
            })
        })
        .expect("window is open");
    drop(owner);

    cx.read(|cx| cx.quit());
    cx.run_until_parked();
    assert!(window.read_with(&cx, |_, _| ()).is_ok());
    assert_eq!(platform.quit_calls.get(), 1);
    drop(guard);
}

#[test]
fn deferred_window_invalidates_consent_before_platform_quit_without_registering_a_guard() {
    let (mut cx, platform) = context(QuitCallbackTiming::AfterLoop);
    let original = cx.add_window(|_, _| Empty);
    let opened = Rc::new(RefCell::new(None));
    let calls = Rc::new(Cell::new(0));
    let _guard = cx.update(|cx| {
        let opened = opened.clone();
        let calls = calls.clone();
        cx.on_request_quit(move |cx| {
            calls.set(calls.get() + 1);
            if calls.get() == 1 {
                let opened = opened.clone();
                cx.defer(move |cx| {
                    let window = cx
                        .open_window(WindowOptions::default(), |_, cx| cx.new(|_| Empty))
                        .expect("the deferred window opens without registering a new guard");
                    opened.borrow_mut().replace(window);
                });
            }
            cx.windows().len() == 1
        })
    });

    cx.read(|cx| cx.quit());
    cx.run_until_parked();
    assert_eq!(calls.get(), 2, "opening a window alone invalidates earlier consent");
    assert_eq!(platform.quit_calls.get(), 0);
    assert!(!platform.stopped.get());
    assert!(original.read_with(&cx, |_, _| ()).is_ok());
    let window = opened.borrow().expect("the deferred window was created");
    assert!(window.read_with(&cx, |_, _| ()).is_ok());
}

#[test]
fn os_quit_rechecks_a_guard_registered_by_deferred_effects_before_shutdown() {
    let (mut cx, platform) = context(QuitCallbackTiming::Immediate);
    let window = cx.add_window(|_, _| Empty);
    let original_calls = Rc::new(Cell::new(0));
    let new_guard_calls = Rc::new(Cell::new(0));
    let _guard = cx.update(|cx| {
        let original_calls = original_calls.clone();
        let new_guard_calls = new_guard_calls.clone();
        cx.on_request_quit(move |cx| {
            original_calls.set(original_calls.get() + 1);
            if original_calls.get() == 1 {
                let new_guard_calls = new_guard_calls.clone();
                cx.defer(move |cx| {
                    cx.on_request_quit(move |_| {
                        new_guard_calls.set(new_guard_calls.get() + 1);
                        false
                    })
                    .detach();
                });
            }
            true
        })
    });

    assert!(!platform.request_os_quit(), "deferred guards invalidate OS quit consent");
    assert!(window.read_with(&cx, |_, _| ()).is_ok());
    assert!(!platform.stopped.get());
    cx.run_until_parked();
    assert_eq!(original_calls.get(), 2);
    assert_eq!(new_guard_calls.get(), 1);
    assert_eq!(platform.quit_calls.get(), 0);
    assert!(!platform.stopped.get());
    assert!(window.read_with(&cx, |_, _| ()).is_ok());
}
