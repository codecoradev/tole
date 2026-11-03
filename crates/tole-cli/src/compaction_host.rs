//! Context compaction, host side (#440): when to compact and against which
//! context window, for the faces that chain turns (`mission`, `chat`,
//! `resume`). The mechanism itself is `tole_core::compaction`.
//!
//! The window comes from an explicit setting (`TOLE_CONTEXT_WINDOW` /
//! config `context_window`) when one is given, else from the provider's
//! `GET /models` catalog — fetched LAZILY, only once a prompt is already
//! large ([`DETECT_FLOOR`]), and at most once per process. Without a known
//! window compaction stays off. A failed compaction is reported and the
//! session goes on uncompacted: compaction is headroom, never a dependency.

use std::time::Duration;
use tole_core::compaction::{self, Policy};
use tole_core::openai::OpenAiConfig;
use tole_core::provider::Provider;
use tole_core::storage::Storage;

/// Below this prompt size (tokens) no window is looked up: no model in use
/// has a window this small, so a short session never pays for `/models`.
pub const DETECT_FLOOR: u64 = 8_000;

/// Timeout of the lazy `/models` lookup.
const DETECT_TIMEOUT: Duration = Duration::from_secs(5);

/// The resolved compaction settings (flags, env, config).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CompactionSettings {
    pub enabled: bool,
    pub at: f64,
    /// Explicit window (tokens); `None` = detect from `/models`.
    pub window: Option<u64>,
}

impl Default for CompactionSettings {
    fn default() -> Self {
        CompactionSettings {
            enabled: true,
            at: Policy::DEFAULT_AT,
            window: None,
        }
    }
}

/// Where a window comes from when none is configured.
pub trait WindowSource {
    /// The model's context window, or `None` when the catalog has none.
    fn window(&mut self) -> Option<u64>;
}

/// `GET /models` on the session's provider.
pub struct ModelsCatalog {
    base_url: String,
    api_key: String,
    model: String,
}

impl ModelsCatalog {
    pub fn new(cfg: &OpenAiConfig, model: &str) -> Self {
        ModelsCatalog {
            base_url: cfg.base_url.clone(),
            api_key: cfg.api_key.clone(),
            model: model.to_string(),
        }
    }
}

impl WindowSource for ModelsCatalog {
    fn window(&mut self) -> Option<u64> {
        match tole_core::openai::fetch_context_window(
            &self.base_url,
            &self.api_key,
            &self.model,
            DETECT_TIMEOUT,
        ) {
            Ok(w) => w,
            Err(e) => {
                eprintln!("tole: compaction: {e}");
                None
            }
        }
    }
}

/// Decides and runs compaction between turns.
pub struct Compactor<W: WindowSource> {
    settings: CompactionSettings,
    source: W,
    /// The detected window, once looked up (`Some(None)` = none reported).
    detected: Option<Option<u64>>,
}

impl<W: WindowSource> Compactor<W> {
    pub fn new(settings: CompactionSettings, source: W) -> Self {
        Compactor {
            settings,
            source,
            detected: None,
        }
    }

    /// The window in use, looking it up the first time it is needed.
    fn window(&mut self) -> Option<u64> {
        if let Some(w) = self.settings.window {
            return Some(w);
        }
        if self.detected.is_none() {
            let w = self.source.window();
            if w.is_none() {
                eprintln!(
                    "tole: compaction off: the provider's /models reports no context window \
                     for this model; set TOLE_CONTEXT_WINDOW (tokens) to enable it"
                );
            }
            self.detected = Some(w);
        }
        self.detected.flatten()
    }

    /// Call between turns (the previous turn settled, the next prompt not
    /// yet committed). Compacts when due — or when the session recorded a
    /// context overflow (#481), whatever the window says — and reports what
    /// happened on stderr.
    pub fn before_turn(&mut self, s: &mut dyn Storage, p: &mut dyn Provider) {
        if !self.settings.enabled {
            return;
        }
        let forced = compaction::overflow_pending(s);
        let trigger = if forced { None } else { self.due(s) };
        if !forced && trigger.is_none() {
            return;
        }
        match compaction::compact(s, p, 1) {
            Ok(Some(c)) => match trigger {
                Some((tokens, window)) => eprintln!(
                    "tole: context compacted: {} earlier entries summarized \
                     ({tokens} of {window} prompt tokens)",
                    c.compacted_entries
                ),
                None => eprintln!(
                    "tole: context compacted after a context-overflow error: {} earlier \
                     entries summarized",
                    c.compacted_entries
                ),
            },
            Ok(None) if forced => eprintln!(
                "tole: context overflow recorded but there is no earlier turn to summarize; \
                 start a new session or ask for less output per turn"
            ),
            Ok(None) => {}
            Err(e) => {
                eprintln!("tole: {e} (continuing uncompacted)");
                return;
            }
        }
        // The flag is spent by a pass that ran (or had nothing to do); a
        // failed summary call keeps it for the next attempt.
        if forced {
            if let Err(e) = compaction::clear_overflow(s) {
                eprintln!("tole: could not clear the context-overflow flag: {e}");
            }
        }
    }

