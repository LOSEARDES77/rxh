# Configuration reference

RXH reads one TOML file (`rxh.toml` by default, `-c` or `RXH_CONFIG` to
change it). `rxh validate` checks a file without starting anything, and
[`examples/full.toml`](../examples/full.toml) shows every option at once.

Durations are strings like `250ms`, `5s`, `1m30s` or `2h` (units `ns`, `us`,
`ms`, `s`, `m`, `h`, `d`), or plain integers meaning seconds. Unknown keys
are errors, so typos do not go unnoticed.

The file has five parts, and top level keys must come before the first table:

```toml
grace_period = "30s"     # top level options

[admin]                  # admin API
[log]                    # access logs
[upstream.NAME]          # named load balancing pools, any number
[[server]]               # listeners, any number
```

## Top level

| Key | Default | Description |
|---|---|---|
| `grace_period` | unset (wait forever) | Time open connections get to finish on stop or reload before they are closed. Idle keep-alive HTTP connections are closed right away. |

## `[admin]`

| Key | Default | Description |
|---|---|---|
| `enabled` | `true` | Serve the admin API. The CLI needs it for live changes; without it, `stop` and `reload` fall back to signals and the PID file. |
| `listen` | `"127.0.0.1:2019"` | Address of the admin API. It has no authentication, keep it on loopback. Changes need a restart. |

## `[log]`

| Key | Default | Description |
|---|---|---|
| `format` | `"text"` | `"text"` keeps one human readable line per request. `"json"` writes one JSON object per line. |
| `output` | `"stdout"` | `"stdout"`, `"stderr"` or a file path (created if missing, always appended to). Only access lines go here; startup and error messages stay on stdout and stderr. |
| `access` | `true` | `false` disables access lines. |

JSON lines for HTTP requests have `ts` (RFC 3339 UTC with milliseconds),
`server` (server name, or its listen address), `remote`, `client_ip` (after
`trusted_proxies`), `method`, `uri`, `proto`, `status`, `duration_ms` (time
until the response headers were ready) and `upstream` (backend address, or
`null` for files, cache hits and errors). TCP connections have `ts`,
`server`, `protocol`, `remote`, `upstream`, `bytes_sent` (client to backend),
`bytes_received`, `duration_ms`; UDP sessions are logged when they end, with
`protocol = "udp"`. Log files are opened when the configuration loads, so
reload after rotating them.

```json
{"ts":"2025-01-01T12:00:00.123Z","server":"web","remote":"203.0.113.5:51000","client_ip":"203.0.113.5","method":"GET","uri":"/api/users","proto":"HTTP/1.1","status":200,"duration_ms":3.4,"upstream":"10.0.0.1:8080"}
```

### Metrics

`GET /metrics` on the admin API returns Prometheus text format 0.0.4.
Counters survive reloads; upstream series are read from the live pools.

| Metric | Labels | Type |
|---|---|---|
| `rxh_build_info` | `version` | gauge, always 1 |
| `rxh_http_requests_total` | `server`, `code` | counter |
| `rxh_http_request_duration_seconds` | `server` | histogram, buckets 0.005 to 10 seconds |
| `rxh_l4_connections_total` | `server`, `protocol` | counter (`tcp` connections, `udp` sessions) |
| `rxh_upstream_requests_total` | `pool`, `upstream` | counter |
| `rxh_upstream_failures_total` | `pool`, `upstream` | counter |
| `rxh_upstream_active_requests` | `pool`, `upstream` | gauge |
| `rxh_upstream_healthy` | `pool`, `upstream` | gauge, 1 or 0 |

`server` is the server name, or its listen address when unnamed (requests that
match no route use the listener name).

## `[upstream.NAME]`: load balancing pools

A pool is a group of backends plus the rules to choose between them. Servers
use it with `forward = "@NAME"`. The same table can also be written inline in
a server: `forward = { policy = "...", backends = [...] }`. Pool names
appear in `rxh status`; inline pools are named `server[INDEX]#PATTERN` or
`SERVER_NAME#PATTERN`.

