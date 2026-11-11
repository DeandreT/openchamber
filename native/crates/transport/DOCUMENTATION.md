# Native transport

`openchamber-native-transport` connects the native desktop to one OpenChamber
server. It owns the endpoint, service identity, authentication, requests, wire
parsing and the global event stream, and hands the rest of the application
core records (`SessionRecord`, `Message`) and domain events. No wire type
leaves the crate. The contract and the reasons for a hand-written adapter are
in [`docs/rust-desktop/OPENCODE_ADAPTER.md`](../../../docs/rust-desktop/OPENCODE_ADAPTER.md).

## Structure

| Module | Owns |
| --- | --- |
| `attachment.rs` | Planning and explicit byte writes under the selected OpenCode server's temporary directory, with retained targets and scoped response validation. |
| `endpoint.rs` | The user-selected base URL, its path prefix and the `ws`/`wss` form of a route. |
| `identity.rs` | `/health` parsing, API compatibility and capabilities. |
| `system.rs` | Independent OpenChamber and OpenCode version readouts from the host's system and upgrade-status routes. |
| `magic_prompts.rs` | The magic prompt overrides of a scope. A failed read is an error. |
| `commands.rs` | The commands of a directory with each one's stored entry, and their create, update and delete. A failed list read is an error; a failed read of one command's stored entry keeps that command as OpenCode listed it. |
| `git_settings.rs` | Git identities, the machine's author and a narrow view of the settings document's Git preferences. |
| `opencode_upgrade.rs` | The OpenCode upgrade status and the install the server runs itself. `upgrade_opencode` waits up to 30 minutes, and a transport error from it says nothing about the install. |
| `compatibility.rs` | The host's OpenCode compatibility (`GET /api/opencode/compatibility`) and its recovery requests: install v2 (`POST /api/opencode/install-v2`) and reload (`POST /api/config/reload`). |
| `credential.rs` | The credential a client sends; its `Debug` output is redacted. |
| `authentication.rs` | Exact bearer status at `/auth/session`, password exchange with explicit session/device trust, bounded response parsing and login rate limits. |
| `request.rs` | The private authenticated request builder, preserving reqwest serialization and publishing a content-free signal on HTTP 401. |
| `client.rs` | `ServerClient`: identity probe, password login, session pages, message pages, whole messages, running status, settings reads and writes (including the whole `shortcutOverrides` map), Chats roots, projects and worktrees. |
| `catalog.rs` | Routing readiness (Auto and the safety net) and the composer's catalogs in one directory: agents, the flat model catalog with OpenCode's default model, folded config defaults, the skill count and the connected MCP count. |
| `mcp.rs` | One directory's MCP servers with their connection status (`GET /api/mcp`), OpenCode's connect and disconnect (`POST /api/experimental/mcp/:server/connect`, `/disconnect`), and the on/off switch, which first writes `disabled` into the config file that defines the server (`GET /api/config/mcp`, `PATCH /api/config/mcp/:name`) and stops at the first failed step. |
| `workflows.rs` | The workflow runs a session proposed (`GET /api/openchamber/workflows?sessionId=`) and the user's approve, decline, stop and resume (`POST /api/openchamber/workflows/:runId/:action`), whose refusal keeps the server's reason. `control_stream.rs` decodes the run and removal events with the same summary decoder; a list with one unreadable run is a failed read. |
| `project.rs` | A directory's Git status (`GET /api/git/status`) and a project's configured actions (`GET /api/projects/:projectId/config`, the path-derived ID). |
| `composer_catalog.rs` | The composer pickers' catalogs no send reads: the commands of a directory (`/api/command`, then each command's `/api/config/commands/:name/config` for its scope) and its snippets (`/api/config/snippets`). The `$` picker's skills come from `composer_preparation.rs`'s `skill_choices`, the same available skills a send resolves. |
| `files.rs` | The Files surface's filesystem routes: folder listings, OpenCode's fuzzy file and folder search (`/api/fs/find`), ripgrep content search, stat, text and bounded raw reads, writes and renames, scoped to the workspace by the directory header. |
| `git.rs` | The Git surface's routes: the full status, path and batch diffs, stage, unstage, discard, commit, branches, the repository's transport binding and planned network operations. |
| `walkthrough.rs` | Cached walkthrough reads, explicit generation and cancellation, memory-only progress, and validated server-owned chapter, stop and hunk identities. |
| `source_control.rs` | The source board's routes (`/api/source-control/*`): providers, instances, a host's auth status, the binding with its provider associations, reference pages, an item's detail, listed change requests' statuses, read-cache refresh, and comment, state and review mutations with their receipt checks. |
| `scheduled_tasks.rs` | Scheduled tasks (`/api/projects/:projectId/scheduled-tasks`): the list, save, delete, run now with its busy answer, a loop file's enable and delete, and target eligibility in batches of 500. A task keeps its raw JSON so a toggle writes back fields this client does not know. |
| `project_context.rs` | Project knowledge (`/api/project-context/:projectId`, a path-derived id): the context, one todo at a time, the whole todo list with plan links and dependencies, notes, plan Markdown by id, the plan's session and share moves. A gone entry answers `None`; a failed read is an error. |
| `prompt.rs` | Prompt submission with its synthetic context, resumption of an uncertain send, slash commands, ordered session model/agent selection, inbox admission receipts, observation and interruption of one named session. |
| `prompt_parts.rs` | What a prompt carries besides text (file references, agent and skill references, metadata, synthetic context), the command request, and their private request bodies. |
| `session_actions.rs` | Rename (`PATCH /api/session/:id`, then a read back and a best-effort user-title note), delete (`DELETE`, 404 is already gone), archive and restore (`POST /api/openchamber/sessions/archive` and `/unarchive`), and pin, work state and the context pins of a message (`context_obligatory_messages`) as merge patches to `/api/openchamber/sessions/:id/metadata` computed from a fresh read; fork (`POST /api/session/:id/fork`) and the fork's source-owned link cleanup, revert stage and clear (`POST .../revert/stage`, `DELETE .../revert`), the `/btw` marker, link, unlink and promotion, and stopping a subagent (a non-resuming synthetic note, then the child's interrupt). |
| `session_warming.rs` | Per-session warming: the snapshot (with its duration), one session's switch, and Settings' mode and duration writes (`/api/session-warming/mode`, `/duration`). |
| `session_goal.rs` | The session goal's writes as merge patches of `metadata.openchamber.goal` (new goal, edit in place, Pause and Resume, removal), the objective file (`/api/goals/objective/:sessionId`, written before the metadata, falling back to the inline objective; a 404 read is a missing file, not a failure), the non-resuming first-turn reminder and `GET /api/small-model`. |
| `review.rs` | Auto-review runs (`GET /api/auto-review`, `POST /api/auto-review/runs`, `DELETE /api/auto-review/runs/:id`) routed to an isolated space's own server by the directory header, the `openchamber:auto-review.updated` broadcast, and the review session a review creates or reuses: a stale link is dropped, a new session is created with the review marker and linked, and deleted again when the link cannot be written. A malformed run makes the whole answer malformed, as the web's schema does. |
| `session_create.rs` | Immutable root-session requests, exact-ID observation and identity/location verification before exposing a session record. |
| `requests.rs`, `request_wire.rs` | Explicit blocking-request lists, exact reads, permission replies, form answers/cancellation and private pinned-2.2 wire decoding. |
| `message_queue.rs` | The server-owned queue (`/api/message-queue`): snapshots, enqueue, remove, reorder and take, routed to an isolated space's own server by the directory header, and the `openchamber:message-queue.updated` broadcast. |
| `queue_manifest.rs` | Captured opt-in manifest and payload queue requests, immutable attachment uploads, bounded downloads and retained enqueue/take/prefix identities. Payload parsing verifies capture and index hashes through the destination's SHA-256 callback. Individual attachment downloads require caller SHA-256 verification before use. |
| `queue_frozen_send.rs` | Immutable bounded outgoing bodies for strict sealed queue sends and ordinary rich local sends, captured transport dispatch and exact-ID observation. |
| `starters.rs` | A project's starters in its setup (personal and shared writes) and the magic-prompt overrides a directory resolves. |
| `permission_policy.rs` | Per-session permission modes: `GET /api/permission-auto-accept` and `PUT /api/permission-auto-accept/sessions/:id`, both answering the whole policy. |
| `behavior.rs` | Settings → Behavior: the global `AGENTS.md` (`/api/behavior/agents-md`), whose write sends the content last read and turns the server's 409 into `AgentsMdError::Conflict`, and the response-style fields of the settings document. |
| `snippets.rs` | Settings → Snippets: list, create, update (rename and scope move), delete and preview through `/api/config/snippets`, with a taken name (409) as its own `SnippetError::NameTaken`, and the name rules of `lib/snippetNames.ts`. |
| `opencode_config.rs` | OpenCode's effective config (`GET /api/config`, documents folded like `mergeConfigDocuments`) and the global writes Settings makes: compaction (`PUT /api/config/compaction`, one field per write) and OpenCode's warming switch (`PUT /api/config/warming`). |
| `public_asset.rs` | `PublicAssetClient`: public assets the web client loads from third-party hosts, such as models.dev's provider logos. |
| `wire.rs` | Serde shapes of the fields the client reads. Private. |
| `projection.rs` | Wire records to core records, mirroring `packages/ui/src/lib/opencode/projection.ts`. Private. |
| `message_search.rs` | One session's messages matching a query, from the server's message index (`/api/openchamber/message-search`); a missing index or route is `Unavailable`, never an empty result. |
| `tool_facts.rs` | A tool call's input and metadata read into the core's `ToolFacts`, field by field as `lib/opencode/tools.ts` reads them; history records and live transitions share it. Private. |
| `notification.rs` | `openchamber:notification` events as `event-pipeline.ts` validates them (every field optional, a wrong type rejects the event; a plugin's notice is always background-only) and the `notificationTemplates` settings field. |
| `dictation.rs` | Dictation against the server's transcription: the `/api/dictation/ws` stream (bearer upgrade, `start` and `ack`, numbered chunks kept until the dictation ends, `finish`, `final`, `cancel` on drop), the start retried every few seconds while the local model downloads and once more at finish, `GET /api/dictation/status`, model download and delete, and the voice key routes (`/api/voice/keys`), which answer only whether a key is saved. |
| `quota.rs` | One provider's quota (`GET /api/quota/:providerId`) as `fetchQuota.ts` validates it: a malformed window, or another provider's answer, rejects the whole answer. Windows and models keep the server's order, decoded from the response body before any sorted map. `POST /api/quota/:providerId/gift-reset/use` spends a gift limit reset; only a `success: true` answer is a success, a declined one keeps the server's `error` text, and only a 4xx answer says nothing was spent. |
| `event.rs` | Bridge frames to `ServerEvent`s, mirroring `lib/opencode/events.ts` and `sync/event-pipeline.ts`. |
| `stream.rs` | `EventStream`: the reconnecting WebSocket subscription. |
| `terminal.rs` | Server terminals: `TerminalApi` (shells, list, touch, create, restart, resize, close, terminal settings) and `TerminalSocket`, the reconnecting protocol v3 socket with its binary codec and input splitting. |
| `parity_tests.rs` | Differential tests against the official client's fixture. |
| `test_server.rs` | Test-only loopback server that answers through a route function and records each request, for the Settings transport modules. |
| `tests/server.rs` | The transport against a scripted loopback HTTP and WebSocket server: login fields, bearer use, failure classes, redirects, paging, cancellation, resume and reset cursors, fragmented frames and the `ready` timeout. |
| `tests/composer_catalog/` | The command list with each config read (a failed read keeps the listed command), snippets and the folder search against the scripted server. |
| `tests/dictation.rs` | The dictation stream against a scripted loopback server: the bearer upgrade, chunk numbering with chunks buffered before the `ack`, the transcript, a start retried while the model downloads that sends every chunk again, a server failure, a cancel when the stream is dropped and a refused connection failing at finish. |
| `tests/terminal.rs` | The terminal socket and commands against scripted loopback servers: bearer upgrade, sequence handling, reconnect with snapshot replay, input under the frame limit, idle close, refused credentials, request fidelity and refusals. |

`examples/probe.rs` exercises the crate against a running server and prints
counts and identifiers only.

## Invariants

- Attachment planning reads the captured server's `/api/info` and allocates an
  app-owned target without writing a file. The caller durably retains that target
  and immutable prepared bytes before an explicit write. Restoring a plan keeps
  the original target; a changed server temporary root is a refusal.
  Writes use raw bytes through the authenticated request builder and require the
  returned location and path to match. Failures do not choose a replacement URI,
  retry, delete server files or fall back to workspace writes. Each file owns its
  outcome independently. The prepared-representation ceiling is 20 MiB; raw input
  limits belong to preparation, and server limits remain a separate contract.
- Authentication checks use the same bearer as subsequent API requests. A
  validated 401 with `locked: true` identifies a password or pairing challenge;
  failed reads and inconsistent bodies grant no authority. Password exchange
  requires an authenticated answer with a bounded header-safe client token.
  Authentication error parsing never includes raw response data in diagnostics.
- Every authenticated HTTP operation signals a definitive 401 through its
  client's authentication watch before a feature-specific body parser runs.
  Cloned clients share that watch. Network failure and resource-policy 403
  responses do not signal revocation. The desktop scopes the watch to the
  connection generation and endpoint, cancels that generation and returns to
  sign-in while retaining drafts and pending mutation identities.
- Work-status settings lists distinguish omitted fields, validated arrays
  including explicit empty arrays, and malformed values including JSON null.
  A malformed layout field leaves unrelated valid settings usable. The
  `MalformedWithPrior` projection holds native-retained presentation only;
  `entries()` grants new list authority only to `Valid`. Raw wire parsing never
  creates retained-prior metadata. Outgoing partial settings writes remain
  plain arrays and omit untouched preferences.
- Event and terminal WebSocket upgrades include the selected HTTP endpoint's
  origin along with the bearer. Password-protected servers check both; an
  absent origin is rejected even when the token is valid. Paths stay in the
  upgrade URL and never in the origin header.

- Filesystem routes send the workspace in `x-opencode-directory`, URI-encoded
  and marked with `x-opencode-directory-encoding: uri`, as `runtimeFetch`
  does; the server decodes only marked values. Paths stay absolute in the
  server's path space.
- A declined filesystem or Git answer keeps its status, the server's message
  and its `reason` or `code`; `not-found`, `not-directory`, `os-permission`
  and `already-exists` are their own `FilesError` variants. A 403 the server
  explained in JSON is its refusal (the filesystem routes report denied access
  so); one without a JSON `error` stays `AuthenticationRequired`. No refusal
  reads as an empty folder, an empty file or an empty diff.
- `web_content` mints the grants previews (`/api/fs/preview`) and extension
  frames (`/api/guests/:id/frame-grant`) load by, builds their addresses with
  the grant in the path, reads the extension panels of `/api/guests` row by
  row, and posts answers to rendered pages. The client's credential goes only
  on these requests, never into an address a page reads.
- `stat_file` asks with `optional=true`, so a missing file is
  `StatRead::Missing` and a failed stat is an error; the open-file poll needs
  that difference. `read_file` accepts only a `text/plain` body.
- `read_raw_file` reads the captured server's existing `/api/fs/raw` route with
  exact path and directory query values and the encoded directory headers. It
  requests no outside-workspace access or byte range. Only a complete 200 without
  `Content-Range` can return bytes. Advertised and streamed bytes share the
  caller's limit; a truncated body fails. MIME is bounded advisory metadata,
  including `application/octet-stream` for Office and HEIC preparation. Raw-read
  errors discard server response content. Authentication signals, cancellation,
  no redirects and no retries come from the existing authenticated client.
- Content search declares `invalid-pattern` and `search-unavailable` as
  outcomes, not errors. Dropping the search future closes the request, which
  ends ripgrep on the server.
- `git_diff` maps the declared 422 `nested_repository` and
  `untracked_directory` and 404 `path_not_found` to `PathDiff` outcomes;
  `git_diffs` carries each path's outcome and never fails the others.
- Push, pull and fetch are planned operations only. The plan body carries
  exactly the fields of its action (the server rejects unknown ones): refs
  for push and pull, `fetchScope: "remote"` for a whole-remote fetch.
  Execute, read and cancel send no body and address the operation by its ID.
  The legacy push, pull and fetch routes are never called. An operation with
  an unknown state or action is malformed.
- A single-ref fetch carries `sourceRef` and `destinationRef`; a whole-remote
  fetch carries `fetchScope: "remote"`. Integration resolves the fetched tracking
  ref to a commit before merging or rebasing.
- Walkthrough reads validate the requested source, complete hunk count and
  unique chapter, stop and hunk identities. Failures never become an empty
  walkthrough. Generation is explicit and is never retried automatically.
- `repository_binding` derives an unconfigured repository's grants as ready
  System grants for every remote (`effectiveRepositoryBinding`). A bound
  grant takes the repository's current endpoints and is ready only while its
  fingerprints still match them.

- `host_session_status` retains optional pending permission/form identity hints
  from `/api/sessions/status`. Invalid entries are omitted independently; omission
  of the whole pending field means unsupported. Hints grant neither action nor
  removal authority. Desktop resolves relevant directories and reads full requests
  before exposing actions. The existing `session_statuses` convenience method
  continues to return only live statuses. For a running entry with a
  `turnStartedAt` and a snapshot with a `serverTime`, the read also returns
  how long the turn had run on the host's clock; a malformed time loses only
  that entry's elapsed time.
- A session record carries `cost` from the session payload, and
  `session.usage.updated` patches it. A missing, negative or non-numeric cost
  is no cost and leaves the known one in place.
- `session.execution.started` and `session.execution.failed` emit a
  `SessionExecution` edge beside their status event; the failure's edge comes
  first in its frame. `session.execution.succeeded`, a real interruption and
  plain status events emit no edge.
- `metadata.openchamber.kind == "workflow-agent"` projects as
  `SessionKind::WorkflowAgent`, with the exact spelling only.
- Session projection recognizes reviews only through the explicit OpenChamber
  metadata marker and an original session identity nonblank under ECMAScript
  trimming. Full metadata updates replace that classification, including when
  a review marker is removed.
- Blocking-request calls use the captured `ServerClient` and `RequestScope`.
  The desktop must bind that client to the scope's verified runtime before dispatch;
  the transport never substitutes the currently selected runtime or directory.
  Location lists carry the encoded directory. Real-session routes resolve their
  location on the server; `global` form routes always carry the captured directory.
- Pending lists return the core's `RequestList<T>`. The response envelope and
  returned location must be valid; otherwise the call fails. Entries decode
  independently, retaining valid siblings and reporting indexed `Malformed`,
  `WrongOwner` or `DuplicateIdentity` issues. Every fully decoded duplicate is
  withheld, so the client never chooses one conflicting owner. Invalid entries
  contribute no guessed IDs. Any issue makes the list incomplete and denies all
  removal authority for that response. Exact gets still fail on malformed or
  mismatched records; a decoding error never becomes not-found.
- Permission and form replies expect an empty 204. Form cancellation is DELETE,
  with an optional `message` query and no body. Plain cancellation omits feedback.
  No blocking-request mutation is automatically retried, including after a lost
  response. Callers retain the exact request key and reconcile uncertain outcomes.
- `RequestError` distinguishes declared permission/form not-found, form already
  settled, invalid answer, missing session and missing location. Request-specific
  errors require matching status, tag, kind and ID. A bare HTTP 404 never proves
  settlement. Error messages omit response bodies, metadata and user answers.
- Decoded form/permission content has redacted Debug implementations in core,
  including raw answer values, defaults and cancellation feedback. Blocking event
  and list Debug output inherits that redaction while retaining IDs and kinds.
- Metadata is display-only JSON text, limited to 8192 bytes plus a truncation marker.
  It carries no routing or permission authority. Lists and details have a 4 MiB
  response bound; exceeding it is a failed read, never a partial list.
- Blocking events preserve their directory and actual owner. Nested `form.created`
  is decoded before session event extraction. A location-owned event without a
  directory yields `BlockingRequestInvalid`, as do malformed request records.
  This carries no mutation authority and leaves the stream consuming unrelated
  events; callers reconcile the known scope. `location.shutdown` names the scope to invalidate;
  the consumer chooses selected-scope reconciliation without project fan-out.

- Session creation requires a caller-retained `ses_` identity and absolute remote
  directory. `CreateSession` captures a normalized request, omits `parentID`, and
  omits the synthetic Auto model. The caller retains Auto in its prompt configuration
  and selects it through OpenChamber before sending. Creation never sends a prompt.
- Creation and exact session observation accept only the requested identity and
  matching normalized absolute location. Unexplained canonical aliases are blocked;
  the native machine's filesystem cannot establish a remote path's identity.
  Mismatches never cause deletion, binding or another mutation. A lost response or
  cancellation requires exact-ID observation, and a successful 404 observation is
  `NotObserved`, not proof that the preceding POST failed. The caller durably retains
  the immutable request and decides whether an explicit same-request retry is safe.

- The identity probe sends no credential. A server that does not answer an
  OpenChamber identity is `NotOpenChamber`, and a server whose API range
  excludes this client is `Incompatible`, before any credential is sent.
- Redirects are never followed, by HTTP or WebSocket requests, so a credential
  never moves to another origin.
- A credential appears only in the `Authorization` header, marked sensitive.
  It is never part of a URL, an error, a log line or `Debug` output.
- `send_prompt` posts a model selection to `/api/session/:id/model`, then an
  agent selection to `/api/session/:id/agent` when supplied, then each context
  item to `/api/session/:id/synthetic` in order, and finally the prompt with its
  exact client-generated message ID to `/api/session/:id/prompt`, as the web
  client's `sendMessage` does. Context uses the prompt's delivery and
  `resume: false`. A text-only prompt's body is byte for byte `{ id, text,
  delivery? }`: empty files, agents, skills and absent metadata are omitted.
  Selection responses are 204. The prompt's 200 response contains an inbox
  user item under `data`; it acknowledges admission, not turn completion.
  Model references use `providerID`, `id` and optional `variant`. Omitting a
  variant clears the previous effort selection, matching `toModelRef`.
- Prompt and context IDs must carry the pinned protocol's `msg_` prefix and be
  unique within the prompt. A prompt needs text, a file or context. Validation
  (`Prompt::is_well_formed`) runs before any selection mutation. `Prompt` and
  its parts redact text, file references, descriptions and metadata in Debug.
- `resume_prompt` resends an uncertain prompt after `observe_send`: it skips
  the leading context items the observation found and sends the rest, then the
  prompt, under their saved IDs. `admitted` must be a leading run of the
  context in order; anything else fails validation before any request.
- Prompt failures retain the session and message IDs and the failed stage,
  including the index of a failed context item. `NotAttempted` means
  selection, local validation or a definite rejection of the first context
  item stopped before anything was admitted. A lost context, prompt or command
  response is `Unknown`, since that item may be admitted. Once any context item
  of the prompt or command is admitted (an earlier item, all of them before the
  prompt, or the `admitted` run of a resume), every other failure, including a
  refusal or an authentication expiry, is `Partial`: the caller keeps the
  attempt for recovery and never rolls it back to a draft.
  `Rejected` identifies a declared rejection or authentication refusal.
  `Unknown` covers lost responses, conflicts, refused redirects, server failures,
  unexpected status and malformed or mismatched receipts. A conflict can refer
  to an already admitted ID. Keep the draft on every failure and
  reconcile the exact ID through inbox, events or history before resending an
  unknown outcome. An inbox item may not yet appear in history.
- No mutation retries or selection rollbacks occur. A successful model
  selection survives a later agent or prompt failure. The authenticated HTTP
  client disables reqwest's default protocol-NACK retries for every request.
  A lost selection response
  may also have applied; re-read session selection before presenting it as
  authoritative. Dropping a submission future cancels local I/O only; retain
  its ID for reconciliation because admission may already have occurred.
  Selection and admission are separate requests, so concurrent clients can
  change session selection between them. The receipt does not attest which
  model will run; session and assistant-step events remain authoritative.
- `observe_send` reads the inbox once, then history for the prompt and for each
  context item the inbox lacks. A pending or delivered prompt settles every
  step. Otherwise it reports the observed leading context, or `OutOfOrder`
  when a context item follows a missing one, since no resend can restore that
  order. Context matches only `synthetic` inbox items and history records.
- `lookup_prompt` reads `GET /api/session/:id/inbox` and, only when the exact
  ID is absent, `GET /api/session/:id/message/:messageID`. It returns a typed
  pending admission receipt or the projected user message from history.
  Inbox identities must belong to the requested session; matching IDs must
  be unique user items with valid admission time and delivery. Other pinned
  inbox types use a validated identity subset. Malformed or conflicting
  identities fail the read, including a history result with another ID or role.
  A history 404 after a successful inbox read returns `NotObserved`, which
  never proves rejection. All other failed reads remain errors. The method
  performs no mutations and makes no resend decision.
- `run_command` posts selection and context the same way, then
  `{ name, text, files?, delivery? }` to `/api/session/:id/command`, which
  answers 204 without a body. OpenCode assigns the command's message ID, so an
  `Unknown` command outcome has no identity to look up and is never resent
  automatically. Its declared 500 is `Unknown`, because the command may have
  started. A slash-prefixed string sent as a prompt is literal text. Admission
  receipts validate the message ID, session and user type, and parse the server's
  timestamp and delivery. They intentionally leave prepared payloads to history
  and events. OpenCode's `session.prompt` hook can transform text, add metadata
  and attachments, and change delivery; a successful receipt does not promise
  an exact echo of the request.
- `abort_session` posts only to the named session's `/interrupt` route with no
  body or resume query. Its boolean is the server's `interrupted` value; false
  is a successful no-op. A lost or malformed response leaves the interruption
  outcome unknown. It never interrupts other sessions or stops the server.
- Session management never retries. Archive and restore answers report
  stamped and failed IDs separately; a malformed answer fails the call
  rather than reading as nothing archived. Metadata patches carry only the
  keys the change touches, so other features' keys stay; an empty patch
  sends nothing. Rename and delete expect 204, as the pinned client does.
- Queue answers and broadcasts carry the revision of the queue they
  describe; a malformed one fails the call (a broadcast becomes
  `Unsupported`), never an empty queue. A session-scoped queue request sends
  the directory header only for a space directory, as `runtimeFetch` does;
  host directories travel in the body. Projections keep each attachment's
  name, source, size and server path, so a caller can tell a take it could not
  record. Enqueued items carry the captured
  selection and no attachments or context. A take returns the payloads a
  snapshot leaves out: attachments as `file://` URIs for server paths or
  their data URLs, and context in send order with an item's instructions
  before it.
