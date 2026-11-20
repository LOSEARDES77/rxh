# Web UI: design

Date: 2026-11-20. Status: waiting for approval.

## Intent

RXH has a read only page at `GET /dashboard`. Everything else (statistics,
adding a host, draining a backend, changing a timeout) needs the CLI, `curl`
or an editor. The goal is one web page, simple to read, where an operator
finds the statistics and can do everything the admin API and the
configuration file allow, like Nginx Proxy Manager does for nginx.

This is the `ROADMAP.md` 0.8 item "Management in the dashboard, like Nginx
Proxy Manager: edit hosts and certificates, access lists".

## Goals

- Statistics with history: requests per second, errors, latency, requests in
  flight and backend health, per server and per pool, for the last hour.
- A form for every configuration key. Nothing requires writing TOML.
- Writing TOML stays possible: every form can switch to the text of its
  table, and the whole file can be edited as text.
- Changes are saved to the configuration file, validated first, and applied
  with a reload. A change that does not validate or does not reload leaves
  the file as it was.
- Every live operation of the admin API has a control: drain, maintenance
  and ready on a backend, reload, certificate reload, upgrade, stop.
- Simple first: the common fields of a host and a pool are at the top, the
  rest is folded away.

## Non-goals

- User accounts, first run setup, sessions, roles, OIDC. That is the next
  project, with its own design. This one keeps the bearer token.
- Uploading or issuing certificates from the page. Certificate files and
  ACME are configured through their forms; the files get on disk another way.
- The CLI tools that do not act on a daemon: `adapt`, `fmt`, `schema`,
  `hash-password`, `completions`, `man`, `file-server`, `reverse-proxy`.
- History on disk or longer than one hour. Prometheus scrapes `/metrics`.
- A log viewer.
- Editing what Docker, Consul, etcd, Nomad or Kubernetes generate. Those
  hosts are shown, read only.

## Anti-goals

- A form save that writes a resolved secret (`{env.X}`, `{file.X}`) or a
  `***` into the file.
- Losing comments or the layout of the configuration file outside the table
  that was edited.
- A Node toolchain in the build, the CI or the packages.
- A page that loads anything from another site.
- Markup built from strings (`innerHTML`): host names, Docker labels and
  backend addresses are not trusted text.

## Constraints found in the code

- `GET /config` returns resolved and redacted values, so forms cannot read
  from it. They read the tables of the files as written.
- `Change::Patch` and `POST /load` run in memory only (`change.rs`).
  Persistence exists only in the CLI: `crates/cli/src/edit.rs`
  (`ConfigFile`, `save_all`, on `toml_edit`), which validates with
  `rxh_server::load_config` and rolls the files back on failure.
- `AdminState::config_path` is `None` for the quick commands. There is then
  no file to edit.
- `Config::json_schema()` (schemars) describes all 292 keys in 71 types;
  224 have a description. 86 `anyOf`/`oneOf`, mostly "one value or a table".
- Metrics are process wide counters (`metrics.rs`) plus pool counters read
  from the registry. Nothing keeps history.
- The dashboard is one embedded file with an inline script, served without
  the token because it holds no data. `admin::tests::dashboard_only_reads`
  forbids write methods and `innerHTML` in it.
- The admin API is plain HTTP. Secrets are shown as `***` when they leave
  the process (`Admin::redact`, `AcmeDns::redact`, `restore_redacted`).

## Approaches

**Forms**

1. Generated from the JSON schema, with a curated layout (chosen). A
   generic renderer draws any schema type; a small layout table says which
   fields of a host and a pool come first and how the rest is grouped. New
   keys appear without touching the page. Types with several shapes look
   plainer than a hand made form; the few that matter get a hand made
   control.
2. Every form written by hand. Best finish, 292 fields to keep in step with
   the code.
3. Hand made forms for the common fields, TOML for the rest. Rejected by
   the user: everything must have a form.

**Saving**

1. Edit the table in the file with `toml_edit`, key by key, validate the
   whole configuration, write atomically, reload, roll back on failure
   (chosen). Comments and layout survive.
2. Send the whole configuration as JSON and rewrite the file. Loses
   comments and breaks `include`. Rejected.

