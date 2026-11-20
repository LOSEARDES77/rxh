//! The tables of a configuration file as entities: every `[[server]]`, every
//! `[upstream.NAME]` and the named sections, read as JSON and written back
//! into the `toml_edit` document without losing the comments of what did not
//! change.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Number, Value as Json};
use toml_edit::{
    Array, ArrayOfTables, DocumentMut, InlineTable, Item, Key, Table, TableLike, TomlError, Value,
};

/// What an entity is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Kind {
    /// One `[[server]]`.
    Server,
    /// One `[upstream.NAME]`.
    Upstream,
    /// One of [`SECTIONS`].
    Section,
}

impl Kind {
    /// The kind named by a path segment of the admin API.
    pub(crate) fn from_segment(segment: &str) -> Option<Self> {
        match segment {
            "server" => Some(Self::Server),
            "upstream" => Some(Self::Upstream),
            "section" => Some(Self::Section),
            _ => None,
        }
    }
}

/// One table of a configuration file.
#[derive(Debug, Serialize)]
pub(crate) struct Entity {
    pub(crate) kind: Kind,
    /// Pool name, section name, or `FILE:POSITION` for a server.
    pub(crate) key: String,
    /// Index of the file that defines it.
    pub(crate) file: usize,
    pub(crate) value: Json,
    /// Text of the table without its header, sub-tables included.
    pub(crate) toml: String,
}

