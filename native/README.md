# Native desktop workspace

The Cargo workspace for the native OpenChamber desktop described in
[`docs/rust-desktop`](../docs/rust-desktop/ARCHITECTURE.md). It is separate
from the Bun `packages/*` workspace.

Linux is the first supported-platform qualification target. Windows and macOS
require separate runtime and release checks before support is claimed.

| Crate | Owns |
| --- | --- |
| `crates/core` (`openchamber-native-core`) | Domain records, sidebar membership and rows, transcripts, drafts, scroll memory and synthetic workloads. Standard library only. See its [DOCUMENTATION.md](crates/core/DOCUMENTATION.md). |
| `crates/transport` (`openchamber-native-transport`) | The connection to an OpenChamber server: endpoint, identity, authentication, requests, wire parsing and the global event stream. See its [DOCUMENTATION.md](crates/transport/DOCUMENTATION.md). |
| `crates/desktop` (`openchamber-native-desktop`) | The GPUI Kit window, views, theme, locale and icon resources, diagnostics, the server connection and application lifetime. See its [DOCUMENTATION.md](crates/desktop/DOCUMENTATION.md). |

## Toolchain

`rust-toolchain.toml` pins Rust 1.97.1 with rustfmt and Clippy; rustup selects
it inside `native/`. Windows builds use the MSVC target. Linux builds need the
GPUI system libraries: Wayland or X11 client libraries, `libxkbcommon`,
`fontconfig`/`freetype` and a Vulkan driver.

Cargo writes to `native/target`. Set `CARGO_TARGET_DIR` to keep a native
Windows build apart from a Linux or WSL build of the same checkout.

## Commands

Run these from the repository root:

```sh
bun run native:test      # all workspace tests, including the sidebar parity fixtures
bun run native:check     # rustfmt check and Clippy with warnings denied
bun run native:dev       # debug build of the desktop, opened on synthetic data
bun run native:build     # release build of the desktop
bun run native:fixtures  # regenerate the TypeScript reference fixtures
bun run native:resources # regenerate theme, locale and icon resources
bun run native:transport-fixtures # regenerate the official-client transport fixtures
```

The desktop reads these environment variables:

| Variable | Effect |
| --- | --- |
| `OPENCHAMBER_NATIVE_WORKLOAD` | `representative` (2,301 sessions, default), `stress` (23,010) or `small` |
| `OPENCHAMBER_NATIVE_THEME` | A built-in theme ID such as `openchamber-dark`; otherwise the system appearance picks the default pair |
| `OPENCHAMBER_NATIVE_LOCALE` | A supported locale such as `ar` or `pt-BR`; otherwise `LANG` |
| `OPENCHAMBER_NATIVE_DIAGNOSTICS` | `1` prints `OPENCHAMBER_NATIVE_MARK {json}` timing lines to stdout |
| `OPENCHAMBER_NATIVE_LOG` | A `tracing` filter for stderr, `warn` by default |
| `OPENCHAMBER_NATIVE_SERVER` | The base URL of an OpenChamber server to read instead of synthetic data, including any path prefix |
| `OPENCHAMBER_NATIVE_PASSWORD` | The UI password of that server, exchanged once for a client token |

During development, connect only to an isolated server, such as
`bun run profile:serve -- --root <dir> --port <port>`; see
`scripts/perf/DOCUMENTATION.md`.

## Local recovery state

Connected clients keep drafts, pending root-session creations and text sends in
`$XDG_CONFIG_HOME/openchamber-native/drafts-and-sends.json`, or the corresponding
`~/.config` path. The directory is private and the file is mode 0600 on Linux.
One process owns the profile's journal lock. A second process can read server
state but cannot edit or send until it can load the local state safely.

The pending-send row checks the original message ID before retrying. Retries
reuse that ID and never run automatically. Normal window close drains pending
saves; a dispatched prompt is recorded durably before network I/O begins.

New-session text submission uses the selected registered project's root directory.
The client saves one session ID before creation, verifies its returned location,
then durably binds the first message before sending. Check status never submits
the saved message. Retry same message uses the original identities and captured
configuration. An observed deletion never triggers recreation. Journal version 2
reads version 1; malformed or future formats remain untouched.

`native:fixtures` runs the TypeScript sidebar projection and rewrites
`crates/core/tests/fixtures/sidebar-projection.json`. `native:resources`
resolves the built-in themes, exports the keys in
`crates/desktop/resources/locale-keys.txt` for every locale and the glyphs in
`crates/desktop/resources/icon-names.txt` from the icon sprite.
`native:transport-fixtures` drives the pinned `@opencode/client` against the
recorded wire samples in `crates/transport/tests/fixtures/wire/` and rewrites
`crates/transport/tests/fixtures/transport.json`. The UI tests in
`packages/ui/src/lib/native/` fail when any generated file is stale.

## Accessibility on Linux

The private-display qualification commands and their dependencies are documented
in [`scripts/parity/DOCUMENTATION.md`](../scripts/parity/DOCUMENTATION.md).
Window controls use Openbox on the owned Xvfb display. IME checks use IBus with
libpinyin; screen-reader checks use Orca and a private, silent Speech Dispatcher.
These tools are test dependencies and are not bundled into the application.

Local toolkit patches are pinned under `vendor/`: retained Markdown selection in
[`gpui-base`](vendor/gpui-base/DOCUMENTATION.md), single-choice confirmation in
[`gpui-component`](vendor/gpui-component/DOCUMENTATION.md), and AT-SPI expansion
state in the bridge below. Each patch documents its compatibility and removal
conditions.

The native window enables the local [`gpui-pre`](vendor/gpui-pre/DOCUMENTATION.md)
logical-layout mode before its first layout. Fractional row sizes remain exact
through list measurement; painting snaps the final absolute edges.

X11 input uses the local [`xim-ctext`](vendor/xim-ctext/DOCUMENTATION.md) and
[`zed-xim`](vendor/zed-xim/DOCUMENTATION.md) decoding patches with the
[`gpui-pre-linux`](vendor/gpui-pre-linux/DOCUMENTATION.md) lifecycle patch.
These keep supported compound-text input valid and malformed callbacks
recoverable without discarding the input connection.

The workspace uses a [local AT-SPI translation patch](vendor/accesskit_atspi_common/DOCUMENTATION.md)
for the pinned AccessKit dependency so expandable controls expose their expanded
or collapsed state and state-change notifications.

AccessKit registers with AT-SPI only while the session's accessibility bus
reports `org.a11y.Status.IsEnabled`. A screen reader turns it on; automation
can set it with
`busctl --user set-property org.a11y.Bus /org/a11y/bus org.a11y.Status IsEnabled b true`
and should restore the previous value afterwards.
