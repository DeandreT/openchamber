# Credential-free validation

## Approved native execution wiring

The user selected native Devin behind OpenCode on 2026-11-13. Production now
publishes immutable execution bindings. OpenCode gates provider use and checks
approvals exposed through ACP; Devin owns local and cloud tool execution.
Earlier fixed-gate results below describe the code before that decision.

Individual focused Windows checks passed serially with Bun 1.4.4. These are
separate runs with the reuse limits described below:

- `production.test.ts`: 70 tests and 372 assertions.
- `runtime.test.ts`: 28 tests and 121 assertions.
- `local-process.test.ts`: 8 tests and 22 assertions.
- `context.test.ts`: 5 tests and 43 assertions.
- `smoke-child.test.ts`: 5 tests and 12 assertions.

The latest cached-route correction has actual captured output for the production,
runtime and smoke-child files. The unchanged context and Windows process results
are reused from earlier separate runs, not rerun merely to produce logs or counts.
These results are not one combined suite or full runtime qualification.

The separate catalog-authority correction has captured before/after output.
Before strict validation, 16 selected cases had 3 passes, 13 failures and 52
assertions. After correction all 16 pass with 100 assertions. Full production
and runtime files pass separately with the counts above. These tests exercise
the real plugin/runtime with fixture SDK transport, without provider access.
The official SDK in-memory successful session now explicitly reports `[]`.
Missing/malformed catalogs preserve state/journal choices and publish no variant;
valid changed catalogs expose correction choices with `configComplete: true`.

Cloud never-reported catalogs remain blocked. Cross-connection recovery of cloud
IDs saved before the first prompt remains unqualified. The pinned SDK's lazy
creation and resume forwarding plus the official cloud guide do not prove that
server behavior. No live check was authorized or performed.

The separate review-findings correction has actual before/after output. The
corrected 9-case reproduction had 2 passes, 7 failures and 19 assertions before
source edits. An earlier test attempt had missing activation IDs; its output is
retained separately and is not the before proof. The final expanded 14-case
regression selection passes with 73 assertions. Final production and runtime
files pass separately with the counts above. Static Node checks pass for both
smoke modules; the smoke itself remains held.

Regressions cover stale open-handle catalogs at prepare/configuration/execution,
catalog updates from setters and completed streams, correction after model
rejection, and queued A versus saved B after recovery. The pinned official SDK
converts an in-memory JSON-RPC InvalidParams model rejection into its public
`ModelRejectedError`; that production path reports `invalid_config` without a
prompt or pending submission. A rejected user context can then submit after
correction. Special mutation IDs `__proto__`, `constructor` and `toString` retain
own JSON data records, replay idempotently within an activation and reject
conflicting parameters after restart. Old activations remain stale.
Catalog loss detected while publishing admission also clears the unstarted
pending record without invoking streaming. Actual streamed uncertainty still
retains the existing no-replay block.

The subsequent refreshed-revision reproduction failed before the guard and
passes after it with 9 assertions. A subset captured at the old revision cannot
persist newly discovered defaults over saved intent. Refresh exposes live B while
saved A remains durable; explicit B correction at the refreshed revision succeeds
without a prompt. An unchanged invalid catalog keeps its revision so correction
can be accepted. Exact recorded-result retries and parameter-conflict rejection
remain covered. The production and runtime counts above are separate final runs.

Before the route correction, 5 selected production cases had 2 passes and 3
failures. Both `getState -> route change -> prepare` directions allocated the
cached runtime, and conflicting saved intent also allocated instead of rejecting.
After correction the full production file passes. Regression cases also preserve
allocated-mode recovery and immutable queued bindings, reject conflicting saved
intent before SDK access, and refresh a changed sign-in target before another click.

The smoke now observes its exact retained child immediately after spawn, waits
for exit acknowledgement within a 5-second cleanup bound, and prints success
only after cleanup. Startup failures, refused stop and missing exit acknowledgement
propagate. A fake-child unit file covers those outcomes without starting OpenCode.
`node --check smoke.mjs` and `node --check smoke-child.mjs` pass. The actual smoke
remains held for independent review; its cleanup has not been qualified against
a real OpenCode child for this revision.

