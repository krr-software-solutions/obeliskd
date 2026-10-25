# obeliskd

A small, hand-written DNS server in Rust. It answers a handful of local rules itself (e.g. names
under `.test`) and stays out of the way for everything else.

> **Status:** early scaffolding. Running it prints a greeting; no DNS yet.

## Goals

- Answer exact-match rules locally (A / AAAA), with the DNS wire format understood from the ground up.
- Edit rules live through a local control socket, with a permission split between normal users and root.
- Run on Linux, macOS, and Windows, with platform-specific setup selected at compile time.
- Optionally, help mint certs so overridden names can serve HTTPS.

## Layout

| Path                | Crate          | What                     |
|---------------------|----------------|--------------------------|
| `code/obeliskd`     | `obeliskd`     | the daemon (binary)      |
| `code/obeliskd-lib` | `obeliskd-lib` | the library it builds on |

## Building

```sh
cargo build
cargo run -p obeliskd
cargo test --workspace
```

Requires Rust 1.85+ (edition 2024).
