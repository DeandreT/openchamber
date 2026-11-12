# Chat drafts and Chats-root sessions

Native port of the reference's Chat draft target (Electron 2.6.1). Nothing in
this packet was compiled or run: cargo, tests and app launches were off limits.
`rustfmt +1.97.1 --edition 2024` with `native/rustfmt.toml` leaves every `.rs`
file under `after/` unchanged.

## Layout

- `before/`: the working-tree copies the edits started from. All 16 still
  matched the tree when the packet was built.
- `after/`: the edited files plus three new ones
  (`core/src/chat_draft.rs`, `desktop/src/chat_draft.rs`,
  `desktop/src/chat_draft_tests.rs`).
- `source.patch`: `diff -ruN before after`. Apply from the repository root
  with `patch -p1`.
- `manifest.json`: SHA-256 of every file in the packet.

The `session_creation.rs` diff is larger than its change: rustfmt re-indents
the creation task once the create call gains `.await.flatten()`.

## How a Chat draft works here

A runtime's new-session draft targets Chat when `chat_drafts` has an entry for
it and `draft_projects` has none. A project chosen by any path wins.

The draft's key is `(runtime, <root>/<YYYY-MM-DD>/session-<id>, "chat-new-session")`.
The client names the folder as soon as the server names its Chats root, and
the first send creates it with `POST /api/fs/mkdir` right before
`POST /api/session`. The folder is the key's directory, so the creation
journal's rule (request directory equals source directory) holds and its
format does not change.

Until the root is known the Chat draft keeps its text under the existing
project-less key. When the root arrives the text moves under the folder key.

Catalogs, panels and the terminal of a Chat draft use the Chats root.

## Hunks for files the merge owns

These files are not in `after/`. Each hunk is required unless marked optional.

### `native/crates/desktop/src/files_view.rs`: panel directory (Files, Git, Diff, terminal)

`terminal_directory` already falls back to `panel_directory`, so this one hunk
covers all four.

```rust
    /// The directory a panel surface shows (`pickEffectiveDirectory`): the
    /// selected session's, else the draft's project, else the Chats root for
    /// a Chat draft (nothing until the server names it), else the active
    /// project.
    pub(crate) fn panel_directory(&self) -> Option<String> {
        if self.selected.is_some() {
            path::normalize_optional(self.selected_session()?.directory.as_deref())
        } else if self.chat_draft_open() {
            // Never the project that was on screen before the Chat.
            self.chat_draft_directory()
        } else {
            let project = self
                .draft_projects
                .get(&self.runtime)
                .map(String::as_str)
                .or_else(|| {
                    self.send_catalog
                        .settings
                        .as_ref()
                        .and_then(|settings| settings.active_project_path())
                })?;
            path::normalize(project)
        }
    }
```

Only the doc comment and the `else if` branch are new.

### `native/crates/desktop/src/workspace_view.rs`: the picker's accessibility action

The Chat row dispatches `ChooseDraftProject` with `openchamber:chats`, which
`choose_draft_project` rejects as unregistered. Route it through the picker's
owner:

```rust
                    cx.defer_in(window, move |this, window, cx| {
-                        this.choose_draft_project(&directory, window, cx);
+                        this.move_draft_to(&directory, window, cx);
                    });
```

### `native/crates/desktop/src/sidebar_view.rs`: toolbar button

```rust
                on_activate(new_session, cx, |this, window, cx| {
-                    this.show_new_session(window, cx);
+                    this.open_new_session(crate::chat_draft::NewSession::Plain, window, cx);
                })
```

### `native/crates/desktop/src/sidebar_view.rs`: the Chats header's "+"

