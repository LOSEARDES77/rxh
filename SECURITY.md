# Security policy

## Supported versions

Security fixes go to the latest release and to `main`. Older releases do not
receive fixes; upgrade to the latest one.

| Version         | Supported |
| --------------- | --------- |
| 0.3.x (`main`)  | Yes       |
| 0.2.x and older | No        |

## Reporting a vulnerability

Do not open a public issue for a vulnerability. Report it privately through
GitHub: **Security > Report a vulnerability** on
<https://github.com/LOSEARDES77/rxh/security/advisories/new>.

Include:

- the RXH version (`rxh --version`) and the platform;
- the configuration needed to reproduce, reduced to the minimum;
- the requests or traffic that trigger the issue, and what happens;
- the impact you expect (bypass of a matcher, crash, memory growth, leak of
  data between clients or upstreams, and so on).

You get an acknowledgement within 7 days. Once the issue is confirmed, a fix
and an advisory are prepared together, and the advisory is published with
the release that fixes it. You are credited in the advisory unless you ask
not to be.

## Scope

In scope: the `rxh` binary and the `rxh-core` and `rxh-server` crates,
including the HTTP, TCP and UDP proxies, TLS termination and automatic
HTTPS, the PROXY protocol parser, the configuration loader and the admin
API.

Out of scope:

- the admin API reachable by untrusted clients: it has no authentication and
  must listen on a loopback address or a protected network (the default is
  `127.0.0.1:2019`);
- `insecure_skip_verify = true` on an upstream, which disables certificate
  checks by design;
- denial of service that needs more traffic than the configured limits and
  timeouts allow;
- `rxh-bench` and the test helpers, which are not shipped.

## Dependencies

Every push, pull request and a daily schedule run `cargo audit` and
`cargo deny` (`.github/workflows/audit.yml`, policy in `deny.toml`). A
vulnerable, unsound, unmaintained or yanked dependency, a license outside the
allow list, or a crate from an unknown source fails the build. An exception
is added to `deny.toml` with the advisory ID and the reason.
