//! Accepted worktree transactions retain their I/O independently of dialog
//! continuations. Normal close waits for receipts; app quit takes the tasks.

use std::collections::HashMap;
use std::future::Future;

use openchamber_native_core::ids::RuntimeId;
use tokio::sync::{oneshot, watch};

use crate::network::NetworkTask;
use crate::project_worktrees::Recovery;

#[derive(Clone)]
pub(crate) enum Outcome {
    Removed,
    /// No cleanup remains, including a failed removal whose archive was restored.
    Failed(String),
    Unresolved(Box<Recovery>),
}

impl Outcome {
    pub(crate) fn settled(&self) -> bool {
        !matches!(self, Self::Unresolved(_))
    }

    pub(crate) fn error(&self) -> Option<&str> {
        match self {
            Self::Removed => None,
            Self::Failed(error) => Some(error),
            Self::Unresolved(recovery) => Some(&recovery.error),
        }
    }
}

struct Pending {
    runtime: RuntimeId,
    directory: String,
    network: tokio::runtime::Handle,
    result: watch::Receiver<Option<Outcome>>,
    io: NetworkTask<Outcome>,
}

impl Pending {
    fn new(
        runtime: RuntimeId,
        directory: String,
        network: &tokio::runtime::Handle,
        operation: impl Future<Output = Outcome> + Send + 'static,
    ) -> (Self, oneshot::Receiver<Outcome>) {
        let (completed, receipt) = oneshot::channel();
        let (result, waiting) = watch::channel(None);
        let io = NetworkTask::spawn_on(network, async move {
            let outcome = operation.await;
            result.send_replace(Some(outcome.clone()));
            let _ = completed.send(outcome.clone());
            outcome
        });
        (
            Self {
                runtime,
                directory,
                network: network.clone(),
                result: waiting,
                io,
            },
            receipt,
        )
    }

    fn settled(&self) -> bool {
        self.result.borrow().as_ref().is_some_and(Outcome::settled)
    }
}

#[derive(Default)]
pub(crate) struct Removals {
    next: u64,
    pending: HashMap<u64, Pending>,
}

impl Removals {
    pub(crate) fn start(
        &mut self,
        runtime: RuntimeId,
        directory: String,
        network: &tokio::runtime::Handle,
        operation: impl Future<Output = Outcome> + Send + 'static,
    ) -> Option<(u64, oneshot::Receiver<Outcome>)> {
        if self.pending.values().any(|pending| {
            pending.runtime == runtime && pending.directory == directory && !pending.settled()
        }) {
            return None;
        }
        let id = self.next;
        self.next += 1;
        let (pending, receipt) = Pending::new(runtime, directory, network, operation);
        self.pending.insert(id, pending);
        Some((id, receipt))
    }

    pub(crate) fn finish(&mut self, id: u64) {
        if self.pending.get(&id).is_some_and(Pending::settled) {
            self.pending.remove(&id);
        }
    }

    pub(crate) fn holds_runtime(&self, runtime: &RuntimeId) -> bool {
        self.pending
            .values()
            .any(|pending| &pending.runtime == runtime && !pending.settled())
    }

    pub(crate) fn retry(&mut self, id: u64) -> Option<oneshot::Receiver<Outcome>> {
        let pending = self.pending.get_mut(&id)?;
        let Some(Outcome::Unresolved(recovery)) = pending.result.borrow().clone() else {
            return None;
        };
        let (replacement, receipt) = Pending::new(
            pending.runtime.clone(),
            pending.directory.clone(),
            &pending.network,
            (*recovery).retry(),
        );
        *pending = replacement;
        Some(receipt)
    }

    pub(crate) fn first_recovery(
        &self,
    ) -> Option<(u64, RuntimeId, tokio::runtime::Handle, Box<Recovery>)> {
        self.pending.iter().find_map(|(id, pending)| {
            let Some(Outcome::Unresolved(recovery)) = pending.result.borrow().clone() else {
                return None;
            };
            Some((
                *id,
                pending.runtime.clone(),
                pending.network.clone(),
                recovery,
            ))
        })
    }

    pub(crate) fn for_project(&self, runtime: &RuntimeId, project: &str) -> Option<u64> {
        self.pending.iter().find_map(|(id, pending)| {
            let result = pending.result.borrow();
            match result.as_ref()? {
                Outcome::Unresolved(recovery)
                    if &pending.runtime == runtime && recovery.project == project =>
                {
                    Some(*id)
                }
                _ => None,
            }
        })
    }

