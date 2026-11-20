# Changelog

## 0.3.0 (unreleased)

Caddy parity for HTTP routing, upstream transports, timeouts, observability,
the file server and automatic HTTPS, plus layer 4 routing. Configuration
files of 0.2.0 keep working, except for the stricter validation listed under
Changed.

### Added

- Documentation site (mdBook, `mdbook build docs`) with a guide per use
  case: HTTP load balancer, TLS gateway, Docker and layer 4 (TCP and UDP),
  next to the configuration reference. The `Docs` workflow builds it on
  every pull request and publishes it to GitHub Pages from `master`. The
  tests validate every configuration of the guides and check the links
  between pages.
- Hot binary upgrade on Linux: `rxh upgrade`, `SIGUSR2` or `POST /upgrade`
  make the daemon start its own command line again, which runs the binary
  now on disk, and hand it every listening socket (TCP, UDP, admin). The new
  process tells the old one when it serves; the old one then stops accepting
  and drains its connections, so no connection is refused. A new process
  that fails to start (broken configuration, bad binary) leaves the old one
  serving. `rxh start` daemons keep their PID file and log.
- `rxh validate` warns on standard error about deprecated keys and values,
  in the main file and in included files, naming the table and the
  replacement; the exit status stays 0. `docs/migration.md` sets the
  deprecation policy (old spellings keep working until the next major
  release), the upgrade steps and the table of deprecated keys.
- `rxh adapt FILE [--adapter caddyfile|nginx]` prints the RXH TOML of a
  Caddyfile or a simple nginx configuration: sites, reverse proxies with
  their load balancing and health checks, file servers, redirects, headers,
  basic auth, TLS files, `upstream` pools and `stream` servers. What has no
  equivalent is listed on standard error and as comments at the top of the
  output; the result is checked with RXH's own parser.
- `rxh fmt [--check] [FILE...]` rewrites configuration files in one layout
  (no indentation, one space around `=`, a blank line before each table,
  one newline at the end) keeping every comment; `--check` fails when a
  file needs formatting, for CI.
- `rxh schema` prints the JSON schema (draft 07) of the configuration file,
  also shipped as `docs/rxh.schema.json`: with a `#:schema` line, Taplo,
  Even Better TOML and Tombi complete keys, show their documentation and
  flag unknown keys and wrong values in the editor.
- Web UI at `GET /dashboard` on the admin API
  (`http://127.0.0.1:2019/dashboard` by default), embedded in the binary,
  without external resources. Five tabs: Overview (requests, errors and
  latency of the last hour, per pool and per server), Hosts, Pools,
  Certificates and Settings. Every key of the configuration has a form
  field generated from the JSON schema, with a "Form | TOML" switch; a save
  is written to the configuration file, validated, applied with a reload
  and rolled back when either fails, keeping the comments outside the
  table that changed. Backends can be drained, put in maintenance and made
  ready; reload, certificate reload, upgrade and stop have a control. With
  `[admin] token` the page and its two assets are served without the token
  and ask for it before reading any data. `/dashboard/` redirects to
  `/dashboard`. No certificate upload and no accounts.
- Admin routes behind the page: `GET /schema`, `GET /stats/history` (one
  sample every 5 seconds, 720 kept, in memory; `?since=<unix seconds>`
  sends only the newer ones, which is what the page polls), `GET /config/files`,
  `PUT /config/files/{index}`, `POST /config/entities/{kind}`, and `PUT`
  and `DELETE /config/entities/{kind}/{key}`. Secrets are shown as `***`
  and a `***` sent back keeps the stored value.
- `rxh top`: requests per second, errors per second and error share,
  mean upstream latency, requests in flight and healthy backends of every
  pool, refreshed every `--interval` (1s) from `GET /metrics` until
  interrupted or for `--count` refreshes. New metrics behind it:
  `rxh_upstream_responses_total`, `rxh_upstream_server_errors_total` (5xx)
  and `rxh_upstream_response_seconds_total` (time to the response headers).
- Access log filters and sampling: `status` (codes or classes such as
  `"5xx"`) and `min_duration` in `[log]` always write the matching lines
  and drop the others; `sample_ratio` writes a random share of the lines no
  filter keeps (1 by default, 0 once a filter is set). Layer 4 connections
  are filtered by `min_duration` and sampled.
- OpenTelemetry tracing: a `[tracing]` table (`endpoint`, `service_name`,
  `sample_ratio`) sends one span per HTTP request to an OTLP/HTTP
  collector in JSON. The W3C `traceparent` of the caller is continued, or
  a trace is started, and backends receive a `traceparent` naming the span
  of rxh as their parent. JSON access lines gain `trace_id`.
