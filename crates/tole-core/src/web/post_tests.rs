use super::*;
use std::io::{Read, Write};
use std::net::{Ipv4Addr, Ipv6Addr, TcpListener};
use std::sync::mpsc;

fn ip(s: &str) -> IpAddr {
    s.parse().unwrap()
}

const LOOP: AddrPolicy = AddrPolicy::new(true, false);
const PRIV: AddrPolicy = AddrPolicy::new(false, true);
const BOTH: AddrPolicy = AddrPolicy::new(true, true);
const NONE: AddrPolicy = AddrPolicy::new(false, false);

#[test]
fn a_public_address_is_always_allowed() {
    for p in [NONE, LOOP, PRIV, BOTH] {
        assert!(p.allows(
            "example.com",
            &[ip("93.184.216.34"), ip("2606:2800:220:1::1")]
        ));
    }
}

#[test]
fn loopback_needs_the_flag_and_a_loopback_host() {
    // The flag and a loopback host together.
    for host in ["localhost", "127.0.0.1", "127.8.9.10", "::1"] {
        assert!(LOOP.allows(host, &[ip("127.0.0.1")]), "{host}");
        assert!(LOOP.allows(host, &[ip("::1")]), "{host}");
        assert!(!NONE.allows(host, &[ip("127.0.0.1")]), "{host}: no flag");
        // `private` does not include loopback.
        assert!(
            !PRIV.allows(host, &[ip("127.0.0.1")]),
            "{host}: private only"
        );
    }
    // A hostile DNS name pointed at loopback is refused even with the flag.
    for host in [
        "gateway.example.com",
        "localhost.evil.com",
        "127.0.0.1.evil.com",
    ] {
        assert!(!LOOP.allows(host, &[ip("127.0.0.1")]), "{host}");
        assert!(!BOTH.allows(host, &[ip("::1")]), "{host}");
    }
    // The unspecified address reaches this host too: it needs the loopback
    // flag and a loopback host, and `private` does not unlock it.
    for a in ["0.0.0.0", "::", "::ffff:0.0.0.0"] {
        assert!(!NONE.allows("localhost", &[ip(a)]), "{a}");
        assert!(!PRIV.allows("localhost", &[ip(a)]), "{a}: private only");
        assert!(
            !PRIV.allows("gateway.example.com", &[ip(a)]),
            "{a}: private only"
        );
        assert!(
            !LOOP.allows("gateway.example.com", &[ip(a)]),
            "{a}: not a loopback host"
        );
        assert!(LOOP.allows("localhost", &[ip(a)]), "{a}");
    }
    // IPv4-mapped loopback is loopback, not a way around the rule.
    assert!(LOOP.allows("localhost", &[ip("::ffff:127.0.0.1")]));
    assert!(!NONE.allows("localhost", &[ip("::ffff:127.0.0.1")]));
    assert!(!LOOP.allows("a.example.com", &[ip("::ffff:127.0.0.1")]));
}

#[test]
fn every_private_form_needs_the_private_flag() {
    let forms = [
        "10.0.0.1",
        "172.16.5.5",
        "192.168.1.1",
        "169.254.169.254", // cloud metadata
        "100.64.0.1",      // CGNAT
        "224.0.0.1",
        "fd00::1",         // ULA
        "fe80::1",         // link-local
        "::ffff:10.0.0.1", // v4-mapped
        "::ffff:169.254.169.254",
        "64:ff9b::a9fe:a9fe",       // NAT64 of 169.254.169.254
        "::ffff:0:a9fe:a9fe",       // IPv4-translated (SIIT) of 169.254.169.254
        "::5efe:a9fe:a9fe",         // ISATAP of 169.254.169.254
        "2001:db8::200:5efe:a00:1", // ISATAP of 10.0.0.1 under another prefix
        "2002:a9fe:a9fe::1",        // 6to4 of 169.254.169.254
    ];
    for f in forms {
        assert!(!NONE.allows("h.example.com", &[ip(f)]), "{f}");
        assert!(
            !LOOP.allows("h.example.com", &[ip(f)]),
            "{f}: loopback flag is not private"
        );
        assert!(PRIV.allows("h.example.com", &[ip(f)]), "{f}");
    }
}

