# Desktop UI parity method

The Rust desktop preserves the repaired Electron desktop's appearance and
behavior. Every migrated feature has a repeatable comparison against that
baseline before it is complete. Intentional changes require the user's approval.
Electron remains usable until the replacement qualifies across the full feature
inventory.

Linux is the initial qualification target. Windows remains a separate later
target; a Linux result never establishes Windows support. Run Linux visual
checks on a private display, isolated from the user's desktop and input.

This is the verification design. Implement its runner and capture adapters in
stage 0 of the [implementation plan](IMPLEMENTATION.md). Existing browser tests
and profiling commands supply part of the Electron coverage; they do not drive
a native GPUI window.

## 1. Establish the reference

Pin a release build of the qualified fork that contains the compact-sidebar and
session-visibility repairs. Record the source revision, built assets, Electron
and OpenCode versions and fixture identity in the baseline manifest. Confirm the
running window uses those assets. A current upstream build or the installed app
with a different profile cannot silently substitute for this reference.

Use disposable profiles and synthetic fixtures for each client. For connected
scenarios, reset independent server data copies to the same seed before each
run. A mutation in the Electron run must not alter the native run's starting
state. Both clients receive the same ordered messages, events, delays and injected
failures. Check content and event identities, not just session counts.

Lock these capture conditions:

- Client-area width and height in logical pixels, Windows display scaling, UI
  zoom, text size, font files/fallbacks, theme, locale and layout direction.
- Selected session, panel widths, expanded groups, preferences, scroll anchor,
  focus, selection, cursor position and window foreground state.
- Fixed fixture timestamps, IDs and event schedule. Capture a streaming
  checkpoint after the same identified event has been painted in both clients.
- Accessibility and reduced-motion state, with separate scenarios for their
  relevant variations.

Capture the unchanged reference twice and establish screenshot/geometry
repeatability. Unexplained variation blocks a comparison. Keep raw captures and
every narrowly defined dynamic-region mask. Prefer deterministic fixture values
over masks; never mask missing controls, shifted rows, loading failures or text
under test.

The baseline is versioned. An upstream sync or approved behavior change creates
a reviewed new baseline and reruns affected scenarios. Keep the earlier baseline
and its difference record. A native failure never rewrites expected output.
If the reference itself fails a required scenario, record the defect and resolve
the expected behavior before marking native parity complete.

## 2. Keep an inventory of user-visible behavior

Give each feature a stable identifier and one or more scenario IDs before
porting it. Build the inventory from the live baseline, navigation, shortcuts,
settings, menus and owning documentation. The following groups are the starting
coverage map; expand each into individual controls and workflows during stage 0.

| Group | Required coverage |
| --- | --- |
| Shell and navigation | Window chrome, compact layout, panel visibility/resizing, tabs, focus, shortcuts, multiple windows and Mini Chat |
| Sessions and projects | Chats, Pinned, In work, search, folders, tree/timeline views, expansion, archive, worktrees, drag/reorder and moved-session visibility |
| Transcript | Prose, Markdown, code/diffs, tables, math/diagrams, images, attachments, reasoning, tools, compaction, links, search, selection and copy |
| Transcript motion | Initial reveal, short/mixed/tall rows, prepend, resize, stream growth, tail following, exact within-row restore and offscreen selection |
| Composer | Drafts, undo, multiline input, paste, attachments, mentions, commands, IME, model/agent/variant choice, submit, queue and steer |
| Agent workflow | Create, stream, abort, permissions/forms, subagents, background commands, BTW, fork, revert, move, archive, goals and review loops |
| Files and source control | Trees, editor/diff, search, save/error states, Git actions, branches, identities, issues and pull requests |
| Browser and terminal | Previews, executable HTML answers, embedded browser actions, SDK panels, terminal input, selection, copy/paste and resize |
| Settings and presentation | Every existing settings page, persistence, themes, text size, all locales, RTL, tooltips, menus, focus and disabled/error states |
| Other panels | Knowledge, plans/todos, schedules, usage, integrations, voice and notifications |
| Runtime and host | Authentication, host switching, reconnect, SSH/relay, deep links, tray, menus, permissions, quit/relaunch, update and profile migration |
| Restricted states | Read-only sessions, unavailable capabilities, enterprise policy, disconnected/partial data and permission failures |

