# XIM client text-error patch

## Ownership

This is `zed-xim` 0.4.0-zed from crates.io. It owns the X11 input-method
protocol used by GPUI. Text conversion belongs to the sibling
[`xim-ctext` patch](../xim-ctext/DOCUMENTATION.md).

The source archive SHA-256 is
`0c0b46ed118eba34d9ba53d94ddc0b665e0e06a2cf874cfa2dd5dec278148642`.
The upstream MIT license, original manifest and VCS metadata are retained.
Unused examples, benchmarks, their development dependencies, editor/CI files,
registry marker and nested lockfile are omitted. Attribute-builder signatures
spell out their existing borrowed lifetimes for current Rust diagnostics.

## Invariants

- Preedit, reset and commit decode failures return `ClientError::DecodeText`.
  Malformed or unsupported wire text cannot panic or reach a text handler.
- A synchronous character commit receives `SyncReply` even when decoding or
  its handler fails, so the server can process later input. The error still
  reaches the caller. A reply transport error takes precedence.
- Reset handler failures propagate to the caller.
- Protocol failures affect the current request. A later valid request can
  still deliver its exact Unicode text.

## Decisions

### Why return typed decoding errors

The XIM server controls incoming bytes. Panicking on invalid text terminates
the desktop; returning an error uses the existing client error path without
inventing text. GPUI owns reporting that error and subsequent UI behavior.
The pinned `gpui-pre-linux` X11 event loop discards its XIM client on any
`ClientError`. Retaining composition after a malformed request requires that
consumer to handle `DecodeText` separately from transport failures.

### Removal

Remove this directory and its Cargo patch when the pinned upstream version
returns decode errors at all 3 text sites and preserves synchronous progress.
Run the focused command and private-display qualification documented in
`xim-ctext/DOCUMENTATION.md` before removing either patch.
