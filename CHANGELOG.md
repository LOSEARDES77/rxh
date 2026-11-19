# Changelog

## 0.3.0 (unreleased)

Caddy parity for HTTP routing, upstream transports, timeouts, observability,
the file server and automatic HTTPS, plus layer 4 routing. Configuration
files of 0.2.0 keep working, except for the stricter validation listed under
Changed.

### Added

- `client_auth = { mode, ca, header }` on servers with `tls`: mutual TLS with
  the four Traefik modes, `"request"`, `"require"`, `"verify_if_given"` and
  `"require_and_verify"` (default). `ca` lists the PEM files of the
  authorities that sign client certificates. On `http` servers `header`
  names a request header that carries the subject of the verified client
  certificate to the backends; a value sent by the client is dropped.
- `[acme] challenge = "dns-01"` with the `[acme.dns]` table (`provider`
  `"cloudflare"`, `"route53"` or `"rfc2136"`, their credentials, `zone_id`,
  `api_url`, `propagation_wait`): `tls = "auto"` certificates validated with
  the ACME DNS-01 challenge through a TXT record at the DNS provider, with
  no open port. It allows wildcard names (`host = "*.example.com"`) on
  `tls = "auto"` servers, stored as `wildcard_.example.com`. The admin API
  shows the provider secrets as `***`.
- `[acme] challenge = "tls-alpn-01"` (default `"http-01"`): `tls = "auto"`
  certificates validated with the ACME TLS-ALPN-01 challenge, answered by
  the TLS listener itself on port 443, so no plain HTTP port is needed.
- Criterion micro benchmarks of `Pool::select` for every load balancing
  policy with 4 and 64 backends: `cargo bench -p rxh-core --bench policies`.
  `cargo test` runs each once as a check.
- Benchmark history: `crates/bench/release.sh` runs `rxh-bench --docker` for
  a release and keeps the JSON in `docs/bench/history`, and
  `rxh-bench --history DIR` renders rxh's numbers across releases with
  regressions of more than 10% marked. Seeded with the 0.2.0 run.
- `runtime = "per_core"` (top level, default `"shared"`): one single-threaded
  runtime per CPU, each with its own `SO_REUSEPORT` listeners and upstream
  connections, instead of one work-stealing runtime. In the `throughput`
  scenario it serves 3% to 96% more requests per second (24% more on 32
  CPUs with 32 connections) at a lower p99: docs/profiling.md.
- `http2_conns_per_host` on a pool: number of HTTP/2 connections kept open
  to each backend with `h2c` (default `1`, as before). Requests use them in
  turn and closed ones are replaced.
- `rxh-bench` scenarios `tls` (HTTPS termination), `http2` (HTTP/2 clients
  over TLS), `large-body` (1 MiB echoed bodies), `websocket`, `udp`, `idle`
  (load next to thousands of idle keep-alive connections) and `memory` (proxy
  RSS per idle connection), and the `--idle-connections` flag. They run on
  rxh, Caddy, NGINX and Traefik.
- Request matchers on routes: `host` (exact or `*.wildcard`, also server
  wide), Caddy style `path` patterns, `method`, `header`, `query` and
  `remote_ip`, ANDed with the `uri` prefix. `rxh service add --host`.
- Virtual hosts: several `http` servers may share a `listen` address (one
  socket, routes tried in order, each keeping its own settings), with TLS
  certificates picked by SNI against `host`.
- Upstream transports: `https://` backends with a pool `tls` table
  (`server_name`, `insecure_skip_verify`, `ca`, `client_cert`/`client_key`),
  `unix//path` backends for HTTP, TCP and health checks.
- Dynamic upstreams: `[upstream.NAME.dynamic]` fills a pool from DNS `A`/`AAAA`
  or `SRV` records and refreshes it, keeping the health and counters of
  unchanged members.
- Timeouts: upstream `response_header_timeout` (`504`), `read_timeout` and
  `write_timeout`; server `read_header_timeout`, `idle_timeout` (also for
  `tcp` connections) and `stream_timeout` (tunnels and `tcp` connections).