/// Content of a new entity.
pub(crate) enum Body<'a> {
    Value(&'a Map<String, Json>),
    Toml(&'a str),
}

/// Sections that are tables, in display order; "top" is the bare keys of the root.
pub(crate) const SECTIONS: [&str; 10] = [
    "top",
    "admin",
    "log",
    "tracing",
    "acme",
    "docker",
    "consul",
    "etcd",
    "nomad",
    "kubernetes",
];

const TOP: &str = SECTIONS[0];

/// The entities of `document`: pools, servers, then the sections present.
pub(crate) fn list(file_index: usize, document: &DocumentMut) -> Vec<Entity> {
    let mut tables: Vec<(Kind, String, Table)> = Vec::new();
    if let Some(upstreams) = document.get("upstream").and_then(Item::as_table_like) {
        for (name, item) in upstreams.iter() {
            if let Some(table) = table_of(item) {
                tables.push((Kind::Upstream, name.to_owned(), table));
            }
        }
    }
    let servers: Vec<Option<Table>> = match document.get("server") {
        Some(Item::ArrayOfTables(servers)) => servers.iter().cloned().map(Some).collect(),
        Some(Item::Value(Value::Array(servers))) => servers
            .iter()
            .map(|server| {
                server
                    .as_inline_table()
                    .cloned()
                    .map(InlineTable::into_table)
            })
            .collect(),
        _ => Vec::new(),
    };
    let servers = servers.into_iter().enumerate();
    for (position, server) in servers {
        if let Some(table) = server {
            tables.push((Kind::Server, position.to_string(), table));
        }
    }

    let mut entities: Vec<Entity> = Vec::new();
    for (kind, key, table) in tables {
        entities.push(Entity {
            kind,
            value: json_of_table(&table),
            toml: body_text(kind, &key, table),
            key: match kind {
                Kind::Server => format!("{file_index}:{key}"),
                Kind::Upstream | Kind::Section => key,
            },
            file: file_index,
        });
    }

    let top = top_of(document);
    if !top.is_empty() {
        entities.push(Entity {
            kind: Kind::Section,
            key: String::from(TOP),
            file: file_index,
            value: json_of_table(top.as_table()),
            toml: top.to_string(),
        });
    }
    for name in &SECTIONS[1..] {
        if let Some(table) = document.get(name).and_then(table_of) {
            entities.push(Entity {
                kind: Kind::Section,
                key: String::from(*name),
                file: file_index,
                value: json_of_table(&table),
                toml: body_text(Kind::Section, name, table),
            });
        }
    }
    entities
}

/// Makes the entity equal to `value`. A key whose value is already the one
/// given is left as written, comment included; a key missing from `value` is
/// removed; objects merge into tables by the same rule; arrays are replaced.
/// On an error the document is unchanged.
pub(crate) fn replace_value(
    document: &mut DocumentMut,
    kind: Kind,
    key: &str,
    value: &Map<String, Json>,
) -> Result<(), String> {
    let mut edited = document.clone();
    if is_top_section(kind, key) {
        merge_top(&mut edited, value)?;
    } else {
        match find(&mut edited, kind, key)? {
            Slot::Table(table) => merge_table(table, value)?,
            Slot::Inline(table) => merge(table, value, /* inline= */ true)?,
        }
    }
    *document = edited;
    Ok(())
}

/// Replaces the entity with `toml`, the body of its table: its keys, then its
/// sub-tables under the headers they have in the file. The rest of the file
/// keeps its text. On an error the document is unchanged.
pub(crate) fn replace_toml(
    document: &mut DocumentMut,
    kind: Kind,
    key: &str,
    toml: &str,
) -> Result<(), String> {
    if is_top_section(kind, key) {
        return replace_top(document, toml);
    }
    let slot = find(document, kind, key)?;
    let mut new = parse_body(kind, key, toml)?;
    match slot {
        Slot::Table(old) => {
            *new.decor_mut() = old.decor().clone();
            set_positions(&mut new, old.position());
            *old = new;
        }
        Slot::Inline(old) => {
            let mut new = new.into_inline_table();
            *new.decor_mut() = old.decor().clone();
            *old = new;
        }
    }
    Ok(())
}

/// Adds an entity and returns its key: the name given for a pool or a
/// section, the position for a server (`key` is not used). On an error the
/// document is unchanged.
pub(crate) fn create(
    document: &mut DocumentMut,
    kind: Kind,
    key: Option<&str>,
    body: Body<'_>,
) -> Result<String, String> {
    match kind {
        Kind::Server => {
            let table = new_table(kind, "", body)?;
            let servers = document
                .entry("server")
                .or_insert_with(|| Item::ArrayOfTables(ArrayOfTables::new()));
            match servers {
                Item::ArrayOfTables(servers) => {
                    servers.push(table);
                    Ok((servers.len() - 1).to_string())
                }
                Item::Value(Value::Array(servers)) => {
                    servers.push(table.into_inline_table());
                    Ok((servers.len() - 1).to_string())
                }
                _ => Err(String::from("`server` is not an array of tables")),
            }
        }
        Kind::Upstream => {
            let name = key.ok_or("a pool needs a name")?;
            let table = new_table(kind, name, body)?;
            let upstreams = document.entry("upstream").or_insert_with(|| {
                let mut table = Table::new();
                table.set_implicit(true);
                Item::Table(table)
            });
            let inline = upstreams.is_value();
            let upstreams = upstreams
                .as_table_like_mut()
                .ok_or("`upstream` is not a table")?;
            if upstreams.contains_key(name) {
                return Err(format!("pool {name} already exists"));
            }
            let pool = if inline {
                Item::Value(Value::InlineTable(table.into_inline_table()))
            } else {
                Item::Table(table)
            };
            upstreams.insert(name, pool);
            Ok(name.to_owned())
        }
        Kind::Section => {
            let name = key.ok_or("a section needs a name")?;
            check_section(name)?;
            let exists = if name == TOP {
                !top_of(document).is_empty()
            } else {
                document.contains_key(name)
            };
            if exists {
                return Err(format!("section {name} already exists"));
            }
            if name == TOP {
                match body {
                    Body::Value(value) => replace_value(document, kind, name, value)?,
                    Body::Toml(toml) => replace_toml(document, kind, name, toml)?,
                }
            } else {
                let table = new_table(kind, name, body)?;
                document.insert(name, Item::Table(table));
            }
            Ok(name.to_owned())
        }
    }
}

/// Deletes an entity. The positions of the servers after a removed one shift
/// down by one.
pub(crate) fn remove(document: &mut DocumentMut, kind: Kind, key: &str) -> Result<(), String> {
    match kind {
        Kind::Server => {
            let position = position(key)?;
            match document.get_mut("server") {
                Some(Item::ArrayOfTables(servers)) if position < servers.len() => {
                    servers.remove(position);
                }
                Some(Item::Value(Value::Array(servers))) if position < servers.len() => {
                    servers.remove(position);
                }
                _ => return Err(format!("no server at position {key}")),
            }
        }
        Kind::Upstream => {
            document
                .get_mut("upstream")
                .and_then(Item::as_table_like_mut)
                .and_then(|upstreams| upstreams.remove(key))
                .ok_or_else(|| format!("pool {key} is not defined"))?;
        }
        Kind::Section => {
            check_section(key)?;
            if key == TOP {
                return Err(String::from("section top cannot be removed"));
            }
            document
                .remove(key)
                .ok_or_else(|| format!("section {key} is not defined"))?;
        }
    }
    Ok(())
}

/// Where an entity lives: under a `[header]`, or written as `{ .. }`.
enum Slot<'d> {
    Table(&'d mut Table),
    Inline(&'d mut InlineTable),
}

fn find<'d>(document: &'d mut DocumentMut, kind: Kind, key: &str) -> Result<Slot<'d>, String> {
    match kind {
        Kind::Server => {
            let position = position(key)?;
            let server = match document.get_mut("server") {
                Some(Item::ArrayOfTables(servers)) => servers.get_mut(position).map(Slot::Table),
                Some(Item::Value(Value::Array(servers))) => servers
                    .get_mut(position)
                    .and_then(Value::as_inline_table_mut)
                    .map(Slot::Inline),
                _ => None,
            };
            server.ok_or_else(|| format!("no server at position {key}"))
        }
        Kind::Upstream => document
            .get_mut("upstream")
            .and_then(Item::as_table_like_mut)
            .and_then(|upstreams| upstreams.get_mut(key))
            .and_then(slot_of)
            .ok_or_else(|| format!("pool {key} is not defined")),
        Kind::Section => {
            check_section(key)?;
            document
                .get_mut(key)
                .and_then(slot_of)
                .ok_or_else(|| format!("section {key} is not defined"))
        }
    }
}

fn slot_of(item: &mut Item) -> Option<Slot<'_>> {
    match item {
        Item::Table(table) => Some(Slot::Table(table)),
        Item::Value(Value::InlineTable(table)) => Some(Slot::Inline(table)),
        _ => None,
    }
}

/// A copy of the table at `item`, whichever way it is written.
fn table_of(item: &Item) -> Option<Table> {
    match item {
        Item::Table(table) => Some(table.clone()),
        Item::Value(Value::InlineTable(table)) => Some(table.clone().into_table()),
        _ => None,
    }
}

fn position(key: &str) -> Result<usize, String> {
    key.parse()
        .map_err(|_| format!("server position {key} is not a number"))
}

fn check_section(name: &str) -> Result<(), String> {
    if SECTIONS.contains(&name) {
        Ok(())
    } else {
        Err(format!("unknown section {name}"))
    }
}

fn is_top_section(kind: Kind, key: &str) -> bool {
    kind == Kind::Section && key == TOP
}

/// True for the root keys that hold entities of their own.
fn is_reserved(key: &str) -> bool {
    key == "server" || key == "upstream" || SECTIONS.contains(&key)
}

