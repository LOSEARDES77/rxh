# Changelog

## 0.2.0 (unreleased)

RXH grows from an HTTP reverse proxy into a general reverse proxy and load
balancer with Caddy level load balancing.

### Added

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
