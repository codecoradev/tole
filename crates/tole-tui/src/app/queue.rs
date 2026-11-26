//! The follow-up queue and the draft it feeds (#462): queued rows, putting
//! queued or steering text back in front of the draft, and starting the next
//! queued prompt.

use crate::{clip, one_line, App, Kind, QUEUE_ROWS};

impl App {
    /// Compact rows of the follow-up queue (empty when nothing waits).
    pub fn queue_rows(&self, width: usize) -> Vec<String> {
        self.queue_rows_max(width, QUEUE_ROWS)
    }

    fn queue_rows_max(&self, width: usize, max: usize) -> Vec<String> {
        if max == 0 {
            return Vec::new();
        }
        let mut rows: Vec<String> = self
            .queue
            .iter()
            .take(max)
            .map(|q| clip(&format!("queued: {}", one_line(q)), width.saturating_sub(1)))
            .collect();
        if self.queue.len() > max {
            rows[max - 1] = clip(
                &format!("+{} more queued", self.queue.len() - max + 1),
                width.saturating_sub(1),
            );
        }
        rows
    }

    /// Rows above the editor: the image row, then the follow-up queue, at
    /// most `QUEUE_ROWS` together.
    pub fn pending_rows(&self, width: usize) -> Vec<String> {
        let mut rows: Vec<String> = self.image_row(width).into_iter().collect();
        let left = QUEUE_ROWS - rows.len();
        rows.extend(self.queue_rows_max(width, left));
        rows
    }

    /// Put queued (and `extra`) messages back in front of the draft; nothing
    /// is lost on cancel or on a turn that did not end in an answer.
    pub(crate) fn return_queue(&mut self, extra: Vec<String>) {
        let mut back = extra;
        back.append(&mut self.queue);
        self.prepend_draft(back);
    }

    pub(crate) fn prepend_draft(&mut self, back: Vec<String>) {
        if back.is_empty() {
            return;
        }
        let draft = self.editor.text().to_string();
        let mut text = back.join("\n");
        if !draft.is_empty() {
            text = format!("{text}\n{draft}");
        }
        self.editor.set(&text);
    }

    /// The head of the queue, when idle: becomes the next prompt (one at a time).
    pub fn start_queued(&mut self) -> Option<String> {
        if self.running || self.queue.is_empty() {
            return None;
        }
        let text = self.queue.remove(0);
        self.running = true;
        self.saw_text = false;
        self.images_sent();
        let echoed = self.transcript.line(Kind::User, &format!("> {text}"));
        self.out.extend(echoed);
        Some(text)
    }

    /// Steering text the settled turn never consumed (the loop drains it).
    pub fn leftover(&mut self, texts: Vec<String>) {
        self.leftover = texts;
    }

    /// Echo a steering message the turn accepted.
    pub fn steered(&mut self, text: &str) {
        let echoed = self
            .transcript
            .line(Kind::User, &format!("> {text}  [steer]"));
        self.out.extend(echoed);
    }

    /// Give a draft back to the editor (a refused steer).
    pub fn restore_draft(&mut self, text: &str) {
        self.editor.set(text);
    }
}
