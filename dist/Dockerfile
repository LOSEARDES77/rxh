# Build:  docker build -f dist/Dockerfile -t rxh .
# Run:    docker run --network host -v $PWD/rxh.toml:/etc/rxh/rxh.toml rxh
FROM rust:1-slim AS build
WORKDIR /src
COPY . .
RUN cargo build --release --locked -p rxh-cli

FROM debian:stable-slim
COPY --from=build /src/target/release/rxh /usr/local/bin/rxh
WORKDIR /etc/rxh
ENTRYPOINT ["rxh", "run", "--config", "/etc/rxh/rxh.toml"]
