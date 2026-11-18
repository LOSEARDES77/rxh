# Roadmap

Where RXH is going: a reverse proxy and load balancer for any protocol, run
as a daemon and managed from an easy CLI, with at least Caddy's load
balancing features and then the best ideas from NGINX, Traefik, HAProxy and
Envoy.

This file orders the work into milestones. The detail lives elsewhere:

- [PARITY.md](PARITY.md): feature by feature status against Caddy, and the
  tables of features taken from other proxies (config sketches, sources).
- [docs/benchmarks.md](docs/benchmarks.md): how RXH compares with Caddy,
  NGINX and Traefik today.
- [docs/test-coverage.md](docs/test-coverage.md): coverage numbers and the
  list of untested code.

Milestones are ordered by value, not by date. Version numbers are a plan,
not a promise. An item moves to [CHANGELOG.md](CHANGELOG.md) when it ships.

## Where RXH is today (0.3.0, unreleased)

- HTTP/1.1, HTTP/2, WebSocket, TCP and UDP proxying with one pool model.
- Every Caddy load balancing policy (with nested `fallback`), retries with
  `retry_match`, active and passive health checks with event hooks and
  webhooks, dynamic upstreams from DNS, TLS and unix socket upstreams,
  `max_conns_per_host`, request and response buffering, `flush_interval`.
- Virtual hosts, request matchers, SNI certificates, automatic HTTPS with
  ACME HTTP-01 and HTTP to HTTPS redirects.
- Routes with `rewrite`, `redirect`, `handle_response`, header rules, body
  size limits, `encode` (zstd, br, gzip), a file server with ranges, ETag
  and precompressed files, and a response cache.
- Layer 4 routing by SNI, ALPN, protocol and HTTP host; PROXY protocol in on
  `http` and `tcp` servers and out to backends on both.
- Caddy's timeouts, Prometheus metrics on the admin API and JSON or text
  access logs.
- Daemon with zero downtime reloads, admin API, `include` files and a CLI
  that edits the configuration file and applies changes live, plus
  `rxh file-server` and `rxh reverse-proxy` quick commands.
- In the benchmark (measured with 0.2.0) RXH passes every scenario and is
  faster than Caddy, but clearly slower than NGINX. See docs/benchmarks.md
  for the numbers.
- 82% line coverage without the benchmark tool, measured before most of the
  0.3 work; docs/test-coverage.md has the details and how to re-measure.

## 0.3: finish Caddy parity for load balancing

The remaining rows of PARITY.md that a load balancer needs every day.

- [x] Timeouts: `read_timeout`, `write_timeout`, `response_header_timeout`,
      `stream_timeout`.
- [ ] Per route timeout overrides.
- [x] Prometheus metrics on the admin listener: requests, latency
      histograms, in-flight, health state per backend, per pool and per
      server.
- [ ] A retry counter in the metrics.
- [x] JSON access logs with a stable field set, log rotation left to the
      system.
- [x] `lb_retry_match`, nested `fallback`, header `+` (add), regex replace
      and wildcard delete.
- [x] `max_conns_per_host`, request and response buffering, body size
      limit.
- [x] `rewrite`, `method`, redirects and HTTP to HTTPS redirect.
- [x] `handle_response`, `replace_status` and `copy_response` on proxy
      routes.
- [x] `encode gzip zstd`; range requests, ETag and precompressed files in
      the file server.
- [x] Health check events (`healthy`, `unhealthy`) as hooks, not only log
      lines.
- [x] `stream_close_delay` and a configurable `flush_interval`.

## 0.4: performance

NGINX serves more than twice the requests of RXH in the same scenarios. The
goal of this milestone is to close most of that gap and to keep it closed.

- [ ] Profile the HTTP hot path (flamegraphs, allocation counts) and write
      down the findings before changing code.
- [ ] One runtime per core with `SO_REUSEPORT` listeners, instead of one
      work-stealing runtime, if profiling shows contention.
- [ ] Fewer allocations per request: reuse header maps and buffers, avoid
      cloning addresses and host names on every pick.
- [ ] `splice` zero copy for TCP on Linux.
- [ ] Upstream HTTP/2 connection pool with more than one connection per
      backend.
- [ ] More benchmark scenarios: TLS termination, HTTP/2 clients, large
      bodies, WebSocket, UDP, thousands of idle connections, and memory use
      per connection.
- [ ] Benchmark history: run `rxh-bench --docker` on a fixed machine for
      every release and keep the JSON, so regressions are visible.
- [ ] Criterion micro benchmarks for the policies in `rxh-core`.

## 0.5: TLS and security

