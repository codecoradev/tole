//! Streamed chunks -> committed scrollback lines. A line is committed when
//! its newline arrives or the stream kind changes; the unfinished tail stays
//! as the live `partial`.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    User,
    Text,
    Reasoning,
    Tool,
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
}

impl Transcript {
    /// Append a streamed chunk; returns the lines it completed.
    pub fn push(&mut self, kind: Kind, chunk: &str) -> Vec<Out> {
        let mut out = Vec::new();
        if self.cur != Some(kind) {
            out.extend(self.flush());
            self.cur = Some(kind);
        }
        self.partial.push_str(&crate::sanitize(chunk));
        while let Some(i) = self.partial.find('\n') {
            let text: String = self.partial.drain(..=i).collect();
            out.push(Out {
                kind,
                text: text.trim_end_matches('\n').to_string(),
            });
        }
        out
    }

    /// A whole line (tool line, note, user echo): flushes the partial first.
    pub fn line(&mut self, kind: Kind, text: &str) -> Vec<Out> {
        let mut out = self.flush();
        out.extend(crate::sanitize(text).split('\n').map(|t| Out {
            kind,
            text: t.to_string(),
        }));
        out
    }

    /// Commit the unfinished tail, if any.
    pub fn flush(&mut self) -> Vec<Out> {
        let kind = self.cur.take();
        match (kind, std::mem::take(&mut self.partial)) {
            (Some(kind), text) if !text.is_empty() => vec![Out { kind, text }],
            _ => vec![],
        }
    }

    pub fn partial(&self) -> Option<(Kind, &str)> {
        self.cur
            .filter(|_| !self.partial.is_empty())
            .map(|k| (k, self.partial.as_str()))
    }
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