- Merged queue sends use `reserve-prefix`, which leaves payloads under the
  server owner's exclusion until admission of the receipt's prompt ID.
  A nonmutating `preview-prefix` supplies the whole prompt
  for a local journal barrier before reservation. A matching 409 response with
  `queue_prefix_unavailable`, session ID and message ID confirms non-acquisition;
  other failures remain ambiguous. The native client compares the reserved
  payload with its journaled request before admitting any prompt.
  The immutable receipt retains composer text, selection and delivery across
  lost responses and restarts. Receipt parsing validates session ownership,
  ordered item identities, delivery and the captured configuration. A failed
  or unsupported reservation is an error; there is no fallback to destructive
  per-item or unbounded takes. Host and isolated-space routing use the same
  authenticated queue transport as existing mutations.
- Payload queue APIs are additive to the legacy and `manifest-v1` adapters.
  `QueuePayloadDocument` validates the strict inner manifest/legacy capture union
  and exact directory. Its original UTF-8 JSON remains immutable. Complete
  captures are at most 50 MiB; non-empty ordered chunks are at most 20 MiB each,
  with at most 4096 distinct blob identities. Declared sizes must sum exactly
  before bytes are read or allocated. Malformed UTF-8, BOMs, unknown typed fields,
  invalid inline data URLs and mismatched digests grant no ownership authority.
