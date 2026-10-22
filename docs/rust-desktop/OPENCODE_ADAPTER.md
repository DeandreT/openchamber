# Rust adapter for OpenChamber and OpenCode APIs

The native desktop reaches OpenCode only through an OpenChamber server, as the
other clients do. This document is the language-specific adapter decision the
[architecture](ARCHITECTURE.md) requires before the connected stage.

## Decision

`openchamber-native-transport` owns every request and parse. It holds
hand-written Serde types for the subset of the wire contract the native client
uses, and converts them into core records at its boundary. No other crate sees
a wire type.

The TypeScript `@opencode/client` and `@opencode/schema` packages pinned in the
repository manifests remain the compatibility source. Two generated fixtures
tie the Rust code to them:

- **Requests.** `packages/ui/src/lib/native/transportFixtures.ts` drives the
  official client against a recording `fetch` for each operation the native
  client performs and records the method, path, query, headers and body. The
  Rust request builders must produce the same request for the same inputs.
- **Responses.** The same generator feeds recorded server responses through the
  UI's own projection (`lib/opencode/projection.ts`, `events.ts`) and records
  the domain values. The Rust parsers must produce the same records from the
  same bytes.

Each fixture names the pinned client version. A change of that pin regenerates
the fixtures in the same change, and a Rust divergence fails the transport
tests. The UI test for the fixture fails when it is stale.

## Why not generate Rust from the schema

`@opencode/schema` is written with Effect Schema, and the published package
carries TypeScript declarations rather than a language-neutral schema. A
generator would have to interpret Effect's runtime types and would still not
cover OpenChamber's overlays (archive, session metadata, spaces) or the
WebSocket bridge frames. The native client needs a small subset; differential
fixtures against the official client catch the drift a generator would
prevent, at a fraction of its cost.

## Why not call OpenCode directly

OpenChamber owns archive state, session metadata writes, spaces, Chats roots,
project and worktree topology, and authentication. The server folds those into
the records it serves. A client that talked to OpenCode directly would lose
them and would need OpenCode's credentials. The native client therefore uses
the server's `/api/*` proxy for OpenCode routes and its own routes for
everything else, keeping the two groups separate inside the transport crate.

## Contract

- **Endpoint.** A user-selected base URL, including any reverse-proxy path
  prefix. Redirects are never followed, because a redirect can move the
  credential to another origin.
- **Identity.** `GET <base>/health` without credentials must answer an
  OpenChamber identity (`openchamberVersion`, `runtime`, `compatibility`,
  optional `serverId`) before any credential is sent.
- **Authentication.** A server without a UI password needs none. A password
  server issues a client token from `POST <base>/auth/session` with
  `issueClientToken: true`; later requests send `Authorization: Bearer`.
  The token never appears in a URL, a log line or a diagnostic marker.
- **Directory scope.** Directory-scoped OpenCode reads send the directory in
  the percent-encoded `x-opencode-directory` header. Global reads send none.
  Every scoped read names a directory the user works in, because a read starts
  that location's MCP servers.
- **Events.** The server's `/api/global/event/ws` bridge carries OpenCode's
  wire events, OpenChamber's own session events and replay identifiers. The
  native client subscribes there, with the bearer token in the upgrade
  request, and reconciles affected scopes after a gap.
- **Outcomes.** Unreachable, not OpenChamber, authentication required,
  authentication rejected, incompatible, malformed and failed are distinct
  results. None of them is reported as an empty success.
