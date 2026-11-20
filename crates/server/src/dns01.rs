//! DNS providers of the ACME DNS-01 challenge: create and delete the TXT
//! record `_acme-challenge.NAME` through the Cloudflare API, the Route 53 API
//! or a DNS UPDATE (RFC 2136) signed with TSIG (RFC 8945, HMAC-SHA256).
//!
//! The zone of a record is found by asking the provider for every suffix of
//! the record name, longest first, so names in delegated subzones work.

use std::hash::{BuildHasher, RandomState};
use std::iter;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use bytes::Bytes;
use hmac::{Hmac, Mac};
use http_body_util::{BodyExt, Full};
use hyper::header::{self, HeaderName, HeaderValue};
use hyper::{Method, Request, StatusCode, Uri};
use hyper_util::rt::TokioIo;
use rxh_core::config::{AcmeDns, DnsProvider};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tokio_rustls::rustls::pki_types::ServerName;
use tokio_rustls::rustls::{ClientConfig, RootCertStore};

const CLOUDFLARE_API: &str = "https://api.cloudflare.com/client/v4";
const ROUTE53_API: &str = "https://route53.amazonaws.com";

/// Upper bound for one provider operation, zone lookup included.
const TIMEOUT: Duration = Duration::from_secs(60);

/// TTL of the challenge records, in seconds.
const TTL: u32 = 60;

const TYPE_SOA: u16 = 6;
const TYPE_TXT: u16 = 16;
const TYPE_TSIG: u16 = 250;
const CLASS_IN: u16 = 1;
/// In an update: delete the record with exactly this data.
const CLASS_NONE: u16 = 254;
const CLASS_ANY: u16 = 255;
const OPCODE_UPDATE: u8 = 5;

/// A published TXT record, kept to delete it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Record {
    /// Zone identifier at Cloudflare and Route 53, zone name for RFC 2136.
    zone: String,
    /// Record identifier at Cloudflare, unused elsewhere.
    id: String,
    name: String,
    value: String,
}

/// Creates the TXT record `name` with `value` at the provider of `dns`.
pub(crate) async fn publish(dns: &AcmeDns, name: &str, value: &str) -> Result<Record, String> {
    let work = async {
        match dns.provider {
            DnsProvider::Cloudflare => cloudflare_publish(dns, name, value).await,
            DnsProvider::Route53 => route53_publish(dns, name, value).await,
            DnsProvider::Rfc2136 => rfc2136_publish(dns, name, value).await,
        }
    };
    tokio::time::timeout(TIMEOUT, work)
        .await
        .unwrap_or_else(|_| Err(String::from("timed out")))
}

/// Deletes a record created by [`publish`] with the same `dns`.
pub(crate) async fn withdraw(dns: &AcmeDns, record: &Record) -> Result<(), String> {
    let work = async {
        match dns.provider {
            DnsProvider::Cloudflare => {
                let path = format!("/zones/{}/dns_records/{}", record.zone, record.id);
                cloudflare(dns, Method::DELETE, &path, None).await?;
                Ok(())
            }
            DnsProvider::Route53 => route53_change(dns, "DELETE", record).await,
            DnsProvider::Rfc2136 => rfc2136_update(dns, record, CLASS_NONE).await,
        }
    };
    tokio::time::timeout(TIMEOUT, work)
        .await
        .unwrap_or_else(|_| Err(String::from("timed out")))
}

/// `name` and its parents, longest first: the zones `name` may be in.
fn zone_candidates(name: &str) -> impl Iterator<Item = &str> {
    iter::successors(Some(name), |name| {
        name.split_once('.').map(|(_, parent)| parent)
    })
}

fn no_zone(name: &str) -> String {
    format!("no zone for {name} in this account")
}

/// Sends one HTTP/1.1 request to an `http://` or `https://` URL (system
/// independent web roots) and returns the status and the body.
async fn http_request(
    method: Method,
    url: &str,
    headers: &[(HeaderName, String)],
    body: String,
) -> Result<(StatusCode, String), String> {
    let uri: Uri = url.parse().map_err(|err| format!("{url}: {err}"))?;
    let authority = uri.authority().ok_or_else(|| format!("{url}: no host"))?;
    let host = authority.host().trim_matches(['[', ']']);
    let https = uri.scheme_str() == Some("https");
    let port = authority.port_u16().unwrap_or(if https { 443 } else { 80 });

    let mut request = Request::builder()
        .method(method)
        .uri(uri.path_and_query().map_or("/", |path| path.as_str()))
        .header(header::HOST, authority.as_str());
    for (name, value) in headers {
        let value = HeaderValue::from_str(value).map_err(|_| format!("invalid {name} value"))?;
        request = request.header(name, value);
    }
    let request = request
        .body(Full::new(Bytes::from(body)))
        .map_err(|err| err.to_string())?;

    let stream = TcpStream::connect((host, port))
        .await
        .map_err(|err| format!("connecting to {authority}: {err}"))?;
    if !https {
        return http_send(stream, request).await;
    }
    let mut roots = RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let config = ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let name = ServerName::try_from(host.to_owned()).map_err(|err| err.to_string())?;
    let stream = TlsConnector::from(Arc::new(config))
        .connect(name, stream)
        .await
        .map_err(|err| format!("TLS with {authority}: {err}"))?;
    http_send(stream, request).await
}

