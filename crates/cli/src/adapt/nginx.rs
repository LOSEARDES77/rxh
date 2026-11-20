//! nginx.conf to RXH: `http` and `stream` servers, `upstream` blocks,
//! `listen`, `server_name`, certificates, prefix and exact `location`s with
//! `proxy_pass`, `root`, `alias`, `return`, headers, `gzip` and
//! `client_max_body_size`.

use std::cmp::Reverse;
use std::collections::HashMap;

use toml_edit::{Array, InlineTable, Item, Table, Value};

use crate::adapt::directive::{self, Directive, Syntax};
use crate::adapt::output::{self, Headers, Output, Server};

/// Directives that tune nginx itself (workers, logs, buffers, TLS
/// protocols) and change nothing RXH has to know.
const IGNORED: [&str; 34] = [
    "user",
    "worker_processes",
    "worker_rlimit_nofile",
    "worker_connections",
    "pid",
    "events",
    "load_module",
    "daemon",
    "pcre_jit",
    "error_log",
    "access_log",
    "log_format",
    "sendfile",
    "tcp_nopush",
    "tcp_nodelay",
    "keepalive_timeout",
    "types_hash_max_size",
    "server_names_hash_bucket_size",
    "server_tokens",
    "default_type",
    "types",
    "charset",
    "ssl_protocols",
    "ssl_ciphers",
    "ssl_prefer_server_ciphers",
    "ssl_session_cache",
    "ssl_session_timeout",
    "ssl_session_tickets",
    "ssl_stapling",
    "ssl_stapling_verify",
    "http2",
    "try_files",
    "proxy_http_version",
    "proxy_redirect",
];

/// Headers RXH already sends to backends (`X-Forwarded-*`, the original
/// `Host`) or handles itself (upgrades), so their `proxy_set_header` lines
/// are dropped.
const AUTOMATIC_HEADERS: [&str; 6] = [
    "host",
    "x-forwarded-for",
    "x-forwarded-proto",
    "x-forwarded-host",
    "upgrade",
    "connection",
];

/// Adds the servers of an nginx configuration to `out`.
pub(crate) fn convert(text: &str, out: &mut Output) -> Result<(), String> {
    let directives: Vec<Directive> = directive::parse(text, Syntax::Nginx)?;
    let mut adapter = Adapter {
        out,
        pools: HashMap::new(),
    };
    // Files of `sites-available` hold bare `server` and `upstream` blocks.
    adapter.http(&directives, &Context::default(), /* top_level= */ true);
    Ok(())
}

struct Adapter<'a> {
    out: &'a mut Output,
    /// RXH pool names of the `upstream` blocks.
    pools: HashMap<String, String>,
}

/// Settings inherited from `http` by `server` and from `server` by
/// `location`.
#[derive(Clone, Default)]
struct Context {
    root: Option<String>,
    index: Vec<String>,
    browse: bool,
    gzip: bool,
    max_body_size: Option<String>,
    request: Headers,
    response: Headers,
}

