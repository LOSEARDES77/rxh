# Test coverage

Measured 2026-11-18 on the tree of task 58 (section 2), after the tests for
daemon signals, reload rollback, the admin API, the CLI commands and the
upstream transport.

## 1. How to measure

```sh
rustup component add llvm-tools
cargo install cargo-llvm-cov --locked

cargo llvm-cov --workspace --html --open     # primary workflow: run all tests, build and open the report
cargo llvm-cov --workspace --summary-only    # per-file table and totals in the terminal
cargo llvm-cov --workspace --json --output-path cov.json   # machine readable (cannot be combined with --html)
cargo llvm-cov report --html                 # re-render from the last run without rebuilding
cargo llvm-cov report --lcov --output-path lcov.info       # for CI / Codecov; also reuses the last run
cargo llvm-cov --workspace --ignore-filename-regex 'crates/bench/' --summary-only   # without bench
```

The HTML report lands in `target/llvm-cov/html/index.html` for a normal run (under
`$CARGO_TARGET_DIR/llvm-cov/html/` if that is set). It shows line-level hits per file.

Notes:

- `cargo llvm-cov` sets `RUSTFLAGS` and `LLVM_PROFILE_FILE` itself, builds into
  `target/llvm-cov-target`, and merges every `.profraw` from the run. Do not run plain `cargo test` with
  `-C instrument-coverage` by hand: stray `*.profraw` files end up inside `crates/*`. Add `*.profraw` to `.gitignore`.
- **Subprocess coverage is captured.** `crates/cli/tests/daemon.rs` spawns the `rxh` binary;
  cargo-llvm-cov builds it instrumented and passes `LLVM_PROFILE_FILE` through the environment, so the
  child's counters are merged. This includes the daemon forked by `rxh start`: the test stops it with
  `rxh stop` (SIGTERM), the daemon exits normally and flushes its counters. A daemon killed with
  SIGKILL (the cleanup guard of a failing test) loses them.
- Branch coverage needs nightly: `cargo +nightly llvm-cov --branch`.

## 2. Current numbers

Measured 2026-11-18 with cargo-llvm-cov 0.9.1 and
`cargo llvm-cov --workspace --ignore-filename-regex 'crates/bench/' --summary-only`. All test
binaries pass. `crates/bench` is left out (benchmark tool without tests, see gap 12).

| Scope | Lines | Regions | Functions |
|---|---|---|---|
| **Workspace without `crates/bench`** | **92.18% (10578/11475)** | **90.30% (12080/13378)** | **88.42% (1283/1451)** |
| Same tree before task 58 | 88.62% (10072/11365) | 86.67% (11490/13257) | 86.12% (1241/1441) |

| Crate | Lines | Regions | Functions |
|---|---|---|---|
| core | 96.7% (3663/3789) | 96.7% | 95.8% |
| server | 89.6% (5599/6246) | 87.2% | 86.5% |
| cli | 90.9% (1221/1343) | 86.8% | 76.7% |
| util | 97.9% (95/97) | 98.6% | 100.0% |

Files covered by task 58, lines before and after:

| File | Before | After |
|---|---|---|
| cli/src/process.rs | 0.00% (0/43) | 98.84% (85/86) |
| cli/src/commands.rs | 62.77% (317/505) | 94.41% (540/572) |
| cli/src/main.rs | 34.21% (26/76) | 61.84% (47/76) |
| server/src/admin.rs | 66.93% (170/254) | 95.67% (243/254) |
| server/src/daemon.rs | 82.94% (248/299) | 93.98% (281/299) |
| server/src/http/transport.rs | 70.56% (127/180) | 93.89% (169/180) |

The line counts of `process.rs` and `commands.rs` grew because their unit tests are in the same file.

Per file, lowest first (paths relative to `crates/`):