| Key | Default | Caddy equivalent | Description |
|---|---|---|---|
| `policy` | `"weighted_round_robin"` | `lb_policy` | Selection policy, see below. Aliases: `lb_policy`, `algorithm`. |
| `backends` | `[]` | upstreams | List of addresses or `{ address = "...", weight = N }`. Address forms: `host:port`, `http://host[:port]` (default port 80), `https://host[:port]` (TLS, default port 443) and `unix//absolute/path.sock` (unix socket, not for UDP). Port ranges `"host:8001-8004"` expand to several backends. Host names are resolved on each dial. A pool cannot mix `https://` and plain backends. Alias: `to`. |
| `choose` | `2` | `random_choose <n>` | Sample size of `random_choose`. |
| `key` | | `query [key]`, `header [field]`, `cookie [name]` | Query parameter, header or cookie name. Required for `query` and `header`; `cookie` defaults to `lb`. |
| `secret` | `""` | `cookie name <secret>` | HMAC secret of the `cookie` policy. |
| `fallback` | `"random"` | `fallback` | Policy used when the key of `query`, `header`, `uri_hash` or `cookie` is missing. A policy name, or a table `{ policy, key, choose, secret, fallback }` whose own `fallback` nests another one, for example `fallback = { policy = "header", key = "x-user", fallback = "round_robin" }`. A `query`, `header` or `cookie` fallback uses its own `key`. Cookie values use the secret of the first `cookie` policy in the chain. |
| `retry_match` | unset | `lb_retry_match` | `{ method = [...], status = [...] }`. `method`: methods whose bodyless requests are retried on another backend (default `GET`, `HEAD`, `OPTIONS`). `status`: response statuses (`"503"`, `"5xx"`) that make the proxy drop the response and retry another backend, only for requests retryable under `method`. Retries obey `retries` and `try_duration`; when they run out, the last response is returned. |
| `retries` | `0` | `lb_retries` | Extra attempts after a failure. |
| `try_duration` | unset | `lb_try_duration` | Keep retrying for this long. Wins over `retries`. Round-trip and `retry_match.status` retries wait `try_interval` first and are skipped when that wait would pass this limit, counted from the first attempt. |
| `try_interval` | `"250ms"` | `lb_try_interval` | Wait between attempts. |
| `dial_timeout` | `"3s"` | `dial_timeout` | Connect timeout. |
| `response_header_timeout` | unset | `response_header_timeout` | Time from sending the request to receiving the response headers. On expiry the client gets `504 Gateway Timeout` and the backend counts a passive failure. |
| `read_timeout` | unset | `read_timeout` | An upstream read that makes no progress for this long fails the connection (also idle pooled connections and upgraded tunnels). |
| `write_timeout` | unset | `write_timeout` | An upstream write that makes no progress for this long fails the connection. |
| `h2c` | `false` | `h2c://` | Speak cleartext HTTP/2 to the backends (one shared connection each). |
| `keepalive` | `"2m"` | `keepalive` | How long idle upstream HTTP connections are kept. `"0s"` disables pooling. |
| `keepalive_idle_conns_per_host` | `32` | `keepalive_idle_conns_per_host` | Idle connections kept per backend. |
| `tls` | | `transport http { tls }` | Table of TLS client settings, see below. Setting it enables TLS for every backend, also without `https://`. |
| `health` | | active health checks | Table, see below. |
| `passive` | | passive health checks | Table, see below. |

### Policies