- [x] Automatic HTTPS with ACME HTTP-01, certificate storage and renewal.
- [ ] ACME TLS-ALPN-01.
- [ ] ACME DNS-01 and wildcard certificates (Cloudflare, Route53,
      RFC 2136).
- [ ] mTLS on listeners with the four Traefik modes, and the client
      certificate subject passed to backends.
- [ ] TLS options: minimum version, cipher suites, curves. OCSP stapling.
- [ ] Certificate reload without restart, and `rxh cert` commands to list
      certificates and expiry dates.
- [ ] IP allow and deny lists, rate limiting, concurrent connection limit
      per client.
- [ ] Basic auth, forward auth (Authelia, Authentik, oauth2-proxy).
- [ ] Security header presets and CORS.
- [ ] Admin API over a unix socket, and optional token authentication for
      remote admin.

## 0.6: discovery and deployment

- [ ] Docker provider: route to containers from `rxh.*` labels and follow
      start and stop events live.
- [ ] Official images (`rxh:latest`, `rxh:0.x`, `-alpine`, distroless) for
      amd64 and arm64, published from CI on release tags.
- [ ] File watch: reload when the configuration file changes.
- [ ] `POST /load` to upload a full configuration, and `PATCH` for one
      pool or server.
- [ ] Environment variable placeholders and secret files in the
      configuration.
- [ ] Packages: static musl binaries, `.deb`, `.rpm`, AUR and Homebrew,
      built by CI. Shell completions and man pages from clap.
- [ ] systemd integration: `sd_notify` readiness, socket activation.
- [ ] Docker Swarm, Podman, Consul, etcd and Nomad providers.
- [ ] Kubernetes Ingress and Gateway API.

## 0.7: traffic management

- [ ] Weighted pools for canary releases and traffic splitting.
- [ ] Request mirroring.
- [ ] Circuit breaker on error ratio or latency.
- [ ] Slow start for new and recovered backends.
- [ ] Drain and maintenance states: `rxh upstream drain web 10.0.0.1:80`.
- [ ] Consistent hashing with bounded loads, Maglev.
- [ ] Per backend connection limits with a queue.
- [ ] Retry budgets and hedged requests.
- [ ] Custom error pages. Upstream errors can already be replaced with
      `handle_response`; errors RXH generates itself cannot.
- [ ] gRPC health checks and gRPC-Web.
- [x] Layer 4 matchers (SNI, HTTP, SSH) and routing; receiving PROXY
      protocol; PROXY protocol on the HTTP transport.

## 0.8: observability and UI

- [ ] OpenTelemetry tracing with W3C trace context.
- [ ] Access log filters and sampling.
- [x] Health events to webhooks.
- [ ] `rxh top`: live requests per second, latency and errors per pool in
      the terminal, built on the metrics endpoint.
- [ ] Web dashboard on top of the admin API: hosts, certificates, access
      lists, live health. Read only first, then management like Nginx
      Proxy Manager.

## 1.0: stable

A release people can run without reading the source.

- [ ] Configuration format frozen, with a documented migration path for
      every later change and `rxh validate` warnings for deprecated keys.
- [ ] `rxh fmt` and a JSON schema for the TOML file, for editor
      completion.
- [ ] `rxh adapt` to convert a Caddyfile or a simple `nginx.conf` into RXH
      TOML.
- [ ] Documentation site with a guide per use case (load balancer, TLS
      gateway, Docker, layer 4).
- [ ] Hot binary upgrade: a new process takes over the listening sockets.
- [ ] Security policy, `cargo audit` and `cargo deny` in CI, and an
      external review of the TLS and admin surface.

## Quality, in every milestone

- [ ] Coverage of 85% or more without the benchmark tool, enforced in CI
      with `cargo llvm-cov --fail-under-lines`. The gaps are listed in
      docs/test-coverage.md; daemon signals, reload rollback and the admin
      API come first.
- [ ] Fuzzing with `cargo fuzz`: configuration parser, PROXY protocol,
      matchers, cache keys.
- [ ] Property tests for the policies: weights respected, hashing stable
      when a backend leaves, no pick of an unhealthy backend.
- [ ] A reusable flaky backend for tests: refuse, accept then close, slow
      headers, truncated body.
- [ ] `rxh-bench --quick` as a correctness gate in CI for RXH, with the
      other proxies as information only.

## Later, or maybe never

Large features that wait for a clear need.

- HTTP/3 and QUIC.
- FastCGI transport.
- Plugins or WASM middleware.
- State shared between several RXH instances (rate limits, sticky tables,
  health), through gossip or Redis.
- GeoIP rules.
- Windows service support.

## Not planned

- A scripting language in the configuration. Matchers and middleware cover
  routing; anything bigger belongs in a backend.
- An application server. RXH serves static files and proxies; it does not
  run PHP, Python or Node itself.