A feature record contains its owner, reference implementation, applicable
platforms, initial state, actions, observable outcomes, visual checkpoints,
required configurations and automation/manual coverage. Include loading, empty,
failed, disabled, hover, focused, selected and restored states where applicable.

Keep requirements and synthetic scenarios in the repository. Keep the current
pass/fail ledger in the private run report, keyed by scenario, implementation
revision and configuration. Use distinct results for passed, failed, not built,
not run and blocked. A skipped or unsupported feature cannot count as passed.
Report coverage against the full inventory as well as the feature being ported.

## 3. Run one scenario through two adapters

Use the host's Python runner to orchestrate serial runs. It reads shared
scenario definitions, sets up isolated state and collects evidence. Two adapters
translate semantic actions such as selecting a session, typing a draft, pressing
a shortcut or opening a menu into each runtime's actual controls.

| Adapter | Mechanism | Evidence |
| --- | --- | --- |
| Electron | Existing CDP helpers and real input dispatch; Windows UI Automation for native shell controls | Window screenshots, DOM geometry, accessibility semantics, visible text and server outcomes |
| Rust on Linux | AT-SPI through GPUI/AccessKit, plus native pointer/keyboard input | Window screenshots, accessible roles/names/state, focus, text and server outcomes |
| Rust on Windows | Windows UI Automation through GPUI/AccessKit, plus native pointer/keyboard input | Separate Windows qualification of the same outcomes |
| GPUI component checks | Kit's `test-support` facilities | Focused semantic/state assertions, complementary to the real-window run |

Prove the native automation adapter on a small fixture window before relying on
it. Verify identification of the owned window, capture dimensions, control
discovery, input, clipboard assertions and cleanup. If a control is inaccessible
to automation, retain a manual scenario and an explicit automation gap. Headless
GPUI tests alone cannot qualify a platform integration. A missing required
accessible role or action is a parity defect even when pointer activation works.

Use stable logical targets mapped to each implementation's element IDs or
accessible controls. Fail on missing or ambiguous targets. Do not solve a
missing accessible control by calling its internal action directly. Diagnostic
hooks can expose read-only state and frame markers; user-path tests go through
hit-testing, focus and event handling. Test keyboard and accessibility activation
separately from pointer input.

Wait for observable state transitions with deadlines, followed by a painted
frame. A fixed sleep cannot prove readiness. Check both the visible outcome and
the core/server result when a scenario changes state. A fake successful toast
must not pass a failed mutation. Test clipboard contents as text, with exact
Unicode and line-break expectations for the action, and restore the fixture's
clipboard state when done.

Reuse `scripts/perf/cdp.mjs`, the deterministic fixture provider and the browser
history test's existing cases. Keep screenshot comparison and performance
measurement separate. Add root package scripts for the parity runner when it
exists; do not add a new CI pipeline for this work.

### Prove the checks catch failures

Before using a gate, run controlled fixture defects: shift a control, hide a
button, change a label, drop a text delta, misdirect focus and restore the wrong
scroll offset. Each relevant comparison must fail. Retain those expected failures
separately from real regressions. Requalify a detector when its capture method,
masks or tolerances change.

## 4. Compare appearance, behavior and accessibility

At every checkpoint, save reference, native, overlay and difference images plus
measured geometry. Use side-by-side review to explain every flagged region.
Compare semantic outcomes separately so a matching screenshot cannot hide a
broken action.

| Check | Completion rule |
| --- | --- |
| Content and controls | Same labels, icons, values, ordering, available actions and state; no missing or clipped content |
| Layout | Same panel widths, row heights, spacing, alignment, wrapping, truncation and popup placement at matched client dimensions |
| Geometry and anchors | At most one physical pixel of coordinate rounding after logical-to-physical conversion; no accumulating drift across rows |
| Styling | Canonical colors, alpha, typography, icon sizes, borders, elevation and focus/selection treatment match the reference |
| Input and workflows | Same shortcuts, pointer behavior, focus order, draft/undo/selection state and successful or failed action outcomes |
| Accessibility | Equivalent roles, names, values, selected/expanded/disabled states, keyboard reachability and actions; platform tree structure can differ |
| Lifecycle | Same state after switching, reload, reconnect and close/reopen where persistence applies; partial failure preserves the correct owner |

