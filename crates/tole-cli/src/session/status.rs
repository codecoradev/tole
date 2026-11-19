//! Session status snapshot (#458): one source for "what is this session
//! running on and how full is it" — model, reasoning effort, context used
//! vs. window, token totals, compactions. A pure function of storage, the
//! resolved settings and an injected [`WindowSource`] (no network here).

use crate::compaction_host::{CompactionSettings, ModelsCatalog, WindowSource};
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
        match self.window.filter(|w| *w > 0) {
            // Integer half-up rounding: float `{:.0}` rounds halves to even.
            Some(w) => format!(
                "context: {used} / {} ({}%)",
                short(w),
                (self.last_prompt_tokens * 100 + w / 2) / w
            ),
            None => format!("context: {used} / unknown"),
        }
    }

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
        .get_register(
            crate::session_settings::EFFORT.0,
            crate::session_settings::EFFORT.1,
        )
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

/// The context window the owner configured (`TOLE_CONTEXT_WINDOW` / config
/// `context_window`), fixed once for a server process so every session's
/// snapshot resolves the window the way `tole status` does.
static CONFIGURED_WINDOW: std::sync::OnceLock<Option<u64>> = std::sync::OnceLock::new();

/// Fix the configured window for server sessions (first call wins).
pub fn set_configured_window(window: Option<u64>) {
    let _ = CONFIGURED_WINDOW.set(window);
}

fn configured_window() -> Option<u64> {
    CONFIGURED_WINDOW.get().copied().flatten()
}

/// A session's status as it was at one moment (#573): the `tole status`
/// snapshot plus the session name, stamped with the time it was taken.
#[derive(Debug, Clone, PartialEq)]
pub struct StatusSnapshot {
    pub status: SessionStatus,
    /// `fact/session_name` register, else the header alias.
    pub name: Option<String>,
    /// Epoch milliseconds when the snapshot was taken.
    pub at_ms: u64,
}

impl StatusSnapshot {
    /// The additive fields of `GET /sessions/{id}/status`. Every unknown
    /// value is `null`, never a made-up one. `stale` says the values come
    /// from the last-known snapshot (a turn holds the session), not a live
    /// read; `snapshotAt` is when they were read.
    pub fn json_fields(snapshot: Option<&Self>, stale: bool) -> serde_json::Value {
        use serde_json::json;
        match snapshot {
            Some(s) => json!({
                "model": s.status.model,
                "effort": s.status.effort,
                "contextWindow": s.status.window,
                "contextUsed": s.status.last_prompt_tokens,
                "name": s.name,
                "snapshotAt": s.at_ms,
                "stale": stale,
            }),
            None => json!({
                "model": null, "effort": null, "contextWindow": null,
                "contextUsed": null, "name": null, "snapshotAt": null,
                "stale": stale,
            }),
        }
    }
}

/// What a live session remembers for the status route: the last snapshot
/// and the window the provider's `/models` reported for a model (asked at
/// most once per model, never from the route).
#[derive(Debug, Default)]
pub struct StatusCell {
    snapshot: Option<StatusSnapshot>,
    window: Option<(String, u64)>,
}

impl StatusCell {
    /// The detected window, when it was detected for this very model.
    pub fn window_for(&self, model: &str) -> Option<u64> {
        self.window
            .as_ref()
            .filter(|(m, _)| m == model)
            .map(|(_, w)| *w)
    }

    pub fn set_window(&mut self, model: &str, window: u64) {
        self.window = Some((model.to_string(), window));
    }

    pub fn snapshot(&self) -> Option<&StatusSnapshot> {
        self.snapshot.as_ref()
    }

    pub fn store(&mut self, s: StatusSnapshot) {
        self.snapshot = Some(s);
    }
}

/// Window source that never touches the network: the configured window, or
/// what an earlier lookup cached.
struct Known(Option<u64>);
impl WindowSource for Known {
    fn window(&mut self) -> Option<u64> {
        self.0
    }
}

/// Take a snapshot of `storage` without any network call: the window is the
/// configured one or `cached`. `default_model` is the configured model.
pub fn take_snapshot(
    storage: &impl Storage,
    default_model: Option<&str>,
    cached: &StatusCell,
) -> StatusSnapshot {
    let model = crate::turn_runner::session_model(storage, default_model.unwrap_or(""));
    let settings = CompactionSettings {
        window: configured_window(),
        ..CompactionSettings::default()
    };
    let status = session_status(
        storage,
        default_model,
        &settings,
        &mut Known(cached.window_for(&model)),
    );
    StatusSnapshot {
        status,
        name: storage.session_name().map(str::to_string),
        at_ms: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0),
    }
}

/// Look up the model's window on the provider (network, bounded by the
/// catalog timeout) when it is needed and not known yet; `true` = the cell
/// gained a window. Call WITHOUT holding any session lock.
pub fn detect_window(
    cell: &std::sync::Mutex<StatusCell>,
    cfg: &tole_core::openai::OpenAiConfig,
    model: &str,
) -> bool {
    if configured_window().is_some()
        || cell
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .window_for(model)
            .is_some()
    {
        return false;
    }
    match ModelsCatalog::new(cfg, model).window() {
        Some(w) => {
            cell.lock()
                .unwrap_or_else(|p| p.into_inner())
                .set_window(model, w);
            true
        }
        None => false,
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
        let d = crate::test_dir::TestDir::new(&format!("tole-status-{tag}"));
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

    #[test]
    fn snapshot_fields_are_null_when_unknown_and_flag_staleness() {
        let none = StatusSnapshot::json_fields(None, true);
        for k in [
            "model",
            "effort",
            "contextWindow",
            "contextUsed",
            "name",
            "snapshotAt",
        ] {
            assert_eq!(none[k], json!(null), "{k}");
        }
        assert_eq!(none["stale"], json!(true));
        let snap = take_snapshot(&store("snap"), None, &StatusCell::default());
        let f = StatusSnapshot::json_fields(Some(&snap), false);
        assert_eq!(f["model"], json!(null));
        assert_eq!(f["contextWindow"], json!(null), "no window detected");
        assert_eq!(f["name"], json!(null));
        assert_eq!(f["stale"], json!(false));
        assert!(f["snapshotAt"].as_u64().unwrap() > 0);
    }

    #[test]
    fn snapshot_uses_the_cached_window_only_for_its_model() {
        let mut s = store("cached");
        turn(&mut s, 41_000, 1, 0);
        s.commit(Commit::new().register(RegisterWrite::set("fact", "model", json!("m1"))))
            .unwrap();
        let mut cell = StatusCell::default();
        cell.set_window("m1", 128_000);
        let f = StatusSnapshot::json_fields(Some(&take_snapshot(&s, None, &cell)), false);
        assert_eq!(
            (f["contextWindow"].clone(), f["contextUsed"].clone()),
            (json!(128_000), json!(41_000))
        );
        s.commit(Commit::new().register(RegisterWrite::set("fact", "model", json!("m2"))))
            .unwrap();
        let f = StatusSnapshot::json_fields(Some(&take_snapshot(&s, None, &cell)), false);
        assert_eq!(
            f["contextWindow"],
            json!(null),
            "another model's window is not reused"
        );
    }
}
