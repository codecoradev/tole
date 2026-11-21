//! A minimal HTTP/1.1 client over a [`Connection`]: one request, then the
//! body as a stream. Understands `Content-Length`, `chunked` (a reverse proxy
//! in front of serve chunks the event stream) and read-until-close (what serve
//! itself sends for SSE).

use crate::error::{Error, Result};
use crate::transport::Connection;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::time::{Duration, Instant};

const MAX_LINE: u64 = 16 * 1024;
const MAX_HEADERS: usize = 100;
/// Most interim (1xx) responses skipped before the real one.
const MAX_INTERIM: usize = 10;
/// Most bytes of chunked trailers read (they are never used).
const MAX_TRAILER_BYTES: usize = 64 * 1024;

/// The response body as it arrives. Reading it never waits for the end of the
/// body: a read returns what the peer has sent so far.
pub struct ResponseBody {
    inner: BufReader<Guard>,
    mode: Mode,
    /// The `Content-Type` header of the response, as sent.
    content_type: Option<String>,
    /// Once a read has failed the body is finished: the framing readers drop
    /// the bytes they had already consumed (half a chunk-size line, one byte
    /// of a CRLF), so a retry of the read would desynchronize. Every later
    /// read fails with the same kind.
    failed: Option<io::ErrorKind>,
}

/// The connection as the reader sees it. Before EVERY socket read it sets the
/// read timeout to what is left of the deadline (or the per-read bound when
/// that is shorter) and fails with `TimedOut` once the deadline has passed (a
/// timeout that expires inside the socket read, which Unix reports as
/// `WouldBlock`, is turned into the same `TimedOut`).
/// A per-read timeout alone lets a server that sends one byte at a time hold
/// a call forever, and one `read` of the body can make many socket reads
/// (status and header lines, chunk-size lines, trailers), so the bound has to
/// sit under all of them.
struct Guard {
    conn: Box<dyn Connection>,
    per_read: Option<Duration>,
    /// How long the response may take once its first byte is in. Until then
    /// only `per_read` applies: a prompt legitimately waits for its turn
    /// before the server answers.
    span: Option<Duration>,
    deadline: Option<Instant>,
}

impl Read for Guard {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let timeout = match self.deadline {
            Some(at) => {
                let left = at.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "the response did not arrive in time",
                    ));
                }
                Some(self.per_read.map_or(left, |p| p.min(left)))
            }
            None => self.per_read,
        };
        self.conn.set_read_timeout(timeout)?;
        let n = match self.conn.read(buf) {
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "a read of the response timed out",
                ))
            }
            r => r?,
        };
        if n > 0 && self.deadline.is_none() {
            // `None` when the sum overflows: an effectively unlimited span.
            self.deadline = self.span.and_then(|s| Instant::now().checked_add(s));
        }
        Ok(n)
    }
}

enum Mode {
    Length(u64),
    Chunked {
        left: u64,
        /// The CRLF after a chunk's data is still to be read (it is read on the
        /// next call, so the data is delivered first).
        need_crlf: bool,
        done: bool,
    },
    UntilClose,
}

/// A request line plus headers and body, sent in one write.
pub(crate) struct Request<'a> {
    pub host: &'a str,
    pub method: &'a str,
    pub path: &'a str,
    pub token: Option<&'a str>,
    pub body: Option<&'a [u8]>,
    /// The `Content-Type` of `body`; sent only when there is one.
    pub content_type: &'a str,
    pub accept: &'a str,
    /// Bound of one socket read.
    pub read_timeout: Option<Duration>,
    /// Bound of the whole response (head and body), counted from its first
    /// byte. `None` = unbounded.
    pub deadline: Option<Duration>,
}

