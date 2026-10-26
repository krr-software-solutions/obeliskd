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

## The model and its state

- `model/`: the allocation model and its validation. `Name` follows the hostname-label rules
  (lowercase LDH, at most 63 characters, no `--` in positions 3–4), so a name works as a DNS label,
  a docker network name and a k8s name. `Supernet` is a name plus an IPv4 CIDR; it must be the
  start of its range and must not overlap another supernet. `Subnet` (nested in its supernet) has
  a kind (`host` | `docker`) and a CIDR that **obeliskd always picks**: the first free, aligned
  range of the requested `--size`. There's no `--cidr`. A docker subnet also records
  `dockerRange`, Docker's own slice at the start of the subnet (`--reserve-size`, required for
  docker and refused for host), and `dockerGateway`, the host's address on the network (always the
  subnet's first usable address, inside the slice). obeliskd will only hand out addresses outside
  that slice.
- `state/`: `Store` holds the whole tree in memory behind a mutex, and every change is validated,
  saved, then made visible. `state/files.rs` owns the on-disk layout, **one file per allocation,
  found by scanning**:
  - `alloc/<supernet>.toml`;
  - `alloc/<supernet>/<subnet>.toml`, which holds a subnet and its `[[ips]]`, each with its `hosts`.

  Only files that changed are written. Removals go first (subnet files, then the supernet's dir,
  then its file) and additions after (a supernet's file before its subnets'), so a crash part-way
  leaves something smaller but consistent. `config.toml` is for daemon settings, never
  allocations.

Deallocating a supernet that still has subnets is refused (409) unless `--recursive`
(`?recursive=true`), which removes its subnets with it; the response includes them. Dealloc routes are `DELETE /supernets/{supernet}` and
`DELETE /subnets/{supernet}/{subnet}` (Admin), and they return what was removed.

The tree is supernet → subnet → ip. `Ip` (nested in its subnet) is a name plus an address that
**obeliskd always picks**: the lowest free one (`model/ip.rs`):

- never an address whose last octet is `0` or `255`, in any subnet;
- in a docker subnet: never inside `dockerRange` (which also holds the gateway), and never the
  subnet's network or broadcast address;
- in a host subnet: anything else in the range.