- Observability: Prometheus `GET /metrics` on the admin API, and a `[log]`
  table for text or JSON access logs to stdout, stderr or a file; every TCP
  and UDP connection writes one record with its byte counts.
- `retry_match = { method, status }`: methods retried after a failed round
  trip and response statuses retried on another backend. `fallback` accepts
  a nested table with its own `key`, `choose`, `secret` and `fallback`.
- Header rules gain `add`, prefix wildcard `remove` (`x-internal-*`) and regex
  `replace`, applied in the order remove, replace, set, add.
- File server: `serve` takes a table (`root`, `browse`, `index`, `hide`,
  `precompressed`); files stream with `Content-Length`, `Last-Modified`, weak
  `ETag` and `Accept-Ranges`, conditional requests get `304`, single ranges
  `206`/`416`, and precompressed `.zst`/`.br`/`.gz` files are served when the
  client accepts them.
- `encode = ["zstd", "br", "gzip"]` on a route compresses file and proxied
  responses on the fly.
- `include` globs split `upstream` and `server` tables over several files;
  reloads re-read them and CLI edits change the file that defines the pool or
  service (`--file` for new ones). `rxh config show [--json]` prints the
  effective configuration.
- Quick commands without a configuration file: `rxh file-server` and
  `rxh reverse-proxy`.
- Layer 4 routing: `tcp` servers take `match` entries with `sni`, `alpn`,
  `protocol` (`tls`, `http`, `ssh`) and `http_host`, read within
  `match_timeout` and replayed to the backend (TLS passthrough by SNI).
- PROXY protocol output on `http` servers: `proxy_protocol = "v1"` or `"v2"`
  sends a PROXY header with the client address on every new connection to a
  backend, like on `tcp` servers. Upstream keep-alive connections are then
  kept per client connection. Rejected with `h2c` pools.
- PROXY protocol input: `proxy_protocol_in = true` on `http` and `tcp`
  servers, limited to `trusted_proxies` when set; the source address is the
  client address everywhere.
- Automatic HTTPS: `tls = "auto"` obtains and renews certificates for the
  server's `host` names from an ACME CA with HTTP-01 (`[acme]` with `email`,
  `ca`, `ca_root`, `storage`, `renew_before`, `http_port`), stored per CA and
  swapped in without a restart.
- `rewrite = "/v2{path}"` on a route changes the URI that is forwarded or
  served (the query is kept unless the rewrite has one) and
  `request.method` changes the method. Placeholders: `{host}`, `{path}`,
  `{query}`, `{uri}`.
- `handle_response` on `forward` routes, for upstream responses matched by
  `status` and `header`: `replace_status` and `copy_response` keep the
  response (with another status), `serve` (with `rewrite`) and `redirect`
  answer with a file or a redirect instead. `{status}` and `{header.NAME}`
  placeholders allow error pages per status and `X-Accel-Redirect`.
- `redirect = "target"` or `{ to, status }` as a route action next to
  `forward` and `serve`: answers `302` (or `301`, `303`, `307`, `308`) with
  the same placeholders in `Location`.
- `max_conns_per_host` on a pool caps the HTTP/1.1 connections to each
  backend; requests over the limit wait for a free connection.
- `request_buffers` and `response_buffers` on a pool read up to that many
  bytes of a body before passing it on; a body that fits is sent with a
  `Content-Length`. Sizes are bytes or strings like `"64KiB"` and `"10MB"`.
- `request.max_body_size` on a route answers `413 Content Too Large` to
  larger request bodies, before dialing when `Content-Length` says so.
- HTTP to HTTPS redirect: `https_redirect = true` on a TLS `http` server
  answers plain requests for its `host` names on `[acme] http_port` with
  `308` to `https://{host}{uri}`. On by default with `tls = "auto"`.
- `flush_interval` on a pool writes responses with a `Content-Length` to
  the client in batches collected for that long; server-sent events and
  streams of unknown length are still written at once.
