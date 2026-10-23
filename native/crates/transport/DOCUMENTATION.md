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
| `client.rs` | `ServerClient`: identity probe, password login, session pages, message pages, whole messages, running status, settings, Chats roots, projects and worktrees. |
| `catalog.rs` | The composer's catalogs in one directory: agents, the flat model catalog with OpenCode's default model, folded config defaults, the skill count and the connected MCP count. |
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

- The identity probe sends no credential. A server that does not answer an
  OpenChamber identity is `NotOpenChamber`, and a server whose API range
  excludes this client is `Incompatible`, before any credential is sent.
- Redirects are never followed, by HTTP or WebSocket requests, so a credential
  never moves to another origin.
- A credential appears only in the `Authorization` header, marked sensitive.
  It is never part of a URL, an error, a log line or `Debug` output.
- Every failure is a distinct `TransportError`. None is an empty success; a
  caller keeps its last valid state.
- `all_sessions` walks every page or fails as a whole. A partial list has no
  authority to remove sessions it did not reach.
- A message page shorter than its limit is the oldest page, whatever cursor
  the server still names.
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

### Recording wire samples

Wire samples come from an isolated server (`bun run profile:serve`) seeded
through `scripts/perf/seed-long-session.mjs` with the fixture provider's
models, so their content is synthetic. Before a sample is checked in, every
absolute path of the isolated project is replaced with
`/home/synthetic/project`, and the sample must contain no other path of the
recording machine. After replacing a sample, run
`bun run native:transport-fixtures` and the transport tests.
