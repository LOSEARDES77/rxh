# Configuration migration

How the configuration file changes between releases, and what to do when
it does.

## Policy

- A key or value that gets a new name is **deprecated**, not removed: the
  old spelling keeps working with the same meaning, and `rxh validate`
  prints a warning that names the file, the table, and the replacement.
- Deprecated spellings are removed only in a major release (2.0 for
  anything deprecated during 1.x). Before 1.0, nothing deprecated is
  removed either; the table below is the full list.
- Every deprecation and every change of meaning is listed under
  `Deprecated` or `Changed` in [`CHANGELOG.md`](../CHANGELOG.md) and gets a
  row in the table below, with the release that deprecated it.
- Stricter validation (a value that used to be accepted and was never
  meant to be) can land in a minor release; the changelog says so under
  `Changed`, and `rxh validate` reports the line to fix before you upgrade.

## Upgrading

1. Read the `Deprecated` and `Changed` sections of the changelog for every
   release between yours and the new one.
2. Run the new binary's `rxh validate -c rxh.toml` against your file
   before restarting. It exits non zero on errors and prints one line per
   deprecated spelling on standard error, in the main file and in every
   file pulled in with `include`:

   ```text
   warning: rxh.toml: server[0]: key `connections` is deprecated since 0.3.0, use `max_connections`
   warning: conf.d/web.toml: upstream.web: policy `WRR` is deprecated since 0.3.0, use `weighted_round_robin`
   ```

   The path after the file name is the table: `upstream.NAME`,
   `server[N]` (counted from 0 in that file), `server[N].forward` for an
   inline pool and `server[N].match[M].forward` for the pool of a route.
3. Rename what the warnings list, using the table below, and run
   `rxh validate` again until it prints no warning. `rxh fmt` keeps your
   comments if you want to tidy the file afterwards.
4. Reload (`rxh reload`) or restart.

Deprecated spellings in Docker labels (`rxh.policy`) and in the admin API
(`POST /load`, `PATCH`) are accepted but not reported; only files are
checked. `rxh adapt` never writes them.

## Deprecated keys and values

| Deprecated | Where | Use instead | Since |
|---|---|---|---|
| `connections` | `[[server]]` | `max_connections` | 0.3.0 |
| `algorithm` | pool (`[upstream.NAME]`, inline `forward`) | `policy` | 0.3.0 |
| `policy = "WRR"`, `policy = "wrr"` | pool | `policy = "weighted_round_robin"` | 0.3.0 |

```toml
# Before
[upstream.web]
algorithm = "WRR"
backends = ["10.0.0.1:80"]

[[server]]
listen = "0.0.0.0:80"
connections = 2048
forward = "@web"

# After
[upstream.web]
policy = "weighted_round_robin"
backends = ["10.0.0.1:80"]

[[server]]
listen = "0.0.0.0:80"
max_connections = 2048
forward = "@web"
```

The Caddy spellings of pool keys (`lb_policy`, `lb_retries`,
`lb_try_duration`, `lb_try_interval`, `to`) are not deprecated: they are
kept on purpose, for people coming from a Caddyfile.
