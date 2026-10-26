# obeliskd

A small, hand-written DNS server in Rust. It answers for `*.obd.internal`, and the OS routes only
that suffix to it.

> **Status:** early. It answers `*.obd.internal` A queries with a placeholder address; rules come next.

## Goals

- Answer exact-match rules locally (A / AAAA), with the DNS wire format understood from the ground up.
- Edit rules live through a local control socket, with a permission split between normal users and root.
- Run on Linux, macOS, and Windows, with platform-specific setup selected at compile time.
- Optionally, help mint certs so overridden names can serve HTTPS.

## Layout

| Path                | Crate          | What                                  |
|---------------------|----------------|---------------------------------------|
| `code/obeliskd`     | `obeliskd`     | the library                           |
| `code/obeliskd-cli` | `obeliskd-cli` | the CLI / daemon (binary: `obeliskd`) |

## Building

```sh
cargo build
sudo ./target/debug/obeliskd daemon run      # needs root: it manages routes and docker networks
./target/debug/obeliskd daemon health        # in another shell
cargo test --workspace
```

Requires Rust 1.85+ (edition 2024).
