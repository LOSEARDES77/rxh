//! OCSP stapling: asks the OCSP responder named in each certificate of a
//! TLS listener whether the certificate is still good, and hands the answer
//! to the clients in the handshake, so they do not have to ask themselves.
//!
//! The answers are refreshed in the background halfway through their
//! validity. A responder that fails is tried again on the next round; the
//! last answer stays stapled until it expires.

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use aws_lc_rs::digest;
use bytes::Bytes;
use http_body_util::{BodyExt, Full, Limited};
use hyper::header::{self, HeaderValue};
use hyper::{Request, Uri};
use hyper_util::rt::TokioIo;
use tokio::net::TcpStream;
use tokio_rustls::rustls::pki_types::CertificateDer;
use tokio_rustls::rustls::sign::CertifiedKey;
use x509_parser::asn1_rs::{Any, Class, FromDer, GeneralizedTime, Tag};
use x509_parser::extensions::{GeneralName, ParsedExtension};
use x509_parser::oid_registry::OID_PKIX_ACCESS_DESCRIPTOR_OCSP;

use crate::tls::CertResolver;

/// Time between two rounds over the certificates of a listener.
const CHECK_INTERVAL: Duration = Duration::from_secs(3600);

/// Time a responder has to answer.
const FETCH_TIMEOUT: Duration = Duration::from_secs(10);

/// Largest response accepted; real ones take a few kilobytes at most.
const MAX_RESPONSE: usize = 64 * 1024;

const SEQUENCE: u8 = 0x30;
const OCTET_STRING: u8 = 0x04;
const INTEGER: u8 = 0x02;

/// `AlgorithmIdentifier` of SHA-1, the only hash every responder accepts in
/// a request (RFC 5019, section 2.1.1).
const SHA1: [u8; 11] = [
    SEQUENCE, 0x09, 0x06, 0x05, 0x2b, 0x0e, 0x03, 0x02, 0x1a, 0x05, 0x00,
];

const TRUNCATED: &str = "truncated response";

/// Keeps the certificates of `resolver` stapled. Runs until dropped.
pub(crate) async fn run(resolver: Arc<CertResolver>) {
    let mut interval = tokio::time::interval(CHECK_INTERVAL);
    loop {
        interval.tick().await;
        round(&resolver).await;
    }
}

/// Staples an answer to every certificate of `resolver` that has none or an
/// aging one. Certificates set since the last round (ACME) are picked up
/// here.
async fn round(resolver: &CertResolver) {
    for key in resolver.keys() {
        let Some(query) = Query::new(&key.cert) else {
            continue;
        };
        let stapled = key
            .ocsp
            .as_deref()
            .and_then(|staple| validity(staple, &query.serial).ok());
        if stapled.is_some_and(|stapled| stapled.is_fresh(unix_now())) {
            continue;
        }
        let ocsp = match query.fetch().await {
            Ok(response) => Some(response),
            Err(err) => {
                eprintln!("OCSP => {}: {err}", query.url);
                if stapled.is_some_and(|stapled| !stapled.is_expired(unix_now())) {
                    continue;
                }
                None
            }
        };
        if ocsp != key.ocsp {
            let stapled = CertifiedKey {
                ocsp,
                ..CertifiedKey::clone(&key)
            };
            resolver.replace(&key, Arc::new(stapled));
        }
    }
}

fn unix_now() -> i64 {
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .expect("the clock is past 1970");
    i64::try_from(now.as_secs()).expect("the clock is before the year 292 billion")
}

/// DER encoding of one element.
fn der(tag: u8, content: &[u8]) -> Vec<u8> {
    let len = u16::try_from(content.len()).expect("OCSP requests are short");
    let mut out = vec![tag];
    match len.to_be_bytes() {
        [0, low] if low < 0x80 => out.push(low),
        [0, low] => out.extend([0x81, low]),
        [high, low] => out.extend([0x82, high, low]),
    }
    out.extend(content);
    out
}

/// The OCSP request for one certificate.
#[derive(Debug)]
struct Query {
    /// The responder, an `http://` URL.
    url: Uri,
    /// Serial number of the certificate, as in its DER.
    serial: Vec<u8>,
    /// DER of the `OCSPRequest`.
    request: Vec<u8>,
}

