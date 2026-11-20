# HTTP load balancer

This guide puts RXH in front of several copies of an HTTP application. By
the end, requests are spread over the backends, a backend that breaks is
taken out of rotation and put back when it recovers, and backends are added,
drained and removed while the proxy runs.

## A first pool

A pool (`[upstream.NAME]`) is a group of backends and the rules to choose
between them. A server (`[[server]]`) listens on an address and forwards to a
pool with `forward = "@NAME"`.

```toml
[upstream.web]
backends = ["127.0.0.1:3001", "127.0.0.1:3002"]

[[server]]
name = "web"
listen = "0.0.0.0:8080"
forward = "@web"
```

```sh
rxh validate -c rxh.toml
rxh run -c rxh.toml
curl http://127.0.0.1:8080/
```

Each request goes to the next backend in turn (`weighted_round_robin`, the
default). The backends see the client in `X-Forwarded-For`, `X-Forwarded-Proto`
and `X-Forwarded-Host`, see
[Headers RXH sets on proxied requests](../configuration.md#headers-rxh-sets-on-proxied-requests).

## Choosing a policy

`policy` decides which backend gets each request. The common choices:

| Policy | Use it when |
|---|---|
| `weighted_round_robin` | Backends are alike, or differ by a known factor (`weight`). The default. |
| `least_conn` | Requests vary in length: long ones pile up on a backend and new ones go elsewhere. |
| `first` | One primary and spares: the first healthy backend gets everything. |
| `ip_hash`, `client_ip_hash` | The same client should land on the same backend, without cookies. |
| `uri_hash` | Backends cache by URL, and each URL should stay on one of them. |
| `cookie` | Sticky sessions: a cookie remembers the backend of each visitor. |

`rxh policies` lists all twelve, and
[Policies](../configuration.md#policies) describes each one. Weights give a
bigger machine a bigger share:

```toml
[upstream.web]
policy = "least_conn"
backends = [
    { address = "127.0.0.1:3001", weight = 3 },
    "127.0.0.1:3002",
]

[[server]]
listen = "0.0.0.0:8080"
forward = "@web"
```

## Health checks

Without health checks a dead backend keeps getting requests, and only a
failed connection moves a request elsewhere. RXH has two kinds of checks,
best used together:

- **Active** checks (`health`) request a URL of every backend in the
  background, and take a backend out after `fails` failed checks in a row.
- **Passive** checks (`passive`) watch real traffic: failed connections,
  the statuses of `unhealthy_status` and responses slower than
  `unhealthy_latency` count as failures, and `max_fails` of them within
  `fail_duration` take the backend out for that long.

```toml
[upstream.web]
policy = "least_conn"
backends = ["127.0.0.1:3001", "127.0.0.1:3002"]

[upstream.web.health]
uri = "/health"
interval = "5s"
timeout = "2s"
fails = 2              # two failed checks in a row take it out
passes = 2             # two good ones put it back

[upstream.web.passive]
fail_duration = "30s"
max_fails = 3
unhealthy_status = ["5xx"]

[[server]]
name = "web"
listen = "0.0.0.0:8080"
forward = "@web"
```

`rxh status` shows the state of each backend. When no backend is healthy the
pool answers `503 Service Unavailable`. To be told when a backend goes down,
add [health event hooks](../configuration.md#upstreamnamehealthhooks-health-event-hooks).

## Retries

A request that cannot reach a backend can try another one. `retries` is the
number of extra attempts; failed connections are always retried, and `GET`,
`HEAD` and `OPTIONS` requests are also retried after a failed round trip.
`retry_match` adds statuses that move a request to another backend:

```toml
[upstream.web]
backends = ["127.0.0.1:3001", "127.0.0.1:3002", "127.0.0.1:3003"]
retries = 2
dial_timeout = "1s"
response_header_timeout = "10s"
retry_match = { status = ["502", "503"] }
retry_budget = { percent = 20, min_retries = 3 }   # bounds retry storms

[[server]]
listen = "0.0.0.0:8080"
forward = "@web"
```

`retry_budget` keeps retries from multiplying the load when every backend is
in trouble, see [Retry budgets and hedging](../configuration.md#retry-budgets-and-hedging).

## Sticky sessions

Applications that keep sessions in memory need every request of a visitor on
the same backend. The `cookie` policy sets a signed cookie with the chosen
backend; new visitors are placed by `fallback`:

```toml
[upstream.web]
policy = "cookie"
key = "lb"                 # cookie name
secret = "change-me"       # signs the cookie value
fallback = "least_conn"
backends = ["127.0.0.1:3001", "127.0.0.1:3002"]

[[server]]
listen = "0.0.0.0:8080"
forward = "@web"
```

When the backend of a cookie is down, the visitor moves to another one.

## Changing backends while it runs

The CLI edits the configuration file (keeping its comments), validates it,
and applies the change to the running daemon. Open connections are not
interrupted.

```sh
rxh upstream add web 127.0.0.1:3003 -w 2   # new backend, live and saved
rxh upstream list web                      # health, requests in flight
rxh upstream drain web 127.0.0.1:3001      # no new requests, sessions finish
rxh upstream ready web 127.0.0.1:3001      # back into rotation
rxh upstream rm web 127.0.0.1:3001         # remove it
```

A deploy usually drains a backend, waits for its `ACTIVE` count to reach 0,
restarts it, and puts it back with `ready`, see
[Draining backends and maintenance](../configuration.md#draining-backends-and-maintenance).
After editing the file by hand, `rxh reload` (or `SIGHUP`) applies it without
dropping connections; with `watch = true` RXH reloads by itself.

## Canary releases

`forward` can split traffic between pools by weight. Moving the weights and
reloading shifts traffic from one version to the other:

```toml
[upstream.stable]
backends = ["127.0.0.1:3001", "127.0.0.1:3002"]
health = { uri = "/health", interval = "5s" }

[upstream.canary]
backends = ["127.0.0.1:4001"]
health = { uri = "/health", interval = "5s" }

[[server]]
listen = "0.0.0.0:8080"
# 95 requests out of 100 to the stable version.
forward = [{ pool = "stable", weight = 95 }, { pool = "canary", weight = 5 }]
```

To test a new version with copies of real traffic instead, see
[Request mirroring](../configuration.md#request-mirroring).

## Several applications on one address

Routes pick a pool by host name, path, method, header or client address.
The first route that matches wins:

```toml
[upstream.api]
policy = "least_conn"
backends = ["127.0.0.1:3001", "127.0.0.1:3002"]

[upstream.site]
backends = ["127.0.0.1:4001"]

[[server]]
name = "front"
listen = "0.0.0.0:8080"
match = [
    { host = "api.example.com", forward = "@api" },
    # /static/app.css is the file /srv/www/static/app.css.
    { path = "/static/*", serve = "/srv/www" },
    { forward = "@site" },
]
```

See [`match` entries](../configuration.md#match-entries-http-routes) for every
matcher, and [Virtual hosts](../configuration.md#virtual-hosts-on-a-shared-address)
to split the sites into separate servers on the same address.

## Watching it

`rxh status` and `rxh top` show the pools live. The admin API serves
Prometheus metrics on `GET /metrics` (requests, latency, per backend
counters and health) and a dashboard on `GET /dashboard`. Access logs go to
standard output by default:

```toml
[log]
format = "json"
output = "/var/log/rxh/access.log"
status = ["5xx"]          # always log server errors
min_duration = "1s"       # and slow requests
sample_ratio = 0.1        # one in ten of the rest

[upstream.web]
backends = ["127.0.0.1:3001", "127.0.0.1:3002"]

[[server]]
listen = "0.0.0.0:8080"
forward = "@web"
```

See [Metrics](../configuration.md#metrics) and [`[log]`](../configuration.md#log).

## Going to production

- Run it under systemd with [`dist/rxh.service`](../../dist/rxh.service), see
  [Running under systemd](../configuration.md#running-under-systemd).
- Set `grace_period` so a stop or reload does not wait forever for slow
  connections.
- Put HTTPS in front: the [TLS gateway guide](tls-gateway.md).
- Upgrade the binary without closing the listening sockets with
  `rxh upgrade`, see
  [Upgrading the binary without downtime](../configuration.md#upgrading-the-binary-without-downtime).