**Statistics**: a ring buffer in the daemon (chosen) over rates computed in
the browser only (charts empty on every page load) and over history on disk
(Prometheus does that).

**Front end**: plain HTML, CSS and JavaScript embedded in the binary
(chosen) over a framework with a build step.

## Design

### Pages

One page at `/dashboard`, five tabs. Sketch of the first:

```
rxh 0.8.0 · up 3d 4h · /etc/rxh/rxh.toml            [ token ]
[Overview] [Hosts] [Pools] [Certificates] [Settings]

 412 req/s   0.3% errors   p95 38 ms   57 in flight   11/12 healthy
 ┌ Requests per second, by status class ─┐ ┌ Latency p50 p95 p99 ─┐
 │ ▁▂▃▅▆▇▆▅▃▂▁▂▃▅▆▇▆▅▃▂                 │ │ ▁▁▂▂▁▁▃▂▁▁           │
 └───────────────────────── 5m 15m [1h] ─┘ └──────────────────────┘
 Pool   req/s  errors  latency  in flight  healthy  trend
 api    301    0.1%    22 ms    40         3/3      ▂▃▅▆
 web    111    0.9%    61 ms    17         2/3      ▃▃▂▃
 Certificates expiring within 30 days: example.org (12 d)
```

- **Overview**: totals, two charts, one row per pool and per server, range
  5 minutes, 15 minutes or 1 hour.
- **Hosts**: the `[[server]]` entries: name, host names, listen, protocol,
  target, TLS, state, source (file name, or "runtime" when it is not in a
  file). Add, edit, delete.
- **Pools**: the `[upstream.NAME]` tables with their backends and live
  health. Per backend: drain, maintenance, ready (runtime, as the CLI does).
  Add, edit, delete; backends are part of the pool form.
- **Certificates**: the list of `GET /certificates` with expiry and OCSP,
  "reload certificates", and the `[acme]` form.
- **Settings**: one form per remaining section (top level keys, `[admin]`,
  `[log]`, `[tracing]`, the providers), the configuration files as text, and
  the daemon controls: reload, upgrade, stop. Upgrade and stop ask for
  confirmation by typing the word.

Sketch of a form:

```
Edit host "web"                              ( Form | TOML )
 Name        [ web            ]
 Host names  [ example.org ] [ + ]
 Listen      [ 0.0.0.0:443 ] [ + ]
 Protocol    ( http ▾ )
 Forward to  ( pool @api ▾ )
 TLS         ( automatic ▾ )
 ▸ Access (ip_allow, ip_deny, basic_auth, forward_auth)
 ▸ Headers and rewrites
 ▸ Timeouts and limits
 ▸ Everything else
                          [ Cancel ]  [ Save and reload ]
```

Each field shows the schema description as help. A validation error from
the daemon is shown at the top of the form, with the form still filled.

### New admin endpoints

All behind the same authentication as the rest of the API.

| Route | Purpose |
|---|---|
| `GET /schema` | `Config::json_schema()`. |
| `GET /stats/history` | The ring buffer, see below. |
| `GET /config/files` | The files (main first, then includes), each with `path`, `revision` (SHA-256 of its text), `text`, and its tables as entities. |
| `PUT /config/files/{index}` | Replace the text of one file. |
| `POST /config/entities/{kind}` | Create a table. Body: `value` or `toml`, optional `file` (default: the main file). |
| `PUT /config/entities/{kind}/{key}` | Replace a table. Body: `value` (JSON) or `toml` (text), and `revision`. |
| `DELETE /config/entities/{kind}/{key}` | Remove a table. |

- `kind` is `server`, `upstream` or `section`. `key` is the pool name, the
  section name (`top`, `admin`, `log`, `tracing`, `acme`, `docker`, `consul`,
  `etcd`, `nomad`, `kubernetes`), or for a server its position in its file,
  as `{file index}:{position}`.
- An entity is `{ kind, key, file, value, toml }`: `value` is the table as
  JSON exactly as written (placeholders not resolved, aliases kept), `toml`
  is its text.
- Every write: check `revision` (409 when the file changed since it was
  read), edit the document, write the files, load the configuration from
  the main file, reload the daemon, and restore the files when the load or
  the reload fails. The answer carries the error text of the failure.