Production tests run the actual plugin setup, registered AI SDK and session
hooks, immutable registry and runtime. Transport fixtures cover local and cloud
success, rejected cloud credentials, provider denial, enterprise sign-in refusal,
captured configuration, original approval IDs, action-policy rechecks,
cancellation, late completion, unload and restart without replay. One case uses
the official Cognition SDK against an in-memory ACP 2 agent through the full
production plugin/model path. No test authenticates or starts a real Devin run.

The new path reproduced a text-stream stall: buffering ACP text returned an
empty stream pull before consuming the completion. Pulls now consume events
until they enqueue a part or finish. Tests also exposed policy denials reported
as unavailable and allocated cloud sessions missing their persisted runtime mode.
Both are corrected and covered.

The Windows hidden-process and inactivity-EOF implementation is unchanged. Its
8 focused tests still pass, including official SDK use against a fake local CLI
under Node and Bun. No broad checks or credentialed qualification ran.

The user separately accepted labeled ordinary-context transfer with weaker
priority. Production context tests cover initial instruction-source/role transfer,
foreign history, hostile delimiter content, incremental prefixes, instruction
updates/removal, immutable capture, replay rejection across restart, and incoming
compacted-history replacement. Encoding tests preserve special values explicitly
and reject unsupported or malformed inputs, including transient media download
credentials. Uncertain transfers preserve the preceding checkpoint and block
restart replay; corrupt checkpoints cannot become empty successful recovery.
Checkpoints retain hashes only.
The final AI SDK view is captured separately to preserve later-hook edits;
tests verify its immutability and independent incremental checkpoint. A targeted
credential-free check also captured actual pinned OpenCode `UserMessage` and
`Media.Asset` instances with inline bytes through the serializer.
These checks do not prove model obedience, privileged instruction semantics or
erasure of Devin's native memory. Native compaction, plans and PR presentation
remain unqualified. Live context transfer still requires authorized qualification.

Review regressions cover configuration-only draft preparation under provider
denial, no draft network/authentication/variant effects, authoritative first-session
cloud routing despite a local plugin default, and subset merging of captured
configuration with newly discovered defaults. SDK-normalized approval cases cover
outside-project `file_path`, local relative paths, cloud command metadata,
multiple files/locations, policy changes before allow, unknown details, unresolved
relative VM paths and partially normalized file metadata. U1/U2/U1 is rejected
both before and after restart through the production hooks. The consumed-user
ledger survives replacement independently of the latest context checkpoint.
Legacy context-only journals without the full ledger report unavailable without
opening or replaying a session.
The runtime draft guard is also tested with a permissive fixture backend, so
configuration-only preparation does not depend on a backend remembering to deny.
`node --check smoke.mjs` passes without starting the smoke runtime.

The plugin advertises the coordinated optional `prepareAfterSession` flag using
the updated shared SDK type/schema. The shared SDK/native creation owner handles
the host changes and retained-message retry flow. Combined native workflow
qualification is still required. The updated credential-free smoke script now
checks draft intent retention and absence of draft variants; it has not been run
for this revision. Earlier smoke results below are historical evidence.

Recorded on 2026-11-13 on Windows. The same isolated smoke passed with
executables reporting `opencode v2.2.6` and `opencode v2.2.7`. The plugin's API
dependencies and qualification runtime are pinned to 2.2.7. The earlier
2.2.6 run is additional compatibility evidence.

## Focused checks

From `tooling/native-agent-devin`:

```powershell
& 'C:/Users/deandre.thomas/AppData/Local/ITS/Tooling/bun-1.4.4/bun-windows-x64/bun.exe' test ./runtime.test.ts
node ../../node_modules/typescript/bin/tsc --ignoreConfig --noEmit --skipLibCheck --module nodenext --target es2023 --allowImportingTsExtensions --strict --types node server.ts runtime.ts rpc.ts bindings.ts model.ts prompt.ts errors.ts
node smoke.mjs 'C:/Users/deandre.thomas/AppData/Local/ITS/Tooling/opencode-2.2.7/node_modules/@opencode/cli-windows-x64/bin/opencode.exe'
```

