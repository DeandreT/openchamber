# Native Rust desktop architecture

The Rust desktop is a native OpenChamber client built with GPUI Kit. Its
application model is independent of the renderer and network libraries.
OpenCode remains the agent engine and authoritative owner of sessions,
messages, tool execution and provider configuration.

This document defines the target architecture. The
[implementation stages](IMPLEMENTATION.md) identify the boundaries to build and
qualify before a desktop release.

## UI parity requirement

The repaired Electron desktop is the reference for appearance, interaction,
accessibility and feature coverage. Follow the [desktop UI parity method](PARITY.md)
for each migrated feature: capture its baseline, run matched visual and behavior
scenarios, resolve differences and retain regression coverage. Intentional
differences require the user's approval. A framework limitation leaves a feature
incomplete until its required behavior is supported or the user changes the
requirement.

The native prototype grows feature by feature. Electron remains usable through
development, and its replacement requires full inventory coverage and the host
and release checks. Each feature's parity gate applies throughout the migration.

## Runtime ownership

```text
GPUI desktop views
        |
        | actions, state changes
        v
Rust application core <----- parsed results and domain events
        |                                ^
        | operation requests             |
        v                                |
Rust transport adapter ------------------+
        |
        | authenticated HTTP and event streams
        v
Existing OpenChamber server
        |
        | OpenCode proxy and server-owned services
        v
OpenCode
```

The first native window uses deterministic in-memory data. The connected client
uses an explicitly selected OpenChamber server. That server supplies projects,
files, Git, settings, terminals, session metadata and the OpenCode proxy.
Connecting to OpenChamber preserves its archive and metadata overlays.

The server stays externally owned during client development. Disconnecting or
closing the native client releases its requests and subscriptions. It does not
stop the server or an agent turn. A local managed host is a later migration
stage with its own process-lifetime contract.

| Runtime | Behavior during the desktop migration |
| --- | --- |
| Native Windows desktop | GPUI views with Rust application state; synthetic data first, explicit server connection next |
| Electron desktop | Existing React client and in-process OpenChamber backend |
| Web | Existing React client and OpenChamber server |
| VS Code | Existing webview and extension-host adapters, with workspace-scoped session behavior |
| Hosted mobile | Existing mobile web interface against its selected server |
| Capacitor mobile | Existing native mobile shell and mobile web interface |

Each client retains its renderer. Shared protocol fixtures establish behavior
across clients. Sharing Rust code with browser clients requires a separate
measured integration at an actual application boundary.

## Source layout and dependency direction

Use a Cargo workspace under `native/`, separate from the Bun `packages/*`
workspace. Introduce each crate when its first consumer exists.

| Planned crate | Owns | Allowed application dependencies |
| --- | --- | --- |
| `openchamber-native-core` | Domain records, sidebar membership, ordered messages, draft identity and state transitions | Standard Rust library; no renderer, network or filesystem APIs |
| `openchamber-native-desktop` | GPUI entities, window lifetime, text/layout adapters, theme and locale bindings, composition of the application | Core; transport when connected behavior is added |
| `openchamber-native-transport` | Endpoint identity, authentication, typed wire parsing, request cancellation and stream decoding | Core, Serde, Tokio, Reqwest, event decoder |

Native host operations start as focused modules in the desktop crate. Extract a
platform crate when it owns a reused contract. A future Rust server can consume
core types without importing GPUI.

The core accepts trusted, explicit inputs and returns owned values, stable
identifiers or borrowed views. JSON belongs to transport and fixture adapters.
Rendering asks for a projection and dispatches an action; it does not interpret
wire events, write server state or mutate unrelated stores.

Keep `main` small. It initializes assets, theme, locale and the window, then
hands application lifetime to one owner. Network startup and history parsing
must not block the first usable window. Avoid empty adapter traits and forwarding
crates created solely for anticipated future ports.

## Packages and version ownership

Pin direct dependencies in the Cargo workspace and commit its lockfile. Use
Rust 1.97.1 with the Windows MSVC target for the initial implementation.

| Dependency | Initial pin | Responsibility |
| --- | --- | --- |
| `gpui-kit` | `0.7.3` | Native window, components, text, lists, assets and AccessKit integration |
| `serde` / `serde_json` | `1.0.229` / `1.0.154` | Typed JSON adapters and fixture decoding |
| `tokio` | `1.53.3` | Background network tasks, bounded delivery and cancellation |
| `reqwest` | `0.13.5` | HTTP transport |
| `eventsource-stream` / `futures-util` | `0.2.3` / `0.3.34` | Incremental SSE decoding and stream consumption |
| `tracing` / `tracing-subscriber` | `0.1.44` / `0.3.23` | Local structured diagnostics and timing |
| `thiserror` | `2.0.21` | Typed errors at adapter boundaries |

