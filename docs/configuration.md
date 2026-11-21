# Configuration reference

RXH reads one TOML file (`rxh.toml` by default, `-c` or `RXH_CONFIG` to
change it). `rxh validate` checks a file without starting anything and
warns about deprecated keys (see [Configuration migration](migration.md)),
and [`examples/full.toml`](../examples/full.toml) shows every option at
once. Step by step guides per use case are listed on the
[documentation home](README.md).

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
[tracing]                # OpenTelemetry spans and W3C trace context
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

## Converting a Caddyfile or an nginx configuration

`rxh adapt FILE` prints the RXH configuration of a Caddyfile or an nginx
configuration on standard output. The format comes from the file name
(`Caddyfile`, `*.caddyfile`; `nginx*`, `*.conf`), else from the content (nginx
when a line ends with `;`); `--adapter caddyfile|nginx` forces it. What has
no RXH equivalent is left out and listed, with its line, on standard error
and as `# Not converted:` comments at the top of the output. The command
fails when the file cannot be parsed, when nothing could be converted, or
when RXH rejects the result (still printed, to fix by hand).

```sh
rxh adapt /etc/caddy/Caddyfile > rxh.toml
rxh adapt --adapter nginx /etc/nginx/sites-available/app > rxh.toml
rxh validate -c rxh.toml
```

Caddyfile: global `email`, `acme_ca`, `admin`, `http_port`, `https_port`,
`auto_https off|disable_redirects` and `grace_period`; site addresses (one
server per listen address, `tls = "auto"` for host names like Caddy's
automatic HTTPS); `reverse_proxy` with load balancing, retries, active and
passive health checks, `header_up`/`header_down` and the `http` transport's
timeouts and TLS; `file_server`, `root`, `redir`, `handle`, `handle_path`,
`route`, `encode`, `header`, `basic_auth`, `tls` with certificate files,
`{$VAR}` as `{env.VAR}`, and named matchers on `path`, `method`, `host`,
`header`, `query`, `remote_ip` and `client_ip`. Routes are ordered as Caddy
orders its directives. Snippets, `import`, `php_fastcgi`, `rewrite`,
`try_files` and other directives are reported.

nginx: `http` and `stream` servers (or bare `server` blocks of a
`sites-available` file), `upstream` blocks with weights, `max_fails`,
`fail_timeout`, `least_conn`, `ip_hash`, `random`, `hash` and `keepalive`;
`listen`, `server_name`, `ssl_certificate`, exact and prefix `location`s
with `proxy_pass`, `root`, `alias`, `index`, `autoindex`, `return` of a
redirect, `add_header`, `proxy_set_header`, `gzip` and
`client_max_body_size`, ordered as nginx picks them. `$host`,
`$request_uri`, `$uri`, `$args` and `$server_name` become placeholders.
Regex and named locations, other variables, `include`, a URI in
`proxy_pass` and `return` of a body are reported.

Caddy's `header` and nginx's `add_header` become `response.headers` on
proxied routes only: RXH's response header rules do not apply to served
files and redirects, so those routes get a warning instead.

```nginx
upstream app { server 10.0.0.1:8080 weight=3; server 10.0.0.2:8080; }
server {
    listen 80;
    server_name example.com;
    location / { proxy_pass http://app; }
}
```

becomes

```toml
# Converted from nginx.conf by rxh adapt.

[upstream.app]
backends = [{ address = "10.0.0.1:8080", weight = 3 }, "10.0.0.2:8080"]

[[server]]
listen = "0.0.0.0:80"
host = "example.com"

[[server.match]]
forward = "@app"
```

## Formatting and editor completion

`rxh fmt [FILE...]` rewrites configuration files (the configuration file
when no FILE is given) in one layout: no indentation, one space around
`=`, no spaces inside `[table]` headers, a blank line before each table,
at most one blank line in a row and one newline at the end. Only
whitespace changes: keys, values, their order and every comment stay. The
spaces before a trailing comment and the inside of arrays and inline
tables are kept as written, so aligned comments and lists stay aligned.
`--check` changes nothing, prints the files that are not formatted and
fails if there is any, for CI. A file that is not valid TOML is reported
and left alone; `fmt` checks the syntax only, `rxh validate` the meaning.

```sh
rxh fmt                            # rxh.toml, or -c FILE
rxh fmt --check conf.d/*.toml      # CI: fails when a file needs formatting
```