impl Query {
    /// Query for the first certificate of `chain`, or `None` if it names no
    /// `http://` responder or `chain` lacks the certificate of its issuer.
    fn new(chain: &[CertificateDer<'_>]) -> Option<Self> {
        let (_, leaf) = x509_parser::parse_x509_certificate(chain.first()?).ok()?;
        let (_, issuer) = x509_parser::parse_x509_certificate(chain.get(1)?).ok()?;
        let url: Uri = leaf.extensions().iter().find_map(|extension| {
            let ParsedExtension::AuthorityInfoAccess(access) = extension.parsed_extension() else {
                return None;
            };
            access.iter().find_map(|description| {
                match (&description.access_method, &description.access_location) {
                    (method, GeneralName::URI(url))
                        if *method == OID_PKIX_ACCESS_DESCRIPTOR_OCSP =>
                    {
                        url.parse().ok()
                    }
                    _ => None,
                }
            })
        })?;
        if url.scheme_str() != Some("http") || url.host().is_none() {
            return None;
        }

        let sha1 = |data: &[u8]| {
            let hash = digest::digest(&digest::SHA1_FOR_LEGACY_USE_ONLY, data);
            der(OCTET_STRING, hash.as_ref())
        };
        let serial = leaf.raw_serial().to_vec();
        let cert_id = [
            &SHA1[..],
            &sha1(leaf.issuer().as_raw()),
            &sha1(&issuer.public_key().subject_public_key.data),
            &der(INTEGER, &serial),
        ]
        .concat();
        // OCSPRequest { TBSRequest { requestList { Request { CertID } } } }
        let mut request = der(SEQUENCE, &cert_id);
        for _ in 0..4 {
            request = der(SEQUENCE, &request);
        }
        Some(Self {
            url,
            serial,
            request,
        })
    }

    /// Asks the responder; returns its response if it says the certificate
    /// is good and has not expired.
    async fn fetch(&self) -> Result<Vec<u8>, String> {
        let response = tokio::time::timeout(FETCH_TIMEOUT, self.post())
            .await
            .map_err(|_| String::from("the responder timed out"))??;
        if validity(&response, &self.serial)?.is_expired(unix_now()) {
            return Err(String::from("the response has expired"));
        }
        Ok(response.to_vec())
    }

    async fn post(&self) -> Result<Bytes, String> {
        let host = self.url.host().expect("checked by Query::new");
        let port = self.url.port_u16().unwrap_or(80);
        let stream = TcpStream::connect((host.trim_matches(['[', ']']), port))
            .await
            .map_err(|err| err.to_string())?;
        let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
            .await
            .map_err(|err| err.to_string())?;
        tokio::task::spawn(connection);

        let path = self.url.path_and_query().map_or("/", |path| path.as_str());
        let authority = self.url.authority().expect("a URL with a host");
        let request = Request::post(path)
            .header(header::HOST, authority.as_str())
            .header(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/ocsp-request"),
            )
            .body(Full::new(Bytes::from(self.request.clone())))
            .map_err(|err| err.to_string())?;
        let response = sender
            .send_request(request)
            .await
            .map_err(|err| err.to_string())?;
        if !response.status().is_success() {
            return Err(format!("status {}", response.status()));
        }
        let body = Limited::new(response.into_body(), MAX_RESPONSE)
            .collect()
            .await
            .map_err(|err| err.to_string())?;
        Ok(body.to_bytes())
    }
}

/// When an OCSP response was made and until when it holds, in seconds since
/// the Unix epoch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Validity {
    this_update: i64,
    /// `None` when the responder promises nothing: such a response is
    /// stapled and fetched again on every round.
    next_update: Option<i64>,
}

impl Validity {
    /// True in the first half of the validity: no need to ask again yet.
    fn is_fresh(self, now: i64) -> bool {
        self.next_update
            .is_some_and(|next| now < self.this_update + (next - self.this_update) / 2)
    }

    fn is_expired(self, now: i64) -> bool {
        self.next_update.is_some_and(|next| now >= next)
    }
}

/// The DER elements `data` is made of.
fn children(mut data: &[u8]) -> Result<Vec<Any<'_>>, String> {
    let mut items = Vec::new();
    while !data.is_empty() {
        let (rest, item) =
            Any::from_der(data).map_err(|err| format!("malformed response: {err}"))?;
        items.push(item);
        data = rest;
    }
    Ok(items)
}

/// The elements inside `items[index]`.
fn inside<'a>(items: &[Any<'a>], index: usize) -> Result<Vec<Any<'a>>, String> {
    children(items.get(index).ok_or(TRUNCATED)?.data)
}

fn time(item: Option<&Any<'_>>) -> Result<i64, String> {
    let time = GeneralizedTime::try_from(item.ok_or(TRUNCATED)?)
        .and_then(|time| time.utc_datetime())
        .map_err(|err| format!("malformed time: {err}"))?;
    Ok(time.unix_timestamp())
}

