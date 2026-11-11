//! Streamed chunks -> committed scrollback lines. A line is committed when
//! its newline arrives or the stream kind changes; the unfinished tail stays
//! as the live `partial`.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    User,
    Text,
    Reasoning,
    Tool,
    /// Continuation lines of a tool result (diff styling applies).
    ToolOut,
    /// Markdown heading / fenced code line (answers only).
    Heading,
    Code,
    Note,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Out {
    pub kind: Kind,
    pub text: String,
}

#[derive(Default)]
pub struct Transcript {
    cur: Option<Kind>,
    partial: String,
    /// UI-state flags (#462): the scrollback is append-only, so they only
    /// shape output that has not been committed yet.
    pub hide_reasoning: bool,
    pub expand_tools: bool,
    in_fence: bool,
    /// Rows of the last tool block that the collapsed view left out.
    last_tool: Vec<String>,
    last_shown: bool,
    /// Label of the expand-tool-output key for the `[+n lines]` hint
    /// (empty = `Ctrl+O`); set from the keymap by the app.
    pub(crate) tools_key: String,
}

/// Most continuation rows kept per tool block, and their width.
const TOOL_ROWS: usize = 50;
const TOOL_COLS: usize = 200;

impl Transcript {
    /// One finished line. Answer text gets minimal markdown classing:
    /// `#` headings, fenced code (fence lines dropped, body indented).
    fn done(&mut self, kind: Kind, text: String) -> Option<Out> {
        if kind == Kind::User {
            self.in_fence = false;
        }
        if kind != Kind::Text {
            return Some(Out { kind, text });
        }
        if text.trim_start().starts_with("```") {
            self.in_fence = !self.in_fence;
            return None;
        }
        if self.in_fence {
            return Some(Out {
                kind: Kind::Code,
                text: format!("    {text}"),
            });
        }
        let hashes = text.chars().take_while(|c| *c == '#').count();
        if (1..=6).contains(&hashes) && text[hashes..].starts_with(' ') {
            return Some(Out {
                kind: Kind::Heading,
                text: text[hashes..].trim().to_string(),
            });
        }
        Some(Out { kind, text })
    }

    /// Append a streamed chunk; returns the lines it completed.
    pub fn push(&mut self, kind: Kind, chunk: &str) -> Vec<Out> {
        if kind == Kind::Reasoning && self.hide_reasoning {
            return Vec::new();
        }
        let mut out = Vec::new();
        if self.cur != Some(kind) {
            out.extend(self.flush());
            self.cur = Some(kind);
        }
        self.partial.push_str(&crate::sanitize(chunk));
        while let Some(i) = self.partial.find('\n') {
            let text: String = self.partial.drain(..=i).collect();
            out.extend(self.done(kind, text.trim_end_matches('\n').to_string()));
        }
        out
    }

    /// A whole line (tool line, note, user echo): flushes the partial first.
    pub fn line(&mut self, kind: Kind, text: &str) -> Vec<Out> {
        let mut out = self.flush();
        for t in crate::sanitize(text).split('\n') {
            out.extend(self.done(kind, t.to_string()));
        }
        out
    }

    /// A finished tool call: one head line, then (collapsed) a `[+n lines]`
    /// hint or (expanded) the remaining rows. Sanitized before anything else.
    pub fn tool_result(&mut self, ok: bool, preview: &str) -> Vec<Out> {
        let clean = crate::sanitize(preview);
        let mut it = clean.lines();
        let first = it.next().unwrap_or("");
        let head: String = first.chars().take(80).collect();
        let cut = first.chars().count() > 80;
        let head = if cut { format!("{head}…") } else { head };
        let mut rest: Vec<String> = Vec::new();
        if cut {
            rest.push(first.chars().take(TOOL_COLS).collect());
        }
        rest.extend(it.map(|l| l.chars().take(TOOL_COLS).collect::<String>()));
        rest.truncate(TOOL_ROWS);
        let mut out = self.line(
            Kind::Tool,
            &format!("  {} {head}", if ok { "✓" } else { "✗" }),
        );
        self.last_shown = self.expand_tools;
        if self.expand_tools {
            out.extend(rows(&rest));
        } else if !rest.is_empty() {
            let n = rest.len();
            let key = if self.tools_key.is_empty() {
                "Ctrl+O"
            } else {
                &self.tools_key
            };
            out.push(Out {
                kind: Kind::Tool,
                text: format!("    [+{n} line{} · {key}]", if n == 1 { "" } else { "s" }),
            });
        }
        self.last_tool = rest;
        out
    }

    /// Ctrl+O: flip how tool output is shown. Expanding prints the rows the
    /// last block left out (the scrollback cannot be edited in place).
    pub fn toggle_tools(&mut self) -> Vec<Out> {
        self.expand_tools = !self.expand_tools;
        let mut out = self.flush();
        if self.expand_tools && !self.last_shown {
            self.last_shown = true;
            out.extend(rows(&self.last_tool));
        }
        out
    }

    /// Commit the unfinished tail, if any.
    pub fn flush(&mut self) -> Vec<Out> {
        let kind = self.cur.take();
        match (kind, std::mem::take(&mut self.partial)) {
            (Some(kind), text) if !text.is_empty() => self.done(kind, text).into_iter().collect(),
            _ => vec![],
        }
    }

    /// Ctrl+T: flip reasoning visibility for output not yet committed.
    pub fn toggle_reasoning(&mut self) -> Vec<Out> {
        self.hide_reasoning = !self.hide_reasoning;
        self.flush()
    }

    pub fn partial(&self) -> Option<(Kind, &str)> {
        self.cur
            .filter(|_| !self.partial.is_empty())
            .map(|k| (k, self.partial.as_str()))
    }
}

fn rows(lines: &[String]) -> Vec<Out> {
    lines
        .iter()
        .map(|l| Out {
            kind: Kind::ToolOut,
            text: format!("    {l}"),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(v: &[Out]) -> Vec<&str> {
        v.iter().map(|o| o.text.as_str()).collect()
    }

    #[test]
    fn chunks_commit_only_on_newline() {
        let mut t = Transcript::default();
        assert!(t.push(Kind::Text, "hel").is_empty());
        assert_eq!(t.partial(), Some((Kind::Text, "hel")));
        let out = t.push(Kind::Text, "lo\nwor");
        assert_eq!(texts(&out), ["hello"]);
        assert_eq!(t.partial(), Some((Kind::Text, "wor")));
        assert_eq!(texts(&t.flush()), ["wor"]);
        assert_eq!(t.partial(), None);
    }

    #[test]
    fn kind_change_flushes_the_other_stream() {
        let mut t = Transcript::default();
        t.push(Kind::Reasoning, "think");
        let out = t.push(Kind::Text, "ans");
        assert_eq!(
            out,
            [Out {
                kind: Kind::Reasoning,
                text: "think".into()
            }]
        );
        assert_eq!(t.partial(), Some((Kind::Text, "ans")));
    }

    #[test]
    fn whole_line_flushes_partial_first_and_keeps_order() {
        let mut t = Transcript::default();
        t.push(Kind::Text, "par");
        let out = t.line(Kind::Tool, "▸ write_file");
        assert_eq!(texts(&out), ["par", "▸ write_file"]);
        assert_eq!(out[1].kind, Kind::Tool);
        assert_eq!(t.partial(), None);
    }

    #[test]
    fn blank_lines_inside_a_stream_survive() {
        let mut t = Transcript::default();
        let out = t.push(Kind::Text, "a\n\nb\n");
        assert_eq!(texts(&out), ["a", "", "b"]);
    }
}