Add dependencies with their first use. GPUI Kit re-exports its matching GPUI
snapshot. Its 0.7.3 manifest pins the GPUI snapshot family to 0.3.9. Use those
types through Kit so a separately selected GPUI release cannot create a second
incompatible type family. GPUI's executor owns UI work; Tokio owns network work.
Neither executor waits synchronously for the other.

Later integrations use `gpui-wry 0.7.3` with its `lb-wry 0.53.3` dependency for
browser embedding, and `tokio-tungstenite 0.30.0` when a runtime WebSocket is
needed. Native process work uses the `windows` APIs. Filesystem watching can use
`notify`. Terminal hosting can use `portable-pty`; terminal emulation and
rendering have separate contracts.

Retain existing server-backed storage first. Introduce `rusqlite` only for an
OpenChamber-owned persistence requirement with a defined schema and migration.
The client never opens the OpenCode database to replace API access.

GPUI and GPUI Kit use Apache-2.0. Audit resolved dependencies and bundled assets
at release time. Zed's editor crate uses different license terms; it is not a
dependency of this design.

## Application state

Every asynchronous operation captures its runtime identity, connection
generation, scope and starting mutation revision. A runtime includes the
verified server identity and its configured endpoint path prefix. Credentials
remain private to the transport owner.

| State | Authority and lifetime |
| --- | --- |
| Global session index | Active and archived records from global server reads; completeness tracked per query and page sequence |
| Directory live state | Events and successful scoped reads for initialized directories; running state and pending requests come from live channels |
| Ordered transcript | Server message order and stable message/part identities; history and live changes reconcile under one session owner |
| Draft | Client-owned text and editing state keyed by runtime, actual directory and session or draft identity |
| Scroll memory | Client-owned message anchor, pixel offset and tail-follow state keyed by runtime and session |
| Presentation | Selection, expansion, focus and text selection; never a source of server ownership or activity |

Model loading explicitly as absent, loading, partial, complete or failed with a
retained snapshot. An empty complete response can replace its exact scope. A
failed response preserves prior data. A page failure retains earlier pages
without granting full-list authority. Cleanup applies only where the server
contract establishes complete coverage.

When a load completes, reject an obsolete generation and overlay mutations newer
than its starting revision. Include removals in that revision history even when
the entity was already absent. A delayed page must not restore a deleted record
or move a session back to its former directory.

Use stable message and part IDs. Preserve server order; ID spelling does not
establish chronology. An unchanged event produces no notification. A text delta
updates its part and affected row without rebuilding the session tree. Coalesce
adjacent text deltas only when identity and ordering permit it. Permission,
completion, deletion and ownership events remain ordered and lossless.

Network delivery uses a bounded queue with backpressure. If a connection fails
before complete delivery, retain the last valid state and reconcile from the
server. Do not drop events to make room or describe an interrupted stream as a
successful completion.

### Sidebar visibility

The native projection preserves the fork's compact layout and session reachability:

- Use the global session index for unopened-directory and archive coverage.
  Expanding or rendering a project row does not initialize its OpenCode location.
- Preserve an active unowned root in Chats until configured topology or canonical
  project metadata resolves its owner. Include its active descendants across
  directories and retain owned children's project navigation.
- Keep fallback roots out of duplicate project, Pinned and In work rows. Do not
  promote a child whose parent is missing, archived or a temporary `/btw` session.
- Use the real session directory for routing, pin identity and drafts. Display
  grouping never rewrites it. OpenCode's catch-all `global` project ID cannot
  establish configured-project ownership.
- Apply authoritative moves without requiring a new prompt, including a move
  announced after the agent becomes idle. Failed topology discovery keeps the
  fallback reachable. Search and collapse preferences retain their existing rules.
- Keep activity separate from membership. Token arrivals do not reorder the
  structural sidebar. Running and pending-request indicators use live state.

A projection over classified facts covers display membership. Directory
normalization, ownership discovery and live event reconciliation need their own
native implementation and tests.

## Server and OpenCode adapters

Separate OpenChamber-owned routes from official OpenCode routes inside the
transport crate. Feature code consumes typed domain operations.

