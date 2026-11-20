//! Caddyfile to RXH: global options, site addresses, `reverse_proxy`,
//! `file_server`, `redir`, `handle`, `handle_path`, `route`, `root`,
//! `encode`, `header`, `basic_auth`, `tls` and named matchers.

use std::cmp::Reverse;
use std::collections::HashMap;

use toml_edit::{Array, InlineTable, Item, Table, Value};

use crate::adapt::directive::{self, Directive, Syntax};
use crate::adapt::output::{self, Headers, Output, Server};

/// Load balancing policies with the same name in Caddy and RXH.
const POLICIES: [&str; 12] = [
    "random",
    "random_choose",
    "first",
    "round_robin",
    "weighted_round_robin",
    "least_conn",
    "ip_hash",
    "client_ip_hash",
    "uri_hash",
    "query",
    "header",
    "cookie",
];

/// Caddy's `private_ranges` shorthand for `remote_ip` and `client_ip`.
const PRIVATE_RANGES: [&str; 7] = [
    "192.168.0.0/16",
    "172.16.0.0/12",
    "10.0.0.0/8",
    "127.0.0.1/8",
    "fd00::/8",
    "::1/128",
    "fc00::/7",
];

/// Adds the sites of a Caddyfile to `out`.
pub(crate) fn convert(text: &str, out: &mut Output) -> Result<(), String> {
    let mut directives: Vec<Directive> = directive::parse(text, Syntax::Caddyfile)?;
    env_placeholders(&mut directives, out);
    let mut adapter = Adapter {
        out,
        http_port: 80,
        https_port: 443,
        auto_https: AutoHttps::On,
    };
    if directives
        .first()
        .is_some_and(|first| first.name.is_empty())
    {
        let global: Directive = directives.remove(0);
        adapter.global(global.children());
    }

    // A file with one site may leave out the braces around it.
    if directives
        .first()
        .is_some_and(|first| first.block.is_none())
    {
        let mut directives = directives.into_iter();
        let addresses: Directive = directives.next().expect("checked above");
        let body: Vec<Directive> = directives.collect();
        adapter.site(&addresses, &body);
        return Ok(());
    }
    for site in &directives {
        match &site.block {
            _ if site.name.starts_with('(') => {
                adapter
                    .out
                    .warn(site.line, format!("snippet {} (inline it)", site.name));
            }
            Some(body) => adapter.site(site, body),
            None => adapter
                .out
                .warn(site.line, format!("{} outside of a site block", site.name)),
        }
    }
    Ok(())
}

