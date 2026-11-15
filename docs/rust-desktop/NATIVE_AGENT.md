# Native-agent protocol v1

The SDK owns this protocol. Its wire types are in
`packages/sdk/src/native-agent.ts`, runtime validators in
`native-agent-schemas.ts`, and shared examples in
`packages/sdk/fixtures/native-agent-v1.json`. The Devin-labelled fixture is a
contract example with invented opaque model IDs. It makes no claim about
Devin's service API or available models. The second provider exercises the
same protocol without Devin identifiers.

## Discovery and transport

Only providers whose `settings.openchamber.nativeAgent` parses as
`{ "protocolVersion": 1 }` receive calls. Use the exact provider ID in
`openchamber.native-agent.<providerID>.v1`. Do not probe ordinary providers or
guess capability from a provider name. Unsupported versions remain unsupported.

Register through OpenCode 2's `ctx.rpc.register` and `Rpc.define`, using the
SDK schemas as Standard Schema inputs, outputs and event schemas. Each of the
5 methods returns `NativeAgentResult`. Expected domain errors use that result;
OpenCode transport/RPC errors remain transport failures. Subscribe to local
event names `stateChanged` and `toolProgress`; OpenCode publishes
`rpc.<rpcID>.<eventName>` with `{data, location}`. Validate the inner `data`
with `nativeAgentEventSchema` and check `location.directory` as well.

The client captures the selected server's identity, authenticated transport and
directory before sending. Server identity is local context, never a plugin
URL. Requests use OpenCode's location header `x-opencode-directory` with the
percent-encoded actual directory. The wire scope must agree with that location
and the provider owning the RPC. A session must belong to that directory.
Reject mismatches inside the plugin before side effects.

Use the same contract for native desktop, web, Electron, VS Code, hosted mobile
and Capacitor clients. This SDK adds no listener, provider entry, credentials
or network request. Runtime adapters retain their existing transport, provider
policy and enterprise gates. Authentication is provider entry and remains
subject to those gates at its executing boundary. Capability advertisement
never overrides policy or authorizes an answer.

## Calls and snapshots

All calls contain `protocolVersion: 1`, an opaque `requestID`, a `scope` with
`directory`, `providerID` and `sessionID`, and `activationID`. `sessionID: null`
means preparation for a draft in that exact directory. Never substitute the
server's home directory or an unrelated selected project. A new session is a
new scope and gets its own preparation; draft state is not silently promoted.

- `getState` reads a complete snapshot without preparing, authenticating or
  submitting anything. `activationID: null` asks for the current activation.
- `prepare` initializes or reconnects the provider runtime. It may inspect an
  existing login but must never launch sign-in, submit a prompt, answer a
  request or replay a submission. It is safe to repeat with a fresh request
  ID. `activationID: null` discovers the current activation.
- `authenticate` adds `expectedRevision` and `userInitiated: true`. It requires
  a non-null activation and a direct explicit user action. No secret is
  accepted or returned by this protocol. The plugin runs its supported login
  flow and owns credentials.
- `setConfig` adds `expectedRevision` and `changes: [{id, value}]`. IDs and
  values are opaque. Validate every change against the current advertised
  choices, then commit all changes atomically or reject all of them. Do not
  infer options from labels or a running process. Stopping preserves choices.
- `answer` adds `expectedRevision` and `answer`. Validate the request ID, kind,
  permission choice or question field values against the current snapshot.
  Permission choices are explicitly `allow` or `deny` for this request only.
  No standing policy mutation is implied. Questions reject unknown or duplicate
  fields/choices, missing required values, disallowed text and excess choices.
  Call `validateNativeAgentAnswer` after parsing on both sides.

Mutation calls require the current non-null `activationID` and exact
`expectedRevision`. Each successful method answers
`{protocolVersion:1, requestID, ok:true, state}`. A known failure answers
`{protocolVersion:1, requestID, ok:false, error:{code, outcome}, state?}`.
`outcome` is `rejected` only when no side effect occurred, otherwise `unknown`.
No arbitrary error message, stack, command output, credentials or raw service
body crosses the diagnostic boundary. Translate allowlisted codes locally.
Optional failure state is complete authoritative state, never an empty fallback.