- More layer 4 matchers in the `match` entries of `tcp` servers:
  `remote_ip` (client networks, the PROXY protocol source with
  `proxy_protocol_in`), `regexp` with `regexp_count` (a regular expression
  on the first bytes of the connection) and `protocol = "postgres"` (a
  PostgreSQL startup message or SSL, GSSAPI encryption or cancel request).
- Layer 4 subroutes: `subroute = [...]` instead of `forward` in a `match`
  entry of a `tcp` server nests a list of entries that is only tried on the
  connections the entry matches.
- gRPC health checks: `health = { grpc = true, grpc_service = "NAME" }` on
  a pool checks each backend with the gRPC health checking protocol
  (`grpc.health.v1.Health/Check` over HTTP/2) and keeps it in rotation
  while it answers `SERVING`.
- gRPC-Web: `grpc_web = true` on an `h2c` pool lets browsers call its gRPC
  backends. `application/grpc-web` and base64 `application/grpc-web-text`
  requests are sent on as gRPC, and the trailers of the response come back
  as the last frame of the body.
- Custom error pages: `error_pages = { "404" = "404.html", "5xx" =
  "50x.html" }` on an `http` server replaces the body of the error
  responses rxh answers itself (no matching route, IP lists, rate limits,
  basic auth, body size limits, `502`, `503` and `504` without a backend,
  file server errors). Status and headers are kept; backend responses are
  untouched. Pages are read at start and on reload.
- Retry budgets and hedged requests: `retry_budget = { percent = 20,
  min_retries = 3 }` on a pool bounds the requests retrying at the same
  time to a share of the requests in flight, so retries cannot multiply the
  load of failing backends. `hedge_delay = "100ms"` sends a retryable HTTP
  request that has no response headers after the delay to a second backend
  as well and uses the first response. Hedges count against the budget.
- Per backend connection limits with a queue: `max_connections` on a pool
  caps the requests in flight on each backend, and `queue_size` lets the
  requests over it wait up to `queue_timeout` (default 5s) for a free
  backend instead of getting `503` at once. `GET /pools` reports the
  requests waiting as `queued`.
- Consistent hashing with bounded loads and Maglev: on a pool with a hash
  policy, `hash_balance_factor = 125` keeps every backend at or below 125%
  of the average load by passing the keys of a busy backend on to the next
  one, and `hash_type = "maglev"` maps keys through a Maglev lookup table
  instead of rendezvous hashing.

- Drain and maintenance states: `rxh upstream drain web 10.0.0.1:80`,
  `rxh upstream maintenance` and `rxh upstream ready` (admin API
  `PUT /pools/{name}/upstreams/state`) take a backend out of rotation while
  requests in flight finish. A draining backend keeps its `cookie` sticky
  sessions; one in maintenance gets nothing. Kept across reloads, shown as
  `state` in `GET /pools` and in `rxh upstream list`.
- Slow start: `slow_start = "30s"` on a pool ramps the share of traffic of
  a backend up from nothing to full when it joins at runtime (admin API,
  DNS or service discovery) or comes back from active or passive health
  checks or an open circuit breaker. Applies to the non hash policies.
- Circuit breaker: `circuit_breaker = { error_ratio = 0.3, latency =
  "500ms" }` on a pool takes a backend out of rotation for `open_duration`
  when its share of failed requests, or of responses slower than `latency`,
  reaches a ratio within `window`. Afterwards one trial request decides
  whether it comes back. The admin API reports the state of each backend as
  `circuit`.
- Request mirroring: `mirror = { pool = "shadow", percent = 10 }` on a
  `forward` route sends a copy of the requests (a share of them with
  `percent`) to other pools in the background and drops their responses.
  Bodies are copied up to `max_body_size` (1 MiB by default).
- Traffic splitting for canary releases: `forward = [{ pool = "v1", weight
  = 90 }, { pool = "v2", weight = 10 }]` splits the requests of an `http`
  server, or the connections of a `tcp` server, between named pools by
  weight. A pool whose members are all down gives its share to the others.
- Release packages built by the `Release` workflow on `v*` tags and attached
  to the GitHub release: static musl archives for x86_64 and aarch64 Linux,
  macOS archives, `.deb` and `.rpm` packages (nfpm), the `rxh-bin` AUR
  `PKGBUILD` and a Homebrew formula, with `SHA256SUMS`. Packages install the
  binary in `/usr/bin`, the systemd units, man pages, shell completions and a
  default `/etc/rxh/rxh.toml` that only runs the admin API. The AUR package
  and a Homebrew tap are updated for releases when their credentials are set
  in the repository.
