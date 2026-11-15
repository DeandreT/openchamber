# Pinned dependency audit

`npm audit --json` on 2026-11-13 reported 24 moderate package entries in this
plugin's local dependency tree. These entries trace to 2 reviewed advisories.
No dependency versions were changed to silence the audit.

## Concrete advisory impact

- [GHSA-3g3m-2fjr-6pw4](https://github.com/advisories/GHSA-3g3m-2fjr-6pw4)
  affects `glob` 9.0.0 through 14.0.1. An aborted glob walk can continue scanning
  directories after its caller times out. Attacker-influenced patterns or
  directory trees can therefore consume I/O and CPU beyond the intended budget.
  The advisory reports availability impact.
- [GHSA-g3hv-75wg-m249](https://github.com/advisories/GHSA-g3hv-75wg-m249)
  affects `minimatch` 7.0.0 through versions before 11.0.1. Patterns processed
  with `optimizationLevel >= 2` can amplify glob parts and cause excessive CPU
  and memory use. The advisory reports availability impact.

## Reachability in this plugin

The dependency path starts at `@opencode/plugin@2.2.7` through
`@opencode/util@2.2.7`. That utility package imports `glob` and `minimatch` in
`dist/glob.js`, and dynamically imports Arborist and Pacote in `dist/npm.js`.
Npm packaging/signing/cache packages and the Google authentication utility
chain account for the propagated audit entries.

The 5 native-agent RPC methods, execution adapter, attachment conversion,
registry and smoke script do not invoke globbing, minimatch, npm package
resolution or recursive removal. No plugin request field is forwarded to a
vulnerable pattern-processing call. The inspection therefore found no concrete
trigger through these plugin-owned paths.

This is not a claim that all OpenCode package installation, file discovery or
Google-provider paths are unreachable or fixed. Those upstream utilities remain
in the pinned dependency graph and need their own evaluation where they are
called. The exact pins stay unchanged pending an upstream-compatible update.
