//! Messages from the turn thread to the UI ([`Msg`]) and how the app turns one
//! into transcript lines and state changes.

use crate::{App, Kind, Pending, TurnEnd};

pub enum Msg {
    Text(String),
    Reasoning(String),
    Tool(String),
    /// A finished tool call; collapsing/diff styling happen in the transcript.
    ToolResult {
        ok: bool,
        preview: String,
    },
    Approval(Pending),
    Done(TurnEnd),
}

impl App {
    pub fn on_msg(&mut self, m: Msg) {
        let lines = match m {
            Msg::Text(t) => {
                self.saw_text = true;
                self.transcript.push(Kind::Text, &t)
            }
            Msg::Reasoning(t) => self.transcript.push(Kind::Reasoning, &t),
            Msg::Tool(t) => self.transcript.line(Kind::Tool, &t),
            Msg::ToolResult { ok, preview } => self.transcript.tool_result(ok, &preview),
            Msg::Approval(p) => {
                let mut v = self.transcript.flush();
                let tag = if p.request.is_destructive() {
                    "destructive"
                } else {
                    "write"
                };
                v.extend(
                    self.transcript
                        .line(Kind::Note, &format!("? {tag}: {}", p.request.text())),
                );
                self.approval = Some(p);
                v
            }
            Msg::Done(end) => {
                self.running = false;
                self.approval = None;
                self.footer_stale = true;
                self.images_stale = std::mem::take(&mut self.images_in_flight);
                let mut v = self.transcript.flush();
                let left = std::mem::take(&mut self.leftover);
                if matches!(end, TurnEnd::Final(_)) {
                    // steering that arrived too late runs next, ahead of the queue
                    self.queue.splice(0..0, left);
                } else {
                    self.return_queue(left);
                }
                match end {
                    TurnEnd::Final(text) if !self.saw_text => {
                        v.extend(self.transcript.line(Kind::Text, &text))
                    }
                    TurnEnd::Final(_) => {}
                    TurnEnd::Cancelled => {
                        v.extend(self.transcript.line(Kind::Note, "(turn cancelled)"))
                    }
                    TurnEnd::Stopped(m) => {
                        v.extend(self.transcript.line(Kind::Note, &format!("({m})")))
                    }
                }
                v
            }
        };
        self.out.extend(lines);
    }
}
