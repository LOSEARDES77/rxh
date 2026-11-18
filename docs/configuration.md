# Configuration reference

RXH reads one TOML file (`rxh.toml` by default, `-c` or `RXH_CONFIG` to
change it). `rxh validate` checks a file without starting anything, and
[`examples/full.toml`](../examples/full.toml) shows every option at once.

Durations are strings like `250ms`, `5s`, `1m30s` or `2h` (units `ns`, `us`,
`ms`, `s`, `m`, `h`, `d`), or plain integers meaning seconds. Unknown keys
are errors, so typos do not go unnoticed.

The file has six parts, and top level keys must come before the first table:

```toml
grace_period = "30s"     # top level options

[admin]                  # admin API
[log]                    # access logs
[acme]                   # automatic HTTPS (tls = "auto")
[upstream.NAME]          # named load balancing pools, any number
[[server]]               # listeners, any number
```

## Top level

| Key | Default | Description |
|---|---|---|
| `grace_period` | unset (wait forever) | Time open connections get to finish on stop or reload before they are closed. Idle keep-alive HTTP connections are closed right away. |
| `include` | `[]` | Files merged into this one, see [Includes](#includes). Must come before the first `[table]`. |

## Includes

`include` splits a configuration into several files, like Nginx `include`:

```toml
include = ["conf.d/*.toml", "extra.toml"]
```

- Paths are relative to the directory of the main file. `*` and `?` work only in
  the last path component, and a glob may match nothing. A path without a
  wildcard must exist.
- An included file may contain only `[upstream.NAME]` and `[[server]]`. Any
  other table, including `include`, is an error.
- The files are merged before validation, so a server can reference a pool
  defined in another file. A pool name defined in two files is an error.
- Servers keep the order main file first, then the included files sorted by
  path. Reloads (`rxh reload`, `SIGHUP`, `POST /reload`) read the files again.
- `rxh upstream add|rm`, `rxh pool rm` and `rxh service rm` edit the file that
  defines the pool or service. `rxh pool add` and `rxh service add` write to
  the main file, or to `--file PATH` (relative to the main file directory),
  which is created if missing and added to `include` if not already covered.
- `rxh config show [--json]` prints the effective merged configuration, from
  the running daemon when reachable, else from the files.

## Quick commands without a file

`rxh file-server [--listen ADDR] [--root DIR] [--no-browse] [--encode]` serves a
directory in the foreground (default `127.0.0.1:8080` and `.`).
`rxh reverse-proxy --from ADDR --to BACKEND... [--policy P] [--protocol http|tcp|udp]
[--host NAME]` proxies in the foreground. Both disable the admin API, and
reload requests answer "no configuration file to reload".

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

## `[acme]`: automatic HTTPS

`tls = "auto"` on an `http` server obtains a certificate for every name of
its `host` list from an ACME certificate authority (Let's Encrypt by default)
with the HTTP-01 challenge, like Caddy's automatic HTTPS:

```toml
[acme]
email = "ops@example.com"

[[server]]
listen = "0.0.0.0:443"
host = ["example.com", "www.example.com"]
tls = "auto"
forward = "@app"
```

| Key | Default | Description |
|---|---|---|
| `email` | | Contact address of the ACME account, for expiry notices. |
| `ca` | `"https://acme-v02.api.letsencrypt.org/directory"` | Directory URL of the CA (must be `https://`). Let's Encrypt staging is `https://acme-staging-v02.api.letsencrypt.org/directory`. |
| `ca_root` | | PEM root certificate trusted for the CA's own HTTPS instead of the system roots, for test CAs like Pebble. Relative to the main file's directory. |
| `storage` | `"rxh-data/certificates"` | Directory of the account and the certificates, relative to the directory of the main configuration file (to the current directory for `rxh reverse-proxy` and other runs without a file). |
| `renew_before` | `"30d"` | Certificates expiring within this time are renewed. |
| `http_port` | `80` | Plain HTTP port: the CA connects to it for HTTP-01 challenges, and [`https_redirect`](#http-to-https-redirect) listens on it. Change it only behind a port forward or for a test CA. |

- Every name in `host` must be an exact, fully qualified domain name (no
  wildcard, IP address or `localhost`); a `tls = "auto"` server without
  `host` is a validation error. Each name gets its own certificate and key.
- Every `http` listener answers `GET /.well-known/acme-challenge/TOKEN` for
  pending challenges before routing. Who answers on `http_port`:
  - A plain (non-TLS) `http` server listening on `http_port` answers them.
    It must be reachable by the CA: one bound to a loopback address only, or
    with `proxy_protocol_in` without a balancer in front that sends the
    PROXY header, cannot pass validation.
  - The [HTTP to HTTPS redirect](#http-to-https-redirect), on by default
    for `tls = "auto"` servers, is such a server: it listens on the
    addresses of the TLS server with `http_port`, all the time. Port 80
    usually needs root or `CAP_NET_BIND_SERVICE`; if it cannot be bound the
    daemon does not start.
  - With `https_redirect = false` and no server of the configuration on
    `http_port`, rxh opens a challenge-only listener on `0.0.0.0:http_port` while it obtains
    certificates, and closes it afterwards. It never shares the port
    (no `SO_REUSEPORT`): if another process holds it, the attempt fails and
    is retried later. A reload that adds a server on that port closes it
    first, and the new server answers the pending challenges. Port 80
    usually needs root or `CAP_NET_BIND_SERVICE`.
  - If only TLS or `tcp` servers use `http_port`, challenges cannot be
    answered: rxh logs it and obtains nothing.
- On start and reload, stored certificates are served right away. Missing
  ones and ones expiring within `renew_before` are obtained in the
  background and swapped in without a restart; a daily check renews them.
  After a failed order a name waits an hour before the next one at the same
  CA, also across reloads (logged once); changing `ca` or fixing a local
  problem (blocked or busy `http_port`) and reloading retries at once. An
  order that is running when the configuration reloads finishes, and the new
  configuration picks the certificate up from the storage. A round gives up
  after one minute for the account plus five minutes per name.
  Until a name has a certificate its TLS handshakes fail and an error is
  logged (once a minute per name). ACME errors never stop the daemon.
- `renew_before` must be shorter than the certificate lifetime (90 days at
  Let's Encrypt), else a certificate is renewed at every daily check and a
  warning is logged.
- The first issuance registers an account, which accepts the CA's terms of
  service, and stores it for later runs.
- Only the daemon (`rxh run`, `rxh start`) manages certificates; an embedded
  `Master` serves `tls = "auto"` names only once a certificate is set through
  `Master::cert_resolvers`.

Storage layout (files `0600`, directories `0700` on unix; private keys are
never logged):

```text
rxh-data/certificates/
  accounts/acme-v02.api.letsencrypt.org_directory.json              account per CA
  acme-v02.api.letsencrypt.org_directory/example.com/cert.pem       certificate chain
  acme-v02.api.letsencrypt.org_directory/example.com/key.pem        private key
```

Accounts and certificates are kept per CA: the directory name is the `ca` URL
without `https://`, other characters than letters, digits, `.` and `-`
replaced by `_`. Switching `ca` (for example from staging to production)
therefore obtains new certificates instead of serving the old CA's.

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
| `max_conns_per_host` | `0` | `max_conns_per_host` | Most HTTP/1.1 connections open to each backend, idle ones included. Requests over the limit wait for a free connection (they are not failed or sent elsewhere; use `passive.unhealthy_request_count` for that). Upgraded tunnels (WebSockets) count. `0` means no limit. Ignored with `h2c`, which shares one connection per backend, and on `tcp` and `udp` servers. |
| `request_buffers` | `0` | `request_buffers` | [Size](#sizes): bytes of each request body read before a backend is picked and dialed. A body that fits is sent with a `Content-Length`; the rest of a larger one is streamed. `0` streams everything. Not applied to upgrades. |
| `response_buffers` | `0` | `response_buffers` | [Size](#sizes): bytes of each response body read before the response goes to the client, which frees the backend from slow clients. Same rules as `request_buffers`; a backend failing meanwhile gives `502`. |
| `tls` | | `transport http { tls }` | Table of TLS client settings, see below. Setting it enables TLS for every backend, also without `https://`. |
| `health` | | active health checks | Table, see below. |
| `passive` | | passive health checks | Table, see below. |

Example: at most 8 connections per backend, uploads up to 64 KiB buffered
before they are sent, responses up to 1 MiB buffered:

```toml
[upstream.app]
backends = ["10.0.0.1:8080", "10.0.0.2:8080"]
max_conns_per_host = 8
request_buffers = "64KiB"
response_buffers = "1MiB"
```

### Sizes

A size is a number of bytes (`4096`) or a string with a unit, case
insensitive: `"512B"`, decimal `"KB"`, `"MB"`, `"GB"` (powers of 1000) or
binary `"KiB"`, `"MiB"`, `"GiB"` (powers of 1024). No fractions.

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
| `hooks` | | Command and webhook for health events. Table, see below. |

#### `[upstream.NAME.health.hooks]`: health event hooks

When active checks mark a backend `healthy` or `unhealthy`, RXH writes the
log line and then runs the pool's hooks. Hooks run in the background, one
event after the other per pool and in the order the events happened, so a
slow hook never delays the checks. Each run has 10 seconds; a hook that fails,
times out or cannot be reached is logged to stderr and the next event still
runs. Passive health checks do not fire hooks.

| Key | Default | Description |
|---|---|---|
| `command` | `[]` | Program and arguments, run without a shell. Its stdout and stderr go to RXH's. |
| `webhook` | | `http://` or `https://` URL that receives a JSON `POST` per event. HTTPS verifies the certificate against the public roots. |

At least one of the two is required. The command gets the event in its
environment: `RXH_EVENT` (`healthy` or `unhealthy`), `RXH_POOL`,
`RXH_UPSTREAM` (the backend address) and `RXH_REASON` (why the last check
failed, empty for `healthy`). It runs first; the webhook is called after it
exits. The webhook body is:

```json
{"event":"unhealthy","pool":"api","upstream":"10.0.0.2:8080","reason":"status 503 Service Unavailable, expected 2xx"}
```

`reason` is left out for `healthy`. Any `2xx` answer counts as delivered;
events are not retried.

```toml
[upstream.api]
backends = ["10.0.0.1:8080", "10.0.0.2:8080"]

[upstream.api.health]
uri = "/health"
interval = "10s"

[upstream.api.health.hooks]
command = ["/usr/local/bin/page-oncall", "--service", "api"]
webhook = "https://hooks.example.com/rxh"
```

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
| `name` | | Name shown in logs, in `rxh status`, and in `Forwarded: by=` and `Via`. Must be unique: inline pools, metrics labels and `rxh service` use it. |
| `forward` | | Proxy target: `"@pool"`, `"host:port"`, a list of backends, or an inline pool table. |
| `serve` | | Serve static files: a directory, or a table, see [File server](#file-server) (http only). |
| `redirect` | | Answer with a redirect: `"target"` or `{ to, status }`, see [Rewrites and redirects](#rewrites-and-redirects) (http only). |
| `uri` | `"/"` | Path prefix for the single `forward`, `serve` or `redirect` of this server (http only). |
| `rewrite` | | New URI for the single `forward` or `serve`, same as in `match` entries (http only). |
| `host` | | Host name(s) this server answers to. In the single pattern form it is the `host` matcher; with `match` it is the default of every entry without its own `host`. Not allowed on `udp` servers. |
| `path`, `method`, `header`, `query`, `remote_ip` | | Request matchers for the single pattern form, same as in `match` entries. |
| `match` | | List of patterns instead of `forward`/`serve`/`redirect`/`uri`: HTTP routes on `http` servers, layer 4 routes on `tcp` servers, see below. |
| `max_connections` | `1024` | Concurrent connections (or UDP sessions). Extra clients wait in the backlog. Alias: `connections`. |
| `tls` | | `{ cert = "cert.pem", key = "key.pem" }`: TLS termination for `http` (with h2 via ALPN) and `tcp`. `"auto"` (http only): certificates from an ACME CA for every name in `host`, see [Automatic HTTPS](#acme-automatic-https). Servers sharing an address pick their certificate by SNI. |
| `https_redirect` | `true` with `tls = "auto"`, else `false` | Redirect plain HTTP requests for this TLS server to HTTPS, see [HTTP to HTTPS redirect](#http-to-https-redirect) (http servers with `tls` only). |
| `http2` | `true` | Accept HTTP/2 (h2 over TLS, h2c in cleartext). |
| `trusted_proxies` | `[]` | CIDR ranges (or `"private_ranges"`) allowed to set `X-Forwarded-*` and the client IP. With `proxy_protocol_in`, also the only peers allowed to send a PROXY header (any peer when empty). |
| `proxy_protocol` | | `"v1"` or `"v2"`: send a PROXY protocol header to the backend (tcp only). |
| `proxy_protocol_in` | `false` | Require a PROXY protocol header (v1 or v2) at the start of every connection and use its source address as the client address, see [PROXY protocol input](#proxy-protocol-input) (http and tcp). Set `trusted_proxies` too: with it empty any directly connecting client can spoof its address. |
| `match_timeout` | `"3s"` | Time a client has to send the PROXY header (`proxy_protocol_in`) and, on `tcp` servers with layer 4 matchers, the first bytes the matchers look at. |
| `read_header_timeout` | `"30s"` (TLS handshake `"10s"`) | Time a client has to send the request headers; slower clients are disconnected (http only). It also bounds the TLS handshake (`http` and `tcp` servers with `tls`) and the time to the first complete request head of a connection, HTTP/2 included, so clients that connect and send nothing are closed too. When unset, request heads get 30 seconds and TLS handshakes 10 seconds. On a listener shared by several http servers, this and `idle_timeout` come from the first server. |
| `idle_timeout` | unset | Keep-alive HTTP connections without traffic and without requests in flight for this long are closed. For `tcp`, a connection with no traffic in either direction for this long is closed. |
| `stream_timeout` | unset | Maximum lifetime of upgraded HTTP tunnels (WebSockets) and of `tcp` connections. |
| `udp_idle_timeout` | `"60s"` | Idle time after which a UDP client session ends (udp only). |
| `request`, `response`, `cache`, `encode` | | Same as in `match` entries, for the single pattern form. |

A `udp` server takes exactly one `forward` and no `match`. A `tcp` server
takes one `forward`, or a `match` list of [layer 4 routes](#match-entries-of-tcp-servers-layer-4-routing).

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
matches nothing) gets the first certificate loaded. Only the first TLS server
on an address may omit `host`: a later host-less server's certificate would
never be served, so it is a validation error. `tls = "auto"` and
`tls = { cert, key }` servers may share an address: each name gets the
certificate of its own server, and the handshake for a `tls = "auto"` name
fails (it never falls back to the first certificate) until its certificate
exists.

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
| `forward` / `serve` / `redirect` | Action, exactly one. `serve` is a directory or a [file server table](#file-server), `redirect` a target or `{ to, status }`, see [Rewrites and redirects](#rewrites-and-redirects). |
| `rewrite` | New URI of the request before it is forwarded or served, see [Rewrites and redirects](#rewrites-and-redirects). |
| `request.method` | Method the request is forwarded or served with instead of its own (upper cased), for example `"POST"`. |
| `request.max_body_size` | [Size](#sizes): largest request body accepted, for example `"10MB"`. Larger ones get `413 Content Too Large`: at once when `Content-Length` says so (nothing is sent to the backend), else when the streamed body passes the limit, which aborts the upstream request (with `request_buffers` at least as large, the backend is never contacted). Unset means no limit; `0` is rejected. Also on `serve` and `redirect` routes, by `Content-Length`. Example: `request = { max_body_size = "10MB" }`. |
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

### Rewrites and redirects

```toml
[[server]]
listen = "0.0.0.0:80"
match = [
    # GET /api/users?page=2 reaches the backend as POST /v2/api/users?page=2.
    { uri = "/api", rewrite = "/v2{path}", request = { method = "POST" }, forward = "@api" },
    # Single page application: every path under /app serves one file.
    { path = "/app/*", rewrite = "/app.html", serve = "/var/www" },
    { uri = "/old", redirect = "/new{path}" },                                   # 302
    { host = "example.com", redirect = { to = "https://www.example.com{uri}", status = 301 } },
    { forward = "@web" },
]
```

`rewrite` and `redirect` targets take these placeholders:

| Placeholder | Value |
|---|---|
| `{host}` | Host of the request (`Host` header or HTTP/2 `:authority`), without the port. |
| `{path}` | Path of the request. |
| `{query}` | Query string without `?`, empty when there is none. |
| `{uri}` | Path, plus `?query` when there is one. |

- `rewrite` must start with `/` and may carry a query
  (`"/search?q={query}"`); without one the query of the request is kept. It
  applies after the route is chosen, so routes are not matched again, and
  it changes what `forward` sends upstream and which file `serve` looks up
  (on a `serve` route with a `uri` prefix, the rewritten path must still
  start with it). Access logs keep the method and URI the client sent. A request
  whose rewrite is not a valid URI (only a malformed `Host` with `{host}`
  can cause it) gets `400`.
- `redirect` answers with an empty body and `Location` set to the rendered
  target, which may be absolute or a path. `status` is `302` by default;
  `301`, `303`, `307` and `308` are accepted. `rewrite` and `request.method`
  are rejected on a `redirect` route.
- Unknown placeholders, spaces and control characters are configuration
  errors.

### HTTP to HTTPS redirect

```toml
[[server]]
listen = "0.0.0.0:443"
host = "example.com"
tls = { cert = "cert.pem", key = "key.pem" }
https_redirect = true    # implied by tls = "auto"
forward = "@app"
```

`https_redirect` on a TLS `http` server makes rxh also listen for plain HTTP
on the addresses of that server with the port replaced by `[acme] http_port`
(80 by default; here `0.0.0.0:80`) and answer `308` with
`Location: https://{host}{uri}`, plus `:port` when the server does not
listen on 443 (the first port of `listen` is used).

- It is on by default for `tls = "auto"` servers (`https_redirect = false`
  turns it off) and off for servers with certificate files.
- Only requests for the `host` names of the TLS server are redirected;
  without `host`, every request is. Other requests get `404`.
- Plain `http` servers of the configuration on the same address share the
  socket and their routes go first, so they can keep serving some paths or
  hosts over HTTP; a route there that matches everything disables the
  redirect.
- The redirect listener inherits `trusted_proxies`, `proxy_protocol_in`,
  `max_connections` and the timeouts of the TLS server, and shows up as an
  extra listener in `rxh status`.
- `https_redirect = true` is a configuration error on servers without `tls`,
  when every `listen` port is 0, and when a TLS or `tcp` server uses
  `http_port`. The automatic redirect of `tls = "auto"` is skipped silently
  in the last two cases.

### `match` entries of `tcp` servers (layer 4 routing)

A `tcp` server can pick the backend from the first bytes of each connection,
like caddy-l4. Entries are tried in order and the first one whose matchers
all hold wins; an entry without matchers matches everything (the default).
When no entry matches, the connection is closed.

```toml
# TLS passthrough: route HTTPS by SNI without terminating it.
[[server]]
listen = "0.0.0.0:443"
protocol = "tcp"
match = [
    { sni = "a.example.com", forward = "10.0.0.1:443" },
    { sni = ["b.example.com", "*.b.example.com"], alpn = "h2", forward = "@b_h2" },
    { protocol = "ssh", forward = "10.0.0.9:22" },
    { forward = "@fallback" },
]
```

| Key | Description |
|---|---|
| `sni` | Server name or list from the TLS ClientHello, same patterns as `host`: exact or leading `*.` wildcard, case insensitive. |
| `alpn` | ALPN protocol or list; matches if the client offers any of them. |
| `protocol` | `"tls"` (a TLS ClientHello), `"http"` (an HTTP/1 request line, or the h2c preface `PRI * HTTP/2.0`) or `"ssh"` (an `SSH-` banner). |
| `http_host` | `Host` header of an HTTP/1 request, same patterns as `host`; the port is ignored. |
| `forward` | Required: where matching connections go. Each entry has its own pool. |

To decide, RXH reads up to 16 KiB from the client for at most
`match_timeout`; these bytes are not lost, they are replayed to the chosen
backend. A client that sends nothing in time (protocols where the server
speaks first), closes early or sends something unrecognized only matches
entries without matchers. The ClientHello may span several TLS records.
When the first entry has no matchers, nothing is read.

When the `tcp` server terminates TLS (`tls` is set), `sni` and `alpn` come
from the completed handshake and `protocol = "tls"` matches every
connection; `http_host` and the other protocols look at the decrypted
stream. The server offers the `alpn` values of its entries during the
handshake and `alpn` matches the negotiated protocol. Entries using only
`sni`, `alpn` or `protocol = "tls"` then need no extra read.

HTTP request matchers (`uri`, `host`, `path`, `method`, `header`, `query`,
`remote_ip`) are rejected on `tcp` and `udp` servers, and layer 4 matchers on
`http` and `udp` servers. `rewrite`, `request`, `response`, `encode` and `cache` only
apply to `http` servers and are rejected elsewhere.

### PROXY protocol input

Behind a load balancer that speaks the PROXY protocol (HAProxy, AWS NLB,
another RXH with `proxy_protocol`), set `proxy_protocol_in = true` on an
`http` or `tcp` server. Every connection must then start with a v1 or v2
header, read within `match_timeout`, before TLS and before any matching. Its
source address becomes the client address everywhere: logs, `ip_hash`,
`remote_ip`, `X-Forwarded-For`, `Forwarded`, `trusted_proxies` decisions and
the outgoing `proxy_protocol` header of `tcp` servers. Connections without a
valid header are closed. A v2 `LOCAL` header (health checks of the balancer),
`UNKNOWN` (v1) and unix or unspecified addresses keep the real peer.

```toml
[[server]]
listen = "0.0.0.0:80"
proxy_protocol_in = true
trusted_proxies = ["10.0.0.0/8"]   # only the balancer may send the header
forward = "@web"
```

If `trusted_proxies` is not empty, only peers in it may connect; others are
closed. **With an empty `trusted_proxies`, any client that can reach the
listener directly can send its own PROXY header and claim any source
address**, defeating `remote_ip`, `ip_hash`, logs and `X-Forwarded-For`. Set
`trusted_proxies` to the balancer's addresses, or make sure only the balancer
can reach the port. Servers sharing a listen address must agree on `proxy_protocol_in`,
and the first one's `trusted_proxies` and `match_timeout` apply to the
header.

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
