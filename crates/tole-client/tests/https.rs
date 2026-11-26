//! The HTTPS transport (#1071) against a local TLS server with throwaway
//! certificates made by the test (`support/tls_pki.rs`): a verified connection
//! works, every certificate that does not verify is refused before the token
//! leaves, the token is only ever a header, redirects are never followed, and
//! every wait is bounded. Loopback only.

#[path = "support/tls_pki.rs"]
mod tls_pki;
#[path = "support/tls_server.rs"]
mod tls_server;

use std::io::Write;
use std::net::TcpListener;
use std::sync::mpsc;
use std::time::{Duration, Instant};
use tls_pki::{ca, issue, pem, Identity, Spec, Validity};
use tls_server::{serve, text, Stream};
use tole_client::{Client, Error, TlsTransport};

const TOKEN: &str = "s3cr3t-token-value";
const LOOPBACK: &[&str] = &["localhost"];
const OK: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 15\r\n\
Connection: close\r\n\r\n{\"status\":\"ok\"}";

fn ok(s: &mut Stream) {
    s.write_all(OK).unwrap();
    s.flush().unwrap();
}

/// A server certificate for `localhost` and 127.0.0.1, issued by `ca`.
fn server_cert(ca: &Identity) -> Identity {
    issue(&Spec::server(LOOPBACK, &[[127, 0, 0, 1]]), Some(ca))
}

/// A client that trusts only `ca`.
fn client_trusting(ca: &Identity, host: &str, port: u16) -> Client {
    let t = TlsTransport::new(&format!("https://{host}:{port}"))
        .unwrap()
        .with_ca_pem(&pem(&ca.cert_der))
        .unwrap();
    Client::new(t).with_token(TOKEN)
}

/// The server's own `Content-Length` / answer, then a pause until `gate` is
/// dropped, so a test never waits on a guessed sleep.
fn hold(s: &mut Stream, gate: &mpsc::Receiver<()>) {
    let _ = gate.recv_timeout(Duration::from_secs(30));
    let _ = s.flush();
}

// ---- the success path --------------------------------------------------------

#[test]
fn a_server_with_a_valid_certificate_answers_over_https() {
    let ca = ca("tole test ca");
    let leaf = server_cert(&ca);
    for host in ["localhost", "127.0.0.1"] {
        let (port, server) = serve(&[&leaf], ok);
        let health = client_trusting(&ca, host, port).health().unwrap();
        assert_eq!(health.status, "ok", "{host}");
        assert!(server.join().unwrap().is_some());
    }
}

#[test]
fn a_pinned_self_signed_certificate_is_accepted_by_name() {
    let own = issue(&Spec::server(LOOPBACK, &[]), None);
    let (port, server) = serve(&[&own], ok);
    let client = client_trusting(&own, "localhost", port);
    assert_eq!(client.health().unwrap().status, "ok");
    server.join().unwrap();
}

#[test]
fn client_connect_picks_https_from_the_scheme() {
    // `https://` is no longer `HttpsUnsupported`: the default build trusts the
    // bundled public roots, so a test certificate is refused as a certificate,
    // not as an unsupported scheme.
    let leaf = issue(&Spec::server(LOOPBACK, &[]), None);
    let (port, server) = serve(&[&leaf], ok);
    let client = Client::connect(&format!("https://localhost:{port}"), None, false).unwrap();
    let err = client.health().unwrap_err();
    assert!(matches!(err, Error::Tls(_)), "{err:?}");
    assert_eq!(server.join().unwrap(), None);
    // Plain http keeps its policy through the same entry point.
    assert!(matches!(
        Client::connect("http://example.com", None, false),
        Err(Error::InsecureTransport(_))
    ));
}

// ---- certificates that do not verify -----------------------------------------

/// The client refuses with [`Error::Tls`], not transient, with the token in
/// neither the error nor a byte the server received.
fn assert_refused(client: &Client, server: std::thread::JoinHandle<tls_server::Seen>) {
    let err = client.health().unwrap_err();
    assert!(
        matches!(&err, Error::Tls(m) if m.contains("invalid peer certificate")),
        "{err:?}"
    );
    assert!(!err.is_transient(), "a bad certificate is not retried");
    assert!(!err.to_string().contains(TOKEN));
    assert!(!format!("{err:?}").contains(TOKEN));
    // The handshake failed: the server never saw an application byte.
    assert_eq!(
        server.join().unwrap(),
        None,
        "no request reached the server"
    );
}

#[test]
fn an_unknown_issuer_is_refused_before_the_token_is_sent() {
    let trusted = ca("the ca the client trusts");
    let stranger = ca("some other ca");
    let leaf = server_cert(&stranger);
    let (port, server) = serve(&[&leaf], ok);
    assert_refused(&client_trusting(&trusted, "localhost", port), server);
}

#[test]
fn the_bundled_public_roots_refuse_a_private_certificate() {
    // No CA given: only the Mozilla roots are trusted, and the test CA is not
    // one of them.
    let private = ca("a private ca");
    let leaf = server_cert(&private);
    let (port, server) = serve(&[&leaf], ok);
    let client = Client::new(TlsTransport::new(&format!("https://localhost:{port}")).unwrap())
        .with_token(TOKEN);
    assert_refused(&client, server);
}

#[test]
fn a_certificate_for_another_host_is_refused() {
    let ca = ca("tole test ca");
    let leaf = issue(&Spec::server(&["other.example"], &[]), Some(&ca));
    let (port, server) = serve(&[&leaf], ok);
    assert_refused(&client_trusting(&ca, "localhost", port), server);
}

#[test]
fn an_expired_certificate_is_refused() {
    let ca = ca("tole test ca");
    let mut spec = Spec::server(LOOPBACK, &[]);
    spec.validity = Validity::Expired;
    let leaf = issue(&spec, Some(&ca));
    let (port, server) = serve(&[&leaf], ok);
    assert_refused(&client_trusting(&ca, "localhost", port), server);
}

#[test]
fn an_ip_url_needs_an_ip_name_in_the_certificate() {
    let ca = ca("tole test ca");
    // A name-only certificate does not cover https://127.0.0.1.
    let leaf = issue(&Spec::server(LOOPBACK, &[]), Some(&ca));
    let (port, server) = serve(&[&leaf], ok);
    assert_refused(&client_trusting(&ca, "127.0.0.1", port), server);
}

#[test]
fn there_is_no_way_to_turn_verification_off() {
    // The whole public surface that shapes trust: a CA list, a connect
    // timeout. Both keep verifying; a bad CA is a config error, not a bypass.
    let t = TlsTransport::new("https://localhost").unwrap();
    assert!(matches!(
        t.with_ca_pem(b"-----BEGIN CERTIFICATE-----\n-----END CERTIFICATE-----\n"),
        Err(Error::InvalidConfig(_))
    ));
}

// ---- the token ---------------------------------------------------------------

#[test]
fn the_token_is_a_header_and_nothing_else() {
    let ca = ca("tole test ca");
    let leaf = server_cert(&ca);
    let (port, server) = serve(&[&leaf], ok);
    let client = client_trusting(&ca, "localhost", port);
    assert!(!format!("{client:?}").contains(TOKEN));
    client.health().unwrap();
    let head = text(&server.join().unwrap().unwrap());
    let mut lines = head.split("\r\n");
    let request_line = lines.next().unwrap();
    assert_eq!(request_line, "GET /health HTTP/1.1");
    assert!(
        head.contains(&format!("\r\nAuthorization: Bearer {TOKEN}\r\n")),
        "{head}"
    );
    assert_eq!(head.matches(TOKEN).count(), 1, "exactly one place: {head}");
    // The Host header is the name, without the default port of another scheme.
    assert!(
        head.contains(&format!("\r\nHost: localhost:{port}\r\n")),
        "{head}"
    );
}

#[test]
fn a_credential_in_the_url_is_refused_and_not_echoed() {
    for url in [
        format!("https://user:{TOKEN}@localhost"),
        format!("https://localhost/?token={TOKEN}"),
        format!("https://localhost/{TOKEN}"),
    ] {
        let err = Client::connect_https(&url, None).unwrap_err();
        assert!(matches!(err, Error::InvalidUrl(_)), "{err:?}");
        assert!(!err.to_string().contains(TOKEN), "{err}");
    }
}

// ---- redirects ---------------------------------------------------------------

#[test]
fn a_redirect_is_never_followed_so_the_token_stays_put() {
    // B is another host the redirect points at. Following it would open a
    // connection to B before the call returned, so a quiet listener afterwards
    // proves nothing was sent there.
    let elsewhere = TcpListener::bind("127.0.0.1:0").unwrap();
    elsewhere.set_nonblocking(true).unwrap();
    let target = elsewhere.local_addr().unwrap().port();
    let ca = ca("tole test ca");
    let leaf = server_cert(&ca);
    for (status, location) in [
        (301, format!("https://127.0.0.1:{target}/health")),
        (302, format!("https://127.0.0.1:{target}/health")),
        (307, format!("http://127.0.0.1:{target}/health")),
        (308, format!("//127.0.0.1:{target}/health")),
        (302, "/health".to_string()),
    ] {
        let (port, server) = serve(&[&leaf], move |s| {
            let wire = format!(
                "HTTP/1.1 {status} Found\r\nLocation: {location}\r\nContent-Length: 0\r\n\
                 Connection: close\r\n\r\n"
            );
            s.write_all(wire.as_bytes()).unwrap();
        });
        let err = client_trusting(&ca, "localhost", port)
            .health()
            .unwrap_err();
        assert!(
            matches!(err, Error::Status { status: s, .. } if s == status),
            "{status}: {err:?}"
        );
        server.join().unwrap();
        assert!(
            matches!(elsewhere.accept(), Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock),
            "{status}: the redirect target was contacted"
        );
    }
}

// ---- timeouts ----------------------------------------------------------------

fn assert_timed_out(err: &Error, started: Instant) {
    assert!(
        matches!(err, Error::Io(e) | Error::Connect(e) if e.kind() == std::io::ErrorKind::TimedOut),
        "{err:?}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "{:?}",
        started.elapsed()
    );
}

#[test]
fn a_server_that_never_finishes_the_handshake_hits_the_connect_budget() {
    // Accepts the TCP connection and says nothing: the budget covers the
    // handshake, not only the TCP connect.
    let silent = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = silent.local_addr().unwrap().port();
    let transport = TlsTransport::new(&format!("https://localhost:{port}"))
        .unwrap()
        .with_connect_timeout(Duration::from_millis(300))
        .unwrap();
    let started = Instant::now();
    let err = Client::new(transport).health().unwrap_err();
    assert_timed_out(&err, started);
    // A connect timeout is a connect error: nothing was sent, a GET may retry.
    assert!(matches!(err, Error::Connect(_)) && err.is_transient());
    drop(silent);
}

#[test]
fn a_dead_port_is_a_transient_connect_error() {
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let err = Client::connect_https(&format!("https://localhost:{port}"), None)
        .unwrap()
        .health()
        .unwrap_err();
    assert!(
        matches!(err, Error::Connect(_)) && err.is_transient(),
        "{err:?}"
    );
}

#[test]
fn an_unanswered_request_hits_the_read_timeout() {
    let ca = ca("tole test ca");
    let leaf = server_cert(&ca);
    let (gate_tx, gate_rx) = mpsc::channel::<()>();
    let (port, server) = serve(&[&leaf], move |s| hold(s, &gate_rx));
    let client = client_trusting(&ca, "localhost", port)
        .with_request_timeout(Duration::from_millis(300))
        .unwrap();
    let started = Instant::now();
    let err = client.health().unwrap_err();
    assert_timed_out(&err, started);
    assert!(
        !err.is_transient(),
        "a request that was sent is not replayed"
    );
    drop(gate_tx);
    server.join().unwrap();
}

#[test]
fn a_stalled_body_hits_the_body_deadline() {
    let ca = ca("tole test ca");
    let leaf = server_cert(&ca);
    let (gate_tx, gate_rx) = mpsc::channel::<()>();
    let (port, server) = serve(&[&leaf], move |s| {
        s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 1000\r\n\r\n{")
            .unwrap();
        s.flush().unwrap();
        hold(s, &gate_rx);
    });
    let client = client_trusting(&ca, "localhost", port)
        .with_body_deadline(Duration::from_millis(400))
        .unwrap();
    let started = Instant::now();
    let err = client.health().unwrap_err();
    assert_timed_out(&err, started);
    drop(gate_tx);
    server.join().unwrap();
}

#[test]
fn a_connection_cut_without_close_notify_is_not_a_clean_end() {
    // The server promises 1000 bytes and vanishes (no TLS close_notify): the
    // client must not take the short body for a complete answer.
    let ca = ca("tole test ca");
    let leaf = server_cert(&ca);
    let (port, server) = serve(&[&leaf], |s| {
        s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 1000\r\n\r\n{\"status\"")
            .unwrap();
    });
    let err = client_trusting(&ca, "localhost", port)
        .health()
        .unwrap_err();
    assert!(matches!(err, Error::Io(_) | Error::Protocol(_)), "{err:?}");
    server.join().unwrap();
}
