//! #1043: `tole serve --socket <path>` over the REAL binary.
//!
//! The route tests at the top are written once and run on both listeners (the
//! TCP serve and the Unix socket serve answer the same routes, auth checks and
//! event stream). Below them: what only the socket has (the private directory,
//! stale-socket handling, the per-launch token that never reaches the argument
//! list or the environment) and the `/health` version fields.

// Spawns the real `tole serve` binary (`shell-tools` feature, #349); the local
// socket is a Unix feature (a Windows named pipe is a follow-up).
#![cfg(all(feature = "shell-tools", unix))]
#![allow(unsafe_code)] // SAFETY: test-only libc call on a path or descriptor the test owns.

use serde_json::Value;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

mod common;
use common::TestDir;

const TOKEN: &str = "per-launch-token-1043-abcdef";

#[derive(Clone, Copy, Debug)]
enum Transport {
    Tcp,
    Socket,
}

struct Server {
    child: Child,
    transport: Transport,
    port: u16,
    socket: PathBuf,
    // Declared last: dropped after the child is killed (see `Drop`).
    dir: TestDir,
}

impl Server {
    fn spawn(transport: Transport) -> Self {
        let dir = TestDir::in_system_temp("ts");
        let socket = dir.join("run").join("s.sock");
        std::fs::create_dir_all(dir.join("ws")).unwrap();
        std::fs::create_dir_all(dir.join("home")).unwrap();
        let token_file = dir.join("token");
        std::fs::write(&token_file, format!("{TOKEN}\n")).unwrap();
        std::fs::set_permissions(&token_file, std::fs::Permissions::from_mode(0o600)).unwrap();
        let (child, port) = common::spawn_listening(|port| {
            let mut cmd = serve_command(&dir);
            match transport {
                Transport::Tcp => cmd.args(["--port", &port.to_string()]),
                Transport::Socket => cmd.arg("--socket").arg(&socket),
            };
            cmd.arg("--token-file").arg(&token_file);
            cmd.stdin(Stdio::null()).stdout(Stdio::null());
            cmd
        });
        Server {
            child,
            transport,
            port,
            socket,
            dir,
        }
    }

    /// Send `req` verbatim over this server's listener; everything it answers.
    fn raw(&self, req: &str) -> String {
        let mut out = String::new();
        match self.transport {
            Transport::Tcp => {
                let mut s = TcpStream::connect(("127.0.0.1", self.port)).unwrap();
                s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
                s.write_all(req.as_bytes()).unwrap();
                let _ = s.read_to_string(&mut out);
            }
            Transport::Socket => {
                let mut s = UnixStream::connect(&self.socket).unwrap();
                s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
                s.write_all(req.as_bytes()).unwrap();
                let _ = s.read_to_string(&mut out);
            }
        }
        out
    }

    fn http(&self, method: &str, path: &str, token: Option<&str>, body: Option<&str>) -> Reply {
        let auth = token
            .map(|t| format!("Authorization: Bearer {t}\r\n"))
            .unwrap_or_default();
        let req = match body {
            Some(b) => format!(
                "{method} {path} HTTP/1.1\r\nHost: x\r\n{auth}Content-Type: application/json\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n{b}",
                b.len()
            ),
            None => {
                format!("{method} {path} HTTP/1.1\r\nHost: x\r\n{auth}Connection: close\r\n\r\n")
            }
        };
        Reply::parse(&self.raw(&req))
    }

    fn get(&self, path: &str) -> Reply {
        self.http("GET", path, Some(TOKEN), None)
    }

    /// Open an SSE stream and read until `needle` has arrived.
    fn stream_until(&self, path: &str, needle: &str) -> String {
        let req =
            format!("GET {path} HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer {TOKEN}\r\n\r\n");
        let mut got = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(15);
        let mut read = |s: &mut dyn Read| {
            let mut buf = [0u8; 4096];
            while Instant::now() < deadline {
                match s.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => got.extend_from_slice(&buf[..n]),
                    Err(_) => break,
                }
                if String::from_utf8_lossy(&got).contains(needle) {
                    break;
                }
            }
        };
        match self.transport {
            Transport::Tcp => {
                let mut s = TcpStream::connect(("127.0.0.1", self.port)).unwrap();
                s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
                s.write_all(req.as_bytes()).unwrap();
                read(&mut s);
            }
            Transport::Socket => {
                let mut s = UnixStream::connect(&self.socket).unwrap();
                s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
                s.write_all(req.as_bytes()).unwrap();
                read(&mut s);
            }
        }
        String::from_utf8_lossy(&got).into_owned()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = &self.dir;
    }
}

