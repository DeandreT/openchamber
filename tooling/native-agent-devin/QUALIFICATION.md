# Devin execution qualification

## Entry conditions

The user selected native Devin behind OpenCode. This decision enables the
implementation and does not authorize a real run. Obtain separate permission
before sign-in, using real provider credentials, submitting local or cloud work,
or incurring spend. Keep tests credential-free until that permission exists.

An independent reviewer must inspect the final plugin changes before runtime
qualification. Record the reviewed SHA-256 hashes and focused test results.
The user accepted instructions/history as labeled ordinary ACP content with
weaker priority. The pinned SDK cannot import privileged system messages or
foreign history with their original authority.
Deploy the compatible SDK, plugin and native host together. Older strict clients
reject the optional preparation capability when it is advertised.
Session-first clients must use `configComplete: true` for live-catalog authority.
Drafts and missing/malformed catalogs must keep false and null execution.
Explicit SDK `[]` is complete; invalid saved choices expose the validated live
catalog with true while requiring correction. Never send a discovery prompt to
obtain catalog defaults. A cloud session that never reports options stays blocked.
Separately authorized live evidence must establish whether an ID persisted before
the first cloud prompt can resume after reconnect; the pinned SDK and official
cloud guide do not establish that behavior.

Use OpenCode 2.2.7 and the pinned Cognition SDK 0.0.1-beta.9. Run in a separate
OpenCode instance with isolated HOME, USERPROFILE, APPDATA and XDG directories,
free loopback ports, a disposable project and an explicit executable path.
Use a separately approved credential injection method. Preserve the user's
existing login, credential files, settings and running services.

## Credential-free proof

Run the package's 5 test files serially. The production suite exercises registered
plugin hooks and includes an official SDK/in-memory ACP 2 round trip. The Windows
suite uses a fake CLI and checks SDK-owned cancellation/termination, buffered
final responses, slow drains, missing EOF and sampled window visibility.

Run `smoke.mjs` with the pinned OpenCode executable after independent review.
It uses no credentials or prompts. Confirm plugin discovery, native RPC
envelopes, configuration-only draft state, retained draft configuration and no
draft variants through plugin reload. A null binding here reflects deferred
preparation. This smoke cannot prove a successful real turn.
Retain its final `childExit` acknowledgement, including the exact owned PID,
exit code and signal. A failed or missing cleanup acknowledgement invalidates
smoke success. The fake-child tests do not replace this real-child check.

## Authorized real runtime checks

1. Connect the isolated app to the isolated OpenCode instance. Confirm read-only
   discovery and draft preparation do not sign in, allocate SDK sessions or
   publish variants. First Send must create the intended real OpenCode session
   with captured agent/route before provider authorization and SDK preparation.
   Prepare local and cloud targets only after their credential use is approved.
   Record the actual CLI
   version, returned model/mode choices and selected runtime.
2. Verify provider-use denial prevents real-session preparation and a captured queued
   submission. Repeat with enterprise mode enabled: sign-in must be refused,
   while an already configured provider follows its provider-use policy.
   Authentication and invalid configuration must retain the first message for
   explicit correction/retry. Verify original message identity and durably
   captured corrections; unknown outcomes must never auto-resubmit.
3. Select model A, capture a queued prompt, switch to B and release the queued
   prompt. Confirm the submitted native model remains A. Reload the plugin and
   verify the same token still resolves to A. Unknown tokens must fail.
4. Submit an explicitly approved disposable task. Confirm final text, corrected
   text, reasoning and original-ID provider-executed tool results in the rendered
   transcript. Confirm progress attaches only after OpenCode persists the call.
5. Trigger an approval Devin actually exposes. Inspect the exact offered one-shot
   IDs. Deny once, then test an allow with a policy change while approval waits.
   Confirm policy denial wins. Record that reads and other internally permitted
   Devin actions are outside this callback guarantee.
6. Stop a running disposable task. Confirm pending approval rejection and SDK
   cancellation. Hold or delay completion where the test environment permits;
   submission must remain pending until authoritative settlement. A late success
   may settle successfully. A lost connection must retain outcome-unknown.
7. Interrupt transport and restart the plugin while work is pending. Confirm the
   persisted runtime and session are loaded, the old variant remains immutable,
   and history replay does not clear uncertainty or resubmit the task. Inspect
   the remote session separately before taking any recovery action.
8. Exercise revoked credentials, failed sign-in, service unavailability and a
   delayed answer. Confirm the UI shows the actual failure or pending state and
   preserves unrelated sessions. Do not publish raw failures or credentials.
9. On native Windows, launch the installed CLI through the app and sample its
   descendants for visible windows. Also observe the screen during startup,
   cancellation and exit. Fake-CLI samples do not prove a real CLI cannot flash.
10. Verify the accepted ordinary-context mapping with initial
    system/agent instructions, dynamic instructions, same-provider continuation,
    provider-switch history, attachments and compaction checkpoints. Include
    repeated requests and restart. Inspect the actual received context and prove
    nothing is silently omitted or duplicated. Qualify plan/PR presentation only
    after its host mapping exists; observation events must not invent tool calls.

## Retention and cleanup

Retain sanitized observations, selected outputs, reviewed hashes and remaining
limitations in the existing shared native-port investigation folder. Never retain
credentials or full private transcripts. Stop only the test instance and its
owned processes. Remove disposable fixtures and isolated staging after reviewed
evidence is published. Preserve any input still needed by an active check.