The final review-3 focused run passed 27 tests and 118 assertions. The tests cover
fixture compatibility, idempotence, stale ownership/revisions,
captured configuration, permission choice identity and denial, cancellation,
uncertain recovery, unknown variants, catalog publication failure and progress
provenance. Two tests use the actual Cognition SDK against an in-memory ACP
agent to prove negotiation and streamed-turn completion for protocol 1 and 2.
The source-only TypeScript command passes. No broad repository
build, lint run or credentialed turn was used as proof.

## Live smoke result

The isolated runtime reported:

```json
{
  "runtimeVersion": "opencode v2.2.7",
  "plugin": "active",
  "protocolVersion": 1,
  "initialAuthentication": "required",
  "prepareError": "authentication_required",
  "execution": null,
  "immutableVariantPublished": true,
  "variantRetainedAfterReload": true,
  "sessions": 0,
  "prompts": 0,
  "credentials": 0
}
```

The exact HTTP RPC envelope is `POST /api/rpc/{rpcID}/{method}` with
`{"input": <parameters>}` and a successful transport response of
`{"output": <NativeAgentResult>}`. Sending parameters without the `input`
wrapper returns `rpc.invalid_input`. Authentication-required is a domain result
inside a successful HTTP transport response.

The smoke process receives fresh HOME, USERPROFILE, APPDATA, LOCALAPPDATA,
XDG_CONFIG_HOME, XDG_DATA_HOME, XDG_CACHE_HOME and XDG_STATE_HOME directories.
Only PATH, SystemRoot, WINDIR, COMSPEC, PATHEXT, TEMP and TMP are inherited.
Its OpenCode password is generated in memory and never printed or retained in
the result. Devin credentials are absent. Child output is discarded. The
script stops its own OpenCode process in `finally`.

The exact pinned CLI was provisioned outside the repository and OneDrive with
`npm install --prefix <LOCALAPPDATA>/ITS/Tooling/opencode-2.2.7 --ignore-scripts --omit=peer @opencode/cli@2.2.7`.
The native executable in `@opencode/cli-windows-x64/bin` reported 2.2.7.
No project manifest, project lockfile or running shared service was changed.

## Review regressions

Before the review fixes, focused tests reproduced 5 failures covering 4
defects: persisted model selections reset to SDK defaults, a late backend open
committed after disposal, a definite policy error became outcome-unknown, and
the opaque option IDs `allow` and `reject` were reinterpreted by the actual
Cognition SDK. Those regression tests pass after the fixes. The permission
tests inspect real in-memory ACP responses, including the original chosen ID.

A subsequent test reproduced a post-submission disconnect whose failed
recovery write incorrectly escaped as a definite unavailable result. Recovery
now preserves outcome-unknown even when persistence/publication fails, and
the test verifies that a second submission stays blocked. The final run also
checks inline attachment conversion and negotiated byte limits. Those additions
require the final scoped review.

The next review reproduced 2 additional failures before correction: an existing
immutable token kept the model catalog text-only after capability negotiation,
and a valid 12,000,000-byte attachment was rejected by the repeated-group base64
regex. Capability changes now explicitly invalidate the model catalog without
minting another variant. The real plugin setup with a fake SDK verifies the
same-token path, no redundant invalidation for unchanged capabilities, client
cleanup on publication failure and disposal during publication.

Generated base64 is no longer redundantly validated. Supplied strings use a
linear character scan with length and padding checks. The focused regression
round-trips 12,000,000 bytes for both forms under a negotiated 16 MiB limit,
rejects malformed input and rejects data above the negotiated limit. A separate
Node v22.23.4 run with `--experimental-transform-types` passed the same 12 MB
generated/supplied-string round-trip. The execution gate remains fixed closed.

