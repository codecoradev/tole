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
    /// yet committed). Compacts when due; reports what happened on stderr.
    pub fn before_turn(&mut self, s: &mut dyn Storage, p: &mut dyn Provider) {
        if !self.settings.enabled {
            return;
        }
        let Some(tokens) = compaction::last_prompt_tokens(s) else {
            return;
        };
        if self.settings.window.is_none() && tokens < DETECT_FLOOR {
            return;
        }
        let Some(window) = self.window() else {
            return;
        };
        let policy = Policy {
            window,
            at: self.settings.at,
            keep_turns: 1,
        };
        if !compaction::due(&policy, s) {
            return;
        }
        match compaction::compact(s, p, policy.keep_turns) {
            Ok(Some(c)) => eprintln!(
                "tole: context compacted: {} earlier entries summarized \
                 ({tokens} of {window} prompt tokens)",
                c.compacted_entries
            ),
            Ok(None) => {}
            Err(e) => eprintln!("tole: {e} (continuing uncompacted)"),
        }
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
