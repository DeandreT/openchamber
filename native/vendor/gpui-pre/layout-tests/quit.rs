//! Public-API regressions for quit preparation while windows and foreground work stay live.

use std::cell::{Cell, RefCell};
use std::future;
use std::rc::Rc;
use std::task::{Poll, Waker};

use gpui::{
    AppContext as _, Context, Empty, IntoElement, QuitMode, Render, Subscription, TestAppContext,
    Window, WindowHandle, WindowOptions,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SaveOutcome {
    Saved,
    Failed,
}

#[derive(Clone, Default)]
struct SaveGate {
    outcome: Rc<Cell<Option<SaveOutcome>>>,
    waker: Rc<RefCell<Option<Waker>>>,
}

impl SaveGate {
    fn complete(&self, outcome: SaveOutcome) {
        self.outcome.set(Some(outcome));
        let waker = self.waker.borrow_mut().take();
        if let Some(waker) = waker {
            waker.wake();
        }
    }

    async fn wait(&self) -> SaveOutcome {
        future::poll_fn(|cx| {
            if let Some(outcome) = self.outcome.get() {
                Poll::Ready(outcome)
            } else {
                self.waker.borrow_mut().replace(cx.waker().clone());
                Poll::Pending
            }
        })
        .await
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ClosePhase {
    Ready,
    Saving,
    Failed,
    Saved,
}

struct SavingView {
    gate: SaveGate,
    phase: ClosePhase,
    attempts: usize,
    completions: usize,
    _quit_guard: Subscription,
}

impl SavingView {
    fn request_close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.phase != ClosePhase::Ready {
            return;
        }
        self.phase = ClosePhase::Saving;
        self.attempts += 1;
        let gate = self.gate.clone();
        cx.spawn_in(window, async move |view, cx| {
            let outcome = gate.wait().await;
            view.update_in(cx, |view, window, cx| {
                view.completions += 1;
                match outcome {
                    SaveOutcome::Saved => {
                        view.phase = ClosePhase::Saved;
                        window.remove_window();
                    }
                    SaveOutcome::Failed => {
                        view.phase = ClosePhase::Failed;
                        cx.notify();
                    }
                }
            })
            .expect("a veto preserves the window for its foreground save completion");
        })
        .detach();
    }
}

impl Render for SavingView {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        Empty
    }
}

fn saving_window(cx: &mut TestAppContext, gate: SaveGate) -> WindowHandle<SavingView> {
    cx.add_window(move |window, cx| SavingView {
        gate,
        phase: ClosePhase::Ready,
        attempts: 0,
        completions: 0,
        _quit_guard: cx.on_request_quit_in(window, |view: &mut SavingView, window, cx| {
            view.request_close(window, cx);
            false
        }),
    })
}

fn quit_after_last_window(cx: &TestAppContext) -> Subscription {
    cx.update(|cx| {
        cx.set_quit_mode(QuitMode::Explicit);
        cx.on_window_closed(|cx, _| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
    })
}

fn observe_window_counts(cx: &TestAppContext, observed: Rc<RefCell<Vec<usize>>>) -> Subscription {
    cx.update(|cx| {
        cx.on_request_quit(move |cx| {
            observed.borrow_mut().push(cx.windows().len());
            true
        })
    })
}

#[gpui::test]
fn persistent_veto_keeps_windows_and_notifies_all_callbacks_once_per_request(
    cx: &mut TestAppContext,
) {
    let window = cx.add_window(|_, _| Empty);
    let observed = Rc::new(RefCell::new(Vec::new()));
    let veto = cx.update(|cx| {
        let observed = observed.clone();
        cx.on_request_quit(move |_| {
            observed.borrow_mut().push("veto");
            false
        })
    });
    let _consent = cx.update(|cx| {
        let observed = observed.clone();
        cx.on_request_quit(move |cx| {
            observed.borrow_mut().push("consent");
            cx.defer(|_| {});
            true
        })
    });

    cx.read(|cx| {
        cx.quit();
        cx.quit();
        cx.quit();
    });
    cx.run_until_parked();
    assert_eq!(
        observed.borrow().as_slice(),
        &["veto", "consent"],
        "queued requests coalesce and a veto does not skip the next callback"
    );
    assert!(window.read_with(cx, |_, _| ()).is_ok());

    cx.read(|cx| cx.quit());
    cx.run_until_parked();
    assert_eq!(
        observed.borrow().as_slice(),
        &["veto", "consent", "veto", "consent"],
        "a veto remains subscribed for a later request"
    );
    assert!(window.read_with(cx, |_, _| ()).is_ok());

    drop(veto);
    cx.read(|cx| cx.quit());
    cx.run_until_parked();
    assert_eq!(
        observed.borrow().as_slice(),
        &["veto", "consent", "veto", "consent", "consent"],
        "dropping the subscription removes only its callback"
    );
    cx.read(|cx| cx.quit());
    cx.run_until_parked();
    assert_eq!(
        observed.borrow().len(),
        5,
        "unrelated deferred effects preserve the accepted consent"
    );
}

#[gpui::test]
fn foreground_save_updates_its_view_before_close_and_final_quit(cx: &mut TestAppContext) {
    let observed = Rc::new(RefCell::new(Vec::new()));
    let _observer = observe_window_counts(cx, observed.clone());
    let _closed = quit_after_last_window(cx);
    let gate = SaveGate::default();
    let window = saving_window(cx, gate.clone());
    // Keep the entity and its guard alive after close to exercise the captured window identity.
    let view = window.entity(cx).expect("the window has a root view");

    window
        .update(cx, |_, _, cx| cx.quit())
        .expect("quit can be requested inside the guarded window and entity update");
    cx.run_until_parked();
    assert_eq!(view.read_with(cx, |view, _| view.phase), ClosePhase::Saving);
    assert_eq!(view.read_with(cx, |view, _| view.completions), 0);
    assert!(window.read_with(cx, |_, _| ()).is_ok());

    cx.read(|cx| cx.quit());
    cx.run_until_parked();
    assert_eq!(view.read_with(cx, |view, _| view.attempts), 1);
    assert_eq!(observed.borrow().as_slice(), &[1, 1]);

    gate.complete(SaveOutcome::Saved);
    cx.run_until_parked();
    assert_eq!(view.read_with(cx, |view, _| view.phase), ClosePhase::Saved);
    assert_eq!(view.read_with(cx, |view, _| view.completions), 1);
    assert!(window.read_with(cx, |_, _| ()).is_err());
    assert_eq!(
        observed.borrow().as_slice(),
        &[1, 1, 0],
        "last-window close reissues quit after foreground preparation finishes"
    );
}

#[gpui::test]
fn failed_save_stays_open_until_an_explicit_retry_succeeds(cx: &mut TestAppContext) {
    let observed = Rc::new(RefCell::new(Vec::new()));
    let _observer = observe_window_counts(cx, observed.clone());
    let _closed = quit_after_last_window(cx);
    let gate = SaveGate::default();
    let window = saving_window(cx, gate.clone());
    let view = window.entity(cx).expect("the window has a root view");

    cx.read(|cx| cx.quit());
    cx.run_until_parked();
    gate.complete(SaveOutcome::Failed);
    cx.run_until_parked();
    assert_eq!(view.read_with(cx, |view, _| view.phase), ClosePhase::Failed);
    assert_eq!(view.read_with(cx, |view, _| view.completions), 1);
    assert!(window.read_with(cx, |_, _| ()).is_ok());

    cx.read(|cx| cx.quit());
    cx.run_until_parked();
    assert_eq!(view.read_with(cx, |view, _| view.attempts), 1);
    assert_eq!(view.read_with(cx, |view, _| view.phase), ClosePhase::Failed);
    assert_eq!(observed.borrow().as_slice(), &[1, 1]);

    let retry = SaveGate::default();
    window
        .update(cx, |view, _, _| {
            view.gate = retry.clone();
            view.phase = ClosePhase::Ready;
        })
        .expect("a failed close leaves its view available for an explicit retry");
    cx.read(|cx| cx.quit());
    cx.run_until_parked();
    assert_eq!(view.read_with(cx, |view, _| view.attempts), 2);
    retry.complete(SaveOutcome::Saved);
    cx.run_until_parked();
    assert_eq!(view.read_with(cx, |view, _| view.phase), ClosePhase::Saved);
    assert_eq!(view.read_with(cx, |view, _| view.completions), 2);
    assert!(window.read_with(cx, |_, _| ()).is_err());
    assert_eq!(observed.borrow().as_slice(), &[1, 1, 1, 0]);
}

#[gpui::test]
fn synchronous_close_reissues_quit_without_reentering_the_window_guard(
    cx: &mut TestAppContext,
) {
    let observed = Rc::new(RefCell::new(Vec::new()));
    let _observer = observe_window_counts(cx, observed.clone());
    let _closed = quit_after_last_window(cx);
    let window = cx.add_window(|_, _| Empty);
    let view = window.entity(cx).expect("the window has a root view");
    let calls = Rc::new(Cell::new(0));
    let _guard = window
        .update(cx, |_, window, cx| {
            let calls = calls.clone();
            cx.on_request_quit_in(window, move |_, window, cx| {
                calls.set(calls.get() + 1);
                window.remove_window();
                cx.quit();
                false
            })
        })
        .expect("the open window can install its guard");

    cx.read(|cx| cx.quit());
    cx.run_until_parked();
    assert_eq!(
        calls.get(),
        1,
        "the window guard must not be entered recursively"
    );
    assert!(window.read_with(cx, |_, _| ()).is_err());
    assert!(
        view.downgrade().upgrade().is_some(),
        "the guard's entity remains retained"
    );
    assert_eq!(
        observed.borrow().as_slice(),
        &[1, 0],
        "the closed window permits the fresh preflight while its subscription is retained"
    );
    cx.read(|cx| cx.quit());
    cx.run_until_parked();
    assert_eq!(calls.get(), 1);
    assert_eq!(observed.borrow().as_slice(), &[1, 0]);
}

#[gpui::test]
fn retained_subscription_does_not_keep_its_released_entity_vetoing(cx: &mut TestAppContext) {
    let window = cx.add_window(|_, _| Empty);
    let calls = Rc::new(Cell::new(0));
    let mut subscription = None;
    let owner = window
        .update(cx, |_, window, cx| {
            cx.new(|cx| {
                let calls = calls.clone();
                subscription = Some(cx.on_request_quit_in(window, move |_, _, _| {
                    calls.set(calls.get() + 1);
                    false
                }));
            })
        })
        .expect("an entity can register a guard for a live window");
    let owner_weak = owner.downgrade();
    let observed = Rc::new(RefCell::new(Vec::new()));
    let _observer = observe_window_counts(cx, observed.clone());

    cx.read(|cx| cx.quit());
    cx.run_until_parked();
    assert_eq!(calls.get(), 1);
    assert!(window.read_with(cx, |_, _| ()).is_ok());
    drop(owner);
    cx.update(|_| {});
    assert!(
        owner_weak.upgrade().is_none(),
        "the subscription must hold its entity weakly"
    );
    cx.read(|cx| cx.quit());
    cx.run_until_parked();
    assert_eq!(
        calls.get(),
        1,
        "released entities no longer receive quit requests"
    );
    assert_eq!(observed.borrow().as_slice(), &[1, 1]);
    cx.read(|cx| cx.quit());
    cx.run_until_parked();
    assert_eq!(observed.borrow().as_slice(), &[1, 1]);
    drop(subscription);
}

#[gpui::test]
fn reissuing_quit_from_a_persistent_veto_does_not_spin(cx: &mut TestAppContext) {
    let window = cx.add_window(|_, _| Empty);
    let calls = Rc::new(Cell::new(0));
    let _guard = cx.update(|cx| {
        let calls = calls.clone();
        cx.on_request_quit(move |cx| {
            calls.set(calls.get() + 1);
            cx.quit();
            false
        })
    });

    for expected in 1..=2 {
        cx.read(|cx| cx.quit());
        cx.run_until_parked();
        assert_eq!(
            calls.get(),
            expected,
            "a persistent veto never schedules itself again"
        );
        assert!(window.read_with(cx, |_, _| ()).is_ok());
    }
}

#[gpui::test]
fn deferred_new_window_gets_its_quit_guard_checked_before_consent_is_accepted(
    cx: &mut TestAppContext,
) {
    let original = cx.add_window(|_, _| Empty);
    let opened = Rc::new(RefCell::new(None));
    let original_calls = Rc::new(Cell::new(0));
    let new_guard_calls = Rc::new(Cell::new(0));
    let _guard = cx.update(|cx| {
        let opened = opened.clone();
        let original_calls = original_calls.clone();
        let new_guard_calls = new_guard_calls.clone();
        cx.on_request_quit(move |cx| {
            original_calls.set(original_calls.get() + 1);
            if original_calls.get() == 1 {
                let opened = opened.clone();
                let new_guard_calls = new_guard_calls.clone();
                cx.defer(move |cx| {
                    let window = cx
                        .open_window(WindowOptions::default(), |window, cx| {
                            cx.new(|cx| {
                                cx.on_request_quit_in(window, move |_, _, _| {
                                    new_guard_calls.set(new_guard_calls.get() + 1);
                                    false
                                })
                                .detach();
                                Empty
                            })
                        })
                        .expect("the consenting callback opens a real deferred window");
                    opened.borrow_mut().replace(window);
                });
            }
            true
        })
    });

    cx.read(|cx| cx.quit());
    cx.run_until_parked();
    assert_eq!(
        original_calls.get(),
        2,
        "consent is checked again after the deferred open"
    );
    assert_eq!(
        new_guard_calls.get(),
        1,
        "the deferred window can veto the pending quit"
    );
    assert!(original.read_with(cx, |_, _| ()).is_ok());
    let window = opened.borrow().expect("the deferred window was opened");
    assert!(window.read_with(cx, |_, _| ()).is_ok());

    cx.read(|cx| cx.quit());
    cx.run_until_parked();
    assert_eq!(original_calls.get(), 3);
    assert_eq!(
        new_guard_calls.get(),
        2,
        "a persistent new veto does not retry itself"
    );
    assert!(window.read_with(cx, |_, _| ()).is_ok());
}

#[gpui::test]
fn deferred_guard_on_an_existing_window_invalidates_earlier_consent(cx: &mut TestAppContext) {
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
                    window
                        .update(cx, |_, window, cx| {
                            cx.on_request_quit_in(window, move |_, _, _| {
                                new_guard_calls.set(new_guard_calls.get() + 1);
                                false
                            })
                            .detach();
                        })
                        .expect("the deferred guard attaches to the existing window");
                });
            }
            true
        })
    });

    cx.read(|cx| cx.quit());
    cx.run_until_parked();
    assert_eq!(
        original_calls.get(),
        2,
        "a new guard requires a fresh consent check"
    );
    assert_eq!(new_guard_calls.get(), 1);
    assert!(window.read_with(cx, |_, _| ()).is_ok());
    assert_eq!(
        cx.read(|cx| cx.windows().len()),
        1,
        "no additional window was opened"
    );

    cx.read(|cx| cx.quit());
    cx.run_until_parked();
    assert_eq!(original_calls.get(), 3);
    assert_eq!(new_guard_calls.get(), 2);
}

