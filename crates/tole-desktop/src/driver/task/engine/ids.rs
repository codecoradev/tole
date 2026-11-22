//! The serve's own ids of the approvals the model shows.
//!
//! The model keeps each approval under a cleaned id (`ApprovalInfo::cleaned`:
//! control and bidi characters gone or made spaces, cut at 64 characters), and
//! one card per cleaned id. An answer must go to the serve under the id the
//! serve used, so the driver keeps the raw id beside the cleaned one, noted
//! from the events at the point they enter the model. Two raw ids that clean to
//! the same string are one card in the model: that card cannot be answered
//! from here (which of the two would it be?), and neither can one the driver
//! never saw. Such a card says it can only be answered elsewhere
//! (`TaskView::undecidable`) and Stop is offered with it.

use std::collections::HashMap;

use tole_desktop_model::task::{ApprovalInfo, TaskEvent};

enum Raw {
    One(String),
    /// Two raw ids clean to this id.
    Many,
}

#[derive(Default)]
pub(super) struct ApprovalIds {
    by_cleaned: HashMap<String, Raw>,
}

fn cleaned(raw: &str) -> String {
    let info = ApprovalInfo {
        id: raw.to_owned(),
        tool: String::new(),
        summary: None,
    };
    info.cleaned().id
}

impl ApprovalIds {
    /// Note the ids of the events about to enter the model.
    pub(super) fn note(&mut self, events: &[TaskEvent]) {
        for event in events {
            match event {
                TaskEvent::ApprovalRequested(info) => self.insert(&info.id),
                TaskEvent::ApprovalsRefreshed { pending } => {
                    pending.iter().for_each(|info| self.insert(&info.id));
                }
                _ => {}
            }
        }
    }

    fn insert(&mut self, raw: &str) {
        let key = cleaned(raw);
        let same = matches!(self.by_cleaned.get(&key), Some(Raw::One(r)) if r == raw);
        if same {
            return;
        }
        let seen = self.by_cleaned.contains_key(&key);
        let value = if seen {
            Raw::Many
        } else {
            Raw::One(raw.to_owned())
        };
        self.by_cleaned.insert(key, value);
    }

    /// Forget the ids the model no longer shows.
    pub(super) fn prune(&mut self, live: &[String]) {
        self.by_cleaned.retain(|k, _| live.contains(k));
    }

    /// The serve's id of the card `shown`, when that is certain.
    pub(super) fn raw(&self, shown: &str) -> Option<&str> {
        match self.by_cleaned.get(shown) {
            Some(Raw::One(raw)) => Some(raw),
            _ => None,
        }
    }

    /// The cards among `live` that cannot be answered from here.
    pub(super) fn unanswerable(&self, live: &[String]) -> Vec<String> {
        let no_raw = |id: &&String| self.raw(id).is_none();
        live.iter().filter(no_raw).cloned().collect()
    }
}