Chromium and GPUI may rasterize glyph edges differently. Raw pixel equality is
therefore a detector, alongside geometry and semantic assertions. A proposed
tolerance must be confined to verified rasterization noise and reviewed before
it can pass. Text metrics, line breaks, baselines and hit targets still have to
match. Do not blur or resize screenshots, raise a global difference percentage,
or approve the entire screen to conceal a defect. Record approved exceptions by
scenario, region, reason and user decision.

Capture transient states too: first revealed frame, menu opening, selection
change, resize, streaming and error recovery. Sample anchor/focus identity and
geometry through the transition. A settled screenshot cannot establish that the
transcript never jumped or the composer never lost focus.

Narrator announcements, IME composition/candidate placement, cross-message
selection and monitor/DPI changes require real Windows checks. A missing manual
check remains not run. Carry the accessibility rules through virtualization;
rows being unmounted cannot silently make a required action unreachable.

## 5. Configuration coverage

Use a standard fixture configuration of 1,280 by 800 logical client pixels,
100% display scaling, fixed fonts and text size, and the canonical light/dark
theme pair. Also capture 900 by 600 and the reference desktop's verified minimum
supported client size. Derive breakpoint cases from actual baseline behavior.

- Run affected features in light and dark themes, English, Arabic and Hebrew.
  Check key/placeholder parity for every locale, and label fitting in all locales
  for affected controls before declaring their localization complete.
- Exercise 100%, 125% and 150% Windows scaling, larger configured text and
  moving between differently scaled monitors. Compare matching configuration
  pairs; do not rescale captures to manufacture agreement.
- Include open/closed side panels, narrow windows, long labels, keyboard focus,
  reduced motion and an active accessibility client.
- Use short and mixed-height conversations, the 120-turn rich transcript and
  2,301-session index. Keep the 23,010-session stress scenario separate.

Per change, run the affected scenarios and a core smoke set for sidebar
visibility, selection/draft preservation, transcript scrolling and keyboard
navigation. A shared text, theme, layout, scroller or input change invalidates
all inventory entries that depend on it. Run their full matrix. Run the full
supported feature/configuration matrix at milestone and replacement gates.
Each platform requires independent qualification.

## 6. Complete one feature at a time

For each feature:

1. Define the scenarios and capture their Electron reference before porting.
2. Implement the Rust feature using canonical tokens, strings and behavior.
3. Run both adapters against the same starting fixture and checkpoints.
4. Fix appearance, interaction, accessibility and lifecycle differences.
5. Rerun affected coverage and the core smoke set on the final implementation.
6. Review the comparison report. The user approves any intentional difference
   before it changes expected behavior.

A feature completes only when every required automated and manual scenario
passes with no unexplained difference. Keep previously qualified features in
regression coverage throughout later work. Missing features remain visible in
the full inventory; an offline transcript prototype qualifies only its stated
scope. The desktop replacement gate requires the full inventory, host/release
checks and applicable performance targets.

When the framework cannot reproduce required behavior, the feature stays
incomplete while its adapter or framework support is resolved. Removing the
behavior or changing its design requires the user's decision. Performance gains
cannot waive a parity failure.

## Evidence and references

Each private run report contains the baseline/candidate and tool identities,
fixture/configuration manifest, scenario results, checkpoint images, geometry,
normalized accessibility observations, input/output assertions, transient-frame
evidence, manual check notes, failures and exact owned-process cleanup results.
Retain raw captures alongside derived differences and preserve failed attempts.
Use synthetic data; never capture personal conversations to create a baseline.

Keep these run artifacts outside deliverable source. Commit only reusable
scenario contracts, synthetic fixtures, capture/comparison tooling and durable
documentation. Review changed expected results independently of the code that
would otherwise fail them.

- [Parity runner, inventory and scenarios](../../scripts/parity/DOCUMENTATION.md)
- [Existing capture and isolation procedure](../../scripts/perf/DOCUMENTATION.md)
- [CDP adapter](../../scripts/perf/cdp.mjs)
- [Browser history/scroll scenarios](../../packages/ui/tests/chat-history-scroll.browser.mjs)
- [Deterministic event stimulus](../../scripts/perf/fixture-provider.mjs)
- [Desktop ownership and lifecycle](../../packages/electron/README.md)
- [Sidebar behavior](../../packages/ui/src/components/session/sidebar/DOCUMENTATION.md)
- [GPUI Kit accessibility/testing guidance](https://github.com/longbridge/gpui-kit/blob/v0.7.3/website/docs/accessibility.md)
