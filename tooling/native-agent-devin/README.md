# Devin native-agent plugin

This repository-local OpenCode 2 plugin supplies the native-agent v1 contract
and a Devin ACP execution adapter. It uses `@opencode/plugin@2.2.7` and
`@cognition-ai/sdk@0.0.1-beta.9`.

OpenCode owns the app session and gates access to Devin. Devin executes its own
local and cloud tools. Prepared state exposes an immutable execution binding;
OpenCode checks provider use and approvals Devin exposes through ACP. Instructions
and history transfer as labeled ordinary ACP context with the weaker priority
the user accepted. Credentialed execution and rendered workflows still require
qualification.

This revision advertises `capabilities.prepareAfterSession: true`. It requires
the updated native-agent SDK and host workflow. Older strict clients reject
the new field, so deploy the compatible plugin, SDK and host together.

## Load from this checkout

Install this folder's dependencies separately:

```sh
npm install --ignore-scripts --workspaces=false --omit=peer
```

Add the absolute folder path to the existing OpenCode configuration:

```jsonc
{
  "$schema": "https://opencode.ai/config.json",
  "plugins": [{
    "package": "C:/dev/openchamber/tooling/native-agent-devin",
    "options": { "mode": "cloud" }
  }]
}
```

`mode` is `local` or `cloud`, defaulting to `local`. `cliPath` selects the
installed Devin executable for local mode. `orgID` optionally selects the
Devin cloud organization. Cloud traffic uses `https://api.devin.ai`.

This package remains outside the Bun workspaces. Its private TypeScript
entrypoints import the SDK protocol and enterprise policy from this checkout;
it is not a standalone published npm distribution.

## Credentials and sign-in

Local mode uses the official SDK and the installed CLI's
existing configuration and cached login. The OpenCode integration offers the
official `devin auth login` command. Native `authenticate` starts that engine
command flow and continues to report authentication required while it is
pending. A later preparation observes the runtime's successful connection.

Windows launches the executable directly with `windowsHide: true` and passes
its pipes and child to public `LocalDevin.connect`. Other platforms retain
`LocalDevin.spawn`. SDK close terminates the owned child. Stdout drains before
EOF reaches the SDK. After exit, a 1-second inactivity fallback settles turns
only when the pipe is empty and EOF is missing. Data refreshes the wait and
buffered bytes keep it open.

Cloud mode reads `DEVIN_API_KEY` only from the OpenCode process environment.
Inject it with the operating system's credential manager or an approved secret
wrapper when launching that process. The plugin never accepts a token in RPC,
plugin options or a file and never stores the token in OpenCode's credential
tables. It verifies a supplied token through `/v3/self` before connecting ACP.
Missing or rejected credentials report `authentication_required`.

No sign-in starts during `getState` or `prepare`. Enterprise mode refuses
native `authenticate` at use time. The existing configured-provider policy
continues to govern provider use.

Draft preparation reads saved configuration only. It stays `unprepared` with
`execution: null`; draft authentication is refused and draft configuration
changes publish no variant. On first Send, the host creates the real OpenCode
session with the captured agent and routing model. Only then can preparation
check that session's provider policy and connect to Devin. Live choices load
during that preparation. Subset configuration updates preserve additional live
fields at their authoritative defaults; invalid captured choices require
explicit correction before the retained message can be submitted.
Preparation rechecks the route after a cached read-only state. Implicit defaults
follow a new route; conflicting saved runtime intent requires explicit correction.
Existing allocated sessions and captured queued bindings keep their runtime.

## Execution adapter

The provider ID is `devin`. Its OpenCode routing model IDs are `local` and
`cloud`. Native account model IDs are separate opaque choices returned by ACP.
The plugin registers through the supported `ctx.aisdk.hook('sdk')` API used by
OpenCode's built-in Claude Agent provider. It opens no HTTP listener.

Configuration produces content-addressed immutable variants. The registry is
persisted before `ctx.model.reload()` publishes a variant, and the plugin reads
the model catalog to verify publication. A captured variant resolves the same
configuration after reload. Unknown tokens fail. Retained variants are never
silently reassigned or removed. A ready native state publishes its verified
binding for normal OpenCode execution.