Replace the body of the `SidebarItem::Zone` arm. Untested GPUI; the offsets
come from the reference (`h-6 w-6`, `inset-e-0.5` inside a wrapper with
`-me-2`, so 2 px from the header's extended edge).

```rust
            SidebarItem::Zone {
                key,
                activity,
                collapsed,
            } => {
                let toggled = item.clone();
                let header = self.header_row(
                    HeaderRow {
                        key,
                        label: zone_label(&strings, *activity),
                        style: HeaderStyle::Caption,
                        collapsed: *collapsed,
                        expandable: true,
                        spacing,
                    },
                    &palette,
                    direction,
                    cx,
                    move |this, _, cx| this.toggle_collapsed(&toggled, cx),
                );
                // Synthetic data has no server to create a Chat on.
                if *activity != ActivityKey::Chats || self.is_synthetic() {
                    header
                } else {
                    let add = on_activate(
                        div()
                            .id("chats-new-session")
                            .role(Role::Button)
                            .accessibility_id("chats-new-session")
                            .aria_label(strings.get("sessions.sidebar.header.actions.newSession"))
                            .absolute()
                            .top(px((CAPTION_HEIGHT - 24.0) / 2.0))
                            .inset_end(direction, px(2.0 - LIST_PADDING_END))
                            .size(px(24.0))
                            .rounded(px(6.0))
                            .flex()
                            .items_center()
                            .justify_center()
                            .cursor_pointer()
                            .invisible()
                            .group_hover("chats-zone", |style| style.visible())
                            .on_mouse_down(gpui_kit::MouseButton::Left, |_, _, cx| {
                                cx.stop_propagation()
                            })
                            .child(glyph("add", 16.0, palette.muted_foreground)),
                        cx,
                        |this, window, cx| {
                            this.open_new_session(crate::chat_draft::NewSession::Chat, window, cx)
                        },
                    )
                    .test_support();
                    div()
                        .relative()
                        .group("chats-zone")
                        .child(header)
                        .child(add)
                        .into_any_element()
                }
            }
```

### `native/crates/desktop/src/panel_tests.rs`: only if two tests fail

`show_unresolved_draft` and
`a_project_less_draft_saves_and_stages_in_the_active_project` call
`show_new_session` and then remove the draft project by hand. `show_new_session`
still changes no target, and `open_connected` starts no topology read, so both
should keep passing. If a fixture ever reaches `open_default_draft` first, add
after the `draft_projects.remove(..)` line:

```rust
        workspace.chat_drafts.close(&workspace.runtime.clone());
```

### `native/crates/desktop/DOCUMENTATION.md`

Replace the `project_chooser.rs` ownership entry:

```markdown
- `project_chooser.rs` is the target picker of a new-session draft: Chat, then
  the registered projects by their saved labels. A choice changes where the
  open draft runs without creating a server session, and the draft's text
  goes along to the new target. Failed discovery remains distinct from an
  empty project list, and a failed refresh keeps previously loaded projects.
- `chat_draft.rs` owns the draft's Chat target per runtime: the folder its
  session is created in, the entry points that open a draft
  (`open_new_session`), the first draft of a runtime, and the retry of a
  failed Chats-root read. `draft_projects` keeps owning project targets, and
  a project there wins over a Chat entry.
```

In the `files_view.rs` ownership entry, change "(else the draft's project,
else the active project)" to "(else the draft's project, else the Chats root
for a Chat draft, else the active project)".

Add under *Invariants*:

```markdown
- A Chat draft never works in a project. Its catalogs, panels and terminal
  use the Chats root, and nothing while the server has not named it. Its
  session is created only in a folder under a managed root.
- A failed Chats-root read is not a server without a root. Either one stops a
  send from the Chat draft; the draft, its target and its text stay, and only
  the failure is read again.
- A new draft opened by an entry point leaves the previous draft's text where
  it was. The target picker moves the open draft, so its text follows.
```

Add under *Decisions*:

```markdown
### Why a Chat draft is keyed on its folder and not on a shared bucket

The reference keys a Chat draft's text on one bucket and prepares the folder
on the first keystroke. The creation journal here rebuilds a create request
from its source draft's directory, and attachment staging belongs to the
draft's directory, so the draft's key names the folder its session will be
created in. The client names the folder once the root is known; the first
send creates it. A draft never sent leaves nothing on the server to remove.

### Why `show_new_session` keeps the draft's target

`show_new_session` only leaves the selected session. The entry points a
person uses go through `open_new_session`, which resolves the target the way
`openNewSessionDraft` does. Handoffs and the project menu choose their
project themselves right after, and would otherwise open a Chat first.
```

