//! The committed JSON Schema of the serve protocol (#1068).
//!
//! Test-only: `schemars` is a dev-dependency and the derives are behind
//! `cfg(test)`, so no build of this crate or of a published crate reaches it.
//! `schema_matches_the_committed_file` regenerates the document and fails when
//! `schema/tole-client.schema.json` differs; `scripts/client-schema.sh update`
//! (`TOLE_UPDATE_SCHEMA=1`) rewrites the file.
//!
//! The document is one object with `requests`, `responses` and `events`
//! (every wire type, so a generator that starts from the root reaches all of
//! them), the shared definitions in `$defs`, and `x-tole-routes`, the table of
//! routes with their request and response type. Keys are sorted so the output
//! does not depend on a serde_json feature another crate turns on.

use crate::client::Route;
use crate::sse::payload::{ClosedData, ResyncData, TextData, ToolFinishedData, ToolStartedData};
use crate::sse::Ready;
use crate::types::*;
use schemars::generate::SchemaSettings;
use schemars::{JsonSchema, SchemaGenerator};
use serde_json::{json, Map, Value};
use std::path::PathBuf;

/// `tole_core::entry::Entry`, mirrored because tole-core must not depend on
/// `schemars`. `the_entry_mirror_matches_tole_core` keeps the two in step: the
/// mirror refuses unknown keys, and both must carry the same key set.
#[derive(serde::Serialize, serde::Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(rename = "Entry")]
pub(crate) struct EntrySchema {
    /// Global monotonic sequence number assigned at commit time.
    seq: u64,
    /// Unique entry id (`e_<seq>` when auto-assigned).
    id: String,
    /// Parent entry id; null only for the session root entry.
    #[serde(rename = "parentId", default)]
    parent_id: Option<String>,
    /// What kind of entry this is. A string, not a closed set: a newer server
    /// may write kinds this client does not know (`message`, `intent`,
    /// `tool_result` and `error` today).
    #[serde(rename = "type")]
    kind: String,
    /// Creation time, milliseconds since the Unix epoch.
    timestamp: u64,
    /// Type-specific payload, owned by the entry kind.
    payload: Value,
}

fn sorted(v: Value) -> Value {
    match v {
        Value::Object(m) => {
            let mut e: Vec<_> = m.into_iter().collect();
            e.sort_by(|a, b| a.0.cmp(&b.0));
            Value::Object(e.into_iter().map(|(k, v)| (k, sorted(v))).collect())
        }
        Value::Array(a) => Value::Array(a.into_iter().map(sorted).collect()),
        v => v,
    }
}

/// What `route_types` names for a route whose answer is the event stream
/// (`text/event-stream`) and not one JSON document: the frames are described by
/// the root `events` group, keyed by event name.
const EVENT_STREAM: &str = "events";

/// `(request type, response type, query parameters)` of a route. Exhaustive,
/// so a new `Route` does not compile until it is described here. The query
/// parameter names are listed by hand beside the client methods that send them.
fn route_types(r: Route) -> (Option<&'static str>, &'static str, &'static [&'static str]) {
    match r {
        Route::Health => (None, "Health", &[]),
        Route::Sessions => (None, "SessionList", &[]),
        Route::SessionIndex => (None, "SessionIndex", &["limit", "cursor"]),
        Route::CreateSession => (Some("NewSession"), "SessionOpened", &[]),
        Route::Prompt => (Some("PromptRequest"), "PromptReply", &[]),
        Route::Cancel => (None, "CancelReply", &[]),
        Route::Status => (None, "SessionStatus", &[]),
        Route::Entries => (None, "EntriesPage", &["after", "limit"]),
        Route::Events => (None, EVENT_STREAM, &["after"]),
        Route::Approvals => (None, "ApprovalList", &["session"]),
        Route::Decide => (Some("DecisionRequest"), "DecisionReply", &[]),
        Route::Models => (None, "Models", &[]),
        Route::Discussions => (None, "DiscussionList", &[]),
        Route::Discussion => (None, "Discussion", &[]),
        Route::DiscussionEstimate => (Some("DiscussionRequest"), "DiscussionEstimate", &[]),
        Route::StartDiscussion => (Some("DiscussionRequest"), "DiscussionStarted", &[]),
        Route::ResumeDiscussion => (None, "DiscussionResumed", &[]),
        Route::StopDiscussion => (None, "DiscussionStopped", &[]),
        Route::CronJobs => (None, "CronJobs", &[]),
        Route::PauseCronJob => (None, "CronJobState", &[]),
        Route::ResumeCronJob => (None, "CronJobState", &[]),
        Route::CronRuns => (None, "CronRuns", &["job", "limit"]),
    }
}

/// An object whose properties are the given schemas, all required.
fn group(items: Vec<(&str, schemars::Schema)>) -> Value {
    let props: Map<String, Value> = items
        .into_iter()
        .map(|(k, s)| (k.into(), s.into()))
        .collect();
    let required: Vec<&String> = props.keys().collect();
    json!({"type": "object", "properties": props, "required": required})
}