    pub(crate) fn outcome(&self, id: u64) -> Option<Outcome> {
        self.pending.get(&id)?.result.borrow().clone()
    }

    pub(crate) fn receipts(&self) -> Vec<watch::Receiver<Option<Outcome>>> {
        self.pending
            .values()
            .filter(|pending| !pending.settled())
            .map(|pending| pending.result.clone())
            .collect()
    }

    pub(crate) fn into_tasks(self) -> Vec<NetworkTask<Outcome>> {
        self.pending
            .into_values()
            .map(|pending| pending.io)
            .collect()
    }
}

pub(crate) async fn wait(receipts: Vec<watch::Receiver<Option<Outcome>>>) -> Result<(), ()> {
    for mut receipt in receipts {
        loop {
            if let Some(outcome) = receipt.borrow_and_update().as_ref() {
                if !outcome.settled() {
                    return Err(());
                }
                break;
            }
            if receipt.changed().await.is_err() {
                return Err(());
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::network::Network;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    fn unresolved(error: &str) -> Outcome {
        use openchamber_native_transport::client::ServerClient;
        use openchamber_native_transport::credential::Credential;
        use openchamber_native_transport::endpoint::Endpoint;
        Outcome::Unresolved(Box::new(Recovery {
            client: ServerClient::new(
                Endpoint::parse("http://localhost:1").expect("the synthetic endpoint parses"),
                Credential::None,
            )
            .expect("the synthetic client initializes"),
            project: "/project".into(),
            target: "/worktree".into(),
            ids: vec![
                openchamber_native_core::ids::SessionId::new("ses_previous"),
                openchamber_native_core::ids::SessionId::new("ses_uncertain_current"),
            ],
            error: error.to_owned(),
        }))
    }

    #[test]
    fn failed_cleanup_retains_exact_ids_after_ui_finish_and_refuses_close() {
        let network = Network::new().expect("the isolated network starts");
        let mut removals = Removals::default();
        for error in ["restore failed", "worktree reconciliation failed"] {
            let (id, receipt) = removals
                .start(
                    RuntimeId::from("captured"),
                    format!("/worktree-{error}"),
                    &network.handle(),
                    async move { unresolved(error) },
                )
                .expect("each independent removal is admitted");
            let outcome = network
                .handle()
                .block_on(receipt)
                .expect("the result arrives");
            assert!(!outcome.settled(), "failed cleanup is unresolved");
            removals.finish(id);
            let Some(Outcome::Unresolved(recovery)) = removals.outcome(id) else {
                panic!("dialog completion must retain failed cleanup");
            };
            assert_eq!(
                recovery
                    .ids
                    .iter()
                    .map(|id| id.as_str())
                    .collect::<Vec<_>>(),
                ["ses_previous", "ses_uncertain_current"],
                "exact attempted IDs survive"
            );
            assert_eq!(
                recovery.project, "/project",
                "the original project survives"
            );
            assert!(
                network
                    .handle()
                    .block_on(wait(removals.receipts()))
                    .is_err(),
                "normal close refuses an unresolved restore or reconciliation"
            );
        }
        assert!(
            removals
                .start(
                    RuntimeId::from("captured"),
                    "/worktree-restore failed".into(),
                    &network.handle(),
                    async { Outcome::Removed },
                )
                .is_none(),
            "a completed but unresolved removal cannot be superseded"
        );
        assert!(
            removals.holds_runtime(&RuntimeId::from("captured")),
            "unresolved results retain their captured runtime"
        );
        let other = RuntimeId::from("another-runtime");
        assert!(
            !removals.holds_runtime(&other),
            "unrelated runtimes are not held"
        );
        let (_, receipt) = removals
            .start(
                other.clone(),
                "/worktree-restore failed".into(),
                &network.handle(),
                async { Outcome::Removed },
            )
            .expect("the same path on another runtime remains independent");
        network
            .handle()
            .block_on(receipt)
            .expect("the independent removal settles");
        assert!(
            !removals.holds_runtime(&other),
            "settled results release their runtime"
        );
    }

    fn refuse_unresolved_close(cx: &mut gpui_kit::TestAppContext, error: &'static str) {
        use gpui_kit::AppContext;
        let (window, workspace) = crate::workspace_tests::open(cx);
        let network = Network::new().expect("the captured network starts");
        let handle = network.handle();
        cx.update_window(window, |_, window, cx| {
            workspace.update(cx, |workspace, cx| {
                let (id, receipt) = workspace
                    .worktree_removals
                    .start(
                        workspace.runtime.clone(),
                        "/worktree".into(),
                        &handle,
                        async move { unresolved(error) },
                    )
                    .expect("the removal starts");
                handle
                    .block_on(receipt)
                    .expect("the unresolved result arrives");
                workspace.worktree_removals.finish(id);
                workspace
                    .retired_networks
                    .push((workspace.runtime.clone(), network));
                workspace.request_close(window, cx);
                assert!(
                    workspace.worktree_close.is_some(),
                    "normal close checks the receipt"
                );
            });
        })
        .expect("the initial close request keeps the window");
        cx.run_until_parked();
        cx.update_window(window, |_, _, cx| {
            let workspace = workspace.read(cx);
            assert!(
                workspace.worktree_close.is_none(),
                "the refused close stops waiting"
            );
            assert!(
                workspace.worktree_removals.first_recovery().is_some(),
                "custody survives normal close"
            );
            assert!(
                workspace
                    .worktree_manager
                    .as_ref()
                    .and_then(gpui_kit::WeakEntity::upgrade)
                    .is_some(),
                "normal close exposes the captured recovery manager"
            );
        })
        .expect("failed cleanup must keep the real Workspace window open");
    }

    #[gpui_kit::test]
    fn normal_close_refuses_failed_restore(cx: &mut gpui_kit::TestAppContext) {
        refuse_unresolved_close(cx, "restore failed");
    }

    #[gpui_kit::test]
    fn normal_close_refuses_failed_reconciliation(cx: &mut gpui_kit::TestAppContext) {
        refuse_unresolved_close(cx, "reconciliation failed");
    }

    #[gpui_kit::test]
    fn failed_recovery_retry_survives_workspace_runtime_retirement(
        cx: &mut gpui_kit::TestAppContext,
    ) {
        use gpui_kit::AppContext;
        use openchamber_native_transport::endpoint::Endpoint;
        use std::time::{Duration, Instant};

        let (window, workspace) = crate::workspace_tests::open(cx);
        let (id, handle) = cx
            .update_window(window, |_, _, cx| {
                workspace.update(cx, |workspace, _| {
                    let original = crate::connection::Connection::new(
                        crate::connection::ServerConfig::for_endpoint(
                            Endpoint::parse("http://localhost:1").expect("original endpoint"),
                        ),
                    )
                    .expect("the original connection owns its runtime");
                    let handle = original.network.handle();
                    workspace.connection = Some(original);
                    let (id, receipt) = workspace
                        .worktree_removals
                        .start(
                            workspace.runtime.clone(),
                            "/worktree".into(),
                            &handle,
                            async { unresolved("initial restore failed") },
                        )
                        .expect("the original removal starts");
                    handle
                        .block_on(receipt)
                        .expect("the original removal reaches unresolved");
                    workspace.worktree_removals.finish(id);
                    let deadline = Instant::now() + Duration::from_secs(3);
                    while !workspace
                        .connection
                        .as_ref()
                        .expect("original")
                        .network
                        .is_idle()
                    {
                        assert!(
                            Instant::now() < deadline,
                            "the completed operation must become idle"
                        );
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    (id, handle)
                })
            })
            .expect("the original workspace stays open");

        let receipt = cx
            .update_window(window, |_, window, cx| {
                workspace.update(cx, |workspace, cx| {
                    workspace.connect_to_host(
                        Endpoint::parse("http://localhost:2").expect("replacement endpoint"),
                        window,
                        cx,
                    );
                    // This regression exercises the old executor's retirement. The
                    // replacement's live authentication would wake GPUI's deterministic
                    // foreground scheduler from a Tokio thread, so cancel it before
                    // that foreground task is polled. Original recovery I/O stays live.
                    workspace
                        .connection
                        .as_mut()
                        .expect("replacement connection")
                        .connect_task = None;
                    workspace.switch_runtime(RuntimeId::from("replacement-host"), window, cx);
                    workspace.release_retired_networks();
                    workspace
                        .worktree_removals
                        .retry(id)
                        .expect("the exact recovery is retryable")
                })
            })
            .expect("the replacement workspace stays open");
        let outcome = handle
            .block_on(receipt)
            .expect("retiring an idle connection must not discard the captured recovery executor");
        assert!(
            matches!(outcome, Outcome::Unresolved(_)),
            "the original unavailable server returns a typed retry failure, preserving custody"
        );
        cx.update(|cx| {
            workspace.update(cx, |workspace, _| {
                workspace.worktree_removals.finish(id);
                workspace.release_retired_networks();
                assert_eq!(
                    workspace.retired_networks.len(),
                    1,
                    "failed retry still owns the original runtime"
                );
                let Some(Outcome::Unresolved(recovery)) = workspace.worktree_removals.outcome(id)
                else {
                    panic!("failed retry retains exact recovery custody");
                };
                assert_eq!(
                    recovery
                        .ids
                        .iter()
                        .map(|id| id.as_str())
                        .collect::<Vec<_>>(),
                    ["ses_previous", "ses_uncertain_current"],
                    "retry keeps original IDs"
                );
                assert_eq!(
                    recovery.client.endpoint().as_str(),
                    "http://localhost:1/",
                    "retry keeps the original server"
                );
            });
        });
    }

    #[test]
    fn closing_a_dialog_and_taking_quit_tasks_preserves_the_rollback_chain() {
        let network = Network::new().expect("the isolated network starts");
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        let reached = Arc::new(AtomicBool::new(false));
        let restored = Arc::new(AtomicBool::new(false));
        let mut removals = Removals::default();
        let operation_gate = gate.clone();
        let operation_reached = reached.clone();
        let operation_restored = restored.clone();
        let (_, ui) = removals
            .start(
                RuntimeId::from("r"),
                "/worktree".into(),
                &network.handle(),
                async move {
                    operation_reached.store(true, Ordering::SeqCst);
                    let _permit = operation_gate
                        .acquire()
                        .await
                        .expect("the test gate stays open");
                    operation_restored.store(true, Ordering::SeqCst);
                    Outcome::Failed("DELETE failed after archive; rollback completed".into())
                },
            )
            .expect("the first transaction is accepted");
        drop(ui);
        let receipts = removals.receipts();
        let tasks = removals.into_tasks();
        network.handle().block_on(async {
            while !reached.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
            assert!(
                !restored.load(Ordering::SeqCst),
                "held removal has not rolled back yet"
            );
            gate.add_permits(1);
            wait(receipts).await.expect("rollback settled");
            for task in tasks {
                assert!(matches!(
                    task.await.expect("quit retained I/O"),
                    Outcome::Failed(_)
                ));
            }
        });
        assert!(
            restored.load(Ordering::SeqCst),
            "quit drains rollback before releasing the network"
        );
    }

    fn workspace_quit(cx: &mut gpui_kit::TestAppContext, normal_close_first: bool) {
        use gpui_kit::AppContext;
        let (window, workspace) = crate::workspace_tests::open(cx);
        let network = Network::new().expect("the operation's network starts");
        let handle = network.handle();
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        let completed = Arc::new(AtomicBool::new(false));
        let operation_gate = gate.clone();
        let operation_completed = completed.clone();
        cx.update_window(window, |_, window, cx| {
            workspace.update(cx, |workspace, cx| {
                let (_, ui) = workspace
                    .worktree_removals
                    .start(
                        workspace.runtime.clone(),
                        "/held-worktree".into(),
                        &handle,
                        async move {
                            let _permit = operation_gate
                                .acquire()
                                .await
                                .expect("the quit gate remains available");
                            operation_completed.store(true, Ordering::SeqCst);
                            Outcome::Failed("failed DELETE rolled back".into())
                        },
                    )
                    .expect("the transaction starts");
                drop(ui);
                workspace
                    .retired_networks
                    .push((workspace.runtime.clone(), network));
                if normal_close_first {
                    workspace.request_close(window, cx);
                    assert!(
                        workspace.worktree_close.is_some(),
                        "normal close waits for the transaction"
                    );
                }
            });
        })
        .expect("the window remains open while DELETE is held");
        cx.update(|cx| {
            cx.on_app_quit(move |_| {
                gate.add_permits(1);
                async {}
            })
            .detach();
        });
        cx.executor().allow_parking();
        cx.quit();
        assert!(
            completed.load(Ordering::SeqCst),
            "the real Workspace quit callback drains independent transaction I/O"
        );
    }

    #[gpui_kit::test]
    fn workspace_app_quit_retains_transaction_io(cx: &mut gpui_kit::TestAppContext) {
        workspace_quit(cx, false);
    }

    #[gpui_kit::test]
    fn app_quit_retains_io_already_waited_by_normal_close(cx: &mut gpui_kit::TestAppContext) {
        workspace_quit(cx, true);
    }
}