- `QueuePayloadPointer` contains input fields only. `QueueIndexedPayload`
  contains a server-returned pointer and its frozen root index. Parsing an input
  pointer or restoring enqueue/prefix input refuses returned `payloadIndex` and
  `payloadIndexHash`. A new edited capture re-encodes its materialized identities
  through `QueuePayloadDocument::from_item` and uses a new operation identity.
  Explicit recovery retains the original request body and upload identities.
- Payload index hashing uses the backend's schema-normalized JSON field order,
  including attachment descriptor order. `QueueSha256` is a required callback
  supplied by the destination's existing SHA-256 implementation. Index parsing
  checks the digest, owner, safe size counters, identity uniqueness and entry
  bounds. Materialization checks every chunk and whole-capture digest, captured
  configuration, preview, context sequence, entry count and hidden attachment
  roots before exposing the expanded item. Official contexts receive frozen IDs;
  legacy context/instruction IDs remain in the index in send order. Short legacy
  attachment IDs come from verified root bindings; null bindings retain long IDs
  from the capture. Legacy `agentPath` remains the model-facing source identity
  through materialization and editing. Neither it nor server paths authorize
  file reads.
- Manifest uses retain optional `agentPath` independently of the immutable upload
  descriptor's safe `filename`. Local conversion and frozen sends use
  `agentPath ?? filename` for each model-facing occurrence. Present values must be
  nonempty strings; explicit null and other types are malformed. Existing prepared
  names retain relative identities, Windows and UNC paths, literal POSIX
  backslashes, tabs and long strings under the same aggregate limits. No trimming,
  URI decoding or path normalization occurs. Omitted fields remain absent in new
  encoding, and restoration retains original capture, index and frame bytes.
  Different uses of one blob may carry different names; original sealed captures
  bind that metadata while file proofs retain only their existing byte authority.
  The field grants no filesystem read, upload, root or acknowledgement capability.
