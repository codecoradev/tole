//! Placeholder app-shell state and its reducer (design board S2).
//!
//! The state is what the window shows: the selected sidebar section, the
//! working folders, and where the desktop is connected. Every change goes
//! through [`ShellState::apply`] so tests need no window. Nothing here talks
//! to a serve yet; the typed client (#1040) will feed actions in later.

use crate::skills::Badge;
use crate::tokens::Tone;
use crate::{strings, Section};

/// Stable handle of a folder in [`ShellState::folders`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FolderId(pub u32);

/// A working folder the owner added (design: the "Folders" list).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Folder {
    pub id: FolderId,
    pub name: String,
    pub path: String,
    /// Sample data of the preview, not a folder of the owner. The first real
    /// folder added replaces all sample folders.
    pub sample: bool,
}

/// Which server the desktop talks to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// The serve on this machine.
    ThisComputer,
    /// A saved remote server, by the name the owner gave it (#1044).
    Remote { name: String },
}

impl Target {
    /// The status this target may show. A remote server is never `Connected`
    /// until a remote client exists (#1044): anything that claims so is read
    /// as `Connecting`, the less connected reading, whoever sets it.
    pub fn clamp(&self, status: ConnectionStatus) -> ConnectionStatus {
        match (self, status) {
            (Target::Remote { .. }, ConnectionStatus::Connected) => ConnectionStatus::Connecting,
            (_, status) => status,
        }
    }
}

/// How far the connection to the target is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionStatus {
    /// No client feeds the model yet: the window is a preview, not a state of
    /// any server.
    Preview,
    Connecting,
    Connected,
    Unreachable,
}