/// `tole serve` over `dir`'s workspace, hermetic, with a provider that is never called.
fn serve_command(dir: &Path) -> Command {
    let mut cmd = common::hermetic_command(env!("CARGO_BIN_EXE_tole"), &dir.join("home"), &[]);
    cmd.args(["serve", "--workspace"])
        .arg(dir.join("ws"))
        .current_dir(dir.join("ws"))
        .env("TOLE_BASE_URL", "http://127.0.0.1:9/v1")
        .env("TOLE_MODEL", "mock-model")
        .env("TOLE_API_KEY", "sk-test")
        .env_remove("TOLE_SERVE_TOKEN");
    cmd
}

struct Reply {
    code: u16,
    head: String,
    body: Value,
}

impl Reply {
    fn parse(raw: &str) -> Reply {
        let (head, body) = raw.split_once("\r\n\r\n").unwrap_or((raw, ""));
        let code = head
            .split_whitespace()
            .nth(1)
            .and_then(|c| c.parse().ok())
            .unwrap_or(0);
        Reply {
            code,
            head: head.to_string(),
            body: serde_json::from_str(body.trim()).unwrap_or(Value::Null),
        }
    }
}

// ---- Route tests shared by both listeners ----------------------------------

fn health_is_open_and_names_the_protocol_and_binary_version(s: &Server) {
    for token in [None, Some(TOKEN), Some("not-the-token")] {
        let r = s.http("GET", "/health", token, None);
        assert_eq!(r.code, 200, "{}", r.head);
        assert_eq!(r.body["status"], "ok");
        assert_eq!(r.body["protocol"], 1);
        assert_eq!(r.body["version"], env!("CARGO_PKG_VERSION"));
    }
    assert_eq!(s.http("GET", "/ready", None, None).code, 200);
}

fn every_route_but_health_wants_the_token(s: &Server) {
    for (method, path) in [
        ("GET", "/sessions"),
        ("POST", "/sessions"),
        ("GET", "/models"),
        ("GET", "/approvals"),
        ("GET", "/sessions/x/events"),
        ("GET", "/sessions/x/entries"),
        ("POST", "/sessions/x/images"),
    ] {
        // One failure per route stays under the limiter's ten a minute.
        let r = s.http(method, path, None, None);
        assert_eq!(r.code, 401, "{method} {path}: {}", r.head);
        assert_eq!(r.body["error"], "unauthorized");
    }
    let r = s.http("GET", "/sessions", Some("wrong"), None);
    assert_eq!(r.code, 401, "{}", r.head);
    assert_eq!(s.get("/sessions").code, 200);
}

fn a_session_opens_lists_and_reads_back(s: &Server) {
    let open = s.http("POST", "/sessions", Some(TOKEN), Some("{}"));
    assert_eq!(open.code, 200, "{}", open.body);
    let id = open.body["sessionId"].as_str().unwrap().to_string();
    let list = s.get("/sessions");
    assert_eq!(list.code, 200);
    assert!(list.body.to_string().contains(&id), "{}", list.body);
    assert_eq!(s.get(&format!("/sessions/{id}")).code, 200);
    let entries = s.get(&format!("/sessions/{id}/entries?after=0&limit=10"));
    assert_eq!(entries.code, 200, "{}", entries.body);
    assert_eq!(entries.body["id"], id.as_str());
    assert_eq!(s.get("/models").code, 200);
    assert_eq!(s.get("/approvals").code, 200);
}

fn unknown_things_are_404_and_bad_framing_is_400(s: &Server) {
    assert_eq!(s.get("/no-such-route").code, 404);
    assert_eq!(s.get("/sessions/nope").code, 404);
    assert_eq!(s.get("/sessions/nope/entries").code, 404);
    assert_eq!(s.get("/sessions/nope/events").code, 404);
    let r = s.http("POST", "/hooks/none", None, Some("{}"));
    assert_eq!(r.code, 404, "{}", r.head);
    let chunked = s.raw(&format!(
        "POST /sessions HTTP/1.1\r\nAuthorization: Bearer {TOKEN}\r\n\
         Transfer-Encoding: chunked\r\nConnection: close\r\n\r\n0\r\n\r\n"
    ));
    assert_eq!(Reply::parse(&chunked).code, 400, "{chunked}");
    let big = s.raw(&format!(
        "POST /sessions HTTP/1.1\r\nAuthorization: Bearer {TOKEN}\r\n\
         Content-Length: 99999999\r\nConnection: close\r\n\r\n"
    ));
    assert_eq!(Reply::parse(&big).code, 413, "{big}");
}