/// True for a root key that belongs to the section `top`.
fn is_top(key: &str, item: &Item) -> bool {
    !is_reserved(key) && item.is_value() && !item.is_inline_table()
}

/// The keys of the section `top` as a document of their own, comments kept.
fn top_of(root: &Table) -> DocumentMut {
    let mut top = DocumentMut::new();
    for (name, item) in root.iter() {
        if is_top(name, item)
            && let Some(key) = root.key(name)
        {
            top.insert_formatted(key, item.clone());
        }
    }
    top
}

fn remove_top(root: &mut Table, keep: impl Fn(&str) -> bool) {
    let stale: Vec<String> = root
        .iter()
        .filter(|(key, item)| is_top(key, item) && !keep(key))
        .map(|(key, _)| key.to_owned())
        .collect();
    for key in stale {
        root.remove(&key);
    }
}

fn merge_top(root: &mut Table, value: &Map<String, Json>) -> Result<(), String> {
    for (key, json) in value {
        let taken = root.get(key).is_some_and(|item| !is_top(key, item));
        if taken || is_reserved(key) || json.is_object() {
            return Err(format!("{key} is not a top level value"));
        }
    }
    remove_top(root, |key| value.contains_key(key));
    for (key, json) in value {
        set(root, key, json, /* inline= */ true)?;
    }
    Ok(())
}

fn replace_top(root: &mut Table, toml: &str) -> Result<(), String> {
    let new = parse_document("", toml)?;
    for (key, item) in new.iter() {
        if !is_top(key, item) {
            return Err(format!("{key} is not a top level value"));
        }
    }
    remove_top(root, |_| false);
    for (name, item) in new.iter() {
        if let Some(key) = new.key(name) {
            root.insert_formatted(key, item.clone());
        }
    }
    Ok(())
}

/// Makes `table` equal to `value`, touching only what differs. `inline` says
/// the table is written as `{ .. }`, where new objects cannot get a header.
fn merge(table: &mut dyn TableLike, value: &Map<String, Json>, inline: bool) -> Result<(), String> {
    let stale: Vec<String> = table
        .iter()
        .filter(|(key, _)| !value.contains_key(*key))
        .map(|(key, _)| key.to_owned())
        .collect();
    for key in stale {
        table.remove(&key);
    }
    for (key, json) in value {
        set(table, key, json, inline)?;
    }
    Ok(())
}

/// [`merge`] for a table with a header or made of dotted keys.
fn merge_table(table: &mut Table, value: &Map<String, Json>) -> Result<(), String> {
    merge(table, value, /* inline= */ false)?;
    if table.is_empty() {
        // Dotted and implicit tables are not written when empty: `{}` would delete the table.
        table.set_dotted(false);
        table.set_implicit(false);
    }
    Ok(())
}

fn set(table: &mut dyn TableLike, key: &str, json: &Json, inline: bool) -> Result<(), String> {
    let Some(item) = table.get_mut(key) else {
        table.insert(key, toml_item(json, inline)?);
        return Ok(());
    };
    if same(&json_of_item(item), json) {
        return Ok(());
    }
    match (json, item) {
        (Json::Object(value), Item::Table(current)) => merge_table(current, value),
        (Json::Object(value), Item::Value(Value::InlineTable(current))) => {
            merge(current, value, /* inline= */ true)
        }
        (_, Item::Value(current)) => {
            let mut new = toml_value(json)?;
            *new.decor_mut() = current.decor().clone();
            *current = new;
            Ok(())
        }
        (_, item) => {
            *item = toml_item(json, inline)?;
            Ok(())
        }
    }
}

/// True if writing `new` over `current` would change nothing. A float of the
/// file stays a float when the client sends it back without its fraction.
fn same(current: &Json, new: &Json) -> bool {
    match (current, new) {
        (Json::Number(current), Json::Number(new)) if current.is_f64() => {
            current.as_f64() == new.as_f64()
        }
        _ => current == new,
    }
}

fn json_of_item(item: &Item) -> Json {
    match item {
        Item::None => Json::Null,
        Item::Value(value) => json_of_value(value),
        Item::Table(table) => json_of_table(table),
        Item::ArrayOfTables(tables) => {
            Json::Array(tables.iter().map(|table| json_of_table(table)).collect())
        }
    }
}

fn json_of_table(table: &dyn TableLike) -> Json {
    let mut object: Map<String, Json> = Map::new();
    for (key, item) in table.iter() {
        object.insert(key.to_owned(), json_of_item(item));
    }
    Json::Object(object)
}

fn json_of_value(value: &Value) -> Json {
    match value {
        Value::String(string) => Json::String(string.value().clone()),
        Value::Integer(integer) => Json::Number(Number::from(*integer.value())),
        Value::Float(float) => Number::from_f64(*float.value())
            .map_or_else(|| Json::String(float.value().to_string()), Json::Number),
        Value::Boolean(boolean) => Json::Bool(*boolean.value()),
        Value::Datetime(datetime) => Json::String(datetime.value().to_string()),
        Value::Array(array) => Json::Array(array.iter().map(json_of_value).collect()),
        Value::InlineTable(table) => json_of_table(table),
    }
}

fn toml_item(json: &Json, inline: bool) -> Result<Item, String> {
    match json {
        Json::Object(value) if !inline => {
            let mut table = Table::new();
            merge(&mut table, value, /* inline= */ false)?;
            Ok(Item::Table(table))
        }
        _ => toml_value(json).map(Item::Value),
    }
}

