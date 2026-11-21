# Accounts, permissions per host and OIDC: design

Date: 2026-11-21. Status: approved.

## Intent

The dashboard and the admin API know one secret, the bearer token, and whoever
has it can do everything. The goal is what Nginx Proxy Manager offers and a bit
more: people sign in with their own account (a local password or the
organisation's OIDC provider), the first account is created on first use, and
an administrator decides per host or per group of hosts who may view, modify,
create and delete.

## Goals

- Local accounts with a password, and sign-in through one OIDC provider.
- First run: when no account exists the page asks to create the first
  administrator. No default credentials.
- Hosts (`[[server]]`) carry groups. Roles grant View, Modify, Create and
  Delete on groups and on single hosts. Users get roles; OIDC users get them
  from a claim of their provider.
- HTTPS on the admin listener itself (`tls` in `[admin]`).
- The bearer token, the CLI and scripts keep working unchanged.
- Everything the page does is an admin API route, and accounts can also be
  managed from the CLI.

## Non-goals

- SAML, LDAP, several OIDC providers, social logins, passkeys, two factor
  codes (the OIDC provider does those).
- Permissions on pools, certificates or settings: those stay administrator
  only.
- Restricting which fields of a host a user with Modify may change.
- ACME certificates for the admin listener (`tls = "auto"` is rejected in
  `[admin]`); point `cert` and `key` at files.
- Sessions that survive a restart or a hot upgrade of the daemon.
- Password reset by e-mail. An administrator or the CLI sets a new one.

## Anti-goals

- A fresh install reachable from the network that anyone can claim.
- A user without a grant learning that a host exists, through any route
  (lists, statistics, error messages).
- A way around the grants: editing a whole file, moving a host into a group,
  or a route that forgot the check.
- Breaking `rxh status` and friends on a machine once accounts exist.
- New dependencies for the protocol work.

## Approaches

1. **Everything inside rxh, no new dependency (chosen).** Users and roles in a
   file, bcrypt (already a dependency), an opaque session cookie with the
   sessions in memory, OIDC written on the existing outbound HTTPS client.
   More own code to review; a restart signs everybody out.
2. **The `openidconnect` crate.** Less protocol code, but dozens of
   dependencies, a second HTTP client and crypto stack; users, roles, grants
   and sessions are ours to write anyway.
3. **An identity proxy in front (Authelia, oauth2-proxy) and trusted
   headers.** Almost no code, but no local accounts and no first-run setup,
   which is what was asked for.

How HTTPS reaches the login was decided separately: `tls` on the admin
listener (chosen), over publishing the admin API as one more rxh host, and
over plain HTTP.

## Who a request is

Checked in this order, first match wins:

1. `Authorization: Bearer <token>` equal to `[admin] token`: administrator,
   as today.
2. A valid session cookie: that user, with the user's roles.
3. No token configured and the request is local: administrator, as today.
   This is what keeps the CLI working and what makes first-run setup from the
   machine possible. A shell on the machine already is an administrator.
4. Otherwise `401`, and the page shows the sign-in screen.

"Local" changes meaning. Today it is read from the `Host` and `Origin`
headers alone, which is safe only because a non-loopback listener must have a
token. From now on a request is local when both hold:

- the connection is local: a unix socket, or a TCP peer whose address is
  loopback or equal to the local address of the same connection (the machine
  talking to itself); and
- today's header rule passes (loopback or listen address `Host`, `Origin`
  equal to `Host`), which is what stops a web page in a local browser.

A `tls` admin listener still answers plain HTTP on the same port, to local
connections only: the first byte of a connection tells a TLS handshake from
HTTP, and a plain connection that is not local is closed. So the CLI needs no
TLS and no change, and `curl http://127.0.0.1:2019/…` on the machine works as
today.

Sign-in with a session is offered only over TLS or on a local request. On a
plain HTTP listener reached from the network only the token works, as today.