    /// The ordinary trigger: `(prompt tokens, window)` when the last call
    /// reached the threshold and a settled turn can be summarized.
    fn due(&mut self, s: &dyn Storage) -> Option<(u64, u64)> {
        let tokens = compaction::last_prompt_tokens(s)?;
        if self.settings.window.is_none() && tokens < DETECT_FLOOR {
            return None;
        }
        let window = self.window()?;
        let policy = Policy {
            window,
            at: self.settings.at,
            keep_turns: 1,
        };
        compaction::due(&policy, s).then_some((tokens, window))
    }
}

/// The operator hint for a provider failure that is a context overflow
/// (#481); empty otherwise.
pub fn overflow_hint(message: &str) -> &'static str {
    if compaction::is_context_overflow(message) {
        " (compaction will run on the next resume)"
    } else {
        ""
    }
}

/// The compactor for a session: its window comes from the settings, else
/// from `/models` for the model the session actually uses (the pinned
/// `fact/model` register wins over the configured one, as for the
/// provider itself).
pub fn for_session(
    settings: CompactionSettings,
    cfg: &OpenAiConfig,
    storage: &impl Storage,
) -> Compactor<ModelsCatalog> {
    let model = crate::turn_runner::session_model(storage, &cfg.model);
    Compactor::new(settings, ModelsCatalog::new(cfg, &model))
}