async fn http_send<S>(
    stream: S,
    request: Request<Full<Bytes>>,
) -> Result<(StatusCode, String), String>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .map_err(|err| err.to_string())?;
    // Ends when `sender` is dropped.
    tokio::task::spawn(connection);
    let response = sender
        .send_request(request)
        .await
        .map_err(|err| err.to_string())?;
    let status = response.status();
    let body = response.into_body().collect().await;
    let body = body.map_err(|err| err.to_string())?.to_bytes();
    Ok((status, String::from_utf8_lossy(&body).into_owned()))
}

/// Calls the Cloudflare API and returns the `result` of a successful answer.
async fn cloudflare(
    dns: &AcmeDns,
    method: Method,
    path: &str,
    body: Option<serde_json::Value>,
) -> Result<serde_json::Value, String> {
    let base = dns.api_url.as_deref().unwrap_or(CLOUDFLARE_API);
    let url = format!("{}{path}", base.trim_end_matches('/'));
    let token = dns.api_token.as_deref().unwrap_or_default();
    let headers = [
        (header::AUTHORIZATION, format!("Bearer {token}")),
        (header::CONTENT_TYPE, String::from("application/json")),
    ];
    let body = body.map(|body| body.to_string()).unwrap_or_default();
    let (status, text) = http_request(method, &url, &headers, body).await?;
    let mut answer: serde_json::Value =
        serde_json::from_str(&text).map_err(|_| format!("HTTP {status} from {url}"))?;
    if !status.is_success() || answer["success"] != true {
        return Err(format!("HTTP {status}: {}", answer["errors"]));
    }
    Ok(answer["result"].take())
}

async fn cloudflare_publish(dns: &AcmeDns, name: &str, value: &str) -> Result<Record, String> {
    let mut zone = dns.zone_id.clone();
    for candidate in zone_candidates(name) {
        if zone.is_some() {
            break;
        }
        let path = format!("/zones?name={candidate}");
        let zones = cloudflare(dns, Method::GET, &path, None).await?;
        zone = zones[0]["id"].as_str().map(str::to_owned);
    }
    let zone = zone.ok_or_else(|| no_zone(name))?;
    let body = serde_json::json!({
        "type": "TXT",
        "name": name,
        "content": format!("\"{value}\""),
        "ttl": TTL,
    });
    let path = format!("/zones/{zone}/dns_records");
    let created = cloudflare(dns, Method::POST, &path, Some(body)).await?;
    let id = created["id"]
        .as_str()
        .ok_or("the answer has no record id")?;
    Ok(Record {
        zone,
        id: id.to_owned(),
        name: name.to_owned(),
        value: value.to_owned(),
    })
}

/// `bytes` in lowercase hex.
pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn hmac_sha256(key: &[u8], data: &[u8]) -> Vec<u8> {
    use hmac::KeyInit;

    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC accepts keys of any size");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

/// `now` as `YYYYMMDDTHHMMSSZ`.
fn amz_date(now: SystemTime) -> String {
    let seconds = now
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs());
    let (days, rest) = (seconds / 86_400, seconds % 86_400);
    // Days to a civil date, after Howard Hinnant's `civil_from_days`.
    let shifted = days + 719_468;
    let (era, day_of_era) = (shifted / 146_097, shifted % 146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    };
    let year = year_of_era + era * 400 + u64::from(month <= 2);
    format!(
        "{year:04}{month:02}{day:02}T{:02}{:02}{:02}Z",
        rest / 3600,
        rest % 3600 / 60,
        rest % 60
    )
}

/// What an AWS Signature Version 4 covers besides the request.
struct SigV4<'a> {
    access_key_id: &'a str,
    secret_access_key: &'a str,
    region: &'a str,
    service: &'a str,
}

