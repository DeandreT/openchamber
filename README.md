# Unmerged native desktop work, exported 2026-11-12

Scratch packets and working notes for the Rust native desktop port, saved here
so local build output could be deleted. Nothing in this branch is built or
tested as a unit. Base for every packet: `feat/rust-native-desktop`.

- `packets/chat-drafts-impl-2de67d0d`: complete proposal for chat drafts and
  Chats-root sessions. Never compiled, not reviewed. Decisions recorded in
  `notes/port-state.md`: picker row label "Chat", keep the existing failure
  toast, create the chat folder on first keystroke as the reference does.
- `packets/header-search-actions-impl-1c1c67fd`, `packets/settings-mcp-agents-impl-f26059d0`,
  `packets/settings-providers-impl-k7q2m9`: partial work from interrupted
  sessions. Treat as starting material only.
- `packets/ssh-unit-a-integration-export`: SSH cleanup-reporting export; applies
  on top of the `native-hosts-relay` source, not the current branch.
- `notes/port-state.md`: running record; newest state is in the last section.
- `notes/parity-544fef273-triage.md`: causes of the first full parity run's
  failures against packaged Electron 2.6.1.
- `tools/`: the serial build/run wrapper and helper scripts the notes refer to.
