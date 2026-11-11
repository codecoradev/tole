//! #558 image input over the REAL serve face: `POST /sessions/{id}/images`
//! stores a raw body (type sniffed from the bytes), a prompt references it
//! by id, the turn records only the reference, and
//! `GET /sessions/{id}/images/{fnv1a64}` serves it back (also after a
//! restart + resume). Swapped files are reported, never served; oversized,
//! wrong-type, over-quota and unauthenticated requests are refused.

// Spawns the real `tole serve` binary (`shell-tools` feature, #349).
#![cfg(feature = "shell-tools")]

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

mod common;
mod serve_harness;
use serve_harness::{new_session, Serve};

// A 1x1 PNG (header + IHDR; the server sniffs, it does not decode).
const PNG: &[u8] = &[
    0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 0x0D, 0x49, 0x48, 0x44, 0x52, 0, 0, 0,
    1, 0, 0, 0, 1, 8, 6, 0, 0, 0, 0x1F, 0x15, 0xC4, 0x89,
];

/// Provider mock that records every request body and answers a final text.
fn recording_mock() -> (String, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&seen);
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let log = Arc::clone(&log);
            std::thread::spawn(move || {
                let mut r = BufReader::new(stream.try_clone().unwrap());
                let mut len = 0usize;
                loop {
                    let mut line = String::new();
                    if r.read_line(&mut line).unwrap_or(0) == 0 {
                        return;
                    }
                    if line == "\r\n" {
                        break;
                    }
                    if let Some((k, v)) = line.split_once(':') {
                        if k.eq_ignore_ascii_case("content-length") {
                            len = v.trim().parse().unwrap_or(0);
                        }
                    }
                }
                let mut body = vec![0u8; len];
                let _ = r.read_exact(&mut body);
                let text = String::from_utf8_lossy(&body).into_owned();
                let streamed = serde_json::from_str::<Value>(&text)
                    .ok()
                    .and_then(|v| v.get("stream").and_then(Value::as_bool))
                    == Some(true);
                log.lock().unwrap().push(text);
                let (kind, data) = if streamed {
                    let delta = json!({"choices": [{"index": 0,
                        "delta": {"content": "SAW-IT"}, "finish_reason": null}]});
                    let usage = json!({"choices": [], "usage": {"prompt_tokens": 1,
                        "completion_tokens": 1, "total_tokens": 2}});
                    (
                        "text/event-stream",
                        format!("data: {delta}\n\ndata: {usage}\n\ndata: [DONE]\n\n"),
                    )
                } else {
                    let data = json!({
                        "id": "c", "object": "chat.completion", "created": 1, "model": "mock",
                        "choices": [{"index": 0, "finish_reason": "stop",
                            "message": {"role": "assistant", "content": "SAW-IT"}}],
                        "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
                    });
                    ("application/json", data.to_string())
                };
                let mut s = stream;
                let _ = write!(
                    s,
                    "HTTP/1.1 200 OK\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{data}",
                    data.len()
                );
            });
        }
    });
    (format!("http://{addr}/v1"), seen)
}

struct Resp {
    code: u16,
    headers: String,
    body: Vec<u8>,
}

impl Resp {
    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or(Value::Null)
    }
    fn header(&self, name: &str) -> Option<String> {
        self.headers.lines().find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.eq_ignore_ascii_case(name).then(|| v.trim().to_string())
        })
    }
}