/// The document, built from the types.
pub(crate) fn generate() -> Value {
    let mut g = SchemaGenerator::new(SchemaSettings::draft2020_12());
    let requests = group(vec![
        ("NewSession", g.subschema_for::<NewSession>()),
        ("PromptRequest", g.subschema_for::<PromptRequest>()),
        ("DecisionRequest", g.subschema_for::<DecisionRequest>()),
        ("DiscussionRequest", g.subschema_for::<DiscussionRequest>()),
    ]);
    let responses = group(vec![
        ("Health", g.subschema_for::<Health>()),
        ("SessionList", g.subschema_for::<SessionList>()),
        ("SessionIndex", g.subschema_for::<SessionIndex>()),
        ("SessionOpened", g.subschema_for::<SessionOpened>()),
        ("PromptReply", g.subschema_for::<PromptReply>()),
        ("CancelReply", g.subschema_for::<CancelReply>()),
        ("SessionStatus", g.subschema_for::<SessionStatus>()),
        ("EntriesPage", g.subschema_for::<EntriesPage>()),
        ("ApprovalList", g.subschema_for::<ApprovalList>()),
        ("DecisionReply", g.subschema_for::<DecisionReply>()),
        ("Models", g.subschema_for::<Models>()),
        ("DiscussionList", g.subschema_for::<DiscussionList>()),
        ("Discussion", g.subschema_for::<Discussion>()),
        (
            "DiscussionEstimate",
            g.subschema_for::<DiscussionEstimate>(),
        ),
        ("DiscussionStarted", g.subschema_for::<DiscussionStarted>()),
        ("DiscussionResumed", g.subschema_for::<DiscussionResumed>()),
        ("DiscussionStopped", g.subschema_for::<DiscussionStopped>()),
        ("CronJobs", g.subschema_for::<CronJobs>()),
        ("CronJobState", g.subschema_for::<CronJobState>()),
        ("CronRuns", g.subschema_for::<CronRuns>()),
    ]);
    // Keyed by the SSE event name; the value is the JSON in the frame's `data:`.
    let events = group(vec![
        ("entry", g.subschema_for::<EntrySchema>()),
        ("ready", g.subschema_for::<Ready>()),
        ("text_delta", g.subschema_for::<TextData>()),
        ("reasoning", g.subschema_for::<TextData>()),
        ("tool_started", g.subschema_for::<ToolStartedData>()),
        ("tool_finished", g.subschema_for::<ToolFinishedData>()),
        ("approval_requested", g.subschema_for::<Approval>()),
        ("approval_resolved", g.subschema_for::<Approval>()),
        ("resync", g.subschema_for::<ResyncData>()),
        ("closed", g.subschema_for::<ClosedData>()),
    ]);
    let routes: Vec<Value> = Route::ALL
        .iter()
        .map(|&r| {
            let (req, res, query) = route_types(r);
            let (response, stream) = if res == EVENT_STREAM {
                (Value::Null, json!(EVENT_STREAM))
            } else {
                (json!(res), Value::Null)
            };
            json!({
                "method": r.method(),
                "path": r.template(),
                "request": req,
                "response": response,
                "stream": stream,
                "query": query,
            })
        })
        .collect();
    let defs = Value::Object(g.take_definitions(true));
    sorted(json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "title": "tole serve client protocol",
        "description": "Generated from the wire and event types of the tole-client crate; do not edit. `requests` and `responses` hold the JSON bodies, `events` the `data:` of each Server-Sent Event of GET /sessions/{id}/events by event name (a frame whose name is not listed is not an error: keep its data as opaque JSON), and `x-tole-routes` which route takes and returns what.",
        "type": "object",
        "properties": {"requests": requests, "responses": responses, "events": events},
        "required": ["requests", "responses", "events"],
        "$defs": defs,
        "x-tole-routes": routes,
    }))
}

fn committed_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("schema/tole-client.schema.json")
}

/// Pretty JSON down to the property level, one line per property below it, so
/// the committed file stays short and a changed field is one changed line.
fn layout(v: &Value, depth: usize, out: &mut String) {
    const INLINE_FROM: usize = 4;
    let indent = |n: usize| "  ".repeat(n);
    match v {
        Value::Object(m) if depth < INLINE_FROM && !m.is_empty() => {
            out.push_str("{\n");
            for (i, (k, v)) in m.iter().enumerate() {
                out.push_str(&indent(depth + 1));
                out.push_str(&serde_json::to_string(k).unwrap());
                out.push_str(": ");
                layout(v, depth + 1, out);
                out.push_str(if i + 1 < m.len() { ",\n" } else { "\n" });
            }
            out.push_str(&indent(depth));
            out.push('}');
        }
        Value::Array(a) if depth < INLINE_FROM && a.iter().any(|v| v.is_object()) => {
            out.push_str("[\n");
            for (i, v) in a.iter().enumerate() {
                out.push_str(&indent(depth + 1));
                layout(v, depth + 1, out);
                out.push_str(if i + 1 < a.len() { ",\n" } else { "\n" });
            }
            out.push_str(&indent(depth));
            out.push(']');
        }
        v => out.push_str(&serde_json::to_string(v).unwrap()),
    }
}

