# Overview

obeliskd is a small DNS server written by hand in Rust. It answers a few exact-match rules itself
(e.g. `my.nested.test → 127.0.0.1`), and either forwards everything else upstream or only ever sees
the suffixes the OS routes to it. Hand-rolling the protocol is part of the point: prefer
understanding the wire format over pulling in a DNS framework.

## Current state

Scaffolding only: the `obeliskd` CLI has `hello` (calls `obeliskd::print_hello()`) and
`completions <shell>`. No networking yet.

## First milestone

Linux-only UDP server on `127.0.0.1:5353` with one hardcoded rule:

- answers `A` for the rule;
- answers `AAAA` with an empty `NOERROR` (not `NXDOMAIN`, which breaks some clients);
- everything else: forward upstream, or `REFUSED` if it ends up answer-only.

Wire it up via a systemd-resolved routing domain (`~test`, default-route off) and verify with
`resolvectl query` and `curl`.

## Open design questions

Nothing below is decided. Don't silently pick an answer in code; raise it.

- Answer-only (OS routing domains) vs a full forwarder, or both.
- Hand-rolled wire format vs `hickory-proto` (leaning hand-rolled).
- One control socket with peer-credential (uid) checks vs two sockets split by file permissions.
- Rule format and storage (static file, in-memory with live edits, persistence).
- When to add a TCP listener (needed for truncated replies if forwarding).
- Cert helpers in v1? Name-constrained local CA vs ACME DNS-01 on an owned domain.
- Whether obeliskd installs the OS routing itself or documents it as manual setup.

These are tracked in the Aletheia knowledge graph under component `OBLSK:obeliskd`.