| File | Lines | Regions | Funcs |
|---|---|---|---|
| cli/src/main.rs | 61.84% (47/76) | 55.56% | 44.44% |
| server/src/dynamic.rs | 69.17% (92/133) | 71.71% | 72.00% |
| server/src/health.rs | 69.87% (109/156) | 68.60% | 38.46% |
| server/src/error.rs | 73.33% (11/15) | 71.43% | 75.00% |
| server/src/acme.rs | 79.43% (560/705) | 76.68% | 76.19% |
| server/src/http/cache.rs | 81.67% (49/60) | 79.31% | 54.55% |
| server/src/stream.rs | 83.52% (147/176) | 71.79% | 75.00% |
| server/src/http/body.rs | 84.23% (187/222) | 83.02% | 93.33% |
| server/src/http/encode.rs | 84.25% (107/127) | 83.92% | 66.67% |
| server/src/udp.rs | 84.25% (123/146) | 74.38% | 76.92% |
| cli/src/client.rs | 84.52% (71/84) | 82.42% | 70.00% |
| core/src/duration.rs | 85.23% (75/88) | 88.43% | 85.71% |
| server/src/tcp.rs | 87.37% (83/95) | 82.39% | 100.00% |
| server/src/files.rs | 87.57% (331/378) | 88.10% | 100.00% |
| server/src/log.rs | 87.63% (85/97) | 86.82% | 85.71% |
| core/src/net.rs | 87.88% (58/66) | 84.34% | 69.23% |
| server/src/http/service.rs | 89.03% (211/237) | 84.91% | 74.29% |
| server/src/hooks.rs | 89.34% (109/122) | 81.08% | 64.71% |
| server/src/registry.rs | 89.74% (105/117) | 81.12% | 68.75% |
| cli/src/edit.rs | 89.82% (353/393) | 83.22% | 76.47% |
| server/src/server.rs | 90.95% (382/420) | 85.14% | 92.86% |
| cli/src/cli.rs | 91.43% (32/35) | 94.92% | 100.00% |
| server/src/http/proxy.rs | 91.48% (333/364) | 90.80% | 88.10% |
| server/src/include.rs | 91.91% (125/136) | 92.22% | 89.47% |
| server/src/http/transport.rs | 93.89% (169/180) | 92.74% | 95.24% |
| server/src/daemon.rs | 93.98% (281/299) | 90.74% | 92.31% |
| cli/src/commands.rs | 94.41% (540/572) | 90.57% | 82.35% |
| server/src/tls.rs | 94.47% (188/199) | 90.91% | 93.33% |
| server/src/http/response_handler.rs | 94.74% (54/57) | 87.04% | 85.71% |
| server/src/master.rs | 95.00% (95/100) | 91.74% | 100.00% |
| server/src/admin.rs | 95.67% (243/254) | 95.11% | 97.30% |
| cli/src/quick.rs | 95.88% (93/97) | 95.37% | 85.71% |
| server/src/http/headers.rs | 96.26% (360/374) | 95.85% | 97.73% |
| core/src/pool.rs | 96.44% (840/871) | 96.03% | 95.90% |
| server/src/proxy_protocol.rs | 96.63% (258/267) | 97.39% | 100.00% |
| server/src/upstream.rs | 96.77% (30/31) | 96.30% | 100.00% |
| core/src/config.rs | 96.89% (2117/2185) | 97.15% | 96.76% |
| core/src/address.rs | 97.30% (72/74) | 95.70% | 100.00% |
| core/src/status.rs | 97.87% (46/47) | 96.67% | 90.00% |
| server/src/http/response.rs | 97.87% (46/47) | 96.30% | 100.00% |
| util/src/flaky_backend.rs | 97.94% (95/97) | 98.57% | 100.00% |
| server/src/timeout.rs | 98.66% (147/149) | 98.52% | 100.00% |
| cli/src/process.rs | 98.84% (85/86) | 95.10% | 100.00% |
| server/src/l4match.rs | 99.06% (421/425) | 97.90% | 100.00% |
| core/src/template.rs | 99.12% (112/113) | 99.35% | 100.00% |
| core/src/matcher.rs | 99.28% (274/276) | 99.50% | 100.00% |
| core/src/random.rs | 100.00% (11/11) | 100.00% | 100.00% |
| core/src/size.rs | 100.00% (58/58) | 100.00% | 100.00% |
| server/src/metrics.rs | 100.00% (158/158) | 100.00% | 100.00% |

Tests: unit tests next to the code in every crate; integration tests in `server/tests/*.rs`
(the admin API in `admin.rs`, upstream connection handling in `upstream_pooling.rs`) and
`cli/tests/daemon.rs` (the `rxh` binary: `run`, `start`, `stop`, `reload`, every client command).

The web UI and its routes were added after the measurement of this section, which was not run
again: `server/src/config_api.rs`, `entity.rs`, `secret.rs`, `edit.rs` and `history.rs` are not in
the tables. Their tests: unit tests next to the code (the JSON merge into the TOML document, masking
and restoring secrets, the mode of a saved file, the history ring), `server/tests/config_edit.rs` (every `/config/files` and
`/config/entities` route against a daemon on port 0: save, invalid value and failed reload rolled
back, included file edited in place, revision conflict, no configuration file, token, a directory
that cannot be written)
and `server/tests/history.rs` (`GET /stats/history`). The page itself has Vitest unit tests in
`crates/server/ui/src` (`bun run test`: the schema renderer, rates and percentiles, the API client,
each tab), run by the CI job `ui`; they are outside the Rust line coverage. No test drives a real
browser: that check is made by hand against a running `rxh`.