- Only a sealed materialization grants `QueuePayloadFilesProof`. It binds the
  original capture and index digests to manifest attachment descriptors or the
  actual decoded length and digest of each legacy attachment. Freshly parsed or
  edited captures grant no proof. The native file owner compares these bindings
  with committed local descriptors and rereads local bytes before acknowledging
  a take. Proofs retain compact metadata and have no serialized constructor.
- `QueueLocalCapture` owns complete native local fields and caller-captured context
  IDs without queue upload descriptors. Its `native-local-v1` source bytes retain
  separate display/delivered text, full prompt parts and ordinal file descriptors
  containing actual prepared length and SHA-256. File names may be original
  model-facing paths; they grant no file reads and are separate from safe queue
  upload basenames. Local capture, restoration and explicit conversion of an
  edited queue document grant no queue file proof or acknowledgement capability.
  Local display content shares the existing 50 MiB whole-capture bound. Queue
  manifest and legacy display content retain their 200,000 UTF-16-unit limit.
- `QueueLocalCapture::rebind_context_ids` consumes only a working local capture.
  The caller supplies the occurrence ledger's admitted IDs in exact context order,
  with the existing count, including zero. The constructor validates unique,
  well-formed IDs. Rebinding preserves every other local field and recaptures typed
  source bytes and their digest. Identical IDs return the capture unchanged,
  preserving exact restored bytes and earlier numeric spellings. Original
  materialized payloads, indexes and file proofs remain with their upstream owner.
  Rebinding performs no I/O and grants no ledger claim, queue transfer,
  acknowledgement, upload or path-read authority.