fn render() -> String {
    let mut s = String::new();
    layout(&generate(), 0, &mut s);
    s.push('\n');
    s
}

#[test]
fn schema_matches_the_committed_file() {
    let want = render();
    let path = committed_path();
    if std::env::var_os("TOLE_UPDATE_SCHEMA").is_some() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, &want).unwrap();
        return;
    }
    let have = std::fs::read_to_string(&path).unwrap_or_default();
    if have != want {
        let (n, (h, w)) = have
            .lines()
            .zip(want.lines())
            .enumerate()
            .find(|(_, (h, w))| h != w)
            .unwrap_or((have.lines().count().min(want.lines().count()), ("", "")));
        panic!(
            "{} is out of date with the wire types (first difference at line {}: committed {h:?}, generated {w:?}). \
             Run `scripts/client-schema.sh update` and commit the result.",
            path.display(),
            n + 1
        );
    }
}

#[test]
fn every_route_names_types_that_the_document_defines() {
    let doc = generate();
    let defs = doc["$defs"].as_object().unwrap();
    let routes = doc["x-tole-routes"].as_array().unwrap();
    for r in routes {
        for k in ["request", "response"] {
            if let Some(name) = r[k].as_str() {
                assert!(
                    defs.contains_key(name),
                    "{} {} names unknown {name}",
                    r["method"],
                    r["path"]
                );
            }
        }
    }
    // A stream route points at a group of the root `events` property.
    let streams: Vec<&str> = routes.iter().filter_map(|r| r["stream"].as_str()).collect();
    assert_eq!(streams, [EVENT_STREAM], "exactly the event route streams");
    assert!(doc["properties"][EVENT_STREAM].is_object());
    assert_eq!(routes.len(), Route::ALL.len());
}

#[test]
fn the_entry_mirror_matches_tole_core() {
    let entry = tole_core::entry::Entry {
        seq: 7,
        id: "e_7".into(),
        parent_id: Some("e_6".into()),
        kind: tole_core::entry::EntryType::new(tole_core::entry::EntryType::MESSAGE),
        timestamp: 1_756_200_000_000,
        payload: json!({"role": "user"}),
    };
    let wire = serde_json::to_value(&entry).unwrap();
    let mirror: EntrySchema = serde_json::from_value(wire.clone()).expect("mirror reads an Entry");
    assert_eq!(serde_json::to_value(&mirror).unwrap(), wire);
    // Both sides must carry the same key set: a field added to tole-core's
    // Entry fails the deny_unknown_fields read above, a field only the mirror
    // has fails here.
    let mut keys: Vec<_> = wire.as_object().unwrap().keys().cloned().collect();
    keys.sort();
    let doc = generate();
    let props = doc["$defs"]["Entry"]["properties"].as_object().unwrap();
    let mut schema_keys: Vec<_> = props.keys().cloned().collect();
    schema_keys.sort();
    assert_eq!(keys, schema_keys);
}

#[test]
fn every_event_name_is_one_the_decoder_types() {
    use crate::sse::{Event, Frame};
    let doc = generate();
    let events = doc["properties"]["events"]["properties"]
        .as_object()
        .unwrap();
    let entry = json!({"seq": 1, "id": "e_1", "parentId": null, "type": "message", "timestamp": 1, "payload": {}});
    let approval = json!({"id": "a", "tool": "t", "status": "pending"});
    let sample = |name: &str| match name {
        "entry" => entry.clone(),
        "ready" => json!({"id": "s", "after": 0, "lastSeq": 1, "busy": false}),
        "text_delta" | "reasoning" => json!({"text": "x"}),
        "tool_started" => json!({"tool": "t", "input": {}}),
        "tool_finished" => json!({"tool": "t", "ok": true, "preview": "p"}),
        "approval_requested" | "approval_resolved" => approval.clone(),
        "resync" => json!({"dropped": 3}),
        "closed" => json!({"id": "s"}),
        other => panic!("no sample for event {other}: add one"),
    };
    for name in events.keys() {
        let frame = Frame {
            id: None,
            event: name.clone(),
            data: sample(name).to_string(),
        };
        let ev = Event::from_frame(&frame).unwrap();
        assert!(
            !matches!(ev, Event::Other { .. }),
            "{name} decodes as Other"
        );
    }
    assert_eq!(
        events.len(),
        10,
        "a decoder arm was added or removed: update the schema"
    );
}