fn an_image_upload_answers_the_same_refusals(s: &Server) {
    let open = s.http("POST", "/sessions", Some(TOKEN), Some("{}"));
    let id = open.body["sessionId"].as_str().unwrap().to_string();
    let r = s.http(
        "POST",
        &format!("/sessions/{id}/images"),
        Some(TOKEN),
        Some("this is not an image"),
    );
    assert_eq!(r.code, 415, "{}", r.body);
    let r = s.http("POST", "/sessions/nope/images", Some(TOKEN), Some("x"));
    assert_eq!(r.code, 404, "{}", r.body);
}

fn the_event_stream_replays_and_is_ready(s: &Server) {
    let open = s.http("POST", "/sessions", Some(TOKEN), Some("{}"));
    let id = open.body["sessionId"].as_str().unwrap().to_string();
    let text = s.stream_until(&format!("/sessions/{id}/events?after=0"), "event: ready");
    assert!(text.starts_with("HTTP/1.1 200 OK"), "{text}");
    assert!(text.contains("Content-Type: text/event-stream"), "{text}");
    assert!(text.contains("event: ready"), "{text}");
}

fn repeated_wrong_tokens_are_rate_limited(s: &Server) {
    let mut last = 0;
    for _ in 0..12 {
        last = s.http("GET", "/sessions", Some("wrong"), None).code;
    }
    assert_eq!(last, 429);
    // The limiter keys the peer: even the right token waits out the window.
    assert_eq!(s.get("/sessions").code, 429);
    // The probes: on the socket every peer shares the bucket, so `/health` and `/ready` stay
    // reachable (an attaching client must see a running serve); on TCP they are limited as before.
    let open = matches!(s.transport, Transport::Socket);
    for path in ["/health", "/ready"] {
        let code = s.http("GET", path, None, None).code;
        assert_eq!(code, if open { 200 } else { 429 }, "{path}");
    }
}

macro_rules! on_both {
    ($($name:ident),* $(,)?) => {
        mod tcp {
            use super::*;
            $( #[test] fn $name() { super::$name(&Server::spawn(Transport::Tcp)); } )*
        }
        mod socket {
            use super::*;
            $( #[test] fn $name() { super::$name(&Server::spawn(Transport::Socket)); } )*
        }
    };
}

on_both!(
    health_is_open_and_names_the_protocol_and_binary_version,
    every_route_but_health_wants_the_token,
    a_session_opens_lists_and_reads_back,
    unknown_things_are_404_and_bad_framing_is_400,
    an_image_upload_answers_the_same_refusals,
    the_event_stream_replays_and_is_ready,
    repeated_wrong_tokens_are_rate_limited,
);

// ---- What only the socket has ------------------------------------------------

#[test]
fn the_served_socket_and_its_directory_are_owner_only() {
    let s = Server::spawn(Transport::Socket);
    let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(s.socket.parent().unwrap()), 0o700);
    assert_eq!(mode(&s.socket), 0o600);
}

fn refused(dir: &TestDir, args: &[&std::ffi::OsStr]) -> String {
    let out = serve_command(dir)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!out.status.success(), "serve must refuse to start");
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[test]
fn serve_refuses_a_wide_directory_a_link_and_a_foreign_file_before_listening() {
    let dir = TestDir::in_system_temp("tr");
    std::fs::create_dir_all(dir.join("ws")).unwrap();
    std::fs::create_dir_all(dir.join("home")).unwrap();
    let os = std::ffi::OsStr::new;

    // Group and other access on the directory.
    for mode in [0o770, 0o755, 0o707] {
        let wide = dir.join(format!("wide{mode:o}"));
        std::fs::create_dir(&wide).unwrap();
        std::fs::set_permissions(&wide, std::fs::Permissions::from_mode(mode)).unwrap();
        let sock = wide.join("s.sock");
        let err = refused(
            &dir,
            &[os("--socket"), sock.as_os_str(), os("--token"), os("t")],
        );
        assert!(err.contains("must be 0700"), "{mode:o}: {err}");
        assert!(!sock.exists());
    }

    // A directory that is a link, and a socket path that is a link.
    let real = dir.join("real");
    std::fs::create_dir(&real).unwrap();
    std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o700)).unwrap();
    let linked_dir = dir.join("linkdir");
    std::os::unix::fs::symlink(&real, &linked_dir).unwrap();
    let sock = linked_dir.join("s.sock");
    let err = refused(
        &dir,
        &[os("--socket"), sock.as_os_str(), os("--token"), os("t")],
    );
    assert!(err.contains("symbolic link"), "{err}");

    let target = dir.join("target");
    std::fs::write(&target, "keep").unwrap();
    let linked_sock = real.join("s.sock");
    std::os::unix::fs::symlink(&target, &linked_sock).unwrap();
    let err = refused(
        &dir,
        &[
            os("--socket"),
            linked_sock.as_os_str(),
            os("--token"),
            os("t"),
        ],
    );
    assert!(err.contains("symbolic link"), "{err}");
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "keep");

    // A directory of another user (root's `/`): only a non-root user can show it.
    if unsafe { libc::geteuid() } != 0 {
        let err = refused(
            &dir,
            &[
                os("--socket"),
                os("/tole-1043.sock"),
                os("--token"),
                os("t"),
            ],
        );
        assert!(err.contains("owned by another user"), "{err}");
    }
}

