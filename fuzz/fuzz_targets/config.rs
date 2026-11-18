//! Configuration parser: any text either parses and validates or fails
//! with an error, and a valid configuration compiles its matchers without
//! panicking.

#![no_main]

use libfuzzer_sys::fuzz_target;
use rxh_core::config::{Config, IncludedConfig};
use rxh_core::matcher::RequestMatcher;

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    let _ = IncludedConfig::from_toml(text);
    let Ok(config) = Config::from_toml(text) else {
        return;
    };
    let _ = config.with_https_redirects();
    for group in config.upstreams.values() {
        let _ = group.expanded_backends();
    }
    for server in &config.servers {
        for pattern in &server.patterns {
            let _ = RequestMatcher::new(pattern, &server.host);
        }
    }
});