The original 25-test evidence is preserved in the retained `devin-plugin`
folder. Updated results and source hashes belong in the dated
`2026-11-13-review-2` subfolder; differing retained files are not overwritten.

Review 3 reproduced a concurrent preparation race before correction: scope B
returned ready while scope A's same-mode capability publication was pending,
then A failed and rolled back. Capability publication now uses a per-mode
promise chain. Every same-mode caller waits for the publication outcome; a
failed in-flight publication fails its waiting callers and closes their own
clients without automatically retrying. A later explicit preparation can try
again. Differing negotiated capabilities publish in order, and a later failed
publication restores the preceding successful capabilities.

The deterministic real-plugin/fake-SDK checks hold the first reload, start
2 scopes, and verify B cannot return ready with unpublished capabilities.
They verify failure closes both newly prepared clients and preserves a prior
ready handle/catalog. A second case verifies an earlier successful publication
survives a later differing-capability failure. Review-3 results and source hashes
are retained separately in `2026-11-13-review-3`. The review-2 packet is preserved.

## Windows hidden-process qualification

The 2026-11-13 scoped run passed 32 tests and 132 assertions across
`runtime.test.ts` and `local-process.test.ts` with Bun 1.4.4 on Windows.
Source-only TypeScript validation also passes with `local-process.ts` and
`local-process-node-smoke.ts` included in the command above.

The process fixture is a locally compiled C# console executable. It implements
only fixture ACP responses, never signs in, sends network traffic or runs tools.
The direct hidden launch passes its owned child and startup-failure promise to
the SDK's public `LocalDevin.connect`. SDK cancellation settles the fixture turn;
SDK close terminates the child; missing executables and pre-handshake exits
reject; turn-time exit rejects an in-flight turn; repeated close is safe.

The fake CLI calls `GetConsoleWindow` from its own process and reports zero.
A live CIM snapshot confirms its direct parent is the test process and its only
descendant is Bun's `conhost.exe`. `EnumWindows` with `IsWindowVisible` finds no
visible top-level windows for either process. The Node 22.23.4 check also reports
no attached console through the wrapper. The unmodified pinned SDK helper has
no attached console in both checked environments, so a pre-change console flash
was not reproduced. The missing `windowsHide` flag was confirmed in pinned SDK
source. These are sampled visibility checks, not a continuous screen recording.

Before the initial child-exit close was added, the Bun fake-CLI turn-exit check
timed out while its SDK stream awaited pipe EOF. The wrapper now closes the
public SDK client after a bounded drain wait and that same test passes.

Selected output, qualification notes and source hashes are retained under
`evidence/devin-windows` in the existing shared native port investigation.
Earlier packets remain unchanged. Fixture binaries and compilation staging are
local disposable output and are removed after the tests.

Real installed Devin CLI children, real authentication, billable turns and
packaged desktop launch remain unqualified. The production execution gate stays
false and native execution bindings stay null.

### Buffered final-response correction

The correction run passes 34 tests and 134 assertions with Bun 1.4.4 on Windows.
The same scoped source TypeScript check passes. A new fake CLI mode writes the
final ACP `end_turn` result, flushes stdout and immediately exits. With SDK
stdout paused until process exit, the original immediate-close wrapper rejected
that valid response as `ConnectionClosedError`. That failing result is retained
separately from the earlier proof packet.

The corrected wrapper pipes stdout through a buffered public stream. It delivers EOF
one event-loop turn after the last data event so the SDK's response promise
continuations finish first. Process exit starts a 1-second inactivity fallback to
SDK close; received EOF clears that timer. The SDK still receives the actual child and
startup-failure promise and owns cancellation and termination.

Node 22.23.4 passes both ordinary immediate-final-exit and paused-until-exit
buffered-final-response checks, plus no-response exit rejection. Bun passes the
buffered-final-response case with 61 bytes waiting after process exit and the
ordinary no-response exit case. A separate Bun case leaves SDK stdout paused
after a no-response exit, suppressing EOF delivery, and verifies bounded
rejection through the fallback. Existing cancellation, owned-child cleanup and
sampled no-visible-window checks still pass.