#[test]
fn a_token_file_with_group_or_other_access_is_refused_with_a_socket() {
    let dir = TestDir::in_system_temp("tf");
    std::fs::create_dir_all(dir.join("ws")).unwrap();
    std::fs::create_dir_all(dir.join("home")).unwrap();
    let tf = dir.join("token");
    std::fs::write(&tf, format!("{TOKEN}\n")).unwrap();
    std::fs::set_permissions(&tf, std::fs::Permissions::from_mode(0o644)).unwrap();
    let sock = dir.join("run/s.sock");
    let os = std::ffi::OsStr::new;
    let err = refused(
        &dir,
        &[
            os("--socket"),
            sock.as_os_str(),
            os("--token-file"),
            tf.as_os_str(),
        ],
    );
    assert!(err.contains("must be 0600"), "{err}");
    assert!(!err.contains(TOKEN));
}

#[test]
fn socket_flags_that_make_no_sense_together_are_refused() {
    let dir = TestDir::in_system_temp("tc");
    std::fs::create_dir_all(dir.join("ws")).unwrap();
    std::fs::create_dir_all(dir.join("home")).unwrap();
    let sock = dir.join("run/s.sock");
    let os = std::ffi::OsStr::new;
    let err = refused(
        &dir,
        &[
            os("--socket"),
            sock.as_os_str(),
            os("--port"),
            os("7999"),
            os("--token"),
            os("t"),
        ],
    );
    assert!(err.contains("cannot be used with"), "{err}");
    let err = refused(
        &dir,
        &[
            os("--socket"),
            sock.as_os_str(),
            os("--bind"),
            os("0.0.0.0"),
            os("--token"),
            os("t"),
        ],
    );
    assert!(err.contains("cannot be used with"), "{err}");
    let err = refused(
        &dir,
        &[
            os("--socket"),
            sock.as_os_str(),
            os("--transport"),
            os("mcp"),
            os("--token"),
            os("t"),
        ],
    );
    assert!(err.contains("REST transport only"), "{err}");
    assert!(!sock.exists());
}