pub(crate) fn send(mut conn: Box<dyn Connection>, req: &Request) -> Result<(u16, ResponseBody)> {
    for (what, bound) in [
        ("read timeout", req.read_timeout),
        ("deadline", req.deadline),
    ] {
        if bound == Some(Duration::ZERO) {
            return Err(Error::InvalidConfig(format!("the {what} cannot be zero")));
        }
    }
    if let Some(t) = req.token {
        if t.is_empty() || t.contains(['\r', '\n']) {
            return Err(Error::Protocol(
                "the token is not a valid header value".into(),
            ));
        }
    }
    let mut head = format!(
        "{} {} HTTP/1.1\r\nHost: {}\r\nAccept: {}\r\nConnection: close\r\n",
        req.method, req.path, req.host, req.accept
    );
    if let Some(t) = req.token {
        head.push_str(&format!("Authorization: Bearer {t}\r\n"));
    }
    if let Some(b) = req.body {
        head.push_str(&format!(
            "Content-Type: {}\r\nContent-Length: {}\r\n",
            req.content_type,
            b.len()
        ));
    }
    head.push_str("\r\n");
    let mut wire = head.into_bytes();
    wire.extend_from_slice(req.body.unwrap_or_default());
    // The write is bounded as a whole by the response deadline (or the read
    // bound when there is none): a peer that stops reading must not block it.
    write_bounded(&mut *conn, &wire, req.deadline.or(req.read_timeout))?;
    conn.flush()?;

    let mut inner = BufReader::new(Guard {
        conn,
        per_read: req.read_timeout,
        span: req.deadline,
        deadline: None,
    });
    for _ in 0..MAX_INTERIM {
        let (status, length, chunked, content_type) = read_head(&mut inner)?;
        // An interim response (100 Continue, 103 Early Hints) is followed by
        // the real one. 101 is final.
        if (100..200).contains(&status) && status != 101 {
            continue;
        }
        // These have no body whatever their headers say (RFC 9112).
        let bodyless = req.method.eq_ignore_ascii_case("HEAD") || matches!(status, 101 | 204 | 304);
        let mode = if bodyless {
            Mode::Length(0)
        } else if chunked {
            Mode::Chunked {
                left: 0,
                need_crlf: false,
                done: false,
            }
        } else if let Some(n) = length {
            Mode::Length(n)
        } else {
            Mode::UntilClose
        };
        return Ok((
            status,
            ResponseBody {
                inner,
                mode,
                content_type,
                failed: None,
            },
        ));
    }
    Err(Error::Protocol("too many interim responses".into()))
}

/// Write all of `data` within `bound` in total (`None` = unbounded). A write
/// timeout that Unix reports as `WouldBlock` is turned into `TimedOut`.
fn write_bounded(
    conn: &mut dyn Connection,
    mut data: &[u8],
    bound: Option<Duration>,
) -> io::Result<()> {
    let end = bound.and_then(|b| Instant::now().checked_add(b));
    while !data.is_empty() {
        if let Some(end) = end {
            let left = end.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "the request could not be written in time",
                ));
            }
            conn.set_write_timeout(Some(left))?;
        }
        match conn.write(data) {
            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(n) => data = &data[n..],
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "the request could not be written in time",
                ))
            }
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// The status line and headers of one response: the status, the
/// `Content-Length` if any, whether the body is chunked, and the
/// `Content-Type` if any.
fn read_head(inner: &mut impl BufRead) -> Result<(u16, Option<u64>, bool, Option<String>)> {
    let status_line = read_line(inner)?;
    let status = status_line
        .strip_prefix("HTTP/1.")
        .and_then(|r| r.split_whitespace().nth(1))
        .and_then(|c| c.parse::<u16>().ok())
        .ok_or_else(|| Error::Protocol("bad status line".into()))?;
    let (mut length, mut chunked, mut content_type) = (None, false, None);
    for _ in 0..=MAX_HEADERS {
        let line = read_line(inner)?;
        if line.is_empty() {
            return Ok((status, length, chunked, content_type));
        }
        if let Some((name, value)) = line.split_once(':') {
            let value = value.trim();
            if name.eq_ignore_ascii_case("content-length") {
                length = Some(
                    value
                        .parse::<u64>()
                        .map_err(|_| Error::Protocol("bad content-length".into()))?,
                );
            } else if name.eq_ignore_ascii_case("content-type") {
                content_type = Some(value.to_string());
            } else if name.eq_ignore_ascii_case("transfer-encoding")
                && value.to_ascii_lowercase().contains("chunked")
            {
                chunked = true;
            }
        }
    }
    Err(Error::Protocol("too many response headers".into()))
}

