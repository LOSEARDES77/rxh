# Benchmarks

RXH against Caddy, NGINX and Traefik, measured with
[`rxh-bench`](../crates/bench/README.md). Every proxy runs in Docker with
host networking, in front of the same backends, with the same load.

Measured 2026-11-21 on an AMD Ryzen 9 7950X (32 threads), Linux 7.3, in one
`rxh-bench --docker` run of every scenario. Each scenario runs for 5 seconds
with 32 keep-alive connections, and `idle` and `memory` hold 5000 idle
connections.

| Proxy | Image |
|---|---|
| rxh 0.3.0 (unreleased) | release binary mounted into `debian:stable-slim` |
| Caddy 2 | `caddy:2` |
| NGINX (open source) | `nginx:stable-alpine` |
| Traefik 3.7 | `traefik:v3` |

## Results

![Requests per second by scenario and proxy](bench/rps.svg)

![p99 latency by scenario and proxy](bench/p99.svg)

The charts leave out `large-body` and `memory`, whose numbers are in
[their own section](#more-scenarios-tls-http2-bodies-websocket-udp-idle-connections).

Requests per second:

| Scenario | rxh | Caddy | NGINX | Traefik |
|---|---:|---:|---:|---:|
| throughput | 216 776 | 105 769 | 309 489 | 160 116 |
| weighted | 215 578 | 107 980 | 292 459 | 159 188 |
| least-conn | 197 727 | 103 843 | 283 299 | 137 823 |
| ip-hash | 214 818 | 106 384 | 322 955 | 159 550 |
| uri-hash | 215 685 | 106 686 | 312 504 | n/a |
| header | 213 364 | 105 721 | 321 176 | n/a |
| cookie | 215 534 | 95 659 | n/a | 158 070 |
| failover | 213 369 | 108 000 | 297 968 | 148 932 |
| active-health | 217 008 | 107 415 | n/a | 161 176 |
| tcp | 276 361 | n/a | 334 747 | 228 753 |

p99 latency in milliseconds:

| Scenario | rxh | Caddy | NGINX | Traefik |
|---|---:|---:|---:|---:|
| throughput | 0.39 | 0.82 | 0.19 | 0.58 |
| weighted | 0.40 | 0.80 | 0.22 | 0.59 |
| least-conn | 0.28 | 0.58 | 0.17 | 0.46 |
| ip-hash | 0.39 | 0.82 | 0.17 | 0.59 |
| uri-hash | 0.40 | 0.82 | 0.18 | n/a |
| header | 0.41 | 0.83 | 0.18 | n/a |
| cookie | 0.39 | 0.88 | n/a | 0.60 |
| failover | 0.39 | 0.80 | 0.33 | 0.61 |
| active-health | 0.39 | 0.81 | n/a | 0.58 |
| tcp | 0.31 | n/a | 0.18 | 0.50 |

The full report, with the verdict and the share of requests per backend for
each run, is in [bench/results.md](bench/results.md). The raw data is in
[bench/results.json](bench/results.json).

## More scenarios: TLS, HTTP/2, bodies, WebSocket, UDP, idle connections

Same run. The scenarios are described in the
[rxh-bench README](../crates/bench/README.md#scenarios).

Requests (or echo round trips) per second, and p99 latency in milliseconds:

| Scenario | rxh | Caddy | NGINX | Traefik |
|---|---:|---:|---:|---:|
| tls | 203 700 / 0.40 | 99 865 / 0.85 | 312 769 / 0.19 | 155 378 / 0.60 |
| http2 | 159 387 / 0.40 | 75 768 / 1.07 | 164 666 / 0.32 | 109 140 / 0.71 |
| large-body (1 MiB each way) | 1 955 / 25.67 | 2 549 / 20.89 | 1 114 / 49.02 | 2 631 / 19.63 (FAIL) |
| websocket | 256 788 / 0.35 | 184 669 / 0.56 | 314 435 / 0.23 | 193 875 / 0.53 |
| udp | 239 114 / 0.22 | n/a | 234 184 / 0.34 | 193 351 / 0.46 |
| idle (load next to 5000 idle connections) | 218 128 / 0.48 | 126 900 / 1.18 | 321 637 / 0.18 | 154 387 / 0.59 |

Memory per idle keep-alive connection (RSS growth over 5000 connections,
after 100 warm-up connections):

| | rxh | Caddy | NGINX | Traefik |
|---|---:|---:|---:|---:|
| Per connection | 35.7 KiB | 33.9 KiB | 0.6 KiB | 36.0 KiB |
| RSS with 100 connections | 19.9 MiB | 68.1 MiB | 235.8 MiB | 112.7 MiB |
| RSS with 5100 connections | 194.3 MiB | 233.7 MiB | 238.7 MiB | 288.7 MiB |

NGINX's RSS is summed over its master and one worker per core (32 here),
which is why it starts high; its per connection cost is the lowest by far.

- **rxh passes all seven.** Traefik failed `large-body` again: 8 of ~6 600
  response bodies were cut off mid-stream. The same backends and client
  pass through the three other proxies.
- **rxh is slower with large bodies**: 2.0 GB/s each way against 2.5 for
  Caddy and 2.6 for Traefik. NGINX buffers request bodies to disk by
  default and comes last.
- **HTTP/2 and UDP close the gap with NGINX**: rxh serves 0.97x NGINX's
  requests over HTTP/2 and matches it on UDP, against 0.65x for HTTP/1.1
  over TLS.
- **Memory**: rxh starts the smallest and holds an idle connection in about
  what Caddy and Traefik take, but about 60 times more than NGINX. Idle
  buffers per connection are a target for the performance milestone.

## What the numbers say

- **Correctness**: rxh passes every scenario. The only failure of the run is
  Traefik's `large-body`.
- **rxh and Caddy**: rxh serves about twice the requests of Caddy, with
  about half the p99 latency.
- **rxh and Traefik**: rxh is 35% to 45% faster over HTTP, 20% faster over
  TCP and 45% faster over HTTP/2. It now also wins the `cookie` scenario.
- **rxh and NGINX**: NGINX is still the fastest over HTTP/1.1, with 1.4 to
  1.5 times the requests of rxh, and 1.2 times over TCP. Closing this gap
  is the performance milestone of the [roadmap](../ROADMAP.md). Over HTTP/2 and UDP they are even.
- **Coverage**: rxh is the only proxy of the four that runs every
  scenario. Open source NGINX has no sticky cookies and no active health
  checks. Traefik hashes only the client IP. Core Caddy has no layer 4
  proxy.

## Limits of this benchmark

- The backends and the load generator run in the same process as the
  benchmark, on the same machine as the proxies. They compete for the same
  cores. Read the ratios, not the absolute values.
- One run per scenario. Differences below 10% are noise; one earlier run
  was off by half and went back to normal when repeated.
- In the first table, requests are small `GET`s over cleartext HTTP/1.1.
  TLS, HTTP/2, large bodies, WebSocket, UDP and idle connections are in
  their own section. TLS clients keep their connection, so handshakes are
  not measured.
- Traefik's `least-conn` uses `p2c`, and NGINX's `ip-hash` uses
  `hash $remote_addr consistent`. See the
  [mapping table](../crates/bench/README.md#nginx-and-traefik).
- Proxies run with their defaults, apart from upstream keep-alive,
  `worker_processes auto`, `client_max_body_size 0` and
  `keepalive_requests 1000000` on NGINX, and rxh's `max_connections`
  raised above the 5000 idle connections.

## History across releases

Every release is measured with `rxh-bench --docker` on the same machine, and
the raw JSON is kept in [bench/history](bench/history/README.md), one
`<version>.json` per release next to a `<version>.md` with the machine, the
commit and the full report. The history page shows rxh's requests per second
and p99 latency for every scenario and release, and marks in bold every
change of more than 10% for the worse against the previous release (single
runs differ by up to 10%).

To add a release, check out its tag on the benchmark machine and run:

```bash
crates/bench/release.sh            # version from Cargo.toml, or pass it: release.sh 0.3.0
```

The script builds the release binaries, runs `rxh-bench --docker`, writes
`docs/bench/history/<version>.json` and `<version>.md`, regenerates
`docs/bench/history/README.md` with `rxh-bench --history docs/bench/history`
and prints the regressions. It refuses to replace an existing version unless
`FORCE=1`, warns when `HEAD` is not tagged `v<version>` or the tree has
uncommitted changes, and exits with 1 when rxh fails a scenario (the results
are kept). Extra arguments go to `rxh-bench`, for example `-s throughput`.
Commit the three files.

Numbers from different machines are not comparable: the history lists the
CPU of every release and warns when they differ. The 0.2.0 entry was
measured on an Intel Core Ultra 7 255H, so its numbers are not comparable
with the run above.

## Policy micro benchmarks

`crates/core/benches/policies.rs` measures `Pool::select`, the backend choice
made for every request, with [Criterion](https://docs.rs/criterion) for every
policy, with 4 and 64 healthy backends. The request carries every key a
policy reads (IPs, path, query, header), and `cookie` sends back the cookie
of a first selection, so it measures the sticky path.

```bash
cargo bench -p rxh-core --bench policies
cargo bench -p rxh-core --bench policies -- cookie   # one policy
```

`cargo test` runs each benchmark once without timing, which checks that every
policy picks a backend and that a valid cookie keeps its backend.

Median time per selection, 0.3.0 on an AMD Ryzen 9 7950X (one thread):

| Policy | 4 backends | 64 backends |
|---|---:|---:|
| `random` | 20 ns | 138 ns |
| `random_choose` | 31 ns | 207 ns |
| `first` | 16 ns | 16 ns |
| `round_robin` | 18 ns | 18 ns |
| `weighted_round_robin` | 19 ns | 20 ns |
| `least_conn` | 18 ns | 139 ns |
| `ip_hash` | 93 ns | 232 ns |
| `client_ip_hash` | 92 ns | 233 ns |
| `uri_hash` | 42 ns | 181 ns |
| `query` | 33 ns | 170 ns |
| `header` | 40 ns | 178 ns |
| `cookie` | 38 ns | 57 ns |

The rotating policies and `first` cost the same at any size. The others look
at every member, so they grow with the pool: `random` draws a random number
per member (reservoir sampling), and `ip_hash` and `client_ip_hash` also
format the IP as a string before hashing. Even the slowest, a hash over 64
backends, stays far below the microseconds of a proxied request.

## Run it yourself

```bash
cargo build --release -p rxh-cli -p rxh-bench
./target/release/rxh-bench --docker --json docs/bench/results.json > docs/bench/results.md
python3 crates/bench/plot.py docs/bench/results.json docs/bench   # needs matplotlib
```
