# Layout

A Cargo workspace; every crate lives under `code/`.

| Path                | Crate          | Kind                 | Role                                    |
|---------------------|----------------|----------------------|-----------------------------------------|
| `code/obeliskd`     | `obeliskd`     | lib                  | everything: protocol, rules, logic      |
| `code/obeliskd-cli` | `obeliskd-cli` | bin (named `obeliskd`) | the CLI / daemon: wiring and startup only |

Keep the binary thin. Logic goes in the `obeliskd` library, where it can be tested.

## The daemon (`code/obeliskd/src/daemon.rs`)

`daemon::run` binds DNS first (the likeliest to fail: port 53 needs root), then the control
socket, and serves both until the shutdown signal.

- DNS lives in `dns/`: `wire.rs` is the protocol, and `mod.rs` holds the zone policy (`respond`, a
  pure function from packet to packet, which is what the tests target) and the UDP loop.
- The control API lives in `control/`.

## The control API (`code/obeliskd/src/control/`)

HTTP over the control socket: the daemon serves it with axum, and the CLI calls it with reqwest.

- `api.rs`: the wire types, shared by both sides (camelCase JSON). Errors are `{ "error": "…" }`.
- `server.rs`: the router, the handlers, and `run` (bind, serve until SIGTERM/Ctrl-C, unbind).
- `client.rs`: `ControlClient`, with one method per endpoint.
- `auth.rs`: who may call what. `Peer` (uid, gid, pid) is captured from the kernel once per
  connection (`ConnInfo`, via axum's `ConnectInfo`). A handler states what it needs by taking an
  extractor:
  - `Caller`: any user allowed on the socket;
  - `Admin`: root;
  - neither: anyone who can connect (e.g. `/health`).

  Requests are parsed first and then rejected with 403. That's intended: there's no
  check-before-read.

To add an endpoint, add its types to `api.rs`, a handler and route in `server.rs`, and a method in
`client.rs`. Test handlers with `MockConnectInfo` (see `server.rs`'s tests).

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

Lives in `code/obeliskd/src/platform/`:

- `mod.rs`: the `PlatformImpl` trait (the contract), and a `#[cfg(target_os = ...)]` export of
  exactly one implementation as `Platform` (`pub use linux::Linux as Platform;`).
- `linux.rs`, `macos.rs`, `windows.rs`: a unit struct per OS implementing `PlatformImpl`.
- `unix.rs` (`#[cfg(unix)]`): the Unix-socket code that Linux and macOS share. Their impls
  delegate to it and add only their own paths.
- **Windows isn't supported yet:** `windows.rs` starts with a `compile_error!`, so only Windows
  builds fail. To start supporting it, delete that line and re-add the Windows `check-target` job.
- Any other OS hits `compile_error!`.

`PlatformImpl` holds only the OS-specific edges: paths and defaults (`state_dir`,
`control_socket_path`, `default_dns_address`), making an address local (`ensure_local_address`), the
control socket's transport (`type ControlListener`, `bind_control`, `unbind_control`,
`control_client`), who is calling (`peer`), and `shutdown_signal`. The API, auth, and state on top
of those are shared code.

Call through the type, `Platform::foo()`, with `PlatformImpl` in scope (both are re-exported at the
crate root). Trait methods take no `self`, so calls are static, the structs are zero-sized, and
other OSes' code isn't compiled. Don't use `&dyn PlatformImpl` unless you need dynamic dispatch
(e.g. test fakes).

To add an operation: add it to `PlatformImpl`, then implement it in all three files (including
`windows.rs`, so it's ready when re-enabled). CI's `check-target` job runs clippy for macOS, so a
missing macOS implementation fails the MR.
Per-OS dependencies go under `[target.'cfg(...)'.dependencies]`.