- `QueuePromptParts.file_references` retains ordered explicit server-file
  references as optional `promptParts.fileReferences`. Manifest and typed legacy
  captures accept reference-only empty-text items. Legacy prompt parts contain
  only these references; empty agent, skill and context defaults normalize away,
  while nonempty official parts and any metadata are refused. Absent or empty
  reference lists encode without the new field, preserving existing default bodies
  and index hashes. URI values are at
  most 65,536 UTF-8 bytes and use literal `file:///` absolute paths without an
  authority, query, fragment, backslash, control character or malformed percent
  escape. Names are nonempty, control-free and at most 4,096 UTF-8 bytes. Every
  occurrence counts toward the existing entry and structured-content limits.
  References add no attachment roots, byte descriptors, file proof or
  acknowledgement capability. No reference is fetched or converted to bytes.
- `QueueFrozenSend::capture` remains sealed-queue-only. `capture_local` shares its
  encoder and accepts ordered `Queued` companions and `Local` captures, including
  an ordinary send with no queued items. Every source retains its configuration;
  the caller explicitly selects which one controls the outgoing send. Context
  IDs remain unique in original order, and conflicting top-level metadata keys
  refuse merging before reservation or admission. Local file ordinals and queue
  file identities use separate prepared-binding variants and cannot substitute
  for one another. The caller verifies staged targets against held prepared bytes.
  Each source's explicit references follow its byte-bound files in the actual
  outgoing `files` array, preserving occurrences, descriptions and mentions.
  Frozen bodies include reference bytes in their actual prompt/frame size and
  validate retained file URIs and names again on restoration.
