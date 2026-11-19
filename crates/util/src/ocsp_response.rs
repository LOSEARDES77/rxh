//! Hand-made OCSP responses, for tests of OCSP stapling.

/// DER encoding of one element.
fn der(tag: u8, content: &[u8]) -> Vec<u8> {
    let len = u16::try_from(content.len()).expect("test elements are short");
    let mut out = vec![tag];
    match len.to_be_bytes() {
        [0, low] if low < 0x80 => out.push(low),
        [0, low] => out.extend([0x81, low]),
        [high, low] => out.extend([0x82, high, low]),
    }
    out.extend(content);
    out
}

/// An OCSP response (RFC 6960) for the certificate with the serial number
/// `serial`, saying it is good or revoked. The times are `GeneralizedTime`
/// strings such as `20300101000000Z`. The signature is empty: nothing can
/// verify it.
#[must_use]
pub fn ocsp_response(
    serial: &[u8],
    good: bool,
    this_update: &str,
    next_update: Option<&str>,
) -> Vec<u8> {
    const SEQUENCE: u8 = 0x30;
    const OCTET_STRING: u8 = 0x04;
    const TIME: u8 = 0x18;
    const SHA1: [u8; 11] = [
        0x30, 0x09, 0x06, 0x05, 0x2b, 0x0e, 0x03, 0x02, 0x1a, 0x05, 0x00,
    ];
    const SHA256_WITH_RSA: [u8; 15] = [
        0x30, 0x0d, 0x06, 0x09, 0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x0b, 0x05, 0x00,
    ];
    const BASIC: [u8; 11] = [
        0x06, 0x09, 0x2b, 0x06, 0x01, 0x05, 0x05, 0x07, 0x30, 0x01, 0x01,
    ];

    let hash = der(OCTET_STRING, &[0; 20]);
    let cert_id = [&SHA1[..], &hash, &hash, &der(0x02, serial)].concat();
    let this_update = der(TIME, this_update.as_bytes());
    let status = if good {
        der(0x80, &[])
    } else {
        der(0xa1, &this_update)
    };
    let next_update =
        next_update.map_or_else(Vec::new, |next| der(0xa0, &der(TIME, next.as_bytes())));
    let single = [
        der(SEQUENCE, &cert_id),
        status,
        this_update.clone(),
        next_update,
    ]
    .concat();
    let data = [
        der(0xa2, &hash),
        this_update,
        der(SEQUENCE, &der(SEQUENCE, &single)),
    ]
    .concat();
    let basic = [
        &der(SEQUENCE, &data),
        &SHA256_WITH_RSA[..],
        &[0x03, 0x01, 0x00],
    ]
    .concat();
    let bytes = [&BASIC[..], &der(OCTET_STRING, &der(SEQUENCE, &basic))].concat();
    let successful = [0x0a, 0x01, 0x00];
    der(
        SEQUENCE,
        &[&successful[..], &der(0xa0, &der(SEQUENCE, &bytes))].concat(),
    )
}
