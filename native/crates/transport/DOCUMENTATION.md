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
| `endpoint.rs` | The user-selected base URL, its path prefix and the `ws`/`wss` form of a route. |
| `identity.rs` | `/health` parsing, API compatibility and capabilities. |
| `credential.rs` | The credential a client sends; its `Debug` output is redacted. |
| `client.rs` | `ServerClient`: identity probe, password login, session pages, message pages, whole messages, running status, settings reads and writes, Chats roots, projects and worktrees. |
| `catalog.rs` | Routing readiness (Auto and the safety net) and the composer's catalogs in one directory: agents, the flat model catalog with OpenCode's default model, folded config defaults, the skill count and the connected MCP count. |
| `project.rs` | A directory's Git status (`GET /api/git/status`) and a project's configured actions (`GET /api/projects/:projectId/config`, the path-derived ID). |
| `prompt.rs` | Prompt submission with its synthetic context, resumption of an uncertain send, slash commands, ordered session model/agent selection, inbox admission receipts, observation and interruption of one named session. |
| `prompt_parts.rs` | What a prompt carries besides text (file references, agent and skill references, metadata, synthetic context), the command request, and their private request bodies. |
| `session_actions.rs` | Rename (`PATCH /api/session/:id`, then a read back and a best-effort user-title note), delete (`DELETE`, 404 is already gone), archive and restore (`POST /api/openchamber/sessions/archive` and `/unarchive`), and pin and work state as merge patches to `/api/openchamber/sessions/:id/metadata` computed from a fresh read. |
| `session_create.rs` | Immutable root-session requests, exact-ID observation and identity/location verification before exposing a session record. |
| `requests.rs`, `request_wire.rs` | Explicit blocking-request lists, exact reads, permission replies, form answers/cancellation and private pinned-2.2 wire decoding. |
| `permission_policy.rs` | Per-session permission modes: `GET /api/permission-auto-accept` and `PUT /api/permission-auto-accept/sessions/:id`, both answering the whole policy. |
| `public_asset.rs` | `PublicAssetClient`: public assets the web client loads from third-party hosts, such as models.dev's provider logos. |
| `wire.rs` | Serde shapes of the fields the client reads. Private. |
| `projection.rs` | Wire records to core records, mirroring `packages/ui/src/lib/opencode/projection.ts`. Private. |
| `event.rs` | Bridge frames to `ServerEvent`s, mirroring `lib/opencode/events.ts` and `sync/event-pipeline.ts`. |
| `stream.rs` | `EventStream`: the reconnecting WebSocket subscription. |
| `parity_tests.rs` | Differential tests against the official client's fixture. |
| `tests/server.rs` | The transport against a scripted loopback HTTP and WebSocket server: login fields, bearer use, failure classes, redirects, paging, cancellation, resume and reset cursors, fragmented frames and the `ready` timeout. |

`examples/probe.rs` exercises the crate against a running server and prints
counts and identifiers only.

## Invariants

- `host_session_status` retains optional pending permission/form identity hints
  from `/api/sessions/status`. Invalid entries are omitted independently; omission
  of the whole pending field means unsupported. Hints grant neither action nor
  removal authority. Desktop resolves relevant directories and reads full requests
  before exposing actions. The existing `session_statuses` convenience method
  continues to return only live statuses.
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
  return the merged document the server answers with.
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

## Decisions

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

### Recording wire samples

Wire samples come from an isolated server (`bun run profile:serve`) seeded
through `scripts/perf/seed-long-session.mjs` with the fixture provider's
models, so their content is synthetic. Before a sample is checked in, every
absolute path of the isolated project is replaced with
`/home/synthetic/project`, and the sample must contain no other path of the
recording machine. After replacing a sample, run
`bun run native:transport-fixtures` and the transport tests.
