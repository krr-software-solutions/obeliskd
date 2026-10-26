# Overview

obeliskd is a small DNS server written by hand in Rust. It answers for one zone,
**`obd.internal`** (`*.obd.internal`), and the OS routes only that suffix to it. Hand-rolling the protocol is part of the point: prefer
understanding the wire format over pulling in a DNS framework.

## Current state

`obeliskd daemon run` answers DNS for `*.obd.internal` over UDP (every name answers with a
placeholder address), and serves the control API on its socket: `GET /health` and supernets
(`obeliskd supernet list`/`alloc`). There are no subnets, ips or records yet; no TCP, no IPv6, no OS
routing setup.

## The zone and its answers (`code/obeliskd/src/dns/`)

The zone is `obd.internal` (`dns::ZONE`). `.internal` is reserved for private use (ICANN, 2024), so
no public name can collide with it. Names are matched case-insensitively and echoed in their
original case.

| Query | Response |
|---|---|
| `*.obd.internal`, type A | `NOERROR`, AA, one A record: the DNS listening address (**placeholder**, a TODO until rules exist), TTL 60 |
| `*.obd.internal`, other types; `obd.internal` itself | `NOERROR`, AA, no answers. Never `NXDOMAIN`: the name exists, just not with that type |
| anything outside the zone | `REFUSED`: not our zone. Never `NXDOMAIN`, which resolvers would cache as "doesn't exist" |
| not a standard query / malformed | `NOTIMP` / `FORMERR` |
| a response, or < 12 bytes | dropped |

The wire format is hand-rolled (`dns/wire.rs`, RFC 1035): just enough to read one question and
write one answer. Queries carrying EDNS (systemd-resolved sends them) are answered without an OPT
record, which RFC 6891 allows.

To route the zone to obeliskd on Linux (by hand for now; `obeliskd install` should do it later):

```sh
sudo ip link add obd0 type dummy && sudo ip link set obd0 up
sudo resolvectl dns obd0 127.0.0.79:53
sudo resolvectl domain obd0 '~obd.internal'
sudo resolvectl default-route obd0 false
resolvectl query anything.obd.internal
```

## Open design questions

Nothing below is decided. Don't silently pick an answer in code; raise it.

- Rule format and storage (in-memory with live edits, persisted in the state dir).
- When to add TCP (needed once an answer can be truncated) and IPv6 (AAAA answers, `[::]` listening).
- Cert helpers in v1? Name-constrained local CA vs ACME DNS-01 on an owned domain.
- How `obeliskd install` sets up OS routing (leaning: it does it itself, via `PlatformImpl`).

Settled so far:
- **Answer-only**, behind an OS routing domain (no forwarding).
- **Hand-rolled** wire format.
- **One control socket**, authorizing callers by their peer credentials.

These are tracked in the Aletheia knowledge graph under component `OBLSK:obeliskd`.

## Configuration: none by hand

Every setting, rule, and cert changes through the `obeliskd` CLI, which talks to the daemon over its
control socket. Nothing is a hand-edited file, so there is no `/etc/obeliskd`. Don't add one, and
don't add flags or files that expect a person to edit config.

- **State dir** (`PlatformImpl::state_dir()`, or `daemon run --state-dir`): everything that
  persists, owned by the daemon. Linux `/var/lib/obeliskd`, macOS
  `/Library/Application Support/obeliskd`. Daemon-only: the dir is `0700`, files `0600`, and the
  daemon runs with umask `077`. At startup it refuses a dir or `config.toml` owned by another uid
  or writable by others, and tightens ones that are merely readable. Nobody reads these files
  directly; every read and write goes through the API.
- **`config.toml`** (in the state dir): the persisted model, so far just `supernets[]`. Written
  atomically (temp file, fsync, rename). A file that won't parse stops the daemon from starting,
  rather than being overwritten.
- **Control socket:** one socket. The daemon checks the caller's kernel-verified uid
  (`peer_cred()`) before reading anything. Access starts root-only (so `sudo` works) and is widened
  through the CLI.

## DNS address

The daemon answers DNS on `--dns-address` (`OBELISKD_DNS_ADDRESS`). The default is
`127.0.0.79:53`, chosen because nothing else is known to use it:

- `.53` and `.54` belong to systemd-resolved, `.11` to Docker's DNS, and `127.0.1.1` to Debian's
  hostname entry.
- Avoid `.35`: other DIY DNS proxies pick it as "53 backwards".

On macOS only `127.0.0.1` exists on `lo0` by default, so `daemon run` adds the alias itself at
startup (`PlatformImpl::ensure_local_address` runs `ifconfig lo0 alias 127.0.0.79`, as root). The
alias doesn't survive a reboot, but the daemon re-adds it every time it starts. On Linux all of
`127.0.0.0/8` is local already, so nothing is needed.
