//! The tree of directives shared by the Caddyfile and nginx syntaxes: a name,
//! arguments and an optional `{ ... }` block of more directives.

use std::iter::Peekable;
use std::vec::IntoIter;

/// One directive, such as `reverse_proxy /api/* localhost:3000 { ... }`.
#[derive(Debug)]
pub(crate) struct Directive {
    pub name: String,
    pub args: Vec<String>,
    /// `None` without braces; `Some` (maybe empty) with them.
    pub block: Option<Vec<Directive>>,
    pub line: usize,
}

impl Directive {
    /// The directives of the block, empty without one.
    pub fn children(&self) -> &[Directive] {
        self.block.as_deref().unwrap_or_default()
    }
}

#[derive(Clone, Copy, PartialEq)]
pub(crate) enum Syntax {
    /// A directive ends with its line; `{` and `}` stand alone.
    Caddyfile,
    /// A directive ends with `;` or a block; `{`, `}` and `;` need no spaces.
    Nginx,
}

struct Token {
    text: String,
    quoted: bool,
    line: usize,
}

impl Token {
    fn is(&self, text: &str) -> bool {
        !self.quoted && self.text == text
    }
}

/// Parses the whole file into its top level directives.
pub(crate) fn parse(text: &str, syntax: Syntax) -> Result<Vec<Directive>, String> {
    let mut tokens = tokenize(text, syntax)?.into_iter().peekable();
    parse_block(&mut tokens, syntax, None)
}

/// Directives up to the `}` that closes the block opened on `opened`, or to
/// the end of the file at the top level.
fn parse_block(
    tokens: &mut Peekable<IntoIter<Token>>,
    syntax: Syntax,
    opened: Option<usize>,
) -> Result<Vec<Directive>, String> {
    let mut directives: Vec<Directive> = Vec::new();
    loop {
        let Some(first) = tokens.peek() else {
            return match opened {
                Some(line) => Err(format!("line {line}: block not closed with }}")),
                None => Ok(directives),
            };
        };
        if first.is("}") {
            let line: usize = first.line;
            tokens.next();
            return match opened {
                Some(_) => Ok(directives),
                None => Err(format!("line {line}: unexpected }}")),
            };
        }
        let line: usize = first.line;
        let mut words: Vec<String> = Vec::new();
        let mut block: Option<Vec<Directive>> = None;
        while let Some(token) = tokens.peek() {
            if token.is("}") || syntax == Syntax::Caddyfile && token.line != line {
                break;
            }
            let token: Token = tokens.next().expect("peeked");
            if token.is("{") {
                block = Some(parse_block(tokens, syntax, Some(token.line))?);
                break;
            }
            if syntax == Syntax::Nginx && token.is(";") {
                break;
            }
            words.push(token.text);
        }
        if syntax == Syntax::Nginx && block.is_none() && words.is_empty() {
            continue;
        }
        let mut words = words.into_iter();
        directives.push(Directive {
            name: words.next().unwrap_or_default(),
            args: words.collect(),
            block,
            line,
        });
    }
}

fn tokenize(text: &str, syntax: Syntax) -> Result<Vec<Token>, String> {
    let (quotes, specials): (&[char], &[char]) = match syntax {
        Syntax::Caddyfile => (&['"', '`'], &[]),
        Syntax::Nginx => (&['"', '\''], &['{', '}', ';']),
    };
    let mut tokens: Vec<Token> = Vec::new();
    let mut chars = text.chars().peekable();
    let mut line: usize = 1;
    while let Some(c) = chars.next() {
        match c {
            '\n' => line += 1,
            c if c.is_whitespace() => {}
            '#' => while chars.next_if(|&c| c != '\n').is_some() {},
            c if specials.contains(&c) => tokens.push(Token {
                text: c.to_string(),
                quoted: false,
                line,
            }),
            c if quotes.contains(&c) => {
                let start: usize = line;
                let mut word = String::new();
                loop {
                    match chars.next() {
                        None => return Err(format!("line {start}: quote not closed")),
                        Some(end) if end == c => break,
                        Some('\\') if c != '`' => match chars.next() {
                            Some(escaped) if escaped == c || escaped == '\\' => word.push(escaped),
                            Some(other) => {
                                word.push('\\');
                                word.push(other);
                            }
                            None => return Err(format!("line {start}: quote not closed")),
                        },
                        Some(other) => {
                            if other == '\n' {
                                line += 1;
                            }
                            word.push(other);
                        }
                    }
                }
                tokens.push(Token {
                    text: word,
                    quoted: true,
                    line: start,
                });
            }
            c => {
                let mut word = String::from(c);
                while let Some(next) =
                    chars.next_if(|&next| !next.is_whitespace() && !specials.contains(&next))
                {
                    word.push(next);
                }
                tokens.push(Token {
                    text: word,
                    quoted: false,
                    line,
                });
            }
        }
    }
    Ok(tokens)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(directives: &[Directive]) -> Vec<(&str, Vec<&str>, usize)> {
        directives
            .iter()
            .map(|d| {
                (
                    d.name.as_str(),
                    d.args.iter().map(String::as_str).collect(),
                    d.line,
                )
            })
            .collect()
    }

    #[test]
    fn caddyfile_lines_blocks_quotes_and_comments() {
        let text = "{\n\temail a@b.c\n}\n\nexample.com, www.example.com {\n\t# comment\n\
                    \theader X-Name \"a {b} c\" # trailing\n\treverse_proxy /api/* :3000 {\n\
                    \t\tlb_policy first\n\t}\n}\n";
        let directives = parse(text, Syntax::Caddyfile).unwrap();
        assert_eq!(
            names(&directives),
            [
                ("", vec![], 1),
                ("example.com,", vec!["www.example.com"], 5)
            ]
        );
        assert_eq!(
            names(directives[0].children()),
            [("email", vec!["a@b.c"], 2)]
        );
        let site = directives[1].children();
        assert_eq!(
            names(site),
            [
                ("header", vec!["X-Name", "a {b} c"], 7),
                ("reverse_proxy", vec!["/api/*", ":3000"], 8)
            ]
        );
        assert_eq!(names(site[1].children()), [("lb_policy", vec!["first"], 9)]);
        assert!(site[0].block.is_none());
    }

    #[test]
    fn nginx_statements_and_blocks() {
        let text = "http{\n  server {listen 80;server_name 'a b';\n location / { return 301 \
                    https://$host$request_uri; } }\n}";
        let directives = parse(text, Syntax::Nginx).unwrap();
        let server = &directives[0].children()[0];
        assert_eq!(
            names(server.children())[..2],
            [("listen", vec!["80"], 2), ("server_name", vec!["a b"], 2)]
        );
        let location = &server.children()[2];
        assert_eq!(location.args, ["/"]);
        assert_eq!(
            names(location.children()),
            [("return", vec!["301", "https://$host$request_uri"], 3)]
        );
    }

    #[test]
    fn unbalanced_input_is_an_error() {
        assert!(
            parse("a {\n b\n", Syntax::Caddyfile)
                .unwrap_err()
                .contains("line 1")
        );
        assert!(parse("}\n", Syntax::Caddyfile).is_err());
        assert!(parse("server { listen 80; ", Syntax::Nginx).is_err());
        assert!(
            parse("a \"b\n", Syntax::Nginx)
                .unwrap_err()
                .contains("quote")
        );
    }
}
