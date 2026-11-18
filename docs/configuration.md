# Configuration reference

RXH reads one TOML file (`rxh.toml` by default, `-c` or `RXH_CONFIG` to
change it). `rxh validate` checks a file without starting anything, and
[`examples/full.toml`](../examples/full.toml) shows every option at once.

Durations are strings like `250ms`, `5s`, `1m30s` or `2h` (units `ns`, `us`,
`ms`, `s`, `m`, `h`, `d`), or plain integers meaning seconds. Unknown keys
are errors, so typos do not go unnoticed.

The file has four parts, and top level keys must come before the first table:

```toml
grace_period = "30s"     # top level options

[admin]                  # admin API
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
| `fallback` | `"random"` | `fallback` | Policy used when the key of `query`, `header`, `uri_hash` or `cookie` is missing. |
| `retries` | `0` | `lb_retries` | Extra attempts after a failure. |
| `try_duration` | unset | `lb_try_duration` | Keep retrying for this long. Wins over `retries`. |
| `try_interval` | `"250ms"` | `lb_try_interval` | Wait between attempts. |
| `dial_timeout` | `"3s"` | `dial_timeout` | Connect timeout. |
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
| `serve` | | Serve static files from this directory (http only). |
| `uri` | `"/"` | Path prefix for the single `forward` or `serve` of this server (http only). |
| `host` | | Host name(s) this server answers to. In the single pattern form it is the `host` matcher; with `match` it is the default of every entry without its own `host`. |
| `path`, `method`, `header`, `query`, `remote_ip` | | Request matchers for the single pattern form, same as in `match` entries. |
| `match` | | List of patterns instead of `forward`/`serve`/`uri`, see below (http only). |
| `max_connections` | `1024` | Concurrent connections (or UDP sessions). Extra clients wait in the backlog. Alias: `connections`. |
| `tls` | | `{ cert = "cert.pem", key = "key.pem" }`: TLS termination for `http` (with h2 via ALPN) and `tcp`. Servers sharing an address pick their certificate by SNI. |
| `http2` | `true` | Accept HTTP/2 (h2 over TLS, h2c in cleartext). |
| `trusted_proxies` | `[]` | CIDR ranges (or `"private_ranges"`) allowed to set `X-Forwarded-*` and the client IP. |
| `proxy_protocol` | | `"v1"` or `"v2"`: send a PROXY protocol header to the backend (tcp only). |
| `udp_idle_timeout` | `"60s"` | Idle time after which a UDP client session ends (udp only). |
| `request`, `response`, `cache` | | Same as in `match` entries, for the single pattern form. |

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
| `forward` / `serve` | Action, exactly one. |
| `request.headers.set` | Headers to set on the upstream request, `{ name = "value" }`. |
| `request.headers.remove` | Headers to remove from the upstream request. |
| `request.headers.forwarded` | `{ extend = true, by = "name" }`: RFC 7239 `Forwarded` header. `extend = false` replaces client values. |
| `response.headers.set` / `remove` | Same for the response. |
| `response.headers.via` | `{ extend = true }` appends this proxy to `Via`. |
| `response.headers.server` | `{ override = true, version = true }`: replace the backend `Server` header with `rxh/VERSION`. |
| `cache` | `{ enabled = true, ttl_seconds = 30, max_entries = 1024, max_body_bytes = 1048576 }`: cache `200` responses to `GET` requests that have a `Content-Length` up to `max_body_bytes` and no `Set-Cookie`, `no-store` or `private`. |

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
