//! The RXH configuration an adapter builds, and its TOML text.

use toml_edit::{Array, ArrayOfTables, DocumentMut, InlineTable, Item, Table, Value};

/// Pools, servers and warnings collected while reading a foreign file.
#[derive(Default)]
pub(crate) struct Output {
    /// Top level keys, such as `grace_period`.
    pub top: Table,
    pub admin: Table,
    pub acme: Table,
    /// Named pools, `[upstream.NAME]`, in file order.
    pub pools: Vec<(String, Table)>,
    pub servers: Vec<Server>,
    /// What could not be converted, with the line it comes from.
    pub warnings: Vec<(usize, String)>,
}

/// One `[[server]]` with its `[[server.match]]` routes.
#[derive(Clone, Default)]
pub(crate) struct Server {
    pub table: Table,
    pub routes: Vec<Table>,
}

impl Output {
    pub fn warn(&mut self, line: usize, message: impl AsRef<str>) {
        self.warnings.push((line, message.as_ref().to_owned()));
    }

    /// `base`, or `base_2`, `base_3`... when a pool already has that name.
    pub fn pool_name(&self, base: &str) -> String {
        let base: String = base
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        let taken = |name: &str| self.pools.iter().any(|(pool, _)| pool == name);
        if !taken(&base) {
            return base;
        }
        let mut index: usize = 2;
        while taken(&format!("{base}_{index}")) {
            index += 1;
        }
        format!("{base}_{index}")
    }

    /// The TOML document, with the warnings as comments at the top.
    pub fn to_toml(&self, source: &str) -> String {
        let mut document = DocumentMut::new();
        for (key, item) in self.top.iter() {
            document.insert(key, item.clone());
        }
        for (key, table) in [("admin", &self.admin), ("acme", &self.acme)] {
            if !table.is_empty() {
                document.insert(key, Item::Table(table.clone()));
            }
        }
        if !self.pools.is_empty() {
            let mut upstream = Table::new();
            upstream.set_implicit(true);
            for (name, pool) in &self.pools {
                upstream.insert(name, Item::Table(pool.clone()));
            }
            document.insert("upstream", Item::Table(upstream));
        }
        let mut servers = ArrayOfTables::new();
        for server in &self.servers {
            let mut table: Table = server.table.clone();
            if !server.routes.is_empty() {
                let routes: ArrayOfTables = server.routes.iter().cloned().collect();
                table.insert("match", Item::ArrayOfTables(routes));
            }
            servers.push(table);
        }
        if !servers.is_empty() {
            document.insert("server", Item::ArrayOfTables(servers));
        }

        let mut text = format!("# Converted from {source} by rxh adapt.\n");
        for (line, warning) in &self.warnings {
            text.push_str(&format!("# Not converted: line {line}: {warning}\n"));
        }
        text.push('\n');
        text.push_str(&document.to_string());
        crate::fmt::format(&text).expect("toml_edit writes valid TOML")
    }
}

/// Header rules of one side of a route (`request.headers` or
/// `response.headers`).
#[derive(Clone, Default)]
pub(crate) struct Headers {
    pub set: Vec<(String, String)>,
    pub add: Vec<(String, String)>,
    pub remove: Vec<String>,
    pub replace: Vec<(String, String, String)>,
}

impl Headers {
    pub fn is_empty(&self) -> bool {
        self.set.is_empty()
            && self.add.is_empty()
            && self.remove.is_empty()
            && self.replace.is_empty()
    }

    /// Adds these rules to `route` as `side = { headers = { ... } }`.
    pub fn apply(&self, route: &mut Table, side: &str) {
        if self.is_empty() {
            return;
        }
        let mut headers = InlineTable::new();
        let pairs = |pairs: &[(String, String)]| -> InlineTable {
            pairs
                .iter()
                .map(|(name, value)| (name.as_str(), Value::from(value.as_str())))
                .collect()
        };
        if !self.remove.is_empty() {
            headers.insert("remove", Value::Array(strings(&self.remove)));
        }
        if !self.replace.is_empty() {
            let replace: InlineTable = self
                .replace
                .iter()
                .map(|(name, search, replace)| {
                    let rule: InlineTable = [("search", search), ("replace", replace)]
                        .into_iter()
                        .map(|(key, text)| (key, Value::from(text.as_str())))
                        .collect();
                    (name.as_str(), Value::InlineTable(rule))
                })
                .collect();
            headers.insert("replace", Value::InlineTable(replace));
        }
        if !self.set.is_empty() {
            headers.insert("set", Value::InlineTable(pairs(&self.set)));
        }
        if !self.add.is_empty() {
            headers.insert("add", Value::InlineTable(pairs(&self.add)));
        }
        let side = route
            .entry(side)
            .or_insert(toml_edit::value(InlineTable::new()))
            .as_inline_table_mut()
            .expect("request and response are inline tables");
        side.insert("headers", Value::InlineTable(headers));
    }
}

/// A string for one item, an array for several.
pub(crate) fn one_or_many(items: &[String]) -> Item {
    match items {
        [item] => toml_edit::value(item.as_str()),
        _ => toml_edit::value(strings(items)),
    }
}

pub(crate) fn strings(items: &[String]) -> Array {
    items.iter().map(String::as_str).collect()
}

/// `address:port` for an upstream address without a port.
pub(crate) fn with_port(address: &str, port: u16) -> String {
    let host_start: usize = address.rfind(']').unwrap_or(0);
    // A placeholder such as `{env.BACKEND}` may hold the port.
    if address.contains("://")
        || address.starts_with("unix/")
        || address.contains('{')
        || address[host_start..].contains(':')
    {
        address.to_owned()
    } else {
        format!("{address}:{port}")
    }
}