| Policy | Picks | Protocols |
|---|---|---|
| `random` | A random available backend. | all |
| `random_choose` | The least loaded of `choose` random backends. | all |
| `first` | The first available backend in order. Use it for primary/standby failover. | all |
| `round_robin` | Each backend in turn, ignoring weights. | all |
| `weighted_round_robin` | Each backend in proportion to its `weight`, smoothly interleaved. Weight 0 takes a backend out of rotation. | all |
| `least_conn` | The backend with the fewest requests (HTTP) or connections (TCP/UDP) in flight. | all |
| `ip_hash` | Rendezvous hash of the peer IP. | all |
| `client_ip_hash` | Rendezvous hash of the client IP from `X-Forwarded-For`, when the peer is in `trusted_proxies`. | all |
| `uri_hash` | Rendezvous hash of path and query. | http |
| `query` | Rendezvous hash of query parameter `key`. | http |
| `header` | Rendezvous hash of header `key`. | http |
| `cookie` | The backend whose signed value is in cookie `key` (`lb` by default). New clients get one through `fallback` and a `Set-Cookie`. | http |

Only available backends are picked: healthy for active and passive checks,
and below `unhealthy_request_count`. When none is available the client gets
`503` (HTTP) or the connection is closed (TCP/UDP).

### `[upstream.NAME.tls]`: TLS to the backends

Used when the pool has `https://` backends or this table. Backend certificates
are checked against the bundled Mozilla roots (`webpki-roots`) plus `ca`. For
HTTPS backends the proxied `Host` header is the backend host (and port unless
443), as in Caddy 2.11, unless the route sets `request.headers.set.Host`.
TLS is pool-wide: with this table an `http://` or `unix//` backend is dialed with TLS too, and unix sockets send SNI `localhost` unless `server_name` is set. Backends added at runtime (`rxh upstream add`, admin API) follow the same rules: mixing schemes or adding `unix//` to a pool used by a UDP server is rejected with `400`.
Layer 4 pools with this table also connect to the backends over TLS, and
active health checks use TLS too.

| Key | Default | Description |
|---|---|---|
| `server_name` | backend host | SNI name sent and checked against the certificate. |
| `insecure_skip_verify` | `false` | Accept any certificate. Insecure. |
| `ca` | | PEM file with extra trusted root certificates. |
| `client_cert` | | PEM client certificate chain for mutual TLS. Needs `client_key`. |
| `client_key` | | PEM private key of `client_cert`. |

### `[upstream.NAME.dynamic]`: members from DNS

Caddy `dynamic a` and `dynamic srv`. The members are the static `backends`
plus the addresses resolved by the last lookup. A refresh adds new addresses
and removes vanished ones; members present before and after keep their health
state and counters. When a lookup fails the previous set stays and the error
goes to stderr. A reload starts the new pools with the DNS members of the old ones. `rxh upstream rm` of a DNS member comes back at the next refresh. A changed SRV weight replaces the member (its counters reset). Members from DNS are not written back by `rxh upstream`
persistence, and a pool with `dynamic` may have an empty `backends`.

```toml
[upstream.app]
backends = []
dynamic = { a = "app.internal:8080", refresh = "30s", versions = ["ipv4"] }
```

| Key | Default | Description |
|---|---|---|
| `a` | | `name:port`. Every `A`/`AAAA` record becomes an `ip:port` backend. Exactly one of `a` and `srv`. |
| `srv` | | `_service._proto.name`. Every record becomes a `target:port` backend (weight = record weight, at least 1). The target stays a host name, resolved on dial. |
| `refresh` | `"1m"` | Time between lookups. |
| `resolvers` | system | List of `ip:port` DNS servers, queried over UDP and TCP. Default: `/etc/resolv.conf`. |
| `versions` | `["ipv4", "ipv6"]` | Record types for `a`. Error together with `srv`. |

With TLS, an `a` pool has IPs as backend hosts, so SNI and certificate
verification cannot use the host name: when `tls` is set (or `a` is used with
`https://` backends in the same pool) `tls.server_name` is required, and
validation fails without it. `srv` backends keep the target name, so
`server_name` is optional there.

### `[upstream.NAME.health]`: active health checks

Checks run in the background for every backend. With `uri` the check is an
HTTP request; without it, a TCP connect.