- `stream_close_delay` on an `http` server closes upgraded tunnels
  (WebSockets) that long after the listener stops on a reload or shutdown;
  unset keeps them open as before.
- Health event hooks: `[upstream.NAME.health.hooks]` runs a `command`
  (event in `RXH_EVENT`, `RXH_POOL`, `RXH_UPSTREAM`, `RXH_REASON`) and posts
  JSON to a `webhook` when active checks mark a backend `healthy` or
  `unhealthy`, in event order and without delaying the checks.
- Tests: `crates/util` (`rxh-test-util`, not published) with `FlakyBackend`, a
  backend that refuses connections, accepts then closes them, delays its
  response head or truncates its body.
- Tests: property tests (`proptest`) of the load balancing policies in
  `crates/core/tests/policy_properties.rs`: weighted round robin picks each
  backend exactly its weight per cycle, hash policies only move the keys of
  a backend that is removed or marked down (and give them back when it
  returns), and no policy, fallback or cookie ever picks a backend that is
  down, failed or full.
- CI: a `coverage` job runs `cargo llvm-cov --fail-under-lines 85` over the
  workspace without `crates/bench` and fails below 85% line coverage.
- Fuzzing with `cargo fuzz` (`fuzz/`, see `docs/fuzzing.md`): targets for the
  configuration parser, the PROXY protocol parser and writer, the host and
  path matchers, layer 4 protocol sniffing and the response cache keys. A
  `fuzz` CI job runs each target for 60 seconds.
- CI: a `bench-gate` job runs `rxh-bench --quick` and fails when RXH fails
  or cannot run a scenario. Caddy runs alongside for information only.
- `docs/profiling.md`: CPU flame graphs, system call and allocation counts of
  the HTTP hot path, with the scripts to reproduce them (`docs/profiling/`).
  No runtime change.
- `SECURITY.md`: supported versions, private reporting through GitHub
  security advisories, scope. An `Audit` workflow runs `cargo audit` and
  `cargo deny` (policy in `deny.toml`: advisories, licenses, bans, sources)
  on every push, pull request and daily.
- Zero copy for `tcp` servers on Linux: connections with plain TCP on both
  sides (no TLS termination, no upstream TLS, `host:port` backends) are
  relayed with `splice(2)`, so the bytes stay in the kernel. Other
  connections and other systems keep the buffered copy. No configuration.

### Changed

