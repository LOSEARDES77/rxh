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
| `health_request_body` | `request_body` | ✅ | `{env.NAME}` and `{file.PATH}` are replaced when the configuration loads, like in every string value. |
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
| Manual TLS certificates | `tls = { cert, key }` | ✅ | Files read again when their contents change (checked every 5 s, like Traefik's file provider watch), without a restart; a pair that fails to load keeps the previous certificate. `rxh cert reload` forces it (Caddy and Nginx need a config reload). |
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
| `POST /load`, `PATCH /config/...` | `POST /load`, `PATCH /config/upstream/{name}`, `PATCH /config/server/{name or index}` | 🟡 | `/load` takes the whole configuration as TOML or JSON; `PATCH` is a JSON merge patch (RFC 7396) of one pool or server, where Caddy replaces the value at any path. Validated and swapped like a reload, rolled back on failure. In memory only: no autosave, the next reload or restart reads the file again. No `POST`/`PUT`/`DELETE` on `/config/...`, no `@id` paths. `POST /reload` reloads the file. |
| `GET /reverse_proxy/upstreams` | same path and fields | ✅ | Plus `GET /pools` with health details. |
| `POST /stop` | same | ✅ | |
| Admin over a unix socket (`admin unix//path`) | `[admin] listen = "unix//run/rxh/admin.sock"` | ✅ | Mode `0600`, stale sockets replaced, removed on shutdown; no `|0660` permission suffix. The CLI takes `--admin unix//path`. |
| Remote admin (`remote_admin` with mTLS identities) | `[admin] token` | 🟡 | Bearer token on every request, required for a non-loopback `listen`. Plain HTTP: no TLS or client certificates on the admin listener, no per-identity access rules. |
| CLI to manage pools, peers and services | `rxh upstream`, `rxh pool`, `rxh service` | ➕ | Edits the TOML keeping comments, validates, applies live. |
| Split configuration (Nginx `include`) | `include = ["conf.d/*.toml"]` | ➕ | Included files hold `upstream` and `server` only; the CLI edits the defining file; `--file PATH` on `pool add` and `service add`. |
| Environment variables in the config (`{$VAR}`, `{env.*}`, `{file.*}`) | `{env.NAME}`, `{file.PATH}` in any string value | 🟡 | Replaced when the file loads and on every reload, in the main and included files; secret files relative to the file that names them, trailing newlines dropped. An unset variable or unreadable file is an error: no `{$VAR:default}`. Not replaced in `POST /load` uploads, and not at request time. |
| Effective config | `rxh config show [--json]` | ➕ | Daemon `/config` when reachable, else the merged files. |
| Live status table | `rxh status` | ➕ | |
| `caddy completion`, `caddy manpage` | `rxh completions SHELL`, `rxh man [--dir DIR]` | ✅ | Generated from the clap definition: bash, zsh, fish, elvish, PowerShell (no nushell); `rxh man` prints rxh(1), `--dir` writes it plus one page per subcommand. |
| systemd `Type=notify`, socket activation (`fd/N`, `fdgram/N` addresses) | `NOTIFY_SOCKET`, `LISTEN_FDS`; `dist/rxh.service`, `dist/rxh.socket` | ✅ | `READY=1`, `RELOADING=1` with `MONOTONIC_USEC` and `STOPPING=1`, so the unit is `Type=notify-reload` (Caddy's is `Type=notify` with `ExecReload`); a failed reload shows in `STATUS=`. Passed sockets are matched to `listen` addresses (TCP, UDP, and a unix or TCP admin socket) instead of naming descriptors, and are kept across reloads. No watchdog, no `LISTEN_FDNAMES`. |
| Certificate list with expiry | `rxh cert list [--json]`, `GET /certificates`; `rxh cert reload`, `POST /certificates/reload` | ➕ | Every certificate of every TLS listener (files and ACME): names, hosts, expiry, days left, source file, OCSP staple. Traefik shows certificates in its dashboard only. |

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
| TLS options: minimum version, cipher suites, curves | Traefik TLS options, Nginx `ssl_protocols` | Done: per server `tls_options = { min_version, cipher_suites, curves }`. `min_version` `"1.2"` (default) or `"1.3"`; rustls has no TLS 1.0/1.1, so there is no `maxVersion` either. `cipher_suites` by IANA name among the nine AEAD suites of rustls (TLS 1.3 ones included, which Traefik cannot restrict; no CBC or RSA key exchange suites). `curves` among `X25519MLKEM768`, `X25519`, `secp256r1`, `secp384r1` (no `secp521r1`); the client's preference decides among the suites and curves listed, not the order of the lists as in Traefik. Servers sharing an address must have the same `tls_options`, where Traefik picks TLS options by SNI. |
| OCSP stapling | Nginx, HAProxy, Caddy (core) | Done: on by default like Caddy, `tls_options = { ocsp_stapling = false }` turns it off. For file and ACME certificates that name an `http://` OCSP responder and come with their issuer: fetched in the background when the listener starts (the first handshakes may go without), refreshed halfway through the validity, kept until it expires when the responder fails. Only `good` answers are stapled. Not cached on disk across restarts (Caddy does), and the signature of the response is left for the clients to verify (like Nginx without `ssl_stapling_verify`). |
| ACME DNS-01 challenge and wildcard certificates | Traefik, Caddy (plugins) | Done: `[acme] challenge = "dns-01"` and `[acme.dns]`, providers Cloudflare, Route 53 and RFC 2136. |
| IP allow and deny lists | Traefik `IPAllowList`, Nginx `allow`/`deny` | Done: `ip_allow` and `ip_deny` on servers (`http`, `tcp`, `udp`) and on `match` entries of `http` servers, deny winning over allow (Nginx checks its rules in order instead). HTTP takes the client IP from `X-Forwarded-For` when the peer is in `trusted_proxies` (left-most entry, no Traefik `ipStrategy.depth`) and answers `403`; `tcp` closes the connection, `udp` drops the datagram. No `"private_ranges"` shorthand in the lists. |
| Rate limiting (requests per second, burst) per client IP, header or route | Traefik `RateLimit`, Nginx `limit_req`, HAProxy stick tables | Done: `rate_limit = { rate, per, burst, key }` on `http` servers and on their `match` entries, a token bucket per client IP (`X-Forwarded-For` of `trusted_proxies`) or per value of a header (`key = "header:X-Api-Key"`); `429` with `Retry-After`. In memory, shared by the runtimes of one process and emptied by a reload; no state shared between instances, no delaying of requests (Nginx `limit_req` without `nodelay`), no rate limit on `tcp` or `udp` servers. |
| Concurrent connection limit per client IP | Traefik `InFlightReq`, Nginx `limit_conn` | Done: `max_connections_per_ip` on `http` and `tcp` servers; connections over the limit are closed on accept. It counts connections of the peer (or of the PROXY header source), not requests in flight: no `X-Forwarded-For`, and one HTTP/2 connection counts once. Not on `udp` servers. |
| Basic auth with hashed passwords | Traefik `BasicAuth`, Nginx, NPM access lists | Done: `basic_auth = { users, realm }` on `http` servers and on their `match` entries (a route's replaces the server's), `401` with `WWW-Authenticate`. bcrypt hashes only (no MD5 `apr1`, SHA-1 or crypt), inline in the configuration (no `htpasswd` file, Traefik `usersFile`); `rxh hash-password` makes them. Verified credentials are remembered in memory, like Caddy. The `Authorization` header goes to the backend as it is (no Traefik `removeHeader` or `headerField`). |
| Forward auth (delegate auth to an external service) | Traefik `ForwardAuth`, Nginx `auth_request` | Done: `forward_auth = { url, copy_headers, timeout }` on `http` servers and on their `match` entries, with Traefik's protocol (`GET` with the request headers and `X-Forwarded-Method`, `-Proto`, `-Host`, `-Uri`, `-For`; `2xx` passes, any other answer goes to the client), which Authelia, Authentik and oauth2-proxy speak; examples for the three in the documentation. `copy_headers` is Traefik `authResponseHeaders` (no regex form). Every request header is sent (no `authRequestHeaders` filter), never the body (no `forwardBody`), `Set-Cookie` of a `2xx` answer is dropped (no `addAuthCookiesToResponse`), one new connection per check, default CAs only for `https://` services. `502`/`504` when the service fails, where Traefik answers `500`. |
| Security headers presets (HSTS, CSP, frame options), CORS | Traefik `Headers` middleware, NPM | Done: `security_headers` (`"basic"`, `"strict"` or `"off"`) and `cors = { origins, methods, headers, expose_headers, credentials, max_age }` on `http` servers and on their `match` entries (a route's replaces the server's). Presets only fill headers the response lacks (Traefik overwrites); HSTS on TLS servers only, never `preload`; individual values are changed with `response.headers.set` rather than one key per header as in Traefik (no `contentSecurityPolicyReportOnly`, `sslRedirect` is `https_redirect`). CORS origins exact, `https://*.example.com` or `*`, no regex (Traefik `accessControlAllowOriginListRegex`); preflights are always answered by RXH (`204`, before auth, never forwarded), the backend's `Access-Control-Allow-Origin`/`-Credentials`/`-Expose-Headers` are replaced, `Vary: Origin` added (Traefik `addVaryHeader`). Unset `headers` allows what the preflight asks for. |
| Request body size limit | Nginx `client_max_body_size`, Traefik `Buffering` | Done: `request.max_body_size`, with `request_buffers` for buffering. |
| GeoIP allow/deny (MaxMind database) | Nginx GeoIP2, HAProxy | |

### Service discovery and configuration providers

| Feature | Inspired by | Notes |
|---|---|---|
| Docker provider: discover containers and route by labels | Traefik Docker provider | Done: `[docker]` (`endpoint`, `listen`) routes to the running containers labelled `rxh.enable=true` from `rxh.http.host`, `rxh.http.port`, `rxh.tcp.listen`, `rxh.tcp.port`, `rxh.pool` and `rxh.policy`, and follows the `start` and `die` events of the Docker API (unix socket or plain TCP). Containers are dynamic members of their pool: replicas come and go without touching the listeners, which are swapped like a reload only when a pool or its hosts, address or policy change. Opt-in only (no `exposedByDefault`), a container is HTTP or TCP, and unlike Traefik there are no labels for TLS, middlewares, path rules, weights or the network to use (first network by name); the generated routes are plain HTTP on one address. No TLS to the Docker API, no UDP routes. |
| Docker image and Compose examples with tags | Traefik images (`traefik:v3`, `v3.x`) | Done: the `Images` workflow builds [`dist/Dockerfile`](dist/Dockerfile) on native amd64 and arm64 runners for every `v*` tag (checked against the workspace version) and publishes multi-platform `latest`, `0.x`, `0.x.y` (Debian slim), `-alpine` (static musl) and `-distroless` tags on `ghcr.io/loseardes77/rxh`, and on Docker Hub when the `DOCKERHUB_IMAGE` variable is set. Images run as UID 65532 with the configuration at `/etc/rxh/rxh.toml`. Pre-releases only get their own tag. No Compose examples beyond the ones of the Docker provider, no image signing or SBOM beyond the default BuildKit provenance, and a release tag of an older line pushed after a newer one moves `latest` back. |
| Distribution packages (static binaries, `.deb`, `.rpm`, AUR, Homebrew) | Caddy (`.deb`/`.rpm` repositories, AUR, Homebrew), Traefik release archives | Done: the `Release` workflow builds on native runners for every `v*` tag and attaches to the GitHub release static musl archives (x86_64, aarch64), macOS archives (x86_64, aarch64), `.deb` and `.rpm` from [`dist/nfpm.yaml`](dist/nfpm.yaml), the `rxh-bin` `PKGBUILD` and the `rxh.rb` Homebrew formula, with `SHA256SUMS`. Packages ship the systemd units, man pages, completions and a default configuration with only the admin API. The AUR and a Homebrew tap are pushed for releases when the repository has their credentials. No apt or dnf repository, no package signing, no Windows build. |
| Docker Swarm and Podman providers | Traefik Swarm provider, Traefik Docker provider on the Podman socket | Done: `[docker] provider = "podman"` uses the same labels through the Docker compatible API of Podman, and reaches a container at the host port it publishes (wildcard addresses on loopback) before its network address, so rootless containers work. `provider = "swarm"` routes to the running tasks of the services labelled `rxh.enable=true`, from the service labels, at the task address on the first non-ingress network; the pool defaults to the service name and the port to the single published target port. Swarm is polled every `refresh` (default 15s, like Traefik's `refreshSeconds`) since the events of other nodes do not reach a node. No `lbswarm` (routing to the service virtual IP) and no `network` label; Podman pods are not handled apart from their containers. |
| Kubernetes Ingress / Gateway API providers | Traefik | First cut: `[kubernetes]` (`endpoint`, `token`, `ingress_class`, `listen`, `refresh`) routes the hosts of the Ingresses and of the Gateway API HTTPRoutes (v1, skipped when not installed) to the ready EndpointSlice addresses of their Service port, one pool `NAMESPACE-SERVICE-PORT` per port, listed every `refresh` (default 15s) instead of watched. Missing, unlike Traefik: TLS to the API, in-cluster service account and kubeconfig (plain HTTP only, meant for a `kubectl proxy` sidecar); path, header, query and method rules, filters, weighted `backendRefs`, `defaultBackend` and hostless rules (reported and left out); Gateway, GatewayClass, `parentRefs` and `ReferenceGrant` (every HTTPRoute with `hostnames` is served); `spec.tls` and Secrets; status updates; namespace selection; annotations; GRPCRoute, TLSRoute, TCPRoute; `ExternalName` Services. |
| Consul, etcd and Nomad service discovery | Traefik, HAProxy | Done: `[consul]`, `[etcd]` and `[nomad]` route to the service instances of the catalog with the `rxh.*` options of the Docker labels (servers named `consul-POOL`...), listed every `refresh` (default 15s, like Traefik's `refreshInterval`) instead of Consul blocking queries or etcd watches. Consul: services tagged `rxh.enable=true` (Traefik `exposedByDefault = false`), only instances with passing checks. Nomad: native service discovery, every namespace, no health status. etcd: keys `PREFIX/SERVICE/INSTANCE/OPTION` through the v3 JSON gateway, unlike Traefik's KV layout of the dynamic configuration. Plain `http://` endpoints only (no TLS, no Unix socket), no etcd authentication, no Consul Connect, datacenter or namespace selection, and instance addresses must be IPs. |
| File provider watch: reload automatically when the config file changes | Traefik file provider `watch = true` | Done: top level `watch = true` reloads when the main file or an included file changes (files added to or removed from an `include` pattern too), with the same validation and rollback as `rxh reload`. Contents polled every 500 ms and applied once stable for one poll, instead of inotify/kqueue (no new dependency, works on network file systems); a rejected edit is not retried until the files change again. Not a CLI flag (Traefik `--providers.file.watch`), and turning it on needs one manual reload. |
| Upload a full configuration through the admin API (`POST /load`) | Caddy, Traefik API | |

### Traffic management

| Feature | Inspired by | Notes |
|---|---|---|
| Traffic splitting and canary releases between pools (weighted services) | Traefik weighted services, Envoy | Done: `forward = [{ pool = "v1", weight = 90 }, { pool = "v2", weight = 10 }]` on `http` (per request) and `tcp` (per connection) servers, in smooth weighted round robin order like Traefik's WRR service. A pool with no available member gives its turn to the next one, like Traefik's `healthCheck` on weighted services. Unlike Traefik: no sticky cookie, no nesting of splits, no UDP, and the targets are named pools only. |
| Request mirroring (shadow traffic) | Traefik mirroring, Nginx `mirror`, Envoy | Done: `mirror = { pool = "shadow", percent = 10, max_body_size = "1MiB" }` (or a list) on `forward` routes of `http` servers, like Traefik's `mirrors` with `percent` and `maxBodySize`. Copies go out in the background and their responses are dropped. Unlike Traefik: bodies without a `Content-Length` are not copied, the default body limit is 1 MiB instead of unlimited, the share is spread evenly instead of random, and no `mirrorBody = false`. |
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