- Frozen send frames retain original JSON bodies in the existing private spool.
  Their metadata is bounded to 4 MiB, the complete frame to 128 MiB, each source
  capture to 50 MiB and combined structured content to 50 MiB. Existing spool
  owners retain their 20 MiB chunk bound. New frame metadata distinguishes queue
  seals from local sources without fake index hashes; older frames remain readable
  as legacy provenance and gain no new ownership capability. Reconstruction and
  explicit retry retain exact original bytes, including earlier numeric spellings.
  Restoration checks metadata structure and the actual retained byte lengths;
  current number notation never changes an older frame's size authority.
  Dispatch and observation perform no uploads, reservations, acknowledgements or
  automatic retries, and never resolve IDs or configuration from mutable UI state.
- `dispatch_step` makes one captured selection or admission request. The caller
  supplies its observed or receipted leading context IDs and can recheck foreground
  gates between requests. Context steps require the next exact ordinal; the prompt
  requires the complete context run. Invalid steps make no request. The whole-send
  `dispatch` method uses the same step implementation and receipt validation.
- New frozen frames tag explicit file-reference ordinals. Strict reference URI
  and name limits apply to those occurrences; prepared-file names retain their
  original contract, including legacy model-facing paths. Earlier frames without
  the optional tag retain their original validation and exact bytes.
- A payload index preview follows the backend's 100 UTF-16-unit slice and may
  contain an escaped unpaired surrogate. The adapter retains those exact units
  privately for index hashing, persistence and capture comparison. The public
  `index().preview` and `index().context_preview` are display-only UTF-8 with replacement characters where
  needed. Persist an indexed payload through `body()` and restore through
  `from_body()`; rebuilding it from the display preview changes its seal.
  Payload prefix composers decode directly into their typed pointer, avoiding
  a `serde_json::Value` intermediate that cannot represent lone surrogates.
  Literal malformed UTF-8, unescaped JSON string controls, array previews and
  previews beyond 103 UTF-16 units remain refusals.
  Captured manifest and legacy context previews use `PayloadPreview`, retaining
  UTF-16 units through parsing and edited capture encoding. Only those display
  fields accept escaped split surrogates; authoritative text and metadata keep
  strict Unicode. Call `display()` for UI text and retain the typed value for edits.
- Edited typed legacy captures omit empty content, text equal to content, empty
  arrays, zero file sizes and the default local source. Every materialized file
  ID remains explicit in a new edited capture. Numeric JSON uses the shorter
  round-tripping decimal or exponential spelling and encodes negative zero as
  zero, preserving JavaScript Number semantics without redundant `.0` suffixes.
  The complete encoded length is counted before allocating the output, with
  equality accepted at 50 MiB. Manifest capture encoding remains unchanged.
  Typed metadata is validated before numeric formatting. Rust integers outside
  JavaScript's safe integer range are refused before conversion; existing finite
  floating-point values keep their JavaScript Number meaning.
- `QueuePayloadTake` keeps transferring and acknowledged states distinct. Its
  own lookup parses compact payload receipts; the original manifest lookup does
  not accept this different handoff shape. `QueueAcknowledge` accepts the shared
  take receipt only after the caller durably owns every part and verified file.
  `QueuePayloadPrefixCapture` checks the exact owner, ordered item IDs, composer
  pointer, delivery and concatenated frozen context IDs. The combined prefix has
  at most 4096 entries and 50 MiB of structured content. Preview and reservation
  retain the same admission barrier and never fall back to destructive take.
- Expanded payload documents belong only to an active operation and the private
  local spool. The native journal stores compact descriptors with byte length and
  SHA-256, original request bodies, pointers and indexes within its existing
  16 MiB limit. Transport defines no new desktop journal schema and performs no
  automatic uploads, downloads, acknowledgement, retries or releases. The caller
  verifies downloaded attachments separately and keeps each original upload
  owner until explicit release is safe. The index's prompt/admission counters
  describe the queue server's encoded requests. Native staged-file sends measure
  their actual outgoing JSON against 128 MiB before any admission; oversized
  server counters do not prevent a complete edit/take handoff.
- Project starters fail as a whole on any malformed entry, and a failed read
  is an error, so a caller never writes back a list built on a failed read.
  Magic-prompt overrides require version 1.
- Every failure is a distinct `TransportError`. None is an empty success; a
  caller keeps its last valid state.
- `all_sessions` walks every page or fails as a whole. A partial list has no
  authority to remove sessions it did not reach.
- A message page shorter than its limit is the oldest page, whatever cursor
  the server still names.
- Older message reads preserve the server's cursor and omit explicit order
  and directory headers, matching the official client's request.
- Session and message routes send no directory header: the server resolves
  the session's own location. Directory-scoped reads encode the directory as
  `encodeURIComponent` does.
- Message pages ask for part shells only when `/health` lists
  `api.message-part-shells.v1`. `session_message` returns the whole message.
- Topology reads use OpenChamber's routes: projects from
  `GET /api/config/settings?surface=desktop`, worktrees from batched
  `POST /api/git/project-worktrees`, running state from
  `GET /api/sessions/status`. None of them starts an OpenCode location. A
  directory the server could not list is reported alone and never erases
  another project's worktrees.
- Settings writes send only the changed keys with
  `PUT /api/config/settings?surface=desktop`, as the web client does, and
  return the merged document the server answers with. General's fields retain
  their actual names: `autoSaveEnabled`, `terminalCloseOnExit` and `terminalShell`.
  Each read field validates independently; an invalid or absent sibling remains
  unknown. The caller owns write ordering, runtime capture and failed-edit state.
- The event stream delivers through a bounded channel and waits for the
  consumer instead of dropping events. A socket that does not send `ready`
  within 2 s or falls silent for 30 s is replaced, as the web client does.
  `Connected { resumed, replay_through }` says whether the server replayed
  everything since the last delivered event, and on a resumed, non-empty
  replay names its last event (the `ready` frame's `replayThrough`); a reset
  retires the cursor. Each `Events { events, event_id }` item is one frame
  with its event id, delivered even when none of its events decode, so the
  consumer can tell when the replay's last event was applied. Dropping `EventStream` stops the
  subscription only; it never stops the server or a running turn.
- Unmodelled event types become `ServerEvent::Unsupported` with their name
  only, never a guessed state change.
- `session.forked` carries IDs only, so it becomes `SessionForked` and the
  consumer reads the record. `session.revert.staged` and `.cleared` patch the
  record's marker; `.committed` becomes `RevertCommitted` followed by the
  patch that clears the marker, so the consumer trims while the marker is
  still known. A staged event without a marker or a commit without `to` is
  `Unsupported`.
- A synthetic record keeps its description and, when its metadata is a valid
  `source: "subagent"` run (`readSubagentRun`), the child, agent and state.
  Valid dispatched-session and workflow report metadata also retains the
  reported session or run identity for transcript turn boundaries, using
  `readDispatchedSessionResult` and `readWorkflowReport` validation rules.
  A tool call's child comes from `metadata.sessionID`/`sessionId`, else its
  `sessionID` input, in history and in the live called, progress, success and
  failed events. Live tool events carry no tool name, so the view decides
  which tools link to their child.
- Promotion of a `/btw` fork drops the marker, then unlinks the parent; a
  failed unlink puts the fork's metadata back, best effort, and fails.
  Stopping a subagent interrupts nothing when the note was not admitted.
- Catalog reads send the directory in `x-opencode-directory`, as the web
  client's `listAgents`, `getProvidersForConfig`, `getConfig` and
  `listMcpServers` do; the skill count reads `GET /api/config/skills` for the
  owning project. Catalog changes (`config`, `agent`, `provider` and `model`
  updates) arrive as one `CatalogUpdated` event naming what changed.
- A session's model and agent come with its record and with
  `session.model.selected` and `session.agent.selected`.
- An assistant message's agent and model come with `session.step.started`,
  and its finish, error and tokens with `session.step.ended` or
  `session.step.failed`, so a replayed stream converges on the history read.
  Compaction records carry their status from history reads and from the
  compaction started, ended and failed events. Catalog models keep a positive
  context window.
- Part times follow `projectAssistantContent` in history reads (text spans
  its step, a tool its run) and the stream's event times live (a text or
  reasoning part from its start, a tool from its call to its result), so the
  replay comparison leaves them out as it leaves out message times. Steps
  carry their cost from history and from the step ended and failed events.