The original correction used an absolute 1-second deadline. The inactivity
correction below replaces that deadline. These checks use a local fake
ACP executable, not an installed or authenticated Devin CLI. Real CLI children,
packaged desktop execution and continuous flash detection remain unqualified.
The execution gate and null bindings are unchanged. The separate correction
packet includes before/after output and hashes; the original packet is preserved.

### Inactivity-based drain correction

The next focused run passes 35 tests and 140 assertions on Windows with Bun
1.4.4. Scoped source TypeScript validation passes. Before the fix, a new test
read a buffered final response 10 bytes at a time across the absolute post-exit
deadline. The actual SDK turn rejected with `ConnectionClosedError` even while
output was progressing. The retained before-result records that failure.

After exit, each SDK-facing stdout data event refreshes a 1-second inactivity
timer. The fallback also checks both stream buffers before closing, preserving
pending bytes during consumer pauses or event-loop stalls. EOF completion,
stream close and explicit SDK close remove the progress/end/close/exit listeners
and cancel the timer. SDK close still invokes the original SDK method and
destroys the owned stdout adapter. The timer remains referenced so Node can
settle a pending SDK turn even when a missing EOF leaves no other active handle.

Node 22.23.4 and Bun 1.4.4 both preserve the actual SDK `end_turn` result after
an initial 1,250-millisecond buffered pause followed by 10-byte reads spaced
250 milliseconds apart. The recorded drain completes after 3,066 milliseconds
on Node and 3,007 milliseconds on Bun. The fake final response is 61 bytes.
Both runtimes still reject a no-response exit and the paused-empty missing-EOF
case. Node's missing-EOF result rejects after 1,011 milliseconds. Tests also
verify no stdout data listeners remain after explicit client close. Existing
cancellation, direct-child ownership and sampled no-visible-window checks pass.

The new packet retains before/after output, scoped type-check status and all
11 source hashes separately. Previous packets remain unchanged. Real installed
Devin CLI children, credentialed or billable work, packaged desktop launch and
continuous flash detection remain unqualified. Buffered consumers must drain
or close explicitly. The production SDK consumes stdout directly. Execution
remains false and bindings remain null.

## Limits of the earlier checks

This proves local loading, discovery, validation, state handling and catalog
publication. It does not prove a credentialed Devin connection, real tool
execution, remote cancellation, automatic recovery or rendered native desktop
behavior. Production execution remains disabled pending the authority decision.

The plugin-local npm install reported 24 moderate package entries propagated
from 2 advisories. [DEPENDENCY_AUDIT.md](./DEPENDENCY_AUDIT.md) records their
impact and the inspected call paths. Dependencies were not upgraded or replaced.

## Verified implementation references

- [OpenCode plugin server API](https://opencode.ai/docs/build/plugins/server)
- [OpenCode RPC contract](https://opencode.ai/docs/build/plugins/rpc)
- [Pinned Claude Agent hook](https://github.com/anomalyco/opencode/blob/v2.2.7/packages/core/src/plugin/provider/claude-agent/aisdk.ts)
- [Pinned variant resolution](https://github.com/anomalyco/opencode/blob/v2.2.7/packages/core/src/model-resolver.ts)
- [Pinned provider-executed tool conversion](https://github.com/anomalyco/opencode/blob/v2.2.7/packages/core/src/aisdk.ts)
- [Pinned authoritative tool events](https://github.com/anomalyco/opencode/blob/v2.2.7/packages/core/src/session/runner/publish-llm-event.ts)
- [Cognition local sessions](https://docs.devin.ai/sdk/guides/local-sessions.md)
- [Cognition cloud sessions](https://docs.devin.ai/sdk/guides/cloud-sessions.md)
- [Devin CLI authentication and ACP](https://docs.devin.ai/cli/reference/commands.md)
