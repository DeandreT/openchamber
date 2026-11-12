# Parity triage: native 544fef273 vs packaged Electron 2.6.1

Run log: `native/target/tmp/parity-all-544fef273-ref261-0py17c_k/command.log`
Report: `native/target/tmp/parity-all-544fef273-ref261-report/` (`R/` below)
Reference: `native/target/tmp/electron-upstream-reference-4x46mdoq/` (app 2.6.1, OpenCode 2.2.5)

Read-only triage. Nothing was built or run. Ordered by payoff.

## 1. Seed project is empty (fixture) - unblocks 4, needed by a 5th

Scenarios: composer.appearance, composer.appearance.narrow-editor,
composer.appearance.project-draft-geometry, composer.attachments.mention-send.

- `tmp/parity/project` holds only `.git` (directory mtime Oct 30; seed built Oct 22).
  `perf-read/{alpha,beta,gamma,delta}.ts` are gone.
- Evidence: `R/composer.appearance/electron/wait-failed.png` shows `@alpha` with
  "No matches found"; `R/composer.appearance.narrow-editor/electron/wait-failed.png`
  shows the Files tree with `.git` only. Both clients fail the same wait.
- The files are written only by `ensureProject` in `scripts/perf/isolated-server.mjs:299`,
  which runs when the isolated server starts on that root (`bun run parity seed`).
  `runner.py:710` then points both clients at `root/project` and never recreates it.
- Fix: `bun run parity seed --force`, or have the runner call `ensureProject` on
  `root/project` before each non-fixture scenario so a cleaned project cannot block a run.
- Not confirmed: what removed the files on Oct 30.

## 2. Native sidebar, rail and title bar differ from 2.6.1 (native) - in all 9 pixel failures

Scenarios: shell.startup, sessions.select-pointer, sessions.works-by-time,
composer.project-choice, transcript.prompt-navigation, workflow.requests.appearance,
workflow.branch.appearance, files.navigation-editor, files.issues-pull-requests.
shell.startup, sessions.works-by-time and composer.project-choice show nothing else.

The reference UI source matches the repo's `packages/ui` for the sidebar (the 50 differing
files are composer submit, input history, queue auto-send and i18n), so this is native drift.

Measured (report geometry, `R/shell.startup/{electron,native}/startup.png`):

| Region | Electron 2.6.1 | Native |
|---|---|---|
| Row width | 256 | 262 |
| Chats header | 44 high | 38 high |
| In work header | y132, 28 high | y126, 30 high |
| Session rows | 32 high (48 for the last) | 34 high (50) |
| "Projects" caption | row at y304, 28 high | missing, a divider line instead |
| Project header | y332 | y308 |
| Headers | muted regular text, no icon | bold with icon |
| Sidebar toolbar | search, add project, display mode (x73), "..." | 4 extra icons on the left (source board at x13, schedules, 2 more), display mode at x246, no "..." |
| Footer | settings only | settings plus 3 icons |
| Right rail | grouped with separators, configure at y371, globe before book | even 36px steps, configure at y344, book before globe, first icon a plain circle |
| Title bar in a session | context ring with "0.7%" and a folder icon | both missing |
| New session label | 112.7 wide | 114 |

- Row model: `caption:projects` comes from `sessionSidebarRowModel.ts:1125-1140`
  (`projectsCaption: !view.hideDirectoryControls`). Native never emits it.
- Likely files: `native/crates/desktop/src/sidebar_view.rs`, the sidebar row model in
  `native/crates/core/src/session_index.rs`, the rail and title bar in
  `workspace_view.rs` / `workspace.rs`.
- Smallest first step: emit the Projects caption row and use the reference header and row
  heights (44/28/32, width 256). That clears the `rows` difference and most geometry in
  every scenario. Toolbar overflow, footer, rail grouping and title bar follow.
- Not confirmed: whether the extra toolbar and footer icons are a deliberate native
  addition. If they are, they need an entry in `scripts/parity/exceptions.json`.

## 3. Transcript and context panel differences (native)

Scenarios: workflow.requests.appearance, workflow.branch.appearance,
transcript.prompt-navigation, sessions.select-pointer.

From `R/workflow.requests.appearance/*/permission.png` and
`R/workflow.branch.appearance/*/turn.png`:

- Markdown table: reference has a bold header and padded rows (about 44 to 56 high,
  cells wrap); native has a regular header and 36px rows.