impl SigV4<'_> {
    /// The `Authorization` header of a request with the headers `host` and
    /// `x-amz-date` (`date`, from [`amz_date`]). `query` must already be
    /// sorted and percent-encoded.
    fn authorization(
        &self,
        method: &str,
        host: &str,
        path: &str,
        query: &str,
        body: &str,
        date: &str,
    ) -> String {
        let canonical = format!(
            "{method}\n{path}\n{query}\nhost:{host}\nx-amz-date:{date}\n\nhost;x-amz-date\n{}",
            hex(&Sha256::digest(body))
        );
        let day = &date[..8];
        let scope = format!("{day}/{}/{}/aws4_request", self.region, self.service);
        let to_sign = format!(
            "AWS4-HMAC-SHA256\n{date}\n{scope}\n{}",
            hex(&Sha256::digest(canonical))
        );
        let mut key = format!("AWS4{}", self.secret_access_key).into_bytes();
        for part in [day, self.region, self.service, "aws4_request", &to_sign] {
            key = hmac_sha256(&key, part.as_bytes());
        }
        format!(
            "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders=host;x-amz-date, \
             Signature={}",
            self.access_key_id,
            hex(&key)
        )
    }
}

/// Text of the first `<tag>` element of `xml`.
fn xml_text<'a>(xml: &'a str, tag: &str) -> Option<&'a str> {
    let start = xml.find(&format!("<{tag}>"))? + tag.len() + 2;
    let len = xml[start..].find(&format!("</{tag}>"))?;
    Some(&xml[start..start + len])
}

/// Calls the Route 53 API and returns the body of a successful answer.
async fn route53(
    dns: &AcmeDns,
    method: Method,
    path: &str,
    query: &str,
    body: String,
) -> Result<String, String> {
    let base = dns.api_url.as_deref().unwrap_or(ROUTE53_API);
    let base = base.trim_end_matches('/');
    let host = base.split_once("://").map_or(base, |(_, host)| host);
    let date = amz_date(SystemTime::now());
    let signature = SigV4 {
        access_key_id: dns.access_key_id.as_deref().unwrap_or_default(),
        secret_access_key: dns.secret_access_key.as_deref().unwrap_or_default(),
        // Route 53 is a global service signed for this region.
        region: "us-east-1",
        service: "route53",
    };
    let authorization = signature.authorization(method.as_str(), host, path, query, &body, &date);
    let headers = [
        (HeaderName::from_static("x-amz-date"), date),
        (header::AUTHORIZATION, authorization),
    ];
    let separator = if query.is_empty() { "" } else { "?" };
    let url = format!("{base}{path}{separator}{query}");
    let (status, text) = http_request(method, &url, &headers, body).await?;
    if !status.is_success() {
        let message = xml_text(&text, "Message").unwrap_or(&text);
        return Err(format!("HTTP {status}: {message}"));
    }
    Ok(text)
}

async fn route53_publish(dns: &AcmeDns, name: &str, value: &str) -> Result<Record, String> {
    let mut zone = dns.zone_id.clone();
    for candidate in zone_candidates(name) {
        if zone.is_some() {
            break;
        }
        let query = format!("dnsname={candidate}&maxitems=1");
        let path = "/2013-04-01/hostedzonesbyname";
        let zones = route53(dns, Method::GET, path, &query, String::new()).await?;
        // The listing starts at the name, it does not filter by it.
        if xml_text(&zones, "Name").map(|zone| zone.trim_end_matches('.')) == Some(candidate) {
            zone =
                xml_text(&zones, "Id").map(|id| id.trim_start_matches("/hostedzone/").to_owned());
        }
    }
    let record = Record {
        zone: zone.ok_or_else(|| no_zone(name))?,
        id: String::new(),
        name: name.to_owned(),
        value: value.to_owned(),
    };
    route53_change(dns, "UPSERT", &record).await?;
    Ok(record)
}

/// `UPSERT` or `DELETE` of the record set of `record`.
// shortcut: UPSERT replaces other TXT values at the name (another ACME
// client's pending challenge); merge with the existing set if that matters.
async fn route53_change(dns: &AcmeDns, action: &str, record: &Record) -> Result<(), String> {
    let body = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
         <ChangeResourceRecordSetsRequest xmlns=\"https://route53.amazonaws.com/doc/2013-04-01/\">\
         <ChangeBatch><Changes><Change><Action>{action}</Action><ResourceRecordSet>\
         <Name>{}.</Name><Type>TXT</Type><TTL>{TTL}</TTL><ResourceRecords><ResourceRecord>\
         <Value>\"{}\"</Value></ResourceRecord></ResourceRecords></ResourceRecordSet></Change>\
         </Changes></ChangeBatch></ChangeResourceRecordSetsRequest>",
        record.name, record.value
    );
    let path = format!("/2013-04-01/hostedzone/{}/rrset", record.zone);
    route53(dns, Method::POST, &path, "", body).await?;
    Ok(())
}

