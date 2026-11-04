//! Session status snapshot (#458): one source for "what is this session
//! running on and how full is it" — model, reasoning effort, context used
//! vs. window, token totals, compactions. A pure function of storage, the
//! resolved settings and an injected [`WindowSource`] (no network here).

use crate::compaction_host::{CompactionSettings, WindowSource};
use tole_core::compaction;
use tole_core::storage::Storage;

#[derive(Debug, Clone, PartialEq)]
pub struct SessionStatus {
    /// Pinned `fact/model`, else the configured model; `None` = neither.
    pub model: Option<String>,
    /// `fact/reasoning_effort` register (#457); `None` while unset.
    pub effort: Option<String>,
    /// Prompt tokens of the last call since the last compaction (0 = none yet).
    pub last_prompt_tokens: u64,
    /// Explicit window, else the source's; `None` = unknown.
    pub window: Option<u64>,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cached_tokens: u64,
    pub compactions: usize,
}

impl SessionStatus {
    /// `last_prompt_tokens / window`; `None` when the window is unknown.
    pub fn fraction_used(&self) -> Option<f64> {
        self.window
            .filter(|w| *w > 0)
            .map(|w| self.last_prompt_tokens as f64 / w as f64)
    }

    /// `context: 41k / 128k (32%)`, or `context: 41k / unknown`.
    pub fn context_line(&self) -> String {
        let used = short(self.last_prompt_tokens);
        match (self.window, self.fraction_used()) {
            // Integer half-up rounding: float `{:.0}` rounds halves to even.
            (Some(w), Some(_)) => format!(
                "context: {used} / {} ({}%)",
                short(w),
                (self.last_prompt_tokens * 100 + w / 2) / w
            ),
            _ => format!("context: {used} / unknown"),
        }
    }
}

impl SessionStatus {
    /// Human lines for `tole status`: model, effort, context, tokens, compactions.
    pub fn lines(&self) -> Vec<String> {
        vec![
            format!("model:   {}", self.model.as_deref().unwrap_or("-")),
            format!("effort:  {}", self.effort.as_deref().unwrap_or("-")),
            self.context_line(),
            format!(
                "tokens:  {} in / {} out / {} cached",
                self.input_tokens, self.output_tokens, self.cached_tokens
            ),
            format!("compactions: {}", self.compactions),
        ]
    }
}

/// `41_000` -> `41k`; below 1000 the plain number.
fn short(n: u64) -> String {
    if n < 1000 {
        n.to_string()
    } else {
        format!("{}k", (n + 500) / 1000)
    }
}

