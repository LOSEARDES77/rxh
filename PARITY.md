# Caddy feature parity

This file tracks how RXH compares with [Caddy](https://caddyserver.com) v2:
which Caddy features RXH supports today, which ones are partial or missing,
and what RXH adds on top. The goal is to support at least everything Caddy
does for reverse proxying and load balancing, then go further, especially at
layer 4.

Run [`rxh-bench`](crates/bench/README.md) to check the load balancing rows
against a real Caddy. It runs the same scenarios on both and reports
correctness and performance side by side. CI runs `rxh-bench --quick` as a
correctness gate for RXH; the Caddy results there are for information only.

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
| `h2c://` upstreams | `h2c = true`, `http2_conns_per_host` | ✅ ➕ | Multiplexed HTTP/2 connections shared by all clients: one per backend by default, `http2_conns_per_host` of them used in turn (Caddy keeps one). |
| `https://` upstreams, `transport http { tls }` | `https://host[:port]`, `[upstream.NAME.tls]` | ✅ | `server_name`, `insecure_skip_verify`, `ca` (extra roots on top of webpki-roots), `client_cert`/`client_key`. `Host` is the backend host unless set. Also used by L4 and health checks. Pools cannot mix `https://` and plain backends. |
| `unix//path` upstreams | `unix//run/app.sock` | ✅ | HTTP, TCP and health checks. Not for UDP. |
| `dynamic srv`, `dynamic a` | `[upstream.NAME.dynamic]` with `srv`/`a`, `refresh`, `resolvers`, `versions` | ✅ | Periodic DNS refresh feeding the pool; health state and counters of unchanged members survive. `a` needs `tls.server_name` with TLS. `dynamic multi` is not implemented. |
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
| `fallback <policy>` | `fallback` | ✅ | Policy name or a table with its own `key`, `secret`, `choose` and a nested `fallback`. |
| Layer 4 policies (caddy-l4) | same policy names on `tcp`/`udp` servers | ✅ | HTTP-only policies are rejected for L4 at load time. |

## Retries

| Caddy | RXH | Status | Notes |
|---|---|---|---|
| `lb_retries` | `retries` | ✅ | |
| `lb_try_duration` | `try_duration` | ✅ | Wins over `retries` when both are set, like Caddy. |
| `lb_try_interval` | `try_interval` | ✅ | Default 250ms. |
| Dial failures always retried | same | ✅ | |
| Round-trip failures retried for `GET` by default | `GET`, `HEAD`, `OPTIONS` without body | ✅ | |
| `lb_retry_match` | `retry_match = { method, status }` | ✅ | `method` lists methods retried after a failed round trip (default GET, HEAD, OPTIONS, bodyless only); `status` drops matching responses and retries another backend. |
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
| `healthy` / `unhealthy` events | log lines, `hooks` (`command`, `webhook`) | ✅ | Per pool. Command gets `RXH_*` variables, webhook a JSON `POST`. Active checks only. |

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
| `header_up` / `header_down` set, add, delete, replace | `request.headers.set/add/remove/replace`, `response.headers.set/add/remove/replace` | ✅ | Plus `add` (Caddy `+`), regex `replace` and `x-prefix-*` wildcard delete. Order: remove, replace, set, add. |
| Upstream keep-alive pool | `keepalive`, `keepalive_idle_conns_per_host` | ✅ | |
| `dial_timeout` | `dial_timeout` | ✅ | Default 3s. |
| `read_timeout`, `write_timeout`, `response_header_timeout` | same names | ✅ | Unset by default. `response_header_timeout` gives `504`; read and write timeouts also apply to idle pooled connections and tunnels. |
| `max_conns_per_host` | `max_conns_per_host` | ✅ | HTTP/1.1 connections per backend, idle ones included; requests over the limit wait for a free connection, like Caddy. Upgraded tunnels count. Not applied to `h2c` (see `http2_conns_per_host`) or to `tcp`/`udp` servers. |
| `proxy_protocol v1/v2` on HTTP transport | `proxy_protocol = "v1"` or `"v2"` on the server | 🟡 | Server level option, shared with `tcp` servers. Sent before the upstream TLS handshake; like Caddy, upstream keep-alive connections are kept per client connection. Not with `h2c` pools (rejected); active health checks send no header. |
| `flush_interval` | `flush_interval` on a pool | ✅ | Unset streams immediately (Caddy's `-1`, so `-1` itself is not accepted). Like Caddy, server-sent events and responses without `Content-Length` are never batched. |
| `request_buffers`, `response_buffers` | same names | ✅ | Bytes or a size (`"64KiB"`). A body that fits is sent with a `Content-Length`; the rest of a larger one is streamed. No `-1` (unlimited); buffered request bodies are not replayed on retries. |
| `request_body { max_size }` | `request.max_body_size` on a route | ✅ | `413`, from `Content-Length` before dialing, or while streaming a body without one. |
| `stream_timeout` | `stream_timeout` on the server | 🟡 | Server level option, also caps `tcp` connections. |
| `stream_close_delay` | `stream_close_delay` on the server | 🟡 | Server level option, like `stream_timeout`. Unset keeps tunnels open across reloads (Caddy closes them at once, set `"0s"` for that). Not applied to `tcp` connections, which follow `grace_period`. |
| `rewrite`, `method` | `rewrite`, `request.method` on a route | ✅ | `rewrite` is a path (optionally with a query) with `{host}`, `{path}`, `{query}` and `{uri}` placeholders; the query is kept unless the rewrite has one. Applied after routing (routes are not matched again), also on `serve` routes. No regex, `strip_prefix` or `replace`. |
| `redir` | `redirect` on a route (`"target"` or `{ to, status }`) | ✅ | Same placeholders; `302` by default, `301`, `303`, `307` and `308` allowed. No `html` redirects. |
| `handle_response`, `replace_status`, `copy_response` | `handle_response` list on a route, with `replace_status`, `copy_response`, `serve` + `rewrite` or `redirect` per entry | ✅ | Matchers: `status` (codes, classes) and `header`; first match wins. `{status}` and `{header.NAME}` placeholders cover error pages and `X-Accel-Redirect`. Handlers are a file or a redirect, not arbitrary routes: no `respond`, no second `forward`, no `copy_response_headers`. |
| `transport fastcgi` | | ❌ | |
| Response cache | `cache` per route | ➕ | Core Caddy needs the cache-handler plugin. |

## Server, routing and files

| Caddy | RXH | Status | Notes |
|---|---|---|---|
| Multiple sites and listeners | `[[server]]`, `listen = [...]` | ✅ | |
| Path matchers | `uri` prefix, `path` patterns | ✅ | Exact, prefix, suffix and middle `*`. Case sensitive, `*` also crosses `/`. |
| Host matchers (virtual hosts) | `host`, server level `host` | ✅ | Exact and `*.domain`. HTTP servers on the same `listen` address share one socket. |
| Header, method, query, remote IP matchers | `header`, `method`, `query`, `remote_ip` | ✅ | AND across matchers, OR inside lists. No regex or percent decoding. |
| Automatic HTTP to HTTPS redirect | `https_redirect` on a TLS server | ✅ | On by default with `tls = "auto"`, opt in with certificate files. `308` to `https://{host}{uri}` from `[acme] http_port` on the addresses of the TLS server, for its `host` names, after the routes of plain `http` servers on that port. |
| Manual TLS certificates | `tls = { cert, key }` | ✅ | |
| Automatic HTTPS (ACME) | `tls = "auto"`, `[acme]` (`email`, `ca`, `ca_root`, `storage`, `renew_before`, `http_port`, `challenge`), `[acme.dns]` | 🟡 | HTTP-01, TLS-ALPN-01 with `challenge = "tls-alpn-01"` (answered by the TLS listener itself on port 443, no HTTP port) or DNS-01 with `challenge = "dns-01"` (TXT record through Cloudflare, Route 53 or RFC 2136 with TSIG, built in instead of Caddy's plugins; allows wildcard names); one type per configuration, no fallback, with any ACME CA (Let's Encrypt by default), one certificate per `host` name, stored as PEM per CA (switching `ca` obtains new certificates), loaded on start, renewed in the background (daily check, `renew_before` 30 days, one-hour backoff per name that survives reloads) and swapped in without restart. Challenge-only listener (no `SO_REUSEPORT`) when nothing uses `http_port`. One DNS provider per configuration, a fixed `propagation_wait` instead of a propagation check, no `_acme-challenge` CNAME delegation. No on-demand TLS, no ZeroSSL fallback, EAB or ARI. |
| SNI with many certificates, mTLS | `tls` and `client_auth` per server on a shared address | ✅ | Certificate picked by SNI against `host` (exact beats wildcard, the first loaded certificate is the default and only the first TLS server on an address may omit `host`, except for `tls = "auto"` names, whose handshakes fail until their certificate exists). mTLS with `client_auth` (`mode`, `ca`, `header`), one policy per listen address. |
| `file_server` with `browse`, `index`, `hide` | `serve` (string or table: `browse`, `index`, `hide`) | ✅ | Index files and directory listing. `hide` takes names and `*` globs matched per path component. |
| Range requests, ETag, conditional requests | `serve` | ✅ | Streamed files, weak ETag, `Last-Modified`, `If-None-Match`, `If-Modified-Since`, `If-Range`, one range (several send the whole file). |
| `file_server precompressed` | `serve.precompressed` | ✅ | `.zst`, `.br`, `.gz` next to the file. |
| `encode zstd br gzip` | `encode` on a route | ✅ | Files and proxied responses; no minimum length or custom type list. |
| `grace_period` | `grace_period` | ✅ | Default waits forever, like Caddy. Idle keep-alive connections close at once. |
| Connection limit | `max_connections` | ➕ | |
| Structured access logs | `[log]` `format = "json"`, `output`, `access` | ✅ | Text or JSON lines on stdout, stderr or a file; no field selection or rotation. |
| Prometheus metrics | `GET /metrics` on the admin API | ✅ | Requests, latency histogram, L4 connections, per upstream requests, failures, in flight and health. |

## Layer 4 (Caddy needs the caddy-l4 plugin)

| caddy-l4 | RXH | Status | Notes |
|---|---|---|---|
| TCP proxy | `protocol = "tcp"` | ✅ | |
| Server timeouts | `read_header_timeout`, `idle_timeout`, `stream_timeout` | ✅ | `read_header_timeout` defaults to 30s and also bounds the TLS handshake (10s when unset) and the first request of a connection, HTTP/2 included; the others are unset by default. `idle_timeout` also closes idle `tcp` connections. |
| UDP proxy | `protocol = "udp"` | ✅ | Per client sessions, `udp_idle_timeout`. |
| TLS termination | `tls` on a `tcp` server | ✅ | |
| PROXY protocol to upstreams | `proxy_protocol = "v1"` or `"v2"` | ✅ | Before the TLS handshake of TLS backends. |
| Load balancing and health checks | same pools as HTTP | ✅ | Active TCP checks, passive dial failures. |
| Matchers (SNI, HTTP, SSH, ...) and routing | `match = [{ sni, alpn, protocol, http_host, forward }]` on `tcp` servers | 🟡 | TLS passthrough by SNI/ALPN, `protocol` `tls`/`http`/`ssh`, HTTP/1 `Host`; first match wins, replayed bytes, `match_timeout`. No other caddy-l4 matchers (`remote_ip`, regexp, postgres, ...) and no subroutes. |
| Receiving PROXY protocol | `proxy_protocol_in = true` (`http` and `tcp`) | ✅ | v1 and v2, gated by `trusted_proxies`; the source address is the client everywhere. |

## Operations: daemon, CLI and admin API

| Caddy | RXH | Status | Notes |
|---|---|---|---|
| `caddy run` | `rxh run` (or plain `rxh`) | ✅ | `runtime = "per_core"` runs one single-threaded runtime per CPU with `SO_REUSEPORT` listeners instead of the shared work-stealing one (Caddy has the Go scheduler only). |
| `caddy start` | `rxh start` | ✅ | Unix only, PID file and log file. The PID file is locked: a second `rxh start` with the same file fails. |
| `caddy stop` | `rxh stop` | ✅ | Admin API, falls back to the PID file. |
| `caddy reload` | `rxh reload`, `SIGHUP` | ✅ | Zero downtime (`SO_REUSEPORT`); an invalid file keeps the running config. |
| `caddy validate` | `rxh validate` | ✅ | |
| `caddy fmt`, `caddy adapt` | | ❌ | |
| `caddy reverse-proxy` | `rxh reverse-proxy --from --to... [--policy] [--protocol] [--host]` | ✅ | Foreground, no file, admin disabled. `rxh service add --to` persists instead. |
| `caddy file-server` | `rxh file-server [--listen] [--root] [--no-browse] [--encode]` | ✅ | Foreground, no file, admin disabled. `rxh service add --serve` persists instead. |
| Admin API on `localhost:2019` | `[admin] listen` (same default) | ✅ | |
| Admin origin / host checks | same | ✅ | Blocks DNS rebinding from browsers. |
| `GET /config/` | `GET /config` | 🟡 | Read only, JSON, includes live pool members. |
| `POST /load`, `PATCH /config/...` | `POST /reload` | 🟡 | Reloads the file; no config upload yet. |
| `GET /reverse_proxy/upstreams` | same path and fields | ✅ | Plus `GET /pools` with health details. |
| `POST /stop` | same | ✅ | |
| Admin over a unix socket | | ❌ | |
| CLI to manage pools, peers and services | `rxh upstream`, `rxh pool`, `rxh service` | ➕ | Edits the TOML keeping comments, validates, applies live. |
| Split configuration (Nginx `include`) | `include = ["conf.d/*.toml"]` | ➕ | Included files hold `upstream` and `server` only; the CLI edits the defining file; `--file PATH` on `pool add` and `service add`. |
| Effective config | `rxh config show [--json]` | ➕ | Daemon `/config` when reachable, else the merged files. |
| Live status table | `rxh status` | ➕ | |

## Next steps

The full plan, by milestone, is in [ROADMAP.md](ROADMAP.md). Ordered by value
for a load balancer:

1. HTTP/3.

## Roadmap beyond Caddy

Features from other reverse proxies and load balancers (Traefik, Nginx,
Nginx Proxy Manager, HAProxy, Envoy) that RXH should grow into after Caddy
parity. Grouped by area, roughly in priority order inside each group.

### Security and access control

| Feature | Inspired by | Notes |
|---|---|---|
| mTLS on listeners: require and verify client certificates | Traefik TLS options `clientAuth`, Nginx `ssl_verify_client` | Done: per server `client_auth = { mode, ca, header }`, modes like Traefik (`request`, `require`, `verify_if_given`, `require_and_verify`, the default). `header` passes the subject of the verified client certificate to the backends (Traefik `PassTLSClientCert` passes more: the PEM and other fields). Servers sharing an address must have the same `client_auth`, where Traefik picks TLS options by SNI. |
| TLS options: minimum version, cipher suites, curves | Traefik TLS options, Nginx `ssl_protocols` | |
| OCSP stapling | Nginx, HAProxy, Caddy (core) | |
| ACME DNS-01 challenge and wildcard certificates | Traefik, Caddy (plugins) | Done: `[acme] challenge = "dns-01"` and `[acme.dns]`, providers Cloudflare, Route 53 and RFC 2136. |
| IP allow and deny lists | Traefik `IPAllowList`, Nginx `allow`/`deny` | Per route, using `trusted_proxies` for the client IP. |
| Rate limiting (requests per second, burst) per client IP, header or route | Traefik `RateLimit`, Nginx `limit_req`, HAProxy stick tables | Token bucket in memory first, later shared. |
| Concurrent connection limit per client IP | Traefik `InFlightReq`, Nginx `limit_conn` | |
| Basic auth with hashed passwords | Traefik `BasicAuth`, Nginx, NPM access lists | |
| Forward auth (delegate auth to an external service) | Traefik `ForwardAuth`, Nginx `auth_request` | Authelia, Authentik, oauth2-proxy integrations. |
| Security headers presets (HSTS, CSP, frame options), CORS | Traefik `Headers` middleware, NPM | |
| Request body size limit | Nginx `client_max_body_size`, Traefik `Buffering` | Done: `request.max_body_size`, with `request_buffers` for buffering. |
| GeoIP allow/deny (MaxMind database) | Nginx GeoIP2, HAProxy | |

### Service discovery and configuration providers

| Feature | Inspired by | Notes |
|---|---|---|
| Docker provider: discover containers and route by labels | Traefik Docker provider | Labels like `rxh.enable=true`, `rxh.http.host=app.example.com`, `rxh.http.port=8080`, `rxh.pool=web`, `rxh.policy=least_conn`, `rxh.tcp.listen=:5432`; watch the Docker socket for start/stop events and update pools live. |
| Docker image and Compose examples with tags | Traefik images (`traefik:v3`, `v3.x`) | Publish `rxh:latest`, `rxh:0.x`, `rxh:0.x.y` and `-alpine`/distroless variants for amd64 and arm64 on GHCR and Docker Hub from CI on release tags. |
| Docker Swarm, Podman and Kubernetes Ingress / Gateway API providers | Traefik | |
| Consul, etcd and Nomad service discovery | Traefik, HAProxy | |
| File provider watch: reload automatically when the config file changes | Traefik file provider `watch = true` | inotify/kqueue, debounced, same validation as `rxh reload`. |
| Upload a full configuration through the admin API (`POST /load`) | Caddy, Traefik API | |

### Traffic management

| Feature | Inspired by | Notes |
|---|---|---|
| Traffic splitting and canary releases between pools (weighted services) | Traefik weighted services, Envoy | `forward = [{ pool = "v1", weight = 90 }, { pool = "v2", weight = 10 }]`. |
| Request mirroring (shadow traffic) | Traefik mirroring, Nginx `mirror`, Envoy | |
| Circuit breaker on error ratio or latency | Traefik `CircuitBreaker`, Envoy outlier detection | |
| Slow start for recovered or new backends | Nginx Plus `slow_start`, HAProxy `slowstart` | Ramp weight up over a duration. |
| Backend drain and maintenance states through CLI and admin API | HAProxy runtime API (`set server state drain`) | `rxh upstream drain web 10.0.0.1:80`. |
| Consistent hashing with bounded loads, Maglev | Envoy, HAProxy `hash-type consistent` | |
| Retry budgets and hedged requests | Envoy | |
| Per backend connection limits and queueing | HAProxy `maxconn` + queue | |
| Redirect and rewrite rules with regex | Nginx `rewrite`, Traefik `RedirectRegex`/`ReplacePath` | HTTP to HTTPS, `redirect` and `rewrite` with placeholders exist; regex captures do not. |
| Custom error pages per status | Traefik `Errors`, Nginx `error_page`, NPM | |
| gRPC health checks and gRPC-Web | Envoy, Traefik | |
| Splice/zero-copy for layer 4 on Linux | HAProxy | Done: `splice(2)` on `tcp` servers when both sides are plain TCP; buffered copy with TLS, `unix/` backends and on other systems. Always on, no option like `nosplice`. |

### Observability and operations

| Feature | Inspired by | Notes |
|---|---|---|
| OpenTelemetry tracing (W3C trace context propagation) | Traefik, Envoy | |
| Dashboard web UI and management UI | Traefik dashboard, Nginx Proxy Manager | Future plan: an NPM-like UI on top of the admin API (hosts, certificates, access lists, live health). |
| Access log filters and sampling | Traefik access logs | |
| Health check events to webhooks | Caddy events, HAProxy agent checks | |
| Plugin or WASM middleware | Traefik plugins, Envoy WASM | |