| Key | Default | Description |
|---|---|---|
| `uri` | | Path and query to request. |
| `port` | backend port | Check this port on the backend host. |
| `upstream` | | Check this fixed `host:port` for every backend. |
| `interval` | `"30s"` | Time between checks. |
| `timeout` | `"5s"` | Limit for one check. |
| `passes` | `1` | Consecutive passes to mark a backend healthy. |
| `fails` | `1` | Consecutive failures to mark a backend unhealthy. |
| `method` | `"GET"` | HTTP method. |
| `status` | `"2xx"` | Expected status: a code (`200`) or a class (`2xx`). |
| `body` | | Regular expression the body must match. |
| `headers` | `{}` | Extra request headers, for example `{ Host = "api.internal" }`. |
| `request_body` | | Request body. |
| `follow_redirects` | `false` | Follow redirects instead of failing on them. |

### `[upstream.NAME.passive]`: passive health checks

Failures in real traffic take a backend out of rotation for `fail_duration`.
Failed dials always count.

| Key | Default | Description |
|---|---|---|
| `fail_duration` | `"30s"` | How long a failure is remembered. |
| `max_fails` | `1` | Failures within `fail_duration` that mark a backend down. |
| `unhealthy_status` | `[]` | Statuses that count as failures, like `["5xx", 429]`. |
| `unhealthy_latency` | unset | Responses slower than this (time to headers) count as failures. |
| `unhealthy_request_count` | `0` | Requests in flight above which a backend is skipped. `0` means no limit. |

## `[[server]]`: listeners

