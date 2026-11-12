# Unit A private SSH cleanup reporting

Unit A passed final frozen-source qualification, including the parent's explicit
not-found parser correction. Parent review and integration remain separate.
The earlier passing candidate and receipt are preserved in `attempt-2/`.

## Scope

Only private cleanup observations and their necessary completion callers changed
in `native-hosts-relay`. Remote stop remains best effort. Existing selection,
UI, keepRunning policy, final 200 ms cap and timeout values remain unchanged.
No dependency, setup fence, strong-session owner, operation-lease framework,
shared-daemon coordinator, accepted-write barrier or terminal preflight was added.

`parity-correction.md` withdraws the stop-on-switch requirement and earlier
product question. It supersedes older scratch advice to veto close permanently
on uncertain remote cleanup. The original failed aggregate and local-master
after-exit failure remain unchanged.

## Changes

- Private reports distinguish remote Stopped, Failed, Absent and Uncertain,
  plus NotRequested when policy gives no stop action.
- The stop command uses the existing CLI `--json` contract. Zero exit with
  warning/stopped:false reports Failed. Malformed, missing or lost replies stay
  Uncertain. Authoritative empty stop results report Absent.
  The parent's reviewed explicit-port `not-found` shape has no PID. A unique
  matching-port stopped:false/not-found result with stoppedCount=0 reports
  Absent before PID validation. Mismatched and duplicate ports, or contradictory
  fields, remain Uncertain. This classification initiates no additional stop.
- Existing CLI serve JSON supplies the acknowledged port and PID. A lost or
  malformed launch response creates no port-based stop authority. Adopted
  managed daemons retain their existing disconnect authority through captured
  CLI identity. Status rechecks suppress destructive stop when identity is
  missing, changed or ambiguous.
- Local reports distinguish reaping the current master from group-signal
  outcomes, including retired master groups. Helpers remain Unconfirmed. No
  report claims that leader reap or a signal request proves all helpers ended.
- Completion now carries an optional report. Existing callers still wait for
  worker completion and do not demand remote success or create a UI veto.

The previous bool was a worker-completion contract. Unit A adds observations;
it does not retroactively treat that bool as a remote-success assertion.

## Evidence

- `before.json` is a real confined SSH wire reproduction before native cleanup
  source changed. It records zero-exit failed-stop JSON, malformed and lost
  responses, and actual fixture running state. It does not claim an old native
  report existed.
- The before wire run's lease receipt is
  `/home/deandre/projects/rust/OpenChamber/native/target/tmp/unit-a-before-nphn_6bq/receipt.json`.
- The first frozen qualification compiled and ran native tests, but its long
  TMPDIR caused existing SSH tests to fail before setup. Its failed receipt is
  `/home/deandre/projects/rust/OpenChamber/native/target/tmp/unit-a-qualification-0fzslye4/receipt.json`.
  `attempt-1/` preserves that source manifest, stage result and scoped patch.
- `tmpdir-socket-diagnostic.json` independently reproduces AF_UNIX path failure
  at 138 bytes and successful private binding at 67 bytes. The retry uses a
  fresh short mode-0700 compiler/test TMPDIR.
- The passing pre-review receipt is
  `/home/deandre/projects/rust/OpenChamber/native/target/tmp/unit-a-qualification-short-tmp-xz9udont/receipt.json`.
  It passed 14 SSH-focused native tests, 6 fixture contracts, scoped all-target
  Clippy with warnings denied, and a debug build. It does not qualify the later
  not-found correction.
- `not-found-change.json` records that only the reviewed parser/test file
  changed after that passing candidate. The prior source manifest, complete
  patch, candidate and receipt link are retained under `attempt-2/`.
- The final receipt is
  `/home/deandre/projects/rust/OpenChamber/native/target/tmp/unit-a-not-found-5xtrkk05/receipt.json`.
  It passed all 15 SSH-focused native tests, scoped all-target Clippy with
  warnings denied, and a debug build. The unchanged fixture source's 6 contract
  tests passed in the preserved earlier run. The final affected-set retry did
  not rerun unrelated fixture tests.
- `final-result.json` records final input, patch and candidate identities,
  unchanged dependency inputs, preserved previous candidate, original-artifact
  checks and the unchanged 17 GiB safety threshold. Minimum sampled free space
  stayed above 50 billion bytes; no capacity-stop signal was sent.
- Copies of before, first failed, pre-review passing and final passing lease
  receipts are included as `*-receipt.json`. The final candidate is `candidate`;
  the prior candidate remains `attempt-2/candidate`.
- `frozen-source.json` records qualification input identities. The launcher
  checks them before and after each stage, uses offline/locked commands and
  explicit dev-profile cleans of the local 3 crates and GPUI base/component/pre.
  The final read-only check also verified the 1,027 input identities and scoped
  rustfmt checks. No tracked source changed during qualification or delivery.
- `unit-a.patch` exports scoped changes against captured preimages, including
  the one main.rs preimage verified against the immutable original source hash.
- `original-artifacts.json` verifies the original candidate, original source
  snapshot and original handoff remain unchanged.

## Integration boundary

Apply only the scoped patch hunks to the parent's combined source. Do not copy
whole worktree files over current main. In particular, retain the parent's
Settings/Knowledge lifecycle, Files ownership, menu/a11y-label and distribution
changes. The Workspace hunk changes only its private cleanup-receipt type and
completion predicate; it adds no close guard or stop-on-selection behavior.

The original application-exit local-master failure is not repaired or requalified
by Unit A. Setup fence, app-owned strong sessions, the small shared-daemon owner,
accepted-task/Files barriers, and terminal preflight remain later units. Preserve
the reference's background selection behavior and best-effort remote stop.

## Limits

CLI status and port-based stop are separate operations. The existing protocol
does not provide atomic conditional stop or proof against same-PID reuse.
Unit A suppresses observed missing or changed identity and grants no authority
from an unknown launch response; it introduces no remote protocol.

The fixture has real OpenSSH and loopback transport with a scripted CLI/HTTP
daemon. Changed identity tests substitute registry identity, not a production
process replacement. Real CLI lifecycle behavior is bounded by the pinned
reference contract, not a deployed production stop run.

Master/group observations do not certify every command or askpass helper.
Best-effort Drop paths remain best effort. Strong process/session ownership and
setup cancellation receipts are later units. No real-window close, held receipt
beyond 200 ms, or aggregate-deadline proof is included in Unit A.

The user's one aggregate deadline and held-quit proof remain required for the
later terminal-preflight unit. No 8-second default, new copy or dependency was
chosen. Parent owns review and integration; no independent review agent, Git,
install or main-source edit was used. Knowledge Base capture is deferred under
this worktree-only scope.