## 3. Biggest gaps

Line numbers of gaps 1 to 4 and 6 are from the measurement of section 2. The other gaps were
written against an older snapshot: their line numbers and percentages are stale, use section 2.

1. **Daemonization and signals: done.** `process.rs` has unit tests for `signal()` (missing PID
   file, `abc`, empty, `0`, `-1`, a reaped PID giving `ESRCH`, SIGTERM and SIGHUP delivered to a
   child process). `cli/tests/daemon.rs::start_reload_and_stop_through_the_pid_file` runs `rxh start`
   with the admin API disabled and nested PID/log paths, then `rxh reload` and `rxh stop` through the
   PID file, and checks the log file; `start_refuses_a_busy_admin_address` and a broken configuration
   cover the foreground failures. `upgrade_hands_the_listening_sockets_to_a_new_process` and
   `upgrade_of_a_background_daemon_keeps_its_pid_file_and_admin_socket` run `rxh upgrade` and
   `SIGUSR2` against real daemons under load (Linux).
   Left: `process.rs` 26 (`create_dir_all` failure) and `main.rs` 38-41, 65-94 (`reverse-proxy` and
   `file-server` startup errors, runtime build failures).
2. **CLI command output and error paths: done.** `status` table (version line, server row, health
   `yes`/`NO`, `(no backends)`), `pool list`, `upstream list [pool]` with and without a daemon,
   `--no-save` without a daemon, admin API errors (404, conflict) as a failing exit code, `stop` and
   `reload` without daemon or PID file, `service rm` of an unknown service, `config show` from the
   file, `validate`, `policies`; unit tests for `describe_target`, `without_nulls` and the missing-file
   paths. No rendering refactor was needed: the tests run the binary.
   Left (`commands.rs`): the `full` and `?` health values (174, 176), the 30 s wait of `stop` when the
   daemon keeps draining (71-75), a non-unreachable error on `stop` (87), and `service add --serve` with
   a missing directory (584-585).
3. **Reload failure and rollback: done.** `server/tests/admin.rs`: invalid TOML, an invalid
   configuration and a deleted file each give 400 while the old listener keeps answering; a UDP
   listener on a busy port makes the reload stop the old generation, fail, and restore it (the old
   backend answers again and `/status` lists only the old server); a UDP reload succeeds through the
   stop-then-start path; a busy `[admin] listen` fails startup with the hint.
   Left (`daemon.rs`): the generation task ending on its own (217-220), the old generation failing to
   stop (129-132, 289, 308), and the `[admin]` change warning (267).
4. **Admin API endpoints: done.** `server/tests/admin.rs` covers every route: `/` and `/status`,
   `/pools`, `/pools/{name}` (404, percent-decoded names), `/reverse_proxy/upstreams`, `/metrics`
   content type, `/config` with live members, `POST`/`PUT` (201, 409, 400 for mixed schemes, invalid
   JSON and a body over 64 KiB, 404), `DELETE` by `?address=` and by JSON body (404 for unknown member
   or pool, 400 without an address), unknown routes, `/reload` without a file, `/stop` ending the
   daemon, and a foreign `Host` or `Origin` (403 on read and write routes).
   Left (`admin.rs`): accept errors (99), `/reload` while the daemon shuts down (192, 197), the
   `starting`/`max_connections_reached`/`shutting_down` state names (299-305).
