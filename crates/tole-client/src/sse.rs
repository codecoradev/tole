//! `GET /sessions/{id}/events`: an incremental Server-Sent Events reader and
//! the typed events it carries.
//!
//! Two kinds of frames, as the server defines them: durable `entry` frames
//! (`id: <seq>`, replayable with `after=<seq>`) and ephemeral ones (text
//! deltas, tool events, approvals) that are best effort and carry no id.

use crate::error::{Error, Result};
use crate::types::{Approval, Entry};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::VecDeque;
use std::io::{ErrorKind, Read};

/// Longest single SSE line accepted (an entry with a large tool result is one
/// line of compact JSON).
const MAX_LINE: usize = 16 * 1024 * 1024;
/// Most bytes of `data:` one event may accumulate over all its lines.
const MAX_EVENT: usize = 16 * 1024 * 1024;

/// One raw SSE frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    /// The `id:` field; set on durable frames only (it is the entry `seq`).
    /// Unlike a browser, the id does not carry over to the next frame.
    pub id: Option<u64>,
    pub event: String,
    /// The `data:` lines joined with `\n`.
    pub data: String,
}

/// What an SSE byte stream yields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SseItem {
    Frame(Frame),
    /// A comment line (`: ping`): the server's keep-alive.
    Heartbeat,
}

/// Push parser: feed it bytes in any split, take complete items out.
pub struct SseParser {
    line: Vec<u8>,
    id: Option<u64>,
    event: Option<String>,
    data: Option<String>,
    /// The previous byte was a CR: a following LF belongs to the same CRLF.
    last_cr: bool,
    /// The first line has been seen (a UTF-8 BOM may only precede it).
    started: bool,
    max_line: usize,
    max_event: usize,
}

impl Default for SseParser {
    fn default() -> Self {
        Self::with_limits(MAX_LINE, MAX_EVENT)
    }
}

impl SseParser {
    /// A parser that refuses a line over `max_line` bytes, or an event whose
    /// `data:` lines add up to over `max_event` bytes.
    pub fn with_limits(max_line: usize, max_event: usize) -> Self {
        Self {
            line: Vec::new(),
            id: None,
            event: None,
            data: None,
            last_cr: false,
            started: false,
            max_line,
            max_event,
        }
    }

    /// Lines end in LF, CRLF or a bare CR, as the SSE spec says.
    pub fn feed(&mut self, chunk: &[u8]) -> Result<Vec<SseItem>> {
        let mut out = Vec::new();
        for &b in chunk {
            match b {
                b'\n' if self.last_cr => self.last_cr = false,
                b'\r' | b'\n' => {
                    self.last_cr = b == b'\r';
                    let mut line = std::mem::take(&mut self.line);
                    let first = !std::mem::replace(&mut self.started, true);
                    if first && line.starts_with(b"\xEF\xBB\xBF") {
                        line.drain(..3);
                    }
                    self.on_line(&String::from_utf8_lossy(&line), &mut out)?;
                }
                _ => {
                    self.last_cr = false;
                    if self.line.len() >= self.max_line {
                        return Err(Error::Protocol("event stream line too long".into()));
                    }
                    self.line.push(b);
                }
            }
        }
        Ok(out)
    }

    fn on_line(&mut self, line: &str, out: &mut Vec<SseItem>) -> Result<()> {
        if line.is_empty() {
            if self.event.is_some() || self.data.is_some() {
                out.push(SseItem::Frame(Frame {
                    id: self.id,
                    event: self.event.take().unwrap_or_else(|| "message".into()),
                    data: self.data.take().unwrap_or_default(),
                }));
            }
            self.id = None;
        } else if line.starts_with(':') {
            out.push(SseItem::Heartbeat);
        } else {
            let (field, value) = line.split_once(':').unwrap_or((line, ""));
            let value = value.strip_prefix(' ').unwrap_or(value);
            match field {
                "id" => self.id = value.parse().ok(),
                "event" => self.event = Some(value.to_string()),
                "data" => {
                    let have = self.data.as_ref().map_or(0, |d| d.len() + 1);
                    if have + value.len() > self.max_event {
                        return Err(Error::Protocol("event stream event too large".into()));
                    }
                    match &mut self.data {
                        Some(d) => {
                            d.push('\n');
                            d.push_str(value);
                        }
                        None => self.data = Some(value.to_string()),
                    }
                }
                _ => {} // retry and unknown fields
            }
        }
        Ok(())
    }
}

