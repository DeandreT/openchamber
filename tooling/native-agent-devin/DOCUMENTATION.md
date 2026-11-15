# Devin native-agent ownership

## Ownership

This non-workspace tool owns the Devin OpenCode plugin implementation. The SDK
owns native-agent v1 types, validators and fixtures. OpenCode owns model
selection, session transport, authoritative transcript parts, permission rules
and plugin storage. Cognition's SDK owns ACP negotiation, turns, transport
cancellation and session loading. Devin owns its credentials and tool execution.

The same plugin RPC serves every OpenCode client. It adds no web, Electron,
VS Code, hosted-mobile, Capacitor or Rust listener. Client interfaces retain
their own authentication transport and native-agent presentation. Native
authentication is refused by enterprise policy inside the executing plugin.

The credential-free smoke owns only its retained isolated OpenCode child. Its
exit watcher observes that child's acknowledgement and bounds its stop wait.
Web, Electron, VS Code, mobile and Rust application startup remain unchanged.

## Invariants

- A plugin instance accepts only its own absolute location, provider ID and
  sessions OpenCode confirms belong to that location. Drafts and sessions
  have separate activations and preparation.
- OpenCode owns the app session and provider-use gate. Devin owns native local
  and cloud tool execution. OpenCode checks approvals exposed through ACP;
  those callbacks do not cover every action Devin permits internally.
- Read-only state never signs in or sends a prompt. Preparing can allocate or
  load an SDK session only for an authoritative OpenCode session scope, and never
  sends or replays a prompt. Draft preparation is configuration-only, unprepared,
  and exposes no execution binding. Draft authentication is refused. Draft
  configuration changes persist intent without publishing a variant.
- `prepareAfterSession: true` requires the host to create the intended OpenCode
  session with its captured agent and route before preparing Devin. Fresh real
  scopes derive runtime mode from that authoritative session route; persisted
  allocated mode remains authoritative on recovery. Subset configuration updates
  preserve additional live fields at their authoritative defaults.
- RPC preparation rechecks the current route even when `getState` cached an
  unallocated entry. Implicit cached defaults follow that route; conflicting
  saved runtime intent is rejected before SDK access. A changed sign-in target
  requires a fresh user click. Allocated recovery and queued immutable execution
  retain their captured runtime instead of following mutable UI selection.
- Revisions and mutations serialize per scope. A reload creates a different
  activation. A recorded request ID cannot execute again or acquire different
  parameters. Admission is durable before an answer or sign-in side effect.
  Mutation IDs are own enumerable data keys on write and hydration, including
  `__proto__`, `constructor` and `toString`. JSON persistence keeps those records.
- Journals and immutable variant records contain configuration and opaque IDs.
  Credentials, prompts, raw failures and service responses are never journaled.
  Missing persistence differs from corrupt or unreadable persistence.
- Context checkpoints contain only versioned SHA-256 fingerprints. A completed
  SDK turn commits the checkpoint with submission settlement. Pending and unknown
  work never grants a new checkpoint or permits replay after restart.
- A separate append-only consumed-user hash ledger survives context replacement
  and compaction. Every prior consumed ID remains rejected across restart.
  Context-only journals without that ledger fail closed because earlier consumed
  IDs cannot be reconstructed from a latest-window checkpoint.
- Context is captured before asynchronous dispatch. The context hook preserves
  original roles, instruction-source metadata and message IDs. Ordinary ACP text
  carries an escaped, typed JSON envelope; it grants no privileged system role.
  Unsupported values fail before submission rather than disappearing.
  Media carrying transient download credentials is refused before transmission.
- The final AI SDK prompt is a separately labeled view with its own prefix
  checkpoint. It preserves edits made after this plugin's context hook. Both
  views describe the same conversation and are never separate submitted turns.
- Continuations append only a verified fingerprint prefix. Changed or removed
  instructions replace the earlier set. Rewritten or compacted history replaces
  the OpenCode context window, while Devin retains its own native conversation.
  An already consumed user ID cannot submit again, including after restart.
