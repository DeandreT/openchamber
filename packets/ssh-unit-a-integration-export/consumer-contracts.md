# Unit A retained-source and consumer contracts

This companion maps the completed qualified export to
`native/target/tmp/ssh-terminal-reconcile-96ac884e7d/boundaries.md`.
It adds no source or archive payload. `consumer-bindings.json` binds each named
prerequisite to the existing qualified input manifest and retained worktree.
The original archive and its manifest remain unchanged.

## Qualified payload

`unit-a.patch` contains exactly 8 paths. Apply scoped hunks after reconciling
their prerequisites with coordinator main. No whole inherited Workspace,
main or lifecycle file is an integration payload.

- `ssh_cleanup.rs` supplies the full private typed report module and its tests.
- `ssh_host.rs` supplies report production, observed daemon identity,
  existing-CLI JSON handling and focused cleanup tests.
- `ssh_view.rs` changes only the completion-pruning predicate.
- `workspace.rs` changes only the receipt-vector type and completion predicate.
- `main.rs` adds only `mod ssh_cleanup;`.
- Desktop `DOCUMENTATION.md` and the 2 maintained fixture files supply the
  scoped documentation and reproduction contracts.

## Report interface

The qualified module defines these crate-private types and functions.

```rust
type CleanupReceipt = tokio::sync::watch::Receiver<Option<CleanupReport>>;
struct CleanupReport { remote: RemoteStop, local: LocalCleanup }
enum RemoteStop { NotRequested, Stopped, Failed, Absent, Uncertain }
struct LocalCleanup {
    leader_reaped: bool,
    group_signals: Vec<GroupSignal>,
    helpers: Helpers,
}
enum GroupSignal { Sent, IdentityUnavailable, NotOwned, Unavailable, Refused }
enum Helpers { Unconfirmed }
enum TargetState { Matching, Absent, Uncertain }
fn target_state(text: &str, port: u16, expected_pid: Option<u32>) -> TargetState;
fn stop_reply(text: &str, port: u16, pid: u32) -> RemoteStop;
fn launch_reply(text: &str) -> Option<(u16, u32)>;
```

`GroupSignal::observed(Result<(), owned_process::SignalError>)` maps signal
observations. `SshSession::completion()` and idempotent `shutdown()` return
`CleanupReceipt`; shutdown requests cleanup and does not certify its result.
`is_alive()` reports forward activity only.

A buffered `Some(report)` establishes worker completion with those observations.
Consumers must inspect buffered state before treating sender closure as worker
loss. Closure without a report retains missing-report uncertainty. `None` alone
does not distinguish pending work from lost work. A receipt owns no executor.
Keep worker/runtime custody separately through the coordinator's selected wait
or approved handoff policy.

`RemoteStop::Failed` and `Uncertain` remain completed best-effort observations.
They cannot become false success, another destructive reconciliation attempt or
permanent normal-close refusal. `leader_reaped` and `GroupSignal::Sent` cannot
establish helper removal. `Helpers::Unconfirmed` remains explicit.

`target_state` requires the observed port/PID/runtime/launch mode. Missing,
changed or ambiguous identity suppresses stop. `launch_reply` requires the
existing CLI's acknowledged nonzero port/PID and daemon launch mode. Unknown
acknowledgement grants no port-based stop authority. `stop_reply` recognizes the
pinned PID-less matching-port stopped:false/not-found shape as Absent before
ordinary PID validation. Mismatched and duplicate matching ports remain
Uncertain. Classification performs no additional remote action.

## Retained prerequisites, without integration patches

These definitions already exist in the qualified hosts input. Their bindings
identify the source examined; they do not authorize copying its implementations
or applying obsolete setup/close drafts.

- `owned_process.rs` supplies `ProcessIdentity`, `identify`, `send`, `Target`,
  `Signal` and `SignalError`. Unit A uses identity-checked process/group signals
  and retains unsuccessful observations.
- `ssh_command.rs` supplies restricted `SshCommand::parse` and `shell_quote`.
  `ssh_prompt.rs` supplies `Askpass`, prompt IPC, control path, command environment
  and rejection observations. Neither is replaced by the report module.