The repository pins OpenCode through the Electron and JavaScript package
manifests. Native compatibility fixtures follow that same version. A native
protocol update belongs in the same change as an engine/client pin update.

OpenCode's documented SDK is TypeScript. The connected Rust stage requires an
explicit language-specific adapter decision before implementation. That decision
must preserve the repository's SDK boundary: use the pinned official contract,
compare Rust requests and parsed responses with the official client, and confine
wire details to the transport module. Existing React clients keep using
`opencodeClient`. Do not infer routes from old OpenCode versions or handwritten
examples. Resolve this prerequisite before the connected stage.

The connection sequence is:

1. Resolve the user-selected endpoint and verify the service identity without
   sending credentials. Preserve reverse-proxy path prefixes and reject redirects
   that could change the credential destination.
2. Authenticate through the server's existing client-token contract. Read
   capabilities and OpenCode compatibility. Authentication required,
   incompatible, unavailable and malformed responses are distinct outcomes.
3. Subscribe to the global event stream and begin bounded global session paging.
   Capture a load revision and reconcile events received during the snapshot.
4. Load the selected transcript and initialize only its required directory.
   Directory-scoped reads carry the encoded directory in the supported contract.
5. On stream interruption, mark affected live scopes stale and reconcile them.
   The OpenCode stream is live-only; reconnecting does not replay missed events.
   Refresh foreground or previously live scopes with bounded work. Known project
   membership alone never starts a location.

The adapter validates status, content type and payload shape before granting
authority. Missing, null and empty fields retain their contract meanings. Unknown
event types do not become fabricated state changes; record their type without
payload content and recover a supported affected scope when possible.

A runtime switch cancels old requests and subscriptions, invalidates their
generation, then installs the new endpoint, credentials and state. Draft and
viewport records keep their original runtime keys. Authentication does not leak
through redirects, logs, WebView contents or a fallback endpoint.

## Native interface design

### Layout and tokens

Use the existing desktop arrangement: compact session sidebar at the inline
start edge, conversation header and transcript in the center, composer anchored
at the bottom. The initial window has no unused panel placeholders. Add the
existing context panels when their behavior is implemented.

Map GPUI colors from the repository's semantic theme JSON. Use the selected
light/dark pair, including `surface`, `interactive`, `chat`, `markdown` and
`syntax` tokens. Preserve alpha and selection semantics. Controls use GPUI
components styled to OpenChamber's raised-control treatment; layout regions stay
flat. Typography and spacing come from `design-system.css` and the current
sidebar/composer styles. Avoid a separate native palette.

Read UI labels from generated native resources sourced from the existing i18n
dictionaries. The generator exports the used keys for every supported locale and
checks key/placeholder parity. Rust views resolve keys at render time. Locale
changes preserve drafts and focus. Arabic and Hebrew mirror logical layout;
window system buttons retain their platform placement. New visible strings are
translated in the canonical dictionaries before export.

### Transcript

Use Kit's `MessageScroller` and `TextView` as the first implementation. Retain
message collection ownership in the core, one scroller state per open transcript
and stable row IDs. Apply append/prepend/remeasure operations to the existing
scroller; resetting it for every delta would discard anchor behavior.

Keep tail following explicit. Growth follows the end only while the reader is
at the tail. A reader away from the tail keeps the same visible message and
offset through streaming, prepend and width changes. Restore a saved anchor
before revealing a switched transcript. Measured row heights supersede initial
estimates; test short rows as well as tall replies.

Kit 0.7.3's public MessageScroller API does not expose persisted pixel offsets.
Exact restoration therefore needs a focused adapter over its supported lower
level list APIs or an upstream addition. Verify that access before accepting the
scroll design. Returning to the right message at the wrong offset fails parity.

Text selection belongs to message/part ranges, independent of row mounting.
Copy must include the complete selected text after scrolling offscreen and back.
Selectable individual TextViews alone do not prove selection across messages.
Keep UTF-8, UTF-16 and grapheme coordinates explicit at text adapters.

Render Markdown, code, tables and local fixture images. Executable HTML and
extension panels use the browser boundary described below. Link activation
follows existing scheme and confirmation rules; markup cannot call native host
operations.

### Composer and focus

Use a retained Kit input entity. Preserve text, selection, undo state and IME
composition when unrelated rows update. Changing sessions saves the outgoing
draft under its captured identity and restores the incoming draft before paint.
Materializing a draft retains the active editor.

Match existing submit/newline shortcuts. Enter during IME composition commits
text without sending a prompt. The synthetic mode routes submission to an
in-memory conversation and a deterministic response stream. It carries a visible
development identity and makes no provider calls.

