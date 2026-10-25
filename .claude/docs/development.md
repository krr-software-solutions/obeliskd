# Development

## Commands

```sh
cargo run -p obeliskd-cli -- --socket /tmp/o.sock daemon run     # run the daemon, no root needed
cargo run -p obeliskd-cli -- --socket /tmp/o.sock daemon health  # call it
cargo run -p obeliskd-cli -- completions zsh                 # shell completions
cargo fmt --all                                              # format
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
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
