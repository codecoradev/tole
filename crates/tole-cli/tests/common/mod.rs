//! Shared helpers for tests that spawn `tole serve`.
//!
//! Picking a port by binding `127.0.0.1:0` and closing it is a race: before
//! the server binds it, the same ephemeral port can be taken by another
//! test's client socket, the server exits with "Address already in use",
//! and a health poll then times out (or, worse, reaches a different
//! listener). Readiness is therefore taken from the server's own
//! "listening on" line, and a server that exits instead is retried on a
//! fresh port.

#![allow(dead_code)]

use std::io::{BufRead, BufReader};
use std::net::TcpListener;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

const ATTEMPTS: usize = 5;
const READY_TIMEOUT: Duration = Duration::from_secs(20);

fn candidate_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// Spawn the command `make(port)` builds until one instance reports
/// "listening on". Its stderr is forwarded to this test's stderr (so it
/// shows up in a failure report). Returns the running child and its port.
pub fn spawn_listening(mut make: impl FnMut(u16) -> Command) -> (Child, u16) {
    for _ in 0..ATTEMPTS {
        let port = candidate_port();
        let mut cmd = make(port);
        cmd.stderr(Stdio::piped());
        let mut child = cmd.spawn().expect("spawn tole");
        let stderr = child.stderr.take().expect("stderr piped");
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                if line.contains("listening on") {
                    let _ = tx.send(true);
                }
                eprintln!("{line}");
            }
            // EOF: the process exited (or closed stderr) before listening.
            let _ = tx.send(false);
        });
        if rx.recv_timeout(READY_TIMEOUT) == Ok(true) {
            return (child, port);
        }
        let _ = child.kill();
        let _ = child.wait();
    }
    panic!("tole serve did not start listening after {ATTEMPTS} attempts");
}