### Preparation after session creation

`capabilities.prepareAfterSession` is an optional boolean. Omitted or false
preserves the existing draft-preparation and immutable draft-binding workflow.
True makes null-session preparation config-only: status is `unprepared`,
execution is null, requests are empty and submission is `none`. Draft reads,
prepare and saved configuration changes start no SDK process, authentication,
cloud session or variant publication. Draft authentication is `not_prepared`.
Older strict v1 clients can reject the additional capability field; providers
requiring this workflow need an updated client. A capability advertises behavior
and grants no provider permission.

The first Send captures the actual directory, routing provider/model, resolved
agent, full message and ordered parts/resources in the creation journal before
creating a real OpenCode session. The creation request establishes that captured
agent and routing model. The plugin evaluates current `provider.use` for that
real session before execution side effects, including on repeated RPC calls.

The client reads the real session's unprepared state and applies captured pairs
whose IDs already exist there before calling prepare. It validates those known
values against the offered choices. Other captured IDs remain pending for live
discovery. This permits provider-owned runtime selection before allocation without
the client interpreting configuration labels or field IDs. After prepare, every
captured ID/value must validate; newly discovered fields keep their authoritative
defaults. The client applies the complete captured subset and admits the prompt
only with the plugin's real ready immutable execution binding.

`state.configComplete` is an optional boolean. For session-first providers, only
true certifies a complete validated current exposed config catalog. Omitted or
false means partial or unreported; authentication, ready status, success and error
codes establish no completeness authority. Draft config-only state keeps it false.
A real execution binding requires it true. Legacy providers with an omitted/false
preparation capability keep their existing behavior.

An explicit reported empty SDK array can establish completeness, together with
the plugin's runtime field. Missing, unreported or malformed supported options
return `unavailable`, preserve previous config and expose null execution with
`configComplete: false`. A complete refreshed catalog may use true on an invalid
saved-choice response. The client retains unknown captured pairs through partial
snapshots and accepts removal only during an explicit correction using the
current true marker. A lazy cloud session that never reports its catalog remains
blocked. The client sends no discovery prompt and synthesizes no empty catalog.

Authentication retains the accepted message and can resume that live continuation
once. Invalid configuration retains its frozen intent and the original message.
Changing a displayed choice blocks automatic continuation. An explicit Retry same
message accepts the pending session's displayed configuration and saves it before
dispatch. Before live discovery it retains still-unknown captured pairs; after
live discovery the user may accept the complete displayed configuration, including
removal of obsolete fields. A retry outside that session reveals it first, without
accepting hidden choices. The message ID, provider, original routing model, agent,
directory, text, files and ordered context stay captured throughout correction.
Restart restores the pending capture without a continuation or automatic replay.
Unknown prompt admission still requires exact-ID observation before any retry.

The plugin serializes state changes per scope, checks mutations at execution
time and increments its safe-integer `revision` for every observable snapshot
change. An activation ID is unique for each new runtime ownership lifetime,
including plugin reload. Revisions may reset only with a new activation.
Duplicate mutations with the same request ID and identical payload must never
execute twice. Reusing an ID with different parameters is `invalid_request`.
The plugin owns durable recovery records for side effects that can outlive a
process, including unresolved submissions and answers. An unavailable runtime
or lost connection never proves that a previous operation failed.

`getState` and `prepare` with a stale non-null activation return
`stale_activation`, optionally with current state. A client can issue a new
generation-scoped read with a null activation to discover the replacement.
It does not retry the mutation against that replacement automatically.

## Client reconciliation and ownership

### Immutable execution binding

Every state includes `execution`, either
`{providerID, modelID, variant}` or `null`. Its provider must match the scope.
These are the ordinary OpenCode routing coordinates. `modelID` selects a plugin
execution route such as local or cloud; it is independent of any opaque native
configuration field whose semantic role is `model`.

