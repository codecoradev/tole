//! The socket under a TLS session, with deadlines instead of timeouts.

use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

/// The TCP socket under the TLS session, with deadlines instead of timeouts.
///
/// A socket timeout bounds ONE socket read. A single `read` of the TLS stream
/// can make many (rustls loops over the socket until it holds one whole
/// record, up to 16 KiB), so a peer that sends a byte just inside the timeout
/// could hold one `read` for the timeout times the record size. Here a timeout
/// set by the caller becomes an absolute point in time, and EVERY socket read
/// or write below it is given what is left, failing with `TimedOut` once it is
/// gone: the bound holds for the call, not for each of its pieces.
pub(crate) struct Bounded {
    pub(crate) sock: TcpStream,
    pub(crate) read_until: Option<Instant>,
    pub(crate) write_until: Option<Instant>,
}

/// What is left until `at`, for a socket timeout; `TimedOut` once it is spent.
fn left_until(at: Instant) -> io::Result<Duration> {
    let left = at.saturating_duration_since(Instant::now());
    if left.is_zero() {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "the deadline has passed",
        ));
    }
    Ok(left)
}

impl Bounded {
    pub(crate) fn new(sock: TcpStream) -> Self {
        Self {
            sock,
            read_until: None,
            write_until: None,
        }
    }
}

impl Read for Bounded {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if let Some(at) = self.read_until {
            self.sock.set_read_timeout(Some(left_until(at)?))?;
        }
        self.sock.read(buf)
    }
}

impl Write for Bounded {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if let Some(at) = self.write_until {
            self.sock.set_write_timeout(Some(left_until(at)?))?;
        }
        self.sock.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.sock.flush()
    }
}

/// `now + timeout`; `None` for no bound (also when the sum overflows).
pub(crate) fn deadline(timeout: Option<Duration>) -> Option<Instant> {
    timeout.and_then(|t| Instant::now().checked_add(t))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_write_after_its_deadline_is_refused_and_one_without_is_not() {
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let sock = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (_peer, _) = listener.accept().unwrap();
        let mut b = Bounded::new(sock);
        assert_eq!(b.write(b"x").unwrap(), 1);
        b.write_until = Some(Instant::now() - Duration::from_secs(1));
        let err = b.write(b"x").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        // The same for reads: nothing is read once the deadline has passed.
        b.read_until = Some(Instant::now() - Duration::from_secs(1));
        assert_eq!(
            b.read(&mut [0u8; 1]).unwrap_err().kind(),
            io::ErrorKind::TimedOut
        );
        b.write_until = None;
        assert_eq!(b.write(b"x").unwrap(), 1);
    }
}
