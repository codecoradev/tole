//! The servers the desktop can talk to (issue #1029, owner decisions D3, D4,
//! D6 in `docs/desktop-design.md` section 0).
//!
//! There is one built-in connection, "This computer": the `tole serve` the
//! desktop starts or attaches to. Remote servers (a VPS, #1044) are prepared
//! but sit behind a setting that is off: while it is off no remote entry is
//! listed, selected or saved.
//!
//! A remote entry holds a name, a validated address ([`Endpoint`]) and, when
//! it needs one, a [`CredentialRef`]: the name under which the credential lives
//! in a [`CredentialStore`] (the OS keychain where there is one). The secret
//! text is never a field of the entry or of [`Connections`]: saving takes it in
//! a [`Secret`], and hands it back in [`SavedRemote`] for the caller to put in
//! the store, so nothing in the model can print, copy or persist it.

use crate::{CredentialRef, Endpoint, EndpointError, Secret};

/// At most this many remote entries; a bound, not a feature.
pub const MAX_REMOTES: usize = 32;
/// Longest name of an entry, in characters.
pub const MAX_NAME_CHARS: usize = 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RemoteId(u32);

/// Which connection is in use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ConnectionId {
    ThisComputer,
    Remote(RemoteId),
}

/// A saved remote server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteEntry {
    pub id: RemoteId,
    pub name: String,
    pub endpoint: Endpoint,
    /// The owner marked this server as a trusted private network (D6): plain
    /// `http://` is then accepted for it (never for a public IP address).
    pub trusted_private_network: bool,
    pub credential: Option<CredentialRef>,
}

/// What the owner typed in the form of a remote server.
#[derive(Debug)]
pub struct RemoteDraft {
    pub name: String,
    pub url: String,
    pub trusted_private_network: bool,
    /// The credential text. On an update, `None` keeps the stored credential.
    pub credential: Option<Secret>,
}

/// The secret of a saved entry, to be put in the store under `reference`. It
/// is returned, not kept: the model forgets the text once the caller has it.
#[derive(Debug)]
pub struct SavedRemote {
    pub id: RemoteId,
    pub store: Option<(CredentialRef, Secret)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectionsError {
    /// The remote setting is off.
    RemoteDisabled,
    NameEmpty,
    NameTooLong,
    NameTaken,
    TooMany,
    UnknownRemote,
    Endpoint(EndpointError),
}

impl From<EndpointError> for ConnectionsError {
    fn from(e: EndpointError) -> Self {
        ConnectionsError::Endpoint(e)
    }
}

/// The connections and which one is selected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Connections {
    remote_enabled: bool,
    remotes: Vec<RemoteEntry>,
    selected: ConnectionId,
    next_id: u32,
}

impl Default for Connections {
    fn default() -> Self {
        Self::new()
    }
}

impl Connections {
    /// "This computer" selected, remote servers off.
    pub fn new() -> Self {
        Connections {
            remote_enabled: false,
            remotes: Vec::new(),
            selected: ConnectionId::ThisComputer,
            next_id: 1,
        }
    }

    pub fn remote_enabled(&self) -> bool {
        self.remote_enabled
    }

    /// Turn the remote setting on or off. Off hides every remote entry and
    /// selects "This computer" again; the entries are kept.
    pub fn set_remote_enabled(&mut self, on: bool) {
        self.remote_enabled = on;
        if !on {
            self.selected = ConnectionId::ThisComputer;
        }
    }

    pub fn selected(&self) -> ConnectionId {
        self.selected
    }

    /// The remote entries the window may list: none while the setting is off.
    pub fn remotes(&self) -> &[RemoteEntry] {
        if self.remote_enabled {
            &self.remotes
        } else {
            &[]
        }
    }

    pub fn remote(&self, id: RemoteId) -> Option<&RemoteEntry> {
        self.remotes().iter().find(|r| r.id == id)
    }