/// Appends `name` in wire format, lowercase (the canonical form TSIG signs).
fn write_name(out: &mut Vec<u8>, name: &str) -> Result<(), String> {
    for label in name.split('.').filter(|label| !label.is_empty()) {
        let len = u8::try_from(label.len()).ok().filter(|len| *len < 64);
        out.push(len.ok_or_else(|| format!("label too long in {name}"))?);
        out.extend_from_slice(label.to_ascii_lowercase().as_bytes());
    }
    out.push(0);
    Ok(())
}

/// A DNS message with the header and one entry in its first section: the
/// question of a query, the zone of an update.
fn dns_message(id: u16, opcode: u8, name: &str, record_type: u16) -> Result<Vec<u8>, String> {
    let mut message = Vec::with_capacity(512);
    message.extend_from_slice(&id.to_be_bytes());
    message.extend_from_slice(&[opcode << 3, 0, 0, 1, 0, 0, 0, 0, 0, 0]);
    write_name(&mut message, name)?;
    message.extend_from_slice(&record_type.to_be_bytes());
    message.extend_from_slice(&CLASS_IN.to_be_bytes());
    Ok(message)
}

/// The UPDATE message that adds (`class` IN) or deletes (`class` NONE) the
/// TXT record of `record` (RFC 2136, section 2.5).
fn update_message(id: u16, record: &Record, class: u16) -> Result<Vec<u8>, String> {
    let mut message = dns_message(id, OPCODE_UPDATE, &record.zone, TYPE_SOA)?;
    // One record in the update section.
    message[9] = 1;
    write_name(&mut message, &record.name)?;
    message.extend_from_slice(&TYPE_TXT.to_be_bytes());
    message.extend_from_slice(&class.to_be_bytes());
    let ttl = if class == CLASS_IN { TTL } else { 0 };
    message.extend_from_slice(&ttl.to_be_bytes());
    let len = u8::try_from(record.value.len()).map_err(|_| "TXT value over 255 bytes")?;
    message.extend_from_slice(&(u16::from(len) + 1).to_be_bytes());
    message.push(len);
    message.extend_from_slice(record.value.as_bytes());
    Ok(message)
}

/// Appends the TSIG record that signs `message` with HMAC-SHA256 at `time`
/// (seconds since the epoch), RFC 8945 section 4.3.3.
fn tsig_sign(message: &mut Vec<u8>, key: &str, secret: &[u8], time: u64) -> Result<(), String> {
    let mut key_name = Vec::new();
    write_name(&mut key_name, key)?;
    let mut algorithm = Vec::new();
    write_name(&mut algorithm, "hmac-sha256")?;
    // 48 bits of time, then the 300 seconds of allowed clock difference.
    let mut timers = time.to_be_bytes()[2..].to_vec();
    timers.extend_from_slice(&300_u16.to_be_bytes());
    // Error and other data: none.
    let tail = [0; 4];

    let mut signed = message.clone();
    signed.extend_from_slice(&key_name);
    signed.extend_from_slice(&CLASS_ANY.to_be_bytes());
    signed.extend_from_slice(&0_u32.to_be_bytes());
    signed.extend_from_slice(&algorithm);
    signed.extend_from_slice(&timers);
    signed.extend_from_slice(&tail);
    let mac = hmac_sha256(secret, &signed);

    let mut rdata = algorithm;
    rdata.extend_from_slice(&timers);
    rdata.extend_from_slice(&u16::try_from(mac.len()).expect("32 bytes").to_be_bytes());
    rdata.extend_from_slice(&mac);
    // Original message identifier.
    rdata.extend_from_slice(&message[..2]);
    rdata.extend_from_slice(&tail);

    message.extend_from_slice(&key_name);
    message.extend_from_slice(&TYPE_TSIG.to_be_bytes());
    message.extend_from_slice(&CLASS_ANY.to_be_bytes());
    message.extend_from_slice(&0_u32.to_be_bytes());
    message.extend_from_slice(&u16::try_from(rdata.len()).expect("short").to_be_bytes());
    message.extend_from_slice(&rdata);
    // One record in the additional section.
    message[11] += 1;
    Ok(())
}