- Permission policies parse `{ sessions, modes?, revision? }` as the web's schema
  does: `modes` is the policy, and without it each legacy `sessions` boolean is
  Auto or Ask. Any malformed entry fails the whole read; a failure is never an
  empty policy. A mode write sends `{ mode, enabled, directory? }` and is never
  retried. For a space directory it also sends `x-opencode-directory` (URI
  encoded, with `x-opencode-directory-encoding: uri`), as the web's
  `runtimeFetch` does, so the write reaches that space's server; a host
  directory travels in the body only.
- `openchamber:permission-auto-accept.updated` and `.left-for-user` arrive on the
  bridge as `PermissionPolicy` and `PermissionLeftForUser`. Frames a host
  forwarded from an isolated space (`spaceId`) are `Unsupported`, because this
  client does not read a space's own policy. `openchamber:routing.updated`
  becomes `RoutingUpdated`, carrying nothing: the consumer rereads `/api/routing`.
- `routing()` reports Auto readiness and the safety net together. A missing
  `jevAvailable` means available, as servers from before the classifier pick
  always ran it.
- `PublicAssetClient` shares nothing with `ServerClient`: it sends no
  credential, cookie or OpenChamber header, requests only HTTPS URLs, follows
  only HTTPS redirects and refuses bodies over 256 KiB.

- Terminal frames are binary in both directions: `0x01`, then UTF-8 JSON.
  A frame the client cannot decode is dropped, as the web client's schema
  drops it. Per attached terminal, an event at or below the last delivered
  sequence is dropped; a snapshot resets that sequence, and output before the
  first snapshot passes, as the web subscriber starts at -1.
- Every (re)open of the terminal socket sends `hello` and one `attach` per
  attached terminal, so the server answers each with a fresh snapshot. The
  client resumes from that snapshot and never from what it missed.
- Input is split into `write` frames of at most 64 KiB after JSON escaping and
  65,536 UTF-16 units of data, at character boundaries and in order. Input the
  socket could not send is reported as `WriteFailed` and never replayed later.
- Reconnects wait `min(500 ms * 2^(n-1), 8 s)`, and 60 s after a 401 or 403.
  The failure count resets when the server's `hello` arrives. Input dials at
  once, as the web client's `write` does.
- With no terminal attached the socket closes after a 15 s grace and dials
  again only on the next attach. Dropping `TerminalSocket` detaches; no
  client action other than `TerminalApi::close` stops a server terminal.
- A terminal command's 4xx answer keeps the server's message and `code`
  (`TERMINAL_CWD_MISSING`); a list read with a malformed envelope is an error,
  while malformed entries are left out.

## Decisions

### Why a command keeps its listing when its config read fails

The palette lists every command OpenCode names. A command's scope badge needs
one more read per command, as `useCommandsStore.loadCommands` makes it; one
failed read keeps that command as listed, without a badge, so one broken file
cannot empty the palette. Only a failed list is an error.

### Why manifest model identities belong to each use

Local edits and frozen sends need the original model-facing identity while byte
handling retains its immutable safe upload descriptor. A per-use optional field
preserves both and keeps duplicate file occurrences independent without adding
path-read or acknowledgement authority.

### Why explicit references have no prepared-file descriptor

PDF and path-only mentions name server files that OpenCode reads when processing
the prompt. Retaining those references preserves their original meaning without
inventing upload identities, byte digests or take acknowledgement evidence.

### Why local captures have a separate source kind

Ordinary rich sends can start without a server queue or upload receipt. Native
prepared descriptors and explicit context IDs validate that local source directly;
the separate type prevents it from acquiring queue-transfer authority by encoding
or mixing with sealed companions.

### Why context rebinding consumes a working local capture

Read-only consumers can need fresh context IDs while the original materialized
capture and proof retain their original identities. The local capture owns this
change so callers preserve rich fields without rebuilding partial copies. An
unchanged binding retains the original bytes because re-encoding can change older
numeric spellings and their digest.

### Why request digits use Serde's supported float formatter

The pinned serde_json formatter uses zmij's shortest nearest-even digits, including
the `2^-25` midpoint where Rust Display chooses a different last digit. Request
serialization adjusts decimal-point placement to ECMAScript notation and uses
the same writer for counting and emission. Compact capture formatting keeps its
existing spelling, and restored frozen bodies remain byte-identical.

The transport enables the existing `serde_json/float_roundtrip` feature so decimal
parsing preserves the exact JavaScript Number across source-spool restoration and
freezing. Cargo unifies that parser feature for consumers of the same serde_json
version in the native dependency graph. It changes parsing precision without
adding a dependency, changing the pinned version or weakening capture limits.

### Why payload captures have separate input and indexed types

The backend derives root indexes after verifying a capture. Separate types keep
returned ownership metadata out of strict enqueue/composer input and let the
existing manifest callers retain their original contract.

### Why payload hashing is supplied by the destination

The desktop already owns SHA-256 for its attachment spool. A required callback
verifies exact payload bytes and schema-ordered index JSON without adding a
second cryptographic implementation or a transport dependency.

### Why payload previews retain UTF-16 separately from display text

The backend's display slice can split an emoji's surrogate pair while its full
capture remains valid. Retaining the original units preserves the sealed index
and full handoff; the UTF-8 display string carries no hash or ownership authority.

### Why edited legacy captures keep compact JSON encoding

Legacy captures can already fill the original 50 MiB budget. Omitting defaults
and using short round-tripping number spellings preserves capacity for a smaller
edit without weakening the capture limit or discarding metadata and identities.

