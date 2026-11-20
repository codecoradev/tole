//! The sidebar sections of the app shell (design board S2).

/// A top-level page the sidebar can show. The words come from
/// [`crate::strings`], never from this module.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Section {
    NewTask,
    Search,
    Approvals,
    Automations,
    Discussions,
    SkillsAndTools,
}

impl Section {
    /// Every section in sidebar order.
    pub const ALL: [Section; 6] = [
        Section::NewTask,
        Section::Search,
        Section::Approvals,
        Section::Automations,
        Section::Discussions,
        Section::SkillsAndTools,
    ];

    /// Position in [`Section::ALL`].
    pub fn index(self) -> usize {
        Section::ALL
            .iter()
            .position(|s| *s == self)
            .expect("every variant is listed in ALL")
    }

    /// The section after this one, wrapping around (keyboard navigation).
    pub fn next(self) -> Section {
        Section::ALL[(self.index() + 1) % Section::ALL.len()]
    }

    /// The section before this one, wrapping around.
    pub fn prev(self) -> Section {
        Section::ALL[(self.index() + Section::ALL.len() - 1) % Section::ALL.len()]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_lists_each_variant_once_in_index_order() {
        for (i, s) in Section::ALL.iter().enumerate() {
            assert_eq!(s.index(), i);
        }
    }

    #[test]
    fn next_and_prev_wrap() {
        assert_eq!(Section::NewTask.prev(), Section::SkillsAndTools);
        assert_eq!(Section::SkillsAndTools.next(), Section::NewTask);
        for s in Section::ALL {
            assert_eq!(s.next().prev(), s);
        }
    }
}