/// Reads frames from a byte source as they arrive. Each `next_item` returns
/// as soon as one complete frame is available: nothing is buffered beyond the
/// frame being assembled.
pub struct FrameReader<R: Read> {
    src: R,
    parser: SseParser,
    queue: VecDeque<SseItem>,
    buf: Box<[u8]>,
    eof: bool,
}

impl<R: Read> FrameReader<R> {
    pub fn new(src: R) -> Self {
        Self {
            src,
            parser: SseParser::default(),
            queue: VecDeque::new(),
            buf: vec![0; 8192].into_boxed_slice(),
            eof: false,
        }
    }

    /// The next item, or `None` at a clean end of stream. A frame cut short by
    /// the end of the stream is dropped, never half delivered.
    pub fn next_item(&mut self) -> Result<Option<SseItem>> {
        loop {
            if let Some(item) = self.queue.pop_front() {
                return Ok(Some(item));
            }
            if self.eof {
                return Ok(None);
            }
            match self.src.read(&mut self.buf) {
                Ok(0) => self.eof = true,
                Ok(n) => self.queue.extend(self.parser.feed(&self.buf[..n])?),
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(e) => return Err(e.into()),
            }
        }
    }
}

/// `ready`: the replay is complete.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Ready {
    pub id: String,
    pub after: u64,
    pub last_seq: u64,
    pub busy: bool,
}

/// A typed frame. Unknown event names are kept as [`Event::Other`], so a newer
/// server never breaks an older client.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// Durable: one log entry, `entry.seq` is its position.
    Entry(Entry),
    Ready(Ready),
    TextDelta {
        text: String,
    },
    Reasoning {
        text: String,
    },
    ToolStarted {
        tool: String,
        input: Value,
    },
    ToolFinished {
        tool: String,
        ok: bool,
        preview: String,
    },
    ApprovalRequested(Approval),
    ApprovalResolved(Approval),
    /// The server dropped `dropped` ephemeral frames for this slow stream.
    Resync {
        dropped: u64,
    },
    /// The session left the server; the stream ends.
    Closed {
        id: String,
    },
    Other {
        event: String,
        data: Value,
    },
}

impl Event {
    /// Decode a frame. Only a durable `entry` frame that does not decode is an
    /// error: losing one would break the seq guarantee. Any other frame of a
    /// shape this client does not know (an unknown name, or a known one that a
    /// newer server changed) becomes [`Event::Other`] with its raw data, since
    /// ephemeral frames are best effort by design.
    pub fn from_frame(f: &Frame) -> Result<Event> {
        match Self::typed(f) {
            Err(_) if f.event != "entry" => Ok(Event::Other {
                event: f.event.clone(),
                data: serde_json::from_str(&f.data)
                    .unwrap_or_else(|_| Value::String(f.data.clone())),
            }),
            r => r,
        }
    }