- The immutable variant registry is directory-scoped even when OpenCode's
  storage is shared by multiple locations. Persistence and catalog publication
  finish before a binding could be exposed. Unknown tokens fail closed.
- Execution uses the captured variant configuration. It never reads the latest
  mutable native model/mode choice while dispatching a queued prompt.
- Preparation retains persisted selections that remain in the refreshed
  choices. A removed selection reports `invalid_config`, exposes the refreshed
  choices with execution blocked, and requires explicit reselection.
- `configComplete: true` certifies a complete validated exposed SDK catalog.
  An explicit SDK `[]` produces the runtime-only catalog. Missing/nonarray
  options and malformed supported model/mode fields report `unavailable`, retain
  prior state and journal choices, and clear completeness and execution.
  Validation finishes before immutable variant publication. Drafts and preparing
  states have false; execution requires true. A complete refreshed catalog with
  an invalid saved selection remains complete on `invalid_config`, while saved
  choices stay durable until explicit correction. Authentication alone grants
  no catalog authority.
- Open SDK handles revalidate their current reported catalog on preparation,
  before configuration mutation admission, after SDK configuration setters, and
  before Ready/stopped binding publication. SDK result updates can revoke
  completeness without erasing prior choices. Valid changed catalogs refresh
  the exposed choices and require correction when saved values are removed.
- Captured immutable choices must remain valid in the live catalog before
  prompt admission. SDK model/mode setters run before recording a pending prompt.
  The SDK's public `ModelRejectedError` is a definite `invalid_config` failure;
  other pre-prompt setter failures report `unavailable`. Neither consumes the
  user or starts a prompt. Once streaming is invoked, uncertain outcomes retain
  the durable no-replay block.
- Submitted work stays pending until an authoritative SDK result settles it.
  A lost connection or restart becomes outcome-unknown and blocks resubmission.
  Loading history does not clear that uncertainty. No automatic retry is made.
- An allocated SDK session records its runtime mode with its ID. Recovery uses
  that persisted mode even if the plugin's configured default later changes.
- Native approvals contain the original offered one-shot IDs. Other option
  kinds are excluded. The provider-use gate and pending-action policy checks
  precede exposing approval, and allow replies are checked again.
- Approval policy uses normalized SDK details, every affected file and location,
  and cloud command metadata. Local relative paths resolve against the actual
  local scope. Absolute VM paths retain their remote identity; relative VM paths
  with no established base are refused. Unknown actions, missing resources and
  partially normalized file metadata are refused without a project-directory fallback.
- Progress can reference only an ACP call in the active execution that an
  OpenCode `session.tool.called` event confirms as provider-executed. Message ID
  comes from that event. The part and call IDs are the persisted tool ID.
  SDK completion and authoritative tool success/failure both end progress.
- State/event diagnostics are allowlisted codes. Exceptions never reach native
  state or SDK stream errors. Subprocess output and credentials are not logged.
- Plugin cleanup aborts its event observer, cancels owned SDK sessions and
  closes only its own clients. Persisted uncertainty remains available to a
  subsequent activation.
- Windows local preparation directly spawns the executable with `windowsHide`
  and passes its pipes, child and startup failure to public `LocalDevin.connect`.
  The SDK owns cancellation and termination. Stdout drains before deferred EOF
  reaches the SDK. After exit, a 1-second inactivity fallback closes only an
  empty pipe whose EOF is absent. Data refreshes that wait; buffered bytes keep
  it open. Drain completion and SDK close remove the wait's listeners and timer.
- Disposal invalidates ownership across awaits. Late-opened handles close
  without committing state or persistence. Already-started storage/publication
  effects drain; native permission waits do not block unload.
- The smoke's success summary follows exact-child exit acknowledgement within
  a 5-second stop bound. Startup, stop rejection and missing acknowledgement fail
  the smoke. Simultaneous check and cleanup failures are both retained.

## Decisions

### Why Windows uses public connect