- Official container images for linux/amd64 and linux/arm64, published on
  GHCR (and Docker Hub when configured) by the `Images` workflow on release
  tags: `latest`/`0.x`/`0.x.y` on Debian slim, `-alpine` with a static musl
  binary and `-distroless`. They run `rxh run` as UID 65532 with
  `RXH_CONFIG=/etc/rxh/rxh.toml` and `/var/lib/rxh` as working directory;
  pre-releases never move the floating tags.
- Docker provider: with a `[docker]` table (`endpoint`, `listen`) the daemon
  routes to the running containers labelled `rxh.enable=true`. `rxh.http.host`
  and `rxh.http.port` make an HTTP route, `rxh.tcp.listen` and `rxh.tcp.port`
  a TCP server, `rxh.pool` groups replicas and `rxh.policy` picks the load
  balancing policy. Docker events are followed live: containers join and
  leave their pool as they start and die, and the listeners are swapped
  without dropping connections only when the routes themselves change. A
  container with wrong labels is ignored and reported; an unreachable Docker
  keeps the last routes and is retried.
- Podman and Docker Swarm providers: `[docker] provider = "podman"` follows
  the Docker compatible API of Podman (default endpoint
  `unix:///run/podman/podman.sock`) and reaches containers at the host port
  they publish, which rootless containers need. `provider = "swarm"` routes
  to the running tasks of the services labelled `rxh.enable=true`, with the
  labels of their service, polled every `[docker] refresh` (default 15s).
- Consul, etcd and Nomad providers: `[consul]`, `[etcd]` and `[nomad]`
  tables (`endpoint`, `listen`, `refresh`, plus `token` for Consul and
  Nomad and `prefix` for etcd) route to the instances of a service catalog,
  listed every `refresh` (default 15s), with the `rxh.*` options of the
  Docker labels. Consul: services tagged `rxh.enable=true`, instances whose
  health checks pass, options from `rxh.KEY=VALUE` tags. Nomad: services
  tagged `rxh.enable=true` in every namespace. etcd: keys
  `PREFIX/SERVICE/INSTANCE/OPTION` read through the v3 JSON gateway. An
  unreachable catalog keeps its last routes; a pool belongs to one provider.
- Kubernetes provider, first cut: a `[kubernetes]` table (`endpoint`,
  `token`, `ingress_class`, `listen`, `refresh`) routes the hosts of the
  Ingresses and of the Gateway API HTTPRoutes to the ready endpoints of
  their Services, one pool per Service port, listed every `refresh` (default
  15s). The API is reached over plain HTTP (`kubectl proxy` by default).
  Only whole hosts are routed: path rules, HTTPRoute matches, filters and
  traffic splitting are reported and left out; Gateways, TLS Secrets and
  status updates are not handled yet (see `docs/configuration.md`).
- systemd integration: `rxh run` reports readiness, reloads (with the
  failure reason in `STATUS=`) and shutdown over `NOTIFY_SOCKET`, and takes
  socket-activated listeners from `LISTEN_FDS`, matched to `listen` and
  `[admin] listen` by address and kept across reloads.
  `dist/rxh.service` is now `Type=notify-reload`; `dist/rxh.socket` is a
  socket activation example.
- `rxh completions SHELL` prints the completion script for bash, zsh, fish,
  elvish or PowerShell, and `rxh man` prints the rxh(1) man page
  (`rxh man --dir DIR` writes `rxh.1` and one `rxh-COMMAND.1` page per
  command and subcommand), both generated from the command line definition.
- `{env.NAME}` and `{file.PATH}` placeholders in any string value of the
  configuration file and its included files: the environment variable, or
  the contents of the file without trailing newlines (relative to the
  directory of the file that names it), so tokens, passwords and addresses
  stay out of the configuration. Replaced at startup and on every reload;
  an unset variable or an unreadable file is a configuration error. Keys,
  numbers and booleans are not replaced, and `POST /load` uploads are taken
  as sent.
- `POST /load` on the admin API runs the configuration sent in the body,
  TOML or JSON (what `GET /config` prints loads again), with the validation
  and rollback of a reload. `PATCH /config/upstream/{name}` and
  `PATCH /config/server/{name or index}` merge a JSON merge patch (RFC 7396,
  JSON or TOML) into one pool or server. Neither writes the configuration
  file: the next reload or restart goes back to it. `[admin]` of an upload
  is ignored, `include` is rejected, and `***` secrets keep their running
  value. `POST /load` without a body still reloads the file.
- `watch = true`: the daemon reloads when the configuration file or one of
  its included files changes (added and removed include matches too), with
  the validation and rollback of `rxh reload`. Contents are polled every
  500 ms and a change is applied once it stayed the same for one poll.
