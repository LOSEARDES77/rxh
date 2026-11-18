//! Layer 4 protocol sniffing (TLS ClientHello, HTTP/1, SSH) of the first
//! bytes of a connection never panics.

#![no_main]

use libfuzzer_sys::fuzz_target;
use rxh_server::fuzzing;

fuzz_target!(|bytes: &[u8]| {
    let _ = fuzzing::l4_detect(bytes);
});