The pinned SDK's spawn helper omits `windowsHide`. Its public connect method
accepts the child and startup-failure promise, so a direct hidden launch keeps
SDK process ownership without private SDK imports or a shell intermediary.
Bun's Windows pipe did not settle an active turn after process exit in the fake
CLI check. Exit therefore starts a 1-second inactivity fallback through public
client close. Progress refreshes the timer and buffered bytes keep the pipe open;
an absolute post-exit cap loses valid final responses during a slow drain.
Closing immediately on exit loses buffered final responses. The stdout adapter
also defers EOF by one event-loop turn so SDK response continuations finish
before its transport-closed handler rejects an active turn.

### Why the supported AI SDK hook is used

OpenCode 2.2.7's built-in Claude Agent uses `ctx.aisdk.hook('sdk')`, and its
adapter preserves provider-executed tool calls and results. This supplies an
existing execution path without an additional listener or an invented native
provider route.

### Why credentials stay outside the native RPC

The RPC contract intentionally accepts no secrets. The pinned engine's
credential table stores credential values directly, so the plugin uses
externally injected cloud credentials and the CLI's existing login rather than
persisting a PAT there or claiming encryption the engine does not establish.

### Why variants remain immutable

OpenCode resolves variant settings into per-request provider options. A durable
content-addressed token therefore freezes native configuration for queued and
first-session work. Reading current native settings at dispatch would silently
change previously captured work.

### Why final text waits for turn completion

ACP can overwrite or abort a message. The supported OpenCode AI SDK stream is
append-only for text. Buffering text until its final value preserves content;
appending every ACP correction would leave incorrect text in durable history.
Each stream pull consumes buffered and observation-only events until it can
enqueue a part or complete, so a text-only turn cannot stall before completion.

### Why recovery does not resubmit

A disconnected cloud or local session may still be running. The pinned SDK can
load it, but history alone does not prove whether the submitted operation
settled. Automatic replay could duplicate actions or billing.

### Why context uses ordinary labeled data

The pinned SDK has roleless ACP prompt blocks and no privileged system-message
or foreign-transcript import. The user accepted transferring instructions and
history as ordinary context with weaker priority. The envelope preserves source
roles and metadata without claiming SDK-level authority. Every original object
uses sorted entries, so embedded role names and encoding tags remain data.
Hashes permit incremental transfer and durable replay rejection without storing
conversation content in the plugin journal. A replacement window is a context
instruction, not an erasure of Devin's own memory or a native compaction result.

### Why preparation follows OpenCode session creation

OpenCode 2.2.7's supported permission assertion requires a real session. Catalog
visibility cannot establish permission for the intended agent. Draft preparation
therefore reads saved configuration only; the host creates the intended session
before policy checks, SDK access and live choice discovery. The optional flag
defaults false for existing providers. Clients with an older strict schema reject
the new advertised field, so SDK, plugin and host deployment must be coordinated.

### Why RPC preparation rechecks cached routes

A read-only snapshot can outlive an OpenCode model switch before any SDK session
exists. RPC preparation therefore checks authoritative routing at admission.
Explicit saved intent is preserved by rejecting a mismatch. Queue execution uses
its immutable binding and allocated recovery uses its durable mode, so neither
is reassigned from the latest selection.

### Why catalog absence blocks preparation

The pinned SDK defines absent options as unreported. Treating absence as empty
would authorize removal of captured fields without a live catalog. Its public
choice type is flat and its defaults advertise no grouped-choice capability,
so unsupported groups are refused without flattening. Cloud sessions that never
report options remain blocked without a discovery prompt. The SDK creates cloud
sessions lazily; cross-connection resumability of IDs saved before the first
prompt remains unqualified.

### Why configuration precedes prompt admission

A queued variant can outlive a model's availability while the latest displayed
selection is still valid. Validating the captured values and applying SDK
configuration before prompt admission keeps a definite configuration rejection
correctable. The pending record precedes the stream boundary, where loss of
the result can leave execution uncertain.