5. **Health checks, redirects and fault injection: `server/src/health.rs` 63-81, 103-114, 125-148, 161-171 (66%, 35% functions).**
   Untested: connect errors/timeouts, redirect following (`Location` parse, "without a usable
   Location", "too many redirects"), status mismatch message, body regex mismatch, and the
   healthy-to-unhealthy-to-healthy transition. Risk: wrong health decisions either send traffic
   to dead backends or drop all backends. Test: unit tests in `health.rs` against a tiny hyper/tokio test
   server whose behavior is switchable (200, 302 loop, 500, slow, wrong body, closed socket);
   assert `pool` health flips after `fails`/`passes` thresholds. Use `tokio::time::pause` to avoid sleeps.
6. **Upstream transport and proxy failure paths: transport done.** `server/tests/upstream_pooling.rs`:
   h2c (one shared HTTP/2 connection with absolute URIs, `http2_conns_per_host` connections used in turn, redial after the backend closes it, 502 when
   it is down), pooling disabled with `keepalive = "0s"` or `keepalive_idle_conns_per_host = 0`, idle
   expiry after `keepalive`, idle connections closed by `read_timeout` replaced without an error, and
   the `keepalive_idle_conns_per_host` cap.
   Left (`transport.rs` 278, 287-293): `SendError::Stale` and `forget`, which need a pooled connection
   that closes between the readiness check and the write; there is no deterministic way to provoke it
   from a test backend. The proxy retry paths are covered by `retry_headers.rs` and `http_proxy.rs`.
7. **Static file server: `server/src/files.rs` 88-128, 168-192, 234-257 (71%).**
   Untested: most of `content_type` (extension table), error branches (88-128: likely 404/403/
   method/range handling), and 234-257. Risk: path traversal and wrong MIME types are security
   and UX bugs. Test: unit-test `content_type` over the whole table; integration test with a temp
   dir serving `GET ../secret`, `%2e%2e/`, a directory without index, `HEAD`, and a missing file, asserting 403/404 and no body leak.
8. **UDP and TCP edge cases: `udp.rs` 63-67, 101-107, 163-192; `tcp.rs` 41-58, 88-121 (76%/79%).**
   Untested: backend ICMP refusal (`record_failure`, session removal, 178-192), idle expiry,
   send errors, PROXY protocol v1 IPv6/UNKNOWN and v2 IPv6/mixed-family headers (88-121), connect failures.
   Risk: session leaks and a wrong PROXY header break backends that parse it. Test: pure unit tests of the
   header builder in `tcp.rs` comparing bytes to the spec for v1/v2 x v4/v6/mixed; in `layer4.rs`, a UDP
   session whose backend is closed (assert the session is dropped and failures counted) and idle timeout with a short config value.
9. **Config validation and editing: `core/src/config.rs` 148-168, 365-373, 509-514, 677-718, 1171-1209; `cli/src/edit.rs` 47-52, 94-102, 215-232, 298-301.**
   Untested: a handful of validation error messages and TOML edit edge cases (92% core, 91% edit).
   Risk: bad configs accepted or misreported at reload. Test: one table-driven unit test in
   `config.rs` with invalid inputs (empty pool, bad duration, duplicate listen, unknown policy) asserting the
   error text; round-trip tests in `edit.rs` for comments/ordering preserved after add/remove.
10. **Small gaps with outsized risk.** `server/src/error.rs` 26-48 (`Display`/`source` conversions, 47%);
    `core/src/duration.rs` 18-24, 125-133 (parse errors, `Display`); `http/cache.rs` 58-98 (expiry, eviction,
    non-GET bypass); `core/src/net.rs` 90-98; `master.rs` 141-155 (worker failure); `registry.rs` 38-39, 133-135.
    Tests: unit tests per module (cache TTL expiry using `tokio::time::pause`, duration error cases, error `Display` strings).
11. **TLS and shared listeners (new): `server/src/tls.rs` 55, 59, 138, 142, 151-153 (93%); `server/src/server.rs` 289-324, 391-393, 421-423, 475-504 (89%); `master.rs` 165-179; `http/service.rs` 97-100.**
    Untested: the few TLS error branches (bad cert/key file, SNI miss handling at 151-153), and in the
    shared-listener/virtual-host code the bind and accept error paths and shutdown of a listener shared by several sites.
    Risk: a bad certificate or one failing site taking down hosts that share a port. Test: in
    `server/tests/virtual_hosts.rs`, a config with an unreadable cert path and a mismatched key asserting a clear startup error;
    two hosts on one port where one has no matching SNI/Host asserting the fallback/404 and that the other still serves;
    reload that removes one host and asserts only it disappears.
12. **`crates/bench` (16%): excluded.** A benchmark tool, not shipped with the proxy; the CI coverage
    gate leaves it out with `--ignore-filename-regex 'crates/bench/'`.
13. **Accounts and permissions (new, not in the measured numbers above).** Integration tests against a
    real daemon: `server/tests/accounts.rs` (setup, sign-in, sessions, limits, `/users`, `/roles`, the gate),
    `server/tests/permissions.rs` (grants on every host route, the allow list of host keys, placeholders),
    `server/tests/admin_tls.rs` (HTTPS and plain HTTP on one port), `server/tests/admin_ssrf.rs` (the proxy
    refusing its own admin API), `cli/tests/users.rs` (`rxh user`); unit tests in `access.rs`, `session.rs`
    and `host_keys.rs`; the page in Vitest (`ui/src/lib/session.test.ts`, `SignIn.test.ts`).
    Untested by automation: a connection that is not from the machine (every test connects from loopback,
    so the setup code over TLS, `password_login: false` and the closed plain HTTP connection are covered by
    unit tests of the peer and by Vitest only), the `Secure` attribute of the cookie over TLS (checked by
    hand), the admin guard on the `forward_auth`, health webhook and tracing dialers (only the forward,
    health check and TCP paths have tests), and restoring the terminal echo of `rxh user` after `Ctrl-C`
    (checked by hand).
    Sign-in through a provider: `server/tests/oidc.rs` against the stand-in provider of
    `util/src/oidc_provider.rs` (plain HTTP and HTTPS with `ca`: the flow, every claim check, the binding
    cookie, limits, timeouts, oversized and redirected answers, no secret in answers or `GET /config`,
    the refusal of a login sent by another site, the redirect to `public_url` under another `Host`, the admin
    guard on its dialers; the log was checked by hand),
    unit tests in `oidc.rs` and `config.rs`, the page in Vitest (`SignIn.test.ts`, `Users.test.ts`). Not
    tested by automation: a real provider (none was used, by hand either), and the `Secure` attribute of
    the binding cookie (unit test only); the flow in a browser over a `tls` admin listener was checked by
    hand.

## 4. Structural recommendations

- **CI job: done.** The `coverage` job of `.github/workflows/ci.yml` runs
  `cargo llvm-cov --workspace --ignore-filename-regex 'crates/bench/' --summary-only --fail-under-lines 85`
  on Ubuntu and fails below 85% of lines. Raise the threshold as gaps close; uploading lcov
  (Codecov/Coveralls) and a patch-coverage gate are not set up.
- **Ignore noise:** add `*.profraw` to `.gitignore`; mark `crates/bench` as excluded in the CI command.
- **Daemon and signals:** test through the real `rxh` binary with temp dirs, random free ports
  (bind to port 0 and read it back), `kill_on_drop` guards, and a bounded poll loop instead of
  sleeps. For the SIGTERM-flush problem, install a SIGTERM handler that exits normally (already the case
  for the server loop) so profile counters flush; `daemonize` itself needs a `fork` and is best covered by the e2e test only.
- **Error paths:** every `map_err`/`Err(` branch listed above gets one test; prefer table-driven tests
  (`for (input, expected) in cases`) to keep the cost low.
- **Admin API:** one helper in `server/tests/util` to start a daemon and call a route; then each
  route is a 5-line case.
- **Fault injection:** a reusable `FlakyBackend` in `server/tests/util` with modes (refuse, accept-then-close,
  slow, bad status, redirect loop, wrong body) used by health, proxy retry and L4 tests.
- **Time:** use `tokio::time::pause()` for health intervals, cache TTL and UDP idle timeouts.
- **Refactor for testability:** move table rendering in `cli/src/commands.rs` into pure functions returning
  `String`.
- Re-measure after each feature merge and refresh section 2.

## 5. Prioritized checklist

- [x] CI coverage job with `--fail-under-lines 85` (bench excluded) and `.gitignore` for `*.profraw`.
- [x] Reload failure and rollback tests (gap 3).
- [x] Admin API route table incl. loopback 403 (gap 4).
- [x] `signal()` PID validation unit tests and `start`/`stop` e2e (gap 1).
- [ ] Health-check fault-injection backend and tests (gap 5).
- [x] Proxy retry, h2c and 502 paths (gap 6; stale-connection retry left).
- [ ] File server traversal and MIME tests (gap 7; traversal done in `server/tests/files.rs`, `content_type` table left).
- [ ] PROXY protocol header byte tests and UDP failure/idle tests (gap 8; header bytes done in `proxy_protocol.rs`, UDP failure and idle left).
- [x] CLI rendering and admin-client e2e tests (gap 2).
- [ ] Config validation/edit edge cases (gap 9).
- [ ] Small modules: error, duration, cache, net, master, registry (gap 10).
- [x] Decide bench policy (gap 12): excluded from the coverage gate.
- [ ] TLS error paths and shared-listener failure isolation (gap 11).
- [ ] Accounts: a non-local peer in an integration test, and the admin guard on every dialer (gap 13).
- [x] HTTP/3: `crates/server/tests/http3.rs` (requests, limits, timeouts, client certificates, reload and stop drain). Not in the measured table above.