#[gpui::test]
fn default_quit_finishes_cleanup_once_when_the_platform_callback_arrives(cx: &mut TestAppContext) {
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
    assert_eq!(cx.platform_quit_count(), 1);
    assert!(window.read_with(cx, |_, _| ()).is_ok());
    assert_eq!(finalized.get(), 0, "platform quit waits for its final callback");

    assert!(cx.simulate_platform_quit());
    assert!(window.read_with(cx, |_, _| ()).is_err());
    assert_eq!(finalized.get(), 1, "cleanup completes before callback return");
    assert!(cx.simulate_platform_quit());
    assert_eq!(finalized.get(), 1, "repeated final callbacks run cleanup once");
}

#[gpui::test]
fn late_idle_window_and_veto_cannot_cancel_committed_final_cleanup(cx: &mut TestAppContext) {
    cx.update(|cx| cx.set_quit_mode(QuitMode::Explicit));
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
    assert_eq!(cx.platform_quit_count(), 1, "preflight reached Platform::quit");
    assert_eq!(finalized.get(), 0);

    let opened = Rc::new(RefCell::new(None));
    let veto_calls = Rc::new(Cell::new(0));
    cx.update(|cx| {
        let opened = opened.clone();
        let veto_calls = veto_calls.clone();
        cx.defer(move |cx| {
            let window = cx
                .open_window(WindowOptions::default(), |window, cx| {
                    cx.new(|cx| {
                        cx.on_request_quit_in(window, move |_, _, _| {
                            veto_calls.set(veto_calls.get() + 1);
                            false
                        })
                        .detach();
                        Empty
                    })
                })
                .expect("remaining idle work opens a window after the platform quit request");
            opened.borrow_mut().replace(window);
        });
    });
    let late = opened.borrow().expect("the idle window was opened");
    assert!(late.read_with(cx, |_, _| ()).is_ok());
    cx.update(|_| assert!(!cx.simulate_platform_quit()));
    assert_eq!(finalized.get(), 0);

    assert!(cx.simulate_platform_quit(), "post-loop final cleanup must proceed");
    assert!(original.read_with(cx, |_, _| ()).is_err());
    assert!(late.read_with(cx, |_, _| ()).is_err());
    assert_eq!(finalized.get(), 1, "cleanup completes before callback return");
    assert!(cx.simulate_platform_quit());
    cx.read(|cx| cx.quit());
    cx.run_until_parked();
    assert_eq!(cx.platform_quit_count(), 1, "committed quit is coalesced");
    assert_eq!(finalized.get(), 1, "repeated final callbacks run cleanup once");
    assert_eq!(veto_calls.get(), 0);
}

