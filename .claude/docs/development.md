# Development

## Commands

```sh
cargo build && sudo ./target/debug/obeliskd --socket /tmp/o.sock daemon run --state-dir /tmp/obd-state --dns-address 127.0.0.79:5379
                                                              # the daemon always needs root (routes, docker)
sudo ./target/debug/obeliskd --socket /tmp/o.sock supernet alloc --supernet obd --cidr 10.57.0.0/16
                                                              # supernets need root; reads need nothing
sudo ./target/debug/obeliskd --socket /tmp/o.sock grant add --role manage --user "$USER" --supernet obd
                                                              # then the rest works without sudo
./target/debug/obeliskd --socket /tmp/o.sock grant check
cargo run -p obeliskd-cli -- --socket /tmp/o.sock supernet list --format json
sudo ./target/debug/obeliskd --socket /tmp/o.sock subnet alloc --supernet obd --subnet k3d-dev --kind docker --size 24 --reserve-size 26
cargo run -p obeliskd-cli -- --socket /tmp/o.sock subnet list
./target/debug/obeliskd --socket /tmp/o.sock supernet status  # sizes, subnets, and a colour grid of the space
./target/debug/obeliskd --socket /tmp/o.sock subnet status --supernet obd   # the same per subnet, down to its ips
sudo ./target/debug/obeliskd --socket /tmp/o.sock ip alloc --supernet obd --subnet k3d-dev --ip ingress
./target/debug/obeliskd --socket /tmp/o.sock ip list
./target/debug/obeliskd --socket /tmp/o.sock ip resolve --supernet obd --subnet k3d-dev --ip ingress
                                                              # just the address; exit 1 if there's none
sudo ./target/debug/obeliskd --socket /tmp/o.sock host alloc --host app.obd.internal --supernet obd --subnet k3d-dev --ip ingress
dig @127.0.0.79 -p 5379 +short app.obd.internal               # → the ingress ip's address
./target/debug/obeliskd --socket /tmp/o.sock host resolve --host app.obd.internal --format json
                                                              # the same answer, with which host matched
sudo ./target/debug/obeliskd --socket /tmp/o.sock subnet dealloc --supernet obd --subnet k3d-dev --recursive
#   add --log-level debug to also see every ip/docker command it runs.
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
point both sides elsewhere with `--socket` or `OBELISKD_SOCKET`. Reads are open to anyone; changes
need root or a grant (see `grant add`).

## CI (`.gitlab-ci.yml`)

MR pipelines, and release pipelines from a version tag (the same setup as rustable). Jobs come
from the shared templates in `krrss/util/pipeline-utils` (`.runner`, `".mr=…,tag=…"`).

| Job               | MR     | Tag  | What                                                           |
|-------------------|--------|------|----------------------------------------------------------------|
| `check-pre-merge` | auto   |      | fails if a `FIX-PRE-MERGE` marker is left anywhere (see below) |
| `check`           | auto   | auto | fmt, clippy (`-D warnings`), tests                             |
| `check-target`    | auto   | auto | clippy for macOS (platform code must compile)                  |
| `shellcheck`      | auto   |      | `install.sh` and `scripts/`                                    |
| `publish-dry-run` | auto   |      | both crates, packaged and verified as crates.io will see them  |
| `build: [target]` | manual | auto | cross-builds `obeliskd` with cargo-zigbuild (x86_64/aarch64 Linux musl and macOS), archived with a `.sha256` |
| `gitlab-release`  |        | auto | archives → the generic package registry, and the GitLab release |
| `github-release`  |        | auto | pushes the tag to the GitHub mirror and creates its release (archives + `install.sh`) |
| `install-smoke`   |        | auto | `install.sh` from that release (binary only) installs that exact version |
| `publish-crates`  |        | auto | `scripts/publish-crates.sh`, last: crates.io can't be undone. One crate at a time, skipping versions already published and waiting (up to 15 min) for each to reach the index, so a failed run can just be retried |

## Releasing

The version comes from the tag: Cargo.toml keeps the placeholder `0.0.0` (and the `=0.0.0` pin
between the crates), and release jobs run `scripts/set-version.sh "$CI_COMMIT_TAG"` first. To
release, tag the merged `main`:

```sh
git tag v0.1.0 && git push origin v0.1.0
```

Needs the CI variables `GITHUB_TOKEN` (writes the GitHub mirror) and `CARGO_REGISTRY_TOKEN`.
The GitHub mirror (`github.com/krr-software-solutions/obeliskd`) is a GitLab push mirror of
protected branches; `github-release` pushes the tag itself so it never waits for the mirror.
`install.sh` downloads and verifies the archive, then runs `sudo obeliskd manage install`
(`OBELISKD_INSTALL_DIR` puts just the binary somewhere, without a service).

## The `FIX-PRE-MERGE` marker

Leave `FIX-PRE-MERGE` in a comment for anything that must be fixed before merge. CI fails while
any remain. `scripts/check-for-fix.sh` skips itself and `.claude/`, so don't write the literal
marker anywhere else (CI comments included) unless you mean it.

## Repo

- GitLab: `gitlab.krr-software-solutions.com/oss/obeliskd` (project ID 22), default branch `main`.
- Merge via MR; squash to a single clean commit before merging.
