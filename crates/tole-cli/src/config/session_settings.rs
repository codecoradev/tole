//! A session's durable settings (#525): the registers that make a session
//! keep its model, reasoning effort and name across faces and resumes, and
//! the forced-compaction request.
//!
//! One place owns the register names and the writers, so the TUI's `/model`,
//! `/effort`, `/name` and `/compact`, and ACP's model picker, write the same
//! registers the same way, and the readers (`turn_runner::session_model`,
//! `session_effort`, `status`) read what they wrote. Settings are append-only
//! register entries, like everything else in the log: a change is a new entry,
//! never an edit.

use tole_core::register::RegisterWrite;
use tole_core::storage::{Commit, Storage, StorageError};

/// The session's model override (`fact/model`).
pub const MODEL: (&str, &str) = ("fact", "model");
/// The session's reasoning-effort override (`fact/reasoning_effort`).
pub const EFFORT: (&str, &str) = ("fact", "reasoning_effort");
/// The session's name (`fact/session_name`); wins over the header alias.
pub const NAME: (&str, &str) = ("fact", "session_name");

/// A model id or session name that is safe to store and show: non-blank,
/// bounded, no control characters.
pub fn clean_label(what: &str, raw: &str) -> Result<String, String> {
    let v = raw.trim();
    if v.is_empty() || v.chars().count() > 120 || v.chars().any(char::is_control) {
        return Err(format!("invalid {what}"));
    }
    Ok(v.to_string())
}

fn commit(s: &mut dyn Storage, w: RegisterWrite) -> Result<(), StorageError> {
    s.commit(Commit::new().register(w)).map(|_| ())
}

/// Store `id` as the session's model, as given (the caller validated it:
/// the TUI with [`set_model`], ACP against its model list).
pub fn write_model(s: &mut dyn Storage, id: &str) -> Result<(), StorageError> {
    commit(
        s,
        RegisterWrite::set(MODEL.0, MODEL.1, serde_json::json!(id)),
    )
}

/// The TUI's model switch: validate the label, then store it.
pub fn set_model(s: &mut dyn Storage, id: &str) -> Result<(), String> {
    let id = clean_label("model id", id)?;
    write_model(s, &id).map_err(|e| format!("storage error: {e}"))
}

/// Set the reasoning effort (`Some`, validated) or clear the override
/// (`None`: the session goes back to the process default).
pub fn set_effort(s: &mut dyn Storage, level: Option<&str>) -> Result<(), String> {
    let write = match level {
        Some(l) => {
            let l = tole_core::openai::parse_reasoning_effort(l)?;
            RegisterWrite::set(EFFORT.0, EFFORT.1, serde_json::json!(l))
        }
        None => RegisterWrite::delete(EFFORT.0, EFFORT.1),
    };
    commit(s, write).map_err(|e| format!("storage error: {e}"))
}

/// Rename the session (the header alias stays as history; the register is the
/// rename).
pub fn set_name(s: &mut dyn Storage, name: &str) -> Result<(), String> {
    let n = clean_label("name", name)?;
    commit(s, RegisterWrite::set(NAME.0, NAME.1, serde_json::json!(n)))
        .map_err(|e| format!("storage error: {e}"))
}

/// The forced pass of #481: the next prompt compacts first (when compaction
/// is enabled). Not a mid-turn compaction.
pub fn request_compaction(s: &mut dyn Storage) -> Result<(), String> {
    tole_core::compaction::mark_overflow(s).map_err(|e| format!("storage error: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tole_core::storage::JsonlStorage;

    fn storage(tag: &str) -> (JsonlStorage, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "tole-settings-{tag}-{}-{:x}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        (JsonlStorage::create(&dir, "s", None).unwrap(), dir)
    }

    #[test]
    fn labels_are_trimmed_bounded_and_free_of_control_characters() {
        assert_eq!(clean_label("name", "  fix parser  ").unwrap(), "fix parser");
        for bad in ["", "   ", "a\nb", "a\u{7}b", &"x".repeat(121)] {
            assert_eq!(
                clean_label("name", bad),
                Err("invalid name".to_string()),
                "{bad:?}"
            );
        }
        assert!(
            clean_label("name", &"x".repeat(120)).is_ok(),
            "120 chars is the limit"
        );
    }

    #[test]
    fn the_model_is_stored_where_the_readers_look_and_bad_ids_write_nothing() {
        let (mut s, dir) = storage("model");
        set_model(&mut s, "  gpt-x  ").unwrap();
        assert_eq!(crate::turn_runner::session_model(&s, "default"), "gpt-x");
        let before = s.entries().len() + s.last_seq() as usize;
        assert!(set_model(&mut s, "bad\nid").is_err());
        assert_eq!(
            s.last_seq() as usize + s.entries().len(),
            before,
            "nothing written"
        );
        // ACP validates against its own model list, then writes as given.
        write_model(&mut s, "from-the-list").unwrap();
        assert_eq!(
            crate::turn_runner::session_model(&s, "default"),
            "from-the-list"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn effort_is_validated_canonicalised_and_clearable() {
        let (mut s, dir) = storage("effort");
        assert_eq!(
            crate::turn_runner::session_effort(&s, Some("low")),
            Some("low".into())
        );
        set_effort(&mut s, Some(" HIGH ")).unwrap();
        assert_eq!(
            crate::turn_runner::session_effort(&s, Some("low")),
            Some("high".into())
        );
        let err = set_effort(&mut s, Some("ultra")).unwrap_err();
        assert!(err.contains("invalid reasoning effort"), "{err}");
        assert_eq!(
            crate::turn_runner::session_effort(&s, Some("low")),
            Some("high".into()),
            "a rejected level changes nothing"
        );
        set_effort(&mut s, None).unwrap();
        assert_eq!(
            crate::turn_runner::session_effort(&s, Some("low")),
            Some("low".into()),
            "cleared: back to the process default"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn the_name_register_renames_and_a_bad_name_is_refused() {
        let (mut s, dir) = storage("name");
        assert_eq!(s.session_name(), None);
        set_name(&mut s, " release notes ").unwrap();
        assert_eq!(s.session_name(), Some("release notes"));
        assert!(set_name(&mut s, "").is_err());
        assert_eq!(s.session_name(), Some("release notes"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_compaction_request_is_a_durable_pending_flag() {
        let (mut s, dir) = storage("compact");
        assert!(!tole_core::compaction::overflow_pending(&s));
        request_compaction(&mut s).unwrap();
        assert!(tole_core::compaction::overflow_pending(&s));
        let _ = std::fs::remove_dir_all(dir);
    }
}
