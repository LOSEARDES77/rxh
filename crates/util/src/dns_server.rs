//! Authoritative DNS server of one zone that accepts dynamic updates (RFC 2136), for tests of
//! the ACME DNS-01 challenge.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, UdpSocket};

const TYPE_SOA: u16 = 6;
const TYPE_TXT: u16 = 16;
const CLASS_IN: u16 = 1;
const CLASS_NONE: u16 = 254;
const OPCODE_UPDATE: u8 = 5;
const RCODE_FORMERR: u8 = 1;
const RCODE_NOTAUTH: u8 = 9;

#[derive(Debug, Default)]
struct Zone {
    /// Lowercase, without the trailing dot.
    name: String,
    /// TXT records: lowercase owner name and text.
    records: Mutex<Vec<(String, String)>>,
    /// Every UPDATE message received, as sent.
    updates: Mutex<Vec<Vec<u8>>>,
}

/// DNS server on a localhost port, over TCP and UDP. It answers SOA queries for its zone and
/// TXT queries from the records added by UPDATE messages. Updates are not authenticated: the
/// raw messages are kept for the test to inspect.
#[derive(Debug)]
pub struct DnsServer {
    address: SocketAddr,
    zone: Arc<Zone>,
}

impl DnsServer {
    /// Starts a server for `zone` on `address` (port 0 for a free one). Must run inside a
    /// Tokio runtime.
    pub async fn spawn(address: &str, zone: &str) -> Self {
        let tcp = TcpListener::bind(address).await.expect("bind TCP");
        let address = tcp.local_addr().expect("address");
        let udp = UdpSocket::bind(address).await.expect("bind UDP");
        let zone = Arc::new(Zone {
            name: zone.trim_end_matches('.').to_ascii_lowercase(),
            ..Zone::default()
        });
        tokio::task::spawn(serve_tcp(tcp, Arc::clone(&zone)));
        tokio::task::spawn(serve_udp(udp, Arc::clone(&zone)));
        Self { address, zone }
    }

    /// Address of the server, the same port for TCP and UDP.
    pub fn address(&self) -> SocketAddr {
        self.address
    }

    /// Texts of the TXT records of `name` right now.
    pub fn txt(&self, name: &str) -> Vec<String> {
        let name = name.to_ascii_lowercase();
        let records = self.zone.records.lock().expect("records lock");
        let matching = records.iter().filter(|(owner, _)| *owner == name);
        matching.map(|(_, text)| text.clone()).collect()
    }

    /// Every UPDATE message received so far, as sent.
    pub fn updates(&self) -> Vec<Vec<u8>> {
        self.zone.updates.lock().expect("updates lock").clone()
    }
}

async fn serve_tcp(listener: TcpListener, zone: Arc<Zone>) {
    while let Ok((mut stream, _)) = listener.accept().await {
        let zone = Arc::clone(&zone);
        tokio::task::spawn(async move {
            while let Ok(len) = stream.read_u16().await {
                let mut message = vec![0; usize::from(len)];
                if stream.read_exact(&mut message).await.is_err() {
                    return;
                }
                let response = answer(&zone, &message);
                let len = u16::try_from(response.len()).expect("short response");
                let _ = stream.write_all(&len.to_be_bytes()).await;
                let _ = stream.write_all(&response).await;
            }
        });
    }
}

async fn serve_udp(socket: UdpSocket, zone: Arc<Zone>) {
    let mut buffer = [0; 4096];
    while let Ok((len, peer)) = socket.recv_from(&mut buffer).await {
        let _ = socket.send_to(&answer(&zone, &buffer[..len]), peer).await;
    }
}

/// Reads an uncompressed name at `*position`, lowercase and without the trailing dot.
fn read_name(message: &[u8], position: &mut usize) -> Option<String> {
    let mut labels: Vec<String> = Vec::new();
    loop {
        let len = usize::from(*message.get(*position)?);
        *position += 1;
        if len == 0 {
            return Some(labels.join("."));
        }
        let label = message.get(*position..*position + len)?;
        labels.push(String::from_utf8_lossy(label).to_ascii_lowercase());
        *position += len;
    }
}

