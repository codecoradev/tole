//! A one-connection TLS server on loopback for the HTTPS tests.
#![allow(dead_code)]

use crate::tls_pki::Identity;
use rustls::pki_types::{CertificateDer, PrivatePkcs8KeyDer};
use rustls::{ServerConfig, ServerConnection, StreamOwned};
use std::io::Read;
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::thread::JoinHandle;

pub type Stream = StreamOwned<ServerConnection, TcpStream>;

/// What the server saw: `None` when the handshake failed (the client refused
/// the certificate), else the bytes of the request head it received.
pub type Seen = Option<Vec<u8>>;

fn server_config(chain: &[&Identity]) -> Arc<ServerConfig> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let certs = chain
        .iter()
        .map(|i| CertificateDer::from(i.cert_der.clone()))
        .collect();
    let key = PrivatePkcs8KeyDer::from(chain[0].key_pkcs8.clone());
    Arc::new(
        ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(certs, key.into())
            .unwrap(),
    )
}

/// Listen on an ephemeral loopback port and serve ONE connection with the
/// certificate of `chain[0]` (the rest of the slice are the intermediates).
/// `script` gets the TLS stream after the request head was read, and may
/// write an answer. Returns the port and a handle whose result says what the
/// server saw.
pub fn serve(
    chain: &[&Identity],
    script: impl FnOnce(&mut Stream) + Send + 'static,
) -> (u16, JoinHandle<Seen>) {
    let config = server_config(chain);
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let handle = std::thread::spawn(move || {
        let (sock, _) = listener.accept().unwrap();
        let conn = ServerConnection::new(config).unwrap();
        let mut stream = StreamOwned::new(conn, sock);
        let mut head = Vec::new();
        let mut buf = [0u8; 1024];
        while !head.windows(4).any(|w| w == b"\r\n\r\n") {
            match stream.read(&mut buf) {
                Ok(0) | Err(_) => return None,
                Ok(n) => head.extend_from_slice(&buf[..n]),
            }
        }
        script(&mut stream);
        Some(head)
    });
    (port, handle)
}

/// The text of a request head.
pub fn text(head: &[u8]) -> String {
    String::from_utf8_lossy(head).into_owned()
}
