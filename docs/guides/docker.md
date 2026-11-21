# Docker

This guide covers two things that go well together: running RXH itself in a
container, and letting RXH route to other containers from their labels, so a
service is reachable as soon as it starts and gone when it stops, without
touching the configuration file.

## Running RXH in a container

The image `ghcr.io/loseardes77/rxh` (amd64 and arm64; `alpine` and
`distroless` variants) runs `rxh run` as an unprivileged user, reading
`/etc/rxh/rxh.toml`. A minimal configuration:

```toml
[upstream.app]
backends = ["app1:8080", "app2:8080"]
health = { uri = "/health", interval = "5s" }

[[server]]
listen = "0.0.0.0:80"
forward = "@app"
```

```sh
docker run -d --name rxh -p 80:80 \
  -v "$PWD/rxh.toml:/etc/rxh/rxh.toml:ro" \
  -v rxh-data:/var/lib/rxh \
  ghcr.io/loseardes77/rxh
docker exec rxh rxh status     # the CLI is in the image
docker kill -s HUP rxh         # reload after editing rxh.toml
```

- Listeners must bind `0.0.0.0` (or `[::]`) to be reachable through the
  published ports. Ports below 1024 work as the unprivileged user.
- Host names in `backends` are resolved on every connection, so the names
  of other containers on the same Docker network work, and follow the
  containers when they are recreated.
- The image ships no configuration, and relative paths of yours are taken
  from `/etc/rxh`, which belongs to root in the image: only the
  configuration file is mounted into it, and the user of the image cannot
  create anything there. Put what the daemon writes,
  the ACME certificates and the admin accounts, in `/var/lib/rxh` (it
  belongs to the user of the image) and keep that on a volume, as
  `-v rxh-data:/var/lib/rxh` does above:

  ```toml
  [admin]
  users = "/var/lib/rxh/users.toml"

  [acme]
  storage = "/var/lib/rxh/certificates"
  ```

  Without that, ACME cannot store its certificates and the first admin
  account cannot be created.
- The admin API stays on `127.0.0.1:2019` inside the container; use
  `docker exec` to reach it.