/// Replaces `{$NAME}` and `{$NAME:default}` with RXH's `{env.NAME}`.
fn env_placeholders(directives: &mut [Directive], out: &mut Output) {
    for directive in directives {
        for word in std::iter::once(&mut directive.name).chain(directive.args.iter_mut()) {
            let mut result = String::new();
            let mut rest: &str = word;
            while let Some(start) = rest.find("{$") {
                let Some(length) = rest[start..].find('}') else {
                    break;
                };
                let inner: &str = &rest[start + 2..start + length];
                let name: &str = match inner.split_once(':') {
                    Some((name, _)) => {
                        out.warn(directive.line, format!("default value of {{${inner}}}"));
                        name
                    }
                    None => inner,
                };
                result.push_str(&rest[..start]);
                result.push_str(&format!("{{env.{name}}}"));
                rest = &rest[start + length + 1..];
            }
            result.push_str(rest);
            *word = result;
        }
        if let Some(block) = &mut directive.block {
            env_placeholders(block, out);
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum AutoHttps {
    On,
    Off,
    DisableRedirects,
}

struct Adapter<'a> {
    out: &'a mut Output,
    http_port: u16,
    https_port: u16,
    auto_https: AutoHttps,
}

/// Settings a block passes down to the blocks inside it.
#[derive(Clone, Default)]
struct Context {
    /// Matchers of the enclosing `handle` blocks.
    matcher: Table,
    /// Path pattern of an enclosing `handle_path`.
    strip: Option<String>,
    root: Option<String>,
    encode: Vec<String>,
    response: Headers,
    basic_auth: Option<InlineTable>,
    /// Named matchers; `None` for those using an unsupported matcher.
    named: HashMap<String, Option<Table>>,
    /// Base of the names of the pools made for this site.
    pool_base: String,
}

/// A site address such as `example.com`, `http://:8080` or `localhost:3000`.
struct Address {
    listen: String,
    host: Option<String>,
    tls: bool,
}

/// What kind of route a directive makes, for the matcher of `handle_path`.
#[derive(Clone, Copy, PartialEq)]
enum Action {
    Forward,
    Serve,
    Redirect,
}

impl Adapter<'_> {
    fn global(&mut self, options: &[Directive]) {
        for option in options {
            let line: usize = option.line;
            match (option.name.as_str(), option.args.as_slice()) {
                ("email", [email]) => {
                    self.out.acme.insert("email", toml_edit::value(email));
                }
                ("acme_ca", [ca]) => {
                    self.out.acme.insert("ca", toml_edit::value(ca));
                }
                ("admin", [off]) if off == "off" => {
                    self.out.admin.insert("enabled", toml_edit::value(false));
                }
                ("admin", [address]) => {
                    if option.block.is_some() {
                        self.out.warn(line, "options of admin");
                    }
                    let listen: String = listen_address(address);
                    self.out.admin.insert("listen", toml_edit::value(listen));
                }
                ("http_port", [port]) => {
                    if let Some(port) = self.port(port, line) {
                        self.http_port = port;
                        self.out
                            .acme
                            .insert("http_port", toml_edit::value(i64::from(port)));
                    }
                }
                ("https_port", [port]) => {
                    if let Some(port) = self.port(port, line) {
                        self.https_port = port;
                    }
                }
                ("auto_https", [mode]) if mode == "off" => self.auto_https = AutoHttps::Off,
                ("auto_https", [mode]) if mode == "disable_redirects" => {
                    self.auto_https = AutoHttps::DisableRedirects;
                }
                ("grace_period", [duration]) => {
                    self.out
                        .top
                        .insert("grace_period", toml_edit::value(duration));
                }
                ("debug", []) => {}
                (name, _) => self.out.warn(line, format!("global option {name}")),
            }
        }
    }

    fn port(&mut self, text: &str, line: usize) -> Option<u16> {
        let port: Option<u16> = text.parse().ok();
        if port.is_none() {
            self.out.warn(line, format!("invalid port {text}"));
        }
        port
    }

    fn site(&mut self, head: &Directive, body: &[Directive]) {
        let words: Vec<&str> = std::iter::once(&head.name)
            .chain(&head.args)
            .flat_map(|word| word.split(','))
            .filter(|word| !word.is_empty())
            .collect();
        let addresses: Vec<Address> = words
            .iter()
            .filter_map(|word| self.address(word, head.line))
            .collect();

        let mut tls_files: Option<InlineTable> = None;
        let mut internal = false;
        for directive in body.iter().filter(|directive| directive.name == "tls") {
            if directive.block.is_some() {
                self.out.warn(directive.line, "options in the tls block");
            }
            match directive.args.as_slice() {
                [email] if email.contains('@') => {
                    if !self.out.acme.contains_key("email") {
                        self.out.acme.insert("email", toml_edit::value(email));
                    }
                }
                [internal_ca] if internal_ca == "internal" => internal = true,
                [cert, key] => {
                    let files: InlineTable = [("cert", cert), ("key", key)]
                        .into_iter()
                        .map(|(key, path)| (key, Value::from(path.as_str())))
                        .collect();
                    tls_files = Some(files);
                }
                _ => self.out.warn(directive.line, "tls arguments"),
            }
        }

        let context = Context {
            pool_base: addresses
                .iter()
                .find_map(|address| address.host.clone())
                .unwrap_or_else(|| String::from("proxy")),
            ..Context::default()
        };
        let routes: Vec<Table> = self.routes(body, &context);
        if routes.is_empty() {
            self.out.warn(
                head.line,
                format!("site {} has nothing rxh can serve", words.join(" ")),
            );
            return;
        }

        // One server per listen address and TLS setting, like Caddy's.
        let mut servers: Vec<(String, bool, Vec<String>)> = Vec::new();
        for address in addresses {
            let mut tls: bool = address.tls;
            if tls && tls_files.is_none() && (internal || address.host.is_none()) {
                self.out.warn(
                    head.line,
                    format!(
                        "HTTPS with Caddy's internal CA on {}: served over plain HTTP",
                        address.listen
                    ),
                );
                tls = false;
            }
            let hosts: Vec<String> = address.host.into_iter().collect();
            match servers
                .iter_mut()
                .find(|(listen, server_tls, _)| *listen == address.listen && *server_tls == tls)
            {
                Some((_, _, server_hosts)) => server_hosts.extend(hosts),
                None => servers.push((address.listen, tls, hosts)),
            }
        }
        for (listen, tls, hosts) in servers {
            let mut table = Table::new();
            table.insert("listen", toml_edit::value(listen));
            if !hosts.is_empty() {
                table.insert("host", output::one_or_many(&hosts));
            }
            if tls {
                match &tls_files {
                    Some(files) => table.insert("tls", toml_edit::value(files.clone())),
                    None => table.insert("tls", toml_edit::value("auto")),
                };
                if tls_files.is_none() && self.auto_https == AutoHttps::DisableRedirects {
                    table.insert("https_redirect", toml_edit::value(false));
                }
            }
            self.out.servers.push(Server {
                table,
                routes: routes.clone(),
            });
        }
    }

    fn address(&mut self, text: &str, line: usize) -> Option<Address> {
        let (scheme, rest): (Option<&str>, &str) = match text.split_once("://") {
            Some((scheme, rest)) => (Some(scheme), rest),
            None => (None, text),
        };
        let authority: &str = match rest.split_once('/') {
            Some((authority, path)) => {
                self.out.warn(
                    line,
                    format!("path /{path} of site address {text} (whole site used)"),
                );
                authority
            }
            None => rest,
        };
        let host_end: usize = authority.rfind(']').unwrap_or(0);
        let (host, port): (&str, Option<u16>) = match authority[host_end..].rfind(':') {
            Some(colon) => {
                let port: u16 = self.port(&authority[host_end + colon + 1..], line)?;
                (&authority[..host_end + colon], Some(port))
            }
            None => (authority, None),
        };
        let tls: bool = match scheme {
            Some("http") => false,
            Some("https") => true,
            Some(other) => {
                self.out
                    .warn(line, format!("scheme {other} of site address {text}"));
                return None;
            }
            None => {
                self.auto_https != AutoHttps::Off
                    && !host.is_empty()
                    && port != Some(self.http_port)
            }
        };
        let port: u16 = port.unwrap_or(if tls { self.https_port } else { self.http_port });
        let ip: &str = host.trim_start_matches('[').trim_end_matches(']');
        let (listen, host): (String, Option<String>) = if host.is_empty() {
            (format!("0.0.0.0:{port}"), None)
        } else if host == "localhost" {
            (format!("127.0.0.1:{port}"), None)
        } else if ip.parse::<std::net::IpAddr>().is_ok() {
            (format!("{host}:{port}"), None)
        } else {
            (format!("0.0.0.0:{port}"), Some(host.to_owned()))
        };
        Some(Address { listen, host, tls })
    }

    /// The routes of a block, ordered like Caddy orders its directives:
    /// `redir`, then `handle` blocks, then `reverse_proxy`, then
    /// `file_server`, longer paths first among the same directive.
    fn routes(&mut self, body: &[Directive], parent: &Context) -> Vec<Table> {
        let mut context: Context = parent.clone();
        for directive in body {
            self.setting(directive, &mut context);
        }

        let mut ranked: Vec<(u8, usize, Vec<Table>)> = Vec::new();
        for directive in body {
            let rank: u8 = match directive.name.as_str() {
                "redir" => 0,
                "handle" | "handle_path" | "route" => 1,
                "reverse_proxy" => 2,
                "file_server" => 3,
                _ => continue,
            };
            let routes: Vec<Table> = match rank {
                0 => self.redirect(directive, &context).into_iter().collect(),
                1 => self.handle(directive, &context),
                2 => self
                    .reverse_proxy(directive, &context)
                    .into_iter()
                    .collect(),
                _ => self.file_server(directive, &context).into_iter().collect(),
            };
            let specificity: usize = directive
                .args
                .first()
                .filter(|arg| arg.starts_with('/'))
                .map_or(0, String::len);
            ranked.push((rank, specificity, routes));
        }
        ranked.sort_by_key(|(rank, specificity, _)| (*rank, Reverse(*specificity)));
        ranked
            .into_iter()
            .flat_map(|(_, _, routes)| routes)
            .collect()
    }

    /// Directives that change the routes of their block instead of making one.
    fn setting(&mut self, directive: &Directive, context: &mut Context) {
        let line: usize = directive.line;
        match directive.name.as_str() {
            name if name.starts_with('@') => self.define(directive, context),
            "root" => match self.plain_args(directive, context) {
                Some([root]) => context.root = Some(root.clone()),
                Some(_) => self.out.warn(line, "root arguments"),
                None => {}
            },
            "encode" => {
                let Some(args) = self.plain_args(directive, context) else {
                    return;
                };
                let formats = args
                    .iter()
                    .chain(directive.children().iter().map(|child| &child.name));
                let mut encode: Vec<String> = Vec::new();
                for format in formats {
                    match format.as_str() {
                        "zstd" | "gzip" | "br" => encode.push(format.clone()),
                        other => self.out.warn(line, format!("encode {other}")),
                    }
                }
                if encode.is_empty() {
                    encode = vec![String::from("zstd"), String::from("gzip")];
                }
                context.encode = encode;
            }
            "header" => {
                let Some(args) = self.plain_args(directive, context) else {
                    return;
                };
                let mut response: Headers = context.response.clone();
                if !args.is_empty() {
                    self.header_rule(&mut response, args, line);
                }
                for child in directive.children() {
                    let args: Vec<String> = std::iter::once(&child.name)
                        .chain(&child.args)
                        .cloned()
                        .collect();
                    self.header_rule(&mut response, &args, child.line);
                }
                context.response = response;
            }
            "basic_auth" | "basicauth" => self.basic_auth(directive, context),
            "tls" | "log" | "redir" | "handle" | "handle_path" | "route" | "reverse_proxy"
            | "file_server" => {}
            name => self.out.warn(line, format!("directive {name}")),
        }
    }

    /// The arguments of a site wide setting, `None` (with a warning) when it
    /// has a matcher other than `*`.
    fn plain_args<'d>(
        &mut self,
        directive: &'d Directive,
        context: &Context,
    ) -> Option<&'d [String]> {
        let (matcher, args) = self.matcher(directive, context)?;
        if !matcher.is_empty() {
            self.out.warn(
                directive.line,
                format!(
                    "{} with a matcher (only site wide is supported)",
                    directive.name
                ),
            );
            return None;
        }
        Some(args)
    }

    fn basic_auth(&mut self, directive: &Directive, context: &mut Context) {
        let Some(args) = self.plain_args(directive, context) else {
            return;
        };
        let args: &[String] = match args {
            [algorithm, rest @ ..] if algorithm == "bcrypt" => rest,
            args => args,
        };
        let mut users = InlineTable::new();
        for user in directive.children() {
            match user.args.as_slice() {
                [hash] => {
                    if !hash.starts_with("$2") {
                        self.out.warn(
                            user.line,
                            format!("password of {} is not a $2 bcrypt hash", user.name),
                        );
                    }
                    users.insert(&user.name, Value::from(hash.as_str()));
                }
                _ => self.out.warn(user.line, "basic_auth user line"),
            }
        }
        let mut auth = InlineTable::new();
        if let [realm] = args {
            auth.insert("realm", Value::from(realm.as_str()));
        }
        auth.insert("users", Value::InlineTable(users));
        context.basic_auth = Some(auth);
    }

    /// A named matcher definition such as `@api path /api/*` or a block.
    fn define(&mut self, directive: &Directive, context: &mut Context) {
        let lines: Vec<(&str, &[String], usize)> = match directive.args.split_first() {
            Some((kind, values)) => vec![(kind.as_str(), values, directive.line)],
            None => directive
                .children()
                .iter()
                .map(|child| (child.name.as_str(), child.args.as_slice(), child.line))
                .collect(),
        };
        let mut matcher = Table::new();
        let mut supported = true;
        for (kind, values, line) in lines {
            match (kind, values) {
                ("path" | "method" | "host", values) => {
                    let values: Vec<String> = values
                        .iter()
                        .map(|value| match kind {
                            "method" => value.to_ascii_uppercase(),
                            _ => value.clone(),
                        })
                        .collect();
                    extend(&mut matcher, kind, &values);
                }
                ("remote_ip" | "client_ip", values) => {
                    let ranges: Vec<String> = values
                        .iter()
                        .flat_map(|value| match value.as_str() {
                            "private_ranges" => PRIVATE_RANGES.map(String::from).to_vec(),
                            _ => vec![value.clone()],
                        })
                        .collect();
                    extend(&mut matcher, "remote_ip", &ranges);
                }
                ("header" | "query", values) => {
                    let pairs: Vec<(String, String)> = match (kind, values) {
                        ("header", [name]) => vec![(name.clone(), String::from("*"))],
                        ("header", [name, value]) => vec![(name.clone(), value.clone())],
                        ("query", pairs) => pairs
                            .iter()
                            .map(|pair| match pair.split_once('=') {
                                Some((key, value)) => (key.to_owned(), value.to_owned()),
                                None => (pair.clone(), String::from("*")),
                            })
                            .collect(),
                        _ => {
                            self.out
                                .warn(line, format!("header matcher of {}", directive.name));
                            supported = false;
                            continue;
                        }
                    };
                    let table = matcher
                        .entry(kind)
                        .or_insert(toml_edit::value(InlineTable::new()))
                        .as_inline_table_mut()
                        .expect("header and query are inline tables");
                    for (key, value) in pairs {
                        table.insert(&key, Value::from(value));
                    }
                }
                (kind, _) => {
                    self.out.warn(
                        line,
                        format!(
                            "{kind} matcher of {}: the directives using it are left out",
                            directive.name
                        ),
                    );
                    supported = false;
                }
            }
        }
        context
            .named
            .insert(directive.name.clone(), supported.then_some(matcher));
    }

    /// The matcher in front of a directive's arguments and the arguments
    /// after it; `None` (with a warning) for an unusable named matcher.
    fn matcher<'d>(
        &mut self,
        directive: &'d Directive,
        context: &Context,
    ) -> Option<(Table, &'d [String])> {
        let args: &[String] = &directive.args;
        let mut matcher = Table::new();
        match args.first() {
            Some(first) if first == "*" => {}
            Some(first) if first.starts_with('/') => {
                matcher.insert("path", toml_edit::value(Array::from_iter([first.as_str()])));
            }
            Some(first) if first.starts_with('@') => match context.named.get(first) {
                Some(Some(named)) => matcher = named.clone(),
                Some(None) => return None,
                None => {
                    self.out
                        .warn(directive.line, format!("undefined matcher {first}"));
                    return None;
                }
            },
            _ => return Some((matcher, args)),
        }
        Some((matcher, &args[1..]))
    }

    /// The matchers of the route a directive makes, combined with those of
    /// its blocks, and the arguments after the matcher.
    fn start<'d>(
        &mut self,
        directive: &'d Directive,
        context: &Context,
        action: Action,
    ) -> Option<(Table, &'d [String])> {
        let (matcher, args) = self.matcher(directive, context)?;
        let mut route: Table = context.matcher.clone();
        let add = |route: &mut Table, key: &str, item: Item| -> bool {
            if route.contains_key(key) {
                return false;
            }
            route.insert(key, item);
            true
        };
        for (key, item) in matcher {
            if !add(&mut route, &key, item) {
                self.out.warn(
                    directive.line,
                    format!(
                        "{} with a {key} matcher inside a block with one",
                        directive.name
                    ),
                );
                return None;
            }
        }
        if let Some(pattern) = &context.strip {
            let (key, value): (&str, Item) = match action {
                Action::Serve => ("uri", toml_edit::value(strip_prefix(pattern))),
                _ => (
                    "path",
                    toml_edit::value(Array::from_iter([pattern.as_str()])),
                ),
            };
            if !add(&mut route, key, value) {
                self.out.warn(
                    directive.line,
                    format!("{} with a path matcher inside handle_path", directive.name),
                );
                return None;
            }
            if action == Action::Forward {
                self.out.warn(
                    directive.line,
                    format!("prefix of handle_path {pattern}: requests are forwarded unchanged"),
                );
            }
        }
        Some((route, args))
    }

    /// Settings of the block that apply to every route.
    fn finish(
        &mut self,
        mut route: Table,
        context: &Context,
        action: Action,
        line: usize,
    ) -> Table {
        if action != Action::Redirect && !context.encode.is_empty() {
            route.insert("encode", toml_edit::value(output::strings(&context.encode)));
        }
        if let Some(auth) = &context.basic_auth {
            route.insert("basic_auth", toml_edit::value(auth.clone()));
        }
        if action == Action::Forward {
            context.response.apply(&mut route, "response");
        } else if !context.response.is_empty() {
            self.out.warn(
                line,
                "header on files and redirects (rxh sets response headers on proxied responses only)",
            );
        }
        route
    }

    fn handle(&mut self, directive: &Directive, context: &Context) -> Vec<Table> {
        let Some((matcher, args)) = self.matcher(directive, context) else {
            return Vec::new();
        };
        if !args.is_empty() {
            self.out
                .warn(directive.line, format!("arguments of {}", directive.name));
        }
        let mut inner: Context = context.clone();
        let strip: Option<&String> = match (directive.name.as_str(), directive.args.first()) {
            ("handle_path", Some(pattern)) if pattern.starts_with('/') => Some(pattern),
            _ => None,
        };
        let mut matcher: Table = matcher;
        if let Some(pattern) = strip {
            matcher.remove("path");
            inner.strip = Some(pattern.clone());
        } else if directive.name == "handle_path" {
            self.out.warn(
                directive.line,
                "handle_path without one path pattern: the prefix is not stripped",
            );
        }
        for (key, item) in matcher {
            if inner.matcher.contains_key(&key) {
                self.out.warn(
                    directive.line,
                    format!(
                        "{} with a {key} matcher inside a block with one",
                        directive.name
                    ),
                );
                return Vec::new();
            }
            inner.matcher.insert(&key, item);
        }
        self.routes(directive.children(), &inner)
    }

    fn redirect(&mut self, directive: &Directive, context: &Context) -> Option<Table> {
        let (mut route, args) = self.start(directive, context, Action::Redirect)?;
        let (to, code): (&String, Option<&str>) = match args {
            [to] => (to, None),
            [to, code] => (to, Some(code.as_str())),
            _ => {
                self.out.warn(directive.line, "redir arguments");
                return None;
            }
        };
        let status: i64 = match code {
            None | Some("temporary") => 302,
            Some("permanent") => 301,
            Some(code) => match code.parse::<i64>() {
                Ok(status @ (301 | 302 | 303 | 307 | 308)) => status,
                _ => {
                    self.out
                        .warn(directive.line, format!("redirect status {code}"));
                    return None;
                }
            },
        };
        let to: String = request_placeholders(to);
        let target: Value = if status == 302 {
            Value::from(to)
        } else {
            let mut target = InlineTable::new();
            target.insert("to", Value::from(to));
            target.insert("status", Value::from(status));
            Value::InlineTable(target)
        };
        route.insert("redirect", Item::Value(target));
        Some(self.finish(route, context, Action::Redirect, directive.line))
    }

    fn file_server(&mut self, directive: &Directive, context: &Context) -> Option<Table> {
        let (mut route, args) = self.start(directive, context, Action::Serve)?;
        let mut browse: bool = match args {
            [] => false,
            [browse] if browse == "browse" => true,
            _ => {
                self.out.warn(directive.line, "file_server arguments");
                false
            }
        };
        let mut serve = InlineTable::new();
        let mut root: String = context.root.clone().unwrap_or_else(|| String::from("."));
        let mut lists: Vec<(&str, Array)> = Vec::new();
        for option in directive.children() {
            match (option.name.as_str(), option.args.as_slice()) {
                ("root", [path]) => root = path.clone(),
                ("browse", _) => browse = true,
                ("index" | "hide" | "precompressed", values) => {
                    lists.push((option.name.as_str(), output::strings(values)));
                }
                (name, _) => self
                    .out
                    .warn(option.line, format!("file_server option {name}")),
            }
        }
        serve.insert("root", Value::from(root));
        serve.insert("browse", Value::from(browse));
        for (key, list) in lists {
            serve.insert(key, Value::Array(list));
        }
        route.insert("serve", toml_edit::value(serve));
        Some(self.finish(route, context, Action::Serve, directive.line))
    }

    fn reverse_proxy(&mut self, directive: &Directive, context: &Context) -> Option<Table> {
        let (mut route, args) = self.start(directive, context, Action::Forward)?;
        let mut pool = Table::new();
        let mut upstreams: Vec<String> = Vec::new();
        let mut weights: Vec<i64> = Vec::new();
        let mut health = Table::new();
        let mut passive = Table::new();
        let mut request = Headers::default();
        let mut response: Headers = context.response.clone();
        let mut context: Context = context.clone();
        context.response = Headers::default();

        self.upstreams(args, &mut upstreams, &mut pool);
        for option in directive.children() {
            let line: usize = option.line;
            let args: &[String] = &option.args;
            match (option.name.as_str(), args) {
                ("to", args) => self.upstreams(args, &mut upstreams, &mut pool),
                ("lb_policy", [policy, rest @ ..]) => {
                    self.policy(policy, rest, option, &mut pool, &mut weights);
                }
                ("lb_retries" | "health_port" | "max_fails", [number]) => {
                    let Ok(number) = number.parse::<i64>() else {
                        self.out.warn(line, format!("{} {number}", option.name));
                        continue;
                    };
                    let (table, key) = match option.name.as_str() {
                        "lb_retries" => (&mut pool, "retries"),
                        "health_port" => (&mut health, "port"),
                        _ => (&mut passive, "max_fails"),
                    };
                    table.insert(key, toml_edit::value(number));
                }
                ("lb_try_duration" | "lb_try_interval", [duration]) => {
                    let key: &str = option.name.trim_start_matches("lb_");
                    pool.insert(key, toml_edit::value(duration));
                }
                ("flush_interval", [duration]) => {
                    if duration != "-1" {
                        pool.insert("flush_interval", toml_edit::value(duration));
                    }
                }
                ("health_uri" | "health_interval" | "health_timeout" | "health_body", [text]) => {
                    let key: &str = option.name.trim_start_matches("health_");
                    health.insert(key, toml_edit::value(text));
                }
                ("health_status", [status]) => {
                    let status: Item = match status.parse::<i64>() {
                        Ok(code) => toml_edit::value(code),
                        Err(_) => toml_edit::value(status),
                    };
                    health.insert("status", status);
                }
                ("fail_duration" | "unhealthy_latency", [duration]) => {
                    passive.insert(&option.name, toml_edit::value(duration));
                }
                ("unhealthy_status", codes) => {
                    passive.insert("unhealthy_status", toml_edit::value(output::strings(codes)));
                }
                ("header_up", args) => self.header_rule(&mut request, args, line),
                ("header_down", args) => self.header_rule(&mut response, args, line),
                ("transport", [http]) if http == "http" => self.transport(option, &mut pool),
                (name, _) => self.out.warn(line, format!("reverse_proxy option {name}")),
            }
        }
        if upstreams.is_empty() {
            self.out
                .warn(directive.line, "reverse_proxy without upstreams");
            return None;
        }
        if !health.is_empty() {
            pool.insert("health", Item::Table(health));
        }
        if !passive.is_empty() {
            pool.insert("passive", Item::Table(passive));
        }

        let forward: Item = if pool.is_empty() {
            output::one_or_many(&upstreams)
        } else {
            let backends: Array = if weights.len() == upstreams.len() {
                upstreams
                    .iter()
                    .zip(&weights)
                    .map(|(address, weight)| {
                        let mut backend = InlineTable::new();
                        backend.insert("address", Value::from(address.as_str()));
                        backend.insert("weight", Value::from(*weight));
                        Value::InlineTable(backend)
                    })
                    .collect()
            } else {
                if !weights.is_empty() {
                    self.out
                        .warn(directive.line, "weights not matching the upstreams");
                }
                output::strings(&upstreams)
            };
            pool.insert("backends", toml_edit::value(backends));
            pool.sort_values_by(|left, _, right, _| key_rank(left).cmp(&key_rank(right)));
            let name: String = self.out.pool_name(&context.pool_base);
            self.out.pools.push((name.clone(), pool));
            toml_edit::value(format!("@{name}"))
        };
        route.insert("forward", forward);
        request.apply(&mut route, "request");
        response.apply(&mut route, "response");
        Some(self.finish(route, &context, Action::Forward, directive.line))
    }

    fn upstreams(&mut self, args: &[String], upstreams: &mut Vec<String>, pool: &mut Table) {
        for address in args {
            let address: &str = match address.strip_prefix("h2c://") {
                Some(rest) => {
                    pool.insert("h2c", toml_edit::value(true));
                    rest
                }
                None => address,
            };
            let address: String = match address.strip_prefix(':') {
                Some(port) => format!("127.0.0.1:{port}"),
                None => output::with_port(address, 80),
            };
            upstreams.push(address);
        }
    }

    fn policy(
        &mut self,
        policy: &str,
        args: &[String],
        option: &Directive,
        pool: &mut Table,
        weights: &mut Vec<i64>,
    ) {
        if !POLICIES.contains(&policy) {
            self.out.warn(option.line, format!("lb_policy {policy}"));
            return;
        }
        pool.insert("policy", toml_edit::value(policy));
        match (policy, args) {
            (_, []) => {}
            ("random_choose", [choose]) => match choose.parse::<i64>() {
                Ok(choose) => {
                    pool.insert("choose", toml_edit::value(choose));
                }
                Err(_) => self.out.warn(option.line, "random_choose size"),
            },
            ("query" | "header" | "cookie", [key]) => {
                pool.insert("key", toml_edit::value(key));
            }
            ("cookie", [key, secret]) => {
                pool.insert("key", toml_edit::value(key));
                pool.insert("secret", toml_edit::value(secret));
            }
            ("weighted_round_robin", values) => {
                for weight in values {
                    match weight.parse::<i64>() {
                        Ok(weight) => weights.push(weight),
                        Err(_) => self.out.warn(option.line, format!("weight {weight}")),
                    }
                }
            }
            _ => self
                .out
                .warn(option.line, format!("arguments of lb_policy {policy}")),
        }
        for child in option.children() {
            match (child.name.as_str(), child.args.as_slice()) {
                ("fallback", [fallback]) if POLICIES.contains(&fallback.as_str()) => {
                    pool.insert("fallback", toml_edit::value(fallback));
                }
                (name, _) => self
                    .out
                    .warn(child.line, format!("lb_policy option {name}")),
            }
        }
    }

    fn transport(&mut self, transport: &Directive, pool: &mut Table) {
        for option in transport.children() {
            match (option.name.as_str(), option.args.as_slice()) {
                (
                    "dial_timeout"
                    | "read_timeout"
                    | "write_timeout"
                    | "response_header_timeout"
                    | "keepalive",
                    [duration],
                ) => {
                    pool.insert(&option.name, toml_edit::value(duration));
                }
                ("tls", []) => {
                    pool.entry("tls").or_insert(Item::Table(Table::new()));
                }
                ("tls_insecure_skip_verify", []) => {
                    let tls = pool
                        .entry("tls")
                        .or_insert(Item::Table(Table::new()))
                        .as_table_mut()
                        .expect("tls is a table");
                    tls.insert("insecure_skip_verify", toml_edit::value(true));
                }
                ("versions", versions) if versions.iter().any(|version| version == "h2c") => {
                    pool.insert("h2c", toml_edit::value(true));
                }
                (name, _) => self
                    .out
                    .warn(option.line, format!("transport option {name}")),
            }
        }
    }

    /// `Name value` (set), `+Name value` (add), `-Name` (remove) or
    /// `Name search replace`.
    fn header_rule(&mut self, headers: &mut Headers, args: &[String], line: usize) {
        if args
            .iter()
            .skip(1)
            .any(|value| value.contains('{') && !value.contains("{env."))
        {
            self.out.warn(
                line,
                "placeholder in a header value (rxh sends it as written)",
            );
        }
        match args {
            [name] if name.starts_with('-') => headers.remove.push(name[1..].to_owned()),
            [name, value] => match name.strip_prefix('+') {
                Some(name) => headers.add.push((name.to_owned(), value.clone())),
                None => headers.set.push((name.clone(), value.clone())),
            },
            [name, search, replace] => {
                headers
                    .replace
                    .push((name.clone(), search.clone(), replace.clone()));
            }
            _ => self.out.warn(line, "header arguments"),
        }
    }
}