impl Adapter<'_> {
    /// The `http` block, or the top level of the file.
    fn http(&mut self, directives: &[Directive], parent: &Context, top_level: bool) {
        for upstream in directives.iter().filter(|d| d.name == "upstream") {
            self.upstream(upstream);
        }
        let mut context: Context = parent.clone();
        self.settings(directives, &mut context);
        for directive in directives {
            match directive.name.as_str() {
                "server" => self.http_server(directive, &context),
                "http" if top_level => self.http(directive.children(), &context, false),
                "stream" if top_level => self.stream(directive.children()),
                "upstream" => {}
                name if self.is_setting(name) => {}
                "include" => self
                    .out
                    .warn(directive.line, "include (convert the file it names)"),
                name => self.out.warn(directive.line, format!("directive {name}")),
            }
        }
    }

    fn is_setting(&self, name: &str) -> bool {
        IGNORED.contains(&name)
            || name.starts_with("gzip_")
            || [
                "root",
                "index",
                "autoindex",
                "gzip",
                "client_max_body_size",
                "add_header",
                "proxy_set_header",
            ]
            .contains(&name)
    }

    /// Applies the inheritable settings among `directives` to `context`.
    /// Like nginx, `add_header` and `proxy_set_header` of a block replace
    /// all of the enclosing block's.
    fn settings(&mut self, directives: &[Directive], context: &mut Context) {
        let mut request = Headers::default();
        let mut response = Headers::default();
        for directive in directives {
            let line: usize = directive.line;
            match (directive.name.as_str(), directive.args.as_slice()) {
                ("root", [root]) => context.root = Some(root.clone()),
                ("index", files) => context.index = files.to_vec(),
                ("autoindex", [on]) => context.browse = on == "on",
                ("gzip", [on]) => context.gzip = on == "on",
                ("client_max_body_size", [size]) => context.max_body_size = self.size(size, line),
                ("add_header", [name, value, ..]) => {
                    if let Some(value) = self.variables(value, line, None) {
                        response.set.push((name.clone(), value));
                    }
                }
                ("proxy_set_header", [name, value]) => {
                    if AUTOMATIC_HEADERS.contains(&name.to_ascii_lowercase().as_str())
                        && value.contains('$')
                    {
                        continue;
                    }
                    if value.is_empty() {
                        request.remove.push(name.clone());
                    } else if let Some(value) = self.variables(value, line, None) {
                        request.set.push((name.clone(), value));
                    }
                }
                _ => {}
            }
        }
        if directives.iter().any(|d| d.name == "add_header") {
            context.response = response;
        }
        if directives.iter().any(|d| d.name == "proxy_set_header") {
            context.request = request;
        }
    }

    fn upstream(&mut self, upstream: &Directive) {
        let [name] = upstream.args.as_slice() else {
            self.out.warn(upstream.line, "upstream without a name");
            return;
        };
        let mut pool = Table::new();
        let mut backends = Array::new();
        let mut passive = Table::new();
        for option in upstream.children() {
            let line: usize = option.line;
            match (option.name.as_str(), option.args.as_slice()) {
                ("server", [address, parameters @ ..]) => {
                    let address: String = match address.strip_prefix("unix:") {
                        Some(path) => format!("unix/{path}"),
                        None => output::with_port(address, 80),
                    };
                    let mut weight: Option<i64> = None;
                    let mut skip = false;
                    for parameter in parameters {
                        match parameter.split_once('=') {
                            Some(("weight", value)) => weight = value.parse().ok(),
                            Some(("max_fails", value)) => {
                                if let Ok(fails) = value.parse::<i64>() {
                                    passive.insert("max_fails", toml_edit::value(fails));
                                }
                            }
                            Some(("fail_timeout", value)) => {
                                passive.insert("fail_duration", toml_edit::value(duration(value)));
                            }
                            None if parameter == "backup" || parameter == "down" => {
                                self.out.warn(line, format!("{parameter} server {address}"));
                                skip = true;
                            }
                            _ => self.out.warn(line, format!("server parameter {parameter}")),
                        }
                    }
                    if skip {
                        continue;
                    }
                    match weight {
                        Some(weight) => {
                            let mut backend = InlineTable::new();
                            backend.insert("address", Value::from(address));
                            backend.insert("weight", Value::from(weight));
                            backends.push(backend);
                        }
                        None => backends.push(address),
                    }
                }
                ("least_conn", []) => {
                    pool.insert("policy", toml_edit::value("least_conn"));
                }
                ("ip_hash", []) => {
                    pool.insert("policy", toml_edit::value("ip_hash"));
                }
                ("random", []) => {
                    pool.insert("policy", toml_edit::value("random"));
                }
                ("random", [two, ..]) if two == "two" => {
                    pool.insert("policy", toml_edit::value("random_choose"));
                    pool.insert("choose", toml_edit::value(2));
                }
                ("hash", [key, ..]) => self.hash(key, line, &mut pool),
                ("keepalive", [connections]) => match connections.parse::<i64>() {
                    Ok(connections) => {
                        pool.insert(
                            "keepalive_idle_conns_per_host",
                            toml_edit::value(connections),
                        );
                    }
                    Err(_) => self.out.warn(line, "keepalive"),
                },
                ("keepalive_timeout", [timeout]) => {
                    pool.insert("keepalive", toml_edit::value(duration(timeout)));
                }
                ("zone", _) => {}
                (name, _) => self.out.warn(line, format!("upstream directive {name}")),
            }
        }
        if backends.is_empty() {
            self.out
                .warn(upstream.line, format!("upstream {name} has no server"));
            return;
        }
        pool.insert("backends", toml_edit::value(backends));
        if !passive.is_empty() {
            pool.insert("passive", Item::Table(passive));
        }
        let pool_name: String = self.out.pool_name(name);
        self.pools.insert(name.clone(), pool_name.clone());
        self.out.pools.push((pool_name, pool));
    }

    fn hash(&mut self, key: &str, line: usize, pool: &mut Table) {
        let (policy, policy_key): (&str, Option<String>) = match key {
            "$request_uri" | "$uri" => ("uri_hash", None),
            "$remote_addr" | "$binary_remote_addr" => ("ip_hash", None),
            key => match (key.strip_prefix("$http_"), key.strip_prefix("$arg_")) {
                (Some(header), _) => ("header", Some(header.replace('_', "-"))),
                (_, Some(argument)) => ("query", Some(argument.to_owned())),
                _ => {
                    self.out.warn(line, format!("hash {key}"));
                    return;
                }
            },
        };
        pool.insert("policy", toml_edit::value(policy));
        if let Some(policy_key) = policy_key {
            pool.insert("key", toml_edit::value(policy_key));
        }
    }

    fn http_server(&mut self, server: &Directive, parent: &Context) {
        let mut context: Context = parent.clone();
        self.settings(server.children(), &mut context);
        let mut listens: Vec<(String, bool)> = Vec::new();
        let mut hosts: Vec<String> = Vec::new();
        // `$server_name` in a `return` may come before `server_name`.
        for directive in server.children().iter().filter(|d| d.name == "server_name") {
            self.server_names(directive, &mut hosts);
        }
        let mut certificate: Option<&String> = None;
        let mut key: Option<&String> = None;
        let mut redirect: Option<Table> = None;
        let mut locations: Vec<(u8, Reverse<usize>, Table)> = Vec::new();

        for directive in server.children() {
            let line: usize = directive.line;
            let args: &[String] = &directive.args;
            match (directive.name.as_str(), args) {
                ("listen", [address, flags @ ..]) => {
                    let tls: bool = flags.iter().any(|flag| flag == "ssl");
                    if let Some(listen) = self.listen(address, flags, line) {
                        listens.push((listen, tls));
                    }
                }
                ("server_name", _) => {}
                ("ssl_certificate", [path]) => certificate = Some(path),
                ("ssl_certificate_key", [path]) => key = Some(path),
                ("return", args) => {
                    let mut route = Table::new();
                    if self.return_action(args, line, hosts.first(), &mut route) {
                        redirect = Some(route);
                    }
                }
                ("location", _) => {
                    if let Some(location) = self.location(directive, &context, hosts.first()) {
                        locations.push(location);
                    }
                }
                (name, _) if self.is_setting(name) => {}
                (name, _) => self.out.warn(line, format!("directive {name}")),
            }
        }

        // Exact locations first, then the longest prefix, as nginx picks.
        locations.sort_by_key(|(rank, length, _)| (*rank, *length));
        let mut routes: Vec<Table> = match redirect {
            // A `return` outside of locations answers every request.
            Some(route) => vec![route],
            None => locations.into_iter().map(|(_, _, route)| route).collect(),
        };
        let has_catch_all: bool = routes
            .iter()
            .any(|route| ["path", "uri"].iter().all(|key| !route.contains_key(key)));
        if !has_catch_all && let Some(root) = &context.root {
            let mut route = Table::new();
            route.insert("serve", toml_edit::value(serve(root, &context)));
            let route: Table = self.finish(route, &context, /* proxied= */ false, server.line);
            routes.push(route);
        }
        if routes.is_empty() {
            self.out
                .warn(server.line, "server with nothing rxh can serve");
            return;
        }
        if listens.is_empty() {
            listens.push((String::from("0.0.0.0:80"), false));
        }

        let tls: Option<Item> = match (certificate, key) {
            (Some(cert), Some(key)) => {
                let mut files = InlineTable::new();
                files.insert("cert", Value::from(cert.as_str()));
                files.insert("key", Value::from(key.as_str()));
                Some(toml_edit::value(files))
            }
            _ if !hosts.is_empty() => Some(toml_edit::value("auto")),
            _ => None,
        };
        for tls_listen in [false, true] {
            let addresses: Vec<String> = listens
                .iter()
                .filter(|(_, tls)| *tls == tls_listen)
                .map(|(address, _)| address.clone())
                .collect();
            if addresses.is_empty() {
                continue;
            }
            let mut table = Table::new();
            table.insert("listen", output::one_or_many(&addresses));
            if !hosts.is_empty() {
                table.insert("host", output::one_or_many(&hosts));
            }
            if tls_listen {
                match &tls {
                    Some(tls) => {
                        if tls.as_str() == Some("auto") {
                            self.out.warn(
                                server.line,
                                "ssl without ssl_certificate: certificates from [acme] instead",
                            );
                        }
                        table.insert("tls", tls.clone());
                    }
                    None => {
                        self.out
                            .warn(server.line, "ssl without certificates or server_name");
                        continue;
                    }
                }
                // nginx redirects only when told to, with its own server.
                if tls.as_ref().and_then(Item::as_str) == Some("auto") {
                    table.insert("https_redirect", toml_edit::value(false));
                }
            }
            self.out.servers.push(Server {
                table,
                routes: routes.clone(),
            });
        }
    }

    /// Adds the names of a `server_name` directive to `hosts`.
    fn server_names(&mut self, directive: &Directive, hosts: &mut Vec<String>) {
        for name in &directive.args {
            match name.as_str() {
                "" | "_" => {}
                name if name.starts_with('~') || name.ends_with(".*") => {
                    self.out.warn(directive.line, format!("server_name {name}"));
                }
                name => match name.strip_prefix('.') {
                    Some(domain) => {
                        hosts.push(domain.to_owned());
                        hosts.push(format!("*.{domain}"));
                    }
                    None => hosts.push(name.to_owned()),
                },
            }
        }
    }

    /// The `listen` address, `None` (with a warning) for unix sockets and
    /// QUIC.
    fn listen(&mut self, address: &str, flags: &[String], line: usize) -> Option<String> {
        for flag in flags {
            match flag.as_str() {
                "ssl" | "http2" | "default_server" | "default" | "reuseport" | "udp" => {}
                flag if flag.starts_with("backlog=") || flag.starts_with("ipv6only=") => {}
                flag => self.out.warn(line, format!("listen parameter {flag}")),
            }
        }
        if address.starts_with("unix:") || flags.iter().any(|flag| flag == "quic") {
            self.out.warn(line, format!("listen {address}"));
            return None;
        }
        let address: &str = address.strip_prefix("*:").unwrap_or(address);
        let address: String = match address.strip_prefix("localhost:") {
            Some(port) => format!("127.0.0.1:{port}"),
            None if address.parse::<u16>().is_ok() => format!("0.0.0.0:{address}"),
            None => output::with_port(address, 80),
        };
        Some(address)
    }

    /// One route for a prefix or exact `location`, with its rank: exact
    /// first, then longer prefixes.
    fn location(
        &mut self,
        location: &Directive,
        parent: &Context,
        server_name: Option<&String>,
    ) -> Option<(u8, Reverse<usize>, Table)> {
        let line: usize = location.line;
        let (exact, path): (bool, &String) = match location.args.as_slice() {
            [modifier, path] if modifier == "=" => (true, path),
            [modifier, path] if modifier == "^~" => (false, path),
            [path] if path.starts_with('/') => (false, path),
            args => {
                self.out.warn(
                    line,
                    format!("location {} (regex and named locations)", args.join(" ")),
                );
                return None;
            }
        };
        let mut context: Context = parent.clone();
        self.settings(location.children(), &mut context);

        let mut route = Table::new();
        let matcher = |route: &mut Table, mount: bool| {
            if exact {
                route.insert("path", toml_edit::value(path));
            } else if mount {
                route.insert("uri", toml_edit::value(path.trim_end_matches('/')));
            } else if path != "/" {
                route.insert("path", toml_edit::value(format!("{path}*")));
            }
        };
        let mut action: Option<&str> = None;
        for directive in location.children() {
            let line: usize = directive.line;
            if action.is_some()
                && ["proxy_pass", "return", "alias"].contains(&directive.name.as_str())
            {
                self.out
                    .warn(line, format!("{} after another action", directive.name));
                continue;
            }
            match (directive.name.as_str(), directive.args.as_slice()) {
                ("proxy_pass", [url]) => {
                    let forward: Item = self.proxy_pass(url, path, line)?;
                    matcher(&mut route, false);
                    route.insert("forward", forward);
                    action = Some("proxy_pass");
                }
                ("return", args) => {
                    matcher(&mut route, false);
                    if !self.return_action(args, line, server_name, &mut route) {
                        return None;
                    }
                    action = Some("return");
                }
                ("alias", [root]) => {
                    matcher(&mut route, !exact);
                    route.insert("serve", toml_edit::value(serve(root, &context)));
                    action = Some("alias");
                }
                ("location", _) => self.out.warn(line, "nested location"),
                ("proxy_connect_timeout" | "proxy_read_timeout" | "proxy_send_timeout", _) => {
                    self.out.warn(
                        line,
                        format!("{} (set timeouts on the pool)", directive.name),
                    );
                }
                (name, _) if self.is_setting(name) => {}
                (name, _) => self.out.warn(line, format!("directive {name}")),
            }
        }
        if action.is_none() {
            let Some(root) = &context.root else {
                self.out
                    .warn(line, format!("location {path} has nothing rxh can serve"));
                return None;
            };
            matcher(&mut route, false);
            route.insert("serve", toml_edit::value(serve(root, &context)));
        }
        let proxied: bool = action == Some("proxy_pass");
        let route: Table = self.finish(route, &context, proxied, line);
        let rank: u8 = if exact { 0 } else { 1 };
        Some((rank, Reverse(path.len()), route))
    }

    /// The `forward` value of `proxy_pass`.
    fn proxy_pass(&mut self, url: &str, location: &str, line: usize) -> Option<Item> {
        if url.contains('$') {
            self.out
                .warn(line, format!("proxy_pass {url} with variables"));
            return None;
        }
        let Some((scheme, rest)) = url.split_once("://") else {
            self.out.warn(line, format!("proxy_pass {url}"));
            return None;
        };
        let (authority, uri): (&str, Option<&str>) = match rest.find('/') {
            Some(slash) => (&rest[..slash], Some(&rest[slash..])),
            None => (rest, None),
        };
        if let Some(uri) = uri
            && !(uri == "/" && location == "/")
        {
            self.out.warn(
                line,
                format!("URI {uri} of proxy_pass: rxh forwards the request path unchanged"),
            );
        }
        if let Some(pool) = self.pools.get(authority) {
            if scheme == "https" {
                self.out.warn(
                    line,
                    format!("https to upstream {authority}: use https:// backends"),
                );
            }
            return Some(toml_edit::value(format!("@{pool}")));
        }
        let address: String = match scheme {
            "https" => format!("https://{authority}"),
            _ => output::with_port(authority, 80),
        };
        Some(toml_edit::value(address))
    }

    /// `return CODE URL` or `return URL` as a redirect; `false` (with a
    /// warning) for other responses.
    fn return_action(
        &mut self,
        args: &[String],
        line: usize,
        server_name: Option<&String>,
        route: &mut Table,
    ) -> bool {
        let (status, target): (i64, &String) = match args {
            [code, target] => match code.parse::<i64>() {
                Ok(status @ (301 | 302 | 303 | 307 | 308)) => (status, target),
                _ => {
                    self.out
                        .warn(line, format!("return {code} (only redirects)"));
                    return false;
                }
            },
            [target] if target.starts_with("http://") || target.starts_with("https://") => {
                (302, target)
            }
            _ => {
                self.out
                    .warn(line, format!("return {} (only redirects)", args.join(" ")));
                return false;
            }
        };
        let Some(target) = self.variables(target, line, server_name) else {
            return false;
        };
        let value: Value = if status == 302 {
            Value::from(target)
        } else {
            let mut redirect = InlineTable::new();
            redirect.insert("to", Value::from(target));
            redirect.insert("status", Value::from(status));
            Value::InlineTable(redirect)
        };
        route.insert("redirect", Item::Value(value));
        true
    }

    /// `text` with nginx variables as RXH placeholders, `None` (with a
    /// warning) when one has no equivalent.
    fn variables(
        &mut self,
        text: &str,
        line: usize,
        server_name: Option<&String>,
    ) -> Option<String> {
        let mut result = String::new();
        let mut rest: &str = text;
        while let Some(dollar) = rest.find('$') {
            result.push_str(&rest[..dollar]);
            let after: &str = &rest[dollar + 1..];
            let (name, length): (&str, usize) = match after.strip_prefix('{') {
                Some(braced) => match braced.find('}') {
                    Some(end) => (&braced[..end], end + 2),
                    None => (braced, braced.len() + 1),
                },
                None => {
                    let end: usize = after
                        .find(|c: char| !c.is_ascii_alphanumeric() && c != '_')
                        .unwrap_or(after.len());
                    (&after[..end], end)
                }
            };
            let replacement: String = match (name, server_name) {
                ("host" | "http_host", _) => String::from("{host}"),
                ("request_uri", _) => String::from("{uri}"),
                ("uri" | "document_uri", _) => String::from("{path}"),
                ("args" | "query_string", _) => String::from("{query}"),
                ("server_name", Some(server_name)) => server_name.clone(),
                _ => {
                    self.out.warn(line, format!("variable ${name} in {text}"));
                    return None;
                }
            };
            result.push_str(&replacement);
            rest = &after[length..];
        }
        result.push_str(rest);
        Some(result)
    }

    /// nginx sizes (`10m`, `512k`) as RXH sizes; `None` for `0`, unlimited.
    fn size(&mut self, size: &str, line: usize) -> Option<String> {
        let (number, unit): (&str, &str) = match size.char_indices().last() {
            Some((index, unit)) if unit.is_ascii_alphabetic() => (&size[..index], &size[index..]),
            _ => (size, ""),
        };
        let unit: &str = match unit.to_ascii_lowercase().as_str() {
            "" => "",
            "k" => "KiB",
            "m" => "MiB",
            "g" => "GiB",
            _ => {
                self.out.warn(line, format!("size {size}"));
                return None;
            }
        };
        match number.parse::<u64>() {
            Ok(0) => None,
            Ok(number) => Some(format!("{number}{unit}")),
            Err(_) => {
                self.out.warn(line, format!("size {size}"));
                None
            }
        }
    }

    /// Settings of the enclosing blocks that apply to every route.
    fn finish(&mut self, mut route: Table, context: &Context, proxied: bool, line: usize) -> Table {
        if context.gzip && !route.contains_key("redirect") {
            route.insert("encode", toml_edit::value(Array::from_iter(["gzip"])));
        }
        if let Some(size) = &context.max_body_size {
            let mut request = InlineTable::new();
            request.insert("max_body_size", Value::from(size.as_str()));
            route.insert("request", toml_edit::value(request));
        }
        if proxied {
            context.request.apply(&mut route, "request");
            context.response.apply(&mut route, "response");
        } else if !context.response.is_empty() {
            self.out.warn(
                line,
                "add_header on files and redirects (rxh sets response headers on proxied \
                 responses only)",
            );
        }
        route
    }

    /// The `stream` block: TCP and UDP servers.
    fn stream(&mut self, directives: &[Directive]) {
        for upstream in directives.iter().filter(|d| d.name == "upstream") {
            self.upstream(upstream);
        }
        for directive in directives {
            match directive.name.as_str() {
                "server" => self.stream_server(directive),
                "upstream" => {}
                name if IGNORED.contains(&name) => {}
                name => self
                    .out
                    .warn(directive.line, format!("stream directive {name}")),
            }
        }
    }

    fn stream_server(&mut self, server: &Directive) {
        let mut listens: Vec<(String, &str)> = Vec::new();
        let mut common = Table::new();
        let mut idle_timeout: Option<String> = None;
        let mut proxy_protocol_in = false;
        for directive in server.children() {
            let line: usize = directive.line;
            match (directive.name.as_str(), directive.args.as_slice()) {
                ("listen", [address, flags @ ..]) => {
                    let protocol: &str = if flags.iter().any(|flag| flag == "udp") {
                        "udp"
                    } else {
                        "tcp"
                    };
                    let flags: Vec<String> = flags
                        .iter()
                        .filter(|flag| {
                            let proxy_protocol: bool = *flag == "proxy_protocol";
                            proxy_protocol_in |= proxy_protocol;
                            !proxy_protocol
                        })
                        .cloned()
                        .collect();
                    if flags.iter().any(|flag| flag == "ssl") {
                        self.out
                            .warn(line, "listen ssl in stream (TLS is passed through)");
                    }
                    if let Some(listen) = self.listen(address, &flags, line) {
                        listens.push((listen, protocol));
                    }
                }
                ("proxy_pass", [target]) => {
                    let forward: String = match self.pools.get(target) {
                        Some(pool) => format!("@{pool}"),
                        None => output::with_port(target, 80),
                    };
                    common.insert("forward", toml_edit::value(forward));
                }
                ("proxy_protocol", [on]) if on == "on" => {
                    common.insert("proxy_protocol", toml_edit::value("v1"));
                }
                ("proxy_timeout", [timeout]) => idle_timeout = Some(duration(timeout)),
                (name, _) if IGNORED.contains(&name) => {}
                (name, _) => self.out.warn(line, format!("stream directive {name}")),
            }
        }
        if !common.contains_key("forward") || listens.is_empty() {
            self.out
                .warn(server.line, "stream server without listen or proxy_pass");
            return;
        }
        for protocol in ["tcp", "udp"] {
            let addresses: Vec<String> = listens
                .iter()
                .filter(|(_, listen_protocol)| *listen_protocol == protocol)
                .map(|(address, _)| address.clone())
                .collect();
            if addresses.is_empty() {
                continue;
            }
            let mut table = Table::new();
            table.insert("listen", output::one_or_many(&addresses));
            table.insert("protocol", toml_edit::value(protocol));
            for (key, item) in common.iter() {
                if protocol == "tcp" || key != "proxy_protocol" {
                    table.insert(key, item.clone());
                }
            }
            if proxy_protocol_in && protocol == "tcp" {
                table.insert("proxy_protocol_in", toml_edit::value(true));
            }
            if let Some(timeout) = &idle_timeout {
                let key: &str = if protocol == "udp" {
                    "udp_idle_timeout"
                } else {
                    "idle_timeout"
                };
                table.insert(key, toml_edit::value(timeout));
            }
            self.out.servers.push(Server {
                table,
                routes: Vec::new(),
            });
        }
    }
}

/// The file server table for `root`.
fn serve(root: &str, context: &Context) -> InlineTable {
    let mut serve = InlineTable::new();
    serve.insert("root", Value::from(root));
    serve.insert("browse", Value::from(context.browse));
    if !context.index.is_empty() {
        serve.insert("index", Value::Array(output::strings(&context.index)));
    }
    serve
}

/// nginx durations: a bare number is seconds.
fn duration(text: &str) -> String {
    if text.chars().all(|c| c.is_ascii_digit()) {
        format!("{text}s")
    } else {
        text.to_owned()
    }
}
