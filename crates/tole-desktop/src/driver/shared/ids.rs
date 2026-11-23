//! The ids of the serve's sessions, as every driver and the backend accept them.

/// The longest session id accepted (the core's own rule, as the model's).
pub(crate) const MAX_SESSION_ID_LEN: usize = 64;

/// Whether `id` is one the serve can have given: `[A-Za-z0-9_-]`, 1 to 64
/// bytes, as the model keeps them. An id with a control character or a
/// megabyte of text is never put in a URL; a huge one is refused by its
/// length, without being scanned.
pub(crate) fn is_session_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_SESSION_ID_LEN
        && (id.chars()).all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
}