    /// Select a connection. A remote that is unknown, or any remote while the
    /// setting is off, is refused.
    pub fn select(&mut self, id: ConnectionId) -> Result<(), ConnectionsError> {
        if let ConnectionId::Remote(r) = id {
            self.require_enabled()?;
            self.remote(r).ok_or(ConnectionsError::UnknownRemote)?;
        }
        self.selected = id;
        Ok(())
    }

    /// Save a new remote server. The address is checked under the D6 rule: the
    /// entry is refused over plain `http://` unless the host is loopback or the
    /// owner marked it as a trusted private network.
    pub fn save_remote(&mut self, draft: RemoteDraft) -> Result<SavedRemote, ConnectionsError> {
        self.require_enabled()?;
        if self.remotes.len() >= MAX_REMOTES {
            return Err(ConnectionsError::TooMany);
        }
        let name = self.checked_name(&draft.name, None)?;
        let endpoint = Endpoint::parse(&draft.url, draft.trusted_private_network)?;
        let id = RemoteId(self.next_id);
        self.next_id += 1;
        let reference = draft
            .credential
            .as_ref()
            .map(|_| CredentialRef::for_remote(id.0));
        self.remotes.push(RemoteEntry {
            id,
            name,
            endpoint,
            trusted_private_network: draft.trusted_private_network,
            credential: reference.clone(),
        });
        Ok(SavedRemote {
            id,
            store: reference.zip(draft.credential),
        })
    }

    /// Change a saved entry. The address is validated again, so turning the
    /// trusted mark off for a server that is reached over plain `http://` is
    /// refused and the entry stays as it was. A draft with no credential keeps
    /// the stored one; a new one is returned to be put in the store under the
    /// same reference.
    pub fn update_remote(
        &mut self,
        id: RemoteId,
        draft: RemoteDraft,
    ) -> Result<SavedRemote, ConnectionsError> {
        self.require_enabled()?;
        let index = self
            .remotes
            .iter()
            .position(|r| r.id == id)
            .ok_or(ConnectionsError::UnknownRemote)?;
        let name = self.checked_name(&draft.name, Some(id))?;
        let endpoint = Endpoint::parse(&draft.url, draft.trusted_private_network)?;
        let entry = &mut self.remotes[index];
        if draft.credential.is_some() && entry.credential.is_none() {
            entry.credential = Some(CredentialRef::for_remote(id.0));
        }
        entry.name = name;
        entry.endpoint = endpoint;
        entry.trusted_private_network = draft.trusted_private_network;
        let store = entry.credential.clone().zip(draft.credential);
        Ok(SavedRemote { id, store })
    }

    /// Remove an entry. The reference of its credential is returned so the
    /// caller can delete it from the store. Selects "This computer" if the
    /// entry was selected.
    pub fn remove_remote(
        &mut self,
        id: RemoteId,
    ) -> Result<Option<CredentialRef>, ConnectionsError> {
        self.require_enabled()?;
        let index = self
            .remotes
            .iter()
            .position(|r| r.id == id)
            .ok_or(ConnectionsError::UnknownRemote)?;
        let entry = self.remotes.remove(index);
        if self.selected == ConnectionId::Remote(id) {
            self.selected = ConnectionId::ThisComputer;
        }
        Ok(entry.credential)
    }

    fn require_enabled(&self) -> Result<(), ConnectionsError> {
        if self.remote_enabled {
            Ok(())
        } else {
            Err(ConnectionsError::RemoteDisabled)
        }
    }

