# RXH test report

Tested 2026-11-24 on Linux amd64 at `7bc47e8`, with Rust 1.100.0, Bun 1.4.2, Chromium 158, and the Kiri 0.2.2 Docker-compatible engine.

## Confirmed bugs

### High · Successful configuration reloads reset some client connections

A daemon proxying to a live HTTP backend returned HTTP 200 for all 75 `POST /reload` calls. With 12 clients opening fresh connections throughout the reloads, 67 of 812 parallel requests failed with `ConnectionResetError`. One of 75 requests sent immediately after each successful reload also reset. The rest returned HTTP 200. The source-built Debian container reproduced the failure: all 75 reloads returned HTTP 200 and 5 of 583 parallel proxy connections reset. The native ARM64 build on the VPS also reproduced it with a static-file listener and no upstream: all 75 reloads returned HTTP 200 and 150 of 1,853 parallel requests reset. With the same ARM test in the source-built Debian container, 58 of 1,750 parallel requests and 3 of 75 immediate requests reset.

Reproduce with an isolated HTTP listener forwarding to a healthy local backend. Start 12 clients repeatedly fetching `/` over fresh connections, then repeatedly POST to the admin `/reload` endpoint while keeping the configuration unchanged. Check for socket resets on the proxy listener even when the admin endpoint reports success.

Two existing tests independently observed the same symptom in the first workspace run and passed on isolated reruns.

