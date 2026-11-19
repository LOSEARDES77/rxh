//! `rxh fmt`: canonical layout of a TOML configuration file.
//!
//! Only whitespace changes; keys, values, their order and every comment stay.
//! Lines lose their indentation (comment lines above a line move with it),
//! `key = value` gets one space around `=`, table headers lose inner spaces
//! and get a blank line before them, runs of blank lines shrink to one and
//! the file ends with a single newline. The spaces before a trailing comment
//! and the inside of arrays and inline tables are kept as written, so
//! hand-aligned comments and lists stay aligned.

use toml_edit::{DocumentMut, Item, KeyMut, RawString, Table, TomlError, Value};

/// Returns `text` in canonical layout, or the TOML syntax error.
pub(crate) fn format(text: &str) -> Result<String, TomlError> {
    let mut document: DocumentMut = text.parse()?;
    format_table(document.as_table_mut());
    let trailing: String = document
        .trailing()
        .as_str()
        .map(trailing)
        .unwrap_or_default();
    document.set_trailing(trailing);

    let output: String = document.to_string();
    let output: &str = output.trim_start_matches('\n').trim_end();
    if output.is_empty() {
        return Ok(String::new());
    }
    Ok(format!("{output}\n"))
}

/// Comment lines after the last item, which have no line to align with.
fn trailing(text: &str) -> String {
    let lines: Vec<&str> = text.lines().map(str::trim).collect();
    prefix(&format!("{}\n", lines.join("\n")))
}

/// Formats the key/value lines and sub-tables of a table.
fn format_table(table: &mut Table) {
    for (mut key, item) in table.iter_mut() {
        match item {
            Item::Value(value) => {
                line_key(&mut key);
                key_value(&mut key, value);
            }
            Item::Table(table) if table.is_dotted() => {
                line_key(&mut key);
                key.leaf_decor_mut().set_suffix("");
                format_dotted(table);
            }
            Item::Table(table) => {
                header_key(&mut key);
                format_header(table);
                format_table(table);
            }
            Item::ArrayOfTables(tables) => {
                header_key(&mut key);
                for table in tables.iter_mut() {
                    format_header(table);
                    format_table(table);
                }
            }
            Item::None => {}
        }
    }
}

/// Formats the rest of a dotted key line such as `a.b.c = 1`, after `a`.
fn format_dotted(table: &mut Table) {
    for (mut key, item) in table.iter_mut() {
        key.leaf_decor_mut().set_prefix("");
        key.dotted_decor_mut().clear();
        match item {
            Item::Value(value) => {
                key_value(&mut key, value);
            }
            Item::Table(table) => {
                key.leaf_decor_mut().set_suffix("");
                format_dotted(table);
            }
            Item::ArrayOfTables(_) | Item::None => {}
        }
    }
}

/// One space around `=`; a trailing comment keeps its alignment.
fn key_value(key: &mut KeyMut<'_>, value: &mut Value) {
    key.leaf_decor_mut().set_suffix(" ");
    value.decor_mut().set_prefix(" ");
    let suffix: String = raw(value.decor().suffix()).map(suffix).unwrap_or_default();
    value.decor_mut().set_suffix(suffix);
}

/// The first key of a line keeps the comments above it, without indentation.
fn line_key(key: &mut KeyMut<'_>) {
    let prefix: String = raw(key.leaf_decor().prefix())
        .map(prefix)
        .unwrap_or_default();
    key.leaf_decor_mut().set_prefix(prefix);
    key.dotted_decor_mut().clear();
}

/// A key inside a `[header]` loses the spaces around it.
fn header_key(key: &mut KeyMut<'_>) {
    key.leaf_decor_mut().clear();
    key.dotted_decor_mut().clear();
}

/// Puts a blank line before an explicit table header. Implicit tables
/// (`[a]` never written, only `[a.b]`) have no header to format.
fn format_header(table: &mut Table) {
    if table.is_implicit() {
        return;
    }
    let comments: String = raw(table.decor().prefix()).map(prefix).unwrap_or_default();
    let suffix: String = raw(table.decor().suffix()).map(suffix).unwrap_or_default();
    let decor = table.decor_mut();
    // Comments right after the last key of the previous table (often keys
    // commented out) stay with it when a blank line already separates them.
    if comments.starts_with('\n') || comments.contains("\n\n") {
        decor.set_prefix(comments);
    } else {
        decor.set_prefix(format!("\n{comments}"));
    }
    decor.set_suffix(suffix);
}