`rxh schema` prints the JSON schema (draft 07) of the configuration file,
generated from the same definitions the parser uses, with the
documentation of every key. The schema of this version is also in
[`docs/rxh.schema.json`](rxh.schema.json). Editors with a TOML language
server that reads JSON schemas ([Taplo](https://taplo.tamasfe.dev/), the
Even Better TOML extension of VS Code, [Tombi](https://tombi-toml.github.io/tombi/))
then complete keys, show their documentation on hover and flag unknown
keys and wrong values as you type. Point the file at the schema with a
directive on its first line:

```toml
#:schema ./rxh.schema.json
# or the published copy:
#:schema https://raw.githubusercontent.com/LOSEARDES77/rxh/main/docs/rxh.schema.json

[[server]]
listen = "127.0.0.1:8080"
forward = "127.0.0.1:3000"
```

```sh
rxh schema > rxh.schema.json
```

The directive is a comment, so RXH ignores it. The schema describes the
main file; files pulled in with `include` hold only `upstream` and `server`
and match it too. Checks that need more than one key (`forward` and
`match` together, a pool that does not exist) are left to `rxh validate`.

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

## Upgrading the binary without downtime

On Linux a running daemon can be replaced by a new binary without closing
its listening sockets. Install the new binary over the old one, then:

```sh
rxh upgrade                  # or: kill -USR2 "$(cat rxh.pid)", or POST /upgrade
```

The daemon starts its own command line again (same program name, arguments,
environment and working directory, so the binary now at that path runs) and
leaves it a duplicate of every socket it listens on: `http`, `tcp` and `udp`
servers and the admin API. The new process loads the configuration file,
its listeners use the sockets whose address equals their `listen`, and it
tells the old process once everything is up. The old process then stops
accepting, lets its open connections finish as on `rxh stop` (`grace_period`,
`stream_close_delay`) and exits. Both processes share the sockets in between,
so no connection is refused and nothing waiting in a backlog is lost.

- If the new process exits before it is ready (a configuration the new
  version rejects, a binary that does not start), the old one logs
  `Upgrade failed, this process keeps serving` and nothing changes;
  `rxh upgrade` and `POST /upgrade` report the failure (`400`). A new process
  that hangs before it is ready leaves the old one serving too.
- `rxh upgrade` waits until the new process answers `GET /status`. Without
  the admin API it sends `SIGUSR2` to the PID in `--pid-file` and returns.
- A daemon started with `rxh start` keeps its log file, and its PID file
  names the new process once the old one has exited.
- The new process starts from the configuration file, like a restart:
  changes made through the admin API only, drain and maintenance states,
  counters, rate limit and sticky session tables are not carried over.
- A `listen` on port 0 gets a new port. With `runtime = "per_core"` one
  socket per address is handed over and the new process binds the others;
  connections queued on the old ones can be reset, as on a reload.
- With systemd, the old process sends `MAINPID=` so the unit follows the new
  one. Sockets that came from socket activation are handed over like the
  others, but systemd already keeps those open across `systemctl restart`,
  which is the simpler way to upgrade there.
- The challenge-only ACME listener is not handed over: it is only open
  while a certificate is being obtained, and the new process retries until
  the port is free.

Not available on other systems: `rxh upgrade` fails and the daemon keeps
running.

## Running under systemd

[`dist/rxh.service`](../dist/rxh.service) runs `rxh run` as a
`Type=notify-reload` service. When `NOTIFY_SOCKET` is set, rxh sends
`READY=1` once every listener and the admin API are up, `RELOADING=1` and
then `READY=1` around every reload (`SIGHUP`, `rxh reload`, `watch`), with
`STATUS=Reload failed: ...` when the new configuration is rejected (the
previous one keeps running), and `STOPPING=1` on shutdown. No watchdog.

The unit runs the daemon as a user systemd makes for it (`DynamicUser=yes`)
with `ProtectSystem=strict` and no `WorkingDirectory` or `StateDirectory`:
the daemon can write nowhere. `/etc/rxh` is readable and not writable, so
the files that are relative to the directory of the configuration by
default, the users file (`rxh-data/users.toml`) and the ACME storage
(`rxh-data/certificates`), cannot be created there: creating the first user
answers `400` with `Permission denied`, and ACME cannot keep its
certificates. Give the service a state directory with a drop-in and put
both there:

```ini
# systemctl edit rxh
[Service]
StateDirectory=rxh
```

```toml
[admin]
users = "/var/lib/rxh/users.toml"

[acme]
storage = "/var/lib/rxh/certificates"
```

Create the first user through the daemon (the page, or `curl` on the
machine to `POST /auth/setup`), so that the file belongs to the user of
the service. `sudo rxh -c /etc/rxh/rxh.toml user ...` then works on it: a
file it replaces keeps its owner. A users file that `rxh user` run as root
creates belongs to root, mode `0600`, and the service cannot read it.

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
applies).

The images ship no configuration. Relative paths of the configuration are
taken from its directory, `/etc/rxh`, which is mounted read-only: the
default ACME storage (`/etc/rxh/rxh-data/certificates`) and users file
(`/etc/rxh/rxh-data/users.toml`) cannot be written there. Point both at
`/var/lib/rxh`, which belongs to UID 65532, and keep it on a volume:

```toml
[admin]
users = "/var/lib/rxh/users.toml"

[acme]
storage = "/var/lib/rxh/certificates"
```

```sh
docker run -d --name rxh -p 80:80 -p 443:443 \
  -v "$PWD/rxh.toml:/etc/rxh/rxh.toml:ro" \
  -v rxh-data:/var/lib/rxh \
  ghcr.io/loseardes77/rxh:0.3
docker exec rxh rxh status   # the admin API stays on 127.0.0.1:2019 inside
docker kill -s HUP rxh       # reload
```

To open the dashboard from outside the container, the admin API must listen
on `0.0.0.0` and be published. A published port reaches the container from
another address, so every request is remote, the host's own browser
included: the listener needs `token` or `tls`. With `tls` and no `token`,
the first user is created with the setup code of the log (`docker logs
rxh`) or with `docker exec -it rxh rxh user add ana`; see
[Docker](guides/docker.md#the-dashboard-from-outside-the-container).

The Docker provider needs the socket mounted and readable by UID 65532
(`--group-add` the group of the socket, or a socket proxy over TCP).

## `[admin]`

| Key | Default | Description |
|---|---|---|
| `enabled` | `true` | Serve the admin API. The CLI needs it for live changes; without it, `stop` and `reload` fall back to signals and the PID file. |
| `listen` | `"127.0.0.1:2019"` | Where the admin API listens: `ip:port`, or `unix//absolute/path.sock` for a unix socket. A TCP address outside loopback needs `token` or `tls`. Changes need a restart. |
| `token` | none | Secret every request must send as `Authorization: Bearer <token>`; `401` otherwise. At least 16 printable ASCII characters, no spaces. Changes need a restart. |
| `tls` | none | `{ cert = "admin.pem", key = "admin.key" }`: HTTPS on the admin listener, from PEM files; plain HTTP is still answered on the same port, to connections from the machine only. `"auto"` is not available here. A file that cannot be read stops the start. `POST /certificates/reload` (`rxh certs reload`) reads the files again; setting or removing `tls` needs a restart. |
| `users` | `"rxh-data/users.toml"` | File holding the user accounts and roles of the admin API. A relative path is relative to the directory of the configuration file, like `[acme] storage`, so the daemon and `rxh user` find the same file wherever they run. Must not be empty. Changes need a restart. In the container images the configuration directory is not writable: set an absolute path on the volume, `users = "/var/lib/rxh/users.toml"`. |

On a unix socket the API is reachable only by the user running the daemon
(and root): the file is created with mode `0600`, a socket left behind by a
dead daemon is replaced, and the file is removed on shutdown.

```toml
[admin]
listen = "unix//run/rxh/admin.sock"
```

For remote administration, listen on a reachable address and set a token
or `tls`. RXH refuses to start with a non-loopback `listen` and neither.

```toml
[admin]
listen = "10.0.0.5:2019"
token = "kq3Zp0uX7mV9cR2tLw8yHs4d"   # for instance: openssl rand -base64 24
```

```toml
[admin]
listen = "0.0.0.0:2019"
tls = { cert = "/etc/rxh/admin.pem", key = "/etc/rxh/admin.key" }
users = "/var/lib/rxh/users.toml"
```

```sh
curl -H "Authorization: Bearer kq3Zp0uX7mV9cR2tLw8yHs4d" http://10.0.0.5:2019/status
rxh --admin 10.0.0.5:2019 --admin-token kq3Zp0uX7mV9cR2tLw8yHs4d status
```

- With `tls` the listener tells a TLS handshake from HTTP by the first byte
  of each connection, so one port answers both. HTTPS (HTTP/1.1, TLS 1.2
  and 1.3, the defaults of a server without `tls_options`) is for
  everybody. Plain HTTP is answered only to a connection from the machine
  itself; from anywhere else it is closed without an answer. So the CLI and
  `curl` on the machine work as without `tls`:

  ```sh
  curl https://10.0.0.5:2019/status       # from anywhere: HTTPS
  curl http://127.0.0.1:2019/status       # on the machine: plain HTTP
  rxh status                              # on the machine: plain HTTP
  ```
- A connection is from the machine when it comes over the unix socket, or
  over TCP from a loopback address or from the very address it connected to
  (the machine calling its own address). The address is the one of the
  connection, not one a header claims. So what stands in front of the API
  decides, not the caller behind it:
  - A reverse proxy or a tunnel on the same machine connects from loopback,
    which makes every request it forwards a request from the machine,
    wherever the caller is; most proxies also send the address they
    connect to as `Host`. Without a token that is administrator access for
    everybody who reaches the proxy: set `token` whenever a proxy on the
    machine forwards to the API.
  - A proxy on another machine, or a port forwarder that connects from
    another address (a published Docker port), makes every request remote.
  - RXH itself is such a proxy, so without a `token` it does not connect to
    its own admin API for anybody: a `forward`, pool member, health check,
    mirror, `forward_auth`, webhook or tracing endpoint that leads there is
    a backend that refuses connections (`502` for HTTP, a closed connection
    for TCP, an unhealthy member), and the log says `refused to proxy to
    the admin API of this process; set [admin] token to allow it` once a
    minute at most. What counts is the address the connection goes to, not
    how it is written: the `listen` address, any loopback address with the
    admin port (a name that resolves to one too), any address of the
    machine when `listen` is `0.0.0.0` or `::`, and the admin unix socket
    under any name. Other ports and other machines are not concerned. With
    a `token` the API can be published through a host, since every request
    then needs the token or a session:

    ```toml
    [admin]
    listen = "127.0.0.1:2019"
    token = "kq3Zp0uX7mV9cR2tLw8yHs4d"

    [[server]]
    name = "admin"
    listen = "0.0.0.0:8080"
    forward = "127.0.0.1:2019"
    ```
- On a `tls` listener a connection has 10 seconds to send its first byte
  and finish the TLS handshake, then it is closed.
- Without a token, on TCP and on a unix socket alike, a request from the
  machine is refused
  with `403` unless its `Host` is
  `localhost`, `127.0.0.1` or `::1` (or the
  `listen` address of a TCP listener) and its `Origin`, when it has one, is
  that same `Host`, port included. The first stops a page of another site
  that reaches the API through DNS rebinding; the second stops a form or a
  script of any other page, including one served by another web server of
  the same machine (`http://localhost:3000`). A unix socket is checked too
  because a browser reaches it once it is forwarded to a local port
  (`ssh -L`). The CLI and `curl` send no `Origin`, and the dashboard sends
  its own, whatever the local port of a tunnel. With a token any `Host` and
  `Origin` are accepted: the token is the check, also on a unix socket. A
  request from another machine (over `tls`) without a token is nobody's
  until it signs in, see [Accounts](#accounts): `401`.

  ```sh
  curl --unix-socket /run/rxh/admin.sock http://localhost/status         # 200
  curl --unix-socket /run/rxh/admin.sock http://rxh/status               # 403: use localhost
  curl -H 'Origin: http://localhost:3000' http://localhost:2019/status   # 403
  ```
- `GET /dashboard`, `/dashboard/app.js` and `/dashboard/app.css` are
  served without the token: the page and its script and style
  hold no data. The page asks who it is, shows a sign-in form to nobody, and
  sends the cookie of the session or the token on its requests.
  So are `GET /auth/session`, `POST /auth/login`, `POST /auth/logout` and
  `POST /auth/setup`, the way in and out for [accounts](#accounts).
- Without `tls` the admin API speaks plain HTTP: the token is as private as
  the network it crosses. Set `tls`, or use a private network, a VPN or an
  SSH tunnel (`ssh -L 2019:/run/rxh/admin.sock host`), not the open
  internet.
- The CLI finds the address in `--admin` / `RXH_ADMIN` (`ip:port` or
  `unix//path`), else in `[admin] listen` of its configuration file. The
  token comes from `--admin-token` / `RXH_ADMIN_TOKEN`, else from `[admin]
  token` of that file, which is only sent to the `listen` address of the
  same file.
- `GET /config` shows the token as `***`. Keep the configuration file
  readable by the daemon's user only.

### Accounts

People sign in to the admin API with a name and a password of their own,
kept in the file `[admin] users` (bcrypt hashes, mode `0600`, created on
first use). The token and the CLI keep working as they are. Who sends a
request is the first of these that holds:

1. `Authorization: Bearer <token>` equal to `[admin] token`: an
   administrator.
2. The cookie of a session, over HTTPS or from the machine: that user,
   with the roles the users file gives the user at that moment. A user that
   was removed from the file, or whose password in the file is not the one
   it signed in with, is nobody, and the session ends.
3. No `token` configured and a request from the machine (the rule above):
   an administrator. This is what `rxh status` and `curl` on the machine
   are.
4. Otherwise nobody: `401`, with `WWW-Authenticate: Bearer` when a token is
   configured.

Only an administrator (the token, a request from the machine without a
token, or a user with the `admin` role) is answered by the routes of the
admin API; any other signed-in user gets `403 {"error": "this needs the
admin role"}` on them, except on the routes that read and change hosts, which
answer as far as the roles of the user allow: see
[Permissions](#permissions). `[admin] users`, like `listen`, `token` and `tls`, is
read when the daemon starts.

```toml
[admin]
listen = "0.0.0.0:2019"
tls = { cert = "/etc/rxh/admin.pem", key = "/etc/rxh/admin.key" }
users = "/var/lib/rxh/users.toml"   # default: rxh-data/users.toml
```

| Route | For | Does |
|---|---|---|
| `GET /auth/session` | anyone | Who the caller is, see below. |
| `POST /auth/login` | anyone | `{"name", "password"}`: `204` and the session cookie. `401 {"error": "wrong name or password"}` for a wrong password and for an unknown name alike; `429` after too many attempts, see below; `403` on plain HTTP from another machine. |
| `POST /auth/logout` | anyone | Ends the session of the cookie and clears the cookie: `204`, also when there is no session. |
| `POST /auth/setup` | see First run | `{"name", "password", "code"?}`: creates the first user and signs it in, `204`. `409` once a user exists. |
| `PUT /auth/password` | a signed-in user | `{"current", "new"}`: `204`. Ends every session of the user and gives this browser a new one. `403` when `current` is wrong. |

```sh
curl -c jar -d '{"name": "ana", "password": "correct horse"}' https://rxh.example.com:2019/auth/login
curl -b jar https://rxh.example.com:2019/auth/session
curl -b jar https://rxh.example.com:2019/status
curl -b jar -X POST -H 'Origin: https://rxh.example.com:2019' https://rxh.example.com:2019/auth/logout
```

```json
{
  "user": "ana",
  "admin": true,
  "roles": ["admin"],
  "grants": [],
  "creatable_groups": [],
  "setup": false,
  "setup_needs_code": false,
  "password_login": true,
  "oidc": false
}
```

- `user` is `null` for a caller that is not signed in, which includes the
  token and a request from the machine: for those `admin` is `true`.
  `grants` and `creatable_groups` are what the roles of a user without
  `admin` allow, and empty for an administrator, who may do everything.
- `setup` is `true` while the users file has no user. `setup_needs_code`
  says that this caller can create the first one only with the setup code.
- `password_login` says whether `POST /auth/login` works on this
  connection: over HTTPS, or from the machine. A password does not travel
  in clear text over the network, so a listener without `tls` reached from
  another machine only takes the token.
- A session is a cookie, `rxh_session`, with `HttpOnly; SameSite=Strict;
  Path=/` and, over HTTPS, `Secure`. It lasts 12 hours from the sign-in,
  used or not. Sessions are kept in memory, at most 1000 (the oldest goes
  first): a restart or a hot upgrade of the daemon signs everybody out.
- A request with a session cookie that is not a `GET` or a `HEAD` must
  carry an `Origin` equal to its `Host`, port included, as a page of the
  dashboard does: `403` otherwise. `curl` needs `-H 'Origin: ...'` for
  those, as above; requests with the token need nothing.
- `POST /auth/login`, `/auth/logout` and `/auth/setup` refuse (`403`) an
  `Origin` that is not their `Host`, with or without a token, so the page
  of another site cannot sign a browser in as an account of its choice.
  Without an `Origin`, as `curl` sends them, they are answered.
- A session exists only over HTTPS or on a connection from the machine. On
  plain HTTP from another machine a cookie is ignored and none is given:
  `POST /auth/setup` with the token creates the user there and signs
  nobody in.
- At most 4 passwords are hashed or compared at a time. A sign-in, a
  password change or a setup that comes while 4 are at it waits for its
  turn, and gets `429` when none came within 5 seconds. A sign-in that
  waited and was refused counts as a failed one for its name.
- After 5 failed sign-ins for a name from one client address, that name is
  refused from that address for a minute (`429`), with the right password
  too. A wrong `current` in `PUT /auth/password` counts as one. A sign-in
  counts as failed until its password is found right, so more than five at
  the same time under one name from one address are refused as well.
- One client address gets 10 sign-in attempts a minute, right or wrong and
  whatever the names: the eleventh is `429` until the oldest is a minute
  old. A password change and a `POST /auth/setup` that sends a `code`
  count as attempts. A unix socket has no such limit, and requests with
  the token or with a session are not sign-ins. Clients behind one address
  (a NAT, a reverse proxy) share its ten.
- A password has at least 8 characters and at most 72 bytes. Hashes never
  leave the daemon.
- Each sign-in, failed or refused sign-in, sign-out, password change and
  the creation of the first user is a line in the log of the daemon, with
  the name and the address of the client.
- Behind a reverse proxy on the same machine every request comes from the
  machine under the name of the proxy, which without a token is refused
  (`403`) before it can sign in: set `token` there, as said above, and
  people sign in with their accounts all the same. They then all count as
  one client address for the two limits above.

#### First run

While the users file has no user the API works as without accounts, and
`POST /auth/setup` creates the first user, with the `admin` role. It is
answered:

- to an administrator: the holder of the token, or a request from the
  machine when no token is set;

  ```sh
  curl -d '{"name": "ana", "password": "correct horse"}' http://127.0.0.1:2019/auth/setup
  ```
- to anyone who sends the setup code, on a `tls` listener without a
  `token`. The code is 10 characters, new at every start, and written to
  the log of the daemon while no user exists:

  ```text
  Admin => no user exists yet; to create the first one from another machine use the setup code k7m2xq9hpd
  ```

  ```sh
  curl -d '{"name": "ana", "password": "correct horse", "code": "k7m2xq9hpd"}' https://rxh.example.com:2019/auth/setup
  ```

Anybody else gets `403`, and once a user exists everybody gets `409`: the
code is of no use any more. There are no default credentials.

#### From the command line

`rxh user` edits the users file directly, with or without a running
daemon, so it is also the way back in when every password is lost. It
reads `[admin] users` from the configuration file (`-c`, default
`rxh.toml`) and, like the daemon, takes a relative path from the directory
of that file: give it the `-c` the daemon runs with, from any directory.
The rules are the ones of the routes below, and their refusals are printed
as they are.

```sh
rxh -c /etc/rxh/rxh.toml user add ana   # the first user gets the admin role
rxh user add luis --role web-editors --role shop
rxh user list                       # "ana admin", "luis web-editors,shop"
rxh user passwd ana
rxh user rm luis                    # never the last administrator
echo 'correct horse' | rxh user passwd ana
```

`add` and `passwd` ask the password twice, without showing it, on a
terminal, and read one line of standard input otherwise. A new password
written by `rxh user passwd`, by hand or by any other process ends the
sessions of that user in a running daemon at their next request, as
`PUT /users/{name}` does.

#### Users and roles

An administrator manages the users file through these routes, which
answer nobody else. They change the file the way an edit by hand does, and
a change applies to the next request, of a signed-in user too.

| Route | Does |
|---|---|
| `GET /users` | `[{"name", "roles"}]`, in the order of the file. Never a hash. |
| `POST /users` | `{"name", "password", "roles"?}`: adds a user, `201` with `{"name", "roles"}`. |
| `PUT /users/{name}` | `{"roles"?, "password"?}`, at least one: replaces them, `204`. A new password ends every session of the user. |
| `DELETE /users/{name}` | Removes the user and ends its sessions, `204`. |
| `GET /roles` | The built-in role first, as `{"name": "admin", "builtin": true}`, then every `[[role]]` as `{"name", "grant"}`. |
| `POST /roles` | `{"name", "grant"?}`: defines a role whose name is free, `201` with the role. |
| `PUT /roles/{name}` | `{"name"?, "grant"}`: replaces the grants of a defined role, `204`. A `name` in the body must be the one of the path. |
| `DELETE /roles/{name}` | Removes a role that no user has, `204`. |

- What the rules of the users file refuse is `409` with their reason: a
  name that is taken or not valid, a password shorter than 8 characters or
  longer than 72 bytes, a role that is not defined, `create` on a host
  grant, the last administrator removed or without `admin`, the `admin`
  role defined or removed, a role some user has (the reason names them).
- A name in the path that names no user or role is `404`; a body that is
  not JSON, lacks a field or has an unknown one is `400`.
- Names in the path are percent-decoded: `/users/ana%40example.com` is the
  user `ana@example.com`.
- Adding a user or setting a password hashes it, and waits for its turn
  with the sign-ins (at most 4 at a time, `429` after 5 seconds).
- Each change is a line in the log of the daemon with who made it (the
  user, `token` or `local`), never a password or a hash.

```sh
H='Authorization: Bearer s3cret-admin-token'
API=https://rxh.example.com:2019
curl -H "$H" $API/users
curl -H "$H" -d '{"name": "web-editors", "grant": [{"group": "web", "allow": ["view", "modify", "create"]}]}' $API/roles
curl -H "$H" -d '{"name": "luis", "password": "luis-password", "roles": ["web-editors"]}' $API/users
curl -H "$H" -X PUT -d '{"roles": ["admin"]}' $API/users/luis
curl -H "$H" -X PUT -d '{"grant": [{"host": "shop", "allow": ["view"]}]}' $API/roles/web-editors
curl -H "$H" -X DELETE $API/users/luis
curl -H "$H" -X DELETE $API/roles/web-editors
```

```text
Admin => Role web-editors created by token
Admin => User luis created by token
Admin => User luis changed by ana
```

#### Permissions

A role grants permissions on hosts: on every `[[server]]` whose `groups` has
a group, or on the one whose `name` is the `host` of the grant. The
permissions of a user on a host add up over its roles and over the groups
of the host; none implies another, so `modify` without `view` changes
nothing.

```toml
# rxh.toml
[[server]]
name = "shop"
listen = "0.0.0.0:8080"
groups = ["web"]
forward = "@api"
```

```toml
# users.toml
[[role]]
name = "web-editors"
grant = [
  { group = "web", allow = ["view", "modify", "create", "delete"] },
  { host = "intranet", allow = ["view"] },
]
```

| Action | Needs |
|---|---|
| See a host in lists, its statistics and its form | `view` |
| Change a host (form or the TOML of its table) | `view` and `modify`, and only the keys of the list below |
| Add a group to a host | `modify` on the host and `create` on the added group |
| Remove a group from a host | `modify` on the host, and still `modify` on it after the change |
| Change the `name` of a host, give it one or take it away | the `admin` role |
| Create a host whose `name` a `host` grant of any role names | the `admin` role |
| Create a host | `create` on every group in its `groups`, which must not be empty; only the keys of the list below, a `host` of its own, and `listen` addresses that are already listened on |
| Set any other key of a host, a value with an `{env.NAME}` or `{file.PATH}` placeholder, a unix socket as a forward target, or a new `listen` address | the `admin` role |
| Delete a host | `view` and `delete` |
| Anything else: pools, sections, whole files, certificates, `GET /config`, `/metrics`, reload, load, patch, upgrade, stop, users and roles | the `admin` role |

A forward target is free text, so a user with `modify` may write the address
of the admin API itself. Without `[admin] token` RXH refuses to connect
there (see [`[admin]`](#admin)): such a host answers `502` and gives nobody
the rights of a request from the machine.

What a signed-in user without the `admin` role gets from each route
(`401` when signed out, as on every route):

| Route | Answer |
|---|---|
| `GET /config/files` | `files` as `{"index", "revision"}` only (no path, no text); of the entities the servers with `view`, whole, and the pools as `{"kind": "upstream", "key": NAME}`; no section. |
| `POST /config/entities/server` | `201` with `create` on every group of the new host. `403` otherwise, for a host without groups, for a key that is not on the list below, and `403 {"error": "this name is reserved"}` for a name a `host` grant names (told only to who may create in those groups). |
| `PUT /config/entities/server/{key}` | `200` with `view` and `modify`, when the change keeps the name, adds only groups with `create`, leaves the host one the user may change and touches only keys of the list below. `403` otherwise. |
| `DELETE /config/entities/server/{key}` | `200` with `view` and `delete`, `403` with `view` only. |
| the three routes above, on a host without `view` or a key that names no host | `404 {"error": "no such host"}`, the same for both. |
| the three routes above, for `upstream` and `section` | `403`. |
| `PUT /config/files/{index}` | `403`. |
| `GET /status` | `version`, `uptime_seconds`, the `servers` with `view` and an empty `pools`. A listener shared with servers the user does not view is listed under the names of the viewed ones. |
| `GET /stats/history` | The samples with the series of the servers with `view` and an empty `pools`. |
| `GET /schema` | The schema, as for everybody. |
| every other route | `403 {"error": "this needs the admin role"}`. |

- Every server entity of `GET /config/files` has `permissions`, what the
  caller may do with it, in this order: `["view", "modify", "create",
  "delete"]` for an administrator. `create` is there when the user may
  create in one of the groups of the host.
- The entity keys (`FILE:POSITION`) and the revision of each file are the
  ones an administrator gets. They tell that the file has other tables and
  that something changed, and nothing else: a save with another revision
  than the one of the file is `409 {"error": "the configuration changed,
  read it again"}`, whatever the key.
- The grants are checked against the `name` and the `groups` the table has
  in the file, and for a change against the ones it would have, secrets put
  back: the form and the TOML text are checked the same. A `name` or a
  group written as a placeholder is not the name or the group it expands
  to, so no grant reaches it.
- A validation error of a save is passed on as it is: a clash with the name
  or the address of a host the user does not view shows there.
- `GET /status` and `GET /stats/history` go by the `name` and the `groups`
  of the running configuration, hosts made by a provider (Docker, Consul,
  Kubernetes...) included; those take their groups from the `rxh.groups`
  option (Kubernetes hosts have none) and no editing route reaches them. A series is shown only when every running
  server that counts under it is viewed: two servers with the same name, or
  the requests that match no server of a shared listener, are shown to who
  views them all. A series is kept under its name for an hour, whatever
  happens to the server: after a server is removed or renamed its series is
  for administrators, and a new server that takes the name shows the counts
  the name had before to who views the new one.
- A user with `modify` or `create` can point a host at any address the
  daemon can reach: give them to people you trust.

A host table can make the daemon read files, run programs, call other
services and open sockets, so a user without the `admin` role sets only the
keys that route or answer traffic for the host. Any other key, and any key
this list does not have, keeps the value it has in the file (it may be
there, set by an administrator, next to a change) and is absent from a new
host; a change to one is `403 {"error": "this needs the admin role:
match[0].forward.health.hooks"}`, with the path of the key. The keys of a
`[[server.match]]` and of a `subroute` entry are the ones of the server.

| Keys | A user may set them |
|---|---|
| `name`, `groups` | As the grants say, see the table above. |
| `listen` | Only addresses a server of the running configuration listens on, port `0` excluded: no new socket. |
| `host` | Yes. A new host must have one, and a host that has one keeps one; blank names do not count. |
| `http_host`, `path`, `uri`, `method`, `query`, `header`, `regexp`, `regexp_count`, `remote_ip`, `sni`, `alpn` | Yes: which requests the host takes. |
| `ip_allow`, `ip_deny`, `rate_limit` | Yes: who reaches the host. |
| `forward` (a pool name, addresses, weighted pools) | Yes, except a `unix//` socket. |
| `redirect`, `rewrite`, `request`, `response`, `cors`, `encode`, `security_headers` | Yes, every key they have today. |
| `cache`: `enabled`, `ttl_seconds` | Yes. `max_entries`, `max_body_bytes`: no, the memory of the daemon. |
| `handle_response`: `status`, `header`, `redirect`, `replace_status`, `rewrite`, `copy_response` | Yes. `serve`: no. |
| `tls = "auto"` | Yes, see below. `tls = { cert, key }`: no, paths. |
| inline `forward` pool: `backends`, `policy`, `choose`, `key`, `hash_type`, `hash_balance_factor`, `retries`, `try_duration`, `try_interval`, `retry_budget`, `retry_match`, `hedge_delay`, `dial_timeout`, `read_timeout`, `write_timeout`, `response_header_timeout`, `queue_timeout`, `flush_interval`, `h2c`, `grpc_web`, `slow_start`, `passive`, `circuit_breaker`, `fallback` (`policy`, `choose`, `key`) | Yes: where the requests of the host go, and how. |
| inline `forward` pool, `health`: `interval`, `timeout`, `uri`, `method`, `port`, `headers`, `request_body`, `status`, `body`, `passes`, `fails`, `grpc`, `grpc_service` | Yes: checks of the forward targets. |
| inline `forward` pool, `tls`: `server_name`, `insecure_skip_verify` | Yes. `ca`, `client_cert`, `client_key`: no, paths. |
| `serve`, `error_pages` | No: paths on the machine of the daemon. |
| `forward.health.hooks`, `forward.health.upstream`, `forward.health.follow_redirects`, `forward.dynamic`, `forward_auth`, `mirror` | No: programs, and requests to somewhere else than the forward targets. |
| `basic_auth`, `forward.secret`, `forward.fallback.secret` | No: credentials. |
| `trusted_proxies`, `client_auth`, `proxy_protocol`, `proxy_protocol_in` | No: who is trusted. |
| `protocol`, `http2`, `http3`, `https_redirect`, `tls_options`, `max_connections`, `max_connections_per_ip`, `match_timeout`, `idle_timeout`, `read_header_timeout`, `stream_timeout`, `stream_close_delay`, `udp_idle_timeout` | No: sockets, and what every server of a shared listener gets. |
| inline `forward` pool: `keepalive`, `keepalive_idle_conns_per_host`, `http2_conns_per_host`, `max_conns_per_host`, `max_connections`, `queue_size`, `request_buffers`, `response_buffers` | No: memory and connections of the daemon. |

- No string a user sets may have an `{env.NAME}` or a `{file.PATH}`
  placeholder: `403`. One an administrator wrote stays as it is.
- A host whose table has such a placeholder in any string keeps its forward
  targets: `forward`, the `backends` and their addresses, and the pools of
  a weighted `forward`, at every depth, new `match` entries included. A
  user who pointed the host at a backend of its own would receive the value
  there (a key in a request header, for example). The answer is `403
  {"error": "this host carries values only an administrator may redirect:
  forward"}`; the other keys of the list still change.
- `tls = "auto"` set by a user does more than answer: the daemon orders
  certificates for the names of `host` the user chose, and, unless
  `https_redirect` (a key for administrators) says no, listens on
  `[acme] http_port` of the same address to redirect to HTTPS. Give
  `create` and `modify` on hosts of a TLS listener with that in mind.
- The list has the keys under the names the reference gives them. An alias
  (`connections` for `max_connections`) and a key the daemon does not know
  are refused like a key for administrators, and so is `protocol`, also as
  the matcher of a `match` entry of a `tcp` server.
- The entries of an array (`match`, `handle_response`, backends) are
  compared by position: an entry with a key for administrators cannot be
  moved or removed by a user, nor can an entry before it.
- An administrator (the token, a request from the machine without a token,
  a user with `admin`) is not limited by any of this.

```sh
API=https://rxh.example.com:2019
ORIGIN="Origin: $API"
curl -c jar -d '{"name": "luis", "password": "luis-password"}' $API/auth/login
curl -b jar $API/config/files      # the hosts of luis, each with "permissions"
curl -b jar $API/status            # their listeners
curl -b jar -H "$ORIGIN" -d '{"value": {"listen": "0.0.0.0:8080", "host": "news.example.com", "groups": ["web"], "forward": "@api"}}' \
  $API/config/entities/server      # 201 {"key": "0:4"}
curl -b jar -H "$ORIGIN" -X DELETE "$API/config/entities/server/0:4?revision=$REVISION"
curl -b jar -H "$ORIGIN" -X PUT -d '{"value": {}, "revision": "..."}' $API/config/entities/upstream/api   # 403
```

```json
{
  "kind": "server",
  "key": "0:0",
  "file": 0,
  "value": { "name": "shop", "listen": "0.0.0.0:8080", "groups": ["web"], "forward": "@api" },
  "toml": "name = \"shop\"\nlisten = \"0.0.0.0:8080\"\ngroups = [\"web\"]\nforward = \"@api\"\n",
  "permissions": ["view", "modify", "create", "delete"]
}
```

#### Accounts and permissions, step by step

How the pieces above fit together, from a fresh install to a user who
manages its own hosts and nothing else.

1. **HTTPS on the admin listener.** A password must not cross the network
   in clear text, so people who sign in from other machines need `tls`.
   Any certificate the browsers of those people trust will do; for a
   test, a self-signed one:

   ```sh
   openssl req -x509 -newkey rsa:2048 -nodes -days 365 -subj /CN=rxh.example.com \
     -addext subjectAltName=DNS:rxh.example.com \
     -keyout /etc/rxh/admin.key -out /etc/rxh/admin.pem
   ```

2. **The configuration.** Without a `token`, the machine itself stays an
   administrator (the CLI, `curl` on the machine) and everybody else signs
   in. Put the users file where the daemon can write: next to the
   configuration by default, which is read-only under the shipped systemd
   unit and in the container images (see
   [Running under systemd](#running-under-systemd) and
   [Running in a container](#running-in-a-container)). Give the hosts
   groups.

   ```toml
   [admin]
   listen = "0.0.0.0:2019"
   tls = { cert = "/etc/rxh/admin.pem", key = "/etc/rxh/admin.key" }
   users = "/var/lib/rxh/users.toml"

   [[server]]
   name = "shop"
   listen = "0.0.0.0:443"
   host = "shop.example.com"
   tls = "auto"
   groups = ["web"]
   forward = "127.0.0.1:8080"
   ```

3. **The first administrator.** At start, while the users file has no
   user, the daemon writes a setup code to its log. Open
   `https://rxh.example.com:2019/dashboard`: from another machine the page
   asks for a name, a password and that code; from the machine itself it
   needs no code. Or, on the machine:

   ```sh
   curl -k -d '{"name": "ana", "password": "correct horse"}' https://127.0.0.1:2019/auth/setup
   rxh -c /etc/rxh/rxh.toml user add ana     # or this, with the daemon running or not
   ```

4. **Roles and users.** In the Users tab (or with `POST /roles` and
   `POST /users`), make a role with grants on groups or on single hosts,
   then a user with that role. The role `web-editors` with View, Modify
   and Create on the group `web`, and the user `luis` with it, give luis
   the hosts of `web` in Overview and Hosts, "New host" in `web`, and
   nothing else.

5. **Every day.** People sign in at `/dashboard`; a session lasts 12
   hours. `rxh status`, `rxh reload` and the other commands keep working
   on the machine over plain HTTP on the same port. `rxh user passwd`
   is the way back in when a password is lost, and ends the sessions of
   that user.

What is not there:

- **OIDC.** Sign-in through an identity provider (`[admin.oidc]`,
  `public_url`) is the next step and does not exist yet: `oidc` in
  `GET /auth/session` is always `false`. Until then, accounts are local
  passwords only. No two-factor codes, no passkeys, no password reset by
  e-mail: an administrator or `rxh user passwd` sets a new one.
- **Sessions that outlive the daemon.** Sessions are kept in memory: a
  restart, and a hot upgrade (`rxh upgrade`), sign everybody out. A
  reload does not.
- **ACME or client certificates on the admin listener.** `tls` in
  `[admin]` takes files only (`"auto"` is refused); renew them with
  your own tool and `rxh cert reload`.
- **Live changes of `[admin]`.** `listen`, `token`, `tls` and `users`
  are read at start; a reload keeps the ones the daemon started with
  (it says so for `listen` and `token`, and says nothing for `tls` and
  `users`). Restart to apply them.
- **Permissions on anything but hosts.** Pools, certificates, the other
  sections, whole files, reload and stop are for administrators.
- **Groups on Kubernetes hosts.** Docker, Podman, Swarm, Consul, Nomad
  and etcd hosts take `rxh.groups`; Kubernetes Ingresses and routes carry
  no such option, so their hosts are for administrators.

Known limits:

- **One writer at a time on the users file.** The daemon and `rxh user`
  do not lock the file against each other: when both write at the same
  moment, the last write wins and the other change is lost. A change
  made by one is seen by the other at its next read (the daemon rereads
  the file when its modification time changes).
- **A local firewall redirect to the admin port is not seen.** RXH
  refuses to proxy to its own admin API while no token is set, by the
  address and port it connects to. A `nftables` or `iptables` rule of the
  machine that redirects another port to the admin port hides that: set
  a `token` on a machine with such rules.
- **Modify is trust over the routing of a host.** A user with `modify`
  may change the matchers around an entry an administrator protected (a
  `match` entry with `serve` or `basic_auth`, for example: its `path` or
  `host`), so that it matches nothing, and append an entry without
  protection that takes its requests. The keys for administrators stay as
  they are, but which requests reach them is the user's to change.
- **Literal secrets in a host are visible to View.** A header value
  written as it is in `request.headers` (an API key for the backend, for
  example) is shown to every user with `view` on the host, in the form
  and the TOML. Put such values in an `{env.NAME}` or `{file.PATH}`
  placeholder: the user sees the placeholder, not the value, and cannot
  point the host elsewhere.
- **The form does not know the allow list.** Without the `admin` role
  the host form still shows every field; a change to a key for
  administrators is refused by the daemon when saving, `403` with the
  path of the key, shown next to the form.
- **The sign-in limits count per address.** Clients behind one address
  (a NAT, a proxy on the machine) share them, and an IPv6 network gives
  one client many addresses.
- **The page reads the session when it opens.** A role given or taken
  away shows in the page at its next load; the daemon applies it at the
  next request.

### Dashboard

`GET /dashboard` on the admin API is a web page that shows what the daemon
does and changes its configuration. It is embedded in the binary, loads
nothing from other sites, and has no key: it is there whenever the admin API
is. `/dashboard/` redirects to `/dashboard`.

```toml
[admin]
listen = "127.0.0.1:2019"   # then open http://127.0.0.1:2019/dashboard
```

Six tabs (Users only for an administrator, see [Accounts](#accounts)):

| Tab | Shows | Changes |
|---|---|---|
| Overview | Requests per second, share of `5xx`, latency p95, requests in flight and healthy backends; a chart of requests per second by status class and one of latency p50, p95 and p99, over 5 minutes, 15 minutes or 1 hour; one row per pool and per server with a trend; the certificates that expire within 30 days. | Nothing. |
| Hosts | Every `[[server]]` of the files: name, host names, listen addresses, protocol, target, TLS, state of its listeners and the file it is in. A server with several `listen` addresses is one row. | New host, Edit, Delete. |
| Pools | Every pool with its backends: health, state, weight, requests in flight, total requests, failures. | New pool, Edit (backends are part of the pool form), and Ready, Drain, Maintenance on each backend. |
| Certificates | The certificates served, soonest expiry first, with listener, source, issuer and OCSP staple. | "Reload certificates" (`POST /certificates/reload`), and the `[acme]` form. |
| Settings | The other sections (`top` level keys, `[admin]`, `[log]`, `[tracing]`, `[docker]`, `[consul]`, `[etcd]`, `[nomad]`, `[kubernetes]`), set or not, and every configuration file as text. | A form per section ("Set up" for one that is not in the file), "Save and reload" of a file edited as text, and the daemon controls: Reload, Upgrade, Stop. |
| Users | The users of `[admin] users` with their roles, and the roles with their grants; `admin` is built in. | New user, the roles of a user (ticked, then Save), Set password, Remove; New role, Edit (a row per grant: group or host, its name chosen from the groups and host names of the servers or typed, View, Modify, Create, Delete) and Remove. |

- Every key of the configuration has a field, drawn from the JSON schema
  (`GET /schema`), with the description of the key as help. The common keys
  of a host and of a pool come first, the others are folded by topic. A
  "Form | TOML" switch at the top of every editor shows the same table as
  text; the two views are not converted into each other, switching starts
  again from the file.
- "Save and reload" writes the file and reloads, see
  [Editing from the dashboard](#editing-from-the-dashboard). An error of the
  daemon is shown above the form, which keeps what was typed.
- When the file changed on disk since the page read it, the save is refused
  (`409`) and the editor offers "Read again", which replaces what is typed
  with what the file holds now. For a file edited as text in Settings,
  "Read again" keeps the typed text and shows next to it what the file
  holds now.
- Hosts and pools that are in no file are shown read only with the badge
  "runtime": those made by Docker, Consul, etcd, Nomad or Kubernetes, the
  inline pools of a `forward`, and servers changed through `PATCH`.
- Ready, Drain and Maintenance are the runtime states of
  [Draining backends and maintenance](#draining-backends-and-maintenance):
  they last "until restart" and do not change the file.
- In Users, ticking Modify, Create or Delete on a grant ticks View, which
  the others need; Create cannot be ticked on a host grant. A refusal of the
  daemon (a name that is not valid, the last administrator, a role some
  user has) is shown next to its row or form, which keeps what was typed.
  Remove asks first, and so does an administrator taking `admin` from
  itself: either one takes the Users tab away at once.
- Upgrade and Stop ask to type the word `upgrade` or `stop`. After Upgrade
  the page keeps working with the new process, whose history starts empty.
  After Stop the page says the daemon was stopped and asks nothing more.
- Without a configuration file (the quick commands) nothing can be saved:
  the tabs that edit (Hosts, Pools, Certificates and Settings) read "No
  configuration file: read only". The Overview tab changes nothing and
  shows no notice.
- Light or dark follows the browser (`prefers-color-scheme`). Every control
  has a label and works from the keyboard: the arrow keys move between the
  tabs, `Tab` between the controls. Focus is not moved into an editor when
  it opens, nor back to the list when it closes.

On a remote host, tunnel the admin address
(`ssh -L 2019:127.0.0.1:2019 host`) and open the same URL. With `token`
set, the page shows its sign-in form; "Use the admin token" there asks for
the token once per browser tab, keeps it in `sessionStorage` (gone when the
tab is closed) and sends it on its requests; a wrong token is asked again.
Without `tls` the admin API is plain HTTP: the token and everything the
page saves cross the network as sent, so keep the tunnel. A browser cannot
open a unix socket: forward it to a local port
(`ssh -L 2019:/run/rxh/admin.sock host`).

The page and [accounts](#accounts):

- It reads `GET /auth/session` when it opens. A caller that is nobody (no
  session, and no token or request from the machine that makes it an
  administrator) gets the sign-in form and nothing else: name and
  password, or the token where `password_login` is `false`. The password
  is sent to `POST /auth/login` and kept nowhere; signing in with a
  password drops a token kept for the tab, which would outrank the session.
- While no user exists (`setup`), a caller that may create the first one
  gets the form for it, with a "Setup code" field when `setup_needs_code`
  says so. An administrator (the machine without a token, or the token)
  can answer "Not now": the choice is kept in the `localStorage` of the
  browser and the API works without accounts as before.
- A signed-in user has its name, "Change password" (`PUT /auth/password`)
  and "Sign out" in the header. A caller that is an administrator without
  being a user, as a browser of the machine is by default, has none of
  them and sees no form.
- Without the `admin` role the page has the Overview and Hosts tabs. The
  overview shows the servers the user views, without pools and
  certificates. Hosts lists them with their groups, and follows the
  `permissions` of each: "Edit" with `modify`, "View" (a form that takes
  no input, without the TOML view) otherwise, "Delete" with `delete`, and
  "New host" for a user with a group to create in, whose form starts with
  the first of its `creatable_groups` in `groups`. The paths of the files
  are not shown. What the daemon refuses, a key the user may not set for
  example, is shown next to the form with the path of the key, and the
  form keeps what was typed.
- A session ends after 12 hours, or when the password of the user changes:
  the next call is answered `401` and the sign-in form comes over the
  page. An open form stays behind it with what was typed, to be saved
  after signing in. Until then the page asks the daemon nothing. When
  somebody else signs in, or the token is given, the tabs start again:
  nothing the user before had on screen is kept, an open form included.

Differences from the design of the page
([`docs/superpowers/specs/2026-11-20-webui-design.md`](superpowers/specs/2026-11-20-webui-design.md)):
a value the form cannot draw gets a JSON box for that key, not a TOML one
(no key of the schema needs it today); the Overview reads `/stats/history`
and `/certificates` and not `/status`; the statistics are sampled by a
thread, not a task, so they outlive the runtimes of the admin API.

#### Statistics history

The daemon takes one sample of its counters every 5 seconds and keeps the
last 720, one hour, in memory. The history survives reloads and is lost on
restart and on a binary upgrade. It is always on when the admin API is.

```sh
curl -s localhost:2019/stats/history | jq '.samples | length'
# Only the samples taken after a time (unix seconds): what the page asks
# every 5 seconds, with the time of the newest sample it has.
curl -s "localhost:2019/stats/history?since=$(($(date +%s) - 60))" | jq '.samples | length'
```

`GET /stats/history` answers `200` with `interval_seconds` (5), `buckets`
(the bounds of the duration histogram, in seconds) and `samples`, oldest
first. A sample has `time` (unix seconds), `servers` (per server: `status`,
the responses by class `1xx` to `5xx`; `duration_count`, `duration_sum`,
`duration_buckets`; `connections` of layer 4) and `pools` (per pool:
`requests`, `failures`, `responses`, `server_errors`, `response_seconds`,
and the gauges `active`, `healthy`, `backends`). Counters are cumulative:
the page draws the difference between two samples. With `?since=<unix
seconds>` only the samples with a later `time` are sent (`interval_seconds`
and `buckets` are always there); a `since` that is not a whole number is
`400`. The answer is JSON on one line.

- Latency percentiles are interpolated inside the histogram buckets, whose
  bounds are 5, 10, 25, 50, 100, 250 and 500 ms, 1, 2.5, 5 and 10 s. Requests
  faster than 5 ms all fall in the first bucket, so their percentiles show
  at most 5 ms whatever the real time (a proxy answering in 0.4 ms reads
  "p95 4.8 ms"); a percentile beyond 10 s shows 10 s. The latency of a pool
  row is a mean, from `response_seconds`, and has no such floor.
- "Errors" is the share of `5xx` answers; `4xx` are in the chart only.
- A server or pool that was removed keeps its row until its samples leave
  the range.
- For anything longer than an hour, or kept across restarts, scrape
  `GET /metrics` with Prometheus.

#### Building the page

The page is a Svelte application in `crates/server/ui`. Its compiled files
(`crates/server/ui/dist/index.html`, `app.js`, `app.css`) are committed and
embedded in the binary, so `cargo build`, `cargo install` and the packages
need neither Bun nor Node. Only a change of the page needs Bun:

```sh
cd crates/server/ui && bun install && bun run build   # then commit dist/
bun run check && bun run test                         # types and unit tests
```

The CI job `ui` runs the same commands and fails when the committed `dist/`
differs from a fresh build.

### Editing from the dashboard

The page saves through these routes, which `curl` can call too. They need
the token like every other route, and answer `{"error": "..."}` on failure.

| Method | Path | Body | Success | Failure |
|---|---|---|---|---|
| `GET` | `/schema` | | `200`: the JSON schema `rxh schema` prints | |
| `GET` | `/stats/history[?since=...]` | | `200`, see [Statistics history](#statistics-history) | `400` `since` is not a number |
| `GET` | `/config/files` | | `200`: `editable`, `files`, `entities` | `500` a file cannot be read or parsed (the answer gives the file, line and column, not its text) |
| `PUT` | `/config/files/{index}` | `{"text", "revision"}` | `200` `{"saved": true}` | `400` not valid TOML, not a valid configuration, reload refused; `404` no such file; `409` |
| `POST` | `/config/entities/{kind}` | `{"value"}` or `{"toml"}`, `"key"` (name of a pool or section), `"file"` (index, default 0) | `201` `{"key": "..."}` | `400` both or neither of `value` and `toml`, name missing or taken, not valid; `404` unknown kind or file; `409` |
| `PUT` | `/config/entities/{kind}/{key}` | `{"value"}` or `{"toml"}`, and `"revision"` | `200` `{"saved": true}` | `400` as above, or `revision` missing; `404` unknown kind, or `key` in no file; `409` |
| `DELETE` | `/config/entities/{kind}/{key}?revision=...` | | `200` `{"saved": true}` | `400` `revision` missing, or the configuration is not valid without it; `404`; `409` |

`409` is answered when `revision` is not the one of the file on disk ("the
file changed, read it again"), and by every write route when the daemon runs
without a configuration file. `500` means a file could not be written or
put back; the message names it. Bodies are JSON, limited to 4 MiB.

```sh
# The files: the main one first, then the included ones.
curl -s localhost:2019/config/files | jq '.files[] | {index, path, revision}'
curl -s localhost:2019/config/files | jq '.entities[] | {kind, key, file}'
curl -s localhost:2019/schema | jq '.properties | keys'

# Change one pool: the whole table as JSON, with the revision of its file.
rev=$(curl -s localhost:2019/config/files | jq -r '.files[0].revision')
curl -X PUT localhost:2019/config/entities/upstream/api \
  -d '{"value": {"backends": ["10.0.0.1:8080", "10.0.0.2:8080"], "policy": "least_conn"}, "revision": "'$rev'"}'

# The same as TOML: the body of the table, without its header.
curl -X PUT localhost:2019/config/entities/upstream/api \
  -d '{"toml": "backends = [\"10.0.0.1:8080\"]\npolicy = \"least_conn\"\n", "revision": "'$rev'"}'

# A new server in the main file, a new pool in the second file.
curl -X POST localhost:2019/config/entities/server \
  -d '{"value": {"name": "blog", "listen": "0.0.0.0:8081", "forward": "@api"}}'
curl -X POST localhost:2019/config/entities/upstream \
  -d '{"key": "blog", "value": {"backends": ["10.0.0.9:80"]}, "file": 1}'

# Delete the server at position 2 of file 0, then a section.
curl -X DELETE "localhost:2019/config/entities/server/0:2?revision=$rev"
curl -X DELETE "localhost:2019/config/entities/section/tracing?revision=$rev"

# Replace a whole file.
jq -n --rawfile text new.toml --arg revision "$rev" '{text: $text, revision: $revision}' |
  curl -X PUT localhost:2019/config/files/0 -d @-
```

- `GET /config/files` lists each file with `index`, `path`, `revision`
  (the SHA-256 of its text on disk) and `text`, and each table as an
  entity `{kind, key, file, value, toml}`: `value` is the table as JSON as
  written (placeholders not replaced, aliases kept), `toml` its text without
  the header. `editable` is `false`, with no file, when the daemon has no
  configuration file. Every write changes the revision: read it again
  before the next one.
- `kind` is `server`, `upstream` or `section`. The `key` of a pool is its
  name; of a section `top` (the keys outside any table), `admin`, `log`,
  `tracing`, `acme`, `docker`, `consul`, `etcd`, `nomad` or `kubernetes`;
  of a server `{file index}:{position in that file}`, as `GET
  /config/files` gives it. Positions shift when a server above is deleted:
  the revision check is what stops a delete from reaching another server.
  The section `top` cannot be deleted.
- A `value` is the whole table: a key with a new value is set, a key left
  out is removed, tables merge by the same rule, arrays are replaced.

What a save does, in order:

1. The revision is compared with the file on disk (`409`).
2. Only the table of the entity is edited; the rest of the file keeps its
   text, and inside the table the keys that did not change keep their
   comments and layout.
3. The file is written atomically (a temporary file next to it, synced to
   disk, then a rename), and the whole configuration is loaded from the main
   file as
   `rxh validate` does. If it is not valid the file is put back and the
   answer is `400` with the reason. A configuration file that is a symbolic
   link stays one: the file it points to is replaced, with the temporary
   file next to that file.
4. The daemon reloads. If the reload is refused (a port in use, a
   certificate that does not load), the file is put back and the answer is
   `400` with the reason.

A save that fails leaves every file byte for byte as it was. Two saves never
run at the same time.

Limits of the edit that keeps comments:

- An array of tables inside an entity that a `value` changes, such as the
  `[[server.match]]` routes of a server, is written back as one inline
  array (`match = [{ ... }, { ... }]`) and loses the comments inside it.
  Left unchanged, it keeps its text. To keep the layout, edit it as TOML.
- In text sent as `toml`, a comment after the last key is dropped; comments
  above and beside the keys are kept.
- A pool written with dotted keys (`upstream.api.backends = [...]`) becomes
  an `[upstream.api]` table when it is saved as `toml`.

Secrets:

- These keys are shown as `***` by `GET /config/files`, in `value`, `toml`
  and `text`, when they hold a literal: `token` of `[admin]`, `[consul]`,
  `[etcd]`, `[nomad]` and `[kubernetes]`; `api_token`, `secret_access_key`
  and `tsig_secret` of `[acme.dns]`; `secret` of a pool, of an inline pool
  of `forward`, and of their `fallback` chains.
- A `***` sent back keeps the value stored in the file; anything else
  replaces it. A `***` where the file stores nothing is refused (`400`).
- A value with a placeholder (`{env.NAME}`, `{file.PATH}`) names where the
  secret is: it is shown and saved as written, never replaced.
- Not masked: credentials inside header maps (`headers` of a health check,
  request headers, such as `Authorization = "Bearer ..."`) and the bcrypt
  hashes of `basic_auth`. Whoever can read the admin API reads them. Keep
  header credentials out of the file with a placeholder:
  `headers = { Authorization = "Bearer {file.secrets/health-token}" }`.
- When a whole file is saved (`PUT /config/files/{index}`), a `***` inside
  a `[[server]]` is restored only if that server is still the one stored at
  its position: same `name`, or same `listen` when it has no name. It is
  refused (`400`, "type its secret again instead of ***") when the server
  moved, or when its name (else its listen address) is not unique in the
  file.

File permissions:

- A save keeps the mode of the file. A file that did not exist is created
  with mode `0600`, because a configuration file can hold secrets. The
  owner and group are kept only when the process may `chown` (root);
  otherwise the saved file belongs to the user of the process.
- The same holds for the CLI commands that edit the files (`rxh upstream
  add`, `rxh pool add`, `rxh service add`, ...): a file that `rxh` creates
  as root is readable by root only, and a daemon running as another user
  cannot read it until it is given (`chown`, or `chmod 640` with its group).
- A save is a new file next to the configuration file, then a rename over
  it. So the user of the daemon must be able to write the directory of each
  configuration file (the directory of the file a symbolic link points to)
  and to read the file; the file itself does not need to be writable.
  Without that the save answers `500`, "the configuration file ... cannot
  be written, nothing was changed", with the error of the system, and
  nothing changes.
- The packaged service cannot save, from the page or from the admin API, as
  it is shipped. The `.deb`, `.rpm` and AUR packages install
  `/etc/rxh/rxh.toml` with mode `0644`, owned by root, and
  [`dist/rxh.service`](../dist/rxh.service) runs the daemon as a dynamic
  user (`DynamicUser=yes`) with `ConfigurationDirectory=rxh` and
  `ProtectSystem=strict`: the daemon reads its configuration and cannot
  write the directory, so the page shows everything and every save answers
  `500`. A setup that saves needs a fixed user for the service that owns
  the configuration directory, and a unit that lets it write there. The
  packages do not provide that yet: they create no such user, and no
  recipe for it has been tested. A daemon started by hand, or by a unit of
  your own, as a user that owns the directory of its configuration file
  saves without anything else.

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
  permanent, edit the file, use `rxh pool`, `rxh upstream` and
  `rxh service`, or save it through the routes of
  [Editing from the dashboard](#editing-from-the-dashboard).
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

### Draining backends and maintenance

A backend can be taken out of rotation without removing it, to deploy or
restart it, like HAProxy's `set server state`:

```bash
rxh upstream drain web 10.0.0.1:80   # no new requests; sticky sessions stay
rxh upstream maint web 10.0.0.1:80   # no requests at all (alias of maintenance)
rxh upstream ready web 10.0.0.1:80   # back into rotation (alias: enable)

# The same through the API; `state` is ready, drain or maintenance.
curl -X PUT localhost:2019/pools/web/upstreams/state \
  -d '{"address": "10.0.0.1:80", "state": "drain"}'
```

| State | New requests and connections | `cookie` sticky sessions | In flight |
|---|---|---|---|
| `ready` | yes | yes | finish |
| `drain` | no | yes | finish |
| `maintenance` | no | no, they move to another backend | finish |

- The CLI prints how many requests are still in flight; `rxh upstream list`
  shows `drain` or `maint` in the `STATE` column, and `GET /pools` has
  `state` on every backend. Wait for `ACTIVE` to reach 0 before stopping the
  backend.
- Hash policies (`ip_hash`, `uri_hash`, ...) send the keys of a backend out
  of rotation to the others, as when it is unhealthy. When no backend is
  left, the pool answers `503`.
- The state is runtime only: it is not written to the configuration file
  and is lost on restart, but a reload (`rxh reload`, `SIGHUP`, `/load`,
  `PATCH`, `watch`) keeps it for the backends that are still there. Health
  checks keep running in every state. With `slow_start`, a backend put back
  to `ready` ramps up again.
- The address is the one listed by `rxh upstream list`. Answers `404` for an
  unknown pool or backend and `400` for an unknown state.

## `[log]`

| Key | Default | Description |
|---|---|---|
| `format` | `"text"` | `"text"` keeps one human readable line per request. `"json"` writes one JSON object per line. |
| `output` | `"stdout"` | `"stdout"`, `"stderr"` or a file path (created if missing, always appended to). Only access lines go here; startup and error messages stay on stdout and stderr. |
| `access` | `true` | `false` disables access lines. |
| `status` | `[]` | HTTP statuses whose lines are always written: codes (`404`) or classes (`"5xx"`). |
| `min_duration` | none | Requests and layer 4 connections lasting at least this long are always written, like `"500ms"`. |
| `sample_ratio` | `1`, or `0` with `status` or `min_duration` | Share of the other lines that are written, from 0 to 1, drawn at random per line. |

### Filters and sampling

A line is written when it matches a filter (`status` or `min_duration`),
otherwise with the probability `sample_ratio`. Setting a filter alone keeps
only the matching lines, like Traefik's access log filters; adding
`sample_ratio` keeps a share of the rest. Without filters, `sample_ratio`
samples every line. Errors and slow requests in full, 1 % of the rest:

```toml
[log]
format = "json"
status = ["5xx", 429]
min_duration = "1s"
sample_ratio = 0.01
```

`status` only applies to HTTP requests; TCP connections and UDP sessions
are kept by `min_duration` or sampled. The text line written when a UDP
session opens is only sampled, as the session has not lasted yet.
Operational messages (failed dials, timeouts) are never filtered. Metrics
count every request whatever the log keeps.

JSON lines for HTTP requests have `ts` (RFC 3339 UTC with milliseconds),
`server` (server name, or its listen address), `remote`, `client_ip` (after
`trusted_proxies`), `method`, `uri`, `proto`, `status`, `duration_ms` (time
until the response headers were ready) and `upstream` (backend address, or
`null` for files, cache hits and errors) and `trace_id` (`null` without
[`[tracing]`](#tracing-opentelemetry)). TCP connections have `ts`,
`server`, `protocol`, `remote`, `upstream`, `bytes_sent` (client to backend),
`bytes_received`, `duration_ms`; UDP sessions are logged when they end, with
`protocol = "udp"`. Log files are opened when the configuration loads, so
reload after rotating them.

```json
{"ts":"2025-01-01T12:00:00.123Z","server":"web","remote":"203.0.113.5:51000","client_ip":"203.0.113.5","method":"GET","uri":"/api/users","proto":"HTTP/1.1","status":200,"duration_ms":3.4,"upstream":"10.0.0.1:8080","trace_id":null}
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
| `rxh_upstream_failures_total` | `pool`, `upstream` | counter, requests and connections without a response |
| `rxh_upstream_responses_total` | `pool`, `upstream` | counter, HTTP responses |
| `rxh_upstream_server_errors_total` | `pool`, `upstream` | counter, HTTP responses with a 5xx status |
| `rxh_upstream_response_seconds_total` | `pool`, `upstream` | counter, time to the response headers summed over the responses |
| `rxh_upstream_active_requests` | `pool`, `upstream` | gauge |
| `rxh_upstream_healthy` | `pool`, `upstream` | gauge, 1 or 0 |

`server` is the server name, or its listen address when unnamed (requests that
match no route use the listener name).

### `rxh top`

`rxh top` reads `/metrics` every second and shows the traffic of every pool
since the previous read:

```sh
$ rxh top --interval 2s
rxh top  admin 127.0.0.1:2019  every 2s
POOL                         REQ/S     ERR/S    ERR%    LATENCY  ACTIVE  HEALTHY
api                          121.8       0.5     0.4      3.2ms       4      2/2
db                            12.0       0.0     0.0          -       1      1/1
```

`REQ/S` counts requests and connections sent to the pool, `ERR/S`
requests and connections that failed without a response plus 5xx responses,
`ERR%` the share of errors among the requests that finished, `LATENCY` the
mean time to the response headers (HTTP pools only), `ACTIVE` requests in
flight and `HEALTHY` healthy backends out of all. On a terminal the table is
redrawn in place until Ctrl-C; piped, one table is appended per refresh.
`--count N` stops after `N` tables. It uses `--admin` and `--admin-token`
like the other commands.

## `[tracing]`: OpenTelemetry

Off unless the table is present. With it, every request of the `http`
servers becomes one span, sent to an OpenTelemetry collector (or Jaeger,
Tempo, any OTLP endpoint) over OTLP/HTTP in JSON.

```toml
[tracing]
endpoint = "http://127.0.0.1:4318/v1/traces"
service_name = "edge"
sample_ratio = 0.1
```

| Key | Default | Description |
|---|---|---|
| `endpoint` | required | `http://` or `https://` URL of the OTLP traces endpoint. The path is not added for you: collectors listen on `/v1/traces`. |
| `service_name` | `"rxh"` | `service.name` of the spans. |
| `sample_ratio` | `1.0` | Share of the traces that start at rxh that are sampled, from `0` to `1`. The decision comes from the trace ID, so services with the same ratio keep the same traces. |

Trace context follows the W3C `traceparent` header:

- A request with a valid `traceparent` continues that trace: the span of
  rxh is a child of the caller's span and keeps the caller's sampling
  decision, whatever `sample_ratio` says. `tracestate` is passed on
  untouched.
- A request without one, with a malformed one or with several starts a new
  trace, and its `tracestate` is dropped.
- Backends, `forward_auth` services and mirrors receive a `traceparent`
  that names the span of rxh as their parent, also for traces that are not
  sampled (flags `00`).

Without `[tracing]`, `traceparent` and `tracestate` reach the backends as
the client sent them.

Each span is a server span named after the method and the `uri` of the
`match` entry (`GET /api`), from the arrival of the request until the
response headers are ready, with the attributes `http.request.method`,
`url.scheme`, `url.path`, `url.query`, `network.protocol.version`,
`server.address` and `server.port` (the `Host`), `client.address` (after
`trusted_proxies`), `http.response.status_code`, `http.route`, `rxh.server`
and `rxh.upstream`.
Responses with a 5xx status mark the span as an error. JSON access lines
carry the `trace_id`.

Spans are sent in batches one second after the first of the batch
finished. A collector that does not answer with a 2xx status within 10
seconds loses the batch; this is logged once until it answers again, and
requests are never held back. At most 2048 spans wait per listener, more
are dropped, and the spans of the last second before rxh stops are not
sent. OTLP over gRPC, request headers for the collector, spans for layer 4
connections and separate client spans per backend attempt are not
implemented.

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
| `rxh.groups` | `groups` of the server, comma separated: the admin API users whose roles grant View on one of them see it in `rxh status` and the dashboard (see [Permissions](#permissions)). Each name matches `[a-z0-9][a-z0-9_-]*`, 64 bytes at most, a repeat counted once. The groups of a pool are those of all its replicas: any container that joins the pool adds its groups to the server. The server stays read only for them: it is not in a file. |

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
      rxh.groups: web, customers
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
groups, address or policy the listeners are swapped as a reload does, without
dropping connections.

- rxh must reach the container addresses: run it on the Docker host or in a
  container on the same Docker network. A container on several networks is
  reached at its address on the first one by name; one with
  `network_mode: host` at `127.0.0.1`.
- A container is HTTP or TCP: `rxh.http.host` and `rxh.tcp.listen` cannot be
  combined. The containers of a pool must agree on `rxh.policy` and
  `rxh.tcp.listen`; their hosts and groups are merged.
- A container whose labels are wrong (unknown `rxh.*` label, missing port,
  invalid host, policy or group name), whose pool has the name of an `[upstream.NAME]`
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
`rxh.tcp.port`, `rxh.pool`, `rxh.policy`, `rxh.groups`): a tag
`rxh.groups=web,customers` for Consul and Nomad, a key
`PREFIX/SERVICE/INSTANCE/groups` for etcd. An instance is reached at its
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
Ingresses and HTTPRoutes carry no `rxh.*` options, so their servers have no
`groups` and only administrators see them in the admin API.

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
| `policy` | `"weighted_round_robin"` | `lb_policy` | Selection policy, see below. Alias: `lb_policy`. The old `algorithm` is deprecated, see [migration](migration.md). |
| `backends` | `[]` | upstreams | List of addresses or `{ address = "...", weight = N }`. Address forms: `host:port`, `http://host[:port]` (default port 80), `https://host[:port]` (TLS, default port 443) and `unix//absolute/path.sock` (unix socket, not for UDP). Port ranges `"host:8001-8004"` expand to several backends. Host names are resolved on each dial. A pool cannot mix `https://` and plain backends. Alias: `to`. |
| `choose` | `2` | `random_choose <n>` | Sample size of `random_choose`. |
| `key` | | `query [key]`, `header [field]`, `cookie [name]` | Query parameter, header or cookie name. Required for `query` and `header`; `cookie` defaults to `lb`. |
| `secret` | `""` | `cookie name <secret>` | HMAC secret of the `cookie` policy. |
| `fallback` | `"random"` | `fallback` | Policy used when the key of `query`, `header`, `uri_hash` or `cookie` is missing. A policy name, or a table `{ policy, key, choose, secret, fallback }` whose own `fallback` nests another one, for example `fallback = { policy = "header", key = "x-user", fallback = "round_robin" }`. A `query`, `header` or `cookie` fallback uses its own `key`. Cookie values use the secret of the first `cookie` policy in the chain. |
| `retry_match` | unset | `lb_retry_match` | `{ method = [...], status = [...] }`. `method`: methods whose bodyless requests are retried on another backend (default `GET`, `HEAD`, `OPTIONS`). `status`: response statuses (`"503"`, `"5xx"`) that make the proxy drop the response and retry another backend, only for requests retryable under `method`. Retries obey `retries` and `try_duration`; when they run out, the last response is returned. |
| `retries` | `0` | `lb_retries` | Extra attempts after a failure. |
| `try_duration` | unset | `lb_try_duration` | Keep retrying for this long. Wins over `retries`. Round-trip and `retry_match.status` retries wait `try_interval` first and are skipped when that wait would pass this limit, counted from the first attempt. |
| `try_interval` | `"250ms"` | `lb_try_interval` | Wait between attempts. |
| `retry_budget` | unset | Envoy `retry_budget` | `{ percent = 20, min_retries = 3 }`: bound on the requests of the pool that are retrying or hedged at the same time, see [Retry budgets and hedging](#retry-budgets-and-hedging). |
| `hedge_delay` | unset | Envoy `hedge_policy` | A retryable HTTP request without response headers after this long is also sent to a second backend; the first response wins, see [Retry budgets and hedging](#retry-budgets-and-hedging). `0` is rejected. |
| `dial_timeout` | `"3s"` | `dial_timeout` | Connect timeout. |
| `response_header_timeout` | unset | `response_header_timeout` | Time from sending the request to receiving the response headers. On expiry the client gets `504 Gateway Timeout` and the backend counts a passive failure. |
| `read_timeout` | unset | `read_timeout` | An upstream read that makes no progress for this long fails the connection (also idle pooled connections and upgraded tunnels). |
| `write_timeout` | unset | `write_timeout` | An upstream write that makes no progress for this long fails the connection. |
| `h2c` | `false` | `h2c://` | Speak cleartext HTTP/2 to the backends, over `http2_conns_per_host` shared connections each. |
| `grpc_web` | `false` | Envoy `grpc_web` | Translate gRPC-Web requests to gRPC and the responses back, see [gRPC-Web](#grpc-web). Needs `h2c`. |
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
| `hash_type` | `"rendezvous"` | HAProxy `hash-type`, Envoy `MAGLEV` | How the hash policies map a key to a backend: `"rendezvous"` or `"maglev"`, see [Consistent hashing](#consistent-hashing). |
| `hash_balance_factor` | unset | HAProxy `hash-balance-factor` | Bounded loads for the hash policies: the most a backend may carry, in percent of the average load, see [Consistent hashing](#consistent-hashing). At least `100`. |
| `slow_start` | unset | Nginx Plus `slow_start` | Duration over which a backend that joins the pool at runtime or comes back healthy ramps up to its full share of the traffic, see [Slow start](#slow-start). `0` is rejected. |
| `max_connections` | `0` | HAProxy `maxconn` | Most requests in flight (connections on `tcp` servers, sessions on `udp`) on each backend, see [Connection limits](#connection-limits). `0` means no limit. |
| `queue_size` | `0` | HAProxy `maxqueue` | Requests that may wait for a backend when all are at `max_connections`. `0` answers `503` at once. Needs `max_connections`. |
| `queue_timeout` | `"5s"` | HAProxy `timeout queue` | Longest wait in the queue before `503`. |

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

#### gRPC-Web

Browsers cannot read HTTP trailers, where gRPC puts the status of a call,
so they speak gRPC-Web instead. With `grpc_web = true` a request with the
content type `application/grpc-web` or `application/grpc-web-text` (the
same in base64), with or without a suffix like `+proto`, reaches the
backends as `application/grpc` with `te: trailers`. The response gets the
content type the client used, and its trailers are appended to the body as
a frame flagged `0x80`. Clients may use HTTP/1.1 or HTTP/2. Unary and server
streaming calls work; each chunk of a `-text` response is padded base64 of
its own. Other requests of the pool, native gRPC included, are not touched.
A `-text` body that is not base64 gets `502`. Browsers on another origin
also need [`cors`](#cors) on the route, exposing `grpc-status` and
`grpc-message`.

```toml
[upstream.grpc]
backends = ["10.0.0.1:50051", "10.0.0.2:50051"]
h2c = true
grpc_web = true
health = { grpc = true }
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

Checks run in the background for every backend. With `grpc` the check is a
call of the gRPC health checking protocol; with `uri` an HTTP request;
without either, a TCP connect.

| Key | Default | Description |
|---|---|---|
| `uri` | | Path and query to request. |
| `grpc` | `false` | Call `grpc.health.v1.Health/Check` over HTTP/2 (TLS when the pool uses it, which then needs `h2c = true`). The backend passes while it answers `SERVING`; `NOT_SERVING`, an error status such as `UNIMPLEMENTED` or no gRPC answer fail it. `headers` are sent as metadata. Cannot be combined with `uri` or `body`. |
| `grpc_service` | | Service name of gRPC checks. Unset asks for the server as a whole. |
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

### Consistent hashing

The hash policies (`ip_hash`, `client_ip_hash`, `uri_hash`, `query` and
`header`) are consistent: a key stays on its backend while that backend is
available, and adding or removing a backend moves only a small share of the
keys. Two settings of the pool tune them, at every level of a `fallback`
chain. Hash policies ignore `weight`.

```toml
[upstream.cache]
policy = "uri_hash"
backends = ["10.0.0.1:8080", "10.0.0.2:8080", "10.0.0.3:8080"]
hash_type = "maglev"
hash_balance_factor = 125
```

`hash_type` chooses how a key finds its backend:

- `"rendezvous"` (default): highest random weight hashing, computed over
  the backends on each request. Removing a backend moves its keys only.
- `"maglev"`: a lookup table of 65537 entries (Google's Maglev, the default
  size of Envoy), built when the members of the pool change, where every
  backend owns an almost equal share. A pick costs the same with any number
  of backends; a membership change can move a few keys between the other
  backends too.

`hash_balance_factor` adds bounded loads (HAProxy `hash-balance-factor`,
Envoy `hash_balance_factor`), with either hash type. It is the most a
backend may carry, in percent of the average number of requests (HTTP) or
connections (TCP/UDP) in flight per available backend: with `125`, no
backend takes more than 1.25 times the average. A key whose backend is at
the bound goes to the next backend of that key, always the same one, and
comes back when the load drops. This keeps a hot key from overloading one
backend, at the price of some stickiness. `100` spreads the load evenly;
values below are rejected. Without it, a key never leaves an available
backend.

With both hash types, the keys of a backend that is unavailable or at the
bound go to the next backend in rendezvous order.


### Connection limits

`max_connections` caps the requests in flight on each backend of a pool
(HAProxy `maxconn`): connections on `tcp` servers, sessions on `udp`. A
backend at its limit is skipped by every policy. When all the usable backends
are at their limit, a request gets `503` (a `tcp` connection is closed),
unless the pool has a queue:

```toml
[upstream.api]
backends = ["10.0.0.1:8080", "10.0.0.2:8080"]
max_connections = 50     # per backend
queue_size = 200         # requests waiting, for the whole pool
queue_timeout = "2s"     # default "5s"
```

Up to `queue_size` requests then wait for a backend to finish a request, and
get `503` after `queue_timeout`. Requests over `queue_size` are refused at
once, and nothing waits for backends that are down, draining or in
maintenance. `GET /pools` shows the requests waiting as `queued`.

The limit is one value for all the backends of the pool and is counted per
process. It is different from `max_conns_per_host`, which bounds the
HTTP/1.1 connections of the transport and makes requests wait for that same
backend without a limit or a timeout. A request with a `cookie` session
whose backend is full goes to another backend. `udp` datagrams of new
sessions over the limit are dropped, never queued.

### Retry budgets and hedging

`retries` and `try_duration` bound the attempts of one request. When many
requests fail at once, each of them retrying multiplies the load on backends
that are already in trouble. `retry_budget` bounds the retries of the whole
pool instead (Envoy `retry_budget`):

```toml
[upstream.api]
backends = ["10.0.0.1:8080", "10.0.0.2:8080", "10.0.0.3:8080"]
retries = 2
retry_match = { status = ["502", "503"] }
retry_budget = { percent = 20, min_retries = 3 }
hedge_delay = "100ms"
```

| Key | Default | Envoy | Meaning |
|---|---|---|---|
| `percent` | `20` | `budget_percent` | Most requests retrying at the same time, in percent of the requests in flight on the pool. `0` to `100`, fractions allowed. |
| `min_retries` | `3` | `min_retry_concurrency` | Requests that may always retry at the same time, so a pool with little traffic can still retry. |

A request counts against the budget from its first retry (after a refused
connection, a failed round trip or a `retry_match.status` response) or its
hedge, until its response is fully sent. A request that wants to retry while
the budget is used up is not retried: the client gets the answer of the
attempt that failed (`502`, or the upstream response for `retry_match.status`).
`retry_budget = {}` uses the defaults. On `tcp` servers a retried connection
counts until it closes. Without `retry_budget` nothing is counted. The budget
is counted per process.

`hedge_delay` cuts the tail latency (Envoy `hedge_policy`, "hedged
requests"): when a backend has not sent its response headers after the
delay, the same request also goes to a second backend. The first response
wins and the other request is cancelled. A good value is the 95th or 99th
percentile of the latency of the backends, so that few requests are sent
twice.

- Only requests that may be retried are hedged: no body, and a method of
  `retry_match.method` (`GET`, `HEAD` and `OPTIONS` by default). Upgrades
  (WebSocket) are not.
- A request is hedged once, on its first attempt, and only on a backend
  other than the first one. `round_robin`, `least_conn` and the random
  policies pick another; hash policies and `cookie` sessions keep picking
  the same backend, so their requests are not hedged. A hedge does not wait
  in the `max_connections` queue.
- A hedge counts against `retry_budget` like a retry. Setting both is
  recommended: without a budget, slow backends double the requests.
- When the first attempt fails and the hedge answers, the client gets the
  hedge's response, without `retries`. When neither answers, the error of
  the first attempt is handled as usual (`retries` apply).
- The cancelled request is not reported to passive health checks or the
  circuit breaker. `response_header_timeout` applies to each of the two.

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
- has its circuit breaker closed by a good trial request;
- is put back to `ready` after `rxh upstream drain` or `maintenance`.

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
| `max_connections` | `1024` | Concurrent connections (or UDP sessions). Extra clients wait in the backlog; a QUIC connection over the limit is refused, see [HTTP/3](#http3). The old `connections` is deprecated, see [migration](migration.md). |
| `tls` | | `{ cert = "cert.pem", key = "key.pem" }`: TLS termination for `http` (with h2 via ALPN) and `tcp`. `"auto"` (http only): certificates from an ACME CA for every name in `host`, see [Automatic HTTPS](#acme-automatic-https). Servers sharing an address pick their certificate by SNI. |
| `groups` | | Groups of this host, like `["web", "customers"]`; admin API roles grant permissions on groups. Each name matches `[a-z0-9][a-z0-9_-]*`, at most 64 bytes, listed once. |
| `client_auth` | | `{ mode, ca, header }`: ask clients for a certificate in the TLS handshake, see [Client certificates (mTLS)](#client-certificates-mtls) (servers with `tls` only). |
| `tls_options` | | `{ min_version, cipher_suites, curves, ocsp_stapling }`: what the TLS handshake accepts, and OCSP stapling, see [TLS options](#tls-options) (servers with `tls` only). |
| `https_redirect` | `true` with `tls = "auto"`, else `false` | Redirect plain HTTP requests for this TLS server to HTTPS, see [HTTP to HTTPS redirect](#http-to-https-redirect) (http servers with `tls` only). |
| `http2` | `true` | Accept HTTP/2 (h2 over TLS, h2c in cleartext). |
| `http3` | unset | Serve HTTP/3 on the UDP port of each `listen` address, see [HTTP/3](#http3). Unset means on for `http` servers with `tls`. `true` makes a UDP port that cannot be bound a start error. `false` turns HTTP/3 off. |
| `trusted_proxies` | `[]` | CIDR ranges (or `"private_ranges"`) allowed to set `X-Forwarded-*` and the client IP. With `proxy_protocol_in`, also the only peers allowed to send a PROXY header (any peer when empty). |
| `ip_allow`, `ip_deny` | `[]` | CIDR ranges or addresses of the clients this server accepts and rejects, on every protocol, see [IP allow and deny lists](#ip-allow-and-deny-lists). |
| `rate_limit` | | `{ rate, per, burst, key }`: requests each client may send, see [Rate limits](#rate-limits) (http only). |
| `basic_auth` | | `{ users, realm }`: ask for a user name and password, see [Basic auth](#basic-auth) (http only). |
| `forward_auth` | | `{ url, copy_headers, timeout }`: let an external service (Authelia, Authentik, oauth2-proxy) decide, see [Forward auth](#forward-auth) (http only). |
| `error_pages` | | `{ "404" = "404.html", "5xx" = "50x.html" }`: files replacing the body of the errors rxh answers itself, see [Error pages](#error-pages) (http only). |
| `max_connections_per_ip` | | Concurrent connections of one client IP; more are closed on accept, see [Connections per client](#connections-per-client) (http and tcp). QUIC connections count too. |
| `proxy_protocol` | | `"v1"` or `"v2"`: send a PROXY protocol header with the client address on every new connection to a backend, see [PROXY protocol output](#proxy-protocol-output) (http and tcp). |
| `proxy_protocol_in` | `false` | Require a PROXY protocol header (v1 or v2) at the start of every connection and use its source address as the client address, see [PROXY protocol input](#proxy-protocol-input) (http and tcp). Set `trusted_proxies` too: with it empty any directly connecting client can spoof its address. |
| `match_timeout` | `"3s"` | Time a client has to send the PROXY header (`proxy_protocol_in`) and, on `tcp` servers with layer 4 matchers, the first bytes the matchers look at. |
| `read_header_timeout` | `"30s"` (TLS handshake `"10s"`) | Time a client has to send the request headers; slower clients are disconnected (http only). It also bounds the TLS handshake (`http` and `tcp` servers with `tls`) and the time to the first complete request head of a connection, HTTP/2 included, so clients that connect and send nothing are closed too. When unset, request heads get 30 seconds and TLS handshakes 10 seconds. On a listener shared by several http servers, this and `idle_timeout` come from the first server. |
| `idle_timeout` | unset | Keep-alive HTTP connections without traffic and without requests in flight for this long are closed; for HTTP/3, connections without requests in flight, see [HTTP/3](#http3). For `tcp`, a connection with no traffic in either direction for this long is closed. |
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

### HTTP/3

An `http` server with `tls` answers HTTP/3 (QUIC) on the UDP port of each of
its `listen` addresses, next to HTTP/1.1 and HTTP/2 on the TCP port. Nothing
else is needed: `http3` is unset by default, which means on.

```toml
[[server]]
name = "app"
listen = "0.0.0.0:443"
tls = { cert = "/etc/rxh/app.pem", key = "/etc/rxh/app.key" }
forward = "127.0.0.1:3000"
http3 = false                   # unset: on; true: a taken UDP port fails the start
```

- Open the UDP port in the firewall, next to the TCP port.
- `http3` unset: HTTP/3 is on for `http` servers with `tls`. A UDP port that
  cannot be bound gives a warning in the log, and the server runs without
  HTTP/3 and without `Alt-Svc`. With `http3 = true` the same error fails the
  start. `http3 = false` turns HTTP/3 off.
- `http3 = true` is a validation error on a server that cannot serve it: not
  an `http` server, no `tls`, `proxy_protocol_in` set, or
  `tls_options.cipher_suites` without a TLS 1.3 suite (QUIC needs TLS 1.3).
  With `http3` unset, such a server runs without HTTP/3.
- Servers that share a listen address share one QUIC endpoint. A server
  that sets `http3` must agree with what the first server on the address
  does: `http3 = false` on a later server while the first one serves
  HTTP/3, or `http3 = true` while it does not, is a validation error.
- A `udp` server and HTTP/3 cannot share a UDP port. `http3 = true` on an
  address whose port a `udp` server listens on (same IP, or either one is
  `0.0.0.0` or `[::]`) is a validation error. With `http3` unset, HTTP/3 is
  off on that address and `Alt-Svc` is not sent.
- RXH adds `Alt-Svc: h3=":<port>"; ma=86400` to responses over HTTP/1.1 and
  HTTP/2 that have none, so clients try HTTP/3 next time. The port is the
  one RXH listens on: behind a port mapping, it is not the public one. The
  header is not sent while HTTP/3 is off.
- `tls`, `client_auth`, `tls_options`, routes, matchers, limits and the
  access log work as for HTTP/1.1 and HTTP/2.
- `max_connections` and `max_connections_per_ip` count QUIC and TCP
  connections together. A QUIC connection over the limit is refused: it
  does not wait.
- When fewer than half of the `max_connections` slots are free, a new QUIC
  client must first prove its address (a QUIC Retry, one more round trip),
  so spoofed packets cannot take the slots. With `max_connections_per_ip`
  set, every new QUIC connection gets a Retry, one more round trip each, so
  spoofed packets cannot take the slots of an address they do not own.
- The request header section is limited to 16 KiB, as for HTTP/2.
- A `Content-Length` that is not one valid length (a sign, letters, an empty
  value, two different values) is answered `400` and never reaches a
  backend. Repeated identical values count as one, and the backend gets
  one. A body shorter or longer than its `Content-Length` resets the
  request stream with `H3_MESSAGE_ERROR`; the backend request is aborted
  and its connection closed, so nothing of the request is read as the
  start of the next one. The bytes that complete the declared length are
  forwarded once the client ends the request stream, which clients do with
  the last bytes.
- A request without `Content-Length` whose end arrives after RXH started to
  forward it goes to an HTTP/1.1 backend with `Transfer-Encoding: chunked`
  when its method takes a body (`POST`, `PUT`, ...). A `GET` or `HEAD`
  goes without a body: what arrives late is dropped.
- One request stream may have 1 MiB of request body on the way, and one
  connection 8 MiB over all its streams (QUIC flow control, fixed). A
  backend that reads slowly slows the client down instead of RXH buffering
  more.
- `idle_timeout` works as over TCP: a connection without requests in
  flight for that long gets a GOAWAY and is closed: the GOAWAY leaves
  between one and two `idle_timeout`s after its last response. A request in flight is never cut
  by it, however long the backend takes. Unset, there is no idle limit.
  Apart from that, RXH sends a QUIC PING every 10 seconds and drops a
  connection whose client stays silent for 30 seconds.
- With `runtime = "per_core"`, QUIC runs on the main runtime.
- Not available over HTTP/3: WebSockets and other upgrades (clients use
  HTTP/1.1 or HTTP/2 for them), 0-RTT, and HTTP/3 to backends (RXH talks
  HTTP/1.1 or HTTP/2 to them).

When a client cancels a request, RXH cancels the backend request, with one
exception. A client that cancels after it sent the whole request (a `GET`,
for example) and before the response starts is not noticed: the backend
request runs to its end. A cancel while the request body is still being
sent, or once the response is being written, is noticed. This is a limit of
the `h3` crate (0.0.8).

On a reload, the UDP socket and its QUIC endpoint stay: nothing is rebound,
and no packet is lost between two sockets. A connection open at the reload
gets a GOAWAY that covers every request the client sent before it. Those
requests finish with the configuration they started under, then RXH closes
the connection. A request the client sends on it after the GOAWAY is
refused with `H3_REQUEST_REJECTED`, which tells the client it was not
processed. The next request of the client opens a new connection, served
by the new configuration. A reload cuts requests in flight only in two
cases:

- they are still running when `grace_period` runs out;
- the new configuration gives the UDP port to a `udp` server: the QUIC
  endpoint and its connections are closed at once, before the new
  configuration starts. If it then fails to start, the running
  configuration serves HTTP/3 again on a new socket, but those requests
  were cut.

On `rxh stop`, connections drain the same way as on a reload: a GOAWAY, and
the requests in flight finish within `grace_period`. A connection without
requests gets the GOAWAY too before it is closed, so a request already on
its way is known not to be processed. A drained connection is closed at
most 3 seconds after its last response.

`rxh upgrade` does not hand the UDP sockets over: the new process binds the
UDP port itself (`SO_REUSEPORT`), and the old process closes its QUIC
connections at once, without a GOAWAY drain. HTTP/3 requests in flight
fail; clients open a new connection to the new process.
With systemd socket activation (`ListenDatagram=` on the port), the UDP
socket systemd passed is not handed over either. The new process cannot
bind the port while the old one holds that socket, so it runs without
HTTP/3 (with `http3 = true` it fails to start and the old process keeps
running). Use `systemctl restart` to upgrade there.

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
  rxh answers itself when no backend can be reached are not (use
  [`error_pages`](#error-pages) for those). Passive health
  checks and `retry_match` see the upstream status, not the replaced one.
- An entry without action or with two, a `replace_status` outside `200` to
  `599`, `copy_response = false`, `rewrite` without `serve` and
  `handle_response` on a `serve` or `redirect` route are configuration
  errors.

### Error pages

`error_pages` on an `http` server replaces the body of the error responses
rxh generates itself (Nginx `error_page`, Traefik `errors`) with a file. Keys
are a status code (`"404"`) or a class (`"5xx"`) from `4xx` and `5xx`;
values are file paths, relative to the working directory like `serve`.

```toml
[[server]]
listen = "0.0.0.0:80"
forward = "@app"
error_pages = { "404" = "/srv/errors/404.html", "503" = "/srv/errors/maintenance.html", "5xx" = "/srv/errors/50x.html" }
```

- An exact code wins over its class; a status with no page keeps the
  default plain text body.
- The pages cover what rxh answers on its own: `404` when no route
  matches, `403` from `ip_allow` and `ip_deny`, `429` from `rate_limit`,
  `401` from `basic_auth`, `413` from `max_body_size`, `400` for invalid
  requests, `502`, `503` and `504` when no backend can be reached or
  answers in time, and the `404`, `405` and `400` of the file server.
- Responses of a backend, of a `forward_auth` service and the `416` of
  range requests are never replaced: handle backend errors with
  [`handle_response`](#handling-upstream-responses).
- The status and the other headers stay (`Retry-After`,
  `WWW-Authenticate`, `Allow`); `Content-Type` comes from the file
  extension, as in the file server. `encode`, `cors` and
  `security_headers` apply to the page as to any response.
- Files are read once, when the server starts and on every reload. A file
  that cannot be read fails the start or the reload; edit a page and
  reload to send the new version.
- Requests that match no route of a listener shared by several servers
  get the pages of the first of them.
- A key outside `4xx` and `5xx` (`"302"`, `"2xx"`), an invalid status, an
  empty path and `error_pages` on `tcp` or `udp` servers are configuration
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

# One port for the office: databases and a SOCKS5 proxy, nothing for others.
[[server]]
listen = "0.0.0.0:5432"
protocol = "tcp"
match = [
    { remote_ip = ["10.0.0.0/8", "fd00::/8"], subroute = [
        { protocol = "postgres", forward = "@postgres" },
        { regexp = '^\x05[\x01-\xff]', regexp_count = 2, forward = "10.0.0.7:1080" },
    ] },
]
```

| Key | Description |
|---|---|
| `sni` | Server name or list from the TLS ClientHello, same patterns as `host`: exact or leading `*.` wildcard, case insensitive. |
| `alpn` | ALPN protocol or list; matches if the client offers any of them. |
| `protocol` | `"tls"` (a TLS ClientHello), `"http"` (an HTTP/1 request line, or the h2c preface `PRI * HTTP/2.0`), `"ssh"` (an `SSH-` banner) or `"postgres"` (the first message of a PostgreSQL client: a startup message of protocol 3, or an SSL, GSSAPI encryption or cancel request). |
| `http_host` | `Host` header of an HTTP/1 request, same patterns as `host`; the port is ignored. |
| `remote_ip` | List of CIDR ranges the client must belong to: the connected peer, or the source of the PROXY header with `proxy_protocol_in`. Needs no bytes from the client. |
| `regexp` | Regular expression ([syntax](https://docs.rs/regex/latest/regex/#syntax)) searched in the first `regexp_count` bytes of the connection. It is not anchored (start it with `^`) and matches bytes, not UTF-8 text: `\xff` is the byte 0xff; `(?u)` turns Unicode on. A client that sends fewer bytes does not match. |
| `regexp_count` | Bytes `regexp` looks at: 1 to 16384, `4` by default. Needs `regexp`. |
| `forward` | Where matching connections go. Each entry has its own pool. Required unless the entry has a `subroute`. |
| `subroute` | Instead of `forward`: a nested `match` list, tried in order on the connections this entry matches. When none of its entries matches, the outer list goes on with the next entry. Subroutes can be nested. |

`postgres` only tells that the client speaks PostgreSQL; clients that start
with an SSL request (most do) hide the user and the database from the proxy.
A `subroute` saves repeating the matchers of the outer entry, and RXH still
reads the first bytes once, whatever the depth. The inline pool of a nested
entry is named after its position: `NAME#1.0` is the first entry of the
subroute of `match[1]`.

To decide, RXH reads up to 16 KiB from the client for at most
`match_timeout`; these bytes are not lost, they are replayed to the chosen
backend. A client that sends nothing in time (protocols where the server
speaks first), closes early or sends something unrecognized only matches
entries without matchers (and those that only use `remote_ip`). The
ClientHello may span several TLS records. With `regexp` entries RXH also
waits, within `match_timeout`, until the client has sent the largest
`regexp_count`. When the first entry has no matchers, or the entries only
use `remote_ip`, nothing is read.

When the `tcp` server terminates TLS (`tls` is set), `sni` and `alpn` come
from the completed handshake and `protocol = "tls"` matches every
connection; `http_host`, `regexp` and the other protocols look at the
decrypted stream. The server offers the `alpn` values of its entries during the
handshake and `alpn` matches the negotiated protocol. Entries using only
`sni`, `alpn` or `protocol = "tls"` then need no extra read.

HTTP request matchers (`uri`, `host`, `path`, `method`, `header`, `query`)
are rejected on `tcp` and `udp` servers, `remote_ip` on `udp` servers, and
the other layer 4 matchers and `subroute` on `http` and `udp` servers. `rewrite`, `request`, `response`, `handle_response`,
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