The connected mode captures session, directory, model, agent, variant and
attachments with the send operation. A failed or ambiguous response cannot erase
a newer draft or restore text into whichever session is currently selected.
Reconcile an ambiguous send before retrying a mutation. Prompt, abort, permission
and form operations need their complete recovery paths before becoming available.

### Accessibility

Provide stable AccessKit IDs, roles, names, state and actions for actual controls.
Keyboard, pointer and accessibility activation dispatch the same domain action.
Expose loading and failure states in text, with a usable retry action where
supported. Streaming announcements must not announce the whole transcript for
each delta.

Test painted semantic trees and a real Windows window. Narrator, keyboard focus,
IME candidate placement, text selection, zoom/DPI changes and mixed-direction
text require platform checks. A headless tree test establishes only its own
semantic assertions.

## Browser, terminal and host boundaries

Browser views execute content that native Markdown cannot. Preserve the SDK's
iframe envelope, capability grants, origin/navigation checks, runtime scoping
and cancellation. Extensions receive only the host contract, never credentials
or unrestricted native functions. Keep browser identity and storage separated
by the owning runtime and extension installation.

Prove Wry composition before selecting it for production: popup layering,
clipping, resize, focus, devtools/control, profile isolation and shutdown. Its
documented native child-view overlap limits affect menus and dialogs. Arbitrary
browser pages, file previews and SDK panels each need their existing security
contract. Windows success does not qualify Linux or macOS.

Keep the existing server's terminal sessions initially. Evaluate native bindings
to the current `libghostty-vt` emulator before changing emulators. PTY creation,
VT state, rendering, clipboard/paste, input/IME and resize are separate owners.
Disconnecting a client follows the server's terminal lifetime rules.

Before adding managed local services, define spawn ownership, shutdown order and
failure cleanup for each child. Wait for owned children to exit and retain enough
identity to avoid killing another process after PID reuse. Never infer ownership
from executable name alone. External servers remain externally owned.

Server-owned enterprise policy continues to apply to native requests. Native
features that load external assets, launch services, add providers or expose
listeners also enforce the applicable machine policy at their own boundary.
Keep the established fail-closed policy precedence. The synthetic client adds no
listener or external-content fetch; the first connected mode reaches only its
explicit server. Relay support requires the existing authenticated encryption,
multiplexing and flow-control contracts before it can advertise compatibility.

## Persistence and rollout

Use a separate development identity and explicit disposable data root for the
native prototype. Synthetic state is in memory. Later durable drafts and native
preferences use versioned, runtime-scoped storage with atomic writes and an
explicit close/flush policy. Retention cannot silently discard unsent content.

Existing Electron profiles, server data and OpenCode storage retain their owners.
A desktop replacement requires an explicit migration for any local preferences
or drafts the user needs to retain. Package signing, installers, updates, SSH and
deep links belong to the host stage. Keep releases independently launchable until
those contracts and the feature inventory qualify the replacement.

## Contract references

- [Electron host](../../packages/electron/README.md)
- [Sync authority and recovery](../../packages/ui/src/sync/DOCUMENTATION.md)
- [Sidebar membership and compact rows](../../packages/ui/src/components/session/sidebar/DOCUMENTATION.md)
- [Transcript scrolling](../../packages/ui/src/components/chat/lib/scroll/DOCUMENTATION.md)
- [Composer](../../packages/ui/src/components/chat/composer/DOCUMENTATION.md)
- [Theme tokens](../../packages/ui/src/lib/theme/themes/flexoki-dark.json)
- [Locale ownership](../../packages/ui/src/lib/i18n/runtime.ts)
- [Authentication](../../packages/web/server/lib/ui-auth/DOCUMENTATION.md)
- [Extension contract](../../packages/sdk/README.md)
- [Relay](../../packages/web/server/lib/relay/DOCUMENTATION.md)
- [Enterprise policy](../../packages/docs/content/docs/security.mdx#enterprise-mode)
- [GPUI Kit package pins](https://github.com/longbridge/gpui-kit/blob/v0.7.3/Cargo.toml)
- [MessageScroller contract](https://github.com/longbridge/gpui-kit/blob/v0.7.3/website/component/message-scroller.md)
- [WebView contract](https://github.com/longbridge/gpui-kit/blob/v0.7.3/website/docs/webview.md)
- [OpenCode client contract](https://opencode.ai/v2/docs/build/client)