#[test]
fn a_stale_socket_is_replaced_and_a_live_one_is_not_taken_over() {
    let dir = TestDir::in_system_temp("tl");
    std::fs::create_dir_all(dir.join("ws")).unwrap();
    std::fs::create_dir_all(dir.join("home")).unwrap();
    let run = dir.join("run");
    std::fs::create_dir(&run).unwrap();
    std::fs::set_permissions(&run, std::fs::Permissions::from_mode(0o700)).unwrap();
    let sock = run.join("s.sock");
    // What a killed serve leaves behind: a socket file nobody listens on.
    drop(std::os::unix::net::UnixListener::bind(&sock).unwrap());
    assert!(sock.exists());

    let os = std::ffi::OsStr::new;
    let (mut child, _) = common::spawn_listening(|_| {
        let mut cmd = serve_command(&dir);
        cmd.arg("--socket")
            .arg(&sock)
            .args(["--token", "t"])
            .stdin(Stdio::null())
            .stdout(Stdio::null());
        cmd
    });
    // The first serve answers; a second one on the same path refuses and says who is there.
    let err = refused(
        &dir,
        &[os("--socket"), sock.as_os_str(), os("--token"), os("t")],
    );
    assert!(err.contains("already runs on it"), "{err}");
    assert!(
        err.contains(&format!("tole {}", env!("CARGO_PKG_VERSION"))),
        "{err}"
    );
    let mut s = UnixStream::connect(&sock).unwrap();
    s.write_all(b"GET /health HTTP/1.1\r\nConnection: close\r\n\r\n")
        .unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).unwrap();
    assert!(
        out.contains("200 OK"),
        "the live serve was not disturbed: {out}"
    );
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(target_os = "linux")]
#[test]
fn the_per_launch_token_comes_from_a_descriptor_and_is_not_in_the_process_list_or_environment() {
    use std::os::fd::FromRawFd;
    let dir = TestDir::in_system_temp("tk");
    std::fs::create_dir_all(dir.join("ws")).unwrap();
    std::fs::create_dir_all(dir.join("home")).unwrap();
    let sock = dir.join("run/s.sock");
    let (mut child, _) = common::spawn_listening(|_| {
        // The desktop's way: a fresh pipe, the token written into it and the
        // write end closed, the read end inherited as the child's stdin.
        let mut fds = [0i32; 2];
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        let mut w = unsafe { std::fs::File::from_raw_fd(fds[1]) };
        w.write_all(TOKEN.as_bytes()).unwrap();
        drop(w);
        let r = unsafe { std::fs::File::from_raw_fd(fds[0]) };
        let mut cmd = serve_command(&dir);
        cmd.arg("--socket")
            .arg(&sock)
            .args(["--token-fd", "0"])
            .stdin(Stdio::from(r))
            .stdout(Stdio::null());
        cmd
    });
    let pid = child.id();
    let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).unwrap();
    assert!(
        !String::from_utf8_lossy(&cmdline).contains(TOKEN),
        "the token is in the process cmdline"
    );
    // serve marks itself non-dumpable (`process_guard`), so for a user that is
    // not root the kernel makes `/proc/<pid>/environ` unreadable: that is the
    // stronger property. Root (and a build without the guard) can read it, and
    // then it must not hold the token either.
    match std::fs::read(format!("/proc/{pid}/environ")) {
        Ok(environ) => assert!(
            !String::from_utf8_lossy(&environ).contains(TOKEN),
            "the token is in the process environ"
        ),
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {}
        Err(e) => panic!("reading the process environ failed unexpectedly: {e}"),
    }
    let call = |token: &str| {
        let mut s = UnixStream::connect(&sock).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        s.write_all(
            format!(
                "GET /sessions HTTP/1.1\r\nAuthorization: Bearer {token}\r\nConnection: close\r\n\r\n"
            )
            .as_bytes(),
        )
        .unwrap();
        let mut out = String::new();
        let _ = s.read_to_string(&mut out);
        Reply::parse(&out).code
    };
    assert_eq!(call(TOKEN), 200);
    assert_eq!(call("not-the-token"), 401);
    let _ = child.kill();
    let _ = child.wait();
}

#[test]
fn a_blank_or_missing_descriptor_stops_serve_without_echoing_anything() {
    let dir = TestDir::in_system_temp("tb");
    std::fs::create_dir_all(dir.join("ws")).unwrap();
    std::fs::create_dir_all(dir.join("home")).unwrap();
    let sock = dir.join("run/s.sock");
    let os = std::ffi::OsStr::new;
    let err = refused(
        &dir,
        &[
            os("--socket"),
            sock.as_os_str(),
            os("--token-fd"),
            os("987"),
        ],
    );
    assert!(err.contains("not an open file descriptor"), "{err}");
    let err = refused(
        &dir,
        &[
            os("--socket"),
            sock.as_os_str(),
            os("--token-fd"),
            os("0"),
            os("--token"),
            os("t"),
        ],
    );
    assert!(err.contains("cannot be used with"), "{err}");
    assert!(!sock.exists());
}