    fn typed(f: &Frame) -> Result<Event> {
        fn data<T: serde::de::DeserializeOwned>(f: &Frame) -> Result<T> {
            Ok(serde_json::from_str(&f.data)?)
        }
        #[derive(Deserialize)]
        struct Text {
            text: String,
        }
        #[derive(Deserialize)]
        struct Started {
            tool: String,
            input: Value,
        }
        #[derive(Deserialize)]
        struct Finished {
            tool: String,
            ok: bool,
            preview: String,
        }
        #[derive(Deserialize)]
        struct Dropped {
            dropped: u64,
        }
        #[derive(Deserialize)]
        struct Closed {
            id: String,
        }
        Ok(match f.event.as_str() {
            "entry" => Event::Entry(data(f)?),
            "ready" => Event::Ready(data(f)?),
            "text_delta" => Event::TextDelta {
                text: data::<Text>(f)?.text,
            },
            "reasoning" => Event::Reasoning {
                text: data::<Text>(f)?.text,
            },
            "tool_started" => {
                let s: Started = data(f)?;
                Event::ToolStarted {
                    tool: s.tool,
                    input: s.input,
                }
            }
            "tool_finished" => {
                let s: Finished = data(f)?;
                Event::ToolFinished {
                    tool: s.tool,
                    ok: s.ok,
                    preview: s.preview,
                }
            }
            "approval_requested" => Event::ApprovalRequested(data(f)?),
            "approval_resolved" => Event::ApprovalResolved(data(f)?),
            "resync" => Event::Resync {
                dropped: data::<Dropped>(f)?.dropped,
            },
            "closed" => Event::Closed {
                id: data::<Closed>(f)?.id,
            },
            other => Event::Other {
                event: other.to_string(),
                data: serde_json::from_str(&f.data)
                    .unwrap_or_else(|_| Value::String(f.data.clone())),
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn frames(items: Vec<SseItem>) -> Vec<Frame> {
        items
            .into_iter()
            .filter_map(|i| match i {
                SseItem::Frame(f) => Some(f),
                SseItem::Heartbeat => None,
            })
            .collect()
    }

    const WIRE: &str = "id: 7\nevent: entry\ndata: {\"a\":1}\n\n: ping\n\nevent: text_delta\ndata: {\"text\":\"h\u{e9}llo\"}\n\n";

    fn expected() -> Vec<Frame> {
        vec![
            Frame {
                id: Some(7),
                event: "entry".into(),
                data: "{\"a\":1}".into(),
            },
            Frame {
                id: None,
                event: "text_delta".into(),
                data: "{\"text\":\"h\u{e9}llo\"}".into(),
            },
        ]
    }

    #[test]
    fn parses_the_same_whatever_the_chunking() {
        // Whole, byte by byte (splits inside a UTF-8 sequence and between
        // CR and LF), and every two-way split.
        let bytes = WIRE.as_bytes();
        let mut p = SseParser::default();
        assert_eq!(frames(p.feed(bytes).unwrap()), expected());
        let mut p = SseParser::default();
        let mut got = Vec::new();
        for b in bytes {
            got.extend(p.feed(&[*b]).unwrap());
        }
        assert_eq!(frames(got), expected());
        for cut in 0..bytes.len() {
            let mut p = SseParser::default();
            let mut got = p.feed(&bytes[..cut]).unwrap();
            got.extend(p.feed(&bytes[cut..]).unwrap());
            assert_eq!(frames(got), expected(), "cut at {cut}");
        }
    }

    #[test]
    fn crlf_multiline_data_comments_and_id_reset() {
        let mut p = SseParser::default();
        let got = p
            .feed(b"id: 3\r\nevent: x\r\ndata: a\r\ndata: b\r\n\r\nevent: y\r\ndata:z\r\n\r\n")
            .unwrap();
        let f = frames(got);
        assert_eq!(f[0].data, "a\nb");
        assert_eq!(f[0].id, Some(3));
        assert_eq!((f[1].id, f[1].data.as_str()), (None, "z"));
        // A heartbeat is reported on its own.
        assert_eq!(p.feed(b": ping\n\n").unwrap(), vec![SseItem::Heartbeat]);
    }

    /// A source that hands out one scripted chunk per read.
    struct Chunks(VecDeque<Vec<u8>>);
    impl Read for Chunks {
        fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
            match self.0.pop_front() {
                Some(c) => {
                    b[..c.len()].copy_from_slice(&c);
                    Ok(c.len())
                }
                None => Ok(0),
            }
        }
    }

    #[test]
    fn a_frame_is_yielded_before_the_next_chunk_is_read_and_a_cut_frame_is_dropped() {
        let chunks = [
            "event: text_delta\nda",
            "ta: {\"text\":\"a\"}\n\nevent: ent",
            "ry\ndata: {",
        ];
        let mut r = FrameReader::new(Chunks(
            chunks.iter().map(|c| c.as_bytes().to_vec()).collect(),
        ));
        let Some(SseItem::Frame(f)) = r.next_item().unwrap() else {
            panic!("a frame")
        };
        assert_eq!(f.event, "text_delta");
        // Only two chunks were consumed to produce it; the third is untouched.
        assert_eq!(r.src.0.len(), 1);
        assert_eq!(
            r.next_item().unwrap(),
            None,
            "the cut frame is not delivered"
        );
    }

    #[test]
    fn bare_cr_and_a_leading_bom_end_and_start_lines_correctly() {
        let wire = "\u{feff}event: a\rdata: 1\r\rid: 4\revent: b\rdata: 2\r\n\r\n";
        let want = vec![
            Frame {
                id: None,
                event: "a".into(),
                data: "1".into(),
            },
            Frame {
                id: Some(4),
                event: "b".into(),
                data: "2".into(),
            },
        ];
        for cut in 0..wire.len() {
            let mut p = SseParser::default();
            let mut got = p.feed(&wire.as_bytes()[..cut]).unwrap();
            got.extend(p.feed(&wire.as_bytes()[cut..]).unwrap());
            assert_eq!(frames(got), want, "cut at {cut}");
        }
        // The BOM is only skipped at the very start.
        let mut p = SseParser::default();
        p.feed(b"data: x\n\n").unwrap();
        let got = p.feed("\u{feff}event: z\ndata: 1\n\n".as_bytes()).unwrap();
        assert_eq!(
            frames(got)[0].event,
            "message",
            "a later BOM spoils its line"
        );
    }

    #[test]
    fn an_events_total_data_is_capped_not_just_one_line() {
        let mut p = SseParser::with_limits(64, 100);
        let mut wire = Vec::new();
        for _ in 0..20 {
            wire.extend_from_slice(b"data: 0123456789\n");
        }
        assert!(matches!(p.feed(&wire), Err(Error::Protocol(_))));
        // Under the cap, many lines are fine; a long line is refused.
        let mut p = SseParser::with_limits(64, 100);
        assert!(p.feed(b"data: a\ndata: b\n\n").is_ok());
        assert!(matches!(p.feed(&[b'x'; 65]), Err(Error::Protocol(_))));
    }

    #[test]
    fn events_are_typed_and_unknown_names_survive() {
        let f = |event: &str, data: &str| Frame {
            id: None,
            event: event.into(),
            data: data.into(),
        };
        assert_eq!(
            Event::from_frame(&f("text_delta", r#"{"text":"x"}"#)).unwrap(),
            Event::TextDelta { text: "x".into() }
        );
        assert_eq!(
            Event::from_frame(&f(
                "ready",
                r#"{"id":"s","after":0,"lastSeq":4,"busy":false}"#
            ))
            .unwrap(),
            Event::Ready(Ready {
                id: "s".into(),
                after: 0,
                last_seq: 4,
                busy: false
            })
        );
        assert!(matches!(
            Event::from_frame(&f(
                "approval_requested",
                r#"{"id":"a","tool":"write_file","status":"pending"}"#
            )),
            Ok(Event::ApprovalRequested(_))
        ));
        assert!(matches!(
            Event::from_frame(&f("future_thing", r#"{"k":1}"#)),
            Ok(Event::Other { .. })
        ));
        // A known name whose data does not decode is kept as `Other`, not an
        // error; only a broken durable `entry` is.
        assert_eq!(
            Event::from_frame(&f("ready", r#"{"id":"s"}"#)).unwrap(),
            Event::Other {
                event: "ready".into(),
                data: json!({"id": "s"})
            }
        );
        assert_eq!(
            Event::from_frame(&f("text_delta", "not json")).unwrap(),
            Event::Other {
                event: "text_delta".into(),
                data: json!("not json")
            }
        );
        assert!(Event::from_frame(&f("entry", r#"{"seq":1}"#)).is_err());
    }
}