#[gpui::test]
fn os_quit_keeps_veto_and_borrow_rejection_until_consent(cx: &mut TestAppContext) {
    let window = cx.add_window(|_, _| Empty);
    let veto_calls = Rc::new(Cell::new(0));
    let veto = cx.update(|cx| {
        let veto_calls = veto_calls.clone();
        cx.on_request_quit(move |_| {
            veto_calls.set(veto_calls.get() + 1);
            false
        })
    });
    let finalized = Rc::new(Cell::new(0));
    let _finalizer = cx.update(|cx| {
        let finalized = finalized.clone();
        cx.on_app_quit(move |_| {
            let finalized = finalized.clone();
            async move { finalized.set(finalized.get() + 1) }
        })
    });

    cx.update(|_| assert!(!cx.simulate_platform_quit()));
    assert_eq!(veto_calls.get(), 0, "borrow rejection does not enter preflight");
    for expected in 1..=2 {
        assert!(!cx.simulate_platform_quit(), "an OS request remains cancellable");
        cx.run_until_parked();
        assert_eq!(veto_calls.get(), expected);
        assert!(window.read_with(cx, |_, _| ()).is_ok());
        assert_eq!(finalized.get(), 0);
        assert_eq!(cx.platform_quit_count(), 0);
    }

    drop(veto);
    assert!(cx.simulate_platform_quit(), "releasing the veto permits OS quit");
    assert!(window.read_with(cx, |_, _| ()).is_err());
    assert_eq!(finalized.get(), 1);
    assert!(cx.simulate_platform_quit());
    assert_eq!(finalized.get(), 1);
    assert_eq!(cx.platform_quit_count(), 0, "OS quit needs no App::quit call");
}

#[gpui::test]
fn os_quit_rechecks_deferred_guards_before_committing(cx: &mut TestAppContext) {
    let window = cx.add_window(|_, _| Empty);
    let new_guard_calls = Rc::new(Cell::new(0));
    let original_calls = Rc::new(Cell::new(0));
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

    assert!(!cx.simulate_platform_quit(), "deferred guards revoke OS consent");
    assert!(window.read_with(cx, |_, _| ()).is_ok());
    cx.run_until_parked();
    assert_eq!(original_calls.get(), 2);
    assert_eq!(new_guard_calls.get(), 1);
    assert_eq!(cx.platform_quit_count(), 0);
    assert!(window.read_with(cx, |_, _| ()).is_ok());
}
