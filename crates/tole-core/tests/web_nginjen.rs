//! #625 part 2: the nginjen backend for `web_search` and `web_fetch`.
//! A local stand-in for the backend on 127.0.0.1; targets are IP literals
//! with the private-address opt-out passed explicitly (no DNS, no network).

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use tole_core::secrets::SecretSet;
use tole_core::tool::Tool;
use tole_core::web::{nginjen_backend, NginjenFetchTool, WebSearchTool};

const KEY: &str = "nginjen-test-key-0123456789";
const SECRET: &str = "web-nginjen-secret-0123456789";

type Seen = Arc<Mutex<Vec<(String, Value)>>>;

/// Answers every request with `reply(path, body)`; records (head, body).
fn backend(reply: fn(&str, &Value) -> (u16, String)) -> (String, Seen) {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap();
    let seen: Seen = Arc::default();
    let s2 = Arc::clone(&seen);
    std::thread::spawn(move || {
        for s in l.incoming().flatten() {
            let mut s = s;
            let mut r = BufReader::new(s.try_clone().unwrap());
            let mut head = String::new();
            let mut len = 0usize;
            loop {
                let mut line = String::new();
                if r.read_line(&mut line).unwrap_or(0) == 0 || line.trim().is_empty() {
                    break;
                }
                if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    len = v.trim().parse().unwrap_or(0);
                }
                head.push_str(&line);
            }
            let mut body = vec![0u8; len];
            let _ = r.read_exact(&mut body);
            let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
            let path = head.split_whitespace().nth(1).unwrap_or("/").to_string();
            let (status, data) = reply(&path, &body);
            s2.lock().unwrap().push((head, body));
            let _ = s.write_all(
                format!(
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{data}",
                    data.len()
                )
                .as_bytes(),
            );
        }
    });
    (format!("http://{addr}"), seen)
}

fn secrets() -> SecretSet {
    SecretSet::default()
        .with("NGINJEN_API_KEY", KEY)
        .with("MY_TOKEN", SECRET)
}

#[test]
fn both_variables_select_the_backend() {
    let env = |pairs: &'static [(&'static str, &'static str)]| {
        move |k: &str| {
            pairs
                .iter()
                .find(|(n, _)| *n == k)
                .map(|(_, v)| v.to_string())
        }
    };
    assert_eq!(
        nginjen_backend(&env(&[
            ("NGINJEN_BASE_URL", "https://n.example/"),
            ("NGINJEN_API_KEY", KEY)
        ])),
        Ok(Some(("https://n.example".to_string(), KEY.to_string())))
    );
    for pairs in [
        &[("NGINJEN_BASE_URL", "https://n.example")][..],
        &[("NGINJEN_API_KEY", KEY)][..],
        &[("NGINJEN_BASE_URL", "  "), ("NGINJEN_API_KEY", KEY)][..],
    ] {
        assert_eq!(nginjen_backend(&env(pairs)), Ok(None), "{pairs:?}");
    }
    let e = nginjen_backend(&env(&[
        ("NGINJEN_BASE_URL", "ftp://n.example"),
        ("NGINJEN_API_KEY", KEY),
    ]))
    .unwrap_err();
    assert!(e.contains("NGINJEN_BASE_URL") && !e.contains(KEY), "{e}");
}

#[test]
fn search_posts_the_redacted_query_with_the_bearer_key() {
    let (base, seen) = backend(|_, _| {
        (
            200,
            json!({"results": [{"title": "T", "url": "https://r.example", "snippet": "S"}]})
                .to_string(),
        )
    });
    let t = WebSearchTool::nginjen(&base, KEY).with_secrets(&secrets());
    let out = t
        .execute(json!({"query": format!("rust {SECRET}")}))
        .unwrap();
    assert_eq!(out["results"][0]["url"], "https://r.example");
    let (head, body) = seen.lock().unwrap()[0].clone();
    assert!(head.starts_with("POST /search "), "{head}");
    assert!(head
        .to_ascii_lowercase()
        .contains(&format!("authorization: bearer {KEY}").to_ascii_lowercase()));
    assert!(!body["query"].as_str().unwrap().contains(SECRET), "{body}");
}