#[test]
fn one_bad_address_among_many_refuses_the_host() {
    let addrs = [ip("93.184.216.34"), ip("169.254.169.254")];
    assert!(!NONE.allows("h.example.com", &addrs));
    assert!(!LOOP.allows("h.example.com", &addrs));
    assert!(PRIV.allows("h.example.com", &addrs));
    // A loopback host that also resolves to a private address needs both.
    let both = [ip("127.0.0.1"), ip("10.0.0.5")];
    assert!(!LOOP.allows("localhost", &both));
    assert!(!PRIV.allows("localhost", &both));
    assert!(BOTH.allows("localhost", &both));
}

#[test]
fn the_url_rules() {
    // https is fine for any host, with any policy.
    assert_eq!(check_url("https://hooks.example.com/tole", NONE), Ok(()));
    assert_eq!(check_url("https://127.0.0.1:8443/x", NONE), Ok(())); // the resolver decides the address
                                                                     // http only for a loopback host with the flag.
    for url in [
        "http://127.0.0.1:8787/e",
        "http://localhost/e",
        "http://[::1]:9/e",
    ] {
        assert_eq!(check_url(url, LOOP), Ok(()), "{url}");
        assert_eq!(check_url(url, NONE), Err(PostError::SchemeRefused), "{url}");
        assert_eq!(check_url(url, PRIV), Err(PostError::SchemeRefused), "{url}");
    }
    for url in [
        "http://example.com/e",
        "http://10.0.0.5/e",
        "http://localhost.evil.com/e",
        "http://127.0.0.1.evil.com/e",
    ] {
        assert_eq!(check_url(url, BOTH), Err(PostError::SchemeRefused), "{url}");
    }
    // Shape.
    assert!(matches!(
        check_url("https://u:p@example.com/", NONE),
        Err(PostError::InvalidUrl(_))
    ));
    assert!(matches!(
        check_url("https://user@example.com/", NONE),
        Err(PostError::InvalidUrl(_))
    ));
    assert!(matches!(
        check_url("ftp://example.com/", NONE),
        Err(PostError::InvalidUrl(_))
    ));
    assert!(matches!(
        check_url("not a url", NONE),
        Err(PostError::InvalidUrl(_))
    ));
    assert!(matches!(
        check_url("https:///path", NONE),
        Err(PostError::InvalidUrl(_))
    ));
    let long = format!("https://example.com/{}", "a".repeat(MAX_URL_LEN));
    assert!(matches!(
        check_url(&long, NONE),
        Err(PostError::InvalidUrl(_))
    ));
}

#[test]
fn the_header_rules() {
    let h = |n: &str, v: &str| vec![(n.to_string(), v.to_string())];
    assert_eq!(check_headers(&h("X-Tole-Signature", "v2=ab")), Ok(()));
    assert_eq!(
        check_headers(&h("Content-Type", "application/json")),
        Ok(())
    );
    for name in [
        "Host",
        "host",
        "Content-Length",
        "Transfer-Encoding",
        "Connection",
        "Cookie",
        "Authorization",
        "Proxy-Authorization",
        "Proxy-Foo",
    ] {
        assert!(
            matches!(
                check_headers(&h(name, "x")),
                Err(PostError::InvalidHeader(_))
            ),
            "{name}"
        );
    }
    assert!(matches!(
        check_headers(&h("bad name", "x")),
        Err(PostError::InvalidHeader(_))
    ));
    assert!(matches!(
        check_headers(&h("X-A", "line\r\nbreak")),
        Err(PostError::InvalidHeader(_))
    ));
}

#[test]
fn the_error_text_is_fixed_codes() {
    for (e, text) in [
        (PostError::SchemeRefused, "scheme_refused"),
        (PostError::RefusedAddress, "refused_address"),
        (PostError::Dns, "dns"),
        (PostError::Redirect(302), "redirect_refused: 302"),
        (PostError::Timeout, "timeout"),
        (PostError::BodyTooLarge, "body_too_large"),
    ] {
        assert_eq!(e.to_string(), text);
    }
}