Each `[[server]]` is a service. Every address in `listen` gets its own
socket, except that `http` servers listing the same address share one socket
(see [Virtual hosts on a shared address](#virtual-hosts-on-a-shared-address)).

| Key | Default | Description |
|---|---|---|
| `listen` | required | `"ip:port"` or a list of them. Port `0` picks a free port. |
| `protocol` | `"http"` | `http`, `tcp` or `udp`. |
| `name` | | Name shown in logs, in `rxh status`, and in `Forwarded: by=` and `Via`. |
| `forward` | | Proxy target: `"@pool"`, `"host:port"`, a list of backends, or an inline pool table. |
| `serve` | | Serve static files: a directory, or a table, see [File server](#file-server) (http only). |
| `uri` | `"/"` | Path prefix for the single `forward` or `serve` of this server (http only). |
| `host` | | Host name(s) this server answers to. In the single pattern form it is the `host` matcher; with `match` it is the default of every entry without its own `host`. |
| `path`, `method`, `header`, `query`, `remote_ip` | | Request matchers for the single pattern form, same as in `match` entries. |
| `match` | | List of patterns instead of `forward`/`serve`/`uri`, see below (http only). |
| `max_connections` | `1024` | Concurrent connections (or UDP sessions). Extra clients wait in the backlog. Alias: `connections`. |
| `tls` | | `{ cert = "cert.pem", key = "key.pem" }`: TLS termination for `http` (with h2 via ALPN) and `tcp`. Servers sharing an address pick their certificate by SNI. |
| `http2` | `true` | Accept HTTP/2 (h2 over TLS, h2c in cleartext). |
| `trusted_proxies` | `[]` | CIDR ranges (or `"private_ranges"`) allowed to set `X-Forwarded-*` and the client IP. |
| `proxy_protocol` | | `"v1"` or `"v2"`: send a PROXY protocol header to the backend (tcp only). |
| `read_header_timeout` | unset | Time a client has to send the request headers; slower clients are disconnected (http only). On a listener shared by several http servers, this and `idle_timeout` come from the first server. |
| `idle_timeout` | unset | Keep-alive HTTP connections without traffic and without requests in flight for this long are closed. For `tcp`, a connection with no traffic in either direction for this long is closed. |
| `stream_timeout` | unset | Maximum lifetime of upgraded HTTP tunnels (WebSockets) and of `tcp` connections. |
| `udp_idle_timeout` | `"60s"` | Idle time after which a UDP client session ends (udp only). |
| `request`, `response`, `cache`, `encode` | | Same as in `match` entries, for the single pattern form. |

A `tcp` or `udp` server takes exactly one `forward` and no `match`.

### Virtual hosts on a shared address

Several `http` servers may list the same `listen` address. They share one
socket and their routes are tried in configuration order, each route keeping
the settings of its own server (`name`, `trusted_proxies`, `host`, headers,
cache). Give each server a `host` so requests reach the right one; a request
no route accepts gets `404`. `http2` and `max_connections` of the shared
socket come from the first server, and `rxh status` shows one listener whose
name joins the server names.

With TLS every server on the address needs its own `tls`, and the certificate
is picked by the SNI name of the client against the servers' `host` patterns:
an exact name beats a wildcard, and a client without SNI (or with a name that
matches nothing) gets the certificate of the first server.

```toml
[[server]]
listen = "0.0.0.0:443"
host = "a.example.com"
tls = { cert = "/etc/rxh/a.pem", key = "/etc/rxh/a.key" }
forward = "@a"

[[server]]
listen = "0.0.0.0:443"
host = ["b.example.com", "*.b.example.com"]
tls = { cert = "/etc/rxh/b.pem", key = "/etc/rxh/b.key" }
forward = "@b"
```

It is a validation error to share an address between an `http` server and a
`tcp` server, between two `tcp` servers or two `udp` servers, or between `http`
servers with and without `tls`. TCP and UDP sockets never conflict, so a `udp`
server may use the address of a `tcp` or `http` server. Port `0` is never
shared: every port `0` entry gets its own free port.

### `match` entries (HTTP routes)

Routes are tried in order; the first one whose matchers all accept the
request wins. Every matcher given on an entry must match (AND). No match
means `404`.

```toml
[[server]]
listen = "0.0.0.0:80"
match = [
    { uri = "/api", forward = "@api" },
    { uri = "/", serve = "/var/www" },
]
```

| Key | Description |
|---|---|
| `uri` | Path prefix. Default `"/"`. Combined (AND) with the matchers below. |
| `host` | Host name or list: exact (`"example.com"`) or leading wildcard (`"*.example.com"`, one or more labels, not `example.com` itself). Case insensitive, the port is ignored. Uses the `Host` header, or the `:authority` of HTTP/2. |
| `path` | Pattern or list; any may match. Exact (`"/health"`), prefix (`"/api/*"`), suffix (`"*.php"`) or with `*` in the middle (`"/user/*/profile"`). Must start with `/` or `*`. `*` matches any run of characters, `/` included. Case sensitive. |
| `method` | Method or list, case insensitive. |
| `header` | `{ name = "value" }`: the header must have this exact value (one of its values); `"*"` only requires the header. |
| `query` | `{ key = "value" }`: same for query parameters (not percent decoded); `"*"` only requires the key. |
| `remote_ip` | List of CIDR ranges the directly connected peer must belong to. |
| `forward` / `serve` | Action, exactly one. `serve` is a directory or a [file server table](#file-server). |
| `encode` | List of `zstd`, `br`, `gzip`: compress this route's responses (files and proxied) with the first one the client accepts in `Accept-Encoding`. Only text, JSON, JavaScript, XML, SVG and WebAssembly responses without a `Content-Encoding`, and never `206` or `304`. Removes `Content-Length`, adds `Vary: Accept-Encoding`, makes a strong `ETag` weak. |
| `request.headers.set` | Headers to set on the upstream request, `{ name = "value" }`. |
| `request.headers.add` | Headers to append a value to, keeping existing ones, `{ name = "value" }`. |
| `request.headers.remove` | Headers to remove from the upstream request. A trailing `*` removes every header with that prefix (`"x-debug-*"`); `"*"` alone is rejected. |
| `request.headers.replace` | `{ "name" = { search = "regex", replace = "text $1" } }`: rewrites each value of the header with a regex. An invalid regex is a configuration error. |
| `request.headers.forwarded` | `{ extend = true, by = "name" }`: RFC 7239 `Forwarded` header. `extend = false` replaces client values. |
| `response.headers.set` / `add` / `remove` / `replace` | Same for the response. |
| `response.headers.via` | `{ extend = true }` appends this proxy to `Via`. |
| `response.headers.server` | `{ override = true, version = true }`: replace the backend `Server` header with `rxh/VERSION`. |
| `cache` | `{ enabled = true, ttl_seconds = 30, max_entries = 1024, max_body_bytes = 1048576 }`: cache `200` responses to `GET` requests that have a `Content-Length` up to `max_body_bytes` and no `Set-Cookie`, `no-store` or `private`. |

Header rules apply in this order: `remove`, `replace`, `set`, `add`.

Virtual hosts on one listener:

```toml
[[server]]
listen = "0.0.0.0:80"
host = "example.com"          # default for entries without `host`
match = [
    { host = "api.example.com", forward = "@api" },
    { path = ["/admin/*"], remote_ip = ["10.0.0.0/8"], forward = "@admin" },
    { method = ["GET", "HEAD"], header = { x-beta = "*" }, forward = "@beta" },
    { forward = "@web" },
]
```

### File server

`serve = "/var/www"` serves a directory with every default below. The table
form changes them:

```toml
serve = { root = ".", browse = true, index = ["index.html"], hide = [".git", "*.pem"], precompressed = ["zstd", "br", "gzip"] }
```

| Key | Default | Description |
|---|---|---|
| `root` | required | Directory to serve. Paths that resolve outside it (`..`, symlinks) answer `404`. |
| `browse` | `true` | List directories that have no index file. With `false` they answer `404`. |
| `index` | `["index.html"]` | Index file names tried in order for a directory. |
| `hide` | `[]` | File names or `*` globs matched against every component of the path; matching paths answer `404` and are left out of listings. |
| `precompressed` | `[]` | `zstd`, `br`, `gzip`: when the client accepts it and `FILE.zst`, `FILE.br` or `FILE.gz` exists, that file is sent with `Content-Encoding` and `Vary: Accept-Encoding`, keeping the type of `FILE`. Tried in the listed order. |

Files are streamed in chunks, never loaded in memory. Only `GET` and `HEAD`
are allowed. Responses carry `Content-Length`, `Last-Modified`, a weak `ETag`
(size and modification time) and `Accept-Ranges: bytes`. `If-None-Match` and
`If-Modified-Since` give `304`. A single `Range` (`a-b`, `a-`, `-n`) gives
`206` with `Content-Range`, a range outside the file gives `416`, and
multiple ranges send the whole file. `If-Range` is honored with a
`Last-Modified` date. Request paths are percent decoded.

### Headers RXH sets on proxied requests

| Header | Value |
|---|---|
| `X-Forwarded-For` | Client IP. Appended to the existing value only when the peer is trusted. |
| `X-Forwarded-Proto` | `http` or `https`. |
| `X-Forwarded-Host` | Original `Host`. |
| `Forwarded` | `for="ip:port";by=NAME;host=HOST;proto=http` |
| `Via` | `1.1 NAME` |
| `Host` | Unchanged. HTTP/2 `:authority` becomes `Host` for HTTP/1.1 backends. |

Hop-by-hop headers (`Connection`, `Keep-Alive`, `TE`, `Upgrade`, ... and every
header listed in `Connection`) are removed in both directions, except what an
upgrade (WebSocket) needs.

## Reloads

`rxh reload`, `SIGHUP` or `POST /reload` re-read the file. The new
configuration starts next to the old one (listeners use `SO_REUSEPORT`), then
the old one stops accepting and drains. If the new file is invalid, the reload
fails and nothing changes. Backends added with `--no-save` are lost on reload;
the default `rxh upstream add` saves them to the file first.