- Turn footer: native appends a timestamp ("Oct 22, 1:36 PM"); reference does not.
  Native also shows copy/comment/more buttons on the last footer in prompt-list.
- User bubble: reference about 196x49, native 178x39.
- Context panel Project row: native adds an icon and chevron and is 8px taller, so
  everything below shifts (work-status card 424 vs 432 high).
- Turn stats in the permission scenario: reference expanded with "Previous turn",
  native collapsed.
- Permission card: reference shows shortcut hints in Deny ("Alt + ...") and Allow once
  ("Alt + Enter"); native shows none. Sidebar badge is an orange shield in the reference
  and a blue circle in native.
- Prompt list popup: 314 wide vs 320. Staged composer: 652/132 vs 647/137.
- Likely files: `turn_stats_section.rs`, `context_panel.rs`, the markdown table and
  message footer views, the permission card view. Not traced to exact lines.

## 4. Files panel and Issues/PR board layouts differ (native)

- files.navigation-editor (57 to 62% of pixels): reference opens Files as a narrow tab
  panel on the right (search field 210x32 at x1012,y129), keeps the Session card, and
  hides the left sidebar (row labels are empty in the reference). Native keeps the sidebar
  and opens a wide split panel (search 378x28 at x616,y54), which squeezes the composer
  and truncates the hero text. Hero text also differs ("Bug or idea? Either works in
  fixture-repo." vs "What are we working on in fixture-repo"). The reference shows an
  "OpenCode update 2.2.7 available" toast, which needs suppressing in the fixture.
- files.issues-pull-requests (18 to 59%): reference has icon-only tabs, a second repo
  selector, a "#3" picker chip, search 299x28, a "Squash and merge" / "Attach to session"
  footer and a GitHub rail icon. Native has text tabs, a two-column list and detail, search
  405x20. In the reference pull-request capture the "#3" picker dropdown is open over the
  detail, so that checkpoint may be a scenario step that now opens a menu in 2.6.1.
  Not confirmed.
- Files: the native files panel and source board views under `native/crates/desktop/src`.
  These are redesigns, the largest items here.

## 5. files.diff-git: reference wait no longer matches 2.6.1 (scenario)

- Wait `document.body.innerText.includes('src/app.txt')` fails although the row is on
  screen (`R/files.diff-git/electron/wait-failed.png`).
- `ChangedPathLabel.tsx` renders the path as a flex container with `src` and `/app.txt`
  as separate flex items, so `innerText` has a line break between them.
- Fix in `scripts/parity/scenarios.json` (both waits of files.diff-git): use
  `!!document.querySelector('[title="src/app.txt"]')`.
- Expect pixel failures after that: the reference shows a "Couldn't read this repository"
  banner and the update toast, native shows neither, and the Git panel layouts differ
  (`R/files.diff-git/native/git.png`). Cause of the banner not confirmed.

## 6. composer.attachments.drop-send: native sends no citation (native)

- Reference user message: text `[second.txt] with attachment`, file source `inline`.
- Native user message: text ` with attachment`, file source `uri`
  (`file:///tmp/opencode/openchamber/.../second.txt`), name `second.txt`.
- Read from `session_message` in `R/composer.attachments.drop-send/{electron,native}/state/data/opencode/opencode.db`.
- Native does not insert the `[name]` citation into the draft when a file is dropped.
  Likely `native/crates/desktop/src/composer_attachment_input.rs` (drop intake).
  The reference insertion point in `packages/ui` was not located.

## 7. workflow.branch.revert-failed: native rolls the composer back (native)

- After the forced 500 on revert the scenario expects the composer to hold
  "Revert target prompt". Native shows "Draft before the failed revert"
  (`R/workflow.branch.revert-failed/native/wait-failed.png`).
- `session_branching.rs:624-628` calls `rollback_revert` on `Err`, restoring the earlier draft.
- Fix: drop that rollback call so the reverted prompt stays, as the reference does.

## 8. composer.editing.streaming: IBus start failed (flaky harness)

- `R/composer.editing.streaming/native/ibus/ibus.log`: `g_dbus_server_new_sync() ...
  Error binding to address (GUnixSocketAddress): Address already in use`.
- composer.editing.ime-commit passed just before with the same fixture.
- A previous ibus-daemon still held the socket. Fix in `scripts/parity/ime_fixture.py`:
  wait for the daemon to exit in `close`, or retry the start once. Rerun to confirm.