**Hosts** (`model/host.rs`) are full hostnames (`app.obd.internal`, never `obd.internal` itself,
and a label may be exactly `*`: a wildcard for one label; `dns/hosts.rs` has the matching and
priority rules). On disk they live **on the ip they resolve to**
(`[[ips]] … hosts = ["app.obd.internal"]`, in the subnet's file), so a host can't outlive its ip.
Hostnames are unique across every subnet. An ip with hosts can't be deallocated, and neither can
anything containing it, even with `--recursive`. DNS reads a host table (`dns::SharedHosts`) that
the store swaps after every change, so queries never wait on the store's lock.

Ip names are unique within their subnet. An ip may carry **one optional `tag`** (`ip alloc --tag
pr-42`, name rules): a group label, a sort of "sub-sub-net", so a set of ips can be found together
(`ip list --tag pr-42`), and later removed together. The tag doesn't affect which address is picked.
The `--tag` filter drops branches with no matching ips, rather than showing them as dash rows. A subnet that still has ips needs `--recursive` to be
deallocated, just like a supernet with subnets.

## The OS side (`code/obeliskd/src/os/`)

The state files are the desired state; `OsEffects` makes the OS match it. Every operation is
idempotent (ensure present / ensure absent):

| Object | Linux | macOS |
|---|---|---|
| supernet | `ip route replace unreachable <cidr> metric 9999` | nothing |
| host subnet | `ip route replace local <cidr> dev lo table local` | nothing (each ip will be an lo0 alias) |
| docker subnet | `docker network create --subnet --gateway --ip-range --label obeliskd.managed=true … obd-<supernet>-<subnet>` | the same |
| ip in a host subnet | nothing (the subnet's `local` route covers it; checked to be bindable) | `ifconfig lo0 alias <ip> netmask 255.255.255.255` |
| ip in a docker subnet | nothing (a reservation) | nothing |

- **Order:** a change goes to the OS first and is saved second; if saving fails, the OS change is
  undone. A removal goes off the OS first and is saved second. If the OS step fails, nothing is
  saved.
- **At daemon start:** `Store::reconcile` re-applies everything. Routes don't survive a reboot,
  and docker networks are checked. Failures are logged as warnings and the daemon still comes up.
- **Docker networks:**
  - obeliskd only ever touches networks labelled `obeliskd.managed=true`;
  - an existing one whose subnet, gateway or ip-range differs is a conflict, never silently "fixed";
  - one that still has containers attached can't be removed (409).
- **Tools:** routes and Docker are driven through their CLIs (`ip`, `docker`), via
  `os/command.rs`. Per-OS route behavior lives in `PlatformImpl` (`ensure_host_subnet`,
  `ensure_supernet_route`, …); Docker is the same on every OS (`os/docker.rs`).
- **Tests:** they use `os::fake::RecordingOs`, which records calls and can be told to fail. The
  real Docker path has an opt-in test: `cargo test -- --ignored` (needs Docker access).

## Logging

The daemon logs with `tracing`. The CLI sets up the subscriber for `daemon run` only:
`--log-level` / `OBELISKD_LOG`, default `info`, takes `EnvFilter` syntax such as
`info,obeliskd::dns=warn`. Output goes to stderr, with color only on a terminal. Client commands
don't log.

| What | Level | Where |
|---|---|---|
| startup steps (state loaded, reconcile, DNS bound, socket bound, ready), shutdown | info | `daemon.rs` |
| each control API request: `POST /ips → 201 Created (2ms) uid=0` | info (warn on 5xx) | `log_request` middleware, `control/server.rs` |
| what each change did: `allocated ip ex1/1/db = 10.57.0.1` | info | `state/mod.rs` |
| each DNS query: `A app.obd.internal → NOERROR 10.57.0.1 from=…` | info | `dns::serve` |
| each OS command run (`ip …`, `docker …`) | debug | `os/command.rs` |
| reconcile failures, failed replies, tightened permissions | warn | where they happen |

Use `tracing::{info, warn, debug}`, never `eprintln!`, in the library.

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
  - neither: anyone who can connect. **All reads work this way** (`/health`, `GET /supernets`):
    nothing obeliskd holds is secret, and needing `sudo` just to look would be friction. Every
    change needs `Admin` (or `Caller`, once access grants exist).

  Admin means root **or the daemon's own uid**. The daemon now always runs as root, so this is
  just root. Requests are parsed first and then rejected with 403. That's intended: there's no
  check-before-read.

To add an endpoint, add its types to `api.rs`, a handler and route in `server.rs`, and a method in
`client.rs`. Domain errors map to statuses in `From<ObeliskdError> for ApiError`: `Invalid` → 400,
`Conflict` → 409, everything else 500.

Every CLI command that talks to the daemon takes `--format pretty|json` (default `pretty`). JSON is
the API type as-is, and pretty-printing lives in `obeliskd-cli/src/pretty.rs`.
**Responses are trees.** Every list and every change returns supernets with their `subnets[]`,
`ips[]` and `hosts[]` nested (`SupernetNode` → `SubnetNode` → `IpNode`, in `api.rs`):

- a list goes down to its own level (`supernet list` stops at supernets, `ip list` reaches ips);
- a level that was asked for is always present, as `[]` when empty, so childless parents show up;
- filters (`--supernet`, `--subnet`) prune the tree;
- a change returns the path to what changed: `{ ex1, subnets: [ { 1, ips: [ { db-5 } ] } ] }`.

`--format json` prints that as-is. Pretty lists are tables with one column per level, and a
parent's columns appear once, on its first row, blank under it. A parent without children gets a
row of `-`, and an empty list is an empty table (`obeliskd-cli/src/pretty.rs`).

`--help` groups options under three headings: **Core Options** (what the command is about),
**Extra Options** (presentation shared by many commands, e.g. `--format`) and **Special Options**
(plumbing on every command: `--socket`, `--help`, `--version`). `command()` in `main.rs` files
every unlabelled option under Core and moves `--help`/`--version` to Special. Only exceptions need
a label: `help_heading = SPECIAL` on an arg, or `#[command(next_help_heading = EXTRA)]` on a
flattened struct. Test handlers with `MockConnectInfo` (see `server.rs`'s tests).

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
