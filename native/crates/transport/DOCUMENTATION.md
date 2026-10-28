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
| `catalog.rs` | The composer's catalogs in one directory: agents, the flat model catalog with OpenCode's default model, folded config defaults, the skill count and the connected MCP count. |
| `project.rs` | A directory's Git status (`GET /api/git/status`) and a project's configured actions (`GET /api/projects/:projectId/config`, the path-derived ID). |
| `prompt.rs` | Typed text submission, ordered session model/agent selection, inbox admission receipts and interruption of one named session. |
| `session_create.rs` | Immutable root-session requests, exact-ID observation and identity/location verification before exposing a session record. |
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
- `send_text` posts a model selection to `/api/session/:id/model`, then an
  agent selection to `/api/session/:id/agent` when supplied, and finally the
  text and exact client-generated message ID to `/api/session/:id/prompt`.
  Selection responses are 204. The prompt's 200 response contains an inbox
  user item under `data`; it acknowledges admission, not turn completion.
  Model references use `providerID`, `id` and optional `variant`. Omitting a
  variant clears the previous effort selection, matching `toModelRef`.
- Prompt IDs must carry the pinned protocol's `msg_` prefix. Validation runs
  before any selection mutation, and `TextPrompt` debug output redacts its text.
- Prompt failures retain the session and message IDs and the failed stage.
  `NotAttempted` means selection or local validation failed before the prompt.
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
- `TextPrompt` accepts text only. Files, agent mentions, skills, metadata,
  synthetic context and slash-command execution require separate typed flows.
  A slash-prefixed string sent through this API is literal text. Admission
  receipts validate the message ID, session and user type, and parse the server's
  timestamp and delivery. They intentionally leave prepared payloads to history
  and events. OpenCode's `session.prompt` hook can transform text, add metadata
  and attachments, and change delivery; a successful receipt does not promise
  an exact echo of the request.
- `abort_session` posts only to the named session's `/interrupt` route with no
  body or resume query. Its boolean is the server's `interrupted` value; false
  is a successful no-op. A lost or malformed response leaves the interruption
  outcome unknown. It never interrupts other sessions or stops the server.
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
  `Connected { resumed }` says whether the server replayed everything since
  the last delivered event; a reset retires the cursor. Dropping `EventStream` stops the
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
- `PublicAssetClient` shares nothing with `ServerClient`: it sends no
  credential, cookie or OpenChamber header, requests only HTTPS URLs, follows
  only HTTPS redirects and refuses bodies over 256 KiB.

## Decisions

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
The generator records the pinned SDK's model, agent, prompt and interrupt
requests including JSON bodies and content type, and its decoded admission and
interrupt responses. Scripted loopback tests cover rejection, disconnect after
receiving a prompt, local cancellation and the no-redirect credential policy.

### Why submission errors identify the admission stage

OpenCode 2.x persists model and agent selections separately from prompt
admission. A later failure cannot undo an earlier selection. Stage and admission
certainty let the caller preserve its draft and reconcile by message ID without
automatically repeating a mutation that may already be running.

### Why admission does not compare prompt content

OpenCode 2.1.4's [`SessionPrompt.prepare`](https://github.com/anomalyco/opencode/blob/v2.1.4/packages/core/src/session/prompt.ts)
invokes plugin hooks before admission. The prepared text, attachments, metadata
and delivery can differ from the request. The receipt projects only admission
identity, creation time and delivery; history and events supply prepared content.
This leaves the outbound text-only contract unchanged.

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
