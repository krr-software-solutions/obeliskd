# obeliskd

Local-dev networking without the mess: obeliskd hands out IP addresses for your services and
clusters, and answers DNS for names under `obd.internal` that point at them.

## Motivation

Have you ever had a setup like this?

```text
app-1
├── local-dev
│   ├── db        : psql -h localhost -p 3110
│   ├── service-1 : http://localhost:3111
│   └── service-2 : http://localhost:3112
└── local-stable
    ├── db        : psql -h localhost -p 3120
    ├── service-1 : http://localhost:3121
    └── service-2 : http://localhost:3122
app-2
├── k3d cluster   : http://*.localhost:8081    (every *.localhost is just 127.0.0.1)
│   ├── web, api  : the ingress tells them apart by Host header: fine, for HTTP
│   └── postgres  : psql -h localhost -p 5433   (no Host header in TCP: one more port-forward)
└── redis         : localhost:6380             (6379 was taken by app-1's)
```

…and a spreadsheet, or a README, or your memory, of which port is which? Every config hardcodes
its port. Two apps can't both use `5432`. Cookies set by `localhost:3111` show up at
`localhost:3121`, since cookies don't care about ports. An ingress or gateway routing by name is
great, but when every `*.localhost` name is the same `127.0.0.1`, it only gets you so far: HTTP
can sometimes get by on the Host header, but anything over plain TCP (databases, caches, queues)
needs a port of its own, and a mapping, and a number to remember. And the day something needs a real
hostname (OAuth callbacks, a service calling another by name), you're editing `/etc/hosts`.

The same setup, with obeliskd:

```text
app-1
├── local-dev
│   ├── db        : psql -h db.dev.app-1.obd.internal
│   ├── service-1 : http://service-1.dev.app-1.obd.internal:8080
│   └── service-2 : http://service-2.dev.app-1.obd.internal:8080
└── local-stable
    ├── db        : psql -h db.stable.app-1.obd.internal
    ├── service-1 : http://service-1.stable.app-1.obd.internal:8080
    └── service-2 : http://service-2.stable.app-1.obd.internal:8080
app-2
├── k3d cluster   : http://*.app-2.obd.internal   (the ingress, on its own MetalLB-pinned ip)
│   └── postgres  : psql -h pg.app-2.obd.internal  (a LoadBalancer Service with its own ip: plain 5432)
└── redis         : redis.app-2.obd.internal       (plain 6379)
```

Every service gets its own ip, so each keeps its default port, and its own name, so nothing
collides, nothing is shared by accident, and configs read like what they point at.

### Parallel work, and agents

With agentic development, it's normal for several agents to work on the same app at once, each on
its own feature, each needing the whole stack running. Two agents in `app-1` can't share
`local-dev` (one migrates the db out from under the other), and a fixed `local-dev` /
`local-stable` split doesn't stretch to five features in flight. Neither does a port ladder.

So being able to spin up a complete, isolated environment in seconds, and throw it away just as
fast, is a must:

```text
app-1
├── feat-login    (agent 1)
│   ├── db        : psql -h db.feat-login.app-1.obd.internal
│   └── service-1 : http://service-1.feat-login.app-1.obd.internal:8080
└── feat-billing  (agent 2)
    ├── db        : psql -h db.feat-billing.app-1.obd.internal
    └── service-1 : http://service-1.feat-billing.app-1.obd.internal:8080
```

With obeliskd that's a tag per environment:

```sh
env=feat-login
for svc in db service-1; do
  obeliskd ip alloc --supernet obd --subnet app-1 --ip "$svc-$env" --tag "$env"
  obeliskd host alloc --host "$svc.$env.app-1.obd.internal" --supernet obd --subnet app-1 --ip "$svc-$env"
done
# …start each service bound to its ip, on its default port…
obeliskd ip dealloc --tag "$env" --recursive   # gone: every ip and name in the environment
```

More generally, running more than a toy locally means fighting the network:

- **Everything wants `127.0.0.1`.** Two Postgres instances, three apps on `:8080`, a k3d cluster's
  load balancer: you juggle ports, and every config hardcodes them.
- **Clusters and containers get whatever subnet Docker picks.** Addresses change between runs,
  and MetalLB needs a range you chose yourself, one that doesn't overlap anything else.
- **Names mean `/etc/hosts`.** Edited by hand, with sudo, no wildcards, and stale entries that
  outlive whatever they pointed at.

## Solution overview

obeliskd is a small daemon (written in Rust, DNS wire format and all) that owns a slice of private
address space and the DNS zone `obd.internal`:

```text
obd               10.57.0.0/16           a supernet: the range obeliskd hands out from
├── k3d-dev       10.57.0.0/22   docker  a subnet → Docker network obd-obd-k3d-dev (for k3d / MetalLB)
└── host          10.57.4.0/24   host    a subnet → every address bindable on this machine
    └── web       10.57.4.1              an ip  ← app.obd.internal, *.app.obd.internal
```

- **Allocation:** supernets → subnets (`host` or `docker`) → named ips. obeliskd always picks
  the addresses, and makes them real: host subnets are local to this machine, so any of their
  ips can be bound straight away, and docker subnets become Docker networks.
- **DNS:** hostnames (wildcards too) point at ips. The OS sends only `obd.internal` queries to
  obeliskd (systemd-resolved on Linux, `/etc/resolver` on macOS), so nothing else changes.
- **No config files:** everything goes through the `obeliskd` CLI. The daemon keeps its state,
  and re-applies routes, networks and DNS routing at every boot.
- **Sharing:** reads are open to everyone, and changes need root or a grant (a role on the whole
  tree, a supernet or a subnet, for a user, a group or everyone).
- **Scripts:** `ip resolve` / `host resolve` print just an address, with exit codes. `supernet
  status` / `subnet status` draw what's taken.

## Installing

Linux (x86_64, aarch64; the service needs systemd) and macOS:

```sh
curl -fsSL https://github.com/krr-software-solutions/obeliskd/releases/latest/download/install.sh | sh
obeliskd manage status   # installed, running, and resolving through the system resolver?
```

It downloads and verifies the binary, then runs `sudo obeliskd manage install`, which:
- puts the binary in `/usr/local/bin`;
- installs shell completions for bash, zsh and fish under `/usr/local/share`;
- starts a systemd/launchd service, now and at every boot.

`obeliskd manage` also has `start`, `stop`, `restart`, `enable`, `disable`, `logs` and `uninstall
[--purge]`.

With Rust instead: `cargo install obeliskd-cli`, then `sudo "$(command -v obeliskd)" manage
install`. sudo's PATH usually doesn't include `~/.cargo/bin`.

**Shell completions** load on their own from those system directories. If your shell doesn't pick
them up (e.g. zsh without `compinit`, or a non-standard setup), add the line for your shell to its
rc file instead:

```sh
source <(obeliskd completions bash)    # ~/.bashrc
source <(obeliskd completions zsh)     # ~/.zshrc, after compinit
obeliskd completions fish | source     # ~/.config/fish/config.fish
```

## Setup

Allocate some space, then name things in it. Changes need root, until you grant yourself a role:

```sh
sudo obeliskd supernet alloc --supernet obd --cidr 10.57.0.0/16
sudo obeliskd grant add --role manage --user "$USER" --supernet obd   # from here on, no sudo for obd

# A Docker network for a k3d cluster: Docker assigns the first /24, obeliskd the rest (MetalLB, --ip)
obeliskd subnet alloc --supernet obd --subnet k3d-dev --kind docker --size 22 --reserve-size 24
k3d cluster create dev --network obd-obd-k3d-dev

# Addresses on this machine, and names for them
obeliskd subnet alloc --supernet obd --subnet host --kind host --size 24
obeliskd ip alloc --supernet obd --subnet host --ip web
obeliskd host alloc --host app.obd.internal --supernet obd --subnet host --ip web
obeliskd host alloc --host '*.app.obd.internal' --supernet obd --subnet host --ip web

python3 -m http.server --bind "$(obeliskd ip resolve --supernet obd --subnet host --ip web)" 8080 &
curl http://api.app.obd.internal:8080/

obeliskd supernet status   # what's allocated, as a grid
obeliskd subnet status     # the same, per subnet, down to the ips
```

Every command takes `--help`, and `--format json` where its output is data.

## Development

| Path                | Crate          | What                                  |
|---------------------|----------------|---------------------------------------|
| `code/obeliskd`     | `obeliskd`     | the library                           |
| `code/obeliskd-cli` | `obeliskd-cli` | the CLI / daemon (binary: `obeliskd`) |

```sh
cargo build
sudo ./target/debug/obeliskd daemon run      # needs root: it manages routes and docker networks
./target/debug/obeliskd daemon health        # in another shell
cargo test --workspace
```

Requires Rust 1.85+ (edition 2024). More in [`CLAUDE.md`](CLAUDE.md) and [`.claude/docs/`](.claude/docs/).

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT),
at your option.