fn toml_value(json: &Json) -> Result<Value, String> {
    Ok(match json {
        Json::Null => return Err(String::from("TOML has no null")),
        Json::Bool(boolean) => Value::from(*boolean),
        Json::Number(number) => match (number.as_i64(), number.as_f64()) {
            (Some(integer), _) => Value::from(integer),
            (None, Some(float)) if number.is_f64() => Value::from(float),
            _ => return Err(format!("{number} does not fit a TOML integer")),
        },
        Json::String(string) => Value::from(string.as_str()),
        Json::Array(values) => {
            let array: Array = values.iter().map(toml_value).collect::<Result<_, _>>()?;
            Value::Array(array)
        }
        Json::Object(values) => {
            let mut table = InlineTable::new();
            for (key, value) in values {
                table.insert(key, toml_value(value)?);
            }
            Value::InlineTable(table)
        }
    })
}

/// Document position of `table` and of every table under it. Tables are
/// written in the order of their positions; one without follows the table
/// before it.
fn set_positions(table: &mut Table, position: Option<isize>) {
    table.set_position(position);
    for (_, item) in table.iter_mut() {
        match item {
            Item::Table(child) => set_positions(child, position),
            Item::ArrayOfTables(children) => {
                for child in children.iter_mut() {
                    set_positions(child, position);
                }
            }
            _ => {}
        }
    }
}

/// Header of the table of an entity, as [`body_text`] writes it.
fn header(kind: Kind, key: &str) -> String {
    match kind {
        Kind::Server => String::from("[[server]]"),
        Kind::Upstream => format!("[upstream.{}]", Key::new(key)),
        Kind::Section => format!("[{}]", Key::new(key)),
    }
}

/// Text of `table` as the table of that entity, without the header line.
fn body_text(kind: Kind, key: &str, mut table: Table) -> String {
    // The header comes first and alone on its line whatever the layout in the file.
    table.decor_mut().clear();
    table.set_dotted(false);
    table.set_implicit(false);
    set_positions(&mut table, None);

    let mut document = DocumentMut::new();
    match kind {
        Kind::Server => {
            let mut servers = ArrayOfTables::new();
            servers.push(table);
            document.insert("server", Item::ArrayOfTables(servers));
        }
        Kind::Upstream => {
            let mut upstreams = Table::new();
            upstreams.set_implicit(true);
            upstreams.insert(key, Item::Table(table));
            document.insert("upstream", Item::Table(upstreams));
        }
        Kind::Section => {
            document.insert(key, Item::Table(table));
        }
    }
    let text = document.to_string();
    let (_, body) = text.split_once('\n').unwrap_or_default();
    body.to_owned()
}

/// Inverse of [`body_text`]. Text that defines anything else than that one
/// table is refused: it would be written to the file under another name.
fn parse_body(kind: Kind, key: &str, toml: &str) -> Result<Table, String> {
    let mut document = parse_document(&format!("{}\n", header(kind, key)), toml)?;
    let table = match kind {
        Kind::Server => match document.remove("server") {
            Some(Item::ArrayOfTables(servers)) if servers.len() == 1 => servers.into_iter().next(),
            _ => None,
        },
        Kind::Upstream => match document.remove("upstream") {
            Some(Item::Table(mut upstreams)) if upstreams.len() == 1 => upstreams.remove(key),
            _ => None,
        }
        .and_then(|pool| pool.into_table().ok()),
        Kind::Section => document
            .remove(key)
            .and_then(|section| section.into_table().ok()),
    };
    let Some(mut table) = table.filter(|_| document.is_empty()) else {
        return Err(format!(
            "the text defines tables outside of {}",
            header(kind, key)
        ));
    };
    // Those of the parsed text mean nothing in the file it goes to.
    table.decor_mut().clear();
    set_positions(&mut table, None);
    Ok(table)
}

/// Parses `prefix` followed by `toml`; an error names the line within `toml`.
fn parse_document(prefix: &str, toml: &str) -> Result<DocumentMut, String> {
    format!("{prefix}{toml}").parse().map_err(|err: TomlError| {
        let offset = err.span().map_or(0, |span| span.start);
        let offset = offset.saturating_sub(prefix.len()).min(toml.len());
        let newlines = toml.as_bytes()[..offset]
            .iter()
            .filter(|byte| **byte == b'\n')
            .count();
        format!("line {}: {}", newlines + 1, err.message())
    })
}

fn new_table(kind: Kind, key: &str, body: Body<'_>) -> Result<Table, String> {
    match body {
        Body::Value(value) => {
            let mut table = Table::new();
            merge(&mut table, value, /* inline= */ false)?;
            Ok(table)
        }
        Body::Toml(toml) => parse_body(kind, key, toml),
    }
}

#[cfg(test)]
mod tests {
    use rxh_core::config::Config;
    use serde_json::json;

    use super::*;

    const BASE: &str = r#"
# Main API pool. This comment must survive edits.
[upstream.api]
policy = "least_conn"
backends = ["127.0.0.1:8080"]

[[server]]
name = "web"
listen = "127.0.0.1:8000"
forward = "@api"
"#;

    fn parse(text: &str) -> DocumentMut {
        text.parse().unwrap()
    }

    /// The document written out and read again, as the daemon will see it.
    fn reparsed(document: &DocumentMut) -> DocumentMut {
        let text = document.to_string();
        text.parse().unwrap_or_else(|err| panic!("{err}\n{text}"))
    }

    fn object(value: Json) -> Map<String, Json> {
        match value {
            Json::Object(map) => map,
            other => panic!("not an object: {other}"),
        }
    }

    fn keys(document: &DocumentMut) -> Vec<(Kind, String)> {
        list(0, document)
            .into_iter()
            .map(|entity| (entity.kind, entity.key))
            .collect()
    }

