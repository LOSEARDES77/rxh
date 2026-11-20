# Layer 4: TCP and UDP

RXH balances any protocol that runs over TCP or UDP: databases, Redis, MQTT,
SMTP, DNS, syslog, game servers. At layer 4 it does not read requests, it
moves bytes: a TCP connection or a UDP client session goes to one backend
for its whole life. This guide balances a database, a DNS service, routes
TLS by name without decrypting it, and keeps the client address visible to
the backends.

## Balancing TCP

`protocol = "tcp"` turns a server into a TCP proxy. Pools work as for HTTP;
without `uri`, the health check is a TCP connect:

```toml
[upstream.redis]
policy = "first"               # the primary while it is up, then the replica
backends = ["10.0.0.1:6379", "10.0.0.2:6379"]
health = { interval = "2s", timeout = "1s", fails = 2 }

[[server]]
name = "redis"
listen = "0.0.0.0:6379"
protocol = "tcp"
forward = "@redis"
```

```sh
redis-cli -p 6379 ping
rxh status                     # active connections per backend
```

`first` gives failover; `least_conn` spreads long lived connections such as
database sessions; `ip_hash` keeps a client on one backend. On Linux, plain
TCP on both sides is copied inside the kernel with `splice(2)`.

A failed connection to one backend is retried on the next one when the pool
has `retries`. Connections are not moved once established: when a backend
dies, its clients reconnect and land on a healthy one.

## Timeouts and limits

Long lived connections need explicit bounds:

```toml
[upstream.postgres]
policy = "least_conn"
backends = ["10.0.0.11:5432", "10.0.0.12:5432"]
dial_timeout = "2s"
retries = 1
health = { interval = "5s" }

[[server]]
name = "postgres"
listen = "0.0.0.0:5432"
protocol = "tcp"
forward = "@postgres"
max_connections = 500          # more wait in the backlog
max_connections_per_ip = 50    # one client cannot take them all
idle_timeout = "30m"           # no traffic either way for this long: close
ip_allow = ["10.0.0.0/8"]      # only the private network
```

`stream_timeout` caps the lifetime of a connection whatever its traffic.

## Balancing UDP

`protocol = "udp"` keeps one session per client address: datagrams of a
client go to the same backend, and the replies come back through RXH. A
session ends after `udp_idle_timeout` without traffic.

```toml
[[server]]
name = "dns"
listen = "0.0.0.0:53"
protocol = "udp"
udp_idle_timeout = "10s"       # DNS exchanges are short
forward = { policy = "round_robin", backends = ["10.0.0.21:53", "10.0.0.22:53"] }
```

```sh
dig @127.0.0.1 example.com
```

A `udp` server and a `tcp` server may share an address: DNS over both, for
instance.

## TLS termination for TCP

A `tcp` server with `tls` decrypts the connection and forwards plain bytes,
so a service without TLS support can be offered over TLS:

```toml
[[server]]
name = "mqtts"
listen = "0.0.0.0:8883"
protocol = "tcp"
tls = { cert = "/etc/rxh/mqtt.pem", key = "/etc/rxh/mqtt.key" }
forward = { policy = "least_conn", backends = ["10.0.0.31:1883", "10.0.0.32:1883"] }
```

`client_auth` and `tls_options` work as on HTTP servers, see the
[TLS gateway guide](tls-gateway.md#client-certificates). In the other
direction, a `[upstream.NAME.tls]` table makes RXH connect to the backends
over TLS.

## TLS passthrough by SNI

To route HTTPS to backends that hold their own certificates, RXH reads the
server name from the TLS ClientHello and forwards the connection untouched.
`match` entries are tried in order; the first whose matchers hold wins:

```toml
[upstream.b]
backends = ["10.0.0.2:443", "10.0.0.3:443"]

[[server]]
name = "passthrough"
listen = "0.0.0.0:443"
protocol = "tcp"
match = [
    { sni = "a.example.com", forward = "10.0.0.1:443" },
    { sni = ["b.example.com", "*.b.example.com"], forward = "@b" },
    { forward = "10.0.0.9:443" },  # everything else
]
```

```sh
openssl s_client -connect 127.0.0.1:443 -servername a.example.com
```

RXH never sees the keys or the requests. The bytes it reads to decide are
replayed to the chosen backend.

## Several protocols on one port

The same matchers tell protocols apart, so one port can serve SSH, HTTPS
and plain HTTP, for a network that only lets port 443 through:

```toml
[[server]]
name = "multiplex"
listen = "0.0.0.0:443"
protocol = "tcp"
match_timeout = "2s"
match = [
    { protocol = "ssh", forward = "127.0.0.1:22" },
    { protocol = "tls", forward = "127.0.0.1:8443" },
    { protocol = "http", forward = "127.0.0.1:8080" },
]
```

`http_host` routes plain HTTP by its `Host` header, `alpn` by the protocols
the TLS client offers, `postgres` recognizes PostgreSQL clients, `regexp`
looks for any byte pattern, and `remote_ip` routes by client network. A
connection that matches no entry is closed. See
[`match` entries of `tcp` servers](../configuration.md#match-entries-of-tcp-servers-layer-4-routing).

## Keeping the client address

A layer 4 proxy hides the client: backends see connections from RXH. The
PROXY protocol fixes this with a header at the start of each connection,
which HAProxy, nginx, Postfix, Dovecot and many databases' proxies
understand:

```toml
[upstream.mail]
backends = ["10.0.0.41:25", "10.0.0.42:25"]

[[server]]
name = "smtp"
listen = "0.0.0.0:25"
protocol = "tcp"
proxy_protocol = "v2"          # the backends must expect it
forward = "@mail"
```

In the other direction, behind a balancer that sends the header (AWS NLB,
HAProxy, another RXH), `proxy_protocol_in = true` reads it and treats its
source as the client, for `ip_allow`, `remote_ip`, `ip_hash` and the logs.
Allow it only from the balancer:

```toml
[upstream.redis]
policy = "ip_hash"
backends = ["10.0.0.1:6379", "10.0.0.2:6379"]

[[server]]
listen = "0.0.0.0:6379"
protocol = "tcp"
proxy_protocol_in = true
trusted_proxies = ["10.1.0.0/24"]   # the balancer's addresses
forward = "@redis"
```

See [PROXY protocol input](../configuration.md#proxy-protocol-input) and
[PROXY protocol output](../configuration.md#proxy-protocol-output).

## Managing layer 4 services live

The CLI works the same for every protocol:

```sh
rxh pool add db --policy first -b 10.0.0.1:5432 -b 10.0.0.2:5432 --health-interval 5s
rxh service add --listen 0.0.0.0:5432 --protocol tcp --to @db --name postgres
rxh upstream add db 10.0.0.3:5432
rxh upstream drain db 10.0.0.1:5432     # no new connections; open ones stay
```

Draining a TCP backend lets its open connections finish on their own; RXH
never cuts them, unless `stream_timeout` or `grace_period` says so.