    fn checked_name(&self, raw: &str, own: Option<RemoteId>) -> Result<String, ConnectionsError> {
        let name = raw.trim();
        if name.is_empty() {
            return Err(ConnectionsError::NameEmpty);
        }
        if name.chars().count() > MAX_NAME_CHARS || name.chars().any(char::is_control) {
            return Err(ConnectionsError::NameTooLong);
        }
        let taken = self
            .remotes
            .iter()
            .any(|r| Some(r.id) != own && r.name.to_lowercase() == name.to_lowercase());
        if taken {
            return Err(ConnectionsError::NameTaken);
        }
        Ok(name.to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PlainHttp;

    const SECRET: &str = "s3cr3t-credential-text";

    fn draft(name: &str, url: &str, trusted: bool, secret: Option<&str>) -> RemoteDraft {
        RemoteDraft {
            name: name.into(),
            url: url.into(),
            trusted_private_network: trusted,
            credential: secret.map(Secret::new),
        }
    }

    fn enabled() -> Connections {
        let mut c = Connections::new();
        c.set_remote_enabled(true);
        c
    }

    #[test]
    fn this_computer_is_the_only_connection_and_remote_is_off() {
        let c = Connections::new();
        assert!(!c.remote_enabled());
        assert_eq!(c.selected(), ConnectionId::ThisComputer);
        assert!(c.remotes().is_empty());
    }

    #[test]
    fn nothing_remote_can_be_saved_or_selected_while_the_setting_is_off() {
        let mut c = Connections::new();
        let err = c
            .save_remote(draft("VPS", "https://vps.example.com", false, None))
            .unwrap_err();
        assert_eq!(err, ConnectionsError::RemoteDisabled);

        let mut c = enabled();
        let id = c
            .save_remote(draft("VPS", "https://vps.example.com", false, None))
            .unwrap()
            .id;
        c.select(ConnectionId::Remote(id)).unwrap();
        c.set_remote_enabled(false);
        assert_eq!(c.selected(), ConnectionId::ThisComputer);
        assert!(c.remotes().is_empty(), "entries are hidden, not listed");
        assert_eq!(
            c.select(ConnectionId::Remote(id)),
            Err(ConnectionsError::RemoteDisabled)
        );
        c.set_remote_enabled(true);
        assert_eq!(c.remotes().len(), 1, "the entry was kept");
    }

    #[test]
    fn an_https_entry_is_saved_and_a_plain_http_one_is_refused_unless_trusted() {
        let mut c = enabled();
        assert!(c
            .save_remote(draft("VPS", "https://vps.example.com", false, None))
            .is_ok());
        let err = c
            .save_remote(draft("Box", "http://192.168.1.20:7801", false, None))
            .unwrap_err();
        assert!(matches!(
            err,
            ConnectionsError::Endpoint(EndpointError::PlainHttp {
                why: PlainHttp::NotTrusted,
                ..
            })
        ));
        assert_eq!(c.remotes().len(), 1, "a refused entry is not stored");

        let id = c
            .save_remote(draft("Box", "http://192.168.1.20:7801", true, None))
            .unwrap()
            .id;
        assert!(c.remote(id).unwrap().trusted_private_network);
        // Loopback needs no mark.
        assert!(c
            .save_remote(draft("Tunnel", "http://127.0.0.1:7801", false, None))
            .is_ok());
        // A public address is refused even with the mark.
        let err = c
            .save_remote(draft("Open", "http://203.0.113.9:7801", true, None))
            .unwrap_err();
        assert!(matches!(
            err,
            ConnectionsError::Endpoint(EndpointError::PlainHttp {
                why: PlainHttp::PublicAddress,
                ..
            })
        ));
    }

    #[test]
    fn the_credential_text_is_handed_back_and_never_kept_in_the_model() {
        let mut c = enabled();
        let saved = c
            .save_remote(draft("VPS", "https://vps.example.com", false, Some(SECRET)))
            .unwrap();
        let (reference, secret) = saved.store.unwrap();
        assert_eq!(secret.expose(), SECRET);

        // The entry holds the reference only, and nothing printable has the text.
        let entry = c.remote(saved.id).unwrap();
        assert_eq!(entry.credential.as_ref(), Some(&reference));
        assert!(!reference.as_str().contains(SECRET));
        let printed = format!("{c:?} {entry:?} {reference:?} {secret:?}");
        assert!(!printed.contains(SECRET), "{printed}");
    }

    #[test]
    fn an_entry_without_a_credential_has_no_reference() {
        let mut c = enabled();
        let saved = c
            .save_remote(draft("VPS", "https://vps.example.com", false, None))
            .unwrap();
        assert!(saved.store.is_none());
        assert!(c.remote(saved.id).unwrap().credential.is_none());
    }

    #[test]
    fn an_update_keeps_the_stored_credential_unless_a_new_one_is_given() {
        let mut c = enabled();
        let first = c
            .save_remote(draft("VPS", "https://vps.example.com", false, Some(SECRET)))
            .unwrap();
        let reference = first.store.unwrap().0;

        let kept = c
            .update_remote(
                first.id,
                draft("VPS 2", "https://vps2.example.com", false, None),
            )
            .unwrap();
        assert!(kept.store.is_none(), "no new secret to store");
        let entry = c.remote(first.id).unwrap();
        assert_eq!(entry.name, "VPS 2");
        assert_eq!(entry.credential.as_ref(), Some(&reference));

        let changed = c
            .update_remote(
                first.id,
                draft("VPS 2", "https://vps2.example.com", false, Some("new-text")),
            )
            .unwrap();
        assert_eq!(
            changed.store.unwrap().0,
            reference,
            "same reference, new text"
        );
    }

    #[test]
    fn an_update_that_would_make_the_address_unsafe_is_refused_and_changes_nothing() {
        let mut c = enabled();
        let id = c
            .save_remote(draft("Box", "http://192.168.1.20:7801", true, None))
            .unwrap()
            .id;
        let before = c.clone();
        let err = c
            .update_remote(id, draft("Box", "http://192.168.1.20:7801", false, None))
            .unwrap_err();
        assert!(matches!(err, ConnectionsError::Endpoint(_)));
        assert_eq!(c, before);
    }

    #[test]
    fn names_are_checked() {
        let mut c = enabled();
        c.save_remote(draft("VPS", "https://a.example.com", false, None))
            .unwrap();
        let cases = [
            ("  ", ConnectionsError::NameEmpty),
            ("vps", ConnectionsError::NameTaken),
            (
                &"x".repeat(MAX_NAME_CHARS + 1),
                ConnectionsError::NameTooLong,
            ),
            ("a\nb", ConnectionsError::NameTooLong),
        ];
        for (name, want) in cases {
            let err = c
                .save_remote(draft(name, "https://b.example.com", false, None))
                .unwrap_err();
            assert_eq!(err, want, "{name:?}");
        }
        // Renaming an entry to its own name is not a clash.
        let id = c.remotes()[0].id;
        assert!(c
            .update_remote(id, draft("VPS", "https://a.example.com", false, None))
            .is_ok());
    }

    #[test]
    fn removing_an_entry_returns_its_reference_and_falls_back_to_this_computer() {
        let mut c = enabled();
        let saved = c
            .save_remote(draft("VPS", "https://vps.example.com", false, Some(SECRET)))
            .unwrap();
        let reference = saved.store.unwrap().0;
        c.select(ConnectionId::Remote(saved.id)).unwrap();
        let gone = c.remove_remote(saved.id).unwrap();
        assert_eq!(gone, Some(reference));
        assert_eq!(c.selected(), ConnectionId::ThisComputer);
        assert_eq!(
            c.remove_remote(saved.id),
            Err(ConnectionsError::UnknownRemote)
        );
    }

    #[test]
    fn the_number_of_entries_is_bounded() {
        let mut c = enabled();
        for i in 0..MAX_REMOTES {
            c.save_remote(draft(
                &format!("s{i}"),
                "https://a.example.com",
                false,
                None,
            ))
            .unwrap();
        }
        let err = c
            .save_remote(draft("one more", "https://a.example.com", false, None))
            .unwrap_err();
        assert_eq!(err, ConnectionsError::TooMany);
    }
}
