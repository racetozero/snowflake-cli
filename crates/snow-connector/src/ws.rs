//! A minimal RFC 6455 WebSocket client (`ws://` and `wss://`) for streaming
//! endpoints: handshake, masked client frames, ping/pong and close.

use base64::Engine;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::time::Duration;

trait Stream: Read + Write {}
impl<T: Read + Write> Stream for T {}

pub struct WebSocket {
    io: BufReader<Box<dyn Stream>>,
}

pub enum Frame {
    Binary(Vec<u8>),
    Text(String),
    Close,
}

fn err(m: impl Into<String>) -> std::io::Error {
    std::io::Error::other(m.into())
}

/// A rustls client trusting the webpki roots, with the `ring` provider.
fn tls_config(
    versions: &[&'static rustls::SupportedProtocolVersion],
) -> Result<rustls::ClientConfig, rustls::Error> {
    let roots = rustls::RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    };
    Ok(rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_protocol_versions(versions)?
    .with_root_certificates(roots)
    .with_no_client_auth())
}

impl WebSocket {
    /// Connect and complete the opening handshake, sending `headers` (`Name: value`).
    pub fn connect(url: &str, headers: &[String], timeout: Duration) -> std::io::Result<WebSocket> {
        let u = url::Url::parse(url).map_err(|e| err(e.to_string()))?;
        let tls = match u.scheme() {
            "ws" => false,
            "wss" => true,
            s => return Err(err(format!("Unsupported scheme: {s}"))),
        };
        let host = u.host_str().ok_or_else(|| err("missing host"))?.to_string();
        let port = u.port().unwrap_or(if tls { 443 } else { 80 });
        let addr = std::net::ToSocketAddrs::to_socket_addrs(&(host.as_str(), port))?
            .next()
            .ok_or_else(|| err("could not resolve host"))?;
        let tcp = TcpStream::connect_timeout(&addr, timeout)?;
        tcp.set_read_timeout(Some(Duration::from_secs(90)))?;
        let stream: Box<dyn Stream> = if tls {
            let config = tls_config(rustls::DEFAULT_VERSIONS).map_err(|e| err(e.to_string()))?;
            let name = rustls::pki_types::ServerName::try_from(host.clone())
                .map_err(|e| err(e.to_string()))?;
            let conn = rustls::ClientConnection::new(Arc::new(config), name)
                .map_err(|e| err(e.to_string()))?;
            Box::new(rustls::StreamOwned::new(conn, tcp))
        } else {
            Box::new(tcp)
        };
        let mut io = BufReader::new(stream);
        let key = base64::engine::general_purpose::STANDARD.encode(rand::random::<[u8; 16]>());
        let path = match u.query() {
            Some(q) => format!("{}?{q}", u.path()),
            None => u.path().to_string(),
        };
        let host_header = if u.port().is_some() {
            format!("{host}:{port}")
        } else {
            host.clone()
        };
        let mut req = format!(
            "GET {path} HTTP/1.1\r\nUpgrade: websocket\r\nHost: {host_header}\r\nOrigin: http://{host_header}\r\nSec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\nConnection: Upgrade\r\n"
        );
        for h in headers {
            req.push_str(h);
            req.push_str("\r\n");
        }
        req.push_str("\r\n");
        io.get_mut().write_all(req.as_bytes())?;
        io.get_mut().flush()?;
        let mut status = String::new();
        io.read_line(&mut status)?;
        if !status.split_whitespace().nth(1).is_some_and(|c| c == "101") {
            return Err(err(format!(
                "Handshake status {}",
                status
                    .split_whitespace()
                    .skip(1)
                    .collect::<Vec<_>>()
                    .join(" ")
            )));
        }
        loop {
            let mut line = String::new();
            if io.read_line(&mut line)? == 0 || line == "\r\n" || line == "\n" {
                break;
            }
        }
        Ok(WebSocket { io })
    }

