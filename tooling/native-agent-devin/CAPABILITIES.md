# Devin enablement inventory

This is the remaining implementation inventory for
[issue #1](https://github.com/DeandreT/openchamber/issues/1). A passing contract
test does not complete the native Devin feature.

## Implemented execution

- Native-agent v1 discovery and all 5 RPC methods use the shared SDK validators.
- Local runtime creation uses `LocalDevin.spawn` outside Windows and a direct
  hidden process with public `LocalDevin.connect` on Windows. Cloud creation uses
  `CloudDevin.connect`. Loading uses the SDK's `loadSession` implementation,
  including its ACP version-specific replay behavior.
- The actual SDK is tested against in-memory ACP 1 and 2 agents. OpenCode
  2.2.7 loads the plugin and exercises discovery/RPC/catalog publication without
  credentials or prompts.
- Immutable variants bind model/mode selections to normal OpenCode execution.
  Registry publication is tested through a real OpenCode plugin reload.
- The approved `prepareAfterSession` capability keeps drafts configuration-only.
  The host creates the intended OpenCode session with its captured agent and
  route before provider authorization and SDK preparation. No draft operation
  signs in, spawns a CLI, connects cloud ACP or publishes a configuration variant.
- RPC preparation rechecks an unallocated cached entry's current authoritative
  route. Implicit defaults can change; saved conflicting intent requires explicit
  correction. Allocated recovery and captured queued execution keep their runtime.
- Offered one-shot permissions retain exact IDs, including IDs named `allow`
  or `reject`. Native requests check configured policy before presentation and
  before an allow reply.
- SDK-confirmed cancellation clears the submission. Disconnection and failed
  recovery persistence keep the operation uncertain and prohibit retry.
- A separate durable consumed-user hash ledger rejects all consumed IDs across
  history replacement and restart. Legacy context-only journals fail closed.
- ACP text corrections and aborted messages produce correct final text.
  Provider-executed tool calls/results preserve original IDs. Progress requires
  the matching persisted OpenCode call identity.
- Inline images, audio and embedded files use the official `prompt.image`,
  `prompt.audio` and `prompt.blobResource` builders. Support and embedded byte
  limits come from the negotiated agent capabilities. Unsupported content and
  external URL attachments fail before submission. No attachment is uploaded
  during the credential-free checks.

## Instructions and provider-switch history

The public SDK accepts roleless `PromptInput` content blocks. `LocalSessionOptions`
and `CloudSessionOptions` expose no privileged system-instruction or arbitrary
foreign-transcript import parameter. `loadSession` loads an existing Devin
conversation and does not import an OpenCode conversation.

The user accepted ordinary-context transfer with weaker priority. The supported
OpenCode context hook captures system/agent instructions with source metadata and
the complete history window, including prior providers. Original roles, IDs and
fields remain escaped JSON data inside one ordinary ACP text block. Unsupported
values fail before submission. No instruction files are written into the checkout.
A separately labeled final AI SDK view preserves later-hook changes and has its
own fingerprint prefix. The two views are context data for the same conversation.

The plugin persists versioned fingerprint checkpoints only after an authoritative
turn result. Continuations append a verified history prefix; instruction changes
and removals replace the previous instruction set. History rewrites and incoming
OpenCode compaction summaries replace the labeled window. Restart retains the
checkpoint and rejects consumed user IDs without resubmission. This preserves
provenance and avoids duplicate envelope entries, but grants no privileged SDK
system role and does not erase Devin's own conversation memory.

## Attachments

The verified public builders cover inline image, audio and embedded resources.
This adapter now maps those formats and checks negotiated limits. Cloud agents
can advertise `cognition.ai/httpUpload`, `httpUploadMaxBytes` and a session upload
URL, and the REST SDK exposes organization attachment upload. Large-file upload,
redirect validation and reference downloads remain separate implementation work.
The adapter does not fetch arbitrary URLs or silently drop unsupported files.

## Live choice catalog completeness

`configComplete: true` certifies a complete validated exposed catalog, including
an explicitly reported empty SDK options array plus the runtime field. Drafts,
preparing states and missing/malformed catalogs have false with null execution.
Authentication, Ready and error codes alone do not certify completeness.
Missing/nonarray options or malformed supported fields return `unavailable`
without replacing prior state/journal choices or publishing a variant. A complete
valid catalog with invalid saved selections returns authenticated `invalid_config`
with all refreshed exposed choices and completeness true. Unsupported fields
remain outside the exposed catalog. Supported duplicate IDs, duplicate choices,
conflicting aliases, invalid labels and nested groups are refused.

Cloud sessions that never report a catalog remain blocked and unqualified. The
pinned SDK creates the actual cloud session lazily on the first prompt and
forwards an existing ID unchanged to resume/load. Neither that client code nor
the official cloud-session guide establishes that a pre-prompt ID is resumable
across connections. That recovery case requires separately authorized live
evidence. No discovery prompt, retry or alternate-ID heuristic is implemented.

## Plans and pull requests

`SessionEvent.plan` exposes complete plan entries. `SessionEvent.pull_request`
is a link/invalidation signal, and cloud snapshots expose PR metadata. These are
verified SDK events, but their native presentation is not implemented by this
plugin. A native plan representation needs a host contract or an agreed
transcript mapping. Inventing provider-executed tool calls for plan updates
would misrepresent observation events as tool execution.

`CloudDevin.mergePullRequest`, `pullRequestMergeStatus` and `closePullRequest`
are public SDK methods. Mutating PR actions need the existing native Git/PR
authorization and UI workflow. They are not automatic consequences of receiving
a PR link.

## Compaction and auxiliary generation

The public `AcpSession` has no dedicated compaction helper. The CLI documents
`/compact` as an ACP slash command, but sending the literal text is not proof
that OpenCode can safely replace its own context window or preserve a compaction
checkpoint. No mapping to the engine's compaction result has been established.
Native compaction remains implementation work.

Incoming OpenCode-compacted history is transferred as an explicit replacement
context window, with summary role and provenance preserved. This is covered by
production wiring tests and is separate from requesting native Devin compaction.

OpenCode title requests receive a fixed local title. Other auxiliary model
requests are refused before a Devin turn. They cannot create an unexpected
billable session while a native title/compaction implementation is absent.

## Authority and qualification

The Windows wrapper now uses a direct `windowsHide: true` launch through public
`LocalDevin.connect`, passing the child and startup-failure promise. A compiled
local fake CLI verifies no attached console, no visible windows in its sampled
process tree, SDK cancellation, close, startup failure and turn-time exit.
Final-response/immediate-exit checks cover normal and buffered output. EOF is
deferred until response continuations settle; a 1-second inactivity fallback
handles missing EOF after exit. Slow-drain checks preserve a buffered response
across the former absolute deadline. Progress refreshes the timer, buffered bytes
keep it open, and drain completion or SDK close removes its listeners and timer.
The tree includes Bun's invisible `conhost.exe`. The pinned helper also had no
attached console in this environment, so a pre-change console flash was not
reproduced. Real installed Devin CLI descendants and packaged desktop startup
still need qualification. No real installed CLI or credentials were used.

Devin runs its own tools, locally or on its VM. The SDK calls `onPermission`
only when the agent requests approval. Neither `normal` mode nor those callbacks
prove enforcement of every OpenCode shell/read/edit deny. The user selected
native Devin behind OpenCode: OpenCode owns the app session and provider-use
gate, Devin executes tools, and exposed approvals receive OpenCode policy checks.
Production execution bindings are enabled under that decision. No consent
setting is added.

Exposed approvals use normalized SDK details for local `file_path`, cloud
commands, every file update and location. Unknown actions and unresolved or
partially normalized resources are refused. Relative local paths resolve against
the local scope. Absolute cloud paths retain VM identity, and relative cloud
paths without an authoritative base are refused. These checks cover exposed
callbacks only; local path rules do not map unmapped VM files onto the checkout.

The optional preparation flag defaults false for older providers. Older strict
clients reject an advertised new field, so the plugin, SDK and host require a
coordinated version rollout. First-Send retained-message handling and native
creation changes belong to the host owner and still require combined validation.

Real authentication, remote execution/cancellation, all rendered native-agent
states, provider-switch history, plans/PRs and compaction still need end-to-end
qualification after their implementations and product decisions. No live
credentialed or billable Devin session has been started for this work.
