# Official images. One target per variant:
#
#   debian      debian:bookworm-slim, glibc (`latest`, `0.x`, `0.x.y`). Default.
#   alpine      alpine, static musl binary (`alpine`, `0.x-alpine`, ...).
#   distroless  gcr.io/distroless/cc-debian12, glibc, no shell (`distroless`, ...).
#
# Build:  docker build -f dist/Dockerfile --target alpine -t rxh:alpine .
# Run:    docker run -p 80:80 -v $PWD/rxh.toml:/etc/rxh/rxh.toml:ro rxh
#
# Every variant runs as UID 65532 with the configuration at /etc/rxh/rxh.toml
# (RXH_CONFIG) and /var/lib/rxh as working directory, where the relative ACME
# storage `rxh-data/certificates` lands: mount a volume there to keep
# certificates. The arguments are an `rxh` command, `run` by default.

FROM rust:1-slim-bookworm AS build-gnu
WORKDIR /src
COPY . .
RUN cargo build --release --locked -p rxh-cli \
    && mkdir -p /out/data \
    && cp target/release/rxh /out/rxh

FROM rust:1-alpine AS build-musl
RUN apk add --no-cache musl-dev
WORKDIR /src
COPY . .
RUN cargo build --release --locked -p rxh-cli \
    && mkdir -p /out/data \
    && cp target/release/rxh /out/rxh

FROM gcr.io/distroless/cc-debian12:nonroot AS distroless
COPY --from=build-gnu /out/rxh /usr/local/bin/rxh
COPY --from=build-gnu --chown=65532:65532 /out/data /var/lib/rxh
ENV RXH_CONFIG=/etc/rxh/rxh.toml
WORKDIR /var/lib/rxh
USER 65532:65532
ENTRYPOINT ["/usr/local/bin/rxh"]
CMD ["run"]

FROM alpine:3 AS alpine
COPY --from=build-musl /out/rxh /usr/local/bin/rxh
COPY --from=build-musl --chown=65532:65532 /out/data /var/lib/rxh
ENV RXH_CONFIG=/etc/rxh/rxh.toml
WORKDIR /var/lib/rxh
USER 65532:65532
ENTRYPOINT ["/usr/local/bin/rxh"]
CMD ["run"]

# Last stage: what `docker build` without --target produces.
FROM debian:bookworm-slim AS debian
COPY --from=build-gnu /out/rxh /usr/local/bin/rxh
COPY --from=build-gnu --chown=65532:65532 /out/data /var/lib/rxh
ENV RXH_CONFIG=/etc/rxh/rxh.toml
WORKDIR /var/lib/rxh
USER 65532:65532
ENTRYPOINT ["/usr/local/bin/rxh"]
CMD ["run"]