/// Sends `message` to the name server of `dns` over TCP and returns the
/// answer, checked to belong to it.
async fn dns_exchange(dns: &AcmeDns, message: &[u8]) -> Result<Vec<u8>, String> {
    let server = dns.server.as_deref().unwrap_or_default();
    let has_port = server.parse::<std::net::SocketAddr>().is_ok()
        || server
            .rsplit_once(':')
            .is_some_and(|(host, port)| !host.contains(':') && port.parse::<u16>().is_ok());
    let address = if has_port {
        server.to_owned()
    } else {
        format!("{server}:53")
    };
    let io = |err: std::io::Error| format!("name server {address}: {err}");
    let mut stream = TcpStream::connect(&address).await.map_err(io)?;
    let len = u16::try_from(message.len()).map_err(|_| "DNS message too long")?;
    stream.write_all(&len.to_be_bytes()).await.map_err(io)?;
    stream.write_all(message).await.map_err(io)?;
    let len = stream.read_u16().await.map_err(io)?;
    let mut answer = vec![0; usize::from(len)];
    stream.read_exact(&mut answer).await.map_err(io)?;
    if answer.len() < 12 || answer[..2] != message[..2] {
        return Err(format!("name server {address}: unexpected answer"));
    }
    Ok(answer)
}

/// True if `answer` holds a SOA record as its first answer, which is how an
/// authoritative server answers a SOA query for the name of a zone.
fn answers_soa(answer: &[u8]) -> bool {
    let rcode = answer[3] & 0x0f;
    let answers = u16::from_be_bytes([answer[6], answer[7]]);
    if rcode != 0 || answers == 0 {
        return false;
    }
    // The question name, its type and class, then the owner of the answer.
    let mut position = 12;
    for after in [4, 0] {
        loop {
            let Some(&len) = answer.get(position) else {
                return false;
            };
            if len >= 0xc0 {
                position += 2;
                break;
            }
            position += 1 + usize::from(len);
            if len == 0 {
                break;
            }
        }
        position += after;
    }
    answer.get(position..position + 2) == Some(&TYPE_SOA.to_be_bytes())
}

fn message_id() -> u16 {
    RandomState::new().hash_one(0_u64) as u16
}

async fn rfc2136_publish(dns: &AcmeDns, name: &str, value: &str) -> Result<Record, String> {
    let mut zone = None;
    for candidate in zone_candidates(name) {
        let query = dns_message(message_id(), 0, candidate, TYPE_SOA)?;
        if answers_soa(&dns_exchange(dns, &query).await?) {
            zone = Some(candidate.to_owned());
            break;
        }
    }
    let record = Record {
        zone: zone.ok_or_else(|| format!("the name server has no zone for {name}"))?,
        id: String::new(),
        name: name.to_owned(),
        value: value.to_owned(),
    };
    rfc2136_update(dns, &record, CLASS_IN).await?;
    Ok(record)
}

// shortcut: the TSIG of the answer is not verified; a forged answer can only
// make a refused update look accepted, and the CA validation then fails.
async fn rfc2136_update(dns: &AcmeDns, record: &Record, class: u16) -> Result<(), String> {
    let mut message = update_message(message_id(), record, class)?;
    if let (Some(key), Some(secret)) = (&dns.tsig_key, dns.tsig_secret_bytes()?) {
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |since| since.as_secs());
        tsig_sign(&mut message, key, &secret, now)?;
    }
    let answer = dns_exchange(dns, &message).await?;
    match answer[3] & 0x0f {
        0 => Ok(()),
        5 => Err(String::from("update refused (REFUSED)")),
        9 => Err(String::from(
            "update refused (NOTAUTH): wrong TSIG key or clock, or not the primary server",
        )),
        rcode => Err(format!("update refused (rcode {rcode})")),
    }
}

#[cfg(test)]
mod tests {
    use std::convert::Infallible;
    use std::net::SocketAddr;
    use std::sync::Mutex;

    use hyper::body::Incoming;
    use hyper::service::service_fn;
    use rxh_core::config::Config;
    use rxh_test_util::DnsServer;
    use tokio::net::TcpListener;

    use super::*;

    fn settings(table: &str) -> AcmeDns {
        let text = format!(
            "[[server]]\nlisten = \"127.0.0.1:443\"\nforward = \"a:1\"\n[acme.dns]\n{table}\n"
        );
        Config::from_toml(&text).unwrap().acme.dns.unwrap()
    }

    /// One request seen by [`fake_api`]: method, path with query, the
    /// `Authorization` header and the body.
    type Seen = (String, String, String, String);