- `[admin] listen = "unix//run/rxh/admin.sock"`: the admin API on a unix
  socket only the daemon's user can open (mode `0600`, stale sockets
  replaced, removed on shutdown). `rxh --admin unix//path` and `RXH_ADMIN`
  accept the same form.
- `[admin] token`: every admin request must carry `Authorization: Bearer
  <token>` (`401` otherwise), which allows a non-loopback `listen` for
  remote administration. The CLI sends `--admin-token` / `RXH_ADMIN_TOKEN`,
  or the token of its configuration file; `GET /config` shows it as `***`.
- `security_headers = "basic" | "strict" | "off"` on `http` servers and on
  their `match` entries: presets of `X-Content-Type-Options`,
  `X-Frame-Options`, `Referrer-Policy`, HSTS (TLS servers only) and, with
  `strict`, a same-origin `Content-Security-Policy`,
  `Cross-Origin-Opener-Policy` and `Permissions-Policy`. Headers the
  response already has are kept.
- `cors = { origins, methods, headers, expose_headers, credentials,
  max_age }` on `http` servers and on their `match` entries: RXH answers
  preflights itself (`204`, before authentication) and sets the
  `Access-Control-*` headers of allowed origins, exact or
  `https://*.example.com`, replacing the backend's.
- `basic_auth = { users, realm }` on `http` servers and on their `match`
  entries: HTTP Basic authentication against bcrypt password hashes, `401`
  with `WWW-Authenticate` for everybody else. `rxh hash-password` hashes a
  password read from standard input.
- `forward_auth = { url, copy_headers, timeout }` on `http` servers and on
  their `match` entries: an external service (Authelia, Authentik,
  oauth2-proxy) gets the head of each request with `X-Forwarded-Method`,
  `-Proto`, `-Host`, `-Uri` and `-For`; a `2xx` answer lets the request
  through with the `copy_headers` of the answer, any other answer goes to
  the client. `502` or `504` when the service fails.
- `rate_limit = { rate, per, burst, key }` on `http` servers and on their
  `match` entries: a token bucket of `burst` requests refilled with `rate`
  requests every `per`, for each client IP or each value of a request header
  (`key = "header:X-Api-Key"`). Requests over the limit get
  `429 Too Many Requests` with `Retry-After`.
- `max_connections_per_ip` on `http` and `tcp` servers: connections of a
  client IP over the limit are closed on accept.
- `ip_allow` and `ip_deny` on servers (`http`, `tcp`, `udp`) and on the
  `match` entries of `http` servers: lists of CIDR ranges or addresses of
  the clients accepted and rejected, a deny winning over an allow. HTTP uses
  the client IP from `X-Forwarded-For` of `trusted_proxies` and answers
  `403`; `tcp` closes the connection, `udp` drops the datagram.
- Certificate files (`tls = { cert, key }`) are read again when their
  contents change, checked every 5 seconds, and served to new handshakes
  without a restart or reload. A pair that fails to load keeps the previous
  certificate.
- `rxh cert list [--json]` (`GET /certificates`): the certificates of every
  TLS listener, file and ACME ones, with names, expiry date, days left,
  source file and OCSP staple, soonest expiry first. `rxh cert reload`
  (`POST /certificates/reload`) reads every certificate file now.
- `tls_options = { min_version, cipher_suites, curves, ocsp_stapling }` on
  servers with `tls`: the lowest TLS version accepted (`"1.2"`, the default,
  or `"1.3"`), the cipher suites offered (IANA names) and the key exchange
  groups offered (`X25519MLKEM768`, `X25519`, `secp256r1`, `secp384r1`).
- OCSP stapling, on by default on every TLS listener
  (`tls_options = { ocsp_stapling = false }` turns it off): certificates
  that name an OCSP responder get its answer stapled to the handshake,
  fetched in the background and refreshed halfway through its validity.
- `client_auth = { mode, ca, header }` on servers with `tls`: mutual TLS with
  the four Traefik modes, `"request"`, `"require"`, `"verify_if_given"` and
  `"require_and_verify"` (default). `ca` lists the PEM files of the
  authorities that sign client certificates. On `http` servers `header`
  names a request header that carries the subject of the verified client
  certificate to the backends; a value sent by the client is dropped.
- `[acme] challenge = "dns-01"` with the `[acme.dns]` table (`provider`
  `"cloudflare"`, `"route53"` or `"rfc2136"`, their credentials, `zone_id`,
  `api_url`, `propagation_wait`): `tls = "auto"` certificates validated with
  the ACME DNS-01 challenge through a TXT record at the DNS provider, with
  no open port. It allows wildcard names (`host = "*.example.com"`) on
  `tls = "auto"` servers, stored as `wildcard_.example.com`. The admin API
  shows the provider secrets as `***`.
