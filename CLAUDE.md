# RXH

Reverse proxy and load balancer. Cargo workspace: `crates/core` (configuration
model, matchers, pools, policies), `crates/server` (daemon, HTTP, TCP, UDP,
admin API), `crates/cli` (the `rxh` binary), `crates/bench` (`rxh-bench`).

## Checks

All three must pass before work counts as done:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

## Tests are part of the change

- Every new behaviour, configuration option, CLI command, admin endpoint and
  bug fix ships with its tests in the same change. No feature without tests.
- A test must fail without the change. Do not write tests that only restate
  the implementation.
- A new configuration key gets parse and validation tests in `crates/core`
  (accepted values and rejected values), and an integration test of its
  runtime effect in `crates/server/tests` (or `crates/cli/tests` for the CLI
  and the daemon).
- Cover the failure path too: timeouts, refused connections, invalid input,
  limits reached.
- Tests bind to port 0 or use the helpers in `crates/util`; never a fixed
  port, never the network outside localhost.

## Check the feature itself

Passing tests are not enough. After implementing a feature, exercise it end
to end against a real `rxh` process: build the binary, write a temporary
configuration in a temporary directory, start test backends, send real
requests (`curl`, `openssl s_client`, `nc`) and compare the result with what
the task asked for. Stop every process you started. Never build or run under
emulation (QEMU, `docker buildx` for another architecture): it takes hours.
Check the native architecture only and say the others were not run. When the feature cannot
run on this machine (CI workflows, registries, cloud providers), say so and
state what was checked instead.

## Documentation moves with the code

Update in the same change: `PARITY.md` (status and notes of the row),
`CHANGELOG.md` (unreleased section), `docs/configuration.md` (every new key,
with an example) and `ROADMAP.md` (tick the item).

## Git

Work on a branch, commit there, never push unless asked.