/// One CRLF-terminated line, without the terminator, capped at [`MAX_LINE`].
fn read_line(r: &mut impl BufRead) -> io::Result<String> {
    let mut line = Vec::new();
    let n = Read::by_ref(r)
        .take(MAX_LINE)
        .read_until(b'\n', &mut line)?;
    if n == 0 {
        return Err(io::ErrorKind::UnexpectedEof.into());
    }
    if line.pop() != Some(b'\n') {
        // Read up to the cap without a newline: a line that is too long; less
        // than the cap: the stream ended in the middle of a line.
        return Err(if n as u64 >= MAX_LINE {
            io::Error::new(io::ErrorKind::InvalidData, "a line is too long")
        } else {
            io::ErrorKind::UnexpectedEof.into()
        });
    }
    if line.last() == Some(&b'\r') {
        line.pop();
    }
    Ok(String::from_utf8_lossy(&line).into_owned())
}

/// How much of `buf` to read for a body with `left` bytes to go. Checked, so a
/// 4 GiB length cannot wrap to zero on a 32-bit target.
fn window(buf_len: usize, left: u64) -> usize {
    buf_len.min(usize::try_from(left).unwrap_or(usize::MAX))
}

impl ResponseBody {
    /// The `Content-Type` header the server sent, if any (the recorded type of
    /// an image, for [`Client::image`](crate::Client::image)).
    pub fn content_type(&self) -> Option<&str> {
        self.content_type.as_deref()
    }

    /// Lift the response deadline: the body is a stream that may legitimately
    /// last (the event stream). Only the per-read bound applies from now on.
    pub(crate) fn unlimit(&mut self) {
        let g = self.inner.get_mut();
        g.span = None;
        g.deadline = None;
    }
}

impl Read for ResponseBody {
    /// A failed read is final: every later read fails with the same kind.
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if let Some(kind) = self.failed {
            return Err(io::Error::new(
                kind,
                "the response cannot be read any more after an earlier error",
            ));
        }
        let r = self.read_body(buf);
        if let Err(e) = &r {
            if e.kind() != io::ErrorKind::Interrupted {
                self.failed = Some(e.kind());
            }
        }
        r
    }
}

