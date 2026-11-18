# Test coverage

Measured 2026-11-18 on the tree that includes the virtual-hosts work (details in section 2).

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
  child's counters are merged. The exception is `cli/src/process.rs::daemonize` (0%): the daemonized
  grandchild is killed by a signal and never flushes its counters (see gap 1).
- Branch coverage needs nightly: `cargo +nightly llvm-cov --branch`.

## 2. Current numbers

Measured 2026-11-18 with cargo-llvm-cov 0.9.1 on the **post-"virtual hosts" tree** (includes
`server/src/tls.rs`, `core/src/matcher.rs` and `server/tests/virtual_hosts.rs`). All test binaries pass.

| Scope | Lines | Regions | Functions |
|---|---|---|---|
| Whole workspace (incl. bench) | 71.19% (4189/5884) | 68.71% (4833/7034) | 70.90% (570/804) |
| **Workspace without `crates/bench`** | **82.17% (4029/4903)** | **79.72% (4638/5818)** | **80.88% (550/680)** |

`crates/bench` is a benchmark tool with no tests; it drags the total down about 11 points, so quote
the second row. (An earlier measurement on the pre-virtual-hosts snapshot gave 81.65% lines without bench.)

| Crate | Lines | Regions | Functions |
|---|---|---|---|
| core | 92.2% (1536/1666) | 91.9% | 89.5% |
| server | 81.0% (1923/2373) | 77.0% | 80.1% |
| cli | 66.0% (570/864) | 63.2% | 64.3% |
| bench (separate) | 16.3% (160/981) | 16.0% | 16.1% |

Per file, lowest first (paths relative to `crates/`):

| File | Lines | Regions | Funcs |
|---|---|---|---|
| bench/src/backend.rs | 0.0% (0/106) | 0.0% | 0.0% |
| bench/src/main.rs | 0.0% (0/158) | 0.0% | 0.0% |
| cli/src/process.rs | 0.0% (0/43) | 0.0% | 0.0% |
| bench/src/scenario.rs | 9.8% (25/254) | 10.4% | 21.1% |
| bench/src/load.rs | 16.1% (30/186) | 17.8% | 16.7% |
| bench/src/target.rs | 37.9% (105/277) | 35.8% | 26.5% |
| cli/src/main.rs | 43.8% (21/48) | 38.5% | 27.3% |
| server/src/error.rs | 46.7% (7/15) | 42.9% | 50.0% |
| cli/src/commands.rs | 51.0% (184/361) | 52.0% | 66.7% |
| server/src/http/transport.rs | 64.3% (81/126) | 64.2% | 75.0% |
| server/src/health.rs | 66.4% (83/125) | 65.1% | 34.8% |
| server/src/daemon.rs | 70.7% (111/157) | 63.2% | 69.6% |
| server/src/admin.rs | 71.3% (186/261) | 69.0% | 75.6% |
| server/src/files.rs | 71.3% (129/181) | 69.1% | 100.0% |
| core/src/duration.rs | 76.1% (67/88) | 79.3% | 71.4% |
| server/src/udp.rs | 76.3% (90/118) | 66.4% | 77.8% |
| server/src/http/proxy.rs | 77.5% (155/200) | 76.2% | 76.9% |
| server/src/tcp.rs | 79.2% (76/96) | 73.8% | 100.0% |
| cli/src/client.rs | 79.8% (67/84) | 75.8% | 65.0% |
| server/src/http/cache.rs | 81.7% (49/60) | 79.3% | 54.5% |
| server/src/registry.rs | 84.7% (61/72) | 73.6% | 61.5% |
| core/src/status.rs | 87.2% (41/47) | 83.3% | 70.0% |
| core/src/net.rs | 87.9% (58/66) | 84.3% | 69.2% |
| server/src/server.rs | 88.9% (256/288) | 79.7% | 90.3% |
| core/src/config.rs | 90.0% (493/548) | 89.8% | 88.4% |
| cli/src/edit.rs | 90.7% (284/313) | 83.8% | 75.0% |
| server/src/http/service.rs | 92.6% (75/81) | 92.8% | 90.9% |
| cli/src/cli.rs | 93.3% (14/15) | 96.7% | 100.0% |
| server/src/tls.rs | 93.4% (114/122) | 87.6% | 90.9% |
| core/src/pool.rs | 94.0% (592/630) | 92.8% | 92.8% |
| server/src/master.rs | 94.3% (83/88) | 89.6% | 100.0% |
| server/src/upstream.rs | 95.0% (38/40) | 85.7% | 80.0% |
| server/src/http/headers.rs | 95.4% (268/281) | 94.9% | 100.0% |
| server/src/http/body.rs | 96.4% (27/28) | 95.5% | 100.0% |
| core/src/matcher.rs | 99.3% (274/276) | 99.5% | 100.0% |
| core/src/random.rs | 100.0% (11/11) | 100.0% | 100.0% |
| server/src/http/response.rs | 100.0% (34/34) | 100.0% | 100.0% |

Existing tests: unit tests in `core`, `cli/src/edit.rs` etc.; integration tests
`server/tests/{http_proxy,layer4,lifecycle,load_balancing,virtual_hosts}.rs` and `cli/tests/daemon.rs`.

## 3. Biggest gaps

Line numbers are from the snapshot; they will shift.