/// A one-shot HTTP server: records the raw request and answers `reply`.
fn serve_once(reply: &'static str) -> (u16, mpsc::Receiver<Vec<u8>>) {
    let l = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = l.local_addr().unwrap().port();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let (mut s, _) = l.accept().unwrap();
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            let n = s.read(&mut chunk).unwrap_or(0);
            buf.extend_from_slice(&chunk[..n]);
            if n == 0 {
                break;
            }
            // Headers end, then the declared body.
            if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&buf[..end]).to_ascii_lowercase();
                let want = head
                    .lines()
                    .find_map(|l| l.strip_prefix("content-length: "))
                    .and_then(|v| v.trim().parse::<usize>().ok())
                    .unwrap_or(0);
                if buf.len() >= end + 4 + want {
                    break;
                }
            }
        }
        let _ = s.write_all(reply.as_bytes());
        let _ = tx.send(buf);
    });
    (port, rx)
}

fn text(raw: &[u8]) -> String {
    String::from_utf8_lossy(raw).into_owned()
}

#[test]
fn it_posts_the_body_and_headers_and_returns_the_status() {
    let (port, rx) = serve_once("HTTP/1.1 200 OK\r\nContent-Length: 0\r\nRetry-After: 7\r\n\r\n");
    let url = format!("http://127.0.0.1:{port}/tole/events");
    let headers = vec![
        ("X-Tole-Delivery".to_string(), "cron:a-1".to_string()),
        ("Content-Type".to_string(), "application/json".to_string()),
    ];
    let mut p = PostParams::new(&url, br#"{"v":1}"#);
    p.headers = &headers;
    p.policy = LOOP;
    let res = vetted_post(&p).unwrap();
    assert_eq!(res.status, 200);
    assert_eq!(res.retry_after.as_deref(), Some("7"));

    let raw = text(&rx.recv_timeout(Duration::from_secs(5)).unwrap());
    let lower = raw.to_ascii_lowercase();
    assert!(raw.starts_with("POST /tole/events HTTP/1.1\r\n"), "{raw}");
    assert!(lower.contains("x-tole-delivery: cron:a-1\r\n"), "{raw}");
    assert!(
        lower.contains("content-type: application/json\r\n"),
        "{raw}"
    );
    assert!(lower.contains("content-length: 7\r\n"), "{raw}");
    assert!(
        lower.contains(&format!(
            "user-agent: tole/{}\r\n",
            env!("CARGO_PKG_VERSION")
        )),
        "{raw}"
    );
    assert!(
        !lower.contains("cookie") && !lower.contains("authorization"),
        "{raw}"
    );
    assert!(raw.ends_with(r#"{"v":1}"#), "{raw}");
}

#[test]
fn a_non_2xx_status_is_a_response_not_an_error() {
    let (port, _rx) = serve_once("HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n");
    let url = format!("http://127.0.0.1:{port}/");
    let mut p = PostParams::new(&url, b"{}");
    p.policy = LOOP;
    assert_eq!(vetted_post(&p).unwrap().status, 503);
}

#[test]
fn a_redirect_is_refused_and_never_followed() {
    // The redirect target is a second server that must see no connection.
    let target = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    target.set_nonblocking(true).unwrap();
    let tport = target.local_addr().unwrap().port();
    let reply: &'static str = Box::leak(
        format!("HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:{tport}/hit\r\nContent-Length: 0\r\n\r\n")
            .into_boxed_str(),
    );
    let (port, _rx) = serve_once(reply);
    let url = format!("http://127.0.0.1:{port}/");
    let mut p = PostParams::new(&url, b"{}");
    p.policy = LOOP;
    assert_eq!(vetted_post(&p), Err(PostError::Redirect(302)));
    std::thread::sleep(Duration::from_millis(200));
    assert!(target.accept().is_err(), "the Location was followed");
}

#[test]
fn a_silent_receiver_times_out() {
    let l = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = l.local_addr().unwrap().port();
    let _hold = std::thread::spawn(move || {
        let (s, _) = l.accept().unwrap();
        std::thread::sleep(Duration::from_secs(4));
        drop(s);
    });
    let url = format!("http://127.0.0.1:{port}/");
    let mut p = PostParams::new(&url, b"{}");
    p.policy = LOOP;
    p.timeout = Duration::from_secs(1);
    let started = std::time::Instant::now();
    assert_eq!(vetted_post(&p), Err(PostError::Timeout));
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "{:?}",
        started.elapsed()
    );
}

#[test]
fn a_closed_port_is_a_transport_error_with_a_fixed_code() {
    let port = {
        let l = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        l.local_addr().unwrap().port()
    };
    let url = format!("http://127.0.0.1:{port}/");
    let mut p = PostParams::new(&url, b"{}");
    p.policy = LOOP;
    match vetted_post(&p) {
        Err(PostError::Transport(kind)) => {
            assert!(!kind.contains("127.0.0.1"), "{kind}");
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn the_resolver_refuses_before_any_connection_and_says_so() {
    // https to a loopback host without the flag: the scheme is fine, the
    // resolver refuses the address, and nothing connects.
    let l = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    l.set_nonblocking(true).unwrap();
    let port = l.local_addr().unwrap().port();
    for policy in [NONE, PRIV] {
        let url = format!("https://localhost:{port}/");
        let mut p = PostParams::new(&url, b"{}");
        p.policy = policy;
        assert_eq!(
            vetted_post(&p),
            Err(PostError::RefusedAddress),
            "{policy:?}"
        );
        let url = format!("https://127.0.0.1:{port}/");
        let mut p = PostParams::new(&url, b"{}");
        p.policy = policy;
        assert_eq!(
            vetted_post(&p),
            Err(PostError::RefusedAddress),
            "{policy:?}"
        );
    }
    let url = format!("https://[{}]:{port}/", Ipv6Addr::LOCALHOST);
    assert_eq!(
        vetted_post(&PostParams::new(&url, b"{}")),
        Err(PostError::RefusedAddress)
    );
    std::thread::sleep(Duration::from_millis(100));
    assert!(
        l.accept().is_err(),
        "a connection was made to a refused address"
    );
}

#[test]
fn a_name_that_does_not_resolve_is_dns_not_a_refusal() {
    // `.invalid` never resolves (RFC 2606), with or without network.
    let p = PostParams::new("https://gateway.tole-test.invalid/", b"{}");
    assert_eq!(vetted_post(&p), Err(PostError::Dns));
}

#[test]
fn the_body_cap_is_checked_before_anything_else() {
    let body = vec![b'a'; MAX_POST_BODY + 1];
    let p = PostParams::new("https://example.com/", &body);
    assert_eq!(vetted_post(&p), Err(PostError::BodyTooLarge));
    // At the cap it passes this check (and fails later on the network).
    let body = vec![b'a'; MAX_POST_BODY];
    let p = PostParams::new("https://gateway.tole-test.invalid/", &body);
    assert_eq!(vetted_post(&p), Err(PostError::Dns));
}

#[test]
fn the_timeout_is_clamped() {
    assert_eq!(clamp_timeout(Duration::ZERO), Duration::from_secs(1));
    assert_eq!(
        clamp_timeout(Duration::from_millis(1)),
        Duration::from_secs(1)
    );
    assert_eq!(
        clamp_timeout(Duration::from_secs(10)),
        Duration::from_secs(10)
    );
    assert_eq!(
        clamp_timeout(Duration::from_secs(1000)),
        Duration::from_secs(30)
    );
}

#[test]
fn a_receiver_that_trickles_its_answer_is_cut_off_by_the_whole_request_timeout() {
    // The receiver sends one header byte at a time: a per-read timeout would
    // never fire, only the whole-request one can cut this off.
    let l = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = l.local_addr().unwrap().port();
    let _drip = std::thread::spawn(move || {
        let (mut s, _) = l.accept().unwrap();
        s.set_read_timeout(Some(Duration::from_millis(200))).ok();
        let mut sink = [0u8; 4096];
        let _ = s.read(&mut sink);
        for b in b"HTTP/1.1 200 OK\r\nX-Slow: aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\r\n\r\n" {
            if s.write_all(&[*b]).is_err() {
                return;
            }
            std::thread::sleep(Duration::from_millis(250));
        }
    });
    let url = format!("http://127.0.0.1:{port}/");
    let mut p = PostParams::new(&url, b"{}");
    p.policy = LOOP;
    p.timeout = Duration::from_secs(1);
    let started = std::time::Instant::now();
    assert_eq!(vetted_post(&p), Err(PostError::Timeout));
    // The answer would take about 13 s to arrive; the cut is at about 1 s.
    assert!(
        started.elapsed() < Duration::from_secs(8),
        "{:?}",
        started.elapsed()
    );
}
