# Native desktop workspace

The Cargo workspace for the native OpenChamber desktop described in
[`docs/rust-desktop`](../docs/rust-desktop/ARCHITECTURE.md). It is separate
from the Bun `packages/*` workspace.

| Crate | Owns |
| --- | --- |
| `crates/core` (`openchamber-native-core`) | Domain records, sidebar membership and rows, transcripts, drafts, scroll memory and synthetic workloads. Standard library only. See its [DOCUMENTATION.md](crates/core/DOCUMENTATION.md). |

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
bun run native:fixtures  # regenerate the TypeScript reference fixtures
```

`native:fixtures` runs the TypeScript sidebar projection and rewrites
`crates/core/tests/fixtures/sidebar-projection.json`. The UI test
`packages/ui/src/lib/native/sidebarParityFixtures.test.ts` fails when that file
is stale.