- `[acme] challenge = "tls-alpn-01"` (default `"http-01"`): `tls = "auto"`
  certificates validated with the ACME TLS-ALPN-01 challenge, answered by
  the TLS listener itself on port 443, so no plain HTTP port is needed.
- Criterion micro benchmarks of `Pool::select` for every load balancing
  policy with 4 and 64 backends: `cargo bench -p rxh-core --bench policies`.
  `cargo test` runs each once as a check.
- Benchmark history: `crates/bench/release.sh` runs `rxh-bench --docker` for
  a release and keeps the JSON in `docs/bench/history`, and
  `rxh-bench --history DIR` renders rxh's numbers across releases with
  regressions of more than 10% marked. Seeded with the 0.2.0 run.
- `runtime = "per_core"` (top level, default `"shared"`): one single-threaded
  runtime per CPU, each with its own `SO_REUSEPORT` listeners and upstream
  connections, instead of one work-stealing runtime. In the `throughput`
  scenario it serves 3% to 96% more requests per second (24% more on 32
  CPUs with 32 connections) at a lower p99: docs/profiling.md.
- `http2_conns_per_host` on a pool: number of HTTP/2 connections kept open
  to each backend with `h2c` (default `1`, as before). Requests use them in
  turn and closed ones are replaced.
- `rxh-bench` scenarios `tls` (HTTPS termination), `http2` (HTTP/2 clients
  over TLS), `large-body` (1 MiB echoed bodies), `websocket`, `udp`, `idle`
  (load next to thousands of idle keep-alive connections) and `memory` (proxy
  RSS per idle connection), and the `--idle-connections` flag. They run on
  rxh, Caddy, NGINX and Traefik.
- Request matchers on routes: `host` (exact or `*.wildcard`, also server
  wide), Caddy style `path` patterns, `method`, `header`, `query` and
  `remote_ip`, ANDed with the `uri` prefix. `rxh service add --host`.
- Virtual hosts: several `http` servers may share a `listen` address (one
  socket, routes tried in order, each keeping its own settings), with TLS
  certificates picked by SNI against `host`.
- Upstream transports: `https://` backends with a pool `tls` table
  (`server_name`, `insecure_skip_verify`, `ca`, `client_cert`/`client_key`),
  `unix//path` backends for HTTP, TCP and health checks.
- Dynamic upstreams: `[upstream.NAME.dynamic]` fills a pool from DNS `A`/`AAAA`
  or `SRV` records and refreshes it, keeping the health and counters of
  unchanged members.
- Timeouts: upstream `response_header_timeout` (`504`), `read_timeout` and
  `write_timeout`; server `read_header_timeout`, `idle_timeout` (also for
  `tcp` connections) and `stream_timeout` (tunnels and `tcp` connections).
- Observability: Prometheus `GET /metrics` on the admin API, and a `[log]`
  table for text or JSON access logs to stdout, stderr or a file; every TCP
  and UDP connection writes one record with its byte counts.
- `retry_match = { method, status }`: methods retried after a failed round
  trip and response statuses retried on another backend. `fallback` accepts
  a nested table with its own `key`, `choose`, `secret` and `fallback`.
- Header rules gain `add`, prefix wildcard `remove` (`x-internal-*`) and regex
  `replace`, applied in the order remove, replace, set, add.
- File server: `serve` takes a table (`root`, `browse`, `index`, `hide`,
  `precompressed`); files stream with `Content-Length`, `Last-Modified`, weak
  `ETag` and `Accept-Ranges`, conditional requests get `304`, single ranges
  `206`/`416`, and precompressed `.zst`/`.br`/`.gz` files are served when the
  client accepts them.
- `encode = ["zstd", "br", "gzip"]` on a route compresses file and proxied
  responses on the fly.
- `include` globs split `upstream` and `server` tables over several files;
  reloads re-read them and CLI edits change the file that defines the pool or
  service (`--file` for new ones). `rxh config show [--json]` prints the
  effective configuration.
- Quick commands without a configuration file: `rxh file-server` and
  `rxh reverse-proxy`.
- Layer 4 routing: `tcp` servers take `match` entries with `sni`, `alpn`,
  `protocol` (`tls`, `http`, `ssh`) and `http_host`, read within
  `match_timeout` and replayed to the backend (TLS passthrough by SNI).
- PROXY protocol output on `http` servers: `proxy_protocol = "v1"` or `"v2"`
  sends a PROXY header with the client address on every new connection to a
  backend, like on `tcp` servers. Upstream keep-alive connections are then
  kept per client connection. Rejected with `h2c` pools.