    fn entity(document: &DocumentMut, kind: Kind, key: &str) -> Entity {
        list(0, document)
            .into_iter()
            .find(|entity| entity.kind == kind && entity.key == key)
            .unwrap_or_else(|| panic!("no {kind:?} {key} in:\n{document}"))
    }

    #[test]
    fn lists_pools_servers_and_sections() {
        let document = parse(&format!("{BASE}\n[log]\nformat = \"json\"\n"));
        assert_eq!(
            keys(&document),
            [
                (Kind::Upstream, String::from("api")),
                (Kind::Server, String::from("0:0")),
                (Kind::Section, String::from("log")),
            ]
        );

        let api = entity(&document, Kind::Upstream, "api");
        assert_eq!(
            api.value,
            json!({"policy": "least_conn", "backends": ["127.0.0.1:8080"]})
        );
        assert_eq!(
            api.toml,
            "policy = \"least_conn\"\nbackends = [\"127.0.0.1:8080\"]\n"
        );
        assert_eq!(
            entity(&document, Kind::Server, "0:0").toml,
            "name = \"web\"\nlisten = \"127.0.0.1:8000\"\nforward = \"@api\"\n"
        );

        let included = list(2, &document);
        assert!(included.iter().all(|entity| entity.file == 2));
        assert_eq!(included[1].key, "2:0");
        assert_eq!(
            serde_json::to_value(&included[1]).unwrap()["kind"],
            json!("server")
        );
    }

    #[test]
    fn kinds_are_read_from_path_segments() {
        assert_eq!(Kind::from_segment("server"), Some(Kind::Server));
        assert_eq!(Kind::from_segment("upstream"), Some(Kind::Upstream));
        assert_eq!(Kind::from_segment("section"), Some(Kind::Section));
        assert_eq!(Kind::from_segment("servers"), None);
        assert_eq!(Kind::from_segment(""), None);
    }

    #[test]
    fn value_keeps_placeholders_and_aliases() {
        let document = parse("[upstream.api]\nlb_policy = \"random\"\ntoken = \"{env.T}\"\n");
        assert_eq!(
            entity(&document, Kind::Upstream, "api").value,
            json!({"lb_policy": "random", "token": "{env.T}"})
        );
    }

    #[test]
    fn value_covers_every_toml_type() {
        let document = parse(
            "[[server]]\ncount = 3\nratio = 0.5\non = true\nsince = 2026-11-20T10:00:00Z\n\
             tls = { cert = \"a.pem\" }\n\n[[server.match]]\nuri = \"/a\"\n\n\
             [server.headers]\nx-a = \"1\"\n",
        );
        assert_eq!(
            entity(&document, Kind::Server, "0:0").value,
            json!({
                "count": 3,
                "ratio": 0.5,
                "on": true,
                "since": "2026-11-20T10:00:00Z",
                "tls": {"cert": "a.pem"},
                "match": [{"uri": "/a"}],
                "headers": {"x-a": "1"},
            })
        );
    }

    #[test]
    fn merge_keeps_comments_of_untouched_keys() {
        let mut document = parse(
            "[upstream.api]\n# why\npolicy = \"least_conn\"\nretries = 2 # twice\nratio = 1.0\n",
        );
        replace_value(
            &mut document,
            Kind::Upstream,
            "api",
            &object(json!({"policy": "least_conn", "retries": 3, "ratio": 1})),
        )
        .unwrap();
        assert_eq!(
            document.to_string(),
            "[upstream.api]\n# why\npolicy = \"least_conn\"\nretries = 3 # twice\nratio = 1.0\n"
        );
    }

    #[test]
    fn merge_removes_missing_keys_and_merges_nested_tables() {
        let mut document = parse(
            "[upstream.api]\nbackends = [\"127.0.0.1:1\"]\nretries = 2\n\n# Probe.\n\
             [upstream.api.health]\nuri = \"/h\"\ninterval = \"1s\"\n",
        );
        replace_value(
            &mut document,
            Kind::Upstream,
            "api",
            &object(json!({
                "backends": ["127.0.0.1:1"],
                "health": {"uri": "/ok"},
                "passive": {"fail_duration": "30s"},
            })),
        )
        .unwrap();
        let text = document.to_string();
        assert!(text.contains("# Probe.\n[upstream.api.health]\nuri = \"/ok\"\n"));
        assert!(!text.contains("interval"), "{text}");
        assert!(!text.contains("retries"), "{text}");
        assert_eq!(
            entity(&reparsed(&document), Kind::Upstream, "api").value,
            json!({
                "backends": ["127.0.0.1:1"],
                "health": {"uri": "/ok"},
                "passive": {"fail_duration": "30s"},
            })
        );
    }

    #[test]
    fn arrays_are_replaced_whole() {
        let mut document = parse(
            "[[server]]\nlisten = [\"127.0.0.1:1\", \"127.0.0.1:2\"]\n\n\
             [[server.match]]\nuri = \"/a\"\n\n[[server.match]]\nuri = \"/b\"\n",
        );
        let value = json!({"listen": ["127.0.0.1:2"], "match": [{"uri": "/b"}]});
        replace_value(&mut document, Kind::Server, "0", &object(value.clone())).unwrap();
        let document = reparsed(&document);
        assert_eq!(keys(&document), [(Kind::Server, String::from("0:0"))]);
        assert_eq!(entity(&document, Kind::Server, "0:0").value, value);
    }