/// Order of the keys of a pool table: policy and backends first.
fn key_rank(key: &toml_edit::Key) -> usize {
    ["policy", "backends"]
        .iter()
        .position(|first| *first == key.get())
        .unwrap_or(2)
}

/// Appends `values` to the array `key` of a matcher table.
fn extend(matcher: &mut Table, key: &str, values: &[String]) {
    let array = matcher
        .entry(key)
        .or_insert(toml_edit::value(Array::new()))
        .as_array_mut()
        .expect("matcher lists are arrays");
    array.extend(values.iter().map(String::as_str));
}

/// `/static` for the `handle_path /static/*` pattern.
fn strip_prefix(pattern: &str) -> String {
    let prefix: &str = pattern.trim_end_matches('*').trim_end_matches('/');
    if prefix.is_empty() {
        String::from("/")
    } else {
        prefix.to_owned()
    }
}

/// Caddy's long request placeholders as the short ones RXH knows.
fn request_placeholders(text: &str) -> String {
    [
        ("{http.request.uri.path}", "{path}"),
        ("{http.request.uri.query}", "{query}"),
        ("{http.request.uri}", "{uri}"),
        ("{http.request.host}", "{host}"),
    ]
    .iter()
    .fold(text.to_owned(), |text, (long, short)| {
        text.replace(long, short)
    })
}

/// An admin address: `localhost` is loopback, a missing host is every
/// interface.
fn listen_address(address: &str) -> String {
    if let Some(port) = address.strip_prefix("localhost:") {
        format!("127.0.0.1:{port}")
    } else if let Some(port) = address.strip_prefix(':') {
        format!("0.0.0.0:{port}")
    } else {
        address.to_owned()
    }
}