impl ResponseBody {
    fn read_body(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let Self { inner, mode, .. } = self;
        if buf.is_empty() {
            return Ok(0);
        }
        match mode {
            Mode::UntilClose => inner.read(buf),
            Mode::Length(left) => {
                if *left == 0 {
                    return Ok(0);
                }
                let max = window(buf.len(), *left);
                let n = inner.read(&mut buf[..max])?;
                if n == 0 {
                    return Err(io::ErrorKind::UnexpectedEof.into());
                }
                *left -= n as u64;
                Ok(n)
            }
            Mode::Chunked {
                left,
                need_crlf,
                done,
            } => {
                if *done {
                    return Ok(0);
                }
                if *need_crlf {
                    let mut crlf = [0u8; 2];
                    inner.read_exact(&mut crlf)?;
                    if &crlf != b"\r\n" {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "a chunk is not followed by CRLF",
                        ));
                    }
                    *need_crlf = false;
                }
                if *left == 0 {
                    let line = read_line(inner)?;
                    let size = line.split(';').next().unwrap_or("").trim();
                    *left = u64::from_str_radix(size, 16).map_err(|_| {
                        io::Error::new(io::ErrorKind::InvalidData, "bad chunk size")
                    })?;
                    if *left == 0 {
                        let (mut lines, mut bytes) = (0, 0);
                        loop {
                            let line = read_line(inner)?;
                            if line.is_empty() {
                                break;
                            }
                            (lines, bytes) = (lines + 1, bytes + line.len() + 2);
                            if lines > MAX_HEADERS || bytes > MAX_TRAILER_BYTES {
                                return Err(io::Error::new(
                                    io::ErrorKind::InvalidData,
                                    "too many chunked trailers",
                                ));
                            }
                        }
                        *done = true;
                        return Ok(0);
                    }
                }
                let max = window(buf.len(), *left);
                let n = inner.read(&mut buf[..max])?;
                if n == 0 {
                    return Err(io::ErrorKind::UnexpectedEof.into());
                }
                *left -= n as u64;
                *need_crlf = *left == 0;
                Ok(n)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    struct Canned(Cursor<Vec<u8>>);
    impl Read for Canned {
        fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
            self.0.read(b)
        }
    }
    impl Write for Canned {
        fn write(&mut self, b: &[u8]) -> io::Result<usize> {
            Ok(b.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    impl Connection for Canned {
        fn set_read_timeout(&mut self, _: Option<Duration>) -> io::Result<()> {
            Ok(())
        }
        fn set_write_timeout(&mut self, _: Option<Duration>) -> io::Result<()> {
            Ok(())
        }
    }

    /// A connection that replays scripted read results, then ends.
    struct Script(std::collections::VecDeque<io::Result<Vec<u8>>>);
    impl Read for Script {
        fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
            match self.0.pop_front() {
                Some(Ok(d)) => {
                    b[..d.len()].copy_from_slice(&d);
                    Ok(d.len())
                }
                Some(Err(e)) => Err(e),
                None => Ok(0),
            }
        }
    }
    impl Write for Script {
        fn write(&mut self, b: &[u8]) -> io::Result<usize> {
            Ok(b.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    impl Connection for Script {
        fn set_read_timeout(&mut self, _: Option<Duration>) -> io::Result<()> {
            Ok(())
        }
        fn set_write_timeout(&mut self, _: Option<Duration>) -> io::Result<()> {
            Ok(())
        }
    }

    fn request<'a>(method: &'a str) -> Request<'a> {
        Request {
            host: "h",
            method,
            path: "/",
            token: None,
            body: None,
            content_type: "application/json",
            accept: "*/*",
            read_timeout: None,
            deadline: None,
        }
    }

    fn get(wire: &str) -> Result<(u16, String)> {
        let req = Request {
            host: "h",
            method: "GET",
            path: "/",
            token: None,
            body: None,
            content_type: "application/json",
            accept: "*/*",
            read_timeout: None,
            deadline: None,
        };
        let (status, mut body) = send(
            Box::new(Canned(Cursor::new(wire.as_bytes().to_vec()))),
            &req,
        )?;
        let mut out = String::new();
        body.read_to_string(&mut out)?;
        Ok((status, out))
    }

    #[test]
    fn the_three_body_framings() {
        let (s, b) = get("HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello").unwrap();
        assert_eq!((s, b.as_str()), (200, "hello"));
        let (_, b) = get("HTTP/1.1 200 OK\r\n\r\nuntil close").unwrap();
        assert_eq!(b, "until close");
        let (_, b) = get(
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n4;x=1\r\ndefg\r\n0\r\n\r\n",
        )
        .unwrap();
        assert_eq!(b, "abcdefg");
    }

    #[test]
    fn a_cut_short_body_is_an_error_not_a_clean_end() {
        assert!(matches!(
            get("HTTP/1.1 200 OK\r\nContent-Length: 9\r\n\r\nshort"),
            Err(Error::Io(_))
        ));
        assert!(matches!(
            get("HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nab"),
            Err(Error::Io(_))
        ));
        assert!(matches!(get("garbage\r\n\r\n"), Err(Error::Protocol(_))));
    }

    fn body_of(wire: &str) -> ResponseBody {
        let req = Request {
            host: "h",
            method: "GET",
            path: "/",
            token: None,
            body: None,
            content_type: "application/json",
            accept: "*/*",
            read_timeout: None,
            deadline: None,
        };
        send(
            Box::new(Canned(Cursor::new(wire.as_bytes().to_vec()))),
            &req,
        )
        .unwrap()
        .1
    }

    #[test]
    fn chunk_data_is_delivered_before_its_crlf_is_read() {
        // The data of a chunk, then silence: the data must not be lost to an
        // error on the CRLF that follows it.
        let mut body = body_of("HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc");
        let mut buf = [0u8; 16];
        assert_eq!(body.read(&mut buf).unwrap(), 3);
        assert_eq!(&buf[..3], b"abc");
        assert!(body.read(&mut buf).is_err(), "the missing CRLF is an error");
    }

    #[test]
    fn a_chunk_must_be_followed_by_exactly_crlf() {
        let mut body =
            body_of("HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabcXX0\r\n\r\n");
        let mut out = Vec::new();
        let err = body.read_to_end(&mut out).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData, "{err}");
    }

    #[test]
    fn chunked_trailers_are_capped() {
        let trailers = "X-T: 1\r\n".repeat(MAX_HEADERS + 1);
        let wire =
            format!("HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n{trailers}\r\n");
        let err = body_of(&wire).read_to_end(&mut Vec::new()).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData, "{err}");
        // A few trailers are fine.
        let ok = "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n0\r\nX-T: 1\r\n\r\n";
        assert!(body_of(ok).read_to_end(&mut Vec::new()).is_ok());
    }

