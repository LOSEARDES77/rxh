# TLS gateway

This guide puts RXH at the edge as the one place where HTTPS is handled: it
holds the certificates, terminates TLS, redirects plain HTTP, and forwards
plain HTTP (or TLS again) to the services behind it.

## Automatic HTTPS

With `tls = "auto"`, RXH obtains a certificate for every name of `host` from
Let's Encrypt and renews it before it expires. Plain HTTP requests for those
names are redirected to HTTPS.

```toml
[acme]
email = "ops@example.com"      # expiry notices from the CA

[upstream.app]
backends = ["127.0.0.1:3000"]

[[server]]
name = "app"
listen = "0.0.0.0:443"
host = ["example.com", "www.example.com"]
tls = "auto"
forward = "@app"
```

What this needs:

- The DNS names point to this machine, and ports 80 and 443 are reachable
  from the internet. Port 80 answers the HTTP-01 challenge of the CA and
  redirects everything else to HTTPS.
- RXH may bind ports below 1024: run it as root, give the binary
  `CAP_NET_BIND_SERVICE`, or use the systemd unit or the container image,
  which handle it.
- The certificates are kept in `rxh-data/certificates` next to the
  configuration file (`storage`). Keep that directory across restarts:
  Let's Encrypt limits how many certificates a name gets per week.

Try it first against the staging CA, whose certificates browsers do not
trust but whose limits are generous:

```toml
[acme]
email = "ops@example.com"
ca = "https://acme-staging-v02.api.letsencrypt.org/directory"

[upstream.app]
backends = ["127.0.0.1:3000"]

[[server]]
listen = "0.0.0.0:443"
host = "example.com"
tls = "auto"
forward = "@app"
```

Without port 80, use `challenge = "tls-alpn-01"` (port 443 only), or
`challenge = "dns-01"` with a DNS provider, which is also the only way to
get wildcard certificates: see
[`[acme]`](../configuration.md#acme-automatic-https) and
[DNS-01 and wildcard certificates](../configuration.md#dns-01-and-wildcard-certificates).

## Your own certificates

Certificates from your own CA, or from certbot, are given as files. RXH reads
them again when they change, so a renewal needs no restart:

```toml
[upstream.app]
backends = ["127.0.0.1:3000"]

[[server]]
listen = "0.0.0.0:443"
host = "app.example.com"
tls = { cert = "/etc/rxh/app.pem", key = "/etc/rxh/app.key" }
https_redirect = true          # also redirect port 80 to HTTPS
forward = "@app"
```

`cert` holds the certificate chain, leaf first; `key` the private key, both in
PEM. With certbot, point them at
`/etc/letsencrypt/live/NAME/fullchain.pem` and `privkey.pem`, and let the
deploy hook tell RXH right away:

```sh
certbot renew --deploy-hook "rxh cert reload"
rxh cert list                  # what every listener serves, soonest expiry first
```

## Several sites on one address

Servers that list the same address share one socket. During the handshake
RXH picks the certificate whose `host` matches the name the client asks for
(SNI), and the request then goes to that server's routes. Automatic and
file certificates can be mixed:

```toml
[acme]
email = "ops@example.com"

[upstream.shop]
backends = ["127.0.0.1:3001", "127.0.0.1:3002"]

[upstream.blog]
backends = ["127.0.0.1:4000"]

[[server]]
name = "shop"
listen = "0.0.0.0:443"
host = "shop.example.com"
tls = "auto"
forward = "@shop"

[[server]]
name = "blog"
listen = "0.0.0.0:443"
host = ["blog.example.org", "*.blog.example.org"]
tls = { cert = "/etc/rxh/blog.pem", key = "/etc/rxh/blog.key" }
forward = "@blog"
```

A client asking for a name that matches no server gets the first certificate
loaded and a `404`. See
[Virtual hosts on a shared address](../configuration.md#virtual-hosts-on-a-shared-address).

## Hardening

TLS 1.2 and 1.3 with the default suites are safe today. A stricter policy, a
preset of security headers (HSTS among them) and a rate limit per client
look like this:

```toml
[upstream.app]
backends = ["127.0.0.1:3000"]

[[server]]
listen = "0.0.0.0:443"
host = "app.example.com"
tls = { cert = "/etc/rxh/app.pem", key = "/etc/rxh/app.key" }
tls_options = { min_version = "1.3" }
https_redirect = true
security_headers = "basic"
rate_limit = { rate = 100, per = "1s", burst = 200 }
read_header_timeout = "10s"
idle_timeout = "2m"
forward = "@app"
```

The handshake happens before RXH knows the site, so servers sharing an
address must have the same `tls_options` (and `client_auth`). See
[TLS options](../configuration.md#tls-options),
[Security headers](../configuration.md#security-headers) and
[Rate limits](../configuration.md#rate-limits).

## Client certificates

For internal services, RXH can require a certificate signed by your own CA
before any request reaches a backend (mutual TLS). `header` passes the
subject of the certificate to the backend, which can tell clients apart:

```toml
[upstream.internal]
backends = ["127.0.0.1:5000"]

[[server]]
listen = "0.0.0.0:8443"
host = "internal.example.com"
tls = { cert = "/etc/rxh/internal.pem", key = "/etc/rxh/internal.key" }
client_auth = { ca = "/etc/rxh/clients-ca.pem", header = "X-Client-Subject" }
forward = "@internal"
```

```sh
curl --cert alice.pem --key alice.key https://internal.example.com:8443/
```

A client without a valid certificate fails the handshake. Other modes accept
clients without one, see
[Client certificates (mTLS)](../configuration.md#client-certificates-mtls).

## TLS to the backends

When the network between RXH and the backends is not trusted, encrypt that
hop too. `https://` backends are checked against the public roots; a
`[upstream.NAME.tls]` table adds a private CA, the name to check, and a client
certificate if the backends ask for one:

```toml
[upstream.api]
backends = ["https://10.0.0.11:8443", "https://10.0.0.12:8443"]

[upstream.api.tls]
server_name = "api.internal"
ca = "/etc/rxh/internal-ca.pem"
client_cert = "/etc/rxh/rxh-client.pem"
client_key = "/etc/rxh/rxh-client.key"

[[server]]
listen = "0.0.0.0:443"
host = "api.example.com"
tls = { cert = "/etc/rxh/api.pem", key = "/etc/rxh/api.key" }
forward = "@api"
```

See [`[upstream.NAME.tls]`](../configuration.md#upstreamnametls-tls-to-the-backends).

## Behind another balancer

When a cloud balancer forwards raw TCP to RXH, the client address is lost
unless the balancer sends a PROXY protocol header. Accept it only from the
balancer's addresses:

```toml
[upstream.app]
backends = ["127.0.0.1:3000"]

[[server]]
listen = "0.0.0.0:443"
host = "app.example.com"
tls = { cert = "/etc/rxh/app.pem", key = "/etc/rxh/app.key" }
proxy_protocol_in = true
trusted_proxies = ["10.0.0.0/8"]
forward = "@app"
```

To pass TLS through to the backends untouched instead of terminating it,
route by SNI on a `tcp` server: see the [layer 4 guide](layer4.md#tls-passthrough-by-sni).
