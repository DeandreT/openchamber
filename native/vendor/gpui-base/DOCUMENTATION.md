# Retained TextView selection

## Ownership

This directory vendors the crates.io `gpui-base` 0.7.3 package. The original
`LICENSE-APACHE`, package manifests, registry metadata and source remain with
the package. The native workspace selects it through `[patch.crates-io]`.

The local patch owns opt-in selection across virtualized Markdown TextViews.
`text/selection_document.rs` owns logical membership and UTF-8 endpoints.
`text_selection.rs` owns window activation and painted input geometry. Existing
TextView parsing and the rendered-text index supply the text and leaf offsets.
The application owns transcript identity, ordering, view lifetimes and updates.

## Application contract

Retain one `TextViewSelectionDocument` for each session and retain the attached
`Entity<TextViewState>` values in that session. Assign distinct
`TextViewSelectionKey` values to message bodies, parts and custom children.
Keys identify content and survive reordering. Supply logical transcript order
separately. Equal orders use keys as a deterministic tie breaker.

```rust
use gpui_base::text::{
    TextView, TextViewSelectionDocument, TextViewSelectionKey, TextViewState,
};

let selection = TextViewSelectionDocument::new();
let body = cx.new(|cx| TextViewState::markdown(source, cx));
selection.attach(TextViewSelectionKey(1), row_order, &body, cx)?;
selection.activate(window, cx);

// Retain `selection` and `body`; render only visible rows.
let element = TextView::new(&body);

// Apply these when transcript data changes, including while the row is absent.
selection.set_text(TextViewSelectionKey(1), updated_source, cx)?;
selection.set_order(TextViewSelectionKey(1), updated_order, cx)?;
selection.remove(TextViewSelectionKey(1), cx);
```

Mount `TextSelectionLayer` at the window root. Keep a root-level Copy action
handler and its key binding mounted while rows disappear. The handler calls
`TextSelection::selected_text(window, cx)` and writes a nonempty result to the
clipboard. A handler attached only to a virtualized row cannot receive Copy
after that row leaves the dispatch tree. Kit's `Root` already has a Copy handler.

Activate the incoming document on session switches and deactivate it when the
transcript closes. Switching clears selection. A document can be active in one
window at a time; moving activation removes its previous window's geometry.
Activation before the first root-layer frame is supported.

### Custom Markdown

Configure `TextViewState::set_markdown_extensions` before attachment, using the
same extensions as the rendered TextView. Unpainted members must have the same
parsed copy representation as painted members. The public setter accepts an
`Arc<MarkdownExtensions>`.

Retain custom heading, list-item and code TextView states too. Attach each with
`attach_child(child_key, parent_key, parent_source_range, &child, cx)`. The source
range is the enclosing custom block's range in the parent's Markdown. Nested
children can themselves be parents. `TextViewState::selection_document` exposes
the parent context through an owned or weak state handle; plugins do not need
to capture a strong document reference. `member(key)` retrieves a retained state
while its application owner still holds it.

Use `set_child_source` to refresh a growing block's source range without
replacing its member. Child attachment reads the committed rendered-text index,
so a custom renderer can attach a child while its parent is being rendered.

A selection within one child copies its partial text. Crossing a child boundary
selects its parent's custom block atomically. Parent and child never both emit
the same content. The custom node's `.text(...)` value defines its copy text,
as in the original renderer. Source spans are mapped with the existing rendered
index, without substring searches.

Remove and reattach children when their parent source identity changes. A
destructive parent update invalidates descendants. A parse failure invalidates
the member; remove and reattach it after correcting its source. An invalidated
member rejects updates through the document and cannot participate in selection.

## Invariants

- Membership contains weak view references and current parsed text. Views and
  window state hold weak document references. There is no ownership cycle.
- Painted registrations are swept normally. Offscreen members have no usable
  hitbox. Removal and text replacement discard geometry before returning.
- Logical endpoints contain a stable member key and a rendered UTF-8 byte
  offset. Reflow never reinterprets an old pixel point as a new text position.
- Copy includes complete intermediate members even if they never painted. It
  uses logical order and emits each member once. Plain text separators retain
  the existing window-selection newline convention.
- Appends preserve fixed offsets only while the rendered prefix through each
  affected endpoint remains identical. Completing Markdown markup can invalidate
  a selection even when the source update is an append.
- Text updates parse synchronously for attached members, using the existing
  incremental tail parser for appends. Copy therefore sees the committed update
  without requiring a frame. Queued older parser results cannot replace it.
- Removing an endpoint clears selection. Removing an intermediate member drops
  its contribution. Unrelated replacements preserve selection.
- Inactive documents provide neither copy text nor pointer targets. Explicit
  clearing and destructive changes cancel the old window gesture as well.
- A focused row can disappear from GPUI's dispatch tree during virtualization.
  Root handles the standard Copy shortcut without a key context only in that
  case, and only when selection has text. Normal input shortcuts keep their
  existing dispatch path. Copy preserves selected leading/trailing whitespace.
- Unattached participants retain the original frame-removal behavior. Detaching
  a member resynchronizes its ordinary background parser before future updates.
- Painting resolves only the current member's range and uses the existing
  ordered leaf index. It does not walk the transcript or draw offscreen content.

## Decisions

### Why selection has a separate logical owner

The original window selection requires both endpoint registrations and builds
copy items from painted participants. Retaining TextView entities alone still
loses selection when those registrations are swept. Explicit document membership
lets geometry expire while text positions survive.

### Why the patch is local

Version 0.7.3 has no supported TextView API that can retain and restore arbitrary
cross-view logical selection independently of painting. This patch keeps the
existing renderer and gives the application an explicit opt-in contract.

Remove the patch when the selected upstream version offers an equivalent public
contract and passes the retained-selection tests, then migrate callers to that
API. Preserve default-participant behavior during that migration.

## Focused validation

From `native/`:

```sh
cargo test --offline --manifest-path vendor/gpui-base/selection-tests/Cargo.toml --test retained_selection
```

The small test package exercises the patched library through public APIs using
GPUI's in-memory test platform. It uses existing GPUI dependencies and avoids
the upstream package's benchmark and optional development dependencies. Its
tests cover pointer selection, unmount/remount, reflow, an unpainted middle
member, Unicode direction, stale geometry, streaming, deletion, ordering,
activation and unchanged default removal behavior. It opens no desktop window.

The application still owns end-to-end clipboard shortcuts, virtual-list scroll
and auto-scroll qualification, custom-plugin wiring and session lifecycle tests.
