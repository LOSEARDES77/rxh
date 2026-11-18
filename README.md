# RXH: reverse proxy and load balancer for HTTP, TCP and UDP

[![CI](https://github.com/LOSEARDES77/rxh/actions/workflows/ci.yml/badge.svg)](https://github.com/LOSEARDES77/rxh/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-1.100%2B-orange.svg)](https://www.rust-lang.org)

**RXH** is a fast, lightweight **reverse proxy** and **load balancer** written
in Rust. It balances **HTTP/1.1, HTTP/2 and WebSocket** traffic (layer 7) and
**any TCP or UDP protocol** (layer 4): databases, Redis, MQTT, gRPC, game
servers, DNS. It runs as a **daemon** that you manage from a **CLI**: add or
remove backends from a load balancing pool while it runs, without dropping a
connection.

RXH implements every load balancing policy, retry option and health check of
[Caddy](https://caddyserver.com)'s `reverse_proxy`, and adds TCP/UDP
balancing, named pools and live pool management on top. See
[PARITY.md](PARITY.md) for the feature by feature comparison, and
[`rxh-bench`](crates/bench/README.md) to measure both side by side.

```console
$ rxh service add --listen 0.0.0.0:80 --to @web --name web
Added service to /etc/rxh/rxh.toml
Daemon reloaded
$ rxh upstream add web 10.0.0.12:8080
Saved 10.0.0.12:8080 to pool web in /etc/rxh/rxh.toml
Added 10.0.0.12:8080 to pool web on the running daemon
$ rxh status
pool web (least_conn)
  ADDRESS                      WEIGHT  HEALTHY  ACTIVE  REQUESTS FAILS
  10.0.0.11:8080                    1      yes      12     48210     0
  10.0.0.12:8080                    1      yes       9        97     0
```

## Features

- **Layer 7 and layer 4.** HTTP reverse proxy, raw TCP proxy (with optional
  TLS termination and PROXY protocol v1/v2) and UDP proxy with per client
  sessions, all from one binary.
- **12 load balancing policies**, the same as Caddy: `random`,
  `random_choose`, `first`, `round_robin`, `weighted_round_robin` (smooth),
  `least_conn`, `ip_hash`, `client_ip_hash`, `uri_hash`, `query`, `header`,
  and `cookie` sticky sessions. Hash policies use rendezvous hashing, so
  adding a backend only moves the keys it takes over.
- **Health checks.** Active HTTP checks (path, port, method, headers, status,
  body regex) or TCP connect checks, plus passive checks on real traffic
  (failed dials, 5xx, slow responses, concurrency limits).
- **Retries and failover**: `retries`, `try_duration`, `try_interval`.
  Failed dials and stale keep-alive connections move to another backend, and
  `GET` requests are retried after a failed round trip.
- **Daemon and CLI**: `rxh start | stop | reload | status`, and
  `rxh upstream | pool | service` to manage peers, pools and listeners. The
  CLI edits the TOML file without losing your comments, validates it, then
  applies the change live.
- **Zero downtime reloads** (`SO_REUSEPORT`). An invalid configuration is
  rejected and the running one keeps serving.
- **Admin API** on `127.0.0.1:2019` (JSON), with Caddy compatible
  `/reverse_proxy/upstreams`.
- **HTTP done right**: keep-alive pools to backends, h2/h2c, WebSocket
  tunnels, hop-by-hop header stripping, `X-Forwarded-*` with
  `trusted_proxies`, RFC 7239 `Forwarded`, `Via`, header rules and a response
  cache.
- **Also**: static file server with directory listing, TLS with ALPN,
  connection limits, graceful shutdown with `grace_period`.

## Benchmarks

`rxh-bench` starts the same backends behind rxh, Caddy, NGINX and Traefik,
runs every scenario on each and checks that each policy does what it
promises. With every proxy in Docker (32 keep-alive connections, Intel Core
Ultra 7 255H), rxh serves about 1.5 times the requests of Caddy, is 10% to
30% faster than Traefik, and reaches about half the throughput of NGINX. It
is the only one of the four that runs all ten scenarios.

![Requests per second by scenario and proxy](docs/bench/rps.svg)

Tables, latency and the limits of the measurement are in
[docs/benchmarks.md](docs/benchmarks.md). Run it yourself:

```bash
cargo build --release -p rxh-cli -p rxh-bench
./target/release/rxh-bench --docker          # all four proxies in Docker
./target/release/rxh-bench --quick --caddy "$(which caddy)"   # rxh and a native Caddy
```

## Install

From source, with Rust 1.100 or newer:

```bash
cargo install --git https://github.com/LOSEARDES77/rxh rxh-cli   # installs `rxh`
```

Or build the repository:

```bash
git clone https://github.com/LOSEARDES77/rxh && cd rxh
cargo build --release        # binary at target/release/rxh
```

Docker and systemd files are in [`dist/`](dist).

## Quick start

1. Write a configuration, for example `rxh.toml`:

   ```toml
   [upstream.web]
   policy = "least_conn"
   backends = ["127.0.0.1:3001", "127.0.0.1:3002"]
   health = { uri = "/health", interval = "5s" }
   passive = { fail_duration = "30s", unhealthy_status = ["5xx"] }

   [[server]]
   listen = "0.0.0.0:8080"
   forward = "@web"

   [[server]]
   listen = "0.0.0.0:6379"
   protocol = "tcp"
   forward = { policy = "first", backends = ["10.0.0.1:6379", "10.0.0.2:6379"] }
   ```

2. Run it in the foreground, or as a daemon:

   ```bash
   rxh run -c rxh.toml        # foreground
   rxh start -c rxh.toml      # background, PID file in /tmp/rxh/rxh.pid
   ```

3. Manage it while it runs:

   ```bash
   rxh status                                   # listeners, pools, health
   rxh upstream add web 127.0.0.1:3003 -w 2     # add a peer (live + saved)
   rxh upstream rm web 127.0.0.1:3001           # drain a peer
   rxh pool add db --policy first -b 10.0.0.1:5432 -b 10.0.0.2:5432 --health-interval 5s
   rxh service add --listen 0.0.0.0:5432 --protocol tcp --to @db --name postgres
   rxh reload                                   # after editing the file by hand
   rxh stop
   ```

More configurations: [`examples/`](examples), including
[every option](examples/full.toml), an
[HTTP load balancer with sticky sessions](examples/http-load-balancer.toml)
and [TCP/UDP balancing](examples/tcp-udp.toml). The full reference is in
[docs/configuration.md](docs/configuration.md).

## CLI

| Command | What it does |
|---|---|
| `rxh run` | Run in the foreground (default command). |
| `rxh start` / `stop` / `reload` | Daemon control through the admin API, with PID file and signal fallbacks. |
| `rxh status [--json]` | Listeners, pools, health and request counters. |
| `rxh validate` | Check a configuration file. |
| `rxh upstream list\|add\|rm` | Manage the backends (peers) of a pool, live and in the file. `--no-save` changes only the running daemon. |
| `rxh pool list\|add\|rm` | Manage `[upstream.NAME]` pools. |
| `rxh service list\|add\|rm` | Manage `[[server]]` listeners (HTTP, TCP, UDP, files). |
| `rxh policies` | List load balancing policies. |

Global flags: `-c/--config` (`RXH_CONFIG`), `--admin` (`RXH_ADMIN`),
`--pid-file`.

## Admin API

JSON over HTTP on `[admin] listen` (default `127.0.0.1:2019`). Requests with
a non-loopback `Host` or `Origin` are rejected.

| Method | Path | Action |
|---|---|---|
| `GET` | `/status` | Version, uptime, listeners, pools |
| `GET` | `/config` | Running configuration with live pool members |
| `GET` | `/pools`, `/pools/{name}` | Members, health, in-flight and total requests |
| `GET` | `/reverse_proxy/upstreams` | Caddy compatible upstream list |
| `POST` | `/pools/{name}/upstreams` | Add `{"address": "host:port", "weight": 1}` |
| `DELETE` | `/pools/{name}/upstreams` | Remove `{"address": "host:port"}` |
| `POST` | `/reload` | Reload the configuration file |
| `POST` | `/stop` | Graceful shutdown |

```bash
curl -s localhost:2019/pools/web | jq
curl -s -X POST localhost:2019/pools/web/upstreams -d '{"address":"10.0.0.13:8080"}'
```

## Project layout

RXH is a Cargo workspace:

| Crate | Role |
|---|---|
| [`crates/core`](crates/core) (`rxh-core`) | Configuration model and the load balancing engine: policies, pools, health state. No I/O. |
| [`crates/server`](crates/server) (`rxh-server`) | Runtime on Tokio and Hyper: listeners, HTTP proxy, TCP/UDP proxy, health checks, admin API, daemon. |
| [`crates/cli`](crates/cli) (`rxh-cli`) | The `rxh` binary: daemon and CLI. |
| [`crates/bench`](crates/bench) (`rxh-bench`) | Correctness and performance comparison against Caddy, NGINX and Traefik. |

## Development

```bash
cargo fmt --all
cargo clippy --workspace --all-targets
cargo test --workspace
./target/release/rxh-bench --quick --no-caddy   # smoke test of every policy
```

Feature progress against Caddy is tracked in [PARITY.md](PARITY.md). The plan
for the next versions is in [ROADMAP.md](ROADMAP.md).

## License

[MIT](LICENSE). RXH started as a learning project by
[Antonio Sarosi](https://github.com/antoniosarosi/rxh).