    #[test]
    fn the_guard_fails_every_read_once_the_deadline_has_passed() {
        let conn = Canned(Cursor::new(b"0123456789".to_vec()));
        // A zero span: the deadline is the moment the first byte arrives.
        let mut g = Guard {
            conn: Box::new(conn),
            per_read: None,
            span: Some(Duration::ZERO),
            deadline: None,
        };
        let mut buf = [0u8; 4];
        assert_eq!(
            g.read(&mut buf).unwrap(),
            4,
            "the first byte starts the clock"
        );
        let err = g.read(&mut buf).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        // No span: never bounded by a deadline.
        let mut g = Guard {
            conn: Box::new(Canned(Cursor::new(b"0123456789".to_vec()))),
            per_read: None,
            span: None,
            deadline: None,
        };
        assert!(g.read(&mut buf).is_ok() && g.read(&mut buf).is_ok());
    }

    fn head_and_body(method: &str, wire: &str) -> Result<(u16, String)> {
        let req = request(method);
        let (status, mut body) = send(
            Box::new(Canned(Cursor::new(wire.as_bytes().to_vec()))),
            &req,
        )?;
        let mut out = String::new();
        body.read_to_string(&mut out)?;
        Ok((status, out))
    }

    #[test]
    fn bodyless_responses_are_framed_without_a_body() {
        // 204 and 304 carry no body whatever Content-Length says, and neither
        // does the answer to a HEAD: no wait for bytes that never come.
        for (method, wire) in [
            (
                "GET",
                "HTTP/1.1 204 No Content\r\nContent-Length: 5\r\n\r\n",
            ),
            (
                "GET",
                "HTTP/1.1 304 Not Modified\r\nContent-Length: 5\r\n\r\n",
            ),
            ("HEAD", "HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\n"),
            ("GET", "HTTP/1.1 101 Switching\r\nContent-Length: 5\r\n\r\n"),
        ] {
            let (_, body) = head_and_body(method, wire).unwrap();
            assert_eq!(body, "", "{method} {wire:?}");
        }
    }

    #[test]
    fn interim_responses_are_skipped_to_the_real_one() {
        let wire = "HTTP/1.1 103 Early Hints\r\nLink: x\r\n\r\nHTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok"
            ;
        assert_eq!(head_and_body("GET", wire).unwrap(), (200, "ok".to_string()));
        // An endless run of them is refused.
        let many = "HTTP/1.1 100 Continue\r\n\r\n".repeat(MAX_INTERIM + 1);
        assert!(matches!(
            head_and_body("GET", &many),
            Err(Error::Protocol(_))
        ));
    }

