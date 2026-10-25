# Development

## Commands

```sh
cargo run -p obeliskd                                        # run the daemon
cargo fmt --all                                              # format
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```

`--locked` matches CI: if `Cargo.lock` needs to change, commit the change.

## CI (`.gitlab-ci.yml`)

MR pipelines only, for now. Jobs come from the shared templates in `krrss/util/pipeline-utils`
(`.runner`, `".mr=…,tag=…"`).

| Job               | When   | What                                                           |
|-------------------|--------|----------------------------------------------------------------|
| `check-pre-merge` | auto   | fails if a `FIX-PRE-MERGE` marker is left anywhere (see below) |
| `check`           | auto   | fmt, clippy (`-D warnings`), tests                             |
| `build: [target]` | manual | cross-builds `obeliskd` with cargo-zigbuild (Linux musl, macOS) |

No release, image, or publish jobs yet.

## The `FIX-PRE-MERGE` marker

Leave `FIX-PRE-MERGE` in a comment for anything that must be fixed before merge. CI fails while
any remain. `scripts/check-for-fix.sh` skips itself and `.claude/`, so don't write the literal
marker anywhere else (CI comments included) unless you mean it.

## Repo

- GitLab: `gitlab.krr-software-solutions.com/oss/obeliskd` (project ID 22), default branch `main`.
- Merge via MR; squash to a single clean commit before merging.