The plugin creates a credential-free immutable configuration snapshot in its
durable registry, then publishes an OpenCode model variant whose
`settings.nativeConfigToken` references that snapshot. The pinned OpenCode
provider path passes those settings to `doStream.providerOptions[providerID]`.
Only after registry persistence and catalog publication both succeed may the
plugin advertise that execution binding. Pending or failed publication exposes
`execution: null`, which blocks sends even if other runtime state is ready.
The wire carries only the ordinary routing variant ID. Registry keys, credentials
and tokens used for authentication never appear in state or configuration.

A send captures the complete execution triple through the existing OpenCode
model/variant path. Queues, starter captures, first-session sends, journal
recovery and retries retain that exact triple. Config changes publish a new
immutable variant and never rewrite the snapshot of an existing one. The
provider resolves the captured variant's registry entry at execution time and
uses that configuration throughout the run, without consulting the current UI
selection. An unknown or unavailable registry entry rejects the execution.
It never falls back to the latest config, a default model or an unbound run.

Snapshot lookup is not an authorization grant. The plugin still validates the
runtime, provider, directory and session ownership and current OpenCode policy.
Registry snapshots exclude credentials and must preserve unresolved execution
records through reload. A fresh activation does not make an uncertain prompt
safe to replay. Draft-to-session handoff prepares the new session scope and
binds the captured draft configuration before its first prompt is dispatched.
Prompt display metadata has no execution or permission authority.

### Snapshot reconciliation

State is keyed by captured server identity, directory, provider ID and session
ID. Accept a first activation only from a currently outstanding discovery
read/prepare in that scope. Events cannot establish or replace an activation.
Within an accepted activation, greater revisions replace the full snapshot;
equal revisions are no-ops and older revisions are ignored. A delayed response
must still match its request ID, captured server/scope and client generation.
Changing server, directory or session invalidates outstanding generations.
Changing the next-message provider does not delete the previous provider's
pending approvals. They remain answerable under their original ownership.

`permissionOwner: native` routes these requests to the plugin. `opencode`
requires an empty native `requests` list and uses OpenCode's existing approval
path. Neither ownership changes saved OpenCode permission policy. A native
answer must also pass current capability and provider-policy checks inside
the plugin. The protocol never overrides a deny from OpenCode.

Disconnected subscriptions miss events. Reconnect with `getState` or `prepare`
and preserve the last valid snapshot until a complete newer snapshot arrives.
Transport failures and malformed payloads are failures, never empty success.
`submission.status: outcome-unknown` blocks automatic resubmission. A new
activation, model change, cancellation, restored history or reconnect never
clears that uncertainty by itself. The plugin reconciles it with its execution
backend and durable records. Prompting and cancellation use the existing
OpenCode session/provider APIs, not extra native-agent methods.

## Live tool progress

`stateChanged` data is `{protocolVersion:1, type:"stateChanged", state}`.
`toolProgress` data contains the protocol version, type, scope, activation ID,
state revision, `messageID`, `partID`, `callID`, monotonic `sequence` and bounded
`text`. The text replaces that live call's progress display; it is not an
append-only log or a final tool result.

Apply it only within the accepted server/scope/activation, at the currently
accepted state revision, to that exact message/part/call while the authoritative
OpenCode part is running and provider-executed by this provider. Ignore old
sequence numbers. Never create a tool from a progress event, modify another
provider's call, revive a finished call or apply progress to resumed history.
Final OpenCode part content always replaces progress. Progress sequences reset
only for a different activation or tool identity. Missing progress is harmless.

## Validation

`native-agent.test.ts` parses shared JSON fixtures, round-trips method/event
envelopes, rejects malformed and oversized data, and checks contextual config
and answer validation for both providers. Rust and plugin tests should consume
these same examples and cover their own state ownership, policy, persistence,
transport and execution boundaries. SDK tests alone do not prove a live Devin
runtime, network integration, or rendered desktop behavior.

OpenCode references used for the transport boundary:
[plugins](https://opencode.ai/docs/build/plugins) and
[RPC](https://opencode.ai/docs/build/plugins/rpc).
