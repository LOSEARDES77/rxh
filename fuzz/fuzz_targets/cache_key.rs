//! Response cache keys: two requests share a key only when they have the
//! same pool, host and URI, whatever bytes the client puts in them.

#![no_main]

use arbitrary::Arbitrary;
use http::Uri;
use libfuzzer_sys::fuzz_target;
use rxh_server::fuzzing;

#[derive(Arbitrary, Debug)]
struct Request<'a> {
    pool: &'a str,
    host: &'a str,
    uri: &'a str,
}

fuzz_target!(|requests: (Request<'_>, Request<'_>)| {
    let (a, b) = requests;
    let (Ok(a_uri), Ok(b_uri)) = (a.uri.parse::<Uri>(), b.uri.parse::<Uri>()) else {
        return;
    };
    if fuzzing::cache_key(a.pool, a.host, &a_uri) == fuzzing::cache_key(b.pool, b.host, &b_uri) {
        assert_eq!(
            (a.pool, a.host, a_uri.to_string()),
            (b.pool, b.host, b_uri.to_string()),
            "different requests must not share a cache entry"
        );
    }
});
