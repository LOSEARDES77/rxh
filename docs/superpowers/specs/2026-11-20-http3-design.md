# HTTP/3 for clients: design

Date: 2026-11-20. Status: waiting for approval.

## Intent

RXH accepts HTTP/3 (QUIC over UDP) from clients, next to HTTP/1.1 and HTTP/2.
It closes the `HTTP/3 ❌` row of `PARITY.md`, the first item of its priority
list. The owner wants HTTP/3 used whenever the client accepts it, so it is on
by default and advertised automatically.

## Goals

- Every HTTP server with TLS also listens for QUIC on the same address and
  port number (UDP), with no configuration.
- Responses over HTTP/1.1 and HTTP/2 on such a server carry
  `Alt-Svc: h3=":<port>"; ma=86400`, so browsers and `curl --http3` switch to
  HTTP/3 on their own.
- An HTTP/3 request goes through the same routes, filters, authentication,
  rate limits, cache, access log, metrics and tracing as any other request.
- The same certificates serve TCP and QUIC: files, `tls = "auto"` (ACME),
  hot reload, OCSP stapling, `client_auth`.
- Reload, graceful stop, hot binary upgrade, systemd socket activation and
  per-core listeners keep working with the UDP socket.

## Non-goals

- HTTP/3 towards backends. Backends stay on HTTP/1.1 and HTTP/2.
- WebSockets and other upgrades over HTTP/3 (extended CONNECT, RFC 9220).
  Clients use HTTP/1.1 or HTTP/2 for those, as browsers already do.
- 0-RTT (early data). Off: early data can be replayed.
- WebTransport, HTTP/3 datagrams, server push.
- An HTTP/3 load generator in `rxh-bench`.
- A `quic` matcher for layer 4 `tcp` servers.

## Anti-goals

- A second certificate path. QUIC must not load certificates on its own.
- A behaviour change for HTTP/1.1 and HTTP/2 clients, other than the new
  `Alt-Svc` header.
- A failure to start because the UDP port is taken when the user did not ask
  for HTTP/3 explicitly (see "Bind failure").

## Constraints

- The TLS stack is rustls with `aws-lc-rs`. `CertResolver`
  (`crates/server/src/tls.rs`) is the only source of certificates.
- Project rules: tests ship with the change, end-to-end check against a real
  `rxh` process, `PARITY.md`, `CHANGELOG.md`, `docs/configuration.md` and
  `ROADMAP.md` updated in the same change.
- Tests bind port 0 on localhost only.

## Approaches

1. **quiche (Cloudflare).** Brings its own BoringSSL, so it cannot use
   `CertResolver`: ACME, OCSP, reload and mutual TLS would be duplicated or
   missing for HTTP/3. Two crypto libraries in the binary. Not async: the
   packet loop, timers and connection table are written by hand. Rejected.
2. **quinn + h3 (picked by the owner).** quinn uses rustls 0.23, the version
   `tokio-rustls` 0.26 already uses, so the QUIC listener takes the same
   `CertResolver` and client verifier. Async on tokio. `h3` is still 0.0.x,
   but it is the HTTP/3 layer of hyper's own ecosystem and of reqwest.
3. **s2n-quic + s2n-quic-h3.** Also rustls-capable, but the HTTP/3 layer is
   not published as a stable crate. Rejected.

## Design

### Configuration (`crates/core`)

New key on `[[server]]`: `http3`, optional boolean.

| Value | Meaning |
|---|---|
| unset | On if the server is `http` with TLS and has no inbound `proxy_protocol`. Off otherwise. |
| `true` | On. Rejected by validation without TLS, on `tcp` and `udp` servers, and with inbound `proxy_protocol` (it has no meaning on QUIC). |
| `false` | Off: no UDP socket, no `Alt-Svc`. |

On a shared listener (several servers on one address) the first member
decides, as it does for `http2`.

`http3` is independent of `http2`. `tls_options` that exclude TLS 1.3 turn
HTTP/3 off when `http3` is unset and are a validation error when it is `true`:
QUIC needs TLS 1.3.

### Listener (`crates/server/src/server.rs`, new `crates/server/src/http3.rs`)

- The daemon (see "Lifecycle") binds a UDP socket on the address of the TCP
  listener, with
  a systemd socket first if one is passed, else a new socket with
  `SO_REUSEPORT` (QUIC sockets only: `udp` servers keep binding without it).
  With port 0 the UDP socket binds the port the TCP listener got.
- **Bind failure.** With `http3 = true` a UDP bind error fails the start, as
  a TCP bind error does. With `http3` unset the error is logged as a warning,
  the server starts without HTTP/3 and sends no `Alt-Svc`.
- The socket backs a `quinn::Endpoint`. Its rustls `ServerConfig` is built by
  the same code path as the TCP one (same `CertResolver`, same `client_auth`
  verifier, same cipher suites) with ALPN `h3` and TLS 1.3 only.
- Per-core mode: one endpoint per runtime, as for TCP. The kernel keeps one
  client address on one socket.

### Requests

- `Service::handle` and `proxy::forward` take `Request<Body>` (the boxed body
  of `http/body.rs`) instead of `Request<hyper::body::Incoming>`. The hyper
  entry point converts with `body::incoming`. No behaviour change.