- A `value` write is a merge into the existing table: a key with a new
  value is set, a missing key is removed, nested tables are merged the same
  way, arrays are replaced whole. Untouched keys keep their comments.
- Secrets leave as `***` in `value`, `toml` and `text`, and a `***` that
  comes back means "keep what is stored". The secret keys are the ones
  `redact` covers today, plus any found while building.
- Without a configuration file (`config_path` is `None`) the write routes
  answer 409 and the page shows everything read only.

`ConfigFile` and `save_all` move from `crates/cli` to `crates/server` so the
CLI and the admin API share one implementation; the CLI commands keep their
behaviour.

### Statistics history

A task in the daemon samples every 5 seconds and keeps 720 samples (one
hour), process wide like the metrics, so it survives reloads but not a
restart or an upgrade. A sample holds cumulative counters:

- per server: requests by status class (1xx to 5xx), the duration histogram
  (count, sum, buckets), layer 4 connections;
- per pool: requests, failures, server errors, response seconds, requests
  in flight, healthy and total backends.

`GET /stats/history` returns `{ interval_seconds, samples: [...] }`. The page
computes rates and percentiles from differences between samples, and asks
again every 5 seconds.

### Front end

- Files embedded with `include_str!`: the page, one stylesheet, a few
  scripts (API client, charts, schema forms, one per tab). Served under
  `/dashboard/` without the token, like the page today: they hold no data.
- Content security policy tightened to `script-src 'self'` and
  `style-src 'self'`; no inline script.
- All requests go through one function that adds the token, kept in
  `sessionStorage` as today. That is the single place the accounts project
  will change.
- Charts are SVG drawn by the page. Light and dark from
  `prefers-color-scheme`. Every control has a label and works from the
  keyboard.
- The schema renderer handles strings, numbers, booleans, enums, arrays,
  tables, maps (`additionalProperties`), and "one value or a list". Hand
  made controls for `forward`, `match`, `redirect`, `tls` and `serve`.
  A value the renderer cannot draw falls back to a TOML text box for that
  key only, so no key is ever unreachable.

### Tests

- `crates/server` unit tests: the merge of a JSON value into a `toml_edit`
  table (comments kept, key removed, nested table, array), secrets masked
  and restored, revision mismatch, the history ring (wraps at 720, counters
  cumulative).
- `crates/server/tests`: each new route against a real daemon on port 0:
  save, file on disk changed, new configuration serving requests; invalid
  value rejected with the file unchanged; reload failure rolls the file
  back; include file edited in place; no configuration file answers 409;
  token required; assets served without token and with the policy header.
- `crates/cli/tests`: the existing edit commands still pass after the move.
- The `dashboard_only_reads` test becomes: no `innerHTML` in any asset, and
  every route the scripts call exists.
- End to end, by hand, as `CLAUDE.md` asks: a real `rxh` with a temporary
  configuration and test backends, the page driven in a browser: add a
  host, edit a pool, break a value, drain a backend, watch the charts.

### Documentation

`PARITY.md`, `CHANGELOG.md`, `docs/configuration.md` (the dashboard section
and the new routes), `ROADMAP.md` (tick the 0.8 item, with a note that
certificate upload and accounts are not in it).

## Calls made by the builder of this design, not by the user

- The page replaces `/dashboard` at the same address; no second page.
- Five tabs, with the daemon controls inside Settings.
- 5 second samples, one hour, always on when the admin API is on.
- Secrets masked as `***` in the editing routes too.
- `revision` check against lost updates.
- Adding or removing a backend goes through the pool form (saved); only
  drain, maintenance and ready are runtime actions.
- The JavaScript has no unit test runner (that would need Node). It is
  covered by the route tests, the asset checks and the browser run.

## Left to the builder

- How a server that runs but is in no file is recognised (generated by a
  provider, or changed through `PATCH`).
- Whether a write with `watch = true` causes a second reload, and avoiding
  it if it does.
- What changing `[admin]` (`listen`, `token`) through a form does to the
  session, and the warning to show.
- The exact layout table: which fields are "common" for a host and a pool.
- The file permissions the packaged service needs to write its
  configuration, and the sentence for the documentation.