### Why attachment targets are planned before writing

An unresolved prompt must keep its original file URI and bytes. Planning gives
the durable content owner that exact target before a write can reach the server,
including when its reply is lost.

### Why a provider's 401 is not the server's sign-in failing

Source-control routes answer 401 when the provider refused the account
(`SOURCE_CONTROL_ACCOUNT_UNAVAILABLE`, a revoked GitHub token). Every other
authenticated route reports 401 as the server's own sign-in failing, which
locks the window. `source_control.rs` sends through
`RequestBuilder::send_for_provider`, which reads the body first and reports
a credential failure only for the server's refusal (`locked: true`, or no
JSON reason); a provider's 401 reaches the board as that route's refusal.

### Why the Files and Git errors have their own enums

The filesystem and Git routes explain refusals in JSON, and the views show
those explanations (a ripgrep reason, `index.lock` held, a branch Git
refused). `TransportError::Http` carries only a status, which would turn
each of those into a generic failure. The routes' known reasons get
variants, so callers can branch on them without parsing text.

### Why blocking-request errors have their own decoder

Permission/form disappearance is actionable while missing location/session errors
are different failures with the same HTTP status. The narrow decoder checks the
pinned protocol's declared tag and request identity before granting settlement
authority. Existing transport error handling for other capabilities stays unchanged.

### Why the WebSocket bridge and not OpenCode's event stream

`/api/global/event/ws` carries OpenCode's events together with OpenChamber's
own (archive, session metadata, live status, heartbeats) and replays from
`lastEventId` after a reconnect. OpenCode's `/api/event` carries none of the
OpenChamber events and does not replay. When replay cannot resume, the
`ready` frame says so and the consumer reconciles affected scopes from
authoritative reads.

### Why part shells

Finished tool results are most of a message page's bytes and show only when a
row expands. A shelled page keeps everything a collapsed row reads and marks
the left-out length in `PartContent::Tool::elided`; the row fetches the whole
message when it needs the result. The core keeps any whole result it already
holds (see its documentation).

### Why provider logos come from models.dev

The web client draws a provider without a bundled logo with
`https://models.dev/logos/<provider>.svg`, straight from the browser. The
native client fetches the same URL, so both show the same logo. The request
names only the provider ID, and it goes through its own client so the server's
credential cannot reach another host.

### Why differential fixtures

`packages/ui/src/lib/native/transportFixtures.ts` drives the pinned
`@opencode/client` against the recorded responses in
`tests/fixtures/wire/`, records the requests it sends and projects the
responses with the UI's own code. `parity_tests.rs` requires the same
requests (path, query, directory and shells headers), the same projected
sessions and messages, and the same older-page cursors. It also replays the
recorded event frames and requires them to converge on the history the server
returns for the same session. The fixture's own UI test fails when it is
stale, so a client upgrade or projection change surfaces in both languages.

The mutation samples are synthetic contract fixtures, not live provider calls.
The generator records the pinned SDK's model, agent, prompt (text-only and with
every part), synthetic, command and interrupt requests including JSON bodies
and content type, and its decoded admission, synthetic and interrupt
responses. Admission bodies are compared byte for byte. Scripted loopback tests cover rejection, disconnect after
receiving a prompt, local cancellation and the no-redirect credential policy.

### Why submission errors identify the admission stage

OpenCode 2.x persists model and agent selections separately from prompt
admission. A later failure cannot undo an earlier selection. Stage and admission
certainty let the caller preserve its draft and reconcile by message ID without
automatically repeating a mutation that may already be running.

### Why context is resumed by prefix and not item by item

The web client admits context and prompt as separate requests, so a lost
response can leave some steps admitted. Resending only a missing step keeps
the transcript free of duplicates, but only a leading run can be completed in
order: a gap with a later item present would put the resent item after it. The
observation reports that case as `OutOfOrder` instead of guessing.

### Why commands have no identity-based recovery

`session.command` returns 204 and the server mints the message ID, unlike
prompts and synthetic items whose IDs the client chooses. Inbox and history
reads therefore cannot attest a command's admission, and a resend could run it
twice. The error stays `Unknown` and the caller shows it without retrying.

### Why admission does not compare prompt content

OpenCode 2.1.4's [`SessionPrompt.prepare`](https://github.com/anomalyco/opencode/blob/v2.1.4/packages/core/src/session/prompt.ts)
invokes plugin hooks before admission. The prepared text, attachments, metadata
and delivery can differ from the request. The receipt projects only admission
identity, creation time and delivery; history and events supply prepared content.
Outbound parts are sent as given; the receipt never echoes them.

[`SessionInbox.reconcile`](https://github.com/anomalyco/opencode/blob/v2.1.4/packages/core/src/session/inbox.ts)
uses first-admission-wins for an existing message ID with the same session and
type, including an item already promoted into history. Its response can describe
an earlier admission's content. Clients must retain IDs for reconciliation and
must not reuse one for an edited draft. This transport performs no automatic
retry.

### Why observation reads the inbox before history

The pinned 2.1.4 client's `session.inbox.list` reads the session inbox from
`{ data: [...] }`. Admission can precede history insertion. A pending exact-ID
user receipt is sufficient evidence of admission; when it is absent,
`session.message.get` reads `{ data: message }` through the existing whole-message
projection. These are separate snapshots, so absence from both cannot authorize
a resend. History records carry no session ID in the pinned schema; their session
is the requested route, while their ID and user role are checked explicitly.
Differential fixtures pin both SDK calls and response wrappers without a live
provider. Whole-message reads reject a mismatched ID or user content without text.

### Why terminal input is split and the web client's is not

The server's socket accepts frames of at most 64 KiB and closes the connection
(1009) on a larger one, so the web client loses a paste over that size. The
native client splits every write under both server limits; the PTY receives
the same bytes in the same order.

### Why the terminal socket keeps no replay projection

The web client keeps 512 KiB of replay text per terminal so a second view of
the same terminal can start without a server round trip. The native panel
keeps every tab's emulator alive and attached, so there is no second view to
seed; a reconnect replays the server's snapshot.

### Why the terminal socket sends the bearer header

Native sets `Authorization` on the upgrade as the event socket does, which the
server's `ensureSessionToken` accepts. The URL token exists for browsers that
cannot set headers and would put a credential in a URL.

### Recording wire samples

Wire samples come from an isolated server (`bun run profile:serve`) seeded
through `scripts/perf/seed-long-session.mjs` with the fixture provider's
models, so their content is synthetic. Before a sample is checked in, every
absolute path of the isolated project is replaced with
`/home/synthetic/project`, and the sample must contain no other path of the
recording machine. After replacing a sample, run
`bun run native:transport-fixtures` and the transport tests.
