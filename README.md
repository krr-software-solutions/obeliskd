# obeliskd

A small, hand-written DNS server in Rust. It answers for `*.obd.internal`, and the OS routes only
that suffix to it.

> **Status:** early. Allocate supernets, subnets and ips, point hostnames under `obd.internal` at them, and DNS answers.

## Goals

- Answer exact-match rules locally (A / AAAA), with the DNS wire format understood from the ground up.
- Edit rules live through a local control socket; root grants users and groups roles on parts of the tree.
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

## Installing

Linux (x86_64, aarch64; the service needs systemd) and macOS:

```sh
curl -fsSL https://github.com/krr-software-solutions/obeliskd/releases/latest/download/install.sh | sh
obeliskd manage status   # installed, running, and resolving through the system resolver?
```

It downloads and verifies the binary, then runs `sudo obeliskd manage install`: the binary goes to
`/usr/local/bin`, and a systemd/launchd service starts it now and at every boot. Or, with Rust:
`cargo install obeliskd-cli`, then `sudo "$(command -v obeliskd)" manage install` (sudo's PATH
usually doesn't include `~/.cargo/bin`).

From a checkout:

```sh
cargo build --release
sudo ./target/release/obeliskd manage install   # /usr/local/bin/obeliskd + a systemd/launchd service, started now and at boot
obeliskd manage status                          # installed, running, and resolving through the system resolver?
```

Requires Rust 1.85+ (edition 2024).

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT),
at your option.
