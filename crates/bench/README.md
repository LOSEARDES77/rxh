# rxh-bench

`rxh-bench` runs the same load balancing scenarios against **rxh** and
**Caddy**. For every scenario it reports:

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
| `--no-rxh`, `--no-caddy` | Skip one of the two targets. |
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

Notes:

- `ip-hash` binds to addresses in 127.0.0.0/8 other than 127.0.0.1. Linux
  supports this out of the box. On macOS, add the aliases first, for example
  `sudo ifconfig lo0 alias 127.0.0.2 up`.
- The backends and the load generator run in the `rxh-bench` process.
  Absolute numbers depend on your machine. Compare the ratio between rxh and
  Caddy, not the raw values.
- For a fair comparison, use `--caddy PATH` rather than Docker. Even with
  host networking, Docker adds some overhead.

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