/// One raw request: `body` as bytes, an optional declared length override.
fn raw(serve: &Serve, method: &str, path: &str, auth: bool, extra: &str, body: &[u8]) -> Resp {
    let mut s = std::net::TcpStream::connect(("127.0.0.1", serve.port)).unwrap();
    s.set_read_timeout(Some(std::time::Duration::from_secs(30)))
        .unwrap();
    let auth = if auth {
        "Authorization: Bearer t0ken\r\n"
    } else {
        ""
    };
    let head = format!(
        "{method} {path} HTTP/1.1\r\n{auth}{extra}Content-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    s.write_all(head.as_bytes()).unwrap();
    let _ = s.write_all(body);
    let mut out = Vec::new();
    let _ = s.read_to_end(&mut out);
    let split = out
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("a response head");
    let headers = String::from_utf8_lossy(&out[..split]).into_owned();
    let code = headers
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    Resp {
        code,
        headers,
        body: out[split + 4..].to_vec(),
    }
}

fn upload(serve: &Serve, id: &str, data: &[u8]) -> Resp {
    raw(
        serve,
        "POST",
        &format!("/sessions/{id}/images"),
        true,
        "Content-Type: image/png\r\n",
        data,
    )
}

fn get_image(serve: &Serve, id: &str, hash: &str, auth: bool) -> Resp {
    raw(
        serve,
        "GET",
        &format!("/sessions/{id}/images/{hash}"),
        auth,
        "",
        b"",
    )
}

fn home(serve: &Serve) -> std::path::PathBuf {
    serve.dir.join("codecora-home")
}

fn spawn(base: &str, env: &[(&str, &str)]) -> Serve {
    let dir = std::env::temp_dir().join(format!(
        "tole-images-{}-{:x}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(dir.join("codecora-home")).unwrap();
    let home = dir.join("codecora-home");
    let mut all: Vec<(&str, &str)> = vec![("CODECORA_HOME", home.to_str().unwrap())];
    all.extend_from_slice(env);
    Serve::spawn_in_env(base, dir.clone(), true, &all)
}

#[test]
fn upload_prompt_read_and_resume_after_a_restart() {
    let (base, seen) = recording_mock();
    let mut serve = spawn(&base, &[]);
    let id = new_session(&serve);

    // Upload: the reference, no server path.
    let up = upload(&serve, &id, PNG);
    assert_eq!(up.code, 200, "{:?}", up.json());
    let r = up.json();
    let hash = r["fnv1a64"].as_str().unwrap().to_string();
    assert_eq!(r["id"], json!(hash));
    assert_eq!(r["mime"], "image/png");
    assert_eq!(r["bytes"], json!(PNG.len()));
    assert!(r.get("path").is_none(), "{r}");

    // Prompt with the image: the provider receives it, the log keeps only
    // the reference.
    let (code, body) = serve.http(
        "POST",
        &format!("/sessions/{id}/prompt"),
        Some(&json!({"text": "what is this?", "images": [hash]}).to_string()),
    );
    assert_eq!(code, 200, "{body}");
    assert_eq!(body["text"], "SAW-IT", "{body}");
    let sent = seen.lock().unwrap().last().cloned().unwrap();
    assert!(sent.contains("data:image/png;base64,"), "{sent}");
    let log = std::fs::read_to_string(serve.session_file(&id)).unwrap();
    assert!(log.contains(&format!("\"fnv1a64\":\"{hash}\"")), "{log}");
    assert!(!log.contains("base64"), "no blob in the log");

    // Read it back: exact bytes, fixed type, nosniff.
    let got = get_image(&serve, &id, &hash, true);
    assert_eq!(got.code, 200, "{}", got.headers);
    assert_eq!(got.body, PNG);
    assert_eq!(got.header("content-type").as_deref(), Some("image/png"));
    assert_eq!(
        got.header("x-content-type-options").as_deref(),
        Some("nosniff")
    );

    // Restart + resume: the image is still served and still re-sent.
    serve.kill();
    let env_home = home(&serve);
    let restarted = Serve::spawn_in_env(
        &base,
        serve.dir.clone(),
        false,
        &[("CODECORA_HOME", env_home.to_str().unwrap())],
    );
    assert_eq!(
        get_image(&restarted, &id, &hash, true).code,
        404,
        "not live"
    );
    let (code, body) = restarted.http(
        "POST",
        "/sessions",
        Some(&json!({"resume": id}).to_string()),
    );
    assert_eq!(code, 200, "{body}");
    let got = get_image(&restarted, &id, &hash, true);
    assert_eq!(got.code, 200, "{:?}", got.json());
    assert_eq!(got.body, PNG);
    let (code, body) = restarted.http(
        "POST",
        &format!("/sessions/{id}/prompt"),
        Some(&json!({"text": "and again?"}).to_string()),
    );
    assert_eq!(code, 200, "{body}");
    let sent = seen.lock().unwrap().last().cloned().unwrap();
    assert!(
        sent.contains("data:image/png;base64,"),
        "the resumed transcript re-sends the image: {sent}"
    );
    drop(restarted);
}

#[test]
fn a_swapped_file_is_reported_and_unrecorded_images_are_404() {
    let (base, _) = recording_mock();
    let serve = spawn(&base, &[]);
    let id = new_session(&serve);
    let other = new_session(&serve);
    let hash = upload(&serve, &id, PNG).json()["id"]
        .as_str()
        .unwrap()
        .to_string();

    // Uploaded but not recorded yet: 404. Another session: 404 too.
    assert_eq!(get_image(&serve, &id, &hash, true).code, 404);
    let (code, body) = serve.http(
        "POST",
        &format!("/sessions/{id}/prompt"),
        Some(&json!({"text": "look", "images": [hash]}).to_string()),
    );
    assert_eq!(code, 200, "{body}");
    assert_eq!(get_image(&serve, &id, &hash, true).code, 200);
    assert_eq!(get_image(&serve, &other, &hash, true).code, 404);
    assert_eq!(get_image(&serve, &id, "0123456789abcdef", true).code, 404);
    assert_eq!(get_image(&serve, &id, "..%2f..%2fetc", true).code, 404);

    // Swap the stored bytes: reported (409), never served.
    let file = home(&serve)
        .join("tole/images/serve")
        .join(&id)
        .join(format!("{hash}.png"));
    let mut swapped = PNG.to_vec();
    swapped.extend_from_slice(b"tampered");
    std::fs::write(&file, &swapped).unwrap();
    let got = get_image(&serve, &id, &hash, true);
    assert_eq!(got.code, 409, "{:?}", got.json());
    assert!(
        got.json()["error"].as_str().unwrap().contains("changed"),
        "{:?}",
        got.json()
    );
    assert_ne!(got.body, swapped);
    // A prompt with the now-mismatched upload is refused as well.
    let (code, body) = serve.http(
        "POST",
        &format!("/sessions/{id}/prompt"),
        Some(&json!({"text": "again", "images": [hash]}).to_string()),
    );
    assert_eq!(code, 409, "{body}");
}

#[test]
fn bad_uploads_and_prompts_are_refused() {
    let (base, _) = recording_mock();
    let serve = spawn(&base, &[]);
    let id = new_session(&serve);

    // Wrong type: the request header claims png, the bytes are text.
    let r = upload(&serve, &id, b"definitely not an image");
    assert_eq!(r.code, 415, "{:?}", r.json());
    // Empty body.
    assert_eq!(upload(&serve, &id, b"").code, 400);
    // Oversized: refused from the declared length, before the body.
    let mut s = std::net::TcpStream::connect(("127.0.0.1", serve.port)).unwrap();
    write!(
        s,
        "POST /sessions/{id}/images HTTP/1.1\r\nAuthorization: Bearer t0ken\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        10 * 1024 * 1024 + 1
    )
    .unwrap();
    let mut out = String::new();
    let _ = s.read_to_string(&mut out);
    assert!(out.starts_with("HTTP/1.1 413"), "{out}");
    // Unknown session.
    assert_eq!(upload(&serve, "srv-nope", PNG).code, 404);

    // Unauthenticated: upload and read both 401 (and nothing stored).
    let r = raw(
        &serve,
        "POST",
        &format!("/sessions/{id}/images"),
        false,
        "",
        PNG,
    );
    assert_eq!(r.code, 401);
    // Over the plain 1 MiB body cap without the token: still 401, not 413,
    // so the answer does not depend on the token outside the limiter.
    assert!(
        oversized_upload(&serve, &id, None).starts_with("HTTP/1.1 401"),
        "unauthenticated oversized upload"
    );
    assert_eq!(get_image(&serve, &id, "0123456789abcdef", false).code, 401);
    // Nothing was stored by any refused upload (the 415 above included).
    let stored = std::fs::read_dir(home(&serve).join("tole/images/serve").join(&id))
        .map(|rd| rd.count())
        .unwrap_or(0);
    assert_eq!(stored, 0, "a refused upload leaves no file");

    // Prompts: unknown id, malformed id, wrong shape.
    for (images, want) in [
        (json!(["0123456789abcdef"]), 404),
        (json!(["../../etc/passwd"]), 400),
        (json!("0123456789abcdef"), 400),
        (json!([1]), 400),
    ] {
        let (code, body) = serve.http(
            "POST",
            &format!("/sessions/{id}/prompt"),
            Some(&json!({"text": "x", "images": images}).to_string()),
        );
        assert_eq!(code, want, "{images}: {body}");
    }
}

/// Declares a 2 MiB upload body (over the 1 MiB pre-auth cap, under the
/// 10 MiB image cap) with `bearer` as the token and returns the raw answer.
fn oversized_upload(serve: &Serve, id: &str, bearer: Option<&str>) -> String {
    let mut s = std::net::TcpStream::connect(("127.0.0.1", serve.port)).unwrap();
    let auth = bearer
        .map(|t| format!("Authorization: Bearer {t}\r\n"))
        .unwrap_or_default();
    write!(
        s,
        "POST /sessions/{id}/images HTTP/1.1\r\n{auth}Content-Length: {}\r\nConnection: close\r\n\r\n",
        2 * 1024 * 1024
    )
    .unwrap();
    let mut out = String::new();
    let _ = s.read_to_string(&mut out);
    out
}

/// The upload ceiling depends on the token, so an oversized declared body
/// must not become a token-validity oracle: a wrong token gets the same 401
/// as any request, counts as an auth failure, and hits the per-IP limiter.
#[test]
fn an_oversized_upload_with_a_wrong_token_is_401_and_rate_limited() {
    let (base, _seen) = recording_mock();
    let serve = spawn(&base, &[]);
    let id = new_session(&serve);
    for i in 0..tole_cli::auth::MAX_AUTH_FAILURES {
        let out = oversized_upload(&serve, &id, Some("wrong-token"));
        assert!(out.starts_with("HTTP/1.1 401"), "attempt {i}: {out}");
    }
    // The limiter now refuses this IP, even for the right token.
    let out = oversized_upload(&serve, &id, Some("wrong-token"));
    assert!(out.starts_with("HTTP/1.1 429"), "{out}");
    assert_eq!(get_image(&serve, &id, "0123456789abcdef", true).code, 429);
}

#[test]
fn quotas_are_enforced_per_session_and_in_total() {
    let (base, _) = recording_mock();
    let per_session = (PNG.len() + 12).to_string();
    let total = (3 * PNG.len()).to_string();
    let serve = spawn(
        &base,
        &[
            ("TOLE_SERVE_IMAGE_SESSION_QUOTA", &per_session),
            ("TOLE_SERVE_IMAGE_TOTAL_QUOTA", &total),
        ],
    );
    let a = new_session(&serve);
    let b = new_session(&serve);
    let c = new_session(&serve);
    let mut bigger = PNG.to_vec();
    bigger.extend_from_slice(b"0123456789");

    assert_eq!(upload(&serve, &a, PNG).code, 200);
    // The same bytes again cost nothing.
    assert_eq!(upload(&serve, &a, PNG).code, 200);
    let r = upload(&serve, &a, &bigger);
    assert_eq!(r.code, 507, "{:?}", r.json());
    assert!(r.json()["error"]
        .as_str()
        .unwrap()
        .contains("TOLE_SERVE_IMAGE_SESSION_QUOTA"));
    assert_eq!(upload(&serve, &b, &bigger).code, 200);
    let r = upload(&serve, &c, &bigger);
    assert_eq!(r.code, 507, "{:?}", r.json());
    assert!(r.json()["error"]
        .as_str()
        .unwrap()
        .contains("TOLE_SERVE_IMAGE_TOTAL_QUOTA"));
}