1. **Daemonization and signals: `cli/src/process.rs` 20-81 (0%), `cli/src/main.rs` 45-76.**
   Untested: `daemonize` (PID file, log redirection, parent dir creation), `signal()` PID
   parsing (`bad PID`, `pid <= 0`), SIGTERM/SIGHUP mapping, `kill` failure. Risk: `rxh start/stop/reload`
   are the production control path; a bad PID file (0 or negative) would signal a process group.
   Test: in `cli/tests/daemon.rs`, `rxh start` with temp PID/log paths, assert the PID file exists and
   the pid is alive, `rxh reload` triggers a log line, `rxh stop` removes the process. Add unit
   tests in `process.rs` for `signal()` with a missing file, `abc`, `0`, `-1`, and a stale pid (ESRCH).
2. **CLI command output and error paths: `cli/src/commands.rs` 72-179, 200-233, 266-362, 440-492 (51.5%).**
   Untested: `status` table rendering (`print_pool`, health column `full`/`NO`/`?`, empty pool),
   `--json` paths, the commands that talk to the admin API for pools/upstreams, and connection-refused
   handling. Risk: operator-facing output and exit codes regress silently. Test: split rendering into
   pure functions taking `&Value` and unit-test them with fixtures; add `cli/tests/admin_client.rs`
   running the `rxh` binary against a stub admin server (`rxh status`, `rxh upstream add/remove`)
   asserting stdout and exit code, plus one test with no daemon asserting a non-zero exit and message.
3. **Reload failure and rollback: `server/src/daemon.rs` 121-128, 152-167, 186-227.**
   Untested: admin bind failure message, generation task ending (`Ok(Err)`/join error), reload with a
   broken config, and the `AddrInUse` fallback (stop old, start new, restore previous on failure, 190-201).
   Risk: a failed reload must keep traffic flowing; this is the most complex state machine in the
   server. Test: in `server/tests/lifecycle.rs`, (a) write an invalid config, POST `/reload`, assert 400
   and the old listener still answers; (b) a UDP/TCP config whose new listener port is taken, assert
   rollback restores the previous one; (c) bind `[admin] listen` to a busy port, assert the startup error text.
4. **Admin API endpoints: `server/src/admin.rs` 96-195, 213-238, 272-289, 330-342, 372-385 (71.6%).**
   Untested: non-loopback Host/Origin rejection (403, 96-99), `PUT/POST pools/:name/upstreams`
   validation errors, `DELETE` by `?address=` vs JSON body, 404 for unknown pool, `/stop`, and
   `/reload` while shutting down (503). Risk: this is a control plane; the loopback check is the
   only auth. Test: a table-driven test in `server/tests/lifecycle.rs` (or new `admin.rs`) sending each
   route with good and bad input, asserting status code and JSON; one test with `Host: evil.example`
   and one with a foreign `Origin` asserting 403.
5. **Health checks, redirects and fault injection: `server/src/health.rs` 63-81, 103-114, 125-148, 161-171 (66%, 35% functions).**
   Untested: connect errors/timeouts, redirect following (`Location` parse, "without a usable
   Location", "too many redirects"), status mismatch message, body regex mismatch, and the
   healthy-to-unhealthy-to-healthy transition. Risk: wrong health decisions either send traffic
   to dead backends or drop all backends. Test: unit tests in `health.rs` against a tiny hyper/tokio test
   server whose behavior is switchable (200, 302 loop, 500, slow, wrong body, closed socket);
   assert `pool` health flips after `fails`/`passes` thresholds. Use `tokio::time::pause` to avoid sleeps.
6. **Upstream transport and proxy failure paths: `http/transport.rs` 82-108, 154-210; `http/proxy.rs` 89-99, 158-178, 255-292 (64%/77%).**
   Untested: h2c upstream (connection reuse, closed sender, rewrite to `http://host/path` and
   header stripping), stale-connection retries exhausted (502), round-trip retry on failure with
   replay, `record_failure` on error. Risk: retries can duplicate non-idempotent requests; h2c
   silently broken. Test: in `server/tests/http_proxy.rs`, an h2c backend (hyper `http2::Builder`) asserting
   response and connection reuse count; a backend that closes after accept to hit the 502 path; one that
   fails once then succeeds to assert exactly one retry and failure counted.
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
12. **`crates/bench` (16%).** No tests. Decide whether it is a shipped tool: if not, exclude it from CI
    coverage; if yes, unit-test `load.rs` percentile/aggregation and `scenario.rs` parsing.

## 4. Structural recommendations

- **CI job:** `cargo llvm-cov --workspace --ignore-filename-regex 'crates/bench/' --lcov --output-path lcov.info --fail-under-lines 80`,
  upload lcov (Codecov/Coveralls) and gate PRs on "no drop in patch coverage". Raise the threshold as gaps close.
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

- [ ] Add CI coverage job with `--fail-under-lines` (start at 80, bench excluded) and `.gitignore` for `*.profraw`.
- [ ] Reload failure and rollback tests (gap 3).
- [ ] Admin API route table incl. loopback 403 (gap 4).
- [ ] `signal()` PID validation unit tests and `start`/`stop` e2e (gap 1).
- [ ] Health-check fault-injection backend and tests (gap 5).
- [ ] Proxy retry, h2c and 502 paths (gap 6).
- [ ] File server traversal and MIME tests (gap 7).
- [ ] PROXY protocol header byte tests and UDP failure/idle tests (gap 8).
- [ ] CLI rendering and admin-client e2e tests (gap 2).
- [ ] Config validation/edit edge cases (gap 9).
- [ ] Small modules: error, duration, cache, net, master, registry (gap 10).
- [ ] Decide bench policy (gap 12).
- [ ] TLS error paths and shared-listener failure isolation (gap 11).
