//! Host and path matchers: compiling and matching never panic, and a
//! pattern matches every name or path it describes.

#![no_main]

use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;
use rxh_core::matcher::{HostPattern, PathPattern};

#[derive(Arbitrary, Debug)]
struct Input<'a> {
    host_pattern: &'a str,
    host: &'a str,
    /// Literal pieces of a path pattern, joined with `*`.
    path_parts: Vec<&'a str>,
    /// What each `*` stands for in a path the pattern must match.
    fillers: Vec<&'a str>,
    path: &'a str,
}

fuzz_target!(|input: Input<'_>| {
    host(input.host_pattern, input.host);
    path(&input.path_parts, &input.fillers, input.path);
});

fn host(pattern: &str, host: &str) {
    let Ok(matcher) = HostPattern::new(pattern) else {
        return;
    };
    let _ = matcher.matches(host);
    // A `:` or `[` in the pattern reads as a port or an IPv6 literal.
    if pattern.contains([':', '[']) {
        return;
    }
    if matcher.is_wildcard() {
        let domain = &pattern[2..];
        assert!(
            matcher.matches(&format!("sub.{domain}")),
            "a wildcard must match a subdomain"
        );
        assert!(
            !matcher.matches(domain),
            "a wildcard must not match the bare domain"
        );
    } else {
        assert!(
            matcher.matches(&format!("{}:8080", pattern.to_ascii_uppercase())),
            "an exact name must match itself in any case and with a port"
        );
    }
}

fn path(parts: &[&str], fillers: &[&str], path: &str) {
    if parts.is_empty() || parts.iter().any(|part| part.contains('*')) {
        return;
    }
    let pattern = parts.join("*");
    let Ok(matcher) = PathPattern::new(&pattern) else {
        return;
    };
    if parts.len() == 1 {
        assert_eq!(
            matcher.matches(path),
            path == pattern,
            "a pattern without `*` matches only itself"
        );
    } else {
        let _ = matcher.matches(path);
    }

    let mut expanded = String::from(parts[0]);
    for (index, part) in parts[1..].iter().enumerate() {
        expanded.push_str(fillers.get(index).copied().unwrap_or_default());
        expanded.push_str(part);
    }
    assert!(
        matcher.matches(&expanded),
        "{pattern:?} must match {expanded:?}"
    );
}