    #[test]
    fn a_timeout_inside_the_socket_read_is_timed_out_and_final() {
        // The chunk-size line arrives in two pieces with a timeout between
        // them (Unix reports it as WouldBlock). The error is TimedOut, and the
        // body does not resume: the half line already consumed is gone.
        let parts = vec![
            Ok(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r".to_vec()),
            Err(io::ErrorKind::WouldBlock.into()),
            Ok(b"\nabc\r\n0\r\n\r\n".to_vec()),
        ];
        let (_, mut body) = send(
            Box::new(Script(parts.into_iter().collect())),
            &request("GET"),
        )
        .unwrap();
        let mut buf = [0u8; 8];
        let first = body.read(&mut buf).unwrap_err();
        assert_eq!(first.kind(), io::ErrorKind::TimedOut, "{first}");
        let again = body.read(&mut buf).unwrap_err();
        assert_eq!(
            again.kind(),
            io::ErrorKind::TimedOut,
            "no resume after a timeout"
        );
        // Through the error type: a timeout is neither a drop nor retryable.
        let err = Error::from(first);
        assert!(matches!(err, Error::Io(_)) && !err.is_transient());
    }

    #[test]
    fn a_failed_body_stays_failed_for_every_error_kind() {
        let wire = "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nzz\r\nabc";
        let (_, mut body) = send(
            Box::new(Canned(Cursor::new(wire.as_bytes().to_vec()))),
            &request("GET"),
        )
        .unwrap();
        let mut buf = [0u8; 8];
        assert_eq!(
            body.read(&mut buf).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        assert_eq!(
            body.read(&mut buf).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        assert!(matches!(
            Error::from(body.read(&mut buf).unwrap_err()),
            Error::Protocol(_)
        ));
    }

    #[test]
    fn a_zero_timeout_or_deadline_is_a_config_error_not_an_io_error() {
        for (read_timeout, deadline) in [(Some(Duration::ZERO), None), (None, Some(Duration::ZERO))]
        {
            let req = Request {
                read_timeout,
                deadline,
                ..request("GET")
            };
            let err = send(Box::new(Canned(Cursor::new(Vec::new()))), &req)
                .err()
                .expect("refused before anything is sent");
            assert!(matches!(err, Error::InvalidConfig(_)), "{err:?}");
            assert!(!err.is_transient());
        }
    }

    #[test]
    fn lines_distinguish_too_long_from_cut_short() {
        let long = format!(
            "HTTP/1.1 200 OK\r\nX: {}",
            "y".repeat(MAX_LINE as usize + 10)
        );
        assert!(matches!(
            head_and_body("GET", &long),
            Err(Error::Protocol(_))
        ));
        let cut = "HTTP/1.1 200 OK\r\nX: y";
        assert!(matches!(
            head_and_body("GET", cut),
            Err(Error::Io(ref e)) if e.kind() == io::ErrorKind::UnexpectedEof
        ));
    }

    #[test]
    fn the_read_window_never_wraps() {
        assert_eq!(window(10, 3), 3);
        assert_eq!(window(10, 0), 0);
        assert_eq!(window(10, u64::MAX), 10);
        // On a 32-bit target 2^32 used to cast to 0.
        assert_eq!(window(10, 0x1_0000_0000), 10);
    }

    /// A connection whose peer stops reading after `room` bytes.
    struct Stuck {
        room: usize,
        timeouts: Vec<Option<Duration>>,
    }
    impl Read for Stuck {
        fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
            Ok(0)
        }
    }
    impl Write for Stuck {
        fn write(&mut self, b: &[u8]) -> io::Result<usize> {
            if self.room == 0 {
                return Err(io::ErrorKind::WouldBlock.into());
            }
            let n = b.len().min(self.room).min(7);
            self.room -= n;
            Ok(n)
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    impl Connection for Stuck {
        fn set_read_timeout(&mut self, _: Option<Duration>) -> io::Result<()> {
            Ok(())
        }
        fn set_write_timeout(&mut self, t: Option<Duration>) -> io::Result<()> {
            self.timeouts.push(t);
            Ok(())
        }
    }

    #[test]
    fn a_write_is_bounded_in_total_and_a_stalled_peer_times_out() {
        // Fits: written in pieces, each with the time that is left.
        let mut c = Stuck {
            room: 100,
            timeouts: Vec::new(),
        };
        write_bounded(&mut c, &[1u8; 50], Some(Duration::from_secs(5))).unwrap();
        assert!(
            c.timeouts.len() > 1
                && c.timeouts
                    .iter()
                    .all(|t| t.is_some_and(|d| d <= Duration::from_secs(5)))
        );
        // The peer stops after 20 bytes: WouldBlock is reported as TimedOut.
        let mut c = Stuck {
            room: 20,
            timeouts: Vec::new(),
        };
        let err = write_bounded(&mut c, &[1u8; 50], Some(Duration::from_secs(5))).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        // Unbounded: no write timeout is touched.
        let mut c = Stuck {
            room: 100,
            timeouts: Vec::new(),
        };
        write_bounded(&mut c, &[1u8; 50], None).unwrap();
        assert!(c.timeouts.is_empty());
    }
}