- `http3.rs` accepts connections, then requests. For each request it builds a
  `Request<Body>` whose body reads the QUIC stream (data, then trailers),
  calls `Service::handle`, and writes the head, the data frames and the
  trailers of the response. Connection-level headers that HTTP/3 forbids
  (`Connection`, `Keep-Alive`, `Transfer-Encoding`, `Upgrade`) are removed
  from the response.
- Wherever the code treats `Version::HTTP_2` specially (authority in the URI
  instead of `Host`, no upgrades, version sent upstream), `HTTP_3` is treated
  the same way.
- The access log and the metrics show the protocol as `HTTP/3.0`.
- `Alt-Svc` is added in the hyper entry point only, never to HTTP/3 responses,
  and never replaces an `Alt-Svc` the backend or a header rule set.

### Limits and timeouts

| Existing setting | Effect on HTTP/3 |
|---|---|
| `max_connections` | One QUIC connection takes one slot of the same semaphore as TCP connections. |
| `max_connections_per_ip` | Same counter as TCP. |
| `idle_timeout` | QUIC `max_idle_timeout`. |
| `read_header_timeout` | A connection that completes no request head in that time is closed. |
| `ip_allow`, `ip_deny`, `trusted_proxies` | Unchanged: checked per request. |
| `client_auth` | Same verifier; the subject reaches `Service::client_subject`. |

### Lifecycle

- **Draining a connection:** it gets a GOAWAY and its in-flight requests
  finish within `grace_period`.
- **Reload (decided by the owner): one QUIC socket outlives the reloads.**
  Two UDP sockets on one port split the packets of every connection between
  them, and RXH starts a new generation on every route change, Docker and
  Kubernetes events included. So the QUIC endpoints do not belong to a
  generation. The daemon keeps them, keyed by listen address, and each
  generation start reconciles the set:
  - Address still serves HTTP/3: the endpoint stays. New connections get the
    TLS configuration, routes, limits and log of the new generation.
    Connections already open keep the generation they were accepted under
    and finish their requests there; that generation's stop waits for them
    within `grace_period`, as it does for TCP connections, then closes them.
  - Address new: bind.
  - Address gone, or `http3` now off: stop accepting, let open connections
    finish within `grace_period`, then close the socket. If the new
    generation needs the UDP port for something else (a `udp` server), close
    at once.
  - A reload that fails leaves the endpoints on the running generation.
  The endpoint must live on a runtime that survives reloads, in per-core
  mode too. If per-core runtimes are rebuilt on reload, QUIC runs on the
  main runtime; one endpoint per core is then left for later.
- **Stop:** as above, then the daemon closes every endpoint.
- **Hot binary upgrade (`rxh upgrade`):** the new process binds its own
  socket; the UDP socket is not leased. When the new process is ready the old
  one closes its endpoints at once: HTTP/3 requests in flight fail and the
  client retries. Known limit, documented. Avoiding it needs packet
  forwarding between processes.

### Known limits, documented

- The `Alt-Svc` port is the port RXH listens on. Behind a port mapping
  (`8443 -> 443`) the advertised port is wrong and clients stay on HTTP/2. A
  key to override it is added when someone needs it.
- UDP must be open in the firewall on that port. Otherwise clients try
  HTTP/3, time out once, and stay on HTTP/2.

## Tests

- `crates/core`: `http3` unset, `true` and `false` parse; the default per
  server type; rejections (no TLS, `tcp`/`udp` server, inbound
  `proxy_protocol`, TLS 1.3 excluded).
- `crates/server/tests/http3.rs`, with a quinn + h3 test client:
  - GET and POST with a body through a real backend; a response larger than
    one QUIC flow-control window; trailers.
  - `Alt-Svc` present over HTTP/1.1 and HTTP/2, absent with `http3 = false`,
    absent over HTTP/3, not replacing a backend's own.
  - `http3 = false` opens no UDP socket.
  - Failure paths: backend refused (502), `max_body_size` (413), idle
    timeout closes the connection, `max_connections` reached, `ip_deny`.
  - `client_auth` mandatory: no certificate fails the handshake.
  - Certificate hot reload reaches QUIC handshakes.
  - Graceful stop lets a request in flight finish.
  - Reload with a request in flight: the request finishes, the same QUIC
    connection keeps working, and a new connection sees the new routes.
  - Reload that turns `http3` off closes the UDP socket.
  - UDP port taken: start fails with `http3 = true`, starts with a warning
    when unset.
- End to end: release binary, temporary configuration, a test backend,
  `curl --http3-only` and `curl --http3` (this machine's curl has HTTP/3).

## Documentation

`PARITY.md` (row and priority list), `CHANGELOG.md`, `ROADMAP.md`,
`docs/configuration.md` (key, example, firewall note, limits),
`docs/rxh.schema.json`, `examples/full.toml`, and the TLS gateway guide.

## Left to the builder

- Exact crate versions and features (`quinn` with `rustls-aws-lc-rs`,
  `h3`, `h3-quinn`), declared once in `[workspace.dependencies]`.
- QUIC transport parameters other than the idle timeout (stream limits,
  windows): library defaults unless a test shows a problem.
- Internal structure of `http3.rs` and of the body adapter.
- How the graceful-stop code is shared with the TCP path.