/// Snapshot a session. `default_model` is the configured model; the window
/// source is asked only when no window is configured AND the session has a
/// prompt to measure (a fresh session never triggers a lookup).
pub fn session_status(
    storage: &impl Storage,
    default_model: Option<&str>,
    settings: &CompactionSettings,
    source: &mut dyn WindowSource,
) -> SessionStatus {
    let pinned = crate::turn_runner::session_model(storage, "");
    let model = Some(pinned)
        .filter(|m| !m.is_empty())
        .or_else(|| default_model.map(str::to_string));
    let effort = storage
        .get_register("fact", "reasoning_effort")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|e| !e.is_empty())
        .map(str::to_string);
    let last = compaction::last_prompt_tokens(storage);
    let window = settings
        .window
        .or_else(|| last.and_then(|_| source.window()));
    let u = crate::usage_report::usage_report(storage.usages());
    SessionStatus {
        model,
        effort,
        last_prompt_tokens: last.unwrap_or(0),
        window,
        input_tokens: u.prompt_tokens,
        output_tokens: u.completion_tokens,
        cached_tokens: u.cached_tokens,
        compactions: compaction::history(storage.entries()).len(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tole_core::entry::{EntryType, NewEntry};
    use tole_core::register::RegisterWrite;
    use tole_core::storage::{Commit, JsonlStorage, UsageRecord};

    struct Fake(Option<u64>, u32);
    impl WindowSource for Fake {
        fn window(&mut self) -> Option<u64> {
            self.1 += 1;
            self.0
        }
    }

    fn store(tag: &str) -> JsonlStorage {
        let d = std::env::temp_dir().join(format!(
            "tole-status-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&d).unwrap();
        JsonlStorage::create(&d, "s", None).unwrap()
    }

    fn turn(s: &mut JsonlStorage, prompt: u64, completion: u64, cached: u64) {
        let msg = |role: &str| {
            NewEntry::root(
                EntryType::new(EntryType::MESSAGE),
                json!({"role": role, "text": role}),
            )
        };
        s.commit(Commit::new().entry(msg("user"))).unwrap();
        let out = s.commit(Commit::new().entry(msg("assistant"))).unwrap();
        s.commit(Commit::new().usage(UsageRecord {
            id: String::new(),
            entry_id: out[0].id.clone(),
            usage: json!({"prompt_tokens": prompt, "completion_tokens": completion,
                "prompt_tokens_details": {"cached_tokens": cached}}),
            cost_usd: None,
        }))
        .unwrap();
    }

    #[test]
    fn fresh_session_is_zeros_and_never_looks_up_a_window() {
        let s = store("fresh");
        let mut src = Fake(Some(128_000), 0);
        let st = session_status(&s, Some("m1"), &CompactionSettings::default(), &mut src);
        assert_eq!(st.model.as_deref(), Some("m1"));
        assert_eq!(st.effort, None);
        assert_eq!(
            (st.last_prompt_tokens, st.input_tokens, st.compactions),
            (0, 0, 0)
        );
        assert_eq!((st.window, src.1), (None, 0));
        assert_eq!(st.context_line(), "context: 0 / unknown");
    }

    #[test]
    fn resumed_session_reports_context_totals_and_detected_window() {
        let mut s = store("resumed");
        turn(&mut s, 30_000, 500, 10_000);
        turn(&mut s, 41_000, 700, 20_000);
        let mut src = Fake(Some(128_000), 0);
        let st = session_status(&s, None, &CompactionSettings::default(), &mut src);
        assert_eq!(st.model, None);
        assert_eq!(st.last_prompt_tokens, 41_000);
        assert_eq!((st.input_tokens, st.output_tokens), (71_000, 1_200));
        assert_eq!(st.cached_tokens, 30_000);
        assert_eq!(st.context_line(), "context: 41k / 128k (32%)");
        assert_eq!(src.1, 1);
    }

    #[test]
    fn explicit_window_wins_and_skips_the_source() {
        let mut s = store("explicit");
        turn(&mut s, 41_000, 1, 0);
        let settings = CompactionSettings {
            window: Some(200_000),
            ..CompactionSettings::default()
        };
        let mut src = Fake(Some(128_000), 0);
        let st = session_status(&s, None, &settings, &mut src);
        assert_eq!((st.window, src.1), (Some(200_000), 0));
        assert_eq!(st.context_line(), "context: 41k / 200k (21%)");
    }

    #[test]
    fn lines_render_dash_for_unset_model_and_effort() {
        let st = session_status(
            &store("lines"),
            None,
            &CompactionSettings::default(),
            &mut Fake(None, 0),
        );
        let l = st.lines();
        assert_eq!(l[0], "model:   -");
        assert_eq!(l[1], "effort:  -");
        assert_eq!(l[2], "context: 0 / unknown");
        assert_eq!(l[3], "tokens:  0 in / 0 out / 0 cached");
        assert_eq!(l[4], "compactions: 0");
    }

    #[test]
    fn unknown_window_is_said_honestly() {
        let mut s = store("unknown");
        turn(&mut s, 41_000, 1, 0);
        let st = session_status(&s, None, &CompactionSettings::default(), &mut Fake(None, 0));
        assert_eq!(st.fraction_used(), None);
        assert_eq!(st.context_line(), "context: 41k / unknown");
    }

    #[test]
    fn pinned_model_and_effort_registers_are_read() {
        let mut s = store("regs");
        s.commit(
            Commit::new()
                .register(RegisterWrite::set("fact", "model", json!("pinned-m")))
                .register(RegisterWrite::set(
                    "fact",
                    "reasoning_effort",
                    json!("high"),
                )),
        )
        .unwrap();
        let st = session_status(
            &s,
            Some("cfg"),
            &CompactionSettings::default(),
            &mut Fake(None, 0),
        );
        assert_eq!(st.model.as_deref(), Some("pinned-m"));
        assert_eq!(st.effort.as_deref(), Some("high"));
    }
}