- `cargo test --locked -p rxh-server --test config_edit a_quoted_pool_name_is_edited -- --exact` saw a reset while checking that a successful pool edit had taken effect. [Test](crates/server/tests/config_edit.rs#L1022)
- `cargo test --locked -p rxh-server --test http3 alt_svc_follows_reloads -- --exact` saw a reset on the HTTPS connection used to check the Alt-Svc header after reload. [Test](crates/server/tests/http3/main.rs#L966)

The reload code starts a new generation and schedules shutdown of the previous one. On Unix, TCP listeners use `SO_REUSEPORT`. A connection queued on the old listener as it closes is one possible explanation. The tests establish the client-visible failure. [Reload handoff](crates/server/src/daemon.rs#L831) [TCP listener](crates/server/src/server.rs#L696)

### Medium · The Compose demo silently shows an empty list when its API fails

Start the example with `docker compose up -d`, stop its API service with `docker compose stop api`, and reload `http://localhost:8080/` in Chromium. The page and heading load, but the list is empty and the page shows no error. The request to `/api/todos` returns HTTP 502 and Chromium reports an uncaught promise rejection when the page parses the text response as JSON. The HTTP 503 JSON error observed during the initial database failure would also fail in the page because it expects an iterable array. The page should show an error when it cannot load the list and reserve the empty state for a successful empty response. [Demo script](docker-compose.yml#L98)

### Medium · The container documentation gives the wrong base directory for listener TLS files

Mount a configuration and its certificate files together under `/etc/rxh` in the Debian image. Set a listener's `tls = { cert = "server.pem", key = "server.key" }` and run RXH with the image's default working directory. RXH exits with `server.pem / server.key: No such file or directory (os error 2)` even though both files are next to the configuration. Starting the native binary from the configuration directory works. Using absolute `/etc/rxh/...` paths in the container also works. The image documentation should clarify which paths use the working directory and which use the configuration directory. [Container path guidance](docs/configuration.md#L521)

### Low · Browser requests for the site icon return errors

Chromium reports `GET /favicon.ico` as HTTP 404 on the native dashboard and on the Compose demo. On the token-protected Docker dashboard it reports HTTP 401. Dashboard navigation and the API-backed demo both work. [Dashboard document](crates/server/ui/index.html) [Compose document](docker-compose.yml#L92)

### Low · The fuzz workspace cannot build with its checked-in lockfile

Run `cargo check --locked --manifest-path fuzz/Cargo.toml` from the repository root. Cargo exits with an error saying it needs to update `fuzz/Cargo.lock`. Checking a temporary copy without `--locked` refreshed the lockfile and built the fuzz workspace successfully. [Fuzz manifest](fuzz/Cargo.toml) [Lockfile](fuzz/Cargo.lock)

## Environment and startup observations

- On the first Compose launch, the PostgreSQL health check passed, then PostgreSQL shut down with `shutting down due to startup process failure`. PostgREST subsequently returned HTTP 503 both directly and through RXH. Restarting `db` let PostgreSQL recover, and both todo rows loaded without changing the Compose configuration. The configured `pg_isready` uses a local socket, so it can also report readiness while the image's initialization server is running. The cause of the PostgreSQL shutdown remains unverified. [Health check](docker-compose.yml#L54)
- The initial Cargo run exhausted available disk space while linking debug test binaries. The run completed after moving build output to a temporary directory and disabling debug information and incremental compilation.

## Verification

| Area | Result |
| --- | --- |
| Rust workspace | `cargo test --workspace --locked --no-fail-fast` produced 1,071 passes, 2 failures, and 4 ignored tests on the first complete run. Both failing tests passed in isolation. A second full run passed all 1,073 enabled tests, with 4 ignored. |
| Rust coverage | 93.89% line coverage excluding `crates/bench`, above the 85% CI threshold. |
| Rust formatting and lint | `cargo fmt --all --check` and `cargo clippy --workspace --all-targets --locked -- -D warnings` passed. |
| Web UI build and tests | Frozen Bun install, type check, all 197 tests across 21 files, and production build passed. Bun audit reported zero vulnerabilities. Rebuilding left the committed UI bundle unchanged. |
| Native CLI and admin API | Validated the default, include, HTTP load-balancer, and TCP/UDP configs. Checked status and effective configuration as JSON, schema generation, policy listing, HTTP forwarding, and Prometheus metrics. |
| Native dashboard | Chromium loaded all six tabs. Initial account setup, password change, sign-out, host and pool creation/deletion in TOML mode, certificate reload, configuration reload, user and role creation/deletion, chart range selection, raw TOML validation and persistence, and confirmed graceful stop were exercised. |
| Docker dashboard | Chromium loaded all six tabs against the registry image and the Debian image built from this checkout. HTTPS token entry, password sign-in and sign-out, host and pool creation/deletion, certificate and configuration reload, user and role creation/deletion, chart range selection, and raw TOML validation and persistence were exercised with writable configuration storage. |
| Compose example | nginx home page, PostgREST rewrite, PostgreSQL seed rows, and query filtering worked after database recovery. A fresh stack using the source-built Debian image also returned both rows. Chromium rendered both items, and a separate test reproduced the API failure display issue. |
| Proxy benchmark | Using the existing release binaries, all 17 quick correctness scenarios passed natively and with RXH mounted into a Debian container by `rxh-bench --docker`. These covered TCP, UDP, TLS, HTTP/2, WebSocket, load balancing, health checks, failover, large bodies, and idle connections. A separate Docker comparison of throughput, TLS, and WebSocket passed for RXH, Caddy, nginx, and Traefik, with JSON export. |
| Release staging | Staged and archived the native executable, default configuration, man pages, shell completions and systemd files. The staged executable validated its bundled configuration. |
| Docker image builds | Debian, Alpine, and Distroless built from the checkout. All three ran RXH. Alpine and Distroless returned the configured HTTP redirect. The separate `musl-binary` target exported a statically linked executable that ran and validated the default configuration. |
| TLS protocol smoke | Real curl requests returned HTTP 200 over HTTP/1.1, HTTP/2 and HTTP/3 natively and through the source-built Debian image using separate TCP and UDP port mappings. |
| Docker label discovery | A labeled nginx backend was added and removed as the container started and stopped, both from native RXH and from containerized RXH. The container test used host networking and root to access the Kiri socket after the group-only socket mount failed. A hostless file route took precedence in the first attempt, consistent with first-match routing. |
| Fuzzing | All five targets completed 60-second runs with no crash using nightly and a refreshed lockfile in a temporary copy. |
| ACME | All three normally ignored end-to-end ACME tests passed using a Dockerized Pebble CA: HTTP-01, TLS-ALPN-01, and wildcard DNS-01 via RFC 2136. |

The automated Rust suites also exercised HTTP/3, gRPC, TLS and mTLS, routing, static files, authentication, OIDC, permissions, rate limits, retries, traffic splitting, mirroring, and observability using local fixtures. The browser pass covered representative interactions in each tab. External provider deployments, every browser form combination, and non-Linux platforms remain outside the verified matrix. The manually operated OIDC provider fixture that runs indefinitely stayed ignored. The initial review changed only this report.

## ARM64 VPS follow-up

Retested `7bc47e8` on an Ubuntu ARM64 VPS. Chromium ran locally against the VPS over SSH forwarding. The VPS test endpoints were reachable via loopback and no system hostname mappings were needed.

- The Compose example started successfully with published ARM64 image `ghcr.io/loseardes77/rxh:0.3` (image revision `6fb18d2`). Chromium displayed both rows, and API query filtering worked. Stopping PostgREST again reproduced the blank list and the unhandled JSON parse error. PostgreSQL stayed healthy on this run.
- Chromium reached all six dashboard tabs on the published ARM64 image. HTTPS token entry, host and pool creation and deletion, certificate and configuration reload, and user creation and deletion passed. The `/favicon.ico` error reproduced.
- The Web UI type check passed, but the Bun-only `oven/bun:1.4.2` image could not run any Vitest files. Its `node` command is Bun's fallback shim, and Vitest's fork workers fail with `TypeError: child.channel?.unref is not a function`. This is a test-tooling compatibility issue observed on the VPS. With Node.js 24.22.3 and the same Bun-installed dependencies, type checking, all 197 tests across 21 files, and the production bundle passed.
- The published Alpine and Distroless ARM64 images started and returned the configured HTTP redirect. Docker label discovery added and removed a backend using Docker Engine and the socket group without running RXH as root.
- The AArch64 executable compiled on the VPS and ran directly on Ubuntu. The native dashboard passed first-user setup, password change and sign-out, host, pool, user and role edits, chart range selection, raw TOML rejection and persistence, and graceful stop. Native HTTP forwarding, Prometheus metrics, and HTTPS over HTTP/1.1 and HTTP/2 also passed.
- All three ARM64 Pebble ACME flows passed. HTTP-01 and TLS-ALPN-01 used a container-local host mapping, and DNS-01 used a local RFC 2136 fixture. The ARM Web UI build matched the committed assets byte for byte.
- The Debian ARM64 image built from the checkout passed the Compose API and browser checks, the six dashboard tabs and their host, pool and user workflows, and live Docker label discovery. It also reproduced the relative listener TLS path failure. Absolute paths let real HTTPS requests negotiate HTTP/1.1 and HTTP/2.
- The full ARM64 Rust suite passed 1,073 enabled tests, with four ignored by default. The first run had four CLI daemon test failures because the slim Rust image lacked the external `kill` command. All four passed when the test image included `procps`. Three of the ignored tests passed separately with Pebble.
- The source-built ARM64 dashboard also passed password sign-in and sign-out, role creation and deletion, chart range selection, and raw TOML rejection and persistence.
- The native ARM benchmark passed all 17 scenarios across two runs. With the SSH session's default 1,024-file soft limit, the idle test opened only 949 of 1,000 connections and the memory test incorrectly reported that it needed Linux `/proc`. Both passed after raising the limit to 8,192 for the benchmark process. The VPS still had its original system limits.
- The Docker ARM benchmark passed all 17 scenarios across two runs. With the Docker daemon's inherited 1,024-file limit, the idle scenario failed 949 keep-alive probes and RXH logged `Too many open files (os error 24)`. A temporary wrapper gave only the benchmark containers an 8,192-file limit. The idle and memory scenarios then passed.

## Corrections

| Finding | Resolution |
| --- | --- |
| TCP resets during reload | The new generation duplicates unchanged listening sockets, preserving their accept queues. Switching runtime modes preserves the per-core queues. A reload that must stop first to rebind UDP holds unchanged TCP sockets until the replacement starts. A regression test verifies that requests queued before the handoff are served by the replacement. |
| Compose error feedback | The example reports loading, an empty result, and an unavailable API separately. It checks the HTTP status and validates the response type before rendering items. |
| PostgreSQL readiness | The health check uses TCP to avoid accepting the temporary, Unix-socket-only initialization server as ready. The original PostgreSQL shutdown is still an environment observation with an unverified cause. |
| Relative TLS paths | The container guide, configuration reference, and Dockerfile describe the working directory for TLS files and static roots, and the configuration directory for includes, ACME storage, and users. |
| Favicon requests | The dashboard and example declare an embedded icon. Chromium loaded both pages without requesting an undeclared `/favicon.ico`. |
| Fuzz lockfile | The checked-in fuzz lockfile is synchronized. `cargo check --locked --manifest-path fuzz/Cargo.toml` passes. |
| Bun-only Vitest workers | CI explicitly runs the tests with Node.js 24. Bun still installs the locked dependencies and builds the UI. All 197 tests passed under Node.js. |
| CLI tests in slim images | Signal tests use the POSIX shell's built-in `kill`. The ARM test container ran with `--init` so it could reap background daemons. |
| Low benchmark descriptor limits | Docker benchmark scenarios that hold idle connections set a scoped descriptor limit. Linux RSS errors now report the failed operation and error. Local benchmark documentation specifies the inherited shell limit. |

The corrected checkout passed 1,074 Rust tests with four ignored by default, plus formatting and Clippy. Local traffic tests completed 1,125 successful reloads and 34,113 parallel HTTP requests without a reset, including runtime changes and mixed TCP/UDP configurations. On the ARM64 VPS, 37 CLI daemon tests passed and 900 successful reloads served 22,799 parallel requests without a reset. The Docker idle and memory benchmarks passed on both machines with the benchmark's own container limit.