fn fetch(base: &str) -> NginjenFetchTool {
    NginjenFetchTool::new(base, KEY, true).with_secrets(&secrets())
}

#[test]
fn fetch_posts_url_and_format_and_maps_the_formats() {
    let (base, seen) = backend(|_, b| {
        let content = format!("converted as {}", b["format"].as_str().unwrap_or("?"));
        (
            200,
            json!({"url": b["url"], "content": content, "status": "ok"}).to_string(),
        )
    });
    let t = fetch(&base);
    let out = t
        .execute(json!({"url": "http://127.0.0.1:9/page"}))
        .unwrap();
    assert_eq!(out["format"], "markdown");
    assert_eq!(out["text"], "converted as markdown");
    for (ours, theirs) in [
        ("text", "content"),
        ("html", "rawHtml"),
        ("compact", "compact"),
    ] {
        let o = t
            .execute(json!({"url": "http://127.0.0.1:9/page", "format": ours}))
            .unwrap();
        assert_eq!(o["text"], format!("converted as {theirs}"), "{ours}");
    }
    let (head, body) = seen.lock().unwrap()[0].clone();
    assert!(head.starts_with("POST /scrape "), "{head}");
    assert!(head.to_ascii_lowercase().contains("authorization: bearer"));
    assert_eq!(
        body,
        json!({"url": "http://127.0.0.1:9/page", "format": "markdown"})
    );
    assert!(t
        .execute(json!({"url": "http://127.0.0.1:9/", "format": "pdf"}))
        .is_err());
}

#[test]
fn links_come_back_as_a_list() {
    let (base, _) = backend(|_, _| {
        (200, json!({"content": json!([{"url": "https://a.example", "text": "A"}]).to_string(), "format": "links"}).to_string())
    });
    let out = fetch(&base)
        .execute(json!({"url": "http://127.0.0.1:9/", "format": "links"}))
        .unwrap();
    assert_eq!(out["links"][0]["url"], "https://a.example");
}

#[test]
fn a_jina_result_is_discarded() {
    let (base, _) = backend(|_, _| {
        (
            200,
            json!({"content": "via jina", "engine_used": "jina"}).to_string(),
        )
    });
    let e = fetch(&base)
        .execute(json!({"url": "http://127.0.0.1:9/"}))
        .unwrap_err();
    assert!(e.contains("Jina") && !e.contains("via jina"), "{e}");
}

#[test]
fn targets_are_vetted_and_the_key_never_shows() {
    let (base, seen) = backend(|_, _| (500, format!("{{\"error\": \"boom {KEY}\"}}")));
    let t = fetch(&base);
    let e = |v: Value| t.execute(v).unwrap_err();
    assert!(e(json!({"url": "ftp://x/"})).contains("http(s)"));
    assert!(e(json!({"url": "https://user:pw@127.0.0.1/"})).contains("credentials"));
    let leak = e(json!({"url": format!("http://127.0.0.1:9/?k={SECRET}")}));
    assert!(
        leak.contains("known secret") && !leak.contains(SECRET),
        "{leak}"
    );
    // Private targets are refused unless allowed (nothing sent).
    let strict = NginjenFetchTool::new(&base, KEY, false).with_secrets(&secrets());
    assert!(strict
        .execute(json!({"url": "http://127.0.0.1:9/"}))
        .is_err());
    assert!(
        seen.lock().unwrap().is_empty(),
        "nothing reached the backend"
    );
    // A backend error never shows the key.
    let err = e(json!({"url": "http://127.0.0.1:9/"}));
    assert!(!err.contains(KEY), "{err}");
}
