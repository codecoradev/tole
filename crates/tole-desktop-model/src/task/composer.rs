//! The composer: text, folder chip, image attachments, and what Enter does.
//!
//! - Enter sends, Shift+Enter breaks the line, Enter while an input method
//!   composes is the input method's. A blank text is never sent.
//! - What Enter does follows [`SendMode`]: a prompt, a note to the running
//!   turn (text only: attachments stay in the draft), or nothing.
//! - An image is at most [`MAX_IMAGE_BYTES`] and named like a png, jpg, jpeg,
//!   gif or webp (a courtesy: serve sniffs the bytes).

use super::event::MAX_TARGET_CHARS;
use super::status::SendMode;
use crate::plain::printable;

/// 10 MiB, `tole_core::image::MAX_IMAGE_BYTES`, which this crate cannot name.
pub const MAX_IMAGE_BYTES: u64 = 10 * 1024 * 1024;

/// At most this many images go with one prompt: the desktop's own limit, since
/// neither the client nor serve has one, and every image is re-sent each step.
pub const MAX_IMAGES: usize = 8;

const IMAGE_EXTENSIONS: [&str; 5] = ["png", "jpg", "jpeg", "gif", "webp"];

/// The working folder, shown as a chip above the text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FolderChip {
    pub name: String,
    pub path: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AttachmentId(pub u32);

/// An image the owner added; the adapter reads and uploads it on send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attachment {
    pub id: AttachmentId,
    pub name: String,
    pub bytes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachError {
    Empty,
    TooLarge { bytes: u64 },
    TooMany,
    NotAnImage,
}

/// The state of an Enter press.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Enter {
    pub shift: bool,
    /// An input method is composing text.
    pub composing: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnterOutcome {
    /// Nothing to do; the key goes to whoever else wants it.
    Ignored,
    /// Insert a line break in the text area.
    Newline,
    Submit(Submission),
}

/// What to send; the composer is already cleared ([`Composer::restore`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Submission {
    pub mode: SendMode,
    pub text: String,
    /// Empty for a steer note.
    pub attachments: Vec<Attachment>,
}

#[derive(Debug, Clone, Default)]
pub struct Composer {
    pub text: String,
    pub folder: Option<FolderChip>,
    pub attachments: Vec<Attachment>,
    next_id: u32,
}

impl Composer {
    /// A composer holding `text`: what the window asks Enter's rules about
    /// the text of its own box.
    pub fn with_text(text: impl Into<String>) -> Self {
        Composer {
            text: text.into(),
            ..Composer::default()
        }
    }

    /// Images only go with a new prompt.
    pub fn can_attach(mode: SendMode) -> bool {
        mode == SendMode::Send
    }

