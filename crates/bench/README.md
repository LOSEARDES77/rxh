# rxh-bench

`rxh-bench` runs the same load balancing scenarios against **rxh**,
**Caddy**, **NGINX** and **Traefik**. For every scenario it reports:

- a correctness verdict (`PASS`, `FAIL` or `n/a`): did the balancer do what
  the policy promises?
- performance numbers: requests per second, p50/p90/p99 latency, errors, and
  how many requests each backend received.

The tool has no external dependencies. The backends and the load generator
are built in, so you don't need `wrk`, `oha` or `hey`.

## Run it

```bash
cargo build --release -p rxh-cli -p rxh-bench
./target/release/rxh-bench --quick                # rxh, plus Caddy if `caddy` is in PATH
./target/release/rxh-bench --quick --caddy-docker # run Caddy from the caddy:2 image
./target/release/rxh-bench --docker               # rxh, Caddy, NGINX and Traefik, all in Docker
./target/release/rxh-bench -s throughput -s cookie -d 10 -c 64 --json results.json
./target/release/rxh-bench --list
```

| Flag | Meaning |
|---|---|
| `--rxh PATH` | rxh binary. The default search order is: next to `rxh-bench`, `target/release/rxh`, `target/debug/rxh`, then `PATH`. |
| `--caddy PATH` | Caddy binary. The default is `caddy` from `PATH`. |
| `--caddy-docker` | When no Caddy binary is found, run Caddy in Docker with `--network host`. The image is pulled if missing. |
| `--caddy-image IMAGE` | Image for `--caddy-docker`. The default is `caddy:2`. |
| `--caddy-l4 PATH` | Caddy built with [caddy-l4](https://github.com/mholt/caddy-l4), for the `tcp` scenario. |
| `--docker` | Run every proxy in Docker with `--network host`, and add NGINX and Traefik. The local rxh binary is mounted into `--rxh-image`, so all four pay the same container cost. |
| `--rxh-image`, `--nginx-image`, `--traefik-image` | Images for `--docker`. The defaults are `debian:stable-slim`, `nginx:stable-alpine` and `traefik:v3`. |
| `--no-rxh`, `--no-caddy`, `--no-nginx`, `--no-traefik` | Skip a target. |
| `-s, --scenario NAME` | Run only this scenario. Repeat the flag to run several. The default is all scenarios. |
| `-d, --duration SECONDS` | Load time per scenario. The default is 5, or 2 with `--quick`. |
| `-c, --concurrency N` | Number of keep-alive client connections. The default is 32, or 16 with `--quick`. |
| `--json FILE` | Also write the results as JSON. |

The exit code is 1 when any scenario fails, so you can use the tool as a
CI smoke test.

A target that is missing, or that cannot run a scenario, appears as `n/a`.
The other target still runs.

## Scenarios

Every scenario starts fresh backends and a fresh proxy. The same setup goes
into an rxh TOML file and an equivalent Caddyfile.

| Scenario | Setup | PASS when |
|---|---|---|
| `throughput` | `round_robin` over 4 backends | No errors, and each backend gets 25% ± 5pp of the requests. |
| `weighted` | `weighted_round_robin` with weights 1:3:2 | Shares stay within 3pp of 1/6, 1/2 and 1/3. |
| `least-conn` | `least_conn`, with one backend 50 ms slower | The slow backend gets less than 20% of the requests (round robin would give it 33%). |
| `ip-hash` | `ip_hash`. Each client connects from its own 127.0.0.x address. | Every IP always reaches the same backend, and at least 2 backends are used. |
| `uri-hash` | `uri_hash` over 64 paths | Every path always reaches the same backend. |
| `header` | `header` policy on `x-user` (64 values) | Every header value always reaches the same backend. |
| `cookie` | `cookie` policy. Each client stores its `Set-Cookie` like a browser session. | Every session always reaches the same backend. |
| `failover` | `round_robin`, 2 retries, passive health (`fail_duration 10s`). One backend is killed after 1/3 of the run. | Client errors stay at or below 1%. |
| `active-health` | `round_robin` with `/health` checked every second. One backend starts answering 503 on `/health`. | No more than one request per client connection reaches that backend 3 seconds after the flip. |
| `tcp` | Layer 4 `round_robin` to 2 TCP echo servers. Each client reconnects every 50 round trips. | No echo errors, and both backends receive connections. Core Caddy has no layer 4 proxy, so this shows `n/a` unless you pass `--caddy-l4`. |

## NGINX and Traefik

NGINX and Traefik run only with `--docker`. They do not have every policy,
so some scenarios show `n/a`:

| rxh policy | NGINX (open source) | Traefik v3 |
|---|---|---|
| `round_robin`, `weighted_round_robin` | default method, `weight=` | default strategy, `weight` |
| `least_conn` | `least_conn` | `strategy: p2c` (power of two choices, the closest relative) |
| `ip_hash` | `hash $remote_addr consistent` (`ip_hash` ignores the last octet) | `strategy: hrw` |
| `uri_hash`, `header` | `hash $request_uri`, `hash $http_x_user` | n/a |
| `cookie` | n/a (NGINX Plus) | `sticky.cookie` |
| retries and passive health | `proxy_next_upstream_tries`, `max_fails` | `retry` middleware, `passiveHealthCheck` |
| active health | n/a (NGINX Plus) | `healthCheck` |
| `tcp` | `stream` module | TCP router |

NGINX runs with `worker_processes auto`, upstream keep-alive and a shared
`zone`.

## Charts

`plot.py` draws requests per second and p99 latency from the JSON file. It
needs Python 3 with matplotlib:

```bash
./target/release/rxh-bench --docker --json docs/bench/results.json > docs/bench/results.md
python3 crates/bench/plot.py docs/bench/results.json docs/bench
```

The latest run is in [docs/benchmarks.md](../../docs/benchmarks.md).

Notes:

- `ip-hash` binds to addresses in 127.0.0.0/8 other than 127.0.0.1. Linux
  supports this out of the box. On macOS, add the aliases first, for example
  `sudo ifconfig lo0 alias 127.0.0.2 up`.
- The backends and the load generator run in the `rxh-bench` process.
  Absolute numbers depend on your machine. Compare the ratios between
  proxies, not the raw values.
- For a fair comparison, run every proxy the same way: all native
  (`--caddy PATH`) or all in Docker (`--docker`).

## Get Caddy

- Binary: download one from <https://caddyserver.com/download>, or install it
  with your package manager (`pacman -S caddy`, `brew install caddy`,
  `apt install caddy`).
- Docker: pass `--caddy-docker`. The tool runs
  `docker run --rm --network host -v <tmp>:/etc/caddy caddy:2 caddy run ...`
  and removes the container when the scenario ends, including on Ctrl-C.
- Layer 4: build Caddy with the plugin using
  `xcaddy build --with github.com/mholt/caddy-l4`, then pass
  `--caddy-l4 ./caddy`.