    #[test]
    fn a_new_nested_table_stays_in_the_server_it_was_added_to() {
        let mut document = parse(
            "[[server]]\nname = \"a\"\nlisten = \"127.0.0.1:1\"\n\n\
             [[server]]\nname = \"b\"\nlisten = \"127.0.0.1:2\"\n",
        );
        let first = json!({
            "name": "a",
            "listen": "127.0.0.1:1",
            "headers": {"x-a": "1"},
        });
        replace_value(&mut document, Kind::Server, "0", &object(first.clone())).unwrap();
        let document = reparsed(&document);
        assert_eq!(
            entity(&document, Kind::Server, "0:0").value,
            first,
            "{document}"
        );
        assert_eq!(
            entity(&document, Kind::Server, "0:1").value,
            json!({"name": "b", "listen": "127.0.0.1:2"}),
            "{document}"
        );
    }

    #[test]
    fn null_is_rejected() {
        let mut document = parse(BASE);
        for value in [
            json!({"policy": null}),
            json!({"backends": ["127.0.0.1:1", null]}),
            json!({"backends": [], "health": {"uri": null}}),
        ] {
            let value = object(value);
            let err = replace_value(&mut document, Kind::Upstream, "api", &value).unwrap_err();
            assert!(err.contains("TOML has no null"), "{err}");
            assert_eq!(document.to_string(), BASE, "a refused edit changes nothing");

            let created = create(
                &mut document,
                Kind::Upstream,
                Some("db"),
                Body::Value(&value),
            );
            assert!(created.unwrap_err().contains("TOML has no null"));
            assert_eq!(document.to_string(), BASE, "a refused edit changes nothing");
        }
    }