- PROXY protocol input: `proxy_protocol_in = true` on `http` and `tcp`
  servers, limited to `trusted_proxies` when set; the source address is the
  client address everywhere.
- Automatic HTTPS: `tls = "auto"` obtains and renews certificates for the
  server's `host` names from an ACME CA with HTTP-01 (`[acme]` with `email`,
  `ca`, `ca_root`, `storage`, `renew_before`, `http_port`), stored per CA and
  swapped in without a restart.
- `rewrite = "/v2{path}"` on a route changes the URI that is forwarded or
  served (the query is kept unless the rewrite has one) and
  `request.method` changes the method. Placeholders: `{host}`, `{path}`,
  `{query}`, `{uri}`.
- `handle_response` on `forward` routes, for upstream responses matched by
  `status` and `header`: `replace_status` and `copy_response` keep the
  response (with another status), `serve` (with `rewrite`) and `redirect`
  answer with a file or a redirect instead. `{status}` and `{header.NAME}`
  placeholders allow error pages per status and `X-Accel-Redirect`.
- `redirect = "target"` or `{ to, status }` as a route action next to
  `forward` and `serve`: answers `302` (or `301`, `303`, `307`, `308`) with
  the same placeholders in `Location`.
- `max_conns_per_host` on a pool caps the HTTP/1.1 connections to each
  backend; requests over the limit wait for a free connection.
- `request_buffers` and `response_buffers` on a pool read up to that many
  bytes of a body before passing it on; a body that fits is sent with a
  `Content-Length`. Sizes are bytes or strings like `"64KiB"` and `"10MB"`.
- `request.max_body_size` on a route answers `413 Content Too Large` to
  larger request bodies, before dialing when `Content-Length` says so.
- HTTP to HTTPS redirect: `https_redirect = true` on a TLS `http` server
  answers plain requests for its `host` names on `[acme] http_port` with
  `308` to `https://{host}{uri}`. On by default with `tls = "auto"`.
- `flush_interval` on a pool writes responses with a `Content-Length` to
  the client in batches collected for that long; server-sent events and
  streams of unknown length are still written at once.
- `stream_close_delay` on an `http` server closes upgraded tunnels
  (WebSockets) that long after the listener stops on a reload or shutdown;
  unset keeps them open as before.
- Health event hooks: `[upstream.NAME.health.hooks]` runs a `command`
  (event in `RXH_EVENT`, `RXH_POOL`, `RXH_UPSTREAM`, `RXH_REASON`) and posts
  JSON to a `webhook` when active checks mark a backend `healthy` or
  `unhealthy`, in event order and without delaying the checks.
- Tests: `crates/util` (`rxh-test-util`, not published) with `FlakyBackend`, a
  backend that refuses connections, accepts then closes them, delays its
  response head or truncates its body.
- Tests: property tests (`proptest`) of the load balancing policies in
  `crates/core/tests/policy_properties.rs`: weighted round robin picks each
  backend exactly its weight per cycle, hash policies only move the keys of
  a backend that is removed or marked down (and give them back when it
  returns), and no policy, fallback or cookie ever picks a backend that is
  down, failed or full.
- CI: a `coverage` job runs `cargo llvm-cov --fail-under-lines 85` over the
  workspace without `crates/bench` and fails below 85% line coverage.
- Fuzzing with `cargo fuzz` (`fuzz/`, see `docs/fuzzing.md`): targets for the
  configuration parser, the PROXY protocol parser and writer, the host and
  path matchers, layer 4 protocol sniffing and the response cache keys. A
  `fuzz` CI job runs each target for 60 seconds.
- CI: a `bench-gate` job runs `rxh-bench --quick` and fails when RXH fails
  or cannot run a scenario. Caddy runs alongside for information only.
- `docs/profiling.md`: CPU flame graphs, system call and allocation counts of
  the HTTP hot path, with the scripts to reproduce them (`docs/profiling/`).
  No runtime change.
- `SECURITY.md`: supported versions, private reporting through GitHub
  security advisories, scope. An `Audit` workflow runs `cargo audit` and
  `cargo deny` (policy in `deny.toml`: advisories, licenses, bans, sources)
  on every push, pull request and daily.
- Zero copy for `tcp` servers on Linux: connections with plain TCP on both
  sides (no TLS termination, no upstream TLS, `host:port` backends) are
  relayed with `splice(2)`, so the bytes stay in the kernel. Other
  connections and other systems keep the buffered copy. No configuration.