- `network.rs` supplies `Network::new` and `handle`; the published cleanup worker
  captures its independent network. This existing capture does not supply
  register-before-spawn setup custody.
- `ssh_host.rs` retains `SshProfile`, `Forward`, `Prompt`, `SshError`, `Attempt`,
  `SshSession`, `Runner` and `ManagedDaemon`. The qualified `Attempt::Drop`
  still aborts its task. It is not the reconciled cooperative setup owner.
- `connection.rs` has `ssh: Option<SshProfile>`,
  `ssh_session: Option<Arc<SshSession>>` and `ssh_failure: Option<SshError>`.
  Its profile path assigns the SSH profile. `host_profiles.rs` supplies the
  profile's SSH field. These are feature prerequisites for the existing view,
  not extra Unit A changes.
- Transport `endpoint.rs::Endpoint::with_transport` takes the transport endpoint
  and `Arc<dyn Send + Sync>` owner. Cloned routed endpoints retain the session
  while preserving logical endpoint identity.
- The qualified Workspace has weak `ssh_sessions` and a receipt vector,
  initialized empty. Its existing final hook upgrades/captures sessions and
  signals them before waiting on receipts. These inherited owners and ordering
  are outside the scoped patch and must not replace coordinator lifecycle code.
- `main.rs` already registers the SSH host/command/prompt/view modules. The
  askpass entry is existing wiring. Only the new cleanup module declaration is
  exported by Unit A.
- Existing Cargo inputs provide Tokio watch/process/runtime support, serde,
  serde_json, thiserror, GPUI Kit and native transport. Unit A changes no Cargo
  manifest or lockfile. `server_version()` reads the existing Electron package
  version; that package file is an external reference, outside the 1,027-entry
  native input manifest. Its separate inspected-file hash is marked accordingly.

The 2 qualified Workspace adapters are exactly:

```diff
-    pub(crate) ssh_cleanup: Vec<tokio::sync::watch::Receiver<bool>>,
+    pub(crate) ssh_cleanup: Vec<crate::ssh_cleanup::CleanupReceipt>,
-                        let _ = stopped.wait_for(|done| *done).await;
+                        let _ = stopped.wait_for(Option::is_some).await;
```

The qualified view prunes any buffered completed report using
`receipt.borrow().is_none()`, regardless of report outcome. It supplies no
completed-outcome retention guarantee. The coordinator's later app owner must
preserve captured observations as required by the reconciliation note. Do not
introduce clean-only pruning, `is_clean`, custom cleanup truths or bool bridges.

## Coordinator reconciliation and proof limits

Published SSH remains available across host selection. Registered setup custody,
fenced publication, app-owned strong sessions and every shared-daemon stop gate
remain separate coordinator integration. The existing status-then-port-stop
protocol is non-atomic against external processes and cannot prove away PID
reuse. Unit A's existing 5-second remote-operation bound sets no terminal budget.

Preserve the coordinator's accepted-callback, Knowledge, Settings and Files
owners. Terminal integration follows applicable write gates and successful
journal finish, including the distinct no-loaded-journal branch. Capture custody
before taking connections and signal all eligible workers before aggregate
waiting. These are reconciliation requirements, not implemented Unit A hunks.

The terminal budget/expiry policy, conflicting shared-daemon preferences,
non-final-window retention and saved-host-link policy remain unselected here.
No 8-second default, new futures-util dependency, cleanup UI text or GPUI 200ms
change is included. All obsolete draft exclusions in `boundaries.md` remain.

The existing successful receipt covers 15 focused native tests, scoped all-target
Clippy and debug build. The preserved earlier receipt covers the unchanged 6
fixture contracts. Artifact delivery runs no qualification, build or runtime.
Unit A supplies no actual normal-window-close, app-exit cleanup, held-quit beyond
200ms or aggregate-deadline proof. The original failed aggregate and confirmed
local-master-after-exit failure remain valid. Prior passing candidates, failed
receipts and source snapshots remain preserved.

Knowledge Base capture remains deferred under the artifact-only write scope.