    const LAYERED: &str = "\
# Reload on change.
watch = true

# Pool.
[upstream.api] # main
policy = \"least_conn\"
backends = [\"127.0.0.1:8080\"]

# Probe.
[upstream.api.health]
uri = \"/h\"

# Front.
[[server]]
listen = \"127.0.0.1:8000\"
forward = \"@api\"

[server.headers]
x-a = \"1\"

[log]
format = \"json\"
# The end.
";

    #[test]
    fn replace_toml_swaps_the_table_only() {
        let mut document = parse(LAYERED);
        replace_toml(
            &mut document,
            Kind::Upstream,
            "api",
            "# New.\nbackends = [\"127.0.0.1:9\"]\n\n[upstream.api.passive]\nfail_duration = \"1s\"\n",
        )
        .unwrap();
        let start = LAYERED.find("policy").unwrap();
        let end = LAYERED.find("\n# Front.").unwrap();
        assert_eq!(
            document.to_string(),
            format!(
                "{}# New.\nbackends = [\"127.0.0.1:9\"]\n\n[upstream.api.passive]\n\
                 fail_duration = \"1s\"\n{}",
                &LAYERED[..start],
                &LAYERED[end..]
            )
        );

        let mut document = parse(LAYERED);
        replace_toml(&mut document, Kind::Server, "0", "serve = \"/srv\"\n").unwrap();
        let start = LAYERED.find("listen").unwrap();
        let end = LAYERED.find("\n[log]").unwrap();
        assert_eq!(
            document.to_string(),
            format!("{}serve = \"/srv\"\n{}", &LAYERED[..start], &LAYERED[end..])
        );
    }

    #[test]
    fn toml_of_an_entity_written_back_changes_nothing() {
        let mut document = parse(LAYERED);
        let entities = list(0, &document);
        assert_eq!(entities.len(), 4);
        for entity in entities {
            let key = entity.key.rsplit(':').next().unwrap();
            replace_toml(&mut document, entity.kind, key, &entity.toml).unwrap();
            assert_eq!(document.to_string(), LAYERED, "{:?} {key}", entity.kind);
            let value = object(entity.value);
            replace_value(&mut document, entity.kind, key, &value).unwrap();
            assert_eq!(document.to_string(), LAYERED, "{:?} {key}", entity.kind);
        }
        assert_eq!(
            entity(&document, Kind::Upstream, "api").toml,
            "policy = \"least_conn\"\nbackends = [\"127.0.0.1:8080\"]\n\n# Probe.\n\
             [upstream.api.health]\nuri = \"/h\"\n"
        );
    }

    #[test]
    fn replace_toml_refuses_text_that_is_not_the_body_of_the_table() {
        let mut document = parse(LAYERED);
        for (kind, key, toml, expected) in [
            (Kind::Upstream, "api", "backends = \n", "line 1"),
            (
                Kind::Upstream,
                "api",
                "backends = []\nretries = \n",
                "line 2",
            ),
            (Kind::Upstream, "api", "[admin]\ntoken = \"x\"\n", "outside"),
            (
                Kind::Upstream,
                "api",
                "[upstream.db]\nbackends = []\n",
                "outside",
            ),
            (
                Kind::Server,
                "0",
                "serve = \"/a\"\n[[server]]\nserve = \"/b\"\n",
                "outside",
            ),
            (
                Kind::Section,
                "log",
                "[upstream.db]\nbackends = []\n",
                "outside",
            ),
            (Kind::Section, "top", "[log]\nformat = \"json\"\n", "log"),
            (Kind::Section, "top", "server = []\n", "server"),
            (Kind::Upstream, "db", "backends = []\n", "db"),
            (Kind::Section, "admin", "token = \"x\"\n", "admin"),
            (Kind::Section, "nope", "a = 1\n", "nope"),
            (Kind::Server, "7", "serve = \"/a\"\n", "7"),
            (Kind::Server, "first", "serve = \"/a\"\n", "first"),
        ] {
            let err = replace_toml(&mut document, kind, key, toml).unwrap_err();
            assert!(err.contains(expected), "{toml:?}: {err}");
            assert_eq!(
                document.to_string(),
                LAYERED,
                "a refused edit changes nothing"
            );
        }
    }

    #[test]
    fn top_holds_the_bare_root_keys() {
        let mut document = parse(
            "# Grace.\ngrace_period = \"10s\"\nwatch = true\ninclude = [\"conf.d/*.toml\"]\n\n\
             [log]\nformat = \"json\"\n",
        );
        let top = entity(&document, Kind::Section, "top");
        assert_eq!(
            top.value,
            json!({"grace_period": "10s", "watch": true, "include": ["conf.d/*.toml"]})
        );
        assert_eq!(
            top.toml,
            "# Grace.\ngrace_period = \"10s\"\nwatch = true\ninclude = [\"conf.d/*.toml\"]\n"
        );

        for value in [
            json!({"log": {"format": "text"}}),
            json!({"server": []}),
            json!({"upstream": "x"}),
            json!({"limits": {"a": 1}}),
        ] {
            let before = document.to_string();
            let err = replace_value(&mut document, Kind::Section, "top", &object(value));
            assert!(err.unwrap_err().contains("top level value"));
            assert_eq!(document.to_string(), before);
        }

        replace_value(
            &mut document,
            Kind::Section,
            "top",
            &object(json!({"grace_period": "10s", "runtime": "per_core"})),
        )
        .unwrap();
        assert_eq!(
            document.to_string(),
            "# Grace.\ngrace_period = \"10s\"\nruntime = \"per_core\"\n\n[log]\nformat = \"json\"\n"
        );

        replace_toml(
            &mut document,
            Kind::Section,
            "top",
            "# Watch.\nwatch = true\n",
        )
        .unwrap();
        assert_eq!(
            document.to_string(),
            "# Watch.\nwatch = true\n\n[log]\nformat = \"json\"\n"
        );
        assert!(remove(&mut document, Kind::Section, "top").is_err());

        replace_value(&mut document, Kind::Section, "top", &Map::new()).unwrap();
        assert_eq!(keys(&document), [(Kind::Section, String::from("log"))]);
    }

    #[test]
    fn create_and_remove_round_trip() {
        let mut document = parse("");
        let pool = object(json!({
            "policy": "least_conn",
            "backends": ["127.0.0.1:8080"],
            "health": {"uri": "/h"},
        }));
        let server = object(json!({"listen": "127.0.0.1:8000", "forward": "@api"}));
        let log = object(json!({"format": "json"}));
        let top = object(json!({"watch": true}));

        let created = [
            create(
                &mut document,
                Kind::Upstream,
                Some("api"),
                Body::Value(&pool),
            ),
            create(
                &mut document,
                Kind::Upstream,
                Some("a.b"),
                Body::Toml(
                    "backends = [\"127.0.0.1:1\"]\n\n[upstream.\"a.b\".health]\nuri = \"/h\"\n",
                ),
            ),
            create(&mut document, Kind::Server, None, Body::Value(&server)),
            create(
                &mut document,
                Kind::Server,
                None,
                Body::Toml("listen = \"127.0.0.1:8001\"\nserve = \"/srv\"\n"),
            ),
            create(&mut document, Kind::Section, Some("log"), Body::Value(&log)),
            create(
                &mut document,
                Kind::Section,
                Some("admin"),
                Body::Toml("listen = \"127.0.0.1:9000\"\n"),
            ),
            create(&mut document, Kind::Section, Some("top"), Body::Value(&top)),
        ];
        assert_eq!(
            created.map(Result::unwrap),
            ["api", "a.b", "0", "1", "log", "admin", "top"]
        );

        let text = document.to_string();
        assert!(!text.contains("[upstream]\n"), "{text}");
        assert!(text.contains("[upstream.\"a.b\"]\n"), "{text}");
        let config = Config::from_toml(&text).unwrap_or_else(|err| panic!("{err}\n{text}"));
        assert_eq!(config.servers.len(), 2);
        assert!(config.upstreams["api"].health.is_some());
        assert!(config.upstreams["a.b"].health.is_some());
        assert!(config.watch);

        let document_read = reparsed(&document);
        assert_eq!(
            keys(&document_read),
            [
                (Kind::Upstream, String::from("api")),
                (Kind::Upstream, String::from("a.b")),
                (Kind::Server, String::from("0:0")),
                (Kind::Server, String::from("0:1")),
                (Kind::Section, String::from("top")),
                (Kind::Section, String::from("admin")),
                (Kind::Section, String::from("log")),
            ]
        );
        assert_eq!(
            entity(&document_read, Kind::Upstream, "api").value,
            Json::Object(pool.clone())
        );
        assert_eq!(
            entity(&document_read, Kind::Server, "0:0").value,
            Json::Object(server.clone())
        );

        for (kind, key, body, expected) in [
            (Kind::Upstream, Some("api"), Body::Value(&pool), "api"),
            (Kind::Upstream, Some("api"), Body::Toml(""), "api"),
            (Kind::Upstream, None, Body::Value(&pool), "name"),
            (Kind::Section, Some("log"), Body::Value(&log), "log"),
            (Kind::Section, Some("top"), Body::Value(&top), "top"),
            (Kind::Section, Some("nope"), Body::Value(&log), "nope"),
            (Kind::Section, None, Body::Value(&log), "name"),
            (Kind::Server, None, Body::Toml("listen = "), "line 1"),
            (Kind::Server, None, Body::Toml("[log]\na = 1\n"), "outside"),
        ] {
            let err = create(&mut document, kind, key, body).unwrap_err();
            assert!(err.contains(expected), "{err}");
            assert_eq!(document.to_string(), text, "a refused edit changes nothing");
        }
        for (kind, key) in [
            (Kind::Section, "top"),
            (Kind::Section, "nope"),
            (Kind::Section, "tracing"),
            (Kind::Upstream, "db"),
            (Kind::Server, "2"),
            (Kind::Server, "x"),
        ] {
            let err = remove(&mut document, kind, key).unwrap_err();
            assert!(err.contains(key), "{err}");
            assert_eq!(document.to_string(), text, "a refused edit changes nothing");
        }

        remove(&mut document, Kind::Server, "0").unwrap();
        assert_eq!(
            entity(&reparsed(&document), Kind::Server, "0:0").value["serve"],
            json!("/srv")
        );
        remove(&mut document, Kind::Server, "0").unwrap();
        remove(&mut document, Kind::Upstream, "api").unwrap();
        remove(&mut document, Kind::Upstream, "a.b").unwrap();
        remove(&mut document, Kind::Section, "log").unwrap();
        remove(&mut document, Kind::Section, "admin").unwrap();
        replace_value(&mut document, Kind::Section, "top", &Map::new()).unwrap();
        assert_eq!(keys(&reparsed(&document)), []);
        assert_eq!(document.to_string().trim(), "");
    }

    /// Review Focus 1: the same entities written without `[header]` tables.
    #[test]
    fn other_layouts_are_listed_and_edited_in_place() {
        for (text, kind, key, value, edited, toml, body) in [
            (
                "server = [{ listen = \"127.0.0.1:1\", forward = \"127.0.0.1:2\" }]\n",
                Kind::Server,
                "0",
                json!({"listen": "127.0.0.1:1", "forward": "127.0.0.1:2"}),
                json!({"listen": "127.0.0.1:1", "forward": "127.0.0.1:9", "tls": {"cert": "a"}}),
                "listen = \"127.0.0.1:1\"\nforward = \"127.0.0.1:2\"\n",
                "serve = \"/srv\"\n\n[server.headers]\nx-a = \"1\"\n",
            ),
            (
                "upstream.api.backends = [\"127.0.0.1:3\"]\n",
                Kind::Upstream,
                "api",
                json!({"backends": ["127.0.0.1:3"]}),
                json!({"backends": ["127.0.0.1:9"], "retries": 2, "health": {"uri": "/h"}}),
                "backends = [\"127.0.0.1:3\"]\n",
                "backends = []\n\n[upstream.api.health]\nuri = \"/h\"\n",
            ),
            (
                "upstream = { api = { backends = [\"127.0.0.1:4\"] } }\n",
                Kind::Upstream,
                "api",
                json!({"backends": ["127.0.0.1:4"]}),
                json!({"backends": ["127.0.0.1:9"], "retries": 2, "health": {"uri": "/h"}}),
                "backends = [\"127.0.0.1:4\"]\n",
                "backends = []\n\n[upstream.api.health]\nuri = \"/h\"\n",
            ),
            (
                "log = { format = \"json\" }\n",
                Kind::Section,
                "log",
                json!({"format": "json"}),
                json!({"format": "text", "fields": {"a": "b"}}),
                "format = \"json\"\n",
                "format = \"text\"\n",
            ),
            (
                "[upstream.api.health]\nuri = \"/h\"\n",
                Kind::Upstream,
                "api",
                json!({"health": {"uri": "/h"}}),
                json!({"backends": ["127.0.0.1:9"], "health": {"uri": "/ok"}}),
                "[upstream.api.health]\nuri = \"/h\"\n",
                "backends = []\n",
            ),
        ] {
            let listed = |document: &DocumentMut| -> Vec<Entity> {
                let mut entities = list(0, &reparsed(document));
                entities.retain(|entity| entity.kind == kind);
                entities
            };
            let one = |document: &DocumentMut| -> Entity {
                let mut entities = listed(document);
                assert_eq!(entities.len(), 1, "{text} became:\n{document}");
                entities.remove(0)
            };

            let original = parse(text);
            let found = one(&original);
            assert_eq!(found.value, value, "{text}");
            assert_eq!(found.toml, toml, "{text}");

            let mut document = original.clone();
            replace_value(&mut document, kind, key, &object(value.clone())).unwrap();
            assert_eq!(document.to_string(), text, "an equal value changes nothing");

            replace_value(&mut document, kind, key, &object(edited.clone())).unwrap();
            assert_eq!(one(&document).value, edited, "{text} became:\n{document}");

            // Emptied, it is still there: the caller removes entities with `remove`.
            replace_value(&mut document, kind, key, &Map::new()).unwrap();
            assert_eq!(
                one(&document).value,
                json!({}),
                "{text} became:\n{document}"
            );

            let mut document = original.clone();
            replace_toml(&mut document, kind, key, body).unwrap();
            let replaced = one(&document);
            let mut expected = parse("");
            create(&mut expected, kind, Some(key), Body::Toml(body)).unwrap();
            assert_eq!(replaced.value, one(&expected).value, "{text}");

            let mut document = original.clone();
            let other = body.replace("upstream.api", "upstream.other");
            let second = create(&mut document, kind, Some("other"), Body::Toml(&other));
            if kind == Kind::Section {
                assert!(second.is_err(), "there is no section `other`");
            } else {
                let second = second.unwrap();
                assert_eq!(listed(&document).len(), 2, "{text} became:\n{document}");
                remove(&mut document, kind, &second).unwrap();
                assert_eq!(one(&document).value, value, "{text} became:\n{document}");
            }

            let mut document = original.clone();
            remove(&mut document, kind, key).unwrap();
            assert_eq!(listed(&document).len(), 0, "{text} became:\n{document}");
        }
    }
}
