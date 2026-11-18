# Caddy feature parity

This file tracks how RXH compares with [Caddy](https://caddyserver.com) v2:
which Caddy features RXH supports today, which ones are partial or missing,
and what RXH adds on top. The goal is to support at least everything Caddy
does for reverse proxying and load balancing, then go further, especially at
layer 4.

Run [`rxh-bench`](crates/bench/README.md) to check the load balancing rows
against a real Caddy. It runs the same scenarios on both and reports
correctness and performance side by side.

Legend: ✅ done · 🟡 partial · ❌ missing · ➕ RXH extension (Caddy has no
equivalent in its core)

Caddy references: [`reverse_proxy`](https://caddyserver.com/docs/caddyfile/directives/reverse_proxy),
[admin API](https://caddyserver.com/docs/api), [CLI](https://caddyserver.com/docs/command-line),
[caddy-l4](https://github.com/mholt/caddy-l4).

## Upstreams

| Caddy | RXH | Status | Notes |
|---|---|---|---|
| Static upstreams `host:port` | `backends = ["host:port"]` | ✅ | Host names are resolved on every dial. |
| Port ranges `host:8001-8006` | same syntax | ✅ | |
| `h2c://` upstreams | `h2c = true` | ✅ | One multiplexed HTTP/2 connection per backend. |
| `https://` upstreams, `transport http { tls }` | | ❌ | Planned: rustls client with system roots. |
| `unix//path` upstreams | | ❌ | |
| `dynamic srv`, `dynamic a`, `dynamic multi` | | ❌ | Planned: periodic DNS refresh feeding the pool. |
| Change upstreams at runtime | `rxh upstream add/rm`, `POST/DELETE /pools/{name}/upstreams` | ✅ ➕ | Caddy needs a full config patch; RXH changes one pool in place and keeps health state and counters of the other members. |
| Named, reusable upstream groups | `[upstream.NAME]`, `forward = "@NAME"` | ➕ | Caddy repeats the upstream list in every `reverse_proxy`. |

## Load balancing policies (`lb_policy`)

| Caddy | RXH `policy` | Status | Notes |
|---|---|---|---|
| `random` | `random` | ✅ | |
| `random_choose <n>` | `random_choose` + `choose = n` | ✅ | |
| `first` | `first` | ✅ | |
| `round_robin` | `round_robin` | ✅ | |
| `weighted_round_robin <weights...>` | `weighted_round_robin` + per backend `weight` | ✅ | Smooth (nginx style) interleaving. Weight 0 disables new picks, like Caddy. Default policy in RXH; Caddy defaults to `random`. |
| `least_conn` | `least_conn` | ✅ | Ties broken randomly. Counts requests in flight (HTTP) or open connections (TCP/UDP). |
| `ip_hash` | `ip_hash` | ✅ | Rendezvous (HRW) hashing, like Caddy. |
| `client_ip_hash` | `client_ip_hash` | ✅ | Uses `trusted_proxies`. |
| `uri_hash` | `uri_hash` | ✅ | |
| `query [key]` | `query` + `key` | ✅ | |
| `header [field]` | `header` + `key` | ✅ | |
| `cookie [name [secret]]` | `cookie` + `key`, `secret` | ✅ | HMAC-SHA256 of the backend address. `Secure; SameSite=None` on TLS listeners. |
| `fallback <policy>` | `fallback` | 🟡 | One level. Caddy allows nesting fallbacks. |
| Layer 4 policies (caddy-l4) | same policy names on `tcp`/`udp` servers | ✅ | HTTP-only policies are rejected for L4 at load time. |

## Retries

| Caddy | RXH | Status | Notes |
|---|---|---|---|
| `lb_retries` | `retries` | ✅ | |
| `lb_try_duration` | `try_duration` | ✅ | Wins over `retries` when both are set, like Caddy. |
| `lb_try_interval` | `try_interval` | ✅ | Default 250ms. |
| Dial failures always retried | same | ✅ | |
| Round-trip failures retried for `GET` by default | `GET`, `HEAD`, `OPTIONS` without body | ✅ | |
| `lb_retry_match` | | ❌ | |
| Stale pooled connection transparently replaced | same | ✅ | |

## Active health checks

| Caddy | RXH `[upstream.X.health]` | Status | Notes |
|---|---|---|---|
| `health_uri` | `uri` | ✅ | |
| `health_upstream` | `upstream` | ✅ | |
| `health_port` | `port` | ✅ | |
| `health_interval` | `interval` | ✅ | Default 30s. |
| `health_timeout` | `timeout` | ✅ | Default 5s. |
| `health_passes` / `health_fails` | `passes` / `fails` | ✅ | |
| `health_method` | `method` | ✅ | |
| `health_status` | `status` (`200`, `2xx`) | ✅ | Default `2xx`. |
| `health_body` | `body` (regex) | ✅ | |
| `health_request_body` | `request_body` | 🟡 | No `{env.*}` placeholders. |
| `health_headers` | `headers` | ✅ | |
| `health_follow_redirects` | `follow_redirects` | 🟡 | Follows the path of `Location` on the same target. |
| TCP connect checks | `health` without `uri` | ➕ | For TCP/UDP pools and non-HTTP backends. |
| `healthy` / `unhealthy` events | log lines | 🟡 | No event hooks yet. |

## Passive health checks

| Caddy | RXH `[upstream.X.passive]` | Status | Notes |
|---|---|---|---|
| `fail_duration` | `fail_duration` | ✅ | Default 30s once the table exists. |
| `max_fails` | `max_fails` | ✅ | |
| `unhealthy_status` | `unhealthy_status` (`503`, `5xx`) | ✅ | |
| `unhealthy_latency` | `unhealthy_latency` | ✅ | Time to response headers. |
| `unhealthy_request_count` | `unhealthy_request_count` | ✅ | |
| Dial failures count as failures | same | ✅ | Also UDP ICMP refusals. |

## HTTP proxying

| Caddy | RXH | Status | Notes |
|---|---|---|---|
| HTTP/1.1 and HTTP/2 clients | same, `http2 = true` | ✅ | h2 over TLS through ALPN, h2c in cleartext. |
| HTTP/3 | | ❌ | |
| WebSockets and other upgrades | same | ✅ | |
| Hop-by-hop headers removed | same | ✅ | Including headers listed in `Connection`. `TE: trailers` kept. |
| `X-Forwarded-For/Proto/Host` | same | ✅ | Client values kept only from `trusted_proxies`. |
| `trusted_proxies` (+ `private_ranges`) | per server `trusted_proxies` | ✅ | |
| `Forwarded` (RFC 7239) and `Via` | `request.headers.forwarded`, `response.headers.via` | ➕ | |
| `header_up` / `header_down` set and delete | `request.headers.set/remove`, `response.headers.set/remove` | 🟡 | No `+` (add), no regex replace, no wildcard delete. |
| Upstream keep-alive pool | `keepalive`, `keepalive_idle_conns_per_host` | ✅ | |
| `dial_timeout` | `dial_timeout` | ✅ | Default 3s. |
| `read_timeout`, `write_timeout`, `response_header_timeout` | | ❌ | |
| `max_conns_per_host` | | 🟡 | `unhealthy_request_count` covers the common use. |
| `proxy_protocol v1/v2` on HTTP transport | | ❌ | Supported on TCP servers. |
| `flush_interval` | | 🟡 | Responses always stream immediately (Caddy's `-1`). |
| `request_buffers`, `response_buffers` | | ❌ | |
| `stream_timeout`, `stream_close_delay` | | ❌ | |
| `rewrite`, `method` | | ❌ | |
| `handle_response`, `replace_status`, `copy_response` | | ❌ | |
| `transport fastcgi` | | ❌ | |
| Response cache | `cache` per route | ➕ | Core Caddy needs the cache-handler plugin. |

## Server, routing and files

| Caddy | RXH | Status | Notes |
|---|---|---|---|
| Multiple sites and listeners | `[[server]]`, `listen = [...]` | ✅ | |
| Path matchers | `uri` prefix, `path` patterns | ✅ | Exact, prefix, suffix and middle `*`. Case sensitive, `*` also crosses `/`. |
| Host matchers (virtual hosts) | `host`, server level `host` | ✅ | Exact and `*.domain`. HTTP servers on the same `listen` address share one socket. |
| Header, method, query, remote IP matchers | `header`, `method`, `query`, `remote_ip` | ✅ | AND across matchers, OR inside lists. No regex or percent decoding. |
| Manual TLS certificates | `tls = { cert, key }` | ✅ | |
| Automatic HTTPS (ACME) | | ❌ | Large feature. |
| SNI with many certificates, mTLS | `tls` per server on a shared address | 🟡 | Certificate picked by SNI against `host` (exact beats wildcard, first is the default). No mTLS. |
| `file_server` with `browse` | `serve` | ✅ | Index files and directory listing. |
| Range requests, ETag, precompressed files | | ❌ | |
| `encode gzip zstd` | | ❌ | |
| `grace_period` | `grace_period` | ✅ | Default waits forever, like Caddy. Idle keep-alive connections close at once. |
| Connection limit | `max_connections` | ➕ | |
| Structured access logs | plain text lines | 🟡 | JSON logs planned. |
| Prometheus metrics | | ❌ | |

## Layer 4 (Caddy needs the caddy-l4 plugin)

| caddy-l4 | RXH | Status | Notes |
|---|---|---|---|
| TCP proxy | `protocol = "tcp"` | ✅ | |
| UDP proxy | `protocol = "udp"` | ✅ | Per client sessions, `udp_idle_timeout`. |
| TLS termination | `tls` on a `tcp` server | ✅ | |
| PROXY protocol to upstreams | `proxy_protocol = "v1"` or `"v2"` | ✅ | |
| Load balancing and health checks | same pools as HTTP | ✅ | Active TCP checks, passive dial failures. |
| Matchers (SNI, HTTP, SSH, ...) and routing | | ❌ | |
| Receiving PROXY protocol | | ❌ | |

## Operations: daemon, CLI and admin API

| Caddy | RXH | Status | Notes |
|---|---|---|---|
| `caddy run` | `rxh run` (or plain `rxh`) | ✅ | |
| `caddy start` | `rxh start` | ✅ | Unix only, PID file and log file. |
| `caddy stop` | `rxh stop` | ✅ | Admin API, falls back to the PID file. |
| `caddy reload` | `rxh reload`, `SIGHUP` | ✅ | Zero downtime (`SO_REUSEPORT`); an invalid file keeps the running config. |
| `caddy validate` | `rxh validate` | ✅ | |
| `caddy fmt`, `caddy adapt` | | ❌ | |
| `caddy reverse-proxy`, `caddy file-server` quick commands | `rxh service add --to/--serve` | 🟡 | Persists to the config file instead of running ad hoc. |
| Admin API on `localhost:2019` | `[admin] listen` (same default) | ✅ | |
| Admin origin / host checks | same | ✅ | Blocks DNS rebinding from browsers. |
| `GET /config/` | `GET /config` | 🟡 | Read only, JSON, includes live pool members. |
| `POST /load`, `PATCH /config/...` | `POST /reload` | 🟡 | Reloads the file; no config upload yet. |
| `GET /reverse_proxy/upstreams` | same path and fields | ✅ | Plus `GET /pools` with health details. |
| `POST /stop` | same | ✅ | |
| Admin over a unix socket | | ❌ | |
| CLI to manage pools, peers and services | `rxh upstream`, `rxh pool`, `rxh service` | ➕ | Edits the TOML keeping comments, validates, applies live. |
| Live status table | `rxh status` | ➕ | |

## Next steps

Ordered by value for a load balancer:

1. HTTPS upstreams (`transport http { tls }`) and unix socket upstreams.
2. Dynamic upstreams from DNS (`A`/`AAAA`, `SRV`).
3. Timeouts: `read_timeout`, `write_timeout`, `response_header_timeout`, `stream_timeout`.
4. Prometheus metrics and JSON access logs.
5. `lb_retry_match`, nested `fallback`, header `+`/regex operations.
6. Compression (`encode`), range requests and ETags for the file server.
7. Automatic HTTPS with ACME.
8. Layer 4 matchers (SNI routing) and receiving PROXY protocol.
9. HTTP/3.