`listen`, `token`, `tls`, `users`, `public_url` and `[admin.oidc]` are read
when the daemon starts, like `token` today; a reload does not apply a change
to them. The documentation says so.

Validation of `[admin]` changes from "a non-loopback `listen` needs `token`"
to "needs `token` or `tls`".

A state changing request (anything but `GET`/`HEAD`) authenticated by a
cookie must carry an `Origin` equal to its `Host`, else `403`. The cookie is
`rxh_session`, `HttpOnly; Secure; SameSite=Strict; Path=/` (`Secure` is left
out on a local plain HTTP request).

## Configuration

```toml
[admin]
listen = "0.0.0.0:2019"
tls = { cert = "admin.pem", key = "admin.key" }   # new; files only
users = "rxh-data/users.toml"                     # new; this is the default
public_url = "https://rxh.example.com:2019"       # new; required with oidc

[admin.oidc]                                      # new, optional
issuer = "https://id.example.com/realms/main"
client_id = "rxh"
client_secret = "…"                               # masked like other secrets
scopes = ["openid", "profile", "email"]           # default
groups_claim = "groups"                           # default
name_claim = "preferred_username"                 # default; falls back to `sub`

[admin.oidc.roles]                                # claim value -> role names
"proxy-admins" = ["admin"]
"web-team" = ["web-editors"]

[[server]]
name = "shop"
groups = ["web", "customers"]                     # new
```

`groups` values and role names match `[a-z0-9][a-z0-9_-]*`. Hosts made by a
provider (Docker, Consul, Nomad, Kubernetes, etcd) take their groups from the
provider's existing label scheme (`rxh.groups=web,customers` for Docker).

`POST /certificates/reload` also rereads the admin certificate.

## The users file

One TOML file, mode `0600`, written with the same atomic write as the
configuration. Not part of the configuration files: it never appears in
`/config/files` and a configuration reload does not validate it. The daemon
rereads it when its modification time changes, so the CLI can edit it while
the daemon runs or is stopped.

```toml
[[user]]
name = "ana"
password = "$2b$12$…"        # bcrypt
roles = ["admin"]

[[user]]
name = "luis"
password = "$2b$12$…"
roles = ["web-editors"]

[[role]]
name = "web-editors"
grant = [
  { group = "web", allow = ["view", "modify", "create", "delete"] },
  { host = "shop", allow = ["view"] },
]
```

- `admin` is a built-in role: everything. It cannot be defined, changed or
  deleted. The last local user with `admin` cannot be removed or lose it.
- A grant names either a `group` or a `host`. `host` is the `name` of a
  `[[server]]`; a server without a name can only be reached through a group.
  `create` on a `host` grant is rejected.
- A user's permissions on a host are the union over the user's roles of the
  grants that name the host or one of its groups. `modify`, `create` and
  `delete` do not imply `view`; the page ticks View when another box is
  ticked.
- OIDC users are not stored. Their name and roles are computed at sign-in
  and live in the session.
- A host in several groups: View, Modify and Delete need a grant through
  any one of them (or through the host). Only Create needs every group.
- Passwords: at least 8 characters, at most 72 bytes (what bcrypt reads), no
  composition rules.

## What each permission allows

| Action | Needs |
|---|---|
| See a host in lists, its statistics and its form | View |
| Change a host (form or the TOML of its table) | Modify |
| Add a group to a host | Modify on the host and Create on the added group |
| Remove a group from a host | Modify on the host, and still Modify on it after the change |
| Change the `name` of a host | the `admin` role |
| Create a host whose `name` a `host` grant of any role names | the `admin` role |
| Create a host | Create on every group in its `groups`, which must not be empty |
| Delete a host | Delete |
| Anything else that changes or reveals something | the `admin` role |

"Anything else" is: pools and their live operations, certificates, settings
sections, whole files, `GET /config`, `/metrics`, reload, load, patch,
upgrade, stop, users and roles. A user without `admin`:

- gets from `GET /config/files` no file text, no sections, the servers with
  View only, and of the pools only the names (to choose one in a host form);
- gets from `GET /status` and `GET /stats/history` only servers with View and
  no pool figures;
- gets `403` on the administrator routes and `404` on a host without View,
  the same answer as for a host that does not exist;
- sees provider hosts with View, read only, as today.

Saving as a user without `admin`:

- The entity keys (`{file}:{position}`) and the file revision stay as they
  are and are given to the user; they reveal that the file has other tables
  and that something changed, which is accepted. The daemon finds the host
  behind a key and checks the grants against its current `name` and `groups`
  and, for a change, against the new ones.
- A stale revision answers `409` with a fixed text ("the configuration
  changed, read it again") and nothing about what changed.
- A validation error is passed on as it is. A clash with the name of a host
  the user cannot see is therefore visible; that is accepted.

### What a user without `admin` may put in a host

Found while building: a host table can make the daemon read files (`serve`,
`error_pages`, certificate paths, `{file.…}` and `{env.…}` placeholders), run
programs (health `hooks.command`), call other services (`forward_auth`,
webhooks) and open sockets. So Modify and Create do not mean "any table".

- A user without `admin` may set only the keys of an allow list kept in one
  place in the code: the keys that route or answer traffic for the host and
  do nothing else. Everything else, and every key the list does not know,
  must keep the value it has in the file (Modify) or be absent (Create); a
  change to it answers `403` and names the key.
- Never on the list: anything that names a path on the daemon's machine,
  runs a command, sends requests anywhere but to the forward targets, holds
  credentials, or changes who is trusted (`trusted_proxies`, client
  certificates). No string may contain a `{file.` or `{env.` placeholder.
  No forward target may be a unix socket.
- `listen`: only addresses that another server of the running configuration
  already listens on; no new sockets. `host` must not be empty on a host a
  user without `admin` creates.
- Forward targets stay free: a user with Modify can point a host at any
  address the daemon can reach. That is accepted, with one exception the
  daemon enforces for everybody: while no `token` is set, the proxy refuses
  to connect to its own admin API, because such a connection would arrive
  as a local administrator.
- One more exception: when the stored table of a host holds a `{env.` or
  `{file.` placeholder in any string (an administrator put a secret there),
  a user without `admin` may not change any forward target of that host,
  or the expanded value would be sent to a target of the user's choosing.
- Claiming a host name nobody uses yet on a shared listener is what Create
  is for; taking over a name another host already answers is refused when
  the configuration validation refuses it, and is otherwise a matter of
  trust.

The check lives in one place that every route goes through, written so that a
new route is administrator only unless it says otherwise.

## Sessions and sign-in

- Session id: 32 random bytes, base64url, kept in memory with the user name,
  the creation time and (for OIDC) the roles. Lifetime 12 hours, not
  extended. At most 1000 sessions; the oldest goes first.
- Local users' roles are looked up on every request, so a change applies at
  once. Removing a user or changing the password ends that user's sessions.
  An OIDC user keeps the roles of sign-in time until the session ends.
- Failed sign-ins: after 5 failures for a name from one client address, that
  name is refused from that address for one minute (`429`). An unknown name costs one bcrypt comparison too, and gets
  the same answer as a wrong password.
- Every sign-in, failed sign-in, sign-out and every change made through the
  API is logged with the user name (`token` or `local` for the other two
  kinds of caller).

## First run

While the users file has no user, `GET /auth/session` says `setup: true` and
the page shows "Create the first administrator" instead of the dashboard, to
callers who may do it:

- a caller who already is an administrator by rule 1 or 3 above (token, or
  local without a token), or
- on a TLS listener without a token, a caller who types the one-time setup
  code. The daemon writes the code (10 random characters) to its log at start
  while no user exists; it stops working once the first user exists.

Until the first user exists nothing else changes: the API behaves as today.

## OIDC

Authorization code flow with PKCE (`S256`), `state` and `nonce`.

- `GET /auth/oidc/login` reads the provider's discovery document
  (`{issuer}/.well-known/openid-configuration`, cached for an hour), stores
  `state`, `nonce` and the PKCE verifier for 10 minutes, and redirects to the
  authorization endpoint with `redirect_uri = {public_url}/auth/oidc/callback`.
- `GET /auth/oidc/callback` checks `state`, exchanges the code at the token
  endpoint over HTTPS with the system trust roots, and reads the ID token.
- The ID token came straight from the token endpoint over TLS, so its
  signature is not checked (OpenID Connect Core 3.1.3.7 allows this). Its
  claims are: `iss` equals the configured issuer, `aud` contains
  `client_id`, `exp` in the future, `nonce` equals the stored one. Anything
  else refuses the sign-in.
- The roles are the union of `[admin.oidc.roles]` over the values of
  `groups_claim` (a string or a list of strings). No role at all refuses the
  sign-in with a page that says so and names the user.
- An `issuer` that is not `https` is rejected at validation, except a
  loopback address (for tests).
- Sign-out ends the rxh session only.

## Routes

| Route | Who | What |
|---|---|---|
| `GET /auth/session` | anyone | `{ user, admin, roles, grants, setup, password_login, oidc }`; `user` is null when signed out |
| `POST /auth/login` | anyone | `{ name, password }`, sets the cookie |
| `POST /auth/logout` | session | ends it |
| `PUT /auth/password` | local user | `{ current, new }` |
| `POST /auth/setup` | see First run | `{ name, password, code? }` |
| `GET /auth/oidc/login`, `GET /auth/oidc/callback` | anyone | see OIDC |
| `GET /users`, `POST /users`, `PUT /users/{name}`, `DELETE /users/{name}` | admin | hashes never leave the daemon |
| `GET /roles`, `POST /roles`, `PUT /roles/{name}`, `DELETE /roles/{name}` | admin | deleting a role in use is refused |

The dashboard assets and `GET /auth/session` stay reachable without
credentials, as the assets are today.

## The page

- Signed out: a sign-in form (name and password), a "Sign in with
  &lt;issuer host&gt;" button when OIDC is configured, and the existing "use
  the admin token" entry behind a link.
- First run: the form described above.
- Header: the user name, "Change password" (local users), "Sign out".
- A user without `admin` sees Overview and Hosts only, with the hosts and
  buttons the grants allow. "New host" asks for the group first, from the
  groups where the user has Create.
- A new tab "Users" for administrators: the users with their roles, and the
  roles with a table of grants (group or host, four checkboxes).
- The host form gets `groups` from the schema like any other key; the hosts
  list shows them.

The page hides what a user may not do, but the daemon is what enforces it.

## CLI

`rxh user list`, `rxh user add <name> [--role <role>]…`, `rxh user passwd
<name>`, `rxh user remove <name>`. They edit the users file directly (found
through the configuration), read the password from the terminal or from
standard input, and are the way back in when every password is lost. Roles
are managed from the page or with the routes above.

## Delivery

Two plans, each leaving working software:

1. Accounts and permissions: `tls` on the admin listener, the users file,
   sessions, sign-in, first run, groups on hosts, roles and grants enforced
   on every route, the page, the CLI, documentation.
2. OIDC on top: configuration, the flow, role mapping, the button,
   documentation.

Tests follow the project rules: unit tests for the grant arithmetic and the
claim checks, integration tests against a real daemon for every route as
administrator, as a user with and without each permission and signed out, and
a fake OIDC provider on localhost in `crates/util` for the flow and each
refusal.

## Left to the builder

- Names of modules and types; how the single authorisation check is shaped.
- The exact label, tag or annotation each provider uses for `groups`.
- The JSON shapes of `/users` and `/roles` beyond what is listed.
- Layout of the sign-in screen and the Users tab, within the existing style
  and content security policy.
- The bcrypt cost (the project's existing default).
