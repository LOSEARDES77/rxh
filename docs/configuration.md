# Configuration reference

RXH reads one TOML file (`rxh.toml` by default, `-c` or `RXH_CONFIG` to
change it). `rxh validate` checks a file without starting anything, and
[`examples/full.toml`](../examples/full.toml) shows every option at once.

Durations are strings like `250ms`, `5s`, `1m30s` or `2h` (units `ns`, `us`,
`ms`, `s`, `m`, `h`, `d`), or plain integers meaning seconds. Unknown keys
are errors, so typos do not go unnoticed.

The file has seven parts, and top level keys must come before the first table:

```toml
grace_period = "30s"     # top level options
runtime = "per_core"
watch = true

[admin]                  # admin API
[log]                    # access logs
[acme]                   # automatic HTTPS (tls = "auto")
[docker]                 # routes to Docker, Podman or Swarm containers from their labels
[consul]                 # routes to Consul catalog services from their tags
[etcd]                   # routes to service instances written in etcd
[nomad]                  # routes to Nomad services from their tags
[kubernetes]             # routes the hosts of Ingresses and HTTPRoutes to their Services
[upstream.NAME]          # named load balancing pools, any number
[[server]]               # listeners, any number
```

## Top level

| Key | Default | Description |
|---|---|---|
| `grace_period` | unset (wait forever) | Time open connections get to finish on stop or reload before they are closed. Idle keep-alive HTTP connections are closed right away. |
| `runtime` | `"shared"` | Threading model of the listeners: `"shared"` or `"per_core"`, see [Runtime](#runtime). |
| `watch` | `false` | Reload when the file or one of its included files changes, see [Watching the files](#watching-the-files). |
| `include` | `[]` | Files merged into this one, see [Includes](#includes). Must come before the first `[table]`. |

## Runtime

```toml
runtime = "per_core"

[[server]]
listen = "0.0.0.0:80"
forward = "@api"
```

| Value | Meaning |
|---|---|
| `"shared"` | One work-stealing runtime with a thread per CPU. Every address has one listener, and the tasks of a connection may run on any thread. The default. |
| `"per_core"` | One single-threaded runtime per CPU the process may use. Each one has its own `SO_REUSEPORT` listener on every `http` and `tcp` address and its own upstream connections, so a request stays on one thread from accept to response. |

`per_core` served 3% to 96% more requests per second than `shared` in the
`throughput` benchmark, with less CPU per request: see
[profiling.md](profiling.md#after-one-runtime-per-core). It is not the default
because of what it gives up:

- The kernel picks the listener of a new connection from a hash of the client
  address and port. A runtime never hands work to another one, so a few busy
  connections that land on the same runtime share one CPU while others idle.
  `per_core` fits many connections; `shared` fits a few heavy ones.
- It needs a kernel that spreads connections over `SO_REUSEPORT` listeners:
  Linux. Elsewhere keep `shared`.

What is counted per runtime and what stays global:

- `keepalive_idle_conns_per_host` and `http2_conns_per_host` apply to each
  runtime: with 8 CPUs and `http2_conns_per_host = 2`, a backend gets up to
  16 HTTP/2 connections.
- `max_connections`, `max_conns_per_host`, rate limits, the cache, health,
  load balancing state and metrics are shared by all runtimes, as with
  `shared`.
- `udp` servers keep one socket, on the first runtime.
- The admin API, health checks and certificate management run on the main
  runtime. `rxh status` shows each listener once, with the state of the
  first runtime.

The number of runtimes is the number of CPUs the process may run on
(`taskset` and container CPU limits count). The key is not allowed in
included files, and a reload may change it.

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

## Watching the files

```toml
watch = true
include = ["conf.d/*.toml"]
```

With `watch = true` the daemon reads the configuration file and the files
its `include` list matches every 500 ms and reloads when their contents
change, exactly as `rxh reload` or `SIGHUP` would: zero downtime, and a file
that does not parse or validate is reported on standard error while the
running configuration keeps serving. A change is applied once the files
stayed the same for one interval, so a file still being written is not read
halfway; in practice a reload follows an edit within about a second. A new
file matching an `include` pattern, or a deleted one, counts as a change.

A rejected edit is not tried again until the files change once more. Setting
`watch = false` in the file stops the watching after that reload; turning it
on takes a manual `rxh reload`, since nothing watches the file yet. Files
are polled rather than followed with inotify or kqueue, which works the
same on every platform and on network file systems. Certificate files of
`tls = { cert, key }` are watched on their own and need no `watch`.

## Environment variables and secret files

Any string value can take part of its text from an environment variable or
a file, so secrets and per-host addresses stay out of the configuration:

```toml
[admin]
listen = "0.0.0.0:2019"
token = "{file./run/secrets/rxh-admin-token}"

[upstream.api]
backends = ["{env.API_HOST}:8080", "{env.API_HOST}:8081"]

[upstream.api.health]
uri = "/health"
headers = { Authorization = "Bearer {file.secrets/health-token}" }

[[server]]
listen = "0.0.0.0:{env.PORT}"
forward = "@api"
```

| Placeholder | Replaced with |
|---|---|
| `{env.NAME}` | The environment variable `NAME` of the daemon (letters, digits and `_`). Set but empty is allowed. |
| `{file.PATH}` | The contents of the file, without trailing newlines (`\n`, `\r`). A relative `PATH` starts at the directory of the file that contains the placeholder, which can be an included file. Fits Docker and Kubernetes secrets mounted as files. |

- Placeholders are replaced when the configuration loads: at startup, on
  `rxh validate`, and on every reload (`rxh reload`, `SIGHUP`, `watch`). A
  reload reads the variables of the running daemon, which do not change, and
  the secret files again. `watch` follows the configuration files, not the
  secret files: run `rxh reload` after rotating a secret.
- An unset variable, an unreadable file or a `{env.` / `{file.` without its
  closing `}` is a configuration error, so the daemon does not start, or a
  reload keeps the running configuration.
- Only string values are replaced, in the main file and the included files.
  Keys, numbers and booleans are not: write the whole value as a string
  where the key accepts one (`listen`, durations, sizes). The value is
  inserted as text, so quotes or newlines in it cannot change the structure
  of the file.
- Other braces, like `{host}` or `{uri}` in `rewrite` and `redirect`, are
  request placeholders and stay as they are.
- `POST /load` and `PATCH` bodies are taken as sent, without placeholders.
- `GET /config` and `rxh config show` print the replaced values. `[admin]
  token` and the DNS provider credentials are shown as `***`; other secrets
  placed in the configuration this way are visible to whoever can read the
  admin API.
- The CLI commands that edit the files (`rxh upstream`, `rxh pool`,
  `rxh service`) keep the placeholders in the file, and validate the result
  with the environment and the files the CLI can read.

## Quick commands without a file

`rxh file-server [--listen ADDR] [--root DIR] [--no-browse] [--encode]` serves a
directory in the foreground (default `127.0.0.1:8080` and `.`).
`rxh reverse-proxy --from ADDR --to BACKEND... [--policy P] [--protocol http|tcp|udp]
[--host NAME]` proxies in the foreground. Both disable the admin API, and
reload requests answer "no configuration file to reload".

## Shell completions and man pages

`rxh completions SHELL` prints the completion script for `bash`, `zsh`,
`fish`, `elvish` or `powershell`; `rxh man` prints the rxh(1) man page, and
`rxh man --dir DIR` writes `rxh.1` plus one page per command
(`rxh-upstream-add.1`, ...) into `DIR`, creating it. Both come from the
command line definition, so they always match the binary.

```sh
rxh completions bash > /usr/share/bash-completion/completions/rxh
rxh completions zsh > /usr/share/zsh/site-functions/_rxh
rxh completions fish > /usr/share/fish/vendor_completions.d/rxh.fish
rxh man --dir /usr/share/man/man1
```

## Installing a release package

Every `v*` tag publishes a GitHub release with these files (the `Release`
workflow, [`.github/workflows/release.yml`](../.github/workflows/release.yml)):

| File | Contents |
|---|---|
| `rxh-0.3.0-x86_64-unknown-linux-musl.tar.gz`, `rxh-0.3.0-aarch64-unknown-linux-musl.tar.gz` | static musl binary, runs on any Linux distribution |
| `rxh-0.3.0-x86_64-apple-darwin.tar.gz`, `rxh-0.3.0-aarch64-apple-darwin.tar.gz` | macOS binary |
| `rxh_0.3.0_amd64.deb`, `rxh_0.3.0_arm64.deb` | Debian and Ubuntu |
| `rxh-0.3.0-1.x86_64.rpm`, `rxh-0.3.0-1.aarch64.rpm` | Fedora, RHEL, openSUSE |
| `PKGBUILD` | the `rxh-bin` AUR package |
| `rxh.rb` | the Homebrew formula (macOS and Linux) |
| `SHA256SUMS` | checksums of every file above it |

Each archive holds `rxh`, the man pages (`man/`), the bash, zsh and fish
completions (`completions/`), the systemd units with `ExecStart=/usr/bin/rxh`
(`systemd/`), the default package configuration `rxh.toml` and the license.
The `.deb`, `.rpm` and AUR packages install them in the usual places:
`/usr/bin/rxh`, `/etc/rxh/rxh.toml` (kept on upgrade when edited),
`/usr/lib/systemd/system/rxh.service` and `rxh.socket`, man pages and
completions. They do not enable or start the service. The default
configuration has no `[[server]]`, so the daemon only runs the admin API on
`127.0.0.1:2019` until servers are added.

```sh
sudo apt install ./rxh_0.3.0_amd64.deb      # or: sudo dnf install ./rxh-0.3.0-1.x86_64.rpm
sudoedit /etc/rxh/rxh.toml
sudo systemctl enable --now rxh
yay -S rxh-bin                              # Arch Linux, from the AUR
brew install loseardes77/rxh/rxh            # Homebrew tap; `brew services start rxh`
```

Homebrew installs the configuration at `$(brew --prefix)/etc/rxh/rxh.toml`.
The AUR package and the Homebrew tap are updated by the workflow for
releases only (not pre-releases), and only when the repository has the
`AUR_SSH_PRIVATE_KEY` secret, or the `HOMEBREW_TAP` variable
(`owner/homebrew-rxh`) and the `HOMEBREW_TAP_TOKEN` secret; otherwise the
`PKGBUILD` and `rxh.rb` of the release can be published by hand. To build the
same files locally, see the comments of [`dist/stage.sh`](../dist/stage.sh),
[`dist/nfpm.yaml`](../dist/nfpm.yaml), [`dist/pkgbuild.sh`](../dist/pkgbuild.sh)
and [`dist/homebrew.sh`](../dist/homebrew.sh).

## Running under systemd

[`dist/rxh.service`](../dist/rxh.service) runs `rxh run` as a
`Type=notify-reload` service. When `NOTIFY_SOCKET` is set, rxh sends
`READY=1` once every listener and the admin API are up, `RELOADING=1` and
then `READY=1` around every reload (`SIGHUP`, `rxh reload`, `watch`), with
`STATUS=Reload failed: ...` when the new configuration is rejected (the
previous one keeps running), and `STOPPING=1` on shutdown. No watchdog.

With socket activation ([`dist/rxh.socket`](../dist/rxh.socket)), systemd
binds the sockets and passes them in `LISTEN_FDS`. A listener whose `listen`
address equals the local address of a passed socket of the same kind uses
that socket instead of binding its own: TCP sockets (`ListenStream=`) for
`http` and `tcp` servers, UDP sockets (`ListenDatagram=`) for `udp` servers,
and a TCP or unix socket for `[admin] listen`. Matching is exact:
`ListenStream=443` is `[::]:443`, not `0.0.0.0:443`. Reloads reuse the same
sockets, so nothing is ever closed; listeners without a passed socket bind
as usual, and a passed socket no listener uses is reported on startup. The
challenge-only ACME listener always binds its own port.

```ini
# rxh.socket
[Socket]
ListenStream=0.0.0.0:80
ListenStream=/run/rxh/admin.sock
# systemd creates the file with mode 0666 otherwise: anyone could use the API.
SocketMode=0600
```

```toml
[admin]
listen = "unix//run/rxh/admin.sock"

[[server]]
listen = "0.0.0.0:80"
forward = "@app"
```

## Running in a container

Images for linux/amd64 and linux/arm64 are published on
`ghcr.io/loseardes77/rxh` (and Docker Hub when the repository is set up for
it) from every release tag, built from [`dist/Dockerfile`](../dist/Dockerfile):

| Variant | Tags | Base |
|---|---|---|
| Debian | `latest`, `0.3`, `0.3.0` | `debian:bookworm-slim` |
| Alpine | `alpine`, `0.3-alpine`, `0.3.0-alpine` | `alpine:3`, static musl binary |
| Distroless | `distroless`, `0.3-distroless`, `0.3.0-distroless` | `gcr.io/distroless/cc-debian12`, no shell |

`MAJOR.MINOR` tags follow the last patch release; pre-releases
(`0.4.0-rc.1`) only get their own tag and never move `latest`. From 1.0 a
`MAJOR` tag (`1`, `1-alpine`) is added.

Every image runs `rxh run` as UID 65532 with `RXH_CONFIG=/etc/rxh/rxh.toml`
and `/var/lib/rxh` as working directory. Mount the configuration there;
arguments replace the command (`docker run ... ghcr.io/loseardes77/rxh
validate`). Listeners must bind `0.0.0.0` (or `[::]`) to be reachable from
outside the container; ports below 1024 work as the unprivileged user
because Docker allows them in the container network namespace (not with
`--network host`, where the host's `net.ipv4.ip_unprivileged_port_start`
applies). The relative ACME storage, `rxh-data/certificates`, lands in
`/var/lib/rxh`: keep it on a volume.

```sh
docker run -d --name rxh -p 80:80 -p 443:443 \
  -v "$PWD/rxh.toml:/etc/rxh/rxh.toml:ro" \
  -v rxh-data:/var/lib/rxh \
  ghcr.io/loseardes77/rxh:0.3
docker exec rxh rxh status   # the admin API stays on 127.0.0.1:2019 inside
docker kill -s HUP rxh       # reload
```

The Docker provider needs the socket mounted and readable by UID 65532
(`--group-add` the group of the socket, or a socket proxy over TCP).

## `[admin]`

| Key | Default | Description |
|---|---|---|
| `enabled` | `true` | Serve the admin API. The CLI needs it for live changes; without it, `stop` and `reload` fall back to signals and the PID file. |
| `listen` | `"127.0.0.1:2019"` | Where the admin API listens: `ip:port`, or `unix//absolute/path.sock` for a unix socket. A TCP address outside loopback needs `token`. Changes need a restart. |
| `token` | none | Secret every request must send as `Authorization: Bearer <token>`; `401` otherwise. At least 16 printable ASCII characters, no spaces. Changes need a restart. |

On a unix socket the API is reachable only by the user running the daemon
(and root): the file is created with mode `0600`, a socket left behind by a
dead daemon is replaced, and the file is removed on shutdown.

```toml
[admin]
listen = "unix//run/rxh/admin.sock"
```

For remote administration, listen on a reachable address and set a token.
RXH refuses to start with a non-loopback `listen` and no token.

```toml
[admin]
listen = "10.0.0.5:2019"
token = "kq3Zp0uX7mV9cR2tLw8yHs4d"   # for instance: openssl rand -base64 24
```

```sh
curl -H "Authorization: Bearer kq3Zp0uX7mV9cR2tLw8yHs4d" http://10.0.0.5:2019/status
rxh --admin 10.0.0.5:2019 --admin-token kq3Zp0uX7mV9cR2tLw8yHs4d status
```

- Without a token, TCP requests must have a loopback `Host` and `Origin`
  (protection against DNS rebinding). With a token, or on a unix socket, any
  `Host` is accepted. A token set together with a unix socket is checked too.
- The admin API speaks plain HTTP: the token is as private as the network it
  crosses. Use a private network, a VPN or an SSH tunnel
  (`ssh -L 2019:/run/rxh/admin.sock host`), not the open internet.
- The CLI finds the address in `--admin` / `RXH_ADMIN` (`ip:port` or
  `unix//path`), else in `[admin] listen` of its configuration file. The
  token comes from `--admin-token` / `RXH_ADMIN_TOKEN`, else from `[admin]
  token` of that file, which is only sent to the `listen` address of the
  same file.
- `GET /config` shows the token as `***`. Keep the configuration file
  readable by the daemon's user only.

### Changing the configuration through the API

`POST /load` runs the configuration in the request body instead of the
running one. `PATCH /config/upstream/{name}` and `PATCH /config/server/{name}`
change one pool or one server. All three validate the result and swap to it
like `rxh reload`: zero downtime, and a rejected change (`400` with the
reason) leaves the running configuration untouched.

```bash
# A whole configuration, TOML or JSON.
curl -X POST localhost:2019/load --data-binary @new.toml
curl -s localhost:2019/config > running.json   # edit it, then:
curl -X POST localhost:2019/load --data-binary @running.json

# One pool: only the keys in the body change.
curl -X PATCH localhost:2019/config/upstream/api -d '{"policy": "least_conn"}'
curl -X PATCH localhost:2019/config/upstream/api \
  -d '{"backends": ["10.0.0.1:8080", "10.0.0.2:8080"], "health": null}'

# One server, by name or by its position in `GET /config`.
curl -X PATCH localhost:2019/config/server/web -d '{"max_connections": 2000}'
curl -X PATCH localhost:2019/config/server/0 --data-binary 'name = "front"'
```

- **Nothing is written to disk.** The change lives until the next reload
  (`rxh reload`, `SIGHUP`, `POST /reload`, an edit seen by `watch = true`) or
  restart, which read the configuration file again. To make a change
  permanent, edit the file or use `rxh pool`, `rxh upstream` and
  `rxh service`.
- A body that starts with `{` is JSON, anything else is TOML, whatever the
  `Content-Type`. Bodies are limited to 4 MiB.
- `POST /load`: `[admin]` in the body is ignored, the API keeps the address
  and token it started with. `include` is rejected, send the merged
  configuration. Secrets sent as the `***` of `GET /config` keep their
  running value. Without a body, `/load` reloads the file like `/reload`.
- `PATCH` is a JSON merge patch ([RFC 7396](https://www.rfc-editor.org/rfc/rfc7396)):
  tables merge key by key, `null` removes a key (back to its default), arrays
  and other values are replaced whole. It is merged into the pool or server
  as `GET /config` prints it, so use those keys: `policy` rather than
  `lb_policy`, and the routes of a server under `match` even when the file
  has a bare `forward`. Backends added through `POST /pools/{name}/upstreams`
  are kept.
- `PATCH` answers `404` for a pool or server that does not exist; it does not
  create or delete them (upload a configuration for that). Inline `forward`
  groups are part of their server.

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
with the HTTP-01 challenge (default), the TLS-ALPN-01 challenge or the
[DNS-01 challenge](#dns-01-and-wildcard-certificates) (the one for wildcard
names), like Caddy's automatic HTTPS:

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
| `challenge` | `"http-01"` | How the CA validates the names: `"http-01"` (a request to port 80) or `"tls-alpn-01"` (a TLS handshake on port 443, no plain HTTP port needed) or `"dns-01"` (a TXT record created at the DNS provider of [`[acme.dns]`](#dns-01-and-wildcard-certificates); no open port needed, and the only one for wildcards). |

- Every name in `host` must be an exact, fully qualified domain name (no
  IP address or `localhost`; a wildcard like `*.example.com` only with
  `challenge = "dns-01"`); a `tls = "auto"` server without
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
- With `challenge = "tls-alpn-01"` the CA connects to port 443 of each name
  with ALPN `acme-tls/1`, so port 443 must reach the listener of the
  `tls = "auto"` server (listen on it, or forward it there). That listener
  answers the handshake with a temporary self-signed certificate that proves
  control of the name, then closes the connection; ordinary clients never
  see it, and a handshake with `acme-tls/1` for a name without a pending
  challenge fails. Nothing has to listen on `http_port`, and no
  challenge-only listener is opened; the HTTP to HTTPS redirect stays on
  unless `https_redirect = false`:

  ```toml
  [acme]
  email = "ops@example.com"
  challenge = "tls-alpn-01"

  [[server]]
  listen = "0.0.0.0:443"
  host = "example.com"
  tls = "auto"
  https_redirect = false    # nothing on port 80
  forward = "@app"
  ```

  A TLS terminating balancer or CDN in front of rxh breaks TLS-ALPN-01 (the
  CA's handshake must arrive untouched); use HTTP-01 or DNS-01 there. Only
  one challenge type is used, there is no fallback to another.
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

### DNS-01 and wildcard certificates

With `challenge = "dns-01"` the CA looks up a TXT record at
`_acme-challenge.NAME`. rxh creates it through the API of the DNS provider
in `[acme.dns]`, waits `propagation_wait`, asks the CA to validate and
deletes the record afterwards (also when the order fails or times out).
Nothing has to be reachable from the internet, and it is the only challenge
that certifies wildcards:

```toml
[acme]
email = "ops@example.com"
challenge = "dns-01"

[acme.dns]
provider = "cloudflare"
api_token = "..."

[[server]]
listen = "0.0.0.0:443"
host = ["*.example.com", "example.com"]   # two certificates
tls = "auto"
forward = "@app"
```

| Key | Default | Description |
|---|---|---|
| `provider` | required | `"cloudflare"`, `"route53"` or `"rfc2136"`. |
| `propagation_wait` | `"60s"` | Time between creating the record and asking the CA to validate, for the record to reach every name server of the zone. `"0s"` is allowed. |
| `api_token` | | Cloudflare (required): API token with the `Zone:Read` and `DNS:Edit` permissions for the zones. |
| `access_key_id`, `secret_access_key` | | Route 53 (required): access key of an IAM user allowed `route53:ListHostedZonesByName` and `route53:ChangeResourceRecordSets`. |
| `zone_id` | | Cloudflare, Route 53: identifier of the zone, instead of looking it up by name. Needed for a Cloudflare token without `Zone:Read`, or when a public and a private Route 53 zone share a name. Only for configurations whose names are all in that zone. |
| `api_url` | the public API | Cloudflare, Route 53: base URL of the API (`http://` or `https://`), for tests and compatible services. |
| `server` | | RFC 2136 (required): primary name server of the zones, `"host"` or `"host:port"` (port 53; an IPv6 address with a port in brackets). Updates and the zone lookup go over TCP. |
| `tsig_key`, `tsig_secret` | | RFC 2136: name and base64 secret of the TSIG key that signs the updates (HMAC-SHA256, as `tsig-keygen` makes). Without them updates are unsigned, for servers that allow them by address. |

```toml
[acme.dns]
provider = "route53"
access_key_id = "AKIA..."
secret_access_key = "..."
```

```toml
[acme.dns]
provider = "rfc2136"          # BIND, Knot, PowerDNS, ...
server = "ns1.example.com"
tsig_key = "rxh-key"
tsig_secret = "bWFkZSB3aXRoIHRzaWcta2V5Z2Vu..."
```

- Keys of another provider are a validation error, as is
  `challenge = "dns-01"` without `[acme.dns]`. `[acme.dns]` with another
  challenge is accepted and unused.
- A wildcard is one leading label (`*.example.com`). It gets its own
  certificate, which does not cover `example.com`: list both for both. `host`
  wildcards match several labels (`a.b.example.com`), certificates only one,
  so deeper names fail verification in the client.
- The zone is found by asking the provider for the record name and each of
  its parents, longest first, so delegated subzones and names of several
  zones work. A `_acme-challenge` CNAME to another zone is not followed.
- The secrets are in the configuration file: keep it private (`chmod 600`).
  `GET /config` on the admin API shows them as `***`.
- One provider for all names. DNS-01 orders run one at a time. Route 53
  replaces other TXT values at the challenge name while validating, and only
  the `aws` partition is supported (no session tokens, China or GovCloud).
- The HTTP to HTTPS redirect still listens on `http_port` unless
  `https_redirect = false`; the challenge itself opens no port.
- A wildcard is stored as `wildcard_.example.com`.

### Certificate storage

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

## `[docker]`: routes to containers from their labels

```toml
[docker]
provider = "docker"      # or "podman", "swarm"
endpoint = "unix:///var/run/docker.sock"
listen = "0.0.0.0:80"
```

With a `[docker]` table the daemon asks Docker for the running containers
labelled `rxh.enable=true`, routes to them as their other `rxh.*` labels say,
and follows the Docker events: a container is routed when it starts and
dropped when it dies, without a reload. Nothing is written to the
configuration file.

| Key | Default | Description |
|---|---|---|
| `provider` | `"docker"` | Engine that runs the containers: `"docker"`, `"podman"` or `"swarm"`, see [Podman](#podman) and [Docker Swarm](#docker-swarm). |
| `endpoint` | `"unix:///var/run/docker.sock"`, `"unix:///run/podman/podman.sock"` with Podman | Engine API: `unix:///path/to/docker.sock` or `tcp://host:port` (plain TCP, for a socket proxy; TLS is not supported). |
| `refresh` | `"15s"` | How often the Swarm services and tasks are listed. Unused by Docker and Podman, whose events are followed. |
| `listen` | `"0.0.0.0:80"` | Address of the HTTP servers made from `rxh.http.host` labels. Servers of the file can listen on the same address. |

Labels of a container:

| Label | Description |
|---|---|
| `rxh.enable` | `true` to route to the container. Containers without it are never looked at. |
| `rxh.http.host` | Host names of an HTTP route on `[docker] listen`, comma separated, exact or `*.example.com`. |
| `rxh.http.port` | Container port of the HTTP route. May be left out when the container exposes exactly one TCP port. |
| `rxh.tcp.listen` | Address of a `tcp` server for the container: `:5432` (every address) or `127.0.0.1:5432`. |
| `rxh.tcp.port` | Container port of the TCP route, with the same default as `rxh.http.port`. |
| `rxh.pool` | Name of the pool. Containers with the same pool are load balanced. Defaults to `PROJECT-SERVICE` for Docker Compose containers, so the replicas of a service share a pool, else to the container name. Letters, digits, `-`, `_` and `.`. |
| `rxh.policy` | Load balancing policy of the pool, see [Policies](#policies). Default `weighted_round_robin`. |

```yaml
# compose.yaml
services:
  app:
    image: my-app
    deploy:
      replicas: 3
    labels:
      rxh.enable: "true"
      rxh.http.host: app.example.com
      rxh.http.port: "8080"
      rxh.policy: least_conn
  db:
    image: postgres:16
    labels:
      rxh.enable: "true"
      rxh.tcp.listen: ":5432"
```

Every pool becomes a server named `docker-POOL` and a pool whose members are
the containers, at their IP address on the Docker network and the container
port (published ports are not needed). Both show in `rxh status` and in the
metrics, not in `rxh config`. A replica that starts or stops only changes the
members of its pool; when a pool appears, disappears or changes its hosts,
address or policy the listeners are swapped as a reload does, without
dropping connections.

- rxh must reach the container addresses: run it on the Docker host or in a
  container on the same Docker network. A container on several networks is
  reached at its address on the first one by name; one with
  `network_mode: host` at `127.0.0.1`.
- A container is HTTP or TCP: `rxh.http.host` and `rxh.tcp.listen` cannot be
  combined. The containers of a pool must agree on `rxh.policy` and
  `rxh.tcp.listen`; their hosts are merged.
- A container whose labels are wrong (unknown `rxh.*` label, missing port,
  invalid host or policy), whose pool has the name of an `[upstream.NAME]`
  of the file, or whose server would make the configuration invalid is
  ignored and reported once on standard error. The other containers are
  not affected.
- While Docker cannot be reached the last known containers stay routed and
  the connection is retried every 2 seconds. The daemon starts without
  Docker.
- The routes are plain HTTP and cannot be tuned beyond the labels (no TLS,
  health checks or header rules); put a server of the file in front for
  those.
- The Docker socket gives control of the host. Prefer a socket proxy that
  only allows `GET /containers/json` and `GET /events` (`GET /services` and
  `GET /tasks` for Swarm).

### Podman

```toml
[docker]
provider = "podman"
# Rootless: the socket of the user, enabled with
# `systemctl --user enable --now podman.socket`.
endpoint = "unix://{env.XDG_RUNTIME_DIR}/podman/podman.sock"
listen = "0.0.0.0:80"
```

Podman is followed through its Docker compatible API with the same labels;
`podman-compose` sets the Compose labels, so its replicas share a pool too.
The difference is how containers are reached: the network of rootless
containers lives in a namespace the host cannot route to, so a container
that publishes the chosen port (`-p 8080` or `-p 127.0.0.1::8080`) is
reached at that host port, on `127.0.0.1` when published on every address.
A port that is not published is reached at the container address, as with
Docker, which works for rootful Podman only.

### Docker Swarm

```toml
[docker]
provider = "swarm"
endpoint = "unix:///var/run/docker.sock"   # of a manager node
listen = "0.0.0.0:80"
refresh = "15s"
```

In Swarm mode the labels are read from the services (`deploy.labels` in a
stack file, `docker service create --label`), and every running task of a
service labelled `rxh.enable=true` is a member of its pool. The pool
defaults to the service name (`STACK_SERVICE` for a stack). A task is
reached at its address on the first network of the service by name, the
`ingress` routing mesh left out, so rxh must be attached to that overlay
network, typically as a service on a manager node. `rxh.http.port` and
`rxh.tcp.port` may be left out when the service publishes exactly one TCP
port: its target port is used.

```yaml
# stack.yaml, `docker stack deploy -c stack.yaml shop`
services:
  web:
    image: my-app
    deploy:
      replicas: 3
      labels:
        rxh.enable: "true"
        rxh.http.host: shop.example.com
        rxh.http.port: "8080"
```

Swarm only sends the events of a node's own containers, so the services and
tasks are listed every `refresh` instead: a task that starts or stops is
routed within that delay. Tasks are reached directly, not through the
virtual IP of the service.

## `[consul]`, `[etcd]`, `[nomad]`: routes from a service catalog

```toml
[consul]
endpoint = "http://127.0.0.1:8500"
token = "{env.CONSUL_HTTP_TOKEN}"
listen = "0.0.0.0:80"
refresh = "15s"
```

Each table makes the daemon list a service catalog every `refresh` and
route to its service instances with the options of the
[`[docker]` labels](#docker-routes-to-containers-from-their-labels)
(`rxh.enable`, `rxh.http.host`, `rxh.http.port`, `rxh.tcp.listen`,
`rxh.tcp.port`, `rxh.pool`, `rxh.policy`). An instance is reached at its
address and port, so `rxh.http.port` and `rxh.tcp.port` are rarely needed.
The pool defaults to the service name, and the server of a pool is named
after the section: `consul-web`. Instances that come and go change the
members of the pool; the listeners are swapped only when the routes change.
While a catalog cannot be reached, or answers something that cannot be
read, the instances last listed stay routed and the catalog is retried
every `refresh`. The tables can be used together and with `[docker]`, but a
pool belongs to one of them: an instance of a pool already routed by
another provider is ignored and reported.

| Key | Default | Description |
|---|---|---|
| `endpoint` | `"http://127.0.0.1:8500"` (Consul), `"http://127.0.0.1:2379"` (etcd), `"http://127.0.0.1:4646"` (Nomad) | HTTP API of the catalog: `http://host:port`. HTTPS and Unix sockets are not supported. |
| `token` | unset | ACL token, sent as `X-Consul-Token` or `X-Nomad-Token`. Not accepted by `[etcd]`. Use a `{env.NAME}` or `{file.PATH}` placeholder to keep it out of the file. |
| `prefix` | `"rxh/"` | `[etcd]` only: where the instances are written. |
| `ingress_class` | unset | `[kubernetes]` only, see [below](#kubernetes-ingress-and-gateway-api). |
| `listen` | `"0.0.0.0:80"` | Address of the HTTP servers made from `rxh.http.host` options. |
| `refresh` | `"15s"` | How often the catalog is listed. |

### Consul

The services of the catalog tagged `rxh.enable=true` are routed, and of
those only the instances whose health checks pass
(`/v1/health/service/NAME?passing`). The options are the `rxh.KEY=VALUE`
tags of each instance; other tags are ignored. An instance without a
service address is reached at the address of its node.

```sh
consul services register -name web -port 8080 \
  -tag rxh.enable=true -tag rxh.http.host=app.example.com
```

### Nomad

The services of Nomad's native service discovery tagged `rxh.enable=true`
are routed, in every namespace the token can read, with the `rxh.KEY=VALUE`
tags of each registration as options. The service API of Nomad does not
report the health of a registration, so an instance is routed for as long
as it is registered.

```hcl
service {
  name     = "web"
  port     = "http"
  provider = "nomad"
  tags     = ["rxh.enable=true", "rxh.http.host=app.example.com"]
}
```

### etcd

Instances are keys under `prefix`, read through the JSON gateway of the
etcd v3 API (`/v3/kv/range`): `PREFIX/SERVICE/INSTANCE/OPTION`, where
`OPTION` is `address` (`ip:port`, or an IP with `http.port` or `tcp.port`)
or an option without its `rxh.` prefix. Every instance is enabled unless
its `enable` key is not `true`, and its pool defaults to `SERVICE`.
Authentication is not supported.

```sh
etcdctl put rxh/web/1/address 10.0.0.5:8080
etcdctl put rxh/web/1/http.host app.example.com
etcdctl put rxh/web/2/address 10.0.0.6:8080
etcdctl put rxh/web/2/http.host app.example.com
```

## `[kubernetes]`: Ingress and Gateway API

```toml
[kubernetes]
endpoint = "http://127.0.0.1:8001"   # kubectl proxy
ingress_class = "rxh"
listen = "0.0.0.0:80"
refresh = "15s"
```

The daemon lists the Ingresses (`networking.k8s.io/v1`), the Gateway API
HTTPRoutes (`gateway.networking.k8s.io/v1`, skipped when the cluster does
not have them), the Services and the EndpointSlices of every namespace
every `refresh`, and serves each host on `listen`. A host is routed to the
ready endpoints of the Service port it points at, directly at the pod
addresses, not through the cluster IP. Each Service port is one pool named
`NAMESPACE-SERVICE-PORT` (server `kubernetes-NAMESPACE-SERVICE-PORT`),
shared by every Ingress and HTTPRoute that points at it; pods that come and
go only change its members. It behaves like the
[service catalogs](#consul-etcd-nomad-routes-from-a-service-catalog)
otherwise: while the API cannot be reached or read, the last routes stay.

| Key | Default | Description |
|---|---|---|
| `endpoint` | `"http://127.0.0.1:8001"` | Kubernetes API over plain HTTP: `http://host:port`. The default is the address of `kubectl proxy`, which can run as a sidecar of rxh in the cluster. |
| `token` | unset | Bearer token sent to the API (`Authorization: Bearer`). Not needed through `kubectl proxy`. |
| `ingress_class` | unset | Only the Ingresses of this class (`spec.ingressClassName`, or the `kubernetes.io/ingress.class` annotation) are routed. Unset routes every Ingress. HTTPRoutes are not filtered. |
| `listen` | `"0.0.0.0:80"` | Address of the HTTP servers of the hosts. |
| `refresh` | `"15s"` | How often the API is listed. |

```yaml
apiVersion: networking.k8s.io/v1
kind: Ingress
metadata:
  name: shop
spec:
  ingressClassName: rxh
  rules:
    - host: shop.example.com
      http:
        paths:
          - path: /
            pathType: Prefix
            backend:
              service:
                name: web
                port:
                  number: 80
```

The account that reads the API needs `list` on `ingresses`, `httproutes`,
`services` and `endpointslices` in every namespace.

This is a first cut. What is missing:

- The API is only reached over plain HTTP: no TLS, no in-cluster service
  account (token and CA files), no kubeconfig. Run `kubectl proxy` next to
  rxh.
- Only whole hosts are routed. Paths other than the prefix `/`, `Exact`
  paths, rules without a host, `defaultBackend` and resource backends of an
  Ingress, and the matches (path, headers, query, method), filters and
  weighted `backendRefs` of an HTTPRoute are left out and reported in the
  log (`Kubernetes => ignored object Ingress default/shop: ...`).
- Gateways and GatewayClasses are not read: every HTTPRoute with
  `hostnames` is served on `listen`, whatever its `parentRefs`, and a route
  without `hostnames` is left out. `ReferenceGrant` is not checked for
  Services of another namespace. No GRPCRoute, TLSRoute, TCPRoute or
  UDPRoute.
- No TLS from the cluster: `spec.tls` and its Secrets are not read, and the
  hosts are served over plain HTTP on one address.
- Nothing is written back: no `status.loadBalancer` on an Ingress, no
  route or Gateway status, no IngressClass or GatewayClass controller name.
- The API is polled, not watched, and the namespaces cannot be restricted.
- A wildcard host `*.example.com` matches one or more labels, where
  Kubernetes asks for exactly one. A Service without ready endpoints has no
  route, instead of answering `503`. `ExternalName` Services, annotations
  and the `rxh.policy` option are not supported.

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
| `h2c` | `false` | `h2c://` | Speak cleartext HTTP/2 to the backends, over `http2_conns_per_host` shared connections each. |
| `http2_conns_per_host` | `1` | | HTTP/2 connections kept open to each backend of an `h2c` pool. The first requests open them, later ones use them in turn, and a closed one is replaced by the next request. More than one helps when a backend limits the concurrent streams of a connection, or when one TCP connection is the bottleneck. A burst on a backend without any open connection may open more; those carry one request and are closed. `0` is rejected. Ignored without `h2c`. |
| `keepalive` | `"2m"` | `keepalive` | How long idle upstream HTTP connections are kept. `"0s"` disables pooling. |
| `keepalive_idle_conns_per_host` | `32` | `keepalive_idle_conns_per_host` | Idle connections kept per backend. |
| `max_conns_per_host` | `0` | `max_conns_per_host` | Most HTTP/1.1 connections open to each backend, idle ones included. Requests over the limit wait for a free connection (they are not failed or sent elsewhere; use `passive.unhealthy_request_count` for that). Upgraded tunnels (WebSockets) count. `0` means no limit. Ignored with `h2c`, which shares `http2_conns_per_host` connections per backend, and on `tcp` and `udp` servers. |
| `request_buffers` | `0` | `request_buffers` | [Size](#sizes): bytes of each request body read before a backend is picked and dialed. A body that fits is sent with a `Content-Length`; the rest of a larger one is streamed. `0` streams everything. Not applied to upgrades. |
| `response_buffers` | `0` | `response_buffers` | [Size](#sizes): bytes of each response body read before the response goes to the client, which frees the backend from slow clients. Same rules as `request_buffers`; a backend failing meanwhile gives `502`. |
| `flush_interval` | unset | `flush_interval` | Hold back response data and write it to the client in batches, at most this long after the first held byte (or as soon as 64 KiB are held, or the body ends). Only for responses with a `Content-Length` that are not `text/event-stream`; streams of unknown length and server-sent events are always written as they arrive, like in Caddy. Unset writes every chunk at once (Caddy's `-1`); `0` and negative values are rejected. |
| `tls` | | `transport http { tls }` | Table of TLS client settings, see below. Setting it enables TLS for every backend, also without `https://`. |
| `health` | | active health checks | Table, see below. |
| `passive` | | passive health checks | Table, see below. |
| `circuit_breaker` | | Traefik `CircuitBreaker` | Table, see below. |
| `slow_start` | unset | Nginx Plus `slow_start` | Duration over which a backend that joins the pool at runtime or comes back healthy ramps up to its full share of the traffic, see [Slow start](#slow-start). `0` is rejected. |

Example: at most 8 connections per backend, uploads up to 64 KiB buffered
before they are sent, responses up to 1 MiB buffered:

```toml
[upstream.app]
backends = ["10.0.0.1:8080", "10.0.0.2:8080"]
max_conns_per_host = 8
request_buffers = "64KiB"
response_buffers = "1MiB"
```

Example: four HTTP/2 connections to each gRPC backend:

```toml
[upstream.grpc]
backends = ["10.0.0.1:50051", "10.0.0.2:50051"]
h2c = true
http2_conns_per_host = 4
```

Example: fewer, larger writes to clients of a backend that sends many small
chunks:

```toml
[upstream.reports]
backends = ["10.0.0.3:8080"]
flush_interval = "100ms"
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
with a circuit breaker that is not open, and below `unhealthy_request_count`. When none is available the client gets
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

### `[upstream.NAME.circuit_breaker]`: circuit breaker

Each backend of the pool gets a breaker that watches its real traffic. While
it is **closed**, requests are counted over `window`. When the share of
failed requests reaches `error_ratio`, or the share of slow ones reaches
`latency_ratio`, the breaker **opens** and the backend is out of rotation for
`open_duration`: the other backends take its requests, and when none is left
the client gets `503` (or the next pool of a weighted split takes over).
After `open_duration` the breaker is **half open**: the backend gets one
trial request at a time. A good answer closes the breaker and starts a new
window; a failed or slow one opens it again for `open_duration`.

At least one of `error_ratio` and `latency` is required; with both, either
condition opens the breaker.

| Key | Default | Description |
|---|---|---|
| `error_ratio` | unset | Share of failed requests that opens the breaker, above `0` up to `1` (`0.5` is half of them). Failed dials, response header timeouts, broken responses and `error_status` responses are failures. |
| `error_status` | `["5xx"]` | Response statuses that count as failures, like `["5xx", 429]`. |
| `latency` | unset | Responses slower than this (time to headers) count as slow. |
| `latency_ratio` | `0.5` | Share of slow responses that opens the breaker, above `0` up to `1`. `0.5` opens when the median latency is over `latency`, `0.05` when the 95th percentile is. |
| `window` | `"10s"` | Period over which requests are counted. Counters start from zero every `window`. |
| `min_requests` | `10` | Requests in a window below which the breaker stays closed. |
| `open_duration` | `"10s"` | How long an open breaker keeps its backend out before the trial request. |

```toml
[upstream.api]
backends = ["10.0.0.1:8080", "10.0.0.2:8080"]

[upstream.api.circuit_breaker]
error_ratio = 0.3        # 30% of the requests failed...
latency = "500ms"        # ...or half of them took over 500ms
window = "10s"
min_requests = 20
open_duration = "30s"
```

The breaker differs from `passive` checks in counting a ratio instead of a
number of failures, so it follows the traffic volume, and in probing with a
single request before the backend takes its full share again. Both can be
set on the same pool.

On `tcp` servers an established connection is a success and a failed dial a
failure; `latency` has no effect. `udp` sessions never report a success, so
do not set a breaker on pools used by `udp` servers. The state is per
backend and per process, starts closed after a reload, and is shown as
`circuit` (`closed`, `open`, `half_open`) in `GET /pools` of the admin API.

### Slow start

`slow_start` gives a backend time to warm up (caches, JIT, connection pools)
before it takes its full share of the traffic. Its share starts at nothing
and grows linearly to full over the duration: half way through, it gets half
of the requests it would normally get.

```toml
[upstream.api]
policy = "least_conn"
backends = ["10.0.0.1:8080", "10.0.0.2:8080"]
slow_start = "30s"
health = { uri = "/health", interval = "5s" }
```

A backend ramps up when it:

- joins the pool at runtime: through `POST /pools/{name}/upstreams` or
  `rxh upstream add`, or found by DNS, Docker or another discovery source;
- is marked healthy again by active health checks;
- is healthy again because its `passive` failures expired;
- has its circuit breaker closed by a good trial request.

Backends listed in the configuration take their full share when RXH starts
or reloads. The ramp applies to `round_robin`, `weighted_round_robin`,
`random`, `random_choose`, `least_conn` and `first`, so a new idle backend is
not flooded by `least_conn`. Hash policies (`ip_hash`, `client_ip_hash`,
`uri_hash`, `header`, `query`) and `cookie` ignore it to keep their keys on
the same backend, except when they fall back to random. When every available
backend is ramping and slow start turns them all away, the request still
goes to one of them instead of failing.

## `[[server]]`: listeners

Each `[[server]]` is a service. Every address in `listen` gets its own
socket, except that `http` servers listing the same address share one socket
(see [Virtual hosts on a shared address](#virtual-hosts-on-a-shared-address)).

| Key | Default | Description |
|---|---|---|
| `listen` | required | `"ip:port"` or a list of them. Port `0` picks a free port. |
| `protocol` | `"http"` | `http`, `tcp` or `udp`. |
| `name` | | Name shown in logs, in `rxh status`, and in `Forwarded: by=` and `Via`. Must be unique: inline pools, metrics labels and `rxh service` use it. |
| `forward` | | Proxy target: `"@pool"`, `"host:port"`, a list of backends, an inline pool table, or a weighted split between pools, see [Traffic splitting and canary releases](#traffic-splitting-and-canary-releases). |
| `serve` | | Serve static files: a directory, or a table, see [File server](#file-server) (http only). |
| `redirect` | | Answer with a redirect: `"target"` or `{ to, status }`, see [Rewrites and redirects](#rewrites-and-redirects) (http only). |
| `uri` | `"/"` | Path prefix for the single `forward`, `serve` or `redirect` of this server (http only). |
| `rewrite` | | New URI for the single `forward` or `serve`, same as in `match` entries (http only). |
| `host` | | Host name(s) this server answers to. In the single pattern form it is the `host` matcher; with `match` it is the default of every entry without its own `host`. Not allowed on `udp` servers. |
| `path`, `method`, `header`, `query`, `remote_ip` | | Request matchers for the single pattern form, same as in `match` entries. |
| `match` | | List of patterns instead of `forward`/`serve`/`redirect`/`uri`: HTTP routes on `http` servers, layer 4 routes on `tcp` servers, see below. |
| `max_connections` | `1024` | Concurrent connections (or UDP sessions). Extra clients wait in the backlog. Alias: `connections`. |
| `tls` | | `{ cert = "cert.pem", key = "key.pem" }`: TLS termination for `http` (with h2 via ALPN) and `tcp`. `"auto"` (http only): certificates from an ACME CA for every name in `host`, see [Automatic HTTPS](#acme-automatic-https). Servers sharing an address pick their certificate by SNI. |
| `client_auth` | | `{ mode, ca, header }`: ask clients for a certificate in the TLS handshake, see [Client certificates (mTLS)](#client-certificates-mtls) (servers with `tls` only). |
| `tls_options` | | `{ min_version, cipher_suites, curves, ocsp_stapling }`: what the TLS handshake accepts, and OCSP stapling, see [TLS options](#tls-options) (servers with `tls` only). |
| `https_redirect` | `true` with `tls = "auto"`, else `false` | Redirect plain HTTP requests for this TLS server to HTTPS, see [HTTP to HTTPS redirect](#http-to-https-redirect) (http servers with `tls` only). |
| `http2` | `true` | Accept HTTP/2 (h2 over TLS, h2c in cleartext). |
| `trusted_proxies` | `[]` | CIDR ranges (or `"private_ranges"`) allowed to set `X-Forwarded-*` and the client IP. With `proxy_protocol_in`, also the only peers allowed to send a PROXY header (any peer when empty). |
| `ip_allow`, `ip_deny` | `[]` | CIDR ranges or addresses of the clients this server accepts and rejects, on every protocol, see [IP allow and deny lists](#ip-allow-and-deny-lists). |
| `rate_limit` | | `{ rate, per, burst, key }`: requests each client may send, see [Rate limits](#rate-limits) (http only). |
| `basic_auth` | | `{ users, realm }`: ask for a user name and password, see [Basic auth](#basic-auth) (http only). |
| `forward_auth` | | `{ url, copy_headers, timeout }`: let an external service (Authelia, Authentik, oauth2-proxy) decide, see [Forward auth](#forward-auth) (http only). |
| `max_connections_per_ip` | | Concurrent connections of one client IP; more are closed on accept, see [Connections per client](#connections-per-client) (http and tcp). |
| `proxy_protocol` | | `"v1"` or `"v2"`: send a PROXY protocol header with the client address on every new connection to a backend, see [PROXY protocol output](#proxy-protocol-output) (http and tcp). |
| `proxy_protocol_in` | `false` | Require a PROXY protocol header (v1 or v2) at the start of every connection and use its source address as the client address, see [PROXY protocol input](#proxy-protocol-input) (http and tcp). Set `trusted_proxies` too: with it empty any directly connecting client can spoof its address. |
| `match_timeout` | `"3s"` | Time a client has to send the PROXY header (`proxy_protocol_in`) and, on `tcp` servers with layer 4 matchers, the first bytes the matchers look at. |
| `read_header_timeout` | `"30s"` (TLS handshake `"10s"`) | Time a client has to send the request headers; slower clients are disconnected (http only). It also bounds the TLS handshake (`http` and `tcp` servers with `tls`) and the time to the first complete request head of a connection, HTTP/2 included, so clients that connect and send nothing are closed too. When unset, request heads get 30 seconds and TLS handshakes 10 seconds. On a listener shared by several http servers, this and `idle_timeout` come from the first server. |
| `idle_timeout` | unset | Keep-alive HTTP connections without traffic and without requests in flight for this long are closed. For `tcp`, a connection with no traffic in either direction for this long is closed. |
| `stream_timeout` | unset | Maximum lifetime of upgraded HTTP tunnels (WebSockets) and of `tcp` connections. |
| `stream_close_delay` | unset | Upgraded HTTP tunnels (WebSockets) are closed this long after the listener stops, on a reload that replaces it or on shutdown; `"0s"` closes them at once, like Caddy's default. Spreading the reconnects of many clients over the delay avoids a thundering herd. Unset keeps tunnels open across reloads until either side ends them (on daemon exit they end with the process). Example: `stream_close_delay = "5m"` (http only). |
| `udp_idle_timeout` | `"60s"` | Idle time after which a UDP client session ends (udp only). |
| `request`, `response`, `cache`, `encode` | | Same as in `match` entries, for the single pattern form. |

A `udp` server takes exactly one `forward` and no `match`. A `tcp` server
takes one `forward`, or a `match` list of [layer 4 routes](#match-entries-of-tcp-servers-layer-4-routing).

On Linux a `tcp` server moves the bytes with `splice(2)`, inside the kernel
and without copying them through rxh, when rxh encrypts neither side: no
`tls` on the server (TLS passthrough counts as plain bytes) and `host:port`
backends without upstream TLS. Bytes read ahead for `match` or
`proxy_protocol_in` are forwarded first. Every other case (TLS termination,
TLS to the backends, `unix/` backends, other operating systems) copies
through a buffer, with the same behaviour: timeouts, byte counts in the
access log and half-closed connections work alike. There is no key to set:

```toml
# Spliced on Linux: plain TCP on both sides.
[[server]]
listen = "0.0.0.0:5432"
protocol = "tcp"
forward = "10.0.0.5:5432"
```

### Traffic splitting and canary releases

`forward` can split the traffic between named pools by weight, for canary
releases and blue-green switches. Each entry is `{ pool, weight }`: `pool`
names an `[upstream.NAME]` (without `@`) and `weight` (default `1`) is its
share relative to the other entries. HTTP servers split requests, `tcp`
servers split connections; `udp` servers cannot split.

```toml
[upstream.v1]
backends = ["10.0.0.1:8080", "10.0.0.2:8080"]

[upstream.v2]
backends = ["10.0.1.1:8080"]

[[server]]
listen = "0.0.0.0:80"
# 9 requests out of 10 to v1, 1 to v2.
forward = [{ pool = "v1", weight = 90 }, { pool = "v2", weight = 10 }]
```

The pools take turns in the smooth weighted round robin order (the one of
`weighted_round_robin`), so the shares hold exactly over every cycle of
`sum of weights / gcd` requests, then each pool picks a member with its own
policy, retries and health checks. A pool whose members are all down or
full (active or passive health checks) gives its turn to the next pool of
the cycle, so a broken canary stops getting traffic until it recovers; when
no pool has an available member, the pool whose turn it is answers.

Weight `0` keeps a pool listed without traffic; change the weights and
reload (`rxh reload`, or `watch = true`) to shift the traffic. At least
one weight must be above zero, and a pool can be listed once. Every pool is
checked against the server like a plain `forward`: a `tcp` server cannot
split to a pool with an HTTP-only policy. Splits are not sticky: a client
can land on either version from one request to the next; use a `match`
entry on a cookie or header to pin testers to the canary.

### Request mirroring

`mirror` on a `forward` route sends a copy of the requests to other pools
(shadow traffic), to try a new version with production traffic before it
answers anyone. The client only ever gets the response of the `forward`: the
mirror's responses are read and dropped, and a slow, failing or down mirror
never delays or changes it. Each entry is a table, or a list of them:

| Key | Default | Description |
|---|---|---|
| `pool` | | Name of an `[upstream.NAME]`, without `@`. |
| `percent` | `100` | Share of the requests copied, from `0` to `100`, spread evenly: `10` copies one request out of every ten. |
| `max_body_size` | `"1MiB"` | [Size](#sizes): largest request body copied. |

```toml
[upstream.v2]
backends = ["10.0.1.1:8080"]

[[server]]
listen = "0.0.0.0:80"
forward = "@v1"
# A copy of 1 request out of 4 goes to v2 as well.
mirror = { pool = "v2", percent = 25 }
```

The copy is the request as the `forward` sends it: same method, URI (after
`rewrite`), `Host`, body, and headers after the `request.headers` rules,
`X-Forwarded-*` and the PROXY protocol header of the server. It is sent at
the same time as the forwarded request, to a member picked by the mirror
pool's own policy; its failures are logged and fed to the pool's passive
health checks, never retried. Notes:

- A request with a body is copied only when its `Content-Length` is at
  most `max_body_size`. The body is then read before it is forwarded;
  bodies without a length (chunked) and larger ones are forwarded without a
  copy.
- WebSocket and other upgrade requests, and responses served from `cache`,
  are not copied.
- A copy may take 30 seconds, response included. Past 1024 copies in flight
  per mirror, requests go without one until some finish.
- `mirror` is only allowed on `forward` routes of `http` servers, and
  `proxy_protocol` with an `h2c` mirror pool is rejected like on `forward`.

### Virtual hosts on a shared address

Several `http` servers may list the same `listen` address. They share one
socket and their routes are tried in configuration order, each route keeping
the settings of its own server (`name`, `trusted_proxies`, `host`, headers,
cache). Give each server a `host` so requests reach the right one; a request
no route accepts gets `404`. `http2` and `max_connections` of the shared
socket come from the first server, `max_connections_per_ip` must be the same
on all of them, and `rxh status` shows one listener whose name joins the
server names.

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
servers with and without `tls` or with different `client_auth` or `tls_options` tables. TCP and UDP sockets never conflict, so a `udp`
server may use the address of a `tcp` or `http` server. Port `0` is never
shared: every port `0` entry gets its own free port.

### Certificate reload and expiry

Certificate files are read again without a restart: every listener checks
the contents of its `cert` and `key` files every 5 seconds and serves the new
certificate to the next handshakes when they change. Open connections keep
the certificate they started with. A pair that fails to load (a half-written
file, a key that does not match the certificate) is logged and the previous
certificate stays until the files change again, so write the key and the
certificate in either order. Symbolic links are followed, as certbot's
`live/` directory uses them. A new certificate gets its OCSP staple on the
next stapling round, within the hour.

`rxh cert reload` (`POST /certificates/reload`) reads every file now, changed
or not, and fails listing the pairs that did not load, for a deploy hook:

```sh
certbot renew --deploy-hook "rxh cert reload"
```

`rxh cert list` (`GET /certificates`) shows what each TLS listener serves,
soonest expiry first, `tls = "auto"` certificates included:

```text
$ rxh cert list
LISTEN                 HOSTS                          EXPIRES               DAYS OCSP   SOURCE
0.0.0.0:443            a.example.com (default)        2027-01-02T10:00:00     44 yes    /etc/rxh/a.pem
0.0.0.0:443            *.b.example.com                2027-02-11T08:30:00     84 no     acme
```

`--json` prints the admin API answer: `subject`, `issuer`, `names` (the DNS
names of the certificate), `not_before`, `not_after`, `days_left` (negative
once expired), `hosts`, `default`, `source` (`"file"` or `"acme"`),
`cert_file`, `key_file` and `ocsp_stapled`.

### Client certificates (mTLS)

`client_auth` on a server with `tls` (files or `"auto"`, `http` or `tcp`)
asks the client for a certificate during the TLS handshake. A client that
does not satisfy the mode fails the handshake and never reaches a backend.

| Key | Default | Description |
|---|---|---|
| `mode` | `"require_and_verify"` | What is asked of the client, see the modes below. |
| `ca` | | PEM file, or list of PEM files, with the certificate authorities that sign client certificates. Required by `verify_if_given` and `require_and_verify`, rejected by the other modes. A missing file or one without certificates fails the start. |
| `header` | | Request header that carries the subject of the client certificate to the backends, such as `CN=alice, O=Example` (http servers, verifying modes only). A value of this header sent by the client is always removed, and `request.headers` rules cannot change it. Without a client certificate (`verify_if_given`) the backend gets no such header. A subject with control characters is not passed. |

| `mode` | Client without a certificate | Certificate checked against `ca` |
|---|---|---|
| `"request"` | accepted | no |
| `"require"` | refused | no |
| `"verify_if_given"` | accepted | yes: a certificate that is not signed by `ca`, expired or not valid yet is refused |
| `"require_and_verify"` | refused | yes |

They are Traefik's `RequestClientCert`, `RequireAnyClientCert`,
`VerifyClientCertIfGiven` and `RequireAndVerifyClientCert`. `request` and
`require` accept any certificate, self-signed ones included, so they say
nothing about who the client is; that is why `header` needs a verifying
mode.

```toml
[[server]]
listen = "0.0.0.0:443"
host = "internal.example.com"
tls = { cert = "/etc/rxh/internal.pem", key = "/etc/rxh/internal.key" }
client_auth = { ca = "/etc/rxh/clients-ca.pem", header = "X-Client-Subject" }
forward = "@internal"

# Certificates are optional here; the backend tells users apart by the header.
[[server]]
listen = "0.0.0.0:8443"
tls = { cert = "/etc/rxh/internal.pem", key = "/etc/rxh/internal.key" }
forward = "@app"

[server.client_auth]
mode = "verify_if_given"
ca = ["/etc/rxh/clients-ca.pem", "/etc/rxh/partners-ca.pem"]
header = "X-Client-Subject"
```

The handshake happens before the request names a host, so `http` servers
sharing a listen address must have the same `client_auth` table. Use
different addresses for different policies. The `ca` files are read when the
listener starts. There is no revocation check (CRL or OCSP).

### TLS options

`tls_options` on a server with `tls` (files or `"auto"`, `http` or `tcp`)
narrows what its TLS handshakes accept. Without it a server takes TLS 1.2
and 1.3 with every cipher suite and curve below, which are all safe today:
set it to follow a stricter policy, not to fix a default.

| Key | Default | Description |
|---|---|---|
| `min_version` | `"1.2"` | Lowest TLS version accepted: `"1.2"` or `"1.3"`. TLS 1.0 and 1.1 are not implemented. |
| `cipher_suites` | all | List of the cipher suites offered, by IANA name, see below. The client's order of preference decides among them. TLS 1.2 clients need one of the `TLS_ECDHE_*` suites for the key type of the certificate (`ECDSA` or `RSA`), TLS 1.3 clients one of the first three; a list without TLS 1.3 suites is rejected with `min_version = "1.3"`. |
| `curves` | all | List of the key exchange groups offered: `"X25519MLKEM768"` (post-quantum hybrid, TLS 1.3 only), `"X25519"`, `"secp256r1"`, `"secp384r1"`. Case insensitive. The client's order of preference decides among them. |
| `ocsp_stapling` | `true` | Staple OCSP responses to the handshakes, see [OCSP stapling](#ocsp-stapling). |

Cipher suites:

| TLS 1.3 | TLS 1.2 |
|---|---|
| `TLS_AES_256_GCM_SHA384` | `TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384` |
| `TLS_AES_128_GCM_SHA256` | `TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256` |
| `TLS_CHACHA20_POLY1305_SHA256` | `TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256` |
| | `TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384` |
| | `TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256` |
| | `TLS_ECDHE_RSA_WITH_CHACHA20_POLY1305_SHA256` |

```toml
# TLS 1.3 only, on the two classic curves.
[[server]]
listen = "0.0.0.0:443"
host = "example.com"
tls = "auto"
tls_options = { min_version = "1.3", curves = ["X25519", "secp384r1"] }
forward = "@app"

# AES-256 only, for both protocol versions, and no OCSP requests.
[[server]]
listen = "0.0.0.0:8443"
tls = { cert = "/etc/rxh/internal.pem", key = "/etc/rxh/internal.key" }
forward = "@internal"

[server.tls_options]
cipher_suites = [
    "TLS_AES_256_GCM_SHA384",
    "TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384",
    "TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384",
]
ocsp_stapling = false
```

A client that shares no version, cipher suite or curve with the server
fails the handshake. Like `client_auth`, the handshake happens before the
request names a host, so `http` servers sharing a listen address must have
the same `tls_options` table.

#### OCSP stapling

A certificate can name the OCSP responder of its authority, which says
whether the certificate has been revoked. With stapling rxh asks the
responder itself and sends the signed answer in the handshake, so clients
neither wait for the responder nor tell it which sites they visit. It is on
for every TLS listener, with certificates from files and from ACME alike,
and does nothing for certificates that name no responder (Let's Encrypt
stopped naming one in 2025).

- The answer is fetched in the background with an HTTP `POST` to the
  responder when the listener starts, so the first handshakes may go
  without it, and again halfway through its validity (checked every hour).
- When the responder fails or says anything but `good`, the last answer
  stays stapled until it expires; then handshakes go on without one. Each
  failure is logged (`OCSP => http://...: reason`).
- The certificate file must hold the certificate of the issuer after the
  server certificate (a full chain), as the request is built from both.
  Only `http://` responders are asked, which is what authorities use.
- rxh does not verify the signature of the answer: the clients do.
- `ocsp_stapling = false` stops the requests, for hosts that cannot reach
  the responder.

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
| `ip_allow`, `ip_deny` | Client IP lists of this route, on top of the server's; a request the route matches but its lists reject gets `403`, see [IP allow and deny lists](#ip-allow-and-deny-lists). |
| `rate_limit` | Rate limit of this route, on top of the server's, see [Rate limits](#rate-limits). |
| `basic_auth`, `forward_auth` | Authentication of this route, each replacing the server's, see [Authentication](#authentication). |
| `forward` / `serve` / `redirect` | Action, exactly one. `serve` is a directory or a [file server table](#file-server), `redirect` a target or `{ to, status }`, see [Rewrites and redirects](#rewrites-and-redirects). |
| `rewrite` | New URI of the request before it is forwarded or served, see [Rewrites and redirects](#rewrites-and-redirects). |
| `handle_response` | List of handlers for upstream responses: change the status, or answer with a file or a redirect instead, see [Handling upstream responses](#handling-upstream-responses) (`forward` only). |
| `mirror` | Copies of the requests sent to other pools, their responses dropped: `{ pool, percent, max_body_size }` or a list, see [Request mirroring](#request-mirroring) (`forward` only). |
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

### IP allow and deny lists

`ip_allow` and `ip_deny` take CIDR ranges (`"10.0.0.0/8"`, `"fd00::/8"`) or
single addresses (`"192.0.2.7"`). A client is accepted when it is in no
`ip_deny` range and, if `ip_allow` is not empty, in an `ip_allow` range: a
deny wins, so `ip_deny` can cut holes in the allowed ranges. IPv4-mapped
IPv6 clients (`::ffff:10.0.0.1`) are checked as IPv4.

| Protocol | Client IP | Rejected client |
|---|---|---|
| `http` | The left-most `X-Forwarded-For` entry when the peer is in `trusted_proxies`, else the peer (or the PROXY header source with `proxy_protocol_in`) | `403 Forbidden`, nothing reaches the backend |
| `tcp` | The peer, or the PROXY header source | Connection closed before the TLS handshake, with a log line |
| `udp` | The sender of the datagram | Datagram dropped, without a log line |

On `http` servers the lists can also go on `match` entries, and a request
must pass both the server's lists and those of the route it matches. Unlike
the `remote_ip` matcher, which makes a request skip to the next route, a
route whose lists reject the client answers `403`. On a shared address each
server's lists apply to its own routes only. `tcp` and `udp` servers take the
lists on the server, not in `match` entries.

With `trusted_proxies`, the proxy in front must replace `X-Forwarded-For`
(or append to a value it trusts): rxh takes the left-most entry, so a
proxy that keeps what the client sent lets the client pick its address.

```toml
[[server]]
listen = "0.0.0.0:443"
tls = "auto"
host = "example.com"
trusted_proxies = ["10.0.0.0/8"]       # the load balancer in front
ip_deny = ["203.0.113.0/24"]
match = [
    # Office and VPN only, except one VPN address.
    { uri = "/admin", ip_allow = ["198.51.100.0/24", "10.8.0.0/16"], ip_deny = ["10.8.0.66"], forward = "@admin" },
    { forward = "@web" },
]

[[server]]
listen = "0.0.0.0:5432"
protocol = "tcp"
ip_allow = ["10.0.0.0/8"]
forward = "10.0.0.5:5432"
```

### Rate limits

`rate_limit` on an `http` server, or on one of its `match` entries, caps the
requests each client may send. Every client has a bucket of `burst` tokens
that gains `rate` tokens every `per`; a request takes one token, and a
request that finds the bucket empty gets `429 Too Many Requests` with a
`Retry-After` header (seconds until the next token, rounded up) without
reaching the backend. Requests are rejected, never delayed.

| Key | Default | Meaning |
|---|---|---|
| `rate` | required | Requests allowed every `per`, at least 1. |
| `per` | `"1s"` | Period of `rate`: `{ rate = 30, per = "1m" }` is one request every 2 seconds. |
| `burst` | `rate` | Size of the bucket: requests a client can send at once after a pause. |
| `key` | `"client_ip"` | What identifies a client. `"client_ip"`: the left-most `X-Forwarded-For` entry when the peer is in `trusted_proxies`, else the peer (or the PROXY header source). `"header:<name>"`: the value of a request header, such as an API key; requests without the header are counted by client IP. |

A server limit is shared by all the routes of the server; a route limit
counts the requests of that route only. A request must pass both, the
server's first, and takes a token from each one it passes. Clients rejected
by `ip_allow` or `ip_deny` get `403` and use no tokens. On a shared address
each server's limit applies to its own routes only.

```toml
[[server]]
listen = "0.0.0.0:443"
tls = "auto"
host = "example.com"
trusted_proxies = ["10.0.0.0/8"]
# 100 requests per second for each client IP, bursts of 200.
rate_limit = { rate = 100, burst = 200 }
match = [
    # 5 login attempts per minute for each client IP.
    { uri = "/login", rate_limit = { rate = 5, per = "1m" }, forward = "@web" },
    # 1000 requests per hour for each API key.
    { uri = "/api", rate_limit = { rate = 1000, per = "1h", burst = 50, key = "header:X-Api-Key" }, forward = "@api" },
    { forward = "@web" },
]
```

The buckets live in memory: they are shared by the runtimes of one process,
start full again after a reload or restart, and are not shared between
several rxh instances. An IPv6 client is one address, not its `/64`. With
`key = "header:..."` the header is whatever the client sends, so a client
that changes it gets a new bucket: use it for values the backend checks
(API keys), together with a server limit by client IP.

### Connections per client

`max_connections_per_ip` on an `http` or `tcp` server is the number of
connections one client IP may have open at once. A connection over the limit
is closed as soon as it is accepted (after the PROXY header with
`proxy_protocol_in`, before the TLS handshake), without a log line.
`max_connections` still caps the total.

```toml
[[server]]
listen = "0.0.0.0:5432"
protocol = "tcp"
max_connections = 500
max_connections_per_ip = 10
forward = "10.0.0.5:5432"
```

The client is the peer of the connection, or the source of the PROXY header:
`X-Forwarded-For` is not known when a connection is accepted, so behind an
HTTP proxy that does not send a PROXY header every client shares the proxy's
count; use `rate_limit` there. It counts connections, not requests: an
HTTP/2 connection with many streams counts once. Connections that survive a
reload are not counted by the new listeners. `udp` servers do not take it.

### Authentication

`basic_auth` and `forward_auth` on an `http` server protect every route of
the server; on a `match` entry they protect that route, and each one
replaces the server's setting of the same name there. A route with both
checks the password first. Both run after `ip_allow`/`ip_deny` and
`rate_limit`, so a rate limit also bounds password guessing.

#### Basic auth

`basic_auth` asks for a user name and password (RFC 7617). Requests without
valid credentials get `401 Unauthorized` with `WWW-Authenticate` and never
reach the backend.

| Key | Default | Meaning |
|---|---|---|
| `users` | required | `{ name = "hash" }`: user names and the bcrypt hash of their passwords (`$2a$`, `$2b$` or `$2y$`), never the passwords themselves. |
| `realm` | `"Restricted"` | Name shown in the password prompt of browsers. |

`rxh hash-password` hashes the password it reads from standard input;
`htpasswd -nB` and `caddy hash-password` produce the same format:

```console
$ echo 'correct horse' | rxh hash-password
$2b$12$kPuqTnXRi3Y7cvkLWm8wAeYpyHmdD9oIczdT8rhRWgBWjBVy7B5.G
```

```toml
[[server]]
listen = "0.0.0.0:443"
tls = "auto"
host = "example.com"
match = [
    { uri = "/admin", rate_limit = { rate = 10, per = "1m" }, basic_auth = { realm = "Admin", users = { root = "$2b$12$kPuqTnXRi3Y7cvkLWm8wAeYpyHmdD9oIczdT8rhRWgBWjBVy7B5.G" } }, forward = "@admin" },
    { forward = "@web" },
]
```

Use it over TLS only: the browser sends the password with every request. A
bcrypt check takes tens of milliseconds on purpose, so rxh runs it off the
request threads and remembers (as a SHA-256, in memory, until a reload) the
credentials that passed; wrong passwords pay the full cost each time. The
`Authorization` header is forwarded to the backend unchanged. bcrypt only
looks at the first 72 bytes of a password.

#### Forward auth

`forward_auth` asks an external service whether a request may pass, the way
Traefik's `ForwardAuth` and Caddy's `forward_auth` do. For each request rxh
sends a `GET` without a body to `url` with the headers of the request
(cookies and `Authorization` included) plus:

| Header | Value |
|---|---|
| `X-Forwarded-Method` | Method of the request. |
| `X-Forwarded-Proto` | `https` or `http`. |
| `X-Forwarded-Host` | Host the client asked for. |
| `X-Forwarded-Uri` | Path and query of the request, before `rewrite`. |
| `X-Forwarded-For` | Client IP. |

`X-Forwarded-For`, `-Proto` and `-Host` sent by the client are only kept
when the peer is in `trusted_proxies`, as for the backends; the method and
URI are always those of the request. A `2xx` answer lets the request
through. Any other answer goes to the client as it is, with its status,
headers and body: the redirect to the login page, or a `401`. When the
service cannot be reached the client gets `502`, when it does not answer
within `timeout`, `504`: without an answer there is no access.

| Key | Default | Meaning |
|---|---|---|
| `url` | required | `http://` or `https://` URL of the verification endpoint of the service. |
| `copy_headers` | `[]` | Headers of the `2xx` answer set on the request sent to the backend, such as the user name. The same headers sent by the client are removed, so the backend can trust them. |
| `timeout` | `"10s"` | Time the service has to answer. |

Authelia:

```toml
[[server]]
listen = "0.0.0.0:443"
tls = "auto"
host = "app.example.com"
forward_auth = { url = "http://authelia:9091/api/authz/forward-auth", copy_headers = ["Remote-User", "Remote-Groups", "Remote-Email", "Remote-Name"] }
forward = "@app"
```

Authentik (proxy provider in "forward auth" mode): the outpost path must
reach the outpost without the check, so the check goes on the application
route instead of the server.

```toml
[[server]]
listen = "0.0.0.0:443"
tls = "auto"
host = "app.example.com"
match = [
    { uri = "/outpost.goauthentik.io", forward = "authentik:9000" },
    { forward_auth = { url = "http://authentik:9000/outpost.goauthentik.io/auth/traefik", copy_headers = ["X-Authentik-Username", "X-Authentik-Groups", "X-Authentik-Email", "X-Authentik-Name", "X-Authentik-Uid"] }, forward = "@app" },
]
```

oauth2-proxy (started with `--reverse-proxy --upstream=static://202`, and
`--set-xauthrequest` for the headers): its root answers `202` to signed in
users and redirects the others to the provider.

```toml
[[server]]
listen = "0.0.0.0:443"
tls = "auto"
host = "app.example.com"
match = [
    { uri = "/oauth2", forward = "oauth2-proxy:4180" },
    { forward_auth = { url = "http://oauth2-proxy:4180/", copy_headers = ["X-Auth-Request-User", "X-Auth-Request-Email"] }, forward = "@app" },
]
```

Every check opens a new connection to the service. An `https://` service is
verified against the public certificate authorities. `Set-Cookie` headers of
a `2xx` answer are not passed on to the client.

### Security headers

`security_headers` adds a preset of security response headers to every
response of an `http` server or of a `match` entry, the ones RXH makes
itself (redirects, `401`, `403`, `404` of a route...) included. A header the
response already has, from the backend or from `response.headers.set`, is
kept as it is: the preset only fills the gaps.

```toml
[[server]]
listen = "0.0.0.0:443"
tls = "auto"
host = "app.example.com"
security_headers = "basic"
match = [
    { uri = "/admin", security_headers = "strict", forward = "@admin" },
    # The backend manages its own headers.
    { uri = "/embed", security_headers = "off", forward = "@widgets" },
    { forward = "@app" },
]
```

| Header | `basic` | `strict` |
|---|---|---|
| `X-Content-Type-Options` | `nosniff` | `nosniff` |
| `X-Frame-Options` | `SAMEORIGIN` | `DENY` |
| `Referrer-Policy` | `strict-origin-when-cross-origin` | `no-referrer` |
| `Strict-Transport-Security` (TLS servers only) | `max-age=31536000` | `max-age=63072000; includeSubDomains` |
| `Content-Security-Policy` | | `default-src 'self'; base-uri 'self'; form-action 'self'; frame-ancestors 'none'; object-src 'none'` |
| `Cross-Origin-Opener-Policy` | | `same-origin` |
| `Permissions-Policy` | | `camera=(), microphone=(), geolocation=(), payment=(), usb=()` |

- An entry's `security_headers` replaces the server's; `"off"` turns the
  server's off for that entry.
- `basic` breaks no ordinary site. `strict` forbids scripts, styles and
  images from other origins and inline scripts: set your own
  `Content-Security-Policy` with `response.headers.set` (or in the backend)
  when a site needs more, the rest of the preset stays.
- HSTS is only sent by servers with `tls`: browsers ignore it over plain
  HTTP. `preload` is never added, submit the domain to the preload list
  yourself and set the header with `response.headers.set`.

### CORS

`cors` makes RXH handle cross-origin resource sharing for an `http` server
or a `match` entry, whatever the backend does.

```toml
[[server]]
listen = "0.0.0.0:443"
tls = "auto"
host = "api.example.com"
cors = { origins = ["https://app.example.com", "https://*.example.com"], credentials = true, max_age = "1h" }
match = [
    # Anybody may read the public data.
    { uri = "/public", cors = { origins = "*", methods = ["GET", "HEAD"] }, forward = "@api" },
    { forward = "@api" },
]
```

| Key | Default | Meaning |
|---|---|---|
| `origins` | required | Allowed origins: `"https://app.example.com"` (scheme, host and port must match), `"https://*.example.com"` for any subdomain (not `example.com` itself), or `"*"` for any origin. Lowercase, without a path. A string or a list. |
| `methods` | `["GET", "HEAD", "POST", "PUT", "PATCH", "DELETE"]` | `Access-Control-Allow-Methods` of preflight answers. |
| `headers` | the requested ones | `Access-Control-Allow-Headers` of preflight answers. Unset, a preflight is allowed the headers it asks for in `Access-Control-Request-Headers`. |
| `expose_headers` | none | `Access-Control-Expose-Headers`: response headers scripts may read besides the safelisted ones. |
| `credentials` | `false` | Allows cookies and `Authorization` (`Access-Control-Allow-Credentials: true`). Cannot be combined with `"*"`. |
| `max_age` | unset | `Access-Control-Max-Age`: how long browsers may cache a preflight answer, as a duration (`"10m"`). |

- Preflights (`OPTIONS` with `Origin` and `Access-Control-Request-Method`)
  are answered by RXH with `204` and never reach the backend. They skip
  `basic_auth` and `forward_auth`, since browsers send them without
  credentials, but not `ip_allow`, `ip_deny` or `rate_limit`. A preflight
  from an origin that is not allowed gets a `204` without the
  `Access-Control-Allow-*` headers, which makes the browser block the
  request.
- Other responses of an allowed origin get `Access-Control-Allow-Origin`
  (the request's origin, or `*` when `origins` has `"*"`),
  `Access-Control-Allow-Credentials` and `Access-Control-Expose-Headers`.
  The `Access-Control-Allow-Origin`, `-Allow-Credentials` and
  `-Expose-Headers` of the backend are always removed, and `Vary: Origin`
  is added unless the answer is `*`.
- An entry's `cors` replaces the server's.

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

### Handling upstream responses

`handle_response` on a `forward` route decides what happens with the
responses of the backend (Caddy's `handle_response`, `replace_status` and
`copy_response`). Each entry has matchers and exactly one action; the first
entry that matches a response runs, and responses matching none are proxied
as usual.

```toml
[[server]]
listen = "0.0.0.0:80"
forward = "@app"
handle_response = [
    # Keep the response, change its status.
    { status = 404, replace_status = 200 },
    # X-Accel-Redirect: the backend names a file, rxh sends it.
    { header = { x-accel-redirect = "*" }, serve = "/srv/private", rewrite = "{header.x-accel-redirect}" },
    # 504 passes untouched, the other errors get /srv/errors/502.html, ...
    { status = 504, copy_response = true },
    { status = "5xx", serve = "/srv/errors", rewrite = "/{status}.html" },
    { status = 401, redirect = { to = "/login?next={path}", status = 303 } },
]
```

| Key | Description |
|---|---|
| `status` | Matcher: a status code, a class (`"5xx"`) or a list of them; any may match. Unset matches every status. |
| `header` | Matcher: `{ name = "value" }`, the response must have the header with this exact value; `"*"` only requires the header. |
| `replace_status` | Action: send the upstream response with this status (`200` to `599`). |
| `copy_response` | Action: send the upstream response as it is (`true`), which exempts it from the entries below, or with a status (`copy_response = 500`, same as `replace_status`). |
| `serve` | Action: drop the upstream response and answer with a file from this directory or [file server table](#file-server). |
| `rewrite` | With `serve`: path of the file instead of the request path. |
| `redirect` | Action: drop the upstream response and answer with a redirect, `"target"` or `{ to, status }` as in [Rewrites and redirects](#rewrites-and-redirects). |

- `rewrite` and `redirect` take the request placeholders (`{host}`,
  `{path}`, `{query}`, `{uri}`) plus `{status}`, the upstream status code,
  and `{header.NAME}`, the first value of an upstream response header
  (empty when missing).
- Matching uses the response as the backend sent it, before
  `response.headers` rules. Responses kept by `replace_status` and
  `copy_response` then go through those rules, `cache` and `encode` as
  usual; responses made by `serve` and `redirect` only go through `encode`.
- A file found by `serve` is sent with the status of the upstream response,
  so an error page keeps its `503`; a missing file is a `404`. The file
  answers any request method, and it cannot be outside the root whatever
  the backend puts in a header.
- Only responses of a backend are handled: the `502`, `503` and `504` that
  rxh answers itself when no backend can be reached are not. Passive health
  checks and `retry_match` see the upstream status, not the replaced one.
- An entry without action or with two, a `replace_status` outside `200` to
  `599`, `copy_response = false`, `rewrite` without `serve` and
  `handle_response` on a `serve` or `redirect` route are configuration
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
`http` and `udp` servers. `rewrite`, `request`, `response`, `handle_response`,
`mirror`, `encode` and `cache` only apply to `http` servers and are rejected
elsewhere.

### PROXY protocol input

Behind a load balancer that speaks the PROXY protocol (HAProxy, AWS NLB,
another RXH with `proxy_protocol`), set `proxy_protocol_in = true` on an
`http` or `tcp` server. Every connection must then start with a v1 or v2
header, read within `match_timeout`, before TLS and before any matching. Its
source address becomes the client address everywhere: logs, `ip_hash`,
`remote_ip`, `X-Forwarded-For`, `Forwarded`, `trusted_proxies` decisions and
the outgoing `proxy_protocol` header. Connections without a
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

### PROXY protocol output

`proxy_protocol = "v1"` (text) or `"v2"` (binary) on an `http` or `tcp` server
starts every new connection to a backend with a PROXY protocol header: the
source is the client address (the one of the incoming header with
`proxy_protocol_in`) and the destination the address the client connected to.
The backend must expect it (`accept-proxy` in HAProxy, `listen ... proxy_protocol`
in nginx, `proxy_protocol_in` in RXH). With TLS backends the header goes first,
in cleartext, and the TLS handshake follows.

```toml
[[server]]
listen = "0.0.0.0:80"
proxy_protocol = "v2"
forward = "@web"
```

On `http` servers:

- The header describes one client connection, so an upstream keep-alive
  connection is only reused for requests of the client connection it was
  opened for. Idle ones are closed after `keepalive`; `max_conns_per_host`
  only counts connections carrying a request.
- It applies to every `forward` of the server, and is rejected when one of
  them uses an `h2c` pool, whose connections are shared by all clients.
- Active health checks send no header: check a port of the backend that does
  not require it (`health.port`), or rely on passive checks.

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