    fn send(&mut self, opcode: u8, payload: &[u8]) -> std::io::Result<()> {
        let mut f = vec![0x80 | opcode];
        let n = payload.len();
        if n < 126 {
            f.push(0x80 | n as u8);
        } else if n <= 0xFFFF {
            f.push(0x80 | 126);
            f.extend_from_slice(&(n as u16).to_be_bytes());
        } else {
            f.push(0x80 | 127);
            f.extend_from_slice(&(n as u64).to_be_bytes());
        }
        let mask: [u8; 4] = rand::random();
        f.extend_from_slice(&mask);
        f.extend(payload.iter().enumerate().map(|(i, b)| b ^ mask[i % 4]));
        self.io.get_mut().write_all(&f)?;
        self.io.get_mut().flush()
    }

    pub fn send_binary(&mut self, payload: &[u8]) -> std::io::Result<()> {
        self.send(0x2, payload)
    }

    /// The next data or close frame (pings are answered, other frames skipped).
    pub fn recv(&mut self) -> std::io::Result<Frame> {
        let mut message: Vec<u8> = Vec::new();
        let mut kind = 0u8;
        loop {
            let mut h = [0u8; 2];
            self.io.read_exact(&mut h)?;
            let fin = h[0] & 0x80 != 0;
            let opcode = h[0] & 0x0F;
            let masked = h[1] & 0x80 != 0;
            let mut len = (h[1] & 0x7F) as u64;
            if len == 126 {
                let mut b = [0u8; 2];
                self.io.read_exact(&mut b)?;
                len = u16::from_be_bytes(b) as u64;
            } else if len == 127 {
                let mut b = [0u8; 8];
                self.io.read_exact(&mut b)?;
                len = u64::from_be_bytes(b);
            }
            let mut mask = [0u8; 4];
            if masked {
                self.io.read_exact(&mut mask)?;
            }
            let mut payload = vec![0u8; len as usize];
            self.io.read_exact(&mut payload)?;
            if masked {
                for (i, b) in payload.iter_mut().enumerate() {
                    *b ^= mask[i % 4];
                }
            }
            match opcode {
                0x8 => return Ok(Frame::Close),
                0x9 => {
                    self.send(0xA, &payload)?;
                    continue;
                }
                0xA => continue,
                0x0 => message.extend(payload),
                op => {
                    kind = op;
                    message = payload;
                }
            }
            if fin {
                return Ok(match kind {
                    0x1 => Frame::Text(String::from_utf8_lossy(&message).into_owned()),
                    _ => Frame::Binary(message),
                });
            }
        }
    }

    pub fn close(&mut self) -> std::io::Result<()> {
        self.send(0x8, &1000u16.to_be_bytes())
    }
}

/// `_probe_cert_for_host`: a TLS 1.3 handshake against the webpki roots.
/// `Some(true)` provisioned, `Some(false)` when the served certificate does not cover
/// `host` or has expired, `None` when inconclusive (network or trust failures).
pub fn tls_probe(host: &str, port: u16, timeout: Duration) -> Option<bool> {
    use std::io::Write as _;
    let Ok(Some(addr)) =
        std::net::ToSocketAddrs::to_socket_addrs(&(host, port)).map(|mut a| a.next())
    else {
        return None;
    };
    let Ok(mut tcp) = TcpStream::connect_timeout(&addr, timeout) else {
        return None;
    };
    let _ = tcp.set_read_timeout(Some(timeout));
    let Ok(config) = tls_config(&[&rustls::version::TLS13]) else {
        return None;
    };
    let Ok(name) = rustls::pki_types::ServerName::try_from(host.to_string()) else {
        return None;
    };
    let Ok(mut conn) = rustls::ClientConnection::new(Arc::new(config), name) else {
        return None;
    };
    while conn.is_handshaking() {
        match conn.complete_io(&mut tcp) {
            Ok(_) => {}
            Err(e) => {
                let invalid = e
                    .get_ref()
                    .and_then(|i| i.downcast_ref::<rustls::Error>())
                    .is_some_and(|r| {
                        matches!(
                            r,
                            rustls::Error::InvalidCertificate(
                                rustls::CertificateError::NotValidForName
                                    | rustls::CertificateError::NotValidForNameContext { .. }
                                    | rustls::CertificateError::Expired
                                    | rustls::CertificateError::ExpiredContext { .. }
                            )
                        )
                    });
                return if invalid { Some(false) } else { None };
            }
        }
    }
    let _ = tcp.flush();
    Some(true)
}