/// `tole status` lines for a session's compactions (empty when none).
pub fn status_lines(entries: &[tole_core::entry::Entry]) -> Vec<String> {
    compaction::history(entries)
        .into_iter()
        .map(|(e, n, before)| {
            let tokens = before.map_or(String::new(), |t| format!(", {t} prompt tokens before"));
            format!(
                "compaction: {} — {n} earlier entries summarized before seq {}{tokens}",
                e.id, e.seq
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};
    use tole_core::entry::{Entry, EntryType, NewEntry};
    use tole_core::provider::{ProviderError, ProviderOutput};
    use tole_core::storage::{Commit, JsonlStorage, UsageRecord};

    struct Fixed(Option<u64>, u32);
    impl WindowSource for Fixed {
        fn window(&mut self) -> Option<u64> {
            self.1 += 1;
            self.0
        }
    }

    struct Summarizer(u32);
    impl Provider for Summarizer {
        fn complete(&mut self, _t: &[Entry]) -> Result<ProviderOutput, ProviderError> {
            self.0 += 1;
            Ok(ProviderOutput::Final {
                text: "SUMMARY".into(),
            })
        }
        fn last_usage(&self) -> Option<Value> {
            Some(json!({"prompt_tokens": 50_000}))
        }
    }

    fn session(prompts: &[u64]) -> JsonlStorage {
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let d = std::env::temp_dir().join(format!(
            "tole-compactor-{}-{}",
            std::process::id(),
            N.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&d).unwrap();
        let mut s = JsonlStorage::create(&d, "s", None).unwrap();
        for (i, t) in prompts.iter().enumerate() {
            let msg = |role: &str| {
                NewEntry::root(
                    EntryType::new(EntryType::MESSAGE),
                    json!({"role": role, "text": format!("{role}{i}")}),
                )
            };
            s.commit(Commit::new().entry(msg("user"))).unwrap();
            let out = s.commit(Commit::new().entry(msg("assistant"))).unwrap();
            s.commit(Commit::new().usage(UsageRecord {
                id: String::new(),
                entry_id: out[0].id.clone(),
                usage: json!({"prompt_tokens": t}),
                cost_usd: None,
            }))
            .unwrap();
        }
        s
    }

    fn compactions(s: &JsonlStorage) -> usize {
        compaction::history(s.entries()).len()
    }

    #[test]
    fn short_sessions_never_look_up_the_window() {
        let mut s = session(&[1_000, 7_999]);
        let mut c = Compactor::new(CompactionSettings::default(), Fixed(Some(8_000), 0));
        c.before_turn(&mut s, &mut Summarizer(0));
        assert_eq!(c.source.1, 0, "no /models call below the floor");
        assert_eq!(compactions(&s), 0);
    }

    #[test]
    fn a_detected_window_triggers_and_is_looked_up_once() {
        let mut s = session(&[40_000, 110_000]);
        let mut c = Compactor::new(CompactionSettings::default(), Fixed(Some(128_000), 0));
        let mut p = Summarizer(0);
        c.before_turn(&mut s, &mut p);
        assert_eq!(compactions(&s), 1);
        assert_eq!((c.source.1, p.0), (1, 1));
        // Right after: the summary call does not re-trigger, and the
        // window is not fetched again.
        c.before_turn(&mut s, &mut p);
        assert_eq!((compactions(&s), c.source.1, p.0), (1, 1, 1));
    }

    #[test]
    fn an_explicit_window_wins_and_no_window_means_off() {
        // Explicit 200k: 110k is below 80%, nothing happens, no lookup.
        let mut s = session(&[40_000, 110_000]);
        let explicit = CompactionSettings {
            window: Some(200_000),
            ..CompactionSettings::default()
        };
        let mut c = Compactor::new(explicit, Fixed(Some(128_000), 0));
        c.before_turn(&mut s, &mut Summarizer(0));
        assert_eq!((compactions(&s), c.source.1), (0, 0));
        // No window anywhere: off (looked up once, then remembered).
        let mut c = Compactor::new(CompactionSettings::default(), Fixed(None, 0));
        c.before_turn(&mut s, &mut Summarizer(0));
        c.before_turn(&mut s, &mut Summarizer(0));
        assert_eq!((compactions(&s), c.source.1), (0, 1));
        // Disabled: nothing at all.
        let off = CompactionSettings {
            enabled: false,
            ..CompactionSettings::default()
        };
        let mut c = Compactor::new(off, Fixed(Some(128_000), 0));
        c.before_turn(&mut s, &mut Summarizer(0));
        assert_eq!((compactions(&s), c.source.1), (0, 0));
    }

    /// #481: an overflow flag forces the pass whatever the window says
    /// (none known, far below the threshold), once, and is then cleared.
    #[test]
    fn an_overflow_flag_forces_one_compaction_and_clears() {
        let mut s = session(&[1_000, 2_000]);
        let mut c = Compactor::new(CompactionSettings::default(), Fixed(None, 0));
        c.before_turn(&mut s, &mut Summarizer(0));
        assert_eq!(compactions(&s), 0, "no flag, no window: nothing");
        compaction::mark_overflow(&mut s).unwrap();
        let mut p = Summarizer(0);
        c.before_turn(&mut s, &mut p);
        assert_eq!((compactions(&s), p.0), (1, 1));
        assert!(!compaction::overflow_pending(&s));
        assert_eq!(c.source.1, 0, "forced: the catalog is not consulted");
        c.before_turn(&mut s, &mut p);
        assert_eq!(compactions(&s), 1, "cleared: not forced again");
    }

    #[test]
    fn a_forced_pass_respects_off_and_keeps_the_flag_on_failure() {
        let mut s = session(&[1_000, 2_000]);
        compaction::mark_overflow(&mut s).unwrap();
        let off = CompactionSettings {
            enabled: false,
            ..CompactionSettings::default()
        };
        Compactor::new(off, Fixed(None, 0)).before_turn(&mut s, &mut Summarizer(0));
        assert_eq!(compactions(&s), 0);
        assert!(
            compaction::overflow_pending(&s),
            "--no-compaction: untouched"
        );
        // A failing summary call: the flag stays for the next attempt.
        struct Down;
        impl Provider for Down {
            fn complete(&mut self, _t: &[Entry]) -> Result<ProviderOutput, ProviderError> {
                Err(ProviderError("down".into()))
            }
        }
        let mut c = Compactor::new(CompactionSettings::default(), Fixed(None, 0));
        c.before_turn(&mut s, &mut Down);
        assert!(compaction::overflow_pending(&s));
        // One settled turn only: nothing to summarize, the flag is dropped.
        let mut one = session(&[1_000]);
        compaction::mark_overflow(&mut one).unwrap();
        c.before_turn(&mut one, &mut Summarizer(0));
        assert_eq!(compactions(&one), 0);
        assert!(!compaction::overflow_pending(&one));
    }

    #[test]
    fn status_reports_each_compaction() {
        let mut s = session(&[40_000, 120_000]);
        let mut c = Compactor::new(CompactionSettings::default(), Fixed(Some(128_000), 0));
        c.before_turn(&mut s, &mut Summarizer(0));
        let lines = status_lines(s.entries());
        assert_eq!(lines.len(), 1);
        assert!(
            lines[0].starts_with("compaction: compaction-1 — 2 earlier entries"),
            "{lines:?}"
        );
        assert!(
            lines[0].contains("120000 prompt tokens before"),
            "{lines:?}"
        );
    }
}