/// Validity of the OCSP `response` (RFC 6960, section 4.2.1) for the
/// certificate with the serial number `serial`.
///
/// shortcut: the signature of the response is not verified (like Nginx
/// without `ssl_stapling_verify`): clients verify it, so a forged response
/// can only make them refuse the handshake. Verify it here if responders
/// are reached over networks an attacker could tamper with.
///
/// # Errors
///
/// Fails unless the response is well formed and says the certificate is
/// good.
fn validity(response: &[u8], serial: &[u8]) -> Result<Validity, String> {
    let response = inside(&children(response)?, 0)?;
    if response.first().map(|status| status.data) != Some(&[0]) {
        return Err(String::from("the responder answered with an error"));
    }
    // [0] { ResponseBytes { type, OCTET STRING { BasicOCSPResponse } } }
    let bytes = inside(&inside(&response, 1)?, 0)?;
    let basic = inside(&inside(&bytes, 1)?, 0)?;
    // ResponseData: the optional version and the responder are context
    // tagged and producedAt is a time, so the responses are its only
    // sequence.
    let data = inside(&basic, 0)?;
    let responses = data
        .iter()
        .find(|field| field.class() == Class::Universal && field.tag() == Tag::Sequence)
        .ok_or(TRUNCATED)?;
    for single in children(responses.data)? {
        let fields = children(single.data)?;
        let cert_id = inside(&fields, 0)?;
        if cert_id.get(3).map(|number| number.data) != Some(serial) {
            continue;
        }
        // CertStatus: good [0], revoked [1], unknown [2].
        match fields.get(1).ok_or(TRUNCATED)?.tag().0 {
            0 => {}
            1 => return Err(String::from("the certificate is revoked")),
            _ => return Err(String::from("the responder does not know the certificate")),
        }
        let this_update = time(fields.get(2))?;
        let next_update = fields
            .get(3)
            .filter(|field| field.class() == Class::ContextSpecific && field.tag().0 == 0);
        let next_update = match next_update {
            Some(next) => Some(time(children(next.data)?.first())?),
            None => None,
        };
        return Ok(Validity {
            this_update,
            next_update,
        });
    }
    Err(String::from("no answer for the certificate"))
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use hyper::Response;
    use hyper::server::conn::http1;
    use hyper::service::service_fn;
    use rxh_test_util::{OcspCertificate, ocsp_response};
    use tokio::net::TcpListener;
    use tokio_rustls::rustls::ServerConfig;

    use super::*;

    const SERIAL: [u8; 3] = [0x01, 0x02, 0x03];
    /// 2030-01-01 00:00:00 UTC.
    const THIS_UPDATE: i64 = 1_893_456_000;
    const DAY: i64 = 86_400;

    fn good() -> Vec<u8> {
        ocsp_response(&SERIAL, true, "20300101000000Z", Some("20300105000000Z"))
    }

    #[test]
    fn validity_of_a_good_response() {
        let validity = validity(&good(), &SERIAL).unwrap();
        let expected = Validity {
            this_update: THIS_UPDATE,
            next_update: Some(THIS_UPDATE + 4 * DAY),
        };
        assert_eq!(validity, expected);

        // Refreshed from the middle of the validity, dropped at its end.
        assert!(validity.is_fresh(THIS_UPDATE + 2 * DAY - 1));
        assert!(!validity.is_fresh(THIS_UPDATE + 2 * DAY));
        assert!(!validity.is_expired(THIS_UPDATE + 4 * DAY - 1));
        assert!(validity.is_expired(THIS_UPDATE + 4 * DAY));
    }

    #[test]
    fn response_without_next_update_is_never_fresh_nor_expired() {
        let response = ocsp_response(&SERIAL, true, "20300101000000Z", None);
        let validity = validity(&response, &SERIAL).unwrap();
        assert_eq!(validity.next_update, None);
        assert!(!validity.is_fresh(THIS_UPDATE));
        assert!(!validity.is_expired(THIS_UPDATE + 1000 * DAY));
    }

    #[test]
    fn bad_responses_are_rejected() {
        let error = |response: &[u8], serial: &[u8]| validity(response, serial).unwrap_err();

        let revoked = ocsp_response(&SERIAL, false, "20300101000000Z", None);
        assert_eq!(error(&revoked, &SERIAL), "the certificate is revoked");
        assert_eq!(error(&good(), &[9]), "no answer for the certificate");
        // OCSPResponse { responseStatus tryLater } has no response bytes.
        let try_later = [SEQUENCE, 0x03, 0x0a, 0x01, 0x03];
        assert_eq!(
            error(&try_later, &SERIAL),
            "the responder answered with an error"
        );
        assert_eq!(error(&[], &SERIAL), TRUNCATED);
        assert!(error(b"<html>busy</html>", &SERIAL).contains("malformed"));
        let bad_time = ocsp_response(&SERIAL, true, "yesterday", None);
        assert!(error(&bad_time, &SERIAL).contains("malformed time"));
        // Cut anywhere, a response is an error and never a panic.
        let good = good();
        for len in 0..good.len() {
            assert!(validity(&good[..len], &SERIAL).is_err(), "cut at {len}");
        }
    }

    /// An OCSP responder that answers with `answer`, or with a 500 when
    /// there is none. Returns its URL and the number of requests it got.
    async fn responder(answer: Arc<Mutex<Option<Vec<u8>>>>) -> (String, Arc<AtomicUsize>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        let requests = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&requests);
        tokio::task::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let (answer, counter) = (Arc::clone(&answer), Arc::clone(&counter));
                let service = service_fn(move |_request| {
                    counter.fetch_add(1, Ordering::Relaxed);
                    let answer = answer.lock().unwrap().clone();
                    let status = if answer.is_some() { 200 } else { 500 };
                    let body = Full::new(Bytes::from(answer.unwrap_or_default()));
                    async move { Response::builder().status(status).body(body) }
                });
                tokio::task::spawn(
                    http1::Builder::new().serve_connection(TokioIo::new(stream), service),
                );
            }
        });
        (url, requests)
    }

    #[tokio::test]
    async fn round_staples_and_keeps_an_answer_until_it_expires() {
        const SERIAL: [u8; 3] = OcspCertificate::SERIAL;
        let answer = Arc::new(Mutex::new(None));
        let (url, requests) = responder(Arc::clone(&answer)).await;
        let certificate = OcspCertificate::new(Some(&url));
        let provider = Arc::clone(ServerConfig::builder().crypto_provider());
        let resolver = CertResolver::new(provider);
        let key = resolver
            .certified_key(
                certificate.chain_pem.as_bytes(),
                certificate.key_pem.as_bytes(),
            )
            .unwrap();
        resolver.set(&[], key).unwrap();
        let stapled = || resolver.select(None).unwrap().ocsp.clone();

        // The responder fails: nothing to staple.
        round(&resolver).await;
        assert_eq!(stapled(), None);
        assert_eq!(requests.load(Ordering::Relaxed), 1);

        // An answer without an end is stapled, and asked for again.
        let endless = ocsp_response(&SERIAL, true, "20200101000000Z", None);
        *answer.lock().unwrap() = Some(endless.clone());
        round(&resolver).await;
        assert_eq!(stapled(), Some(endless.clone()));
        *answer.lock().unwrap() = None;
        round(&resolver).await;
        assert_eq!(stapled(), Some(endless), "kept while the responder fails");
        assert_eq!(requests.load(Ordering::Relaxed), 3);

        // In the first half of its validity an answer is not asked for again.
        let fresh = ocsp_response(&SERIAL, true, "20200101000000Z", Some("29990101000000Z"));
        *answer.lock().unwrap() = Some(fresh.clone());
        round(&resolver).await;
        *answer.lock().unwrap() = None;
        round(&resolver).await;
        assert_eq!(stapled(), Some(fresh));
        assert_eq!(requests.load(Ordering::Relaxed), 4);

        // An expired answer goes away when the responder has no new one.
        let expired = ocsp_response(&SERIAL, true, "20200101000000Z", Some("20200102000000Z"));
        let current = resolver.select(None).unwrap();
        let aged = CertifiedKey {
            ocsp: Some(expired),
            ..CertifiedKey::clone(&current)
        };
        resolver.replace(&current, Arc::new(aged));
        round(&resolver).await;
        assert_eq!(stapled(), None);
        assert_eq!(requests.load(Ordering::Relaxed), 5);
    }

    #[test]
    fn certificates_without_responder_or_issuer_are_skipped() {
        use tokio_rustls::rustls::pki_types::pem::PemObject;

        let chain = |certificate: &OcspCertificate| -> Vec<CertificateDer<'static>> {
            CertificateDer::pem_slice_iter(certificate.chain_pem.as_bytes())
                .collect::<Result<_, _>>()
                .unwrap()
        };
        let with_responder = chain(&OcspCertificate::new(Some("http://ocsp.test/")));
        let query = Query::new(&with_responder).unwrap();
        assert_eq!(query.url, "http://ocsp.test/");
        assert_eq!(query.serial, OcspCertificate::SERIAL);
        assert!(query.request.ends_with(&der(INTEGER, &query.serial)));

        assert!(Query::new(&with_responder[..1]).is_none(), "no issuer");
        assert!(Query::new(&chain(&OcspCertificate::new(None))).is_none());
        let https = OcspCertificate::new(Some("https://ocsp.test/"));
        assert!(Query::new(&chain(&https)).is_none(), "http only");
    }

    #[test]
    fn der_lengths() {
        assert_eq!(der(INTEGER, &[7]), [INTEGER, 1, 7]);
        assert_eq!(
            der(OCTET_STRING, &[0; 0x80])[..3],
            [OCTET_STRING, 0x81, 0x80]
        );
        assert_eq!(
            der(OCTET_STRING, &[0; 0x101])[..4],
            [OCTET_STRING, 0x82, 0x01, 0x01]
        );
    }
}
