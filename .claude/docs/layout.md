# Layout

A Cargo workspace; every crate lives under `code/`.

| Path                | Crate          | Kind                 | Role                                    |
|---------------------|----------------|----------------------|-----------------------------------------|
| `code/obeliskd`     | `obeliskd`     | lib                  | everything: protocol, rules, logic      |
| `code/obeliskd-cli` | `obeliskd-cli` | bin (named `obeliskd`) | the CLI / daemon: wiring and startup only |

Keep the binary thin. Logic goes in the `obeliskd` library, where it can be tested.

## The CLI entry point

`code/obeliskd-cli/src/main.rs` follows one shape:

- `Cli` / `Command`: clap derive. Each subcommand maps to a `command_*` function.
- `main_impl() -> Result<(), ObeliskdError>`: parses, dispatches, and propagates errors with `?`.
- `#[tokio::main] async fn main() -> ExitCode`: only calls `main_impl` and turns an error into
  `ERROR - …` on stderr and exit code 1.

Errors are `obeliskd::ObeliskdError` (`thiserror`, in `code/obeliskd/src/error.rs`). Add a variant
per failure kind, with `#[from]` for wrapped errors, rather than stringly `Generic` messages.

The binary is named `obeliskd` too (`[[bin]] name`), so it sets `doc = false`: otherwise
`cargo doc` writes both to `target/doc/obeliskd` and one clobbers the other.

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
   (copy `code/obeliskd/Cargo.toml`).
2. Add it to `members` in the root `Cargo.toml`.
3. If other crates use it, add it to `[workspace.dependencies]` with a `path` and `version = "=0.0.0"`.

## Platform-specific code

When it arrives: a trait for the contract, a unit struct per OS, and a `#[cfg(target_os = ...)]`
re-export (`pub use linux::Linux as Current;`). Call it statically (`Current::foo()`), not through
`&dyn`. Unsupported targets hit `compile_error!`. Per-OS dependencies go under
`[target.'cfg(...)'.dependencies]`.