The adapter forwards ACP text, reasoning and provider-executed tool calls and
results into OpenCode's authoritative transcript. Text and reasoning are held
until turn completion so ACP overwrites and aborted messages produce the
correct final text. Tool events stream during the turn. Bounded live progress
is emitted only after OpenCode persists the matching provider-executed call;
the original ACP call ID remains the OpenCode tool ID. Finished calls cannot
be revived by progress.

Only offered one-shot permission choices appear in native requests. IDs remain
unchanged. OpenCode checks a pending action before the native request appears
and checks an allow reply again. A configured denial rejects the ACP request.
The checks use normalized SDK command/file details and every affected location.
Local relative paths resolve against the real local project. Cloud files keep
their absolute VM identity; local project-path rules do not establish a mapping
to those files. Relative VM paths with no established base and approvals with
unknown actions or incomplete resources are refused.
Stopping the OpenCode stream aborts the SDK turn and rejects pending approvals.
An SDK-confirmed cancellation preserves configuration and clears the submission.

The supported OpenCode context hook captures original instructions, their source
metadata and the complete message window before asynchronous dispatch. Escaped
JSON preserves original roles as data. It does not create privileged SDK system
messages. A separately labeled final AI SDK view preserves edits by later hooks.
Both views describe the same conversation. New turns append only a verified history prefix; changed or removed
instructions and rewritten history have explicit replacement semantics. Completed
checkpoints persist hashes only. Repeated consumed user IDs fail before another
submission, including after restart. Unsupported context values fail explicitly.
Consumed IDs remain in a separate hash-only ledger when history is replaced or
compacted. Earlier context-only journals without that complete ledger are
refused because earlier consumed IDs cannot be reconstructed safely.

## Remaining limits

- Devin owns execution of its local and remote tools. ACP permission callbacks
  cover only requests the agent sends. OpenCode's per-tool rules cannot be
  claimed to cover tools Devin permits internally. Selecting `normal` mode
  does not establish that guarantee. Native execution follows Devin's own tool
  authority and approval model.
- Structured native question answers are unsupported because the pinned SDK
  supplies question metadata without a verified structured answer operation.
  The plugin never advertises an answerable structured question. Ordinary
  follow-up text remains the session conversation path.
- Native configuration exposes the model and mode setters the SDK actually
  supports. Independent effort fields and cloud model selection are not
  guessed from labels or implemented with undocumented requests.
- The adapter maps inline attachment bytes through the official ACP builders
  and negotiated capabilities/limits. Large-file upload, plan presentation,
  pull-request actions, auxiliary generation and provider-specific compaction
  remain implementation work. Auxiliary calls cannot start another Devin turn
  accidentally. [CAPABILITIES.md](./CAPABILITIES.md) records the supported
  routes and the remaining semantic decisions.
- Instructions and provider-switch history are ordinary prompt context. Devin
  retains its own conversation and instruction hierarchy. A compacted OpenCode
  window replaces the labeled context window; it cannot erase Devin's existing
  memory or claim native compaction. Historical binary data remains explicitly
  encoded context, while the current turn also uses negotiated ACP media blocks.
- A lost connection preserves an outcome-unknown submission. `prepare` can
  reload the allocated SDK session, but does not resend the prompt or clear
  uncertainty from history alone. The pinned adapter has no proven automatic
  settlement for that case, so further submission remains blocked.
- No credentialed Devin turn or rendered desktop workflow has been tested.

## Validation

```sh
bun test ./runtime.test.ts
bun test ./local-process.test.ts
bun test ./production.test.ts
bun test ./context.test.ts
bun test ./smoke-child.test.ts
node smoke.mjs <absolute-path-to-opencode-executable>
```

The smoke script starts its own loopback OpenCode with fresh HOME, USERPROFILE,
APPDATA and XDG directories. It forwards only executable/Windows essentials,
uses a generated in-memory server password, discards subprocess output, passes
no Devin credentials, and stops only its own process. It verifies empty
sessions, plugin activation, provider capability, RPC input/output envelopes,
configuration-only draft preparation, saved draft configuration across reload,
and absence of draft variants. It sends no prompt. The updated smoke has not
been rerun for this revision.
It waits up to 5 seconds for the exact retained child's exit acknowledgement
before printing its success JSON. The summary includes that child's PID, exit
code and signal. Startup or cleanup failure makes the smoke fail.

See [VALIDATION.md](./VALIDATION.md) for the recorded credential-free run and
[DOCUMENTATION.md](./DOCUMENTATION.md) for ownership and invariants.
