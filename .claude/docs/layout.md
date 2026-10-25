# Layout

A Cargo workspace; every crate lives under `code/`.

| Path                | Crate          | Kind | Role                                      |
|---------------------|----------------|------|-------------------------------------------|
| `code/obeliskd`     | `obeliskd`     | bin  | the daemon: wiring and startup only       |
| `code/obeliskd-lib` | `obeliskd-lib` | lib  | everything else: protocol, rules, logic   |

Keep the binary thin. Logic goes in `obeliskd-lib`, where it can be tested.

## Shared settings

The root `Cargo.toml` owns everything shared:

- `[workspace.package]`: version (placeholder `0.0.0`), edition, `rust-version`, repository. Crates
  inherit with `version.workspace = true` etc.
- `[workspace.dependencies]`: every dependency version, including the workspace crates
  themselves. Crates depend with `foo.workspace = true`, never their own version.
- `[workspace.lints]`: `unsafe_code = "forbid"` and clippy `pedantic`. Each crate opts in with
  `[lints] workspace = true`.

## Adding a crate

1. Create `code/<name>/` with `Cargo.toml` inheriting the workspace package fields and lints
   (copy `code/obeliskd-lib/Cargo.toml`).
2. Add it to `members` in the root `Cargo.toml`.
3. If other crates use it, add it to `[workspace.dependencies]` with a `path` and `version = "=0.0.0"`.

## Platform-specific code

When it arrives: a trait for the contract, a unit struct per OS, and a `#[cfg(target_os = ...)]`
re-export (`pub use linux::Linux as Current;`). Call it statically (`Current::foo()`), not through
`&dyn`. Unsupported targets hit `compile_error!`. Per-OS dependencies go under
`[target.'cfg(...)'.dependencies]`.