    /// HTTP server answering every request with what `respond` returns for
    /// its method and path.
    async fn fake_api(
        respond: fn(&str, &str) -> (u16, &'static str),
    ) -> (SocketAddr, Arc<Mutex<Vec<Seen>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let seen: Arc<Mutex<Vec<Seen>>> = Arc::default();
        let log = Arc::clone(&seen);
        tokio::task::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let log = Arc::clone(&log);
                let service = service_fn(move |request: Request<Incoming>| {
                    let log = Arc::clone(&log);
                    async move {
                        let method = request.method().to_string();
                        let path = request.uri().to_string();
                        let header = |name: HeaderName| {
                            let value = request.headers().get(name);
                            value.map(|value| value.to_str().unwrap().to_owned())
                        };
                        let authorization = header(header::AUTHORIZATION).unwrap_or_default();
                        assert!(header(header::HOST).is_some());
                        let (status, answer) = respond(&method, &path);
                        let body = request.into_body().collect().await.unwrap().to_bytes();
                        let body = String::from_utf8(body.to_vec()).unwrap();
                        log.lock()
                            .unwrap()
                            .push((method, path, authorization, body));
                        let mut response = hyper::Response::new(Full::new(Bytes::from(answer)));
                        *response.status_mut() = StatusCode::from_u16(status).unwrap();
                        Ok::<_, Infallible>(response)
                    }
                });
                tokio::task::spawn(
                    hyper::server::conn::http1::Builder::new()
                        .serve_connection(TokioIo::new(stream), service),
                );
            }
        });
        (address, seen)
    }

    #[test]
    fn zone_candidates_go_from_the_name_to_the_top() {
        let candidates: Vec<&str> = zone_candidates("_acme-challenge.www.a.test").collect();
        assert_eq!(
            candidates,
            ["_acme-challenge.www.a.test", "www.a.test", "a.test", "test"]
        );
    }

    #[tokio::test]
    async fn cloudflare_creates_and_deletes_the_record() {
        let (address, seen) = fake_api(|method, path| match (method, path) {
            ("GET", "/client/v4/zones?name=a.test") => (
                200,
                r#"{"success":true,"errors":[],"result":[{"id":"zone1"}]}"#,
            ),
            ("GET", _) => (200, r#"{"success":true,"errors":[],"result":[]}"#),
            ("POST", "/client/v4/zones/zone1/dns_records") => (
                200,
                r#"{"success":true,"errors":[],"result":{"id":"rec1"}}"#,
            ),
            ("DELETE", "/client/v4/zones/zone1/dns_records/rec1") => (
                200,
                r#"{"success":true,"errors":[],"result":{"id":"rec1"}}"#,
            ),
            _ => (
                403,
                r#"{"success":false,"errors":[{"code":10000,"message":"Authentication error"}]}"#,
            ),
        })
        .await;
        let dns = settings(&format!(
            "provider = \"cloudflare\"\napi_token = \"t0k3n\"\n\
             api_url = \"http://{address}/client/v4/\""
        ));

        let record = publish(&dns, "_acme-challenge.www.a.test", "value-1")
            .await
            .unwrap();
        withdraw(&dns, &record).await.unwrap();

        let calls_seen = seen.lock().unwrap().clone();
        let calls: Vec<String> = calls_seen
            .iter()
            .map(|(method, path, ..)| format!("{method} {path}"))
            .collect();
        assert_eq!(
            calls,
            [
                "GET /client/v4/zones?name=_acme-challenge.www.a.test",
                "GET /client/v4/zones?name=www.a.test",
                "GET /client/v4/zones?name=a.test",
                "POST /client/v4/zones/zone1/dns_records",
                "DELETE /client/v4/zones/zone1/dns_records/rec1",
            ]
        );
        assert!(calls_seen.iter().all(|call| call.2 == "Bearer t0k3n"));
        let created: serde_json::Value = serde_json::from_str(&calls_seen[3].3).unwrap();
        assert_eq!(
            created,
            serde_json::json!({
                "type": "TXT",
                "name": "_acme-challenge.www.a.test",
                "content": "\"value-1\"",
                "ttl": 60,
            })
        );

        // A configured zone is not looked up; API errors are reported.
        let dns = AcmeDns {
            zone_id: Some(String::from("other")),
            ..dns
        };
        let err = publish(&dns, "_acme-challenge.a.test", "v")
            .await
            .unwrap_err();
        assert!(
            err.contains("403") && err.contains("Authentication error"),
            "{err}"
        );
        let calls_seen = seen.lock().unwrap().clone();
        assert_eq!(calls_seen.len(), 6);
        assert_eq!(calls_seen[5].1, "/client/v4/zones/other/dns_records");
    }

    #[tokio::test]
    async fn cloudflare_reports_a_missing_zone_and_an_unreachable_api() {
        let (address, _) =
            fake_api(|_, _| (200, r#"{"success":true,"errors":[],"result":[]}"#)).await;
        let dns = settings(&format!(
            "provider = \"cloudflare\"\napi_token = \"t\"\napi_url = \"http://{address}\""
        ));
        let err = publish(&dns, "_acme-challenge.a.test", "v")
            .await
            .unwrap_err();
        assert!(err.contains("no zone for _acme-challenge.a.test"), "{err}");

        let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = closed.local_addr().unwrap();
        drop(closed);
        let dns = settings(&format!(
            "provider = \"cloudflare\"\napi_token = \"t\"\napi_url = \"http://{address}\""
        ));
        let err = publish(&dns, "_acme-challenge.a.test", "v")
            .await
            .unwrap_err();
        assert!(err.contains("connecting to"), "{err}");
    }

    /// The `get-vanilla` case of the AWS Signature Version 4 test suite.
    #[test]
    fn sigv4_matches_the_aws_test_suite() {
        let date = amz_date(SystemTime::UNIX_EPOCH + Duration::from_secs(1_440_938_160));
        assert_eq!(date, "20150830T123600Z");
        let signature = SigV4 {
            access_key_id: "AKIDEXAMPLE",
            secret_access_key: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
            region: "us-east-1",
            service: "service",
        };
        assert_eq!(
            signature.authorization("GET", "example.amazonaws.com", "/", "", "", &date),
            "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/service/aws4_request, \
             SignedHeaders=host;x-amz-date, \
             Signature=5fa00fa31553b73ebf1942676e86291e8372ff2a2260956d9b8aae1d763fbf31"
        );
        // Leap day and the months before March.
        let leap = SystemTime::UNIX_EPOCH + Duration::from_secs(1_709_251_199);
        assert_eq!(amz_date(leap), "20240229T235959Z");
        let january = SystemTime::UNIX_EPOCH + Duration::from_secs(1_767_225_600);
        assert_eq!(amz_date(january), "20260101T000000Z");
    }

    #[tokio::test]
    async fn route53_upserts_and_deletes_the_record() {
        let (address, seen) = fake_api(|method, path| match (method, path) {
            ("GET", "/2013-04-01/hostedzonesbyname?dnsname=a.test&maxitems=1") => (
                200,
                "<ListHostedZonesByNameResponse><HostedZones><HostedZone>\
                 <Id>/hostedzone/Z1</Id><Name>a.test.</Name></HostedZone></HostedZones>\
                 </ListHostedZonesByNameResponse>",
            ),
            // Listings start at the name: another zone is not a match.
            ("GET", _) => (
                200,
                "<ListHostedZonesByNameResponse><HostedZones><HostedZone>\
                 <Id>/hostedzone/Z9</Id><Name>b.test.</Name></HostedZone></HostedZones>\
                 </ListHostedZonesByNameResponse>",
            ),
            ("POST", "/2013-04-01/hostedzone/Z1/rrset") => (200, "<ChangeInfo/>"),
            _ => (
                403,
                "<ErrorResponse><Error><Code>AccessDenied</Code>\
                 <Message>not authorized</Message></Error></ErrorResponse>",
            ),
        })
        .await;
        let dns = settings(&format!(
            "provider = \"route53\"\naccess_key_id = \"AKIDEXAMPLE\"\n\
             secret_access_key = \"secret\"\napi_url = \"http://{address}\""
        ));

        let record = publish(&dns, "_acme-challenge.a.test", "value-1")
            .await
            .unwrap();
        withdraw(&dns, &record).await.unwrap();

        let seen = seen.lock().unwrap().clone();
        let calls: Vec<String> = seen
            .iter()
            .map(|(method, path, ..)| format!("{method} {path}"))
            .collect();
        assert_eq!(
            calls,
            [
                "GET /2013-04-01/hostedzonesbyname?dnsname=_acme-challenge.a.test&maxitems=1",
                "GET /2013-04-01/hostedzonesbyname?dnsname=a.test&maxitems=1",
                "POST /2013-04-01/hostedzone/Z1/rrset",
                "POST /2013-04-01/hostedzone/Z1/rrset",
            ]
        );
        for (.., authorization, _) in &seen {
            assert!(
                authorization.starts_with("AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/")
                    && authorization.contains("/us-east-1/route53/aws4_request, "),
                "{authorization}"
            );
        }
        let record_set = "<Name>_acme-challenge.a.test.</Name><Type>TXT</Type><TTL>60</TTL>\
                          <ResourceRecords><ResourceRecord><Value>\"value-1\"</Value>";
        assert!(seen[2].3.contains("<Action>UPSERT</Action>") && seen[2].3.contains(record_set));
        assert!(seen[3].3.contains("<Action>DELETE</Action>") && seen[3].3.contains(record_set));

        let dns = AcmeDns {
            zone_id: Some(String::from("Z2")),
            ..dns
        };
        let err = publish(&dns, "_acme-challenge.a.test", "v")
            .await
            .unwrap_err();
        assert!(
            err.contains("403") && err.contains("not authorized"),
            "{err}"
        );
        let err = publish(
            &AcmeDns {
                zone_id: None,
                ..dns
            },
            "_acme-challenge.c.test",
            "v",
        )
        .await
        .unwrap_err();
        assert!(err.contains("no zone"), "{err}");
    }

    #[tokio::test]
    async fn rfc2136_adds_and_deletes_the_record_in_the_zone_of_the_name() {
        let server = DnsServer::spawn("127.0.0.1:0", "a.test").await;
        let dns = settings(&format!(
            "provider = \"rfc2136\"\nserver = \"{}\"\ntsig_key = \"Rxh-Key.\"\n\
             tsig_secret = \"c2VjcmV0\"",
            server.address()
        ));
        let name = "_acme-challenge.www.a.test";

        let first = publish(&dns, name, "value-1").await.unwrap();
        // A wildcard and its base name are validated at the same record name.
        let second = publish(&dns, name, "value-2").await.unwrap();
        assert_eq!(first.zone, "a.test");
        assert_eq!(server.txt(name), ["value-1", "value-2"]);
        withdraw(&dns, &first).await.unwrap();
        assert_eq!(server.txt(name), ["value-2"]);
        withdraw(&dns, &second).await.unwrap();
        assert!(server.txt(name).is_empty());

        // Every update is signed: one more record, the TSIG of the key.
        let updates = server.updates();
        assert_eq!(updates.len(), 4);
        let mut key_name = Vec::new();
        write_name(&mut key_name, "rxh-key").unwrap();
        key_name.extend_from_slice(&TYPE_TSIG.to_be_bytes());
        for update in &updates {
            assert_eq!(update[10..12], [0, 1], "additional records");
            assert!(update.windows(key_name.len()).any(|part| part == key_name));
        }

        // Not this server's zone, a server that refuses, and no server.
        let err = publish(&dns, "_acme-challenge.b.test", "v")
            .await
            .unwrap_err();
        assert!(err.contains("no zone for _acme-challenge.b.test"), "{err}");
        let stray = Record {
            zone: String::from("b.test"),
            ..first
        };
        let err = withdraw(&dns, &stray).await.unwrap_err();
        assert!(err.contains("NOTAUTH"), "{err}");
        let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let dns = settings(&format!(
            "provider = \"rfc2136\"\nserver = \"{}\"",
            closed.local_addr().unwrap()
        ));
        drop(closed);
        let err = publish(&dns, name, "v").await.unwrap_err();
        assert!(err.contains("name server 127.0.0.1:"), "{err}");
    }

    /// Fixed input. BIND 9 accepts the updates this code signs with the
    /// current time (checked by hand with `named` and `tsig-keygen`).
    #[test]
    fn update_message_and_tsig_have_the_wire_format() {
        let record = Record {
            zone: String::from("a.test"),
            id: String::new(),
            name: String::from("_acme-challenge.A.test"),
            value: String::from("abc"),
        };
        let mut message = update_message(0x1234, &record, CLASS_IN).unwrap();
        let unsigned = "1234280000010000000100000161047465737400000600010f\
                        5f61636d652d6368616c6c656e67650161047465737400\
                        001000010000003c000403616263";
        assert_eq!(hex(&message), unsigned);
        let delete = update_message(0x1234, &record, CLASS_NONE).unwrap();
        assert_eq!(
            hex(&delete[delete.len() - 14..]),
            "001000fe00000000000403616263",
            "class NONE and no TTL delete exactly this record"
        );

        tsig_sign(&mut message, "Key.", b"secret", 0x0001_0203_0405).unwrap();
        let mut signed = hex(&message);
        assert!(signed.starts_with(&unsigned[..20]));
        assert_eq!(&signed[20..24], "0001", "the TSIG is an additional record");
        signed.replace_range(..unsigned.len(), "");
        let mac = hex(&hmac_sha256(
            b"secret",
            &[
                &message[..10],
                &[0, 0][..],
                &message[12..unsigned.len() / 2],
                b"\x03key\x00\x00\xff\x00\x00\x00\x00\x0bhmac-sha256\x00",
                b"\x00\x01\x02\x03\x04\x05\x01\x2c\x00\x00\x00\x00",
            ]
            .concat(),
        ));
        assert_eq!(
            signed,
            format!(
                "036b65790000fa00ff00000000003d0b686d61632d736861323536000001020304\
                 05012c0020{mac}123400000000"
            )
        );
        assert!(write_name(&mut Vec::new(), &"a".repeat(64)).is_err());
    }
}