### `native/crates/core/DOCUMENTATION.md`

Add to the ownership list, next to `draft.rs`:

```markdown
- `chat_draft.rs` ports the new-session draft's Chat target: which target an
  entry point opens (`openNewSessionDraft`), the dated folder of a new Chat
  (`createChatDirectory`), the directory a Chat draft works in
  (`pickEffectiveDirectory`), the states of the Chats-root read, and the text
  a moved draft carries (`useComposerDraft.ts`).
```

Add under *Invariants*:

```markdown
- An explicit directory inside a managed Chats root opens another Chat, never
  a project draft rooted in a chat's folder.
- A Chats-root read that failed and a server without a root are different
  states.
```

### `native/crates/transport/DOCUMENTATION.md`

Add to the files routes:

```markdown
- `create_chat_directory` posts `/api/fs/mkdir` with no directory header. The
  server accepts a path under its managed Chats roots whatever project is
  active, creates missing parents, and answers success again for a folder
  that exists, so a retried create repeats it safely.
```

### Locale keys

No key list change is needed: every string used is already listed. See the
gap about the Chat row's label below.

## Affected surfaces

| Runtime | What happens |
|---|---|
| Native desktop | Everything in this packet |
| Web, Electron, VS Code, hosted mobile, Capacitor | Untouched; no shared code changes |

## Tests in the packet

Core (`chat_draft.rs`): target resolution per entry point, removed project,
forwarded chat directory, root states, folder name and containment, sidebar
placement of a session created in the folder, effective directory, carried
text.

Desktop (`chat_draft_tests.rs`): plain New session opens a Chat keyed in its
own folder under the root; entry points; switching target keeps text; a new
draft leaves text in place; a failed root read keeps the draft and creates
nothing; an unsent Chat draft is found again after a restart; a relocated
root replaces the folder. `ui_memory.rs` gains a restart test for the last
target.

Not covered by a test: the mkdir-then-create request order against a server,
and the journal accepting a Chat creation's staged selection. Both need the
`creation_tests` server, whose non-prompt POSTs answer `204 {}`; the mkdir
call expects `{"success": true}`, so that fixture needs one branch for
`/api/fs/mkdir` before a Chat creation can run in it.

## Known gaps against the reference

- **Folder timing.** The reference creates the folder on the first keystroke
  and deletes it when the draft closes. Native creates it at the first send.
  An attachment staged from a Chat draft before that send targets a folder
  that does not exist yet and will fail to stage.
- **MCP for a Chat draft.** The reference prepares the folder when the MCP
  surface opens and reads status there. Native reads nothing for a Chat draft
  (`mcp_directory` is `None`), as it does for a project-less draft.
- **Canonical paths.** If the server's Chats root is behind a symlink, the
  created session may report the canonical folder and the existing strict
  location check will call it a mismatch. The reference accepts the server's
  directory. Not reproduced; it follows from reading `create_session`'s
  callers.
- **Explicit unregistered directory.** The shortcut from a session in a
  directory that is neither registered nor a chat opens a Chat; the reference
  opens a project draft there. Native cannot create a session in an
  unregistered directory.
- **Starter chips** do nothing on a Chat draft: `activate_starter` needs a
  registered project.
- **Files do not follow a retarget.** The picker moves the draft's text; its
  attachments and context stay with the old target. The reference moves
  attached files too.
- **Hidden Chats section.** The reference turns the Chats section back on
  when a Chat is created while it is hidden. Not ported.
- **Chat row label.** The picker's Chat row reads "Chats"
  (`sessions.sidebar.activity.chatsTitle`); the reference reads "Chat"
  (`layout.mainTab.chat`), which is not in `locale-keys.txt`. Adding the key
  and regenerating `locales.json` lets `CHAT_LABEL` in `project_chooser.rs`
  switch.
- **Failure wording.** A send blocked by a failed root read shows the
  existing "The message wasn't sent. Its text is back in the composer." A
  string naming the Chats folder needs new wording in every locale, which is
  the maintainer's call.
- **Keyboard reveal of the "+".** It shows on hover; the reference also shows
  it while focus is inside the header.
