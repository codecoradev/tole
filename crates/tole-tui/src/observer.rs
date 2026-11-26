//! Observer callbacks -> UI messages: the [`TurnObserver`] a turn thread
//! reports through, and the one-line tool summary it shows.

use std::sync::mpsc::Sender;
use std::sync::Mutex;

use serde_json::Value;
use tole_core::turn::TurnObserver;

use crate::{clip, one_line, Msg};

/// Observer callbacks -> UI messages.
pub(crate) struct ChanObserver(pub(crate) Mutex<Sender<Msg>>);

impl ChanObserver {
    fn send(&self, m: Msg) {
        let _ = self.0.lock().unwrap_or_else(|p| p.into_inner()).send(m);
    }
}

/// `▸ write_file out.txt`: the tool plus its first short string argument.
pub fn tool_line(tool: &str, input: &Value) -> String {
    let arg = input
        .as_object()
        .and_then(|o| o.values().find_map(Value::as_str))
        .map(|s| clip(&one_line(s.lines().next().unwrap_or("")), 60))
        .unwrap_or_default();
    format!("▸ {tool} {arg}").trim_end().to_string()
}

impl TurnObserver for ChanObserver {
    fn tool_started(&self, tool: &str, input: &Value) {
        self.send(Msg::Tool(tool_line(tool, input)));
    }
    fn tool_finished(&self, _tool: &str, ok: bool, preview: &str) {
        self.send(Msg::ToolResult {
            ok,
            preview: preview.to_string(),
        });
    }
    fn reasoning(&self, text: &str) {
        self.send(Msg::Reasoning(text.to_string()));
    }
    fn text_delta(&self, text: &str) {
        self.send(Msg::Text(text.to_string()));
    }
}
