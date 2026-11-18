//! Resolves the top level `include` list of a configuration file.

use std::io;
use std::path::{Path, PathBuf};

/// Files named by `patterns`, relative to the directory of `main`, sorted by
/// path without duplicates.
///
/// `*` and `?` are allowed only in the last path component. A pattern with
/// no wildcard must exist, a glob may match nothing.
pub(crate) fn expand(main: &Path, patterns: &[String]) -> io::Result<Vec<PathBuf>> {
    let base = main.parent().unwrap_or(Path::new(""));
    let mut files = Vec::new();

    for pattern in patterns {
        let path = base.join(pattern);
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| invalid(pattern, "is not a file path"))?;
        let directory = path.parent().unwrap_or(Path::new(""));

        if Path::new(pattern)
            .parent()
            .and_then(Path::to_str)
            .is_some_and(|text| text.contains(['*', '?']))
        {
            return Err(invalid(
                pattern,
                "has a wildcard outside the last component",
            ));
        }
        if !name.contains(['*', '?']) {
            if !path.is_file() {
                return Err(invalid(pattern, "does not exist"));
            }
            files.push(path);
            continue;
        }

        let entries = std::fs::read_dir(directory).map_err(|err| {
            invalid(
                pattern,
                &format!("cannot read {}: {err}", directory.display()),
            )
        })?;
        for entry in entries {
            let entry = entry?;
            let file_name = entry.file_name();
            if file_name
                .to_str()
                .is_some_and(|file_name| wildcard_match(name, file_name))
                && entry.path().is_file()
            {
                files.push(entry.path());
            }
        }
    }

    files.sort();
    files.dedup();
    Ok(files)
}

/// True if `file` (which may not exist yet) is named by one of `patterns`,
/// relative to the directory of `main`.
pub fn covers(main: &Path, patterns: &[String], file: &Path) -> bool {
    fn normal(path: &Path) -> PathBuf {
        path.components()
            .filter(|part| !matches!(part, std::path::Component::CurDir))
            .collect()
    }
    let base = main.parent().unwrap_or(Path::new(""));
    let file = normal(file);
    let Some(file_name) = file.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    patterns.iter().any(|pattern| {
        let path = normal(&base.join(pattern));
        path.parent() == file.parent()
            && path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| wildcard_match(name, file_name))
    })
}

fn invalid(pattern: &str, problem: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("include {pattern:?} {problem}"),
    )
}

/// Matches `text` against a pattern where `*` is any run of characters and
/// `?` is exactly one.
fn wildcard_match(pattern: &str, text: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let text: Vec<char> = text.chars().collect();
    let (mut p, mut t) = (0, 0);
    // Position after the last `*` and the text position it currently covers up to.
    let mut star: Option<(usize, usize)> = None;

    while t < text.len() {
        match pattern.get(p) {
            Some('*') => {
                star = Some((p + 1, t));
                p += 1;
            }
            Some(&c) if c == '?' || c == text[t] => {
                p += 1;
                t += 1;
            }
            _ => match star {
                Some((after, covered)) => {
                    star = Some((after, covered + 1));
                    p = after;
                    t = covered + 1;
                }
                None => return false,
            },
        }
    }
    pattern[p..].iter().all(|&c| c == '*')
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    #[test]
    fn wildcards() {
        assert!(wildcard_match("*.toml", "api.toml"));
        assert!(wildcard_match("a?c.toml", "abc.toml"));
        assert!(wildcard_match("*", ""));
        assert!(wildcard_match("a*b*c", "aXXbYYc"));
        assert!(!wildcard_match("*.toml", "api.toml.bak"));
        assert!(!wildcard_match("a?c", "ac"));
    }

    #[test]
    fn expands_relative_to_main_sorted() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::create_dir(dir.path().join("conf.d")).expect("mkdir");
        for name in [
            "conf.d/b.toml",
            "conf.d/a.toml",
            "conf.d/skip.txt",
            "extra.toml",
        ] {
            fs::write(dir.path().join(name), "").expect("write");
        }
        let main = dir.path().join("rxh.toml");
        let patterns = vec![String::from("extra.toml"), String::from("conf.d/*.toml")];
        let files = expand(&main, &patterns).expect("expands");
        let names: Vec<_> = files
            .iter()
            .map(|path| path.strip_prefix(dir.path()).expect("inside").to_owned())
            .collect();
        assert_eq!(
            names,
            [
                PathBuf::from("conf.d/a.toml"),
                PathBuf::from("conf.d/b.toml"),
                PathBuf::from("extra.toml")
            ]
        );

        assert!(
            expand(&main, &[String::from("conf.d/*.nothing")])
                .expect("ok")
                .is_empty()
        );
        assert!(expand(&main, &[String::from("missing.toml")]).is_err());
        assert!(expand(&main, &[String::from("*/a.toml")]).is_err());

        let globs = [String::from("conf.d/*.toml")];
        assert!(covers(&main, &globs, &dir.path().join("./conf.d/new.toml")));
        assert!(!covers(&main, &globs, &dir.path().join("other/new.toml")));
        assert!(!covers(&main, &globs, &dir.path().join("conf.d/new.txt")));
    }
}
