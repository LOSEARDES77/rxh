# RXH documentation

RXH is a reverse proxy and load balancer for HTTP/1.1, HTTP/2, WebSocket,
and any TCP or UDP protocol. It runs as a daemon driven by one TOML file and
a CLI that changes pools and listeners while it runs, without dropping a
connection.

This site has two parts: **guides**, one per use case, that build a working
setup step by step, and the **reference**, which lists every key.

## Guides

| Guide | You want to |
|---|---|
| [HTTP load balancer](guides/load-balancer.md) | Spread HTTP traffic over several backends, take broken ones out of rotation, add and remove backends live. |
| [TLS gateway](guides/tls-gateway.md) | Terminate HTTPS in front of your services, with automatic certificates or your own, several sites on one address, client certificates. |
| [Docker](guides/docker.md) | Run RXH in a container, and route to containers from their labels as they start and stop. |
| [Layer 4: TCP and UDP](guides/layer4.md) | Balance databases, Redis, DNS or game servers, route TLS by SNI without decrypting it, carry client addresses with the PROXY protocol. |

## Install

```sh
cargo install --git https://github.com/LOSEARDES77/rxh rxh-cli   # installs `rxh`
docker run -p 80:80 -v "$PWD/rxh.toml:/etc/rxh/rxh.toml:ro" ghcr.io/loseardes77/rxh
```

Static binaries, `.deb`, `.rpm`, AUR and Homebrew packages are published on
every release, see
[Installing a release package](configuration.md#installing-a-release-package).

## The commands you will use

```sh
rxh validate -c rxh.toml   # check a file without starting anything
rxh run -c rxh.toml        # run in the foreground
rxh start -c rxh.toml      # run as a background daemon
rxh status                 # listeners, pools and backend health
rxh reload                 # read the file again, without dropping connections
rxh stop
```

The CLI talks to the daemon through the admin API, on `127.0.0.1:2019` by
default (see [`[admin]`](configuration.md#admin)).

## Building this site

The pages are plain Markdown, readable on GitHub as they are. The site is
built with [mdBook](https://rust-lang.github.io/mdBook/):

```sh
cargo install mdbook
mdbook serve docs          # http://localhost:3000, rebuilt on every change
mdbook build docs          # static site in target/book
```

Every `toml` block of the guides is a complete configuration: the test
`crates/core/tests/guides.rs` parses and validates each one, and checks the
links between the pages, so a guide cannot drift from what RXH accepts.
