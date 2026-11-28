//! #1024: `GET /info` through `tole-client`, against the REAL serve face (the
//! `tole` binary, a scripted provider mock): the typed answer carries all the
//! server sent and nothing else, it agrees with `GET /health` on the version
//! and the protocol, the feature names are a sorted set, and the route is
//! behind the token like every route but `/health`.

// Spawns the real `tole serve` binary (`shell-tools` feature, #349).
#![cfg(feature = "shell-tools")]

use serde_json::Value;
use tole_client::{Client, Error};

mod common;
mod serve_harness;
use serve_harness::{spawn_mock, Serve};

#[test]
fn info_describes_the_server_it_meets() {
    let serve = Serve::spawn(&spawn_mock(0));
    let url = format!("http://127.0.0.1:{}", serve.port);
    let client = |token: &str| Client::connect_tcp(&url, Some(token.into()), false).unwrap();

    let info = client("t0ken").info().unwrap();
    let raw = serve.http("GET", "/info", None).1;
    assert_eq!(serde_json::to_value(&info).unwrap(), raw, "lossless");

    // The same numbers `/health` reports (#1043), so a client may use either.
    let health = client("t0ken").health().unwrap();
    assert_eq!(Some(info.protocol_version), health.protocol);
    assert_eq!(Some(&info.version), health.version.as_ref());
    assert_eq!(info.version, env!("CARGO_PKG_VERSION"));
    assert_eq!(
        info.protocol_version,
        tole_cli::protocol::SERVE_PROTOCOL_VERSION
    );

    // A sorted set of names, and the ones the wire document leads with.
    let mut sorted = info.features.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(info.features, sorted, "sorted, no repeats");
    for name in ["entries", "events.sse", "approvals", "missions", "resume"] {
        assert!(info.has(name), "{name}: {:?}", info.features);
    }
    assert!(!info.has("no.such.feature"));
    // The shared token is `full` (#1320).
    assert_eq!(info.scope.as_deref(), Some("full"));
    // Nothing but the four keys: no path, user name, token name or setting.
    let keys: Vec<&str> = raw
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(keys, ["features", "protocolVersion", "scope", "version"]);

    // Behind the token: a wrong one is a 401, no answer without one.
    assert!(matches!(
        client("bad").info(),
        Err(Error::Status { status: 401, .. })
    ));
    let (code, body) = serve.http_with("GET", "/info", false, None);
    assert_eq!((code, &body["error"]), (401, &Value::from("unauthorized")));
}