- A proxied HTTP request makes 26 allocator calls instead of 84. The
  forwarding headers and the text access line are written into buffers kept
  per thread, values that never change (`Via`, `Server`, the peer IP) are
  built once and shared, the request head is copied for a retry only when
  one is possible, and backend addresses and host names are no longer copied
  for every request. The `throughput` scenario of `rxh-bench` serves 7% more
  requests per second (227 800 to 243 800). Headers sent to backends and
  clients, and log lines, are unchanged. Numbers in
  [docs/profiling.md](docs/profiling.md#after-fewer-allocations).

- `rxh-bench` exits with 1 only when rxh fails a scenario or cannot run it
  (`n/a`); failures of Caddy, NGINX and Traefik are reported but no longer
  change the exit code.

- Response cache keys carry the length of the pool name and of the `Host`
  header. Joined with `|`, a request with `Host: h|/a` for `/b` and one with
  `Host: h` for `/a|/b` shared an entry, so a client could plant a response
  for another host and path (found by the `cache_key` fuzz target).
- `proxy_protocol` on a `tcp` server with TLS backends sends the header
  before the TLS handshake, as the PROXY protocol requires, instead of inside
  the TLS session. A backend that refuses the header now counts as a failed
  dial and is retried on another backend.
- Servers with `tls = "auto"` now listen on `[acme] http_port` (their own
  addresses with that port) all the time, to redirect to HTTPS and answer
  challenges, instead of opening a challenge-only listener on `0.0.0.0`
  while issuing. Starting fails if the port cannot be bound; set
  `https_redirect = false` for the previous behaviour.

- `read_header_timeout` now also bounds the TLS handshake and the time to the
  first complete request of a connection, HTTP/2 included. When unset, TLS
  handshakes get 10 seconds and the first request head 30 seconds, so
  clients that connect and send nothing no longer hold a connection slot.
- `try_duration` also bounds round-trip and status retries, and round-trip
  retries wait `try_interval` between attempts, like dial retries.
- File routes answer methods other than `GET` and `HEAD` with `405`.
- `tls` is an enum in the configuration model (`{ cert, key }` or `"auto"`);
  existing TOML files are unaffected.
- Stricter validation, so `rxh validate` rejects what the daemon would:
  - HTTP request matchers on `tcp` and `udp` servers, and `rewrite`,
    `request`, `response`, `encode` and `cache` on them;
  - `host` on `udp` servers;
  - two servers with the same `name`;
  - a TLS server without `host` that is not the first on a shared address
    (its certificate would never be served);
  - an invalid `health.body` regex and a backend listed twice in one pool.
- Rust 1.100 toolchain (`rust-toolchain.toml`) and the latest dependency
  releases; the file server's HTML assets are embedded without a minifying
  build step.
- Dropped the unmaintained `daemonize` (RUSTSEC-2025-0069) and
  `rustls-pemfile` (RUSTSEC-2025-0134) crates. PEM files are read with
  `rustls-pki-types`; `rxh start` forks with `libc` directly.
- `rxh start` locks the PID file before forking: a second daemon with the
  same PID file now fails in the foreground ("is locked, is rxh already
  running?") instead of exiting silently in the background, and the PID file
  is written before `rxh start` returns.

### Fixed

- Reloading a configuration with a UDP server could fail with "Address
  already in use" and keep the old configuration: the stopped listener
  reported its shutdown while its sessions still held the socket. The
  sessions now end before the listener reports that it has stopped.

## 0.2.0 (unreleased)

RXH grows from an HTTP reverse proxy into a general reverse proxy and load
balancer with Caddy level load balancing.

### Added

- `http2_conns_per_host` on a pool: number of HTTP/2 connections kept open
  to each backend with `h2c` (default `1`, as before). Requests use them in
  turn and closed ones are replaced.
- Layer 4 proxying: `protocol = "tcp"` (with TLS termination and PROXY
  protocol v1/v2) and `protocol = "udp"` (per client sessions).
- Named load balancing pools (`[upstream.NAME]`, `forward = "@NAME"`).
- All Caddy load balancing policies: `random`, `random_choose`, `first`,
  `round_robin`, `weighted_round_robin`, `least_conn`, `ip_hash`,
  `client_ip_hash`, `uri_hash`, `query`, `header`, `cookie`, with `fallback`.
- Retries (`retries`, `try_duration`, `try_interval`), active HTTP/TCP health
  checks and passive health checks.
- Keep-alive connection pool to backends and optional h2c.
- `X-Forwarded-For/Proto/Host` with `trusted_proxies`, hop-by-hop header
  removal, quoted RFC 7239 `Forwarded` values.
- Admin API on `127.0.0.1:2019` and the `rxh` CLI: `run`, `start`, `stop`,
  `reload`, `status`, `validate`, `upstream`, `pool`, `service`, `policies`.
- Zero downtime reloads with `SO_REUSEPORT`; invalid files are rejected.
- `grace_period`, closing of idle keep-alive connections on shutdown.
- `rxh-bench`, a correctness and performance comparison against Caddy.
- [PARITY.md](PARITY.md), tracking feature parity with Caddy.

### Changed

- The project is a Cargo workspace: `rxh-core`, `rxh-server`, `rxh-cli`,
  `rxh-bench`. Builds and tests on stable Rust.
- The default policy is smooth weighted round robin (interleaved instead of
  blocks).
- CLI: `--daemon`, `--kill-daemon` and `--reload-config` became `rxh start`,
  `rxh stop` and `rxh reload`.
- TLS listeners negotiate HTTP/2 through ALPN.
- HTML escaping of file names and paths in the directory listing.
