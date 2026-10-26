# Development

## Commands

```sh
cargo build && sudo ./target/debug/obeliskd --socket /tmp/o.sock daemon run --state-dir /tmp/obd-state --dns-address 127.0.0.79:5379
                                                              # the daemon always needs root (routes, docker)
sudo ./target/debug/obeliskd --socket /tmp/o.sock supernet alloc --supernet obd --cidr 10.57.0.0/16
                                                              # changes need root too; reads don't
cargo run -p obeliskd-cli -- --socket /tmp/o.sock supernet list --format json
sudo ./target/debug/obeliskd --socket /tmp/o.sock subnet alloc --supernet obd --subnet k3d-dev --kind docker --size 24 --reserve-size 26
cargo run -p obeliskd-cli -- --socket /tmp/o.sock subnet list
sudo ./target/debug/obeliskd --socket /tmp/o.sock ip alloc --supernet obd --subnet k3d-dev --ip ingress
./target/debug/obeliskd --socket /tmp/o.sock ip list
sudo ./target/debug/obeliskd --socket /tmp/o.sock subnet dealloc --supernet obd --subnet k3d-dev --recursive
#   port 5379, not 53 (needs root) or 5353 (mDNS, already held by avahi/browsers).
#   --state-dir and --dns-address are `daemon run` flags; clients only ever need --socket.
cargo run -p obeliskd-cli -- --socket /tmp/o.sock daemon health  # call it
cargo run -p obeliskd-cli -- completions zsh                 # shell completions
cargo fmt --all                                              # format
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
cargo test --workspace --locked -- --ignored                 # + the real-Docker test (needs docker access)
```

`--locked` matches CI: if `Cargo.lock` needs to change, commit the change.

The default socket (`/run/obeliskd/obeliskd.sock` on Linux) needs root to create. For development,
point both sides elsewhere with `--socket` or `OBELISKD_SOCKET`. Only root passes the `Caller` and
`Admin` checks for now, but `/health` is open to anyone.

## CI (`.gitlab-ci.yml`)

MR pipelines only, for now. Jobs come from the shared templates in `krrss/util/pipeline-utils`
(`.runner`, `".mr=…,tag=…"`).

| Job               | When   | What                                                           |
|-------------------|--------|----------------------------------------------------------------|
| `check-pre-merge` | auto   | fails if a `FIX-PRE-MERGE` marker is left anywhere (see below) |
| `check`           | auto   | fmt, clippy (`-D warnings`), tests                             |
| `check-target`    | auto   | clippy for macOS (platform code must compile)                  |
| `build: [target]` | manual | cross-builds the `obeliskd` binary with cargo-zigbuild (Linux musl, macOS) |

No release, image, or publish jobs yet.

## The `FIX-PRE-MERGE` marker

Leave `FIX-PRE-MERGE` in a comment for anything that must be fixed before merge. CI fails while
any remain. `scripts/check-for-fix.sh` skips itself and `.claude/`, so don't write the literal
marker anywhere else (CI comments included) unless you mean it.

## Repo

- GitLab: `gitlab.krr-software-solutions.com/oss/obeliskd` (project ID 22), default branch `main`.
- Merge via MR; squash to a single clean commit before merging.