See [Running in a container](../configuration.md#running-in-a-container) for
the tags and variants.

### The dashboard from outside the container

With the default `listen`, `127.0.0.1:2019`, `docker exec rxh rxh status`
comes from inside the container, which counts as the machine itself: it is
an administrator without a token. A published port needs the admin API on
`0.0.0.0`, with a `token` or `tls`, and there nobody is an administrator
for where it connects from, `docker exec` included. A published port
usually reaches the container from another address (the bridge gateway, or
the client itself), but not always: rootless Podman can connect from the
container's own address and `kubectl port-forward` from loopback, for every
client. So on such a listener every caller needs the token or a session.

With `tls`, people sign in with accounts. Mount a certificate next to the
configuration:

```toml
[admin]
listen = "0.0.0.0:2019"
tls = { cert = "/etc/rxh/admin.pem", key = "/etc/rxh/admin.key" }
users = "/var/lib/rxh/users.toml"

[acme]
storage = "/var/lib/rxh/certificates"

[[server]]
listen = "0.0.0.0:80"
forward = "app:8080"
```

```sh
docker run -d --name rxh -p 80:80 -p 2019:2019 \
  -v "$PWD/rxh.toml:/etc/rxh/rxh.toml:ro" \
  -v "$PWD/admin.pem:/etc/rxh/admin.pem:ro" \
  -v "$PWD/admin.key:/etc/rxh/admin.key:ro" \
  -v rxh-data:/var/lib/rxh \
  ghcr.io/loseardes77/rxh
docker logs rxh | grep 'setup code'      # for the first account, from the page
docker exec -it rxh rxh user add ana     # or create it here instead
```

Then open `https://HOST:2019/dashboard`. The key must be readable by UID
65532. Until the first account exists the page asks for the setup code of
the log; after that, for a name and a password. `docker exec rxh rxh
status` and the other commands that talk to the daemon now answer `401`:
add a `token` to `[admin]` to keep using them (they read it from the
configuration file), while `rxh user` needs none. Sessions live in memory:
recreating or restarting the container signs everybody out, the accounts
stay on the volume.

With a `token` instead of `tls`, the page asks for the token ("Use the admin
token"), and the API is plain HTTP: keep it on a private network or behind
an SSH tunnel. See [Accounts](../configuration.md#accounts).

## Routes from container labels

With a `[docker]` table RXH watches the Docker API. Every running container
labelled `rxh.enable=true` is routed as its other `rxh.*` labels say:

```toml
[docker]
endpoint = "unix:///var/run/docker.sock"
listen = "0.0.0.0:80"          # where the HTTP routes of the labels listen
```

```sh
docker run -d --label rxh.enable=true \
  --label rxh.http.host=whoami.localhost \
  --label rxh.http.port=80 \
  nginx:stable-alpine
curl -H 'Host: whoami.localhost' http://127.0.0.1/
```

The container is routed when it starts and dropped when it stops, without a
reload. Containers of the same pool (by default, the replicas of one Compose
service) are load balanced together. `rxh status` shows them as a pool and a
server named `docker-POOL`.

| Label | Meaning |
|---|---|
| `rxh.enable` | `true` to route to the container. |
| `rxh.http.host` | Host names of an HTTP route, comma separated, exact or `*.example.com`. |
| `rxh.http.port` | Container port of the HTTP route; optional when the container exposes one port. |
| `rxh.tcp.listen` | Address of a TCP route instead: `:5432` or `127.0.0.1:5432`. |
| `rxh.tcp.port` | Container port of the TCP route. |
| `rxh.pool` | Pool name; containers with the same pool are balanced together. |
| `rxh.policy` | Load balancing policy of the pool. |

Containers are reached at their address on the Docker network, so their
ports do not need to be published, but RXH must run on the Docker host or in
a container on the same network.

## A complete Compose stack

RXH in a container, watching the socket, in front of three replicas of an
application and a database:

```yaml
# compose.yaml
services:
  rxh:
    image: ghcr.io/loseardes77/rxh
    ports:
      - "80:80"
      - "5432:5432"
    volumes:
      - ./rxh.toml:/etc/rxh/rxh.toml:ro
      - /var/run/docker.sock:/var/run/docker.sock:ro
    # The image runs as UID 65532: give it the group that owns the socket
    # (`stat -c %g /var/run/docker.sock` on the host).
    group_add:
      - "${DOCKER_GID:-999}"

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
    environment:
      POSTGRES_PASSWORD: example
    labels:
      rxh.enable: "true"
      rxh.tcp.listen: ":5432"
```

with this `rxh.toml`:

```toml
[docker]
listen = "0.0.0.0:80"
```

```sh
DOCKER_GID=$(stat -c %g /var/run/docker.sock) docker compose up -d
docker compose up -d --scale app=5     # the new replicas join the pool
```

## HTTPS and tuning in front of the label routes

Label routes are plain HTTP with the defaults of their pool: no TLS, health
checks or header rules. For those, keep the label routes on an internal
address and put a server of the file in front of them:

```toml
[acme]
email = "ops@example.com"

[docker]
listen = "127.0.0.1:8080"

[upstream.containers]
backends = ["127.0.0.1:8080"]

[[server]]
name = "edge"
listen = "0.0.0.0:443"
host = ["app.example.com", "api.example.com"]
tls = "auto"
security_headers = "basic"
forward = "@containers"
```

The `Host` header passes through unchanged, so the label routes still pick
the right containers. The [TLS gateway guide](tls-gateway.md) covers the TLS
side.

## Securing the socket

The Docker socket gives full control of the host. RXH only needs to list
containers and follow events, so a socket proxy that allows nothing else is
safer than the socket itself:

```toml
[docker]
# A socket proxy allowing only GET /containers/json and GET /events.
endpoint = "tcp://docker-socket-proxy:2375"
listen = "0.0.0.0:80"
```

## Podman and Swarm

- **Podman**: `provider = "podman"`, with the rootful socket or the socket
  of the user; containers that publish their port are reached on the host.
  See [Podman](../configuration.md#podman).
- **Swarm**: `provider = "swarm"` on a manager node reads the labels of the
  services (`deploy.labels`) and balances over their tasks. See
  [Docker Swarm](../configuration.md#docker-swarm).

```toml
[docker]
provider = "podman"
endpoint = "unix:///run/podman/podman.sock"
listen = "0.0.0.0:80"
```

The full list of keys and rules is in
[`[docker]`](../configuration.md#docker-routes-to-containers-from-their-labels).
