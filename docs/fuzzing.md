# Fuzzing

The parsers that read untrusted or hand written input are fuzzed with
[`cargo fuzz`](https://github.com/rust-fuzz/cargo-fuzz) (libFuzzer). The
targets live in `fuzz/`, a crate outside the main workspace because
`cargo fuzz` needs a nightly toolchain. They reach the internals of
`rxh-server` through its `fuzzing` feature, which only the fuzz crate turns
on.

| Target | Input | Checks |
|---|---|---|
| `config` | Any UTF-8 text | `Config::from_toml` and `IncludedConfig::from_toml` return an error instead of panicking; a valid configuration expands its pools, adds HTTPS redirects and compiles every request matcher. |
| `proxy_protocol` | Raw bytes, or a source, a destination and trailing bytes | The v1/v2 parser never panics, never waits for more than the longest header, a header ends inside its input and parses the same without the bytes after it, a truncated header waits for more; every header RXH writes reads back to the same client address. |
| `matchers` | Host and path patterns and candidates | Compiling and matching never panic; `*.example.com` matches a subdomain but not `example.com`; an exact host matches itself in upper case and with a port; a path pattern matches every path that fills its `*`, and without `*` only itself. |
| `l4_detect` | Raw bytes | Sniffing a TLS ClientHello, an HTTP/1 request head or an SSH banner never panics. |
| `cache_key` | Two (pool, host, URI) triples | Two requests share a response cache key only when pool, host and URI are equal. |

## Running

Install the tool once and use the nightly toolchain:

```sh
rustup toolchain install nightly
cargo install cargo-fuzz
cd fuzz
cargo +nightly fuzz list
cargo +nightly fuzz run proxy_protocol -- -max_total_time=60
```

Without `-max_total_time` a target runs until it finds a crash or is stopped
with Ctrl+C. Add `-jobs=N -workers=N` to use several cores.

The configuration target learns much faster from real files. Seed its corpus
with the examples before the first run:

```sh
mkdir -p corpus/config
cp ../examples/*.toml ../examples/includes/*.toml corpus/config/
cargo +nightly fuzz run config -- -max_total_time=120
```

`corpus/` and `artifacts/` are not committed. The `fuzz` job of
`.github/workflows/ci.yml` runs every target for 60 seconds on each push.

## When a target fails

libFuzzer prints the panic and writes the input to
`fuzz/artifacts/<target>/crash-<hash>`. Replay it, then minimize it:

```sh
cargo +nightly fuzz run cache_key artifacts/cache_key/crash-<hash>
cargo +nightly fuzz tmin cache_key artifacts/cache_key/crash-<hash>
cargo +nightly fuzz fmt cache_key artifacts/cache_key/crash-<hash>
```

Fix the bug and add a regular test with the minimized input to the crate
that owns the code, so `cargo test --workspace` keeps it fixed without
nightly. The first run of `cache_key` found that keys joined with `|` were
ambiguous (`Host: h|/a` with `/b` against `Host: h` with `/a|/b`); the
regression tests are `keys_keep_pool_host_and_uri_apart` in
`crates/server/src/http/cache.rs` and `cache_keeps_host_and_uri_apart` in
`crates/server/tests/http_proxy.rs`.
