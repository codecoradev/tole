use super::*;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;

const SECRET: &str = "web-builtin-secret-0123456789";

fn secrets() -> SecretSet {
    SecretSet::default().with("MY_TOKEN", SECRET)
}

type Seen = Arc<Mutex<Vec<String>>>;

/// A local server: `route(path) -> raw response`; records request heads.
fn serve(route: impl Fn(&str) -> String + Send + 'static) -> (String, Seen) {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap();
    let seen: Seen = Arc::default();
    let s2 = Arc::clone(&seen);
    std::thread::spawn(move || {
        for s in l.incoming().flatten() {
            let mut s = s;
            let mut r = BufReader::new(s.try_clone().unwrap());
            let mut head = String::new();
            loop {
                let mut line = String::new();
                if r.read_line(&mut line).unwrap_or(0) == 0 || line.trim().is_empty() {
                    break;
                }
                head.push_str(&line);
            }
            let path = head.split_whitespace().nth(1).unwrap_or("/").to_string();
            s2.lock().unwrap().push(head);
            let _ = s.write_all(route(&path).as_bytes());
        }
    });
    (format!("http://{addr}"), seen)
}

fn resp(status: &str, ctype: &str, extra: &str, body: &str) -> String {
    format!(
        "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

const PAGE: &str = "<html><body><h1>Title</h1><p>Some <strong>bold</strong> text and \
                    <a href=\"/about\">About us</a>.</p></body></html>";

fn site(robots: &'static str) -> impl Fn(&str) -> String + Send + 'static {
    move |path| match path {
        "/robots.txt" => resp("200 OK", "text/plain", "", robots),
        "/page" => resp("200 OK", "text/html; charset=utf-8", "", PAGE),
        "/hop" => resp("302 Found", "text/html", "Location: /page\r\n", ""),
        "/image" => resp("200 OK", "image/png", "", "PNG"),
        "/big" => resp(
            "200 OK",
            "text/plain",
            "",
            &"x".repeat(FETCH_CAP_BYTES + 10),
        ),
        "/missing" => resp("404 Not Found", "text/html", "", "nope"),
        _ => resp("404 Not Found", "text/plain", "", ""),
    }
}

fn fetch_tool() -> BuiltinFetchTool {
    BuiltinFetchTool::new(Arc::new(TolHttp::new(&secrets(), true)))
}

#[test]
fn fetch_converts_html_and_follows_vetted_redirects() {
    let (base, seen) = serve(site(""));
    let t = fetch_tool();
    let out = t.execute(json!({"url": format!("{base}/hop")})).unwrap();
    assert_eq!(out["format"], "markdown");
    assert!(out["url"].as_str().unwrap().ends_with("/page"), "{out}");
    let text = out["text"].as_str().unwrap();
    assert!(
        text.contains("# Title") && text.contains("**bold**"),
        "{text}"
    );
    for (format, want) in [
        ("text", "Some bold text"),
        ("html", "<strong>bold</strong>"),
    ] {
        let o = t
            .execute(json!({"url": format!("{base}/page"), "format": format}))
            .unwrap();
        assert!(o["text"].as_str().unwrap().contains(want), "{format}: {o}");
    }
    let o = t
        .execute(json!({"url": format!("{base}/page"), "format": "links"}))
        .unwrap();
    assert_eq!(o["links"][0]["url"], format!("{base}/about"));
    // Every request carried tole's User-Agent, robots.txt included.
    let heads = seen.lock().unwrap();
    assert!(
        heads
            .iter()
            .all(|h| h.to_ascii_lowercase().contains("user-agent: tole/")),
        "{heads:?}"
    );
    assert!(heads[0].starts_with("GET /robots.txt"));
}

#[test]
fn fetch_refuses_what_the_guards_refuse() {
    let (base, seen) = serve(site(""));
    let t = fetch_tool();
    let e = |v: Value| t.execute(v).unwrap_err();
    assert!(e(json!({"url": "ftp://x/"})).contains("http(s)"));
    let leak = e(json!({"url": format!("{base}/page?k={SECRET}")}));
    assert!(
        leak.contains("known secret") && !leak.contains(SECRET),
        "{leak}"
    );
    assert!(e(json!({"url": format!("{base}/image")})).contains("content-type"));
    assert!(e(json!({"url": format!("{base}/missing")})).contains("404"));
    assert!(e(json!({"url": format!("{base}/page"), "format": "pdf"})).contains("format"));
    let before = seen.lock().unwrap().len();
    // Private addresses: refused before anything is sent.
    let strict = BuiltinFetchTool::new(Arc::new(TolHttp::new(&secrets(), false)));
    assert!(strict
        .execute(json!({"url": format!("{base}/page")}))
        .is_err());
    assert_eq!(
        seen.lock().unwrap().len(),
        before,
        "nothing reached the server"
    );
    // A big page is truncated and flagged, not refused.
    let o = t
        .execute(json!({"url": format!("{base}/big"), "format": "text"}))
        .unwrap();
    assert_eq!(o["truncated"], true);
    assert!(o["text"].as_str().unwrap().chars().count() <= TEXT_CAP_CHARS);
}

#[test]
fn robots_txt_is_honoured_and_fails_open() {
    let (base, _) = serve(site("User-agent: *\nDisallow: /page\n"));
    let e = fetch_tool()
        .execute(json!({"url": format!("{base}/page")}))
        .unwrap_err();
    assert!(e.contains("robots.txt"), "{e}");
    // No robots.txt (404): allowed.
    let (base, _) = serve(|path: &str| match path {
        "/page" => resp("200 OK", "text/html", "", PAGE),
        _ => resp("404 Not Found", "text/plain", "", ""),
    });
    assert!(fetch_tool()
        .execute(json!({"url": format!("{base}/page")}))
        .is_ok());
}

#[test]
fn every_hop_passes_the_hop_policy() {
    let s = secrets();
    assert!(check_hop(true, "https://a.example/x", &s).is_ok());
    assert!(matches!(
        check_hop(true, "http://a.example/x", &s),
        Err(HttpError::Redirect(_))
    ));
    assert!(check_hop(false, "http://a.example/x", &s).is_ok());
    assert!(check_hop(false, &format!("http://a.example/?t={SECRET}"), &s).is_err());
    // A redirect to a URL carrying a secret is refused mid-chain.
    let (base, seen) = serve(move |path: &str| match path {
        "/robots.txt" => resp("404 Not Found", "text/plain", "", ""),
        "/leak" => resp(
            "302 Found",
            "text/html",
            &format!("Location: /page?k={SECRET}\r\n"),
            "",
        ),
        _ => resp("200 OK", "text/html", "", PAGE),
    });
    let e = fetch_tool()
        .execute(json!({"url": format!("{base}/leak")}))
        .unwrap_err();
    assert!(e.contains("known secret") && !e.contains(SECRET), "{e}");
    assert!(!seen.lock().unwrap().iter().any(|h| h.contains(SECRET)));
}

#[test]
fn the_pacer_spaces_one_host_and_stays_bounded() {
    let p = Pacer::default();
    let start = Instant::now();
    p.wait("a.example");
    p.wait("b.example");
    assert!(
        start.elapsed() < Duration::from_millis(500),
        "different hosts do not wait"
    );
    p.wait("a.example");
    assert!(start.elapsed() >= HOST_INTERVAL, "the same host waits");
    for i in 0..(PACER_HOSTS + 10) {
        p.wait(&format!("h{i}.example"));
    }
    assert_eq!(p.hosts(), PACER_HOSTS);
}

/// Answers each engine by host, records the URLs it was asked for.
struct Engines {
    asked: Mutex<Vec<String>>,
    answer: fn(&str) -> (u16, &'static str),
}

impl HttpClient for Engines {
    fn get(&self, r: &HttpRequest) -> Result<HttpResponse, HttpError> {
        self.asked.lock().unwrap().push(r.url.clone());
        let (status, body) = (self.answer)(&r.url);
        Ok(HttpResponse {
            status,
            url: r.url.clone(),
            headers: vec![],
            body: body.as_bytes().to_vec(),
        })
    }
}

const DDG: &str =
    "<table><tr><td><a class=\"result-link\" href=\"https://example.org/a\">Result A</a>\
                   </td></tr><tr><td class=\"result-snippet\">About A</td></tr></table>";
const BRAVE: &str = "<div class=\"snippet\"><a href=\"https://example.org/b\"><div class=\"title\">Result B</div></a>\
                     <div class=\"snippet-description\">About B</div></div>";

#[test]
fn search_tries_duckduckgo_then_bing_then_brave_and_redacts() {
    let engines = Arc::new(Engines {
        asked: Mutex::default(),
        answer: |u| {
            if u.contains("duckduckgo") {
                (200, DDG)
            } else {
                (500, "")
            }
        },
    });
    let t = BuiltinSearchTool::new(engines.clone(), &secrets());
    let out = t
        .execute(json!({"query": format!("rust {SECRET}")}))
        .unwrap();
    assert_eq!(
        (out["engine"].as_str(), out["fallback"].as_bool()),
        (Some("duckduckgo-lite"), Some(false))
    );
    assert_eq!(out["results"][0]["url"], "https://example.org/a");
    let asked = engines.asked.lock().unwrap().clone();
    assert_eq!(asked.len(), 1);
    assert!(!asked[0].contains("web-builtin-secret") && !out.to_string().contains(SECRET));
    // A blocked engine (non-2xx) falls back, in order.
    let engines = Arc::new(Engines {
        asked: Mutex::default(),
        answer: |u| {
            if u.contains("brave") {
                (200, BRAVE)
            } else {
                (429, "")
            }
        },
    });
    let t = BuiltinSearchTool::new(engines.clone(), &secrets());
    let out = t.execute(json!({"query": "rust"})).unwrap();
    assert_eq!(
        (out["engine"].as_str(), out["fallback"].as_bool()),
        (Some("brave"), Some(true))
    );
    let asked = engines.asked.lock().unwrap().clone();
    assert!(
        asked[0].contains("duckduckgo") && asked[1].contains("bing") && asked[2].contains("brave"),
        "{asked:?}"
    );
    // All down: an error, no results invented.
    let engines = Arc::new(Engines {
        asked: Mutex::default(),
        answer: |_| (503, ""),
    });
    assert!(BuiltinSearchTool::new(engines, &secrets())
        .execute(json!({"query": "x"}))
        .is_err());
    // The query is capped.
    let engines = Arc::new(Engines {
        asked: Mutex::default(),
        answer: |_| (200, DDG),
    });
    let t = BuiltinSearchTool::new(engines.clone(), &secrets());
    let out = t
        .execute(json!({"query": "q".repeat(QUERY_CAP_CHARS * 2)}))
        .unwrap();
    assert!(out["query"].as_str().unwrap().chars().count() <= QUERY_CAP_CHARS);
}

#[test]
fn the_client_caps_engine_bodies() {
    let (base, _) =
        serve(|_: &str| resp("200 OK", "text/html", "", &"y".repeat(CLIENT_CAP_BYTES + 1)));
    let http = TolHttp::new(&secrets(), true);
    let req = HttpRequest::get(format!("{base}/")).max_bytes(usize::MAX);
    assert!(matches!(http.get(&req), Err(HttpError::TooLarge { .. })));
}