    /// Add an image by its file name and size.
    pub fn attach(&mut self, name: &str, bytes: u64) -> Result<AttachmentId, AttachError> {
        let ext = name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase());
        if !ext.is_some_and(|e| IMAGE_EXTENSIONS.contains(&e.as_str())) {
            return Err(AttachError::NotAnImage);
        }
        if self.attachments.len() >= MAX_IMAGES {
            return Err(AttachError::TooMany);
        }
        match bytes {
            0 => return Err(AttachError::Empty),
            b if b > MAX_IMAGE_BYTES => return Err(AttachError::TooLarge { bytes }),
            _ => {}
        }
        let id = AttachmentId(self.next_id);
        self.next_id += 1;
        let name = printable(name, MAX_TARGET_CHARS);
        self.attachments.push(Attachment { id, name, bytes });
        Ok(id)
    }

    pub fn remove(&mut self, id: AttachmentId) {
        self.attachments.retain(|a| a.id != id);
    }

    /// Whether Enter could send now.
    pub fn can_submit(&self, mode: SendMode) -> bool {
        !matches!(mode, SendMode::Disabled(_)) && !self.text.trim().is_empty()
    }

    pub fn enter(&mut self, key: Enter, mode: SendMode) -> EnterOutcome {
        if key.composing || matches!(mode, SendMode::Disabled(_)) {
            return EnterOutcome::Ignored;
        }
        if key.shift {
            return EnterOutcome::Newline;
        }
        if !self.can_submit(mode) {
            return EnterOutcome::Ignored;
        }
        let attachments = if mode == SendMode::Send {
            std::mem::take(&mut self.attachments)
        } else {
            Vec::new()
        };
        let text = std::mem::take(&mut self.text).trim_end().to_string();
        EnterOutcome::Submit(Submission {
            mode,
            text,
            attachments,
        })
    }

    /// Put a failed send back; text typed meanwhile stays after it.
    pub fn restore(&mut self, sent: Submission) {
        if !self.text.is_empty() {
            self.text = format!("{}\n{}", sent.text, self.text);
        } else {
            self.text = sent.text;
        }
        let mut back = sent.attachments;
        back.append(&mut self.attachments);
        back.truncate(MAX_IMAGES); // the restored ones first
        self.attachments = back;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task::status::Phase;

    #[rustfmt::skip]
    const ENTER: Enter = Enter { shift: false, composing: false };

    #[rustfmt::skip]
    fn typed(text: &str) -> Composer {
        Composer { text: text.into(), ..Composer::default() }
    }

    fn submitted(c: &mut Composer, mode: SendMode) -> Submission {
        match c.enter(ENTER, mode) {
            EnterOutcome::Submit(s) => s,
            other => panic!("not sent: {other:?}"),
        }
    }

    #[test]
    fn enter_sends_shift_enter_breaks_the_line_and_composing_is_the_input_methods() {
        let mut c = typed("tidy the folder  \n");
        c.attach("shot.PNG", 1_000).unwrap();
        let shift = Enter {
            shift: true,
            ..ENTER
        };
        let ime = Enter {
            composing: true,
            ..ENTER
        };
        assert_eq!(c.enter(shift, SendMode::Send), EnterOutcome::Newline);
        assert_eq!(c.enter(ime, SendMode::Send), EnterOutcome::Ignored);
        let s = submitted(&mut c, SendMode::Send);
        assert_eq!(
            (s.text.as_str(), s.attachments.len()),
            ("tidy the folder", 1)
        );
        assert!(c.text.is_empty() && c.attachments.is_empty());
    }

    #[test]
    fn blank_text_and_a_disabled_composer_send_nothing_and_keep_the_text() {
        assert_eq!(
            typed(" \n ").enter(ENTER, SendMode::Send),
            EnterOutcome::Ignored
        );
        let (mut c, off) = (typed("wait"), SendMode::Disabled(Phase::WaitingApproval));
        assert_eq!(c.enter(ENTER, off), EnterOutcome::Ignored);
        assert_eq!(c.text, "wait");
        assert!(!c.can_submit(off) && c.can_submit(SendMode::Steer));
        c.attach("a.jpg", 10).unwrap();
        let s = submitted(&mut c, SendMode::Steer);
        assert!(s.attachments.is_empty() && c.attachments.len() == 1);
        assert!(!Composer::can_attach(SendMode::Steer));
    }

    #[test]
    fn images_are_capped_at_ten_mebibytes_and_must_look_like_images() {
        let mut c = Composer::default();
        assert!(c.attach("big.png", MAX_IMAGE_BYTES).is_ok());
        let over = MAX_IMAGE_BYTES + 1;
        assert_eq!(
            c.attach("big.png", over),
            Err(AttachError::TooLarge { bytes: over })
        );
        assert_eq!(c.attach("empty.png", 0), Err(AttachError::Empty));
        assert_eq!(c.attach("notes.txt", 5), Err(AttachError::NotAnImage));
        assert_eq!(c.attachments.len(), 1);
        (1..MAX_IMAGES).for_each(|_| {
            c.attach("a.png", 5).unwrap();
        });
        assert_eq!(c.attach("a.png", 5), Err(AttachError::TooMany));
        assert_eq!(c.attachments.len(), MAX_IMAGES);
    }

    #[test]
    fn restoring_a_failed_send_keeps_the_draft_and_what_was_typed_since() {
        let mut c = typed("first");
        let a = c.attach("a.gif", 5).unwrap();
        let b = c.attach("b.webp", 5).unwrap();
        c.remove(a);
        let sent = submitted(&mut c, SendMode::Send);
        c.text = "second".into();
        c.restore(sent);
        assert_eq!((c.text.as_str(), c.attachments[0].id), ("first\nsecond", b));
    }
}
