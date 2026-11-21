//! Credentials of remote servers, by reference (issue #1029, owner decision
//! D4): the model names a credential, a [`CredentialStore`] (the OS keychain
//! where there is one) holds the text.

use std::fmt;

/// A secret value. Not `Clone`, not `Display`, and `Debug` prints no text, so
/// it cannot reach a log, a screen or a copy of the state by accident.
pub struct Secret(String);

impl Secret {
    pub fn new(text: impl Into<String>) -> Self {
        Secret(text.into())
    }

    /// The text, for the one place that hands it to a [`CredentialStore`].
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

/// The name of a credential in a [`CredentialStore`]; opaque to the model.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CredentialRef(String);

impl CredentialRef {
    /// The name of the credential of remote entry `id`.
    pub(crate) fn for_remote(id: u32) -> Self {
        CredentialRef(format!("tole-desktop/remote/{id}"))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Where credentials are kept. The model only names them; an implementation
/// (the OS keychain, an owner decision) lives outside the model.
pub trait CredentialStore {
    fn put(&mut self, reference: &CredentialRef, secret: &Secret) -> Result<(), StoreError>;
    fn get(&self, reference: &CredentialRef) -> Result<Option<Secret>, StoreError>;
    fn delete(&mut self, reference: &CredentialRef) -> Result<(), StoreError>;
}

/// A credential store failed; the text says why and never holds a secret.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreError(pub String);

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[derive(Default)]
    struct Memory(HashMap<String, String>);

    impl CredentialStore for Memory {
        fn put(&mut self, r: &CredentialRef, s: &Secret) -> Result<(), StoreError> {
            self.0.insert(r.as_str().into(), s.expose().into());
            Ok(())
        }
        fn get(&self, r: &CredentialRef) -> Result<Option<Secret>, StoreError> {
            Ok(self.0.get(r.as_str()).map(Secret::new))
        }
        fn delete(&mut self, r: &CredentialRef) -> Result<(), StoreError> {
            self.0.remove(r.as_str());
            Ok(())
        }
    }

    #[test]
    fn a_secret_prints_no_text_and_a_store_gives_it_back_by_reference() {
        let secret = Secret::new("s3cr3t-credential-text");
        assert!(!format!("{secret:?}").contains("s3cr3t"));

        let reference = CredentialRef::for_remote(7);
        assert_eq!(reference.as_str(), "tole-desktop/remote/7");
        let mut store = Memory::default();
        store.put(&reference, &secret).unwrap();
        let back = store.get(&reference).unwrap().unwrap();
        assert_eq!(back.expose(), secret.expose());
        store.delete(&reference).unwrap();
        assert!(store.get(&reference).unwrap().is_none());
    }
}
