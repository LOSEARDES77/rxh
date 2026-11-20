//! Checks the documentation site in `docs/`: every `toml` block of the guides
//! is a complete configuration that RXH accepts, and every relative link of
//! the site's own pages leads to an existing file and heading.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use regex::Regex;
use rxh_core::config::Config;

/// Guides, by path relative to `docs/`. Included at compile time so a guide
/// listed here cannot go missing.
const GUIDES: [(&str, &str); 4] = [
    (
        "guides/load-balancer.md",
        include_str!("../../../docs/guides/load-balancer.md"),
    ),
    (
        "guides/tls-gateway.md",
        include_str!("../../../docs/guides/tls-gateway.md"),
    ),
    (
        "guides/docker.md",
        include_str!("../../../docs/guides/docker.md"),
    ),
    (
        "guides/layer4.md",
        include_str!("../../../docs/guides/layer4.md"),
    ),
];

fn docs_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs")
}

/// Contents of the fenced blocks of `markdown` whose info string is `lang`.
fn code_blocks(markdown: &str, lang: &str) -> Vec<String> {
    let mut blocks: Vec<String> = Vec::new();
    // The open block, if any: whether it is in `lang`, and its contents.
    let mut open: Option<(bool, String)> = None;
    for line in markdown.lines() {
        let fence = line.trim_start().starts_with("```");
        match open.as_mut() {
            None if fence => {
                let info = line.trim_start().trim_start_matches('`');
                open = Some((info == lang, String::new()));
            }
            None => {}
            Some(_) if fence => {
                if let Some((true, block)) = open.take() {
                    blocks.push(block);
                }
            }
            Some((_, block)) => {
                block.push_str(line);
                block.push('\n');
            }
        }
    }
    blocks
}

/// Lines of `markdown` outside fenced code blocks.
fn prose_lines(markdown: &str) -> Vec<&str> {
    let mut in_fence = false;
    let mut lines: Vec<&str> = Vec::new();
    for line in markdown.lines() {
        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
        } else if !in_fence {
            lines.push(line);
        }
    }
    lines
}

/// Anchors of the headings of `markdown`, as GitHub and mdBook make them:
/// lower case, spaces to `-`, punctuation other than `-` and `_` dropped, and
/// `-1`, `-2`, ... appended to repeated ones.
fn anchors(markdown: &str) -> Vec<String> {
    let mut seen: HashMap<String, usize> = HashMap::new();
    let mut anchors: Vec<String> = Vec::new();
    for line in prose_lines(markdown) {
        let Some(title) = line.strip_prefix('#') else {
            continue;
        };
        let title = title.trim_start_matches('#').trim();
        let slug: String = title
            .chars()
            .filter_map(|c| match c {
                ' ' => Some('-'),
                '-' | '_' => Some(c),
                c if c.is_alphanumeric() => Some(c.to_ascii_lowercase()),
                _ => None,
            })
            .collect();
        let count = seen.entry(slug.clone()).or_insert(0);
        anchors.push(if *count == 0 {
            slug
        } else {
            format!("{slug}-{count}")
        });
        *count += 1;
    }
    anchors
}

#[test]
fn guide_configurations_are_valid() {
    for (name, text) in GUIDES {
        let blocks = code_blocks(text, "toml");
        assert!(
            !blocks.is_empty(),
            "a guide teaches with configurations, {name} has none"
        );
        for block in blocks {
            if let Err(err) = Config::from_toml(&block) {
                panic!("{name}: a configuration of the guide is rejected: {err}\n{block}");
            }
        }
    }
}

#[test]
fn site_links_resolve() {
    let docs = docs_dir();
    let link = Regex::new(r"\]\(([^)\s]+)\)").expect("the pattern is a valid regex");
    let mut pages: Vec<(String, String)> = vec![
        (
            "SUMMARY.md".to_owned(),
            include_str!("../../../docs/SUMMARY.md").to_owned(),
        ),
        (
            "README.md".to_owned(),
            include_str!("../../../docs/README.md").to_owned(),
        ),
    ];
    for (name, text) in GUIDES {
        pages.push((name.to_owned(), text.to_owned()));
    }

    let mut broken: Vec<String> = Vec::new();
    for (name, text) in &pages {
        let page = docs.join(name);
        let base = page.parent().expect("a page is inside docs/");
        for line in prose_lines(text) {
            for captures in link.captures_iter(line) {
                let target = &captures[1];
                if target.contains("://") || target.starts_with("mailto:") {
                    continue;
                }
                let (path, anchor) = target.split_once('#').unwrap_or((target, ""));
                let file = if path.is_empty() {
                    page.clone()
                } else {
                    base.join(path)
                };
                let Ok(contents) = fs::read_to_string(&file) else {
                    broken.push(format!("{name}: {target}: no such file"));
                    continue;
                };
                if !anchor.is_empty()
                    && file.extension().is_some_and(|ext| ext == "md")
                    && !anchors(&contents).iter().any(|known| known == anchor)
                {
                    broken.push(format!("{name}: {target}: no such heading"));
                }
            }
        }
    }
    assert!(broken.is_empty(), "broken links:\n{}", broken.join("\n"));
}

#[test]
fn summary_lists_every_guide() {
    let summary = include_str!("../../../docs/SUMMARY.md");
    for (name, _) in GUIDES {
        assert!(
            summary.contains(&format!("({name})")),
            "{name} must be in SUMMARY.md, or the site does not show it"
        );
    }
}

#[test]
fn anchors_follow_github_rules() {
    let markdown = "# `[upstream.NAME]`: pools\n```toml\n# not a heading\n```\n## A b\n## A b\n";
    assert_eq!(anchors(markdown), ["upstreamname-pools", "a-b", "a-b-1"]);
}

#[test]
fn code_blocks_keep_only_the_language() {
    let markdown = "```sh\nrxh run\n```\n```toml\na = 1\n```\n";
    assert_eq!(code_blocks(markdown, "toml"), ["a = 1\n"]);
}
