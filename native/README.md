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

AccessKit registers with AT-SPI only while the session's accessibility bus
reports `org.a11y.Status.IsEnabled`. A screen reader turns it on; automation
can set it with
`busctl --user set-property org.a11y.Bus /org/a11y/bus org.a11y.Status IsEnabled b true`
and should restore the previous value afterwards.