fn raw(raw: Option<&RawString>) -> Option<&str> {
    raw.and_then(RawString::as_str)
}

/// The comment and blank lines before a line, with at most one blank line
/// in a row. The text after the last newline is the indentation of the line,
/// which goes; comment lines move left by as much, so a comment block keeps
/// its shape (continuation comments aligned under a trailing comment).
fn prefix(text: &str) -> String {
    let (lines, indentation): (&str, &str) = match text.rfind('\n') {
        Some(newline) => (&text[..=newline], &text[newline + 1..]),
        None => ("", text),
    };
    let mut output = String::new();
    let mut previous_blank = false;
    for line in lines.lines() {
        let line: &str = line.trim_end();
        let blank: bool = line.is_empty();
        if !(blank && previous_blank) {
            output.push_str(line.strip_prefix(indentation).unwrap_or(line.trim_start()));
            output.push('\n');
        }
        previous_blank = blank;
    }
    output
}

/// What follows a value or a header on its line: nothing, or the comment
/// with the spaces before it as written (at least one), so aligned trailing
/// comments stay aligned.
fn suffix(text: &str) -> String {
    let comment: &str = text.trim_end();
    match comment.trim_start() {
        "" => String::new(),
        _ if comment.starts_with('#') => format!(" {comment}"),
        _ => comment.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_spacing_and_keeps_comments() {
        let input = "\n\n# Top comment.\n\n\n  a   =  1   # one\nb=\"two\"\n\
                     [ server ]  # header\n    listen=[\"127.0.0.1:80\",\n      \"[::1]:80\"]\n\
                     # Before the list.\n\n\n[[ upstream.api.backends ]]\naddress = \"x\"\n\n\n# End.\n\n";
        let expected = "# Top comment.\n\na = 1   # one\nb = \"two\"\n\n\
                        [server]  # header\nlisten = [\"127.0.0.1:80\",\n      \"[::1]:80\"]\n\
                        # Before the list.\n\n[[upstream.api.backends]]\naddress = \"x\"\n\n# End.\n";
        assert_eq!(format(input).unwrap(), expected);
    }

    #[test]
    fn formats_dotted_keys() {
        assert_eq!(
            format("  log . level =\"debug\"  # dotted\n").unwrap(),
            "log.level = \"debug\"  # dotted\n"
        );
    }

    #[test]
    fn keeps_trailing_comments_with_their_table() {
        let input = "[a]\nx = 1\n# y = 2\n\n# About b.\n[b]\n# About c.\n[c]\n";
        let expected = "[a]\nx = 1\n# y = 2\n\n# About b.\n[b]\n\n# About c.\n[c]\n";
        assert_eq!(format(input).unwrap(), expected);
    }

    #[test]
    fn keeps_comment_blocks_aligned() {
        let input = "  [server]\n  policy = \"first\"  # a long comment\n                    # continued\n  \
                     # own line\n  listen = \"x\"#tight\n";
        let expected = "[server]\npolicy = \"first\"  # a long comment\n                  # continued\n\
                        # own line\nlisten = \"x\" #tight\n";
        assert_eq!(format(input).unwrap(), expected);
    }

    #[test]
    fn keeps_strings_and_inline_tables_as_written() {
        let input = "tls = {  cert = \"a.pem\", key = \"b.pem\" }\ntext = \"\"\"\n  two  \n\n\n  lines\"\"\"\n";
        assert_eq!(format(input).unwrap(), input);
    }

    #[test]
    fn is_idempotent() {
        let input = "a=1\n[b]\n  c = 2 #x\n\n\n\n[d.e]\nf = 3\n";
        let once: String = format(input).unwrap();
        assert_eq!(format(&once).unwrap(), once);
        assert_eq!(once, "a = 1\n\n[b]\nc = 2 #x\n\n[d.e]\nf = 3\n");
    }

    #[test]
    fn handles_empty_and_comment_only_files() {
        assert_eq!(format("").unwrap(), "");
        assert_eq!(format("\n\n  \n").unwrap(), "");
        assert_eq!(format("  # only\n\n").unwrap(), "# only\n");
        assert_eq!(
            format("a = 1\n# no newline").unwrap(),
            "a = 1\n# no newline\n"
        );
    }

    #[test]
    fn rejects_invalid_toml() {
        let err = format("a = \n").expect_err("missing value");
        assert!(err.to_string().contains("line 1"), "{err}");
    }
}