fn read_u16(message: &[u8], position: &mut usize) -> Option<u16> {
    let bytes = message.get(*position..*position + 2)?;
    *position += 2;
    Some(u16::from_be_bytes([bytes[0], bytes[1]]))
}

fn write_name(out: &mut Vec<u8>, name: &str) {
    for label in name.split('.').filter(|label| !label.is_empty()) {
        out.push(u8::try_from(label.len()).expect("short label"));
        out.extend_from_slice(label.as_bytes());
    }
    out.push(0);
}

/// The response to `message`; a format error if it cannot be read.
fn answer(zone: &Zone, message: &[u8]) -> Vec<u8> {
    let (rcode, question_end, answers) =
        handle(zone, message).unwrap_or((RCODE_FORMERR, 12.min(message.len()), Vec::new()));
    let mut response = message[..question_end].to_vec();
    response.resize(response.len().max(12), 0);
    // QR and AA, keeping the opcode.
    response[2] = 0x84 | (response[2] & 0x78);
    response[3] = rcode;
    if question_end <= 12 {
        response[4..6].fill(0);
    }
    let count = u16::try_from(answers.len()).expect("few answers");
    response[6..8].copy_from_slice(&count.to_be_bytes());
    response[8..12].fill(0);
    for rdata in answers {
        // Owner: a pointer to the question name.
        response.extend_from_slice(&[0xc0, 12]);
        response.extend_from_slice(&message[question_end - 4..question_end]);
        response.extend_from_slice(&0_u32.to_be_bytes());
        response.extend_from_slice(&u16::try_from(rdata.len()).expect("short").to_be_bytes());
        response.extend_from_slice(&rdata);
    }
    response
}

/// Applies `message`. Returns the response code, where the first section ends and the record
/// data of the answers.
fn handle(zone: &Zone, message: &[u8]) -> Option<(u8, usize, Vec<Vec<u8>>)> {
    let opcode = (*message.get(2)? >> 3) & 0x0f;
    let mut position = 8;
    let update_count = read_u16(message, &mut position)?;
    position = 12;
    let name = read_name(message, &mut position)?;
    let query_type = read_u16(message, &mut position)?;
    position += 2;
    let question_end = position;

    if opcode != OPCODE_UPDATE {
        let answers = match query_type {
            TYPE_SOA if name == zone.name => {
                let mut rdata = Vec::new();
                write_name(&mut rdata, "ns.invalid");
                write_name(&mut rdata, "admin.invalid");
                rdata.extend_from_slice(&[0; 20]);
                vec![rdata]
            }
            TYPE_TXT => {
                let records = zone.records.lock().expect("records lock");
                let matching = records.iter().filter(|(owner, _)| *owner == name);
                matching
                    .map(|(_, text)| {
                        let mut rdata = vec![u8::try_from(text.len()).expect("short text")];
                        rdata.extend_from_slice(text.as_bytes());
                        rdata
                    })
                    .collect()
            }
            _ => Vec::new(),
        };
        return Some((0, question_end, answers));
    }

    zone.updates
        .lock()
        .expect("updates lock")
        .push(message.to_vec());
    if name != zone.name {
        return Some((RCODE_NOTAUTH, question_end, Vec::new()));
    }
    for _ in 0..update_count {
        let owner = read_name(message, &mut position)?;
        let record_type = read_u16(message, &mut position)?;
        let class = read_u16(message, &mut position)?;
        position += 4;
        let len = usize::from(read_u16(message, &mut position)?);
        let rdata = message.get(position..position + len)?;
        position += len;
        if record_type != TYPE_TXT {
            continue;
        }
        let text = String::from_utf8_lossy(rdata.get(1..)?).into_owned();
        let mut records = zone.records.lock().expect("records lock");
        match class {
            CLASS_IN => records.push((owner, text)),
            CLASS_NONE => records.retain(|record| *record != (owner.clone(), text.clone())),
            _ => {}
        }
    }
    Some((0, question_end, Vec::new()))
}
