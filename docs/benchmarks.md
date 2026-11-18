# Benchmarks

RXH against Caddy, NGINX and Traefik, measured with
[`rxh-bench`](../crates/bench/README.md). Every proxy runs in Docker with
host networking, in front of the same backends, with the same load.

Measured 2026-11-18 on an Intel Core Ultra 7 255H (16 threads), Linux 7.3.
Each scenario runs for 5 seconds with 32 keep-alive connections.

| Proxy | Image |
|---|---|
| rxh 0.2.0 | release binary mounted into `debian:stable-slim` |
| Caddy 2 | `caddy:2` |
| NGINX (open source) | `nginx:stable-alpine` |
| Traefik 3.7 | `traefik:v3` |

## Results

![Requests per second by scenario and proxy](bench/rps.svg)

![p99 latency by scenario and proxy](bench/p99.svg)

Requests per second:

| Scenario | rxh | Caddy | NGINX | Traefik |
|---|---:|---:|---:|---:|
| throughput | 138 548 | 88 232 | 218 945 | 114 217 |
| weighted | 140 674 | 87 920 | 249 433 | 123 621 |
| least-conn | 128 661 | 86 823 | 278 087 | 109 913 |
| ip-hash | 142 265 | 87 235 | 245 292 | 120 854 |
| uri-hash | 114 083 | 84 189 | 233 607 | n/a |
| header | 135 184 | 85 445 | 239 717 | n/a |
| cookie | 112 793 | 79 696 | n/a | 119 561 |
| failover | 142 170 | 88 806 | 190 324 | 110 918 |
| active-health | 136 747 | 87 950 | n/a | 123 563 |
| tcp | 270 016 | n/a | 361 123 | 140 306 |

p99 latency in milliseconds:

| Scenario | rxh | Caddy | NGINX | Traefik |
|---|---:|---:|---:|---:|
| throughput | 0.64 | 1.32 | 0.46 | 0.97 |
| weighted | 0.62 | 1.34 | 0.41 | 0.92 |
| least-conn | 0.52 | 1.04 | 0.28 | 0.72 |
| ip-hash | 0.61 | 1.33 | 0.42 | 0.94 |
| uri-hash | 0.91 | 1.40 | 0.44 | n/a |
| header | 0.65 | 1.33 | 0.43 | n/a |
| cookie | 0.76 | 1.43 | n/a | 0.94 |
| failover | 0.61 | 1.31 | 0.66 | 1.06 |
| active-health | 0.67 | 1.31 | n/a | 0.90 |
| tcp | 0.32 | n/a | 0.31 | 1.31 |

The full report, with the verdict and the share of requests per backend for
each run, is in [bench/results.md](bench/results.md). The raw data is in
[bench/results.json](bench/results.json).

## What the numbers say

- **Correctness**: every scenario a proxy can run passes. No run failed.
- **rxh and Caddy**: rxh serves about 1.5 times the requests of Caddy, with
  about half the p99 latency.
- **rxh and Traefik**: rxh is 10% to 30% faster over HTTP and almost twice
  as fast over TCP. Traefik wins the `cookie` scenario by 6%.
- **rxh and NGINX**: NGINX is the fastest. It serves 1.3 to 2.2 times the
  requests of rxh. Closing this gap is the performance milestone of the
  [roadmap](../ROADMAP.md).
- **Coverage**: rxh is the only proxy of the four that runs all ten
  scenarios. Open source NGINX has no sticky cookies and no active health
  checks. Traefik hashes only the client IP. Core Caddy has no layer 4
  proxy.

## Limits of this benchmark

- The backends and the load generator run in the same process as the
  benchmark, on the same machine as the proxies. They compete for the same
  cores. Read the ratios, not the absolute values.
- One run per scenario. Differences below 10% are noise; one earlier run
  was off by half and went back to normal when repeated.
- Requests are small `GET`s over cleartext HTTP/1.1. TLS, HTTP/2, large
  bodies and many idle connections are not measured yet.
- Traefik's `least-conn` uses `p2c`, and NGINX's `ip-hash` uses
  `hash $remote_addr consistent`. See the
  [mapping table](../crates/bench/README.md#nginx-and-traefik).
- Proxies run with their defaults, apart from upstream keep-alive and
  `worker_processes auto` on NGINX.

## Run it yourself

```bash
cargo build --release -p rxh-cli -p rxh-bench
./target/release/rxh-bench --docker --json docs/bench/results.json > docs/bench/results.md
python3 crates/bench/plot.py docs/bench/results.json docs/bench   # needs matplotlib
```
