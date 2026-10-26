# Overview

obeliskd is a small DNS server written by hand in Rust. It answers for one zone,
**`obd.internal`** (`*.obd.internal`), and the OS routes only that suffix to it. Hand-rolling the protocol is part of the point: prefer
understanding the wire format over pulling in a DNS framework.

## Current state

`obeliskd daemon run` answers DNS for `*.obd.internal` over UDP from its **hosts** (hostname → ip),
and serves the control API on its socket: `GET /health`, supernets
(`obeliskd supernet list`/`alloc`/`dealloc`), subnets (`obeliskd subnet list`/`alloc`/`dealloc`), ips
(`obeliskd ip list`/`alloc`/`dealloc`) and hosts (`obeliskd host list`/`alloc`/`dealloc`).
Supernets and subnets are **real on the OS**:

- a supernet gets a Linux `unreachable` catch-all route;
- a host subnet gets a Linux `local` route;
- a docker subnet becomes a docker network `obd-<supernet>-<subnet>`.

An ip in a host subnet is usable as soon as it's allocated: on Linux the subnet's `local` route
covers it, and on macOS it gets a `/32` lo0 alias. An ip in a docker subnet is a reservation that a
container (`--ip`) or a MetalLB-pinned Service claims.

The daemon **requires root**, and re-applies all of this at every start. There's no TCP, no IPv6, and no DNS routing setup (`~obd.internal`).

## The zone and its answers (`code/obeliskd/src/dns/`)

The zone is `obd.internal` (`dns::ZONE`). `.internal` is reserved for private use (ICANN, 2024), so
no public name can collide with it. Names are matched case-insensitively and echoed in their
original case.

| Query | Response |
|---|---|
| a host, type A | `NOERROR`, AA, one A record: its ip's address, TTL 60 |
| no exact host, but a wildcard host matches (`*.b.x.obd.internal`) | the winning pattern's address (see below) |
| a host, other types | `NOERROR`, AA, no answers (NODATA): the name exists, just not with that type |
| `obd.internal` itself, or a name with hosts below it (e.g. `x.obd.internal` when `a.x.obd.internal` exists) | NODATA: these exist too (RFC 8020's "empty non-terminals") |
| any other name in the zone | **`NXDOMAIN`**, AA. obeliskd knows every name. There's no SOA in the answer, so resolvers don't cache it, and a new host works immediately |
| anything outside the zone | `REFUSED`: not our zone. Never `NXDOMAIN`, which resolvers would cache as "doesn't exist" |
| not a standard query / malformed | `NOTIMP` / `FORMERR` |
| a response, or < 12 bytes | dropped |

**Wildcard hosts.** A label can be exactly `*`, anywhere in the name (`*.apps…`, `a.*.x…`,
`*.*.x…`), and it matches **exactly one label**. When several hosts match a name:

- an exact host wins;
- otherwise the most specific pattern wins, judged **from the right** (from the zone outward): at
  the first label where two patterns differ, a literal beats `*`.

So for `a.b.x.obd.internal`, `*.b.x` beats `a.*.x`. There are never ties. See `dns/hosts.rs`.

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
- **The state files** (in the state dir) are **one file per allocation**, found by scanning:
  - `alloc/<supernet>.toml`;
  - `alloc/<supernet>/<subnet>.toml`, which holds the subnet and its ips.

  Only changed files are written, each atomically (temp file, fsync, rename). State that won't
  parse or doesn't add up stops the daemon from starting rather than being overwritten. "Doesn't
  add up" means a name that doesn't match its filename, or a `<supernet>/` dir without its
  `<supernet>.toml`. `config.toml` is for daemon settings only: so far, the grants on the whole
  tree. Allocations that older versions kept there are moved into `alloc/` automatically.
- **Control socket:** one socket, open to anyone; reads need nothing. For changes, the daemon takes
  the caller's kernel-verified uid (`peer_cred()`), looks up all of that user's groups, and checks
  the **grants** (`obeliskd grant …`): a role (`use` < `manage` < `admin`) for a user, a group or
  everyone, on the whole tree, a supernet or a subnet, covering everything inside it. Root may do
  anything; supernets themselves are root-only.

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