- HTTP/3 (QUIC): every `http` server with `tls` answers HTTP/3 on the UDP
  port of its `listen` addresses, and adds `Alt-Svc: h3=":<port>"; ma=86400`
  to responses over HTTP/1.1 and HTTP/2. New server key `http3`: unset is
  on, `true` makes a UDP port that cannot be bound a start error, `false`
  turns it off. Open the UDP port in the firewall. Not over HTTP/3:
  WebSockets and other upgrades, 0-RTT, HTTP/3 to backends. Reloads keep the
  UDP socket and drain open connections with a GOAWAY. `rxh upgrade` closes
  QUIC connections at once and the new process binds the UDP port itself;
  HTTP/3 requests in flight fail, and with systemd socket activation the new
  process runs without HTTP/3. A `Content-Length` that is not one valid
  length is answered `400`, and a body that does not match it resets the
  stream with `H3_MESSAGE_ERROR` without completing the backend request.
  Flow control holds at most 1 MiB per request stream and 8 MiB per
  connection. `idle_timeout` closes connections without requests in flight,
  as over TCP; RXH sends a QUIC PING every 10 s and drops silent clients
  after 30 s. With `max_connections_per_ip`, new QUIC clients get a Retry
  first. Servers sharing a listen address must agree on `http3`, and
  `http3 = true` on the UDP port of a `udp` server is a validation error.
  See [HTTP/3](docs/configuration.md#http3).

### Deprecated

- `connections` on `[[server]]`: use `max_connections`.
- `algorithm` on pools: use `policy`.
- The policy values `WRR` and `wrr`: use `weighted_round_robin`.

### Changed

- Without `[admin] token`, the admin API on a unix socket now refuses with
  `403` a request whose `Host` is not `localhost`, `127.0.0.1` or `::1`, as
  the TCP listener already did: once the socket is forwarded to a local
  port (`ssh -L 2019:/run/rxh/admin.sock host`) any web page could reach it
  and, with the routes that save the configuration, add a server. On both
  listeners `Origin` must now be the `Host` of the request, port included,
  so a page served on another local port (`http://localhost:3000`) is
  refused too. The CLI is not affected; a script that talks to the socket
  with another name (`curl --unix-socket ... http://rxh/status`) must use
  `http://localhost/`. With a token nothing changes.
- `rxh_upstream_failures_total` counts requests and connections that failed
  without a response only. Responses that passive health checks treat as
  failures (`unhealthy_status`, `unhealthy_latency`) are no longer added to
  it; 5xx responses are in `rxh_upstream_server_errors_total`.
- The commands that edit the configuration files (`rxh upstream add`,
  `rxh pool add`, `rxh service add`, ...) and the saves of the web UI keep
  the mode of the file they rewrite, and create a file that did not exist
  with mode `0600` instead of the mode the umask gave (usually `0644`). The
  owner is kept only when the process may `chown`. A configuration file
  created by `rxh` run as root is therefore not readable by a daemon
  running as another user until it is given to it (`chown`, or `chmod 640`
  with its group).
- `GET /dashboard` is the web UI and no longer the read only page; with
  `[admin] token`, `/dashboard/app.js` and `/dashboard/app.css` are served
  without the token too.
- `[admin] listen` on an address outside loopback is rejected unless
  `[admin] token` is set; it used to serve the admin API to the network
  without authentication.

- A proxied HTTP request makes 26 allocator calls instead of 84. The
  forwarding headers and the text access line are written into buffers kept
  per thread, values that never change (`Via`, `Server`, the peer IP) are
  built once and shared, the request head is copied for a retry only when
  one is possible, and backend addresses and host names are no longer copied
  for every request. The `throughput` scenario of `rxh-bench` serves 7% more
  requests per second (227 800 to 243 800). Headers sent to backends and
  clients, and log lines, are unchanged. Numbers in
  [docs/profiling.md](docs/profiling.md#after-fewer-allocations).

- `rxh-bench` exits with 1 only when rxh fails a scenario or cannot run it
  (`n/a`); failures of Caddy, NGINX and Traefik are reported but no longer
  change the exit code.

- Response cache keys carry the length of the pool name and of the `Host`
  header. Joined with `|`, a request with `Host: h|/a` for `/b` and one with
  `Host: h` for `/a|/b` shared an entry, so a client could plant a response
  for another host and path (found by the `cache_key` fuzz target).
- `proxy_protocol` on a `tcp` server with TLS backends sends the header
  before the TLS handshake, as the PROXY protocol requires, instead of inside
  the TLS session. A backend that refuses the header now counts as a failed
  dial and is retried on another backend.
- Servers with `tls = "auto"` now listen on `[acme] http_port` (their own
  addresses with that port) all the time, to redirect to HTTPS and answer
  challenges, instead of opening a challenge-only listener on `0.0.0.0`
  while issuing. Starting fails if the port cannot be bound; set
  `https_redirect = false` for the previous behaviour.

- `read_header_timeout` now also bounds the TLS handshake and the time to the
  first complete request of a connection, HTTP/2 included. When unset, TLS
  handshakes get 10 seconds and the first request head 30 seconds, so
  clients that connect and send nothing no longer hold a connection slot.
- `try_duration` also bounds round-trip and status retries, and round-trip
  retries wait `try_interval` between attempts, like dial retries.
- File routes answer methods other than `GET` and `HEAD` with `405`.
- `tls` is an enum in the configuration model (`{ cert, key }` or `"auto"`);
  existing TOML files are unaffected.
- Stricter validation, so `rxh validate` rejects what the daemon would:
  - HTTP request matchers on `tcp` and `udp` servers, and `rewrite`,
    `request`, `response`, `encode` and `cache` on them;
  - `host` on `udp` servers;
  - two servers with the same `name`;
  - a TLS server without `host` that is not the first on a shared address
    (its certificate would never be served);
  - an invalid `health.body` regex and a backend listed twice in one pool.
- Rust 1.100 toolchain (`rust-toolchain.toml`) and the latest dependency
  releases; the file server's HTML assets are embedded without a minifying
  build step.
- Dropped the unmaintained `daemonize` (RUSTSEC-2025-0069) and
  `rustls-pemfile` (RUSTSEC-2025-0134) crates. PEM files are read with
  `rustls-pki-types`; `rxh start` forks with `libc` directly.
- `rxh start` locks the PID file before forking: a second daemon with the
  same PID file now fails in the foreground ("is locked, is rxh already
  running?") instead of exiting silently in the background, and the PID file
  is written before `rxh start` returns.

### Fixed

- A configuration file that cannot be written (a read-only directory, for
  instance) is reported as such by the commands that edit the files and by
  the saves of the web UI: "the configuration file /etc/rxh/rxh.toml cannot
  be written, nothing was changed: ...". The message used to start with
  "the change would break the configuration" and to end with "rollback
  failed, check these files by hand", although nothing had been written.
- An HTTP/3 request carrying `Connection: upgrade` and `Upgrade` no longer
  asks the backend for the upgrade: the fields are dropped, as HTTP/3 has
  no upgrades.
- Reloading a configuration with a UDP server could fail with "Address
  already in use" and keep the old configuration: the stopped listener
  reported its shutdown while its sessions still held the socket. The
  sessions now end before the listener reports that it has stopped.

## 0.2.0 (unreleased)

RXH grows from an HTTP reverse proxy into a general reverse proxy and load
balancer with Caddy level load balancing.

### Added

- `http2_conns_per_host` on a pool: number of HTTP/2 connections kept open
  to each backend with `h2c` (default `1`, as before). Requests use them in
  turn and closed ones are replaced.
- Layer 4 proxying: `protocol = "tcp"` (with TLS termination and PROXY
  protocol v1/v2) and `protocol = "udp"` (per client sessions).
- Named load balancing pools (`[upstream.NAME]`, `forward = "@NAME"`).
- All Caddy load balancing policies: `random`, `random_choose`, `first`,
  `round_robin`, `weighted_round_robin`, `least_conn`, `ip_hash`,
  `client_ip_hash`, `uri_hash`, `query`, `header`, `cookie`, with `fallback`.
- Retries (`retries`, `try_duration`, `try_interval`), active HTTP/TCP health
  checks and passive health checks.
- Keep-alive connection pool to backends and optional h2c.
- `X-Forwarded-For/Proto/Host` with `trusted_proxies`, hop-by-hop header
  removal, quoted RFC 7239 `Forwarded` values.
- Admin API on `127.0.0.1:2019` and the `rxh` CLI: `run`, `start`, `stop`,
  `reload`, `status`, `validate`, `upstream`, `pool`, `service`, `policies`.
- Zero downtime reloads with `SO_REUSEPORT`; invalid files are rejected.
- `grace_period`, closing of idle keep-alive connections on shutdown.
- `rxh-bench`, a correctness and performance comparison against Caddy.
- [PARITY.md](PARITY.md), tracking feature parity with Caddy.

### Changed

- The project is a Cargo workspace: `rxh-core`, `rxh-server`, `rxh-cli`,
  `rxh-bench`. Builds and tests on stable Rust.
- The default policy is smooth weighted round robin (interleaved instead of
  blocks).
- CLI: `--daemon`, `--kill-daemon` and `--reload-config` became `rxh start`,
  `rxh stop` and `rxh reload`.
- TLS listeners negotiate HTTP/2 through ALPN.
- HTML escaping of file names and paths in the directory listing.
