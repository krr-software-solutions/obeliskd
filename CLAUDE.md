# CLAUDE.md

Lean working notes for obeliskd: a hand-written Rust DNS server. Keep this file short; detail
lives in `.claude/docs/`.

## Before you push

```sh
cargo fmt --all
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```

CI runs the same; clippy warnings (pedantic) fail the MR.

## Docs

- [Overview](.claude/docs/overview.md): what obeliskd is, current state, and the open design questions.
- [Layout](.claude/docs/layout.md): the workspace, the crates, and how to add one.
- [Development](.claude/docs/development.md): commands, CI, MRs, and conventions.