impl ConnectionStatus {
    /// The colour meaning of the status dot. A preview claims nothing, so it
    /// is neutral, and never the colour of "connected".
    pub fn tone(self) -> Tone {
        match self {
            ConnectionStatus::Preview => Tone::Neutral,
            ConnectionStatus::Connecting => Tone::Attention,
            ConnectionStatus::Connected => Tone::Good,
            ConnectionStatus::Unreachable => Tone::Bad,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Connection {
    pub target: Target,
    pub status: ConnectionStatus,
}

/// Everything that can change the shell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    SelectSection(Section),
    /// The "New task" button: goes to the home page. Creating the session
    /// comes with the typed client (#1040); until then this is all it does.
    NewTask,
    /// `None` clears the folder selection. An unknown id is ignored.
    SelectFolder(Option<FolderId>),
    /// Adds a folder and selects it; a folder with the same path is selected
    /// instead of added twice.
    AddFolder {
        name: String,
        path: String,
    },
    /// Removes a folder; the selection is cleared if it pointed at it.
    RemoveFolder(FolderId),
    SetConnectionStatus(ConnectionStatus),
    /// Replaces the target and the status together (see
    /// [`Connection::resolve`]).
    SetConnection(Connection),
    /// The approvals badge: `ApprovalsState::waiting_badge` of the queue, so a
    /// list that is not known reads `Unknown`, never a zero.
    SetApprovalsPending(Badge),
    /// How many automations are on.
    SetAutomationsOn(usize),
    /// How many discussions need the owner.
    SetDiscussionsNeedYou(usize),
    /// How many skills and tools are new and not yet reviewed.
    SetSkillsNew(usize),
    ToggleSidebar,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellState {
    pub section: Section,
    pub folders: Vec<Folder>,
    pub selected_folder: Option<FolderId>,
    pub connection: Connection,
    pub approvals_pending: Badge,
    pub automations_on: usize,
    pub discussions_need_you: usize,
    pub skills_new: usize,
    pub sidebar_collapsed: bool,
    next_folder_id: u32,
}

impl ShellState {
    /// An empty shell: no folders, connecting to this computer.
    pub fn new() -> Self {
        ShellState {
            section: Section::NewTask,
            folders: Vec::new(),
            selected_folder: None,
            connection: Connection {
                target: Target::ThisComputer,
                status: ConnectionStatus::Connecting,
            },
            approvals_pending: Badge::Count(0),
            automations_on: 0,
            discussions_need_you: 0,
            skills_new: 0,
            sidebar_collapsed: false,
            next_folder_id: 1,
        }
    }

    /// The state the first window shows: a preview with the sample folders of
    /// the design draft, flagged as sample data and not connected to anything.
    /// Replaced by real data once a client feeds the model (#1040).
    pub fn placeholder() -> Self {
        let mut state = ShellState::new();
        for (name, path) in [
            ("Invoices", "~/Invoices"),
            ("Reports", "~/Reports"),
            ("Photos", "~/Photos"),
        ] {
            state.add_folder(name.to_owned(), path.to_owned(), true);
        }
        state.apply(Action::SelectFolder(None));
        state.apply(Action::SetConnectionStatus(ConnectionStatus::Preview));
        state
    }

    /// Applies one action. Returns `true` when the state changed, so a host
    /// can skip a redraw.
    pub fn apply(&mut self, action: Action) -> bool {
        let before = self.clone();
        match action {
            Action::SelectSection(section) => self.section = section,
            Action::NewTask => self.section = Section::NewTask,
            Action::SelectFolder(None) => self.selected_folder = None,
            Action::SelectFolder(Some(id)) => {
                if self.folders.iter().any(|f| f.id == id) {
                    self.selected_folder = Some(id);
                }
            }
            Action::AddFolder { name, path } => self.add_folder(name, path, false),
            Action::RemoveFolder(id) => {
                self.folders.retain(|f| f.id != id);
                if self.selected_folder == Some(id) {
                    self.selected_folder = None;
                }
            }
            Action::SetConnectionStatus(status) => {
                self.connection.status = self.connection.target.clamp(status);
            }
            Action::SetConnection(connection) => {
                let status = connection.target.clamp(connection.status);
                self.connection = Connection {
                    status,
                    ..connection
                };
            }
            Action::SetApprovalsPending(badge) => self.approvals_pending = badge,
            Action::SetAutomationsOn(count) => self.automations_on = count,
            Action::SetDiscussionsNeedYou(count) => self.discussions_need_you = count,
            Action::SetSkillsNew(count) => self.skills_new = count,
            Action::ToggleSidebar => self.sidebar_collapsed = !self.sidebar_collapsed,
        }
        *self != before
    }

    fn add_folder(&mut self, name: String, path: String, sample: bool) {
        if !sample {
            // The first real folder replaces the sample folders of the preview.
            self.folders.retain(|f| !f.sample);
            if self.selected().is_none() {
                self.selected_folder = None;
            }
        }
        if let Some(existing) = self.folders.iter().find(|f| f.path == path) {
            self.selected_folder = Some(existing.id);
            return;
        }
        let id = FolderId(self.next_folder_id);
        self.next_folder_id += 1;
        self.folders.push(Folder {
            id,
            name,
            path,
            sample,
        });
        self.selected_folder = Some(id);
    }

    /// Whether any folder shown is sample data (the window then says so). It
    /// is false from the moment the first real folder is added or the last
    /// sample folder is removed.
    pub fn sample_data(&self) -> bool {
        self.folders.iter().any(|f| f.sample)
    }

    /// The folder currently selected, if any.
    pub fn selected(&self) -> Option<&Folder> {
        let id = self.selected_folder?;
        self.folders.iter().find(|f| f.id == id)
    }

    /// The count shown next to a sidebar row: only the four rows that have
    /// one, and only when it is above zero. An approvals list that is not
    /// known has no count: see [`ShellState::badge_unknown`].
    pub fn badge(&self, section: Section) -> Option<usize> {
        let count = match section {
            Section::Approvals => self.approvals_pending.shown().unwrap_or(0),
            Section::Automations => self.automations_on,
            Section::Discussions => self.discussions_need_you,
            Section::SkillsAndTools => self.skills_new,
            Section::NewTask | Section::Search => 0,
        };
        (count > 0).then_some(count)
    }
}

impl ShellState {
    /// The row has no count because it cannot be known now (approvals only):
    /// the sidebar shows a mark instead of nothing, which would read as zero.
    pub fn badge_unknown(&self, section: Section) -> bool {
        section == Section::Approvals && self.approvals_pending == Badge::Unknown
    }

    /// What the sidebar draws beside a row: the count in words, the unknown
    /// mark, or nothing.
    pub fn badge_text(&self, section: Section) -> Option<String> {
        if self.badge_unknown(section) {
            return Some(strings::BADGE_UNKNOWN.to_owned());
        }
        (self.badge(section)).map(|count| strings::badge_label(section, count))
    }
}

impl Default for ShellState {
    fn default() -> Self {
        ShellState::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn add(state: &mut ShellState, name: &str, path: &str) -> bool {
        state.apply(Action::AddFolder {
            name: name.to_owned(),
            path: path.to_owned(),
        })
    }

    #[test]
    fn new_shell_starts_on_new_task_connecting_to_this_computer() {
        let s = ShellState::new();
        assert_eq!(s.section, Section::NewTask);
        assert!(s.folders.is_empty());
        assert_eq!(s.connection.target, Target::ThisComputer);
        assert_eq!(s.connection.status, ConnectionStatus::Connecting);
    }

    #[test]
    fn placeholder_is_a_preview_with_flagged_sample_folders_and_claims_no_connection() {
        let s = ShellState::placeholder();
        assert_eq!(s.folders.len(), 3);
        assert_eq!(s.selected_folder, None);
        assert!(s.sample_data());
        assert!(s.folders.iter().all(|f| f.sample));
        assert_eq!(s.connection.status, ConnectionStatus::Preview);
        assert_ne!(s.connection.status, ConnectionStatus::Connected);
    }

    #[test]
    fn a_new_shell_has_no_sample_data() {
        assert!(!ShellState::new().sample_data());
    }

    #[test]
    fn the_first_real_folder_replaces_the_sample_folders() {
        let mut s = ShellState::placeholder();
        assert!(s.apply(Action::AddFolder {
            name: "Mine".to_owned(),
            path: "/mine".to_owned(),
        }));
        assert!(!s.sample_data());
        assert_eq!(s.folders.len(), 1);
        assert_eq!(s.folders[0].name, "Mine");
        assert!(!s.folders[0].sample);
        assert_eq!(s.selected().map(|f| f.name.as_str()), Some("Mine"));
    }

    #[test]
    fn removing_the_last_sample_folder_clears_the_sample_flag() {
        let mut s = ShellState::placeholder();
        let ids: Vec<_> = s.folders.iter().map(|f| f.id).collect();
        s.apply(Action::RemoveFolder(ids[0]));
        s.apply(Action::RemoveFolder(ids[1]));
        assert!(s.sample_data());
        s.apply(Action::RemoveFolder(ids[2]));
        assert!(!s.sample_data());
        assert!(s.folders.is_empty());
    }

    #[test]
    fn a_second_real_folder_keeps_the_first() {
        let mut s = ShellState::placeholder();
        for path in ["/a", "/b"] {
            s.apply(Action::AddFolder {
                name: path.to_owned(),
                path: path.to_owned(),
            });
        }
        assert_eq!(s.folders.len(), 2);
        assert!(!s.sample_data());
    }

    #[test]
    fn select_section_reports_change_only_once() {
        let mut s = ShellState::new();
        assert!(s.apply(Action::SelectSection(Section::Approvals)));
        assert_eq!(s.section, Section::Approvals);
        assert!(!s.apply(Action::SelectSection(Section::Approvals)));
    }

    #[test]
    fn new_task_goes_home_and_keeps_the_folder() {
        let mut s = ShellState::placeholder();
        let id = s.folders[0].id;
        s.apply(Action::SelectFolder(Some(id)));
        s.apply(Action::SelectSection(Section::Approvals));
        assert!(s.apply(Action::NewTask));
        assert_eq!(s.section, Section::NewTask);
        assert_eq!(s.selected_folder, Some(id));
        assert!(!s.apply(Action::NewTask));
    }

    #[test]
    fn add_folder_selects_it_and_ids_are_unique() {
        let mut s = ShellState::new();
        assert!(add(&mut s, "A", "/a"));
        assert!(add(&mut s, "B", "/b"));
        assert_ne!(s.folders[0].id, s.folders[1].id);
        assert_eq!(s.selected().map(|f| f.name.as_str()), Some("B"));
    }

    #[test]
    fn adding_the_same_path_selects_the_existing_folder() {
        let mut s = ShellState::new();
        add(&mut s, "A", "/a");
        add(&mut s, "B", "/b");
        add(&mut s, "A again", "/a");
        assert_eq!(s.folders.len(), 2);
        assert_eq!(s.selected().map(|f| f.name.as_str()), Some("A"));
    }

    #[test]
    fn select_unknown_folder_is_ignored() {
        let mut s = ShellState::new();
        add(&mut s, "A", "/a");
        assert!(!s.apply(Action::SelectFolder(Some(FolderId(99)))));
        assert!(s.selected().is_some());
        assert!(s.apply(Action::SelectFolder(None)));
        assert!(s.selected().is_none());
    }

    #[test]
    fn removing_the_selected_folder_clears_the_selection() {
        let mut s = ShellState::new();
        add(&mut s, "A", "/a");
        let id = s.folders[0].id;
        assert!(s.apply(Action::RemoveFolder(id)));
        assert!(s.folders.is_empty());
        assert_eq!(s.selected_folder, None);
    }

    #[test]
    fn removed_ids_are_not_reused() {
        let mut s = ShellState::new();
        add(&mut s, "A", "/a");
        let first = s.folders[0].id;
        s.apply(Action::RemoveFolder(first));
        add(&mut s, "B", "/b");
        assert_ne!(s.folders[0].id, first);
    }

    #[test]
    fn four_rows_have_a_count_and_show_it_only_above_zero() {
        let mut s = ShellState::new();
        let counted = [
            Section::Approvals,
            Section::Automations,
            Section::Discussions,
            Section::SkillsAndTools,
        ];
        for section in counted {
            assert_eq!(s.badge(section), None, "{section:?} at zero");
        }
        s.apply(Action::SetApprovalsPending(Badge::Count(2)));
        s.apply(Action::SetAutomationsOn(3));
        s.apply(Action::SetDiscussionsNeedYou(1));
        s.apply(Action::SetSkillsNew(4));
        assert_eq!(s.badge(Section::Approvals), Some(2));
        assert_eq!(s.badge(Section::Automations), Some(3));
        assert_eq!(s.badge(Section::Discussions), Some(1));
        assert_eq!(s.badge(Section::SkillsAndTools), Some(4));
        assert_eq!(s.badge(Section::NewTask), None);
        assert_eq!(s.badge(Section::Search), None);
        // Back to zero hides the count again.
        s.apply(Action::SetAutomationsOn(0));
        assert_eq!(s.badge(Section::Automations), None);
        assert!(!s.apply(Action::SetAutomationsOn(0)));
    }

    #[test]
    fn an_unknown_approvals_list_shows_a_mark_not_a_zero() {
        let mut s = ShellState::new();
        assert_eq!(s.badge_text(Section::Approvals), None, "a known zero");
        s.apply(Action::SetApprovalsPending(Badge::Unknown));
        assert_eq!(s.badge(Section::Approvals), None);
        assert!(s.badge_unknown(Section::Approvals));
        assert_eq!(s.badge_text(Section::Approvals).as_deref(), Some("?"));
        assert!(!s.badge_unknown(Section::Automations));
        s.apply(Action::SetApprovalsPending(Badge::Count(3)));
        assert_eq!(s.badge_text(Section::Approvals).as_deref(), Some("3"));
        s.apply(Action::SetAutomationsOn(2));
        assert_eq!(s.badge_text(Section::Automations).as_deref(), Some("2 on"));
    }

    #[test]
    fn a_preview_status_is_neutral_and_never_the_colour_of_connected() {
        assert_eq!(ConnectionStatus::Preview.tone(), Tone::Neutral);
        assert_ne!(
            ConnectionStatus::Preview.tone(),
            ConnectionStatus::Connected.tone()
        );
        let tones: Vec<_> = [
            ConnectionStatus::Preview,
            ConnectionStatus::Connecting,
            ConnectionStatus::Connected,
            ConnectionStatus::Unreachable,
        ]
        .map(ConnectionStatus::tone)
        .into();
        for (i, a) in tones.iter().enumerate() {
            assert!(tones[i + 1..].iter().all(|b| a != b));
        }
    }

    #[test]
    fn connection_status_and_sidebar_toggle() {
        let mut s = ShellState::new();
        assert!(s.apply(Action::SetConnectionStatus(ConnectionStatus::Unreachable)));
        assert_eq!(s.connection.status, ConnectionStatus::Unreachable);
        assert!(s.apply(Action::ToggleSidebar));
        assert!(s.sidebar_collapsed);
        assert!(s.apply(Action::ToggleSidebar));
        assert!(!s.sidebar_collapsed);
    }
}
