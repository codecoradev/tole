//! Usage report derived from the durable usage ledger (issue #211).
//!
//! A pure fold over `UsageRecord`s: shared by `tole status`, the serve
//! status JSON, the session-status tool and the mission `fact/mission`
//! register, so every face reports the same numbers. Read-time only —
//! nothing here writes or changes any budget decision.
//!
//! Tolerant by construction: older sessions have no `tole_wire`, some
//! gateways report no cached tokens, mocks report nothing. Unknown is
//! modelled as `None`/`*_known: false` and rendered `n/a`, never as a
//! measured zero.

use serde_json::{json, Value};
use tole_core::storage::UsageRecord;

/// One step's request-size split, in characters (see
/// `tole_core::openai::WireStats`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WirePoint {
    /// `system_chars + tools_chars`: the stable prefix of the request.
    pub prefix_chars: u64,
    pub system_chars: u64,
    pub tools_chars: u64,
    pub history_chars: u64,
    /// Message count of the request; `None` when the record does not carry it
    /// (unknown is never rendered as a measured zero).
    pub messages: Option<u64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct UsageReport {
    /// Provider steps with reported usage (`provider_steps`, same as the mission
    /// step budget counts).
    pub steps: u64,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    /// `completion_tokens_details.reasoning_tokens`; 0 with
    /// `reasoning_known: false` when no step reported it.
    pub reasoning_tokens: u64,
    pub reasoning_known: bool,
    /// Cached prompt tokens; 0 with `cached_known: false` when no step
    /// reported them.
    pub cached_tokens: u64,
    pub cached_known: bool,
    /// cached / prompt over the steps that reported cached tokens; None
    /// when unknown or when those steps' prompt total is 0.
    pub cache_hit_rate: Option<f64>,
    /// First / last ledger step carrying `tole_wire` (None for sessions
    /// recorded before it existed).
    pub wire_first: Option<WirePoint>,
    pub wire_last: Option<WirePoint>,
    /// History growth in characters per step between `wire_first` and
    /// `wire_last` (None unless they are different steps).
    pub history_growth_per_step: Option<f64>,
}

/// True when the ledger row carries a provider-reported usage object,
/// i.e. it is NOT a wire-only row (`{"tole_wire": ...}` and nothing
/// else, stored when a provider reports request sizes but no usage).
/// The single definition of "provider step" for the mission step budget,
/// `fact/mission.steps` and the report, so wire-only rows never change
/// step accounting (#211).
pub fn reports_provider_usage(rec: &UsageRecord) -> bool {
    match rec.usage.as_object() {
        Some(o) => !(o.len() == 1 && o.contains_key("tole_wire")),
        None => true,
    }
}

/// Number of provider steps in a ledger (see `reports_provider_usage`).
pub fn provider_steps(usages: &[UsageRecord]) -> u64 {
    usages.iter().filter(|u| reports_provider_usage(u)).count() as u64
}

fn u64_at(v: &Value, path: &[&str]) -> Option<u64> {
    let mut cur = v;
    for k in path {
        cur = cur.get(k)?;
    }
    cur.as_u64()
}

/// Cached prompt tokens of one step. Preference order: the OpenAI
/// `prompt_tokens_details.cached_tokens`, then the gateway variant
/// `prompt_tokens_details.cached_read_tokens` (both seen in
/// evals/traces/glm), then top-level `cached_tokens`, Anthropic-style
/// `cache_read_input_tokens` and DeepSeek-style `prompt_cache_hit_tokens`.
fn cached_of(u: &Value) -> Option<u64> {
    u64_at(u, &["prompt_tokens_details", "cached_tokens"])
        .or_else(|| u64_at(u, &["prompt_tokens_details", "cached_read_tokens"]))
        .or_else(|| u64_at(u, &["cached_tokens"]))
        .or_else(|| u64_at(u, &["cache_read_input_tokens"]))
        .or_else(|| u64_at(u, &["prompt_cache_hit_tokens"]))
}

fn wire_of(u: &Value) -> Option<WirePoint> {
    let w = u.get("tole_wire")?;
    let system_chars = w.get("system_chars")?.as_u64()?;
    let tools_chars = w.get("tools_chars")?.as_u64()?;
    let history_chars = w.get("history_chars")?.as_u64()?;
    Some(WirePoint {
        prefix_chars: system_chars.saturating_add(tools_chars),
        system_chars,
        tools_chars,
        history_chars,
        messages: w.get("messages").and_then(Value::as_u64),
    })
}

/// Fold a session's usage ledger into a report. Never panics; missing or
/// malformed fields are treated as unknown.
pub fn usage_report(usages: &[UsageRecord]) -> UsageReport {
    let mut r = UsageReport {
        steps: provider_steps(usages),
        prompt_tokens: 0,
        completion_tokens: 0,
        reasoning_tokens: 0,
        reasoning_known: false,
        cached_tokens: 0,
        cached_known: false,
        cache_hit_rate: None,
        wire_first: None,
        wire_last: None,
        history_growth_per_step: None,
    };
    // Prompt tokens of the steps that reported cached tokens (rate
    // denominator: a step with no cache info must not dilute the rate).
    let mut prompt_of_cached_steps: u64 = 0;
    let mut first_idx: Option<usize> = None;
    let mut last_idx: Option<usize> = None;
    for (i, rec) in usages.iter().enumerate() {
        let u = &rec.usage;
        let prompt = u64_at(u, &["prompt_tokens"]).unwrap_or(0);
        r.prompt_tokens = r.prompt_tokens.saturating_add(prompt);
        r.completion_tokens = r
            .completion_tokens
            .saturating_add(u64_at(u, &["completion_tokens"]).unwrap_or(0));
        if let Some(x) = u64_at(u, &["completion_tokens_details", "reasoning_tokens"]) {
            r.reasoning_known = true;
            r.reasoning_tokens = r.reasoning_tokens.saturating_add(x);
        }
        if let Some(c) = cached_of(u) {
            r.cached_known = true;
            r.cached_tokens = r.cached_tokens.saturating_add(c);
            prompt_of_cached_steps = prompt_of_cached_steps.saturating_add(prompt);
        }
        if let Some(w) = wire_of(u) {
            if r.wire_first.is_none() {
                r.wire_first = Some(w);
                first_idx = Some(i);
            }
            r.wire_last = Some(w);
            last_idx = Some(i);
        }
    }
    if r.cached_known && prompt_of_cached_steps > 0 {
        r.cache_hit_rate = Some(r.cached_tokens as f64 / prompt_of_cached_steps as f64);
    }
    if let (Some(f), Some(l), Some(fi), Some(li)) = (r.wire_first, r.wire_last, first_idx, last_idx)
    {
        if li > fi {
            r.history_growth_per_step =
                Some((l.history_chars as f64 - f.history_chars as f64) / (li - fi) as f64);
        }
    }
    r
}

fn wire_json(w: &WirePoint) -> Value {
    json!({
        "prefix_chars": w.prefix_chars,
        "system_chars": w.system_chars,
        "tools_chars": w.tools_chars,
        "history_chars": w.history_chars,
        "messages": w.messages,
    })
}

impl UsageReport {
    /// The additive JSON object served by the status faces. Unknown
    /// values are `null` (never a fabricated zero).
    pub fn to_json(&self) -> Value {
        let wire = match (&self.wire_first, &self.wire_last) {
            (Some(f), Some(l)) => json!({
                "first": wire_json(f),
                "last": wire_json(l),
                "history_growth_per_step": self.history_growth_per_step,
            }),
            _ => Value::Null,
        };
        json!({
            "steps": self.steps,
            "prompt_tokens": self.prompt_tokens,
            "completion_tokens": self.completion_tokens,
            "reasoning_tokens": if self.reasoning_known { json!(self.reasoning_tokens) } else { Value::Null },
            "cached_tokens": if self.cached_known { json!(self.cached_tokens) } else { Value::Null },
            "cache_hit_rate": self.cache_hit_rate,
            "wire": wire,
        })
    }

    /// Human lines for `tole status`, rendered AFTER the pre-existing
    /// lines. Unknown values read `n/a`.
    pub fn render_lines(&self) -> Vec<String> {
        let reasoning = if self.reasoning_known {
            self.reasoning_tokens.to_string()
        } else {
            "n/a".to_string()
        };
        let cached = if self.cached_known {
            self.cached_tokens.to_string()
        } else {
            "n/a".to_string()
        };
        let rate = match self.cache_hit_rate {
            Some(x) => format!("{:.1}%", x * 100.0),
            None => "n/a".to_string(),
        };
        let mut lines = vec![
            format!("steps:   {}", self.steps),
            format!(
                "tokens:  {} prompt / {} completion / {reasoning} reasoning / {cached} cached (hit rate {rate})",
                self.prompt_tokens, self.completion_tokens
            ),
        ];
        match (&self.wire_first, &self.wire_last) {
            (Some(f), Some(l)) => {
                let point = |w: &WirePoint| {
                    format!(
                        "{} prefix (system+tools) + {} history chars",
                        w.prefix_chars, w.history_chars
                    )
                };
                lines.push(format!("wire:    first step: {}", point(f)));
                lines.push(format!("         last step:  {}", point(l)));
                lines.push(match self.history_growth_per_step {
                    Some(g) => format!("         history growth: {g:+.1} chars/step"),
                    None => "         history growth: n/a".to_string(),
                });
            }
            _ => lines.push("wire:    n/a (no per-step request size recorded)".to_string()),
        }
        lines
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(usage: Value) -> UsageRecord {
        UsageRecord {
            id: String::new(),
            entry_id: "e".into(),
            usage,
            cost_usd: None,
        }
    }

    fn wire(sys: u64, tools: u64, hist: u64) -> Value {
        json!({"system_chars": sys, "tools_chars": tools, "history_chars": hist, "messages": 3})
    }

    #[test]
    fn no_usages_is_all_unknown_and_does_not_panic() {
        let r = usage_report(&[]);
        assert_eq!(r.steps, 0);
        assert!(!r.cached_known && !r.reasoning_known);
        assert_eq!(r.cache_hit_rate, None);
        assert!(r.wire_first.is_none() && r.wire_last.is_none());
        let text = r.render_lines().join("\n");
        assert!(text.contains("steps:   0"));
        assert!(text.contains("hit rate n/a"));
        assert!(text.contains("wire:    n/a"));
        let j = r.to_json();
        assert_eq!(j["steps"], json!(0));
        assert_eq!(j["cached_tokens"], Value::Null);
        assert_eq!(j["wire"], Value::Null);
    }

    #[test]
    fn usages_without_tole_wire_render_na_not_zero() {
        let r = usage_report(&[
            rec(json!({"prompt_tokens": 100, "completion_tokens": 10})),
            rec(json!({"prompt_tokens": 50, "completion_tokens": 5})),
        ]);
        assert_eq!(r.steps, 2);
        assert_eq!((r.prompt_tokens, r.completion_tokens), (150, 15));
        assert!(r.wire_first.is_none());
        let text = r.render_lines().join("\n");
        assert!(text.contains("wire:    n/a"), "{text}");
        assert!(text.contains("n/a reasoning / n/a cached"), "{text}");
        assert_eq!(r.to_json()["wire"], Value::Null);
    }

    #[test]
    fn wire_first_last_and_growth() {
        let r = usage_report(&[
            rec(json!({"prompt_tokens": 10, "tole_wire": wire(100, 900, 50)})),
            rec(json!({"prompt_tokens": 10, "tole_wire": wire(100, 900, 150)})),
            rec(json!({"prompt_tokens": 10, "tole_wire": wire(100, 900, 250)})),
        ]);
        let (f, l) = (r.wire_first.unwrap(), r.wire_last.unwrap());
        assert_eq!((f.prefix_chars, f.history_chars), (1000, 50));
        assert_eq!((l.prefix_chars, l.history_chars), (1000, 250));
        assert_eq!(r.history_growth_per_step, Some(100.0));
        let j = r.to_json();
        assert_eq!(j["wire"]["first"]["history_chars"], json!(50));
        assert_eq!(j["wire"]["last"]["prefix_chars"], json!(1000));
        assert_eq!(j["wire"]["history_growth_per_step"], json!(100.0));
        assert!(r.render_lines().join("\n").contains("+100.0 chars/step"));
    }

    #[test]
    fn wire_only_rows_do_not_count_as_provider_steps() {
        let wire_only = rec(json!({"tole_wire": wire(1, 2, 3)}));
        assert!(!reports_provider_usage(&wire_only));
        assert!(reports_provider_usage(&rec(json!({"prompt_tokens": 1}))));
        assert!(
            reports_provider_usage(&rec(json!({}))),
            "empty object counted before #211"
        );
        assert!(reports_provider_usage(&rec(
            json!({"prompt_tokens": 1, "tole_wire": wire(1, 2, 3)})
        )));
        let mixed = [
            rec(json!({"prompt_tokens": 5, "tole_wire": wire(1, 2, 3)})),
            wire_only.clone(),
            rec(json!({"prompt_tokens": 7})),
        ];
        assert_eq!(provider_steps(&mixed), 2);
        let r = usage_report(&mixed);
        assert_eq!(r.steps, 2);
        assert_eq!(r.prompt_tokens, 12);
        // Wire points still span ALL rows that carry tole_wire.
        assert!(r.wire_first.is_some() && r.wire_last.is_some());
    }

    #[test]
    fn single_wire_step_has_no_growth() {
        let r = usage_report(&[rec(json!({"tole_wire": wire(1, 2, 3)}))]);
        assert!(r.wire_first.is_some());
        assert_eq!(r.history_growth_per_step, None);
        assert!(r.render_lines().join("\n").contains("growth: n/a"));
        // A wire-only record carries no tokens: they read 0 / unknown.
        assert_eq!((r.prompt_tokens, r.completion_tokens), (0, 0));
        assert!(!r.cached_known);
    }

    #[test]
    fn records_without_wire_between_wired_ones_use_real_step_distance() {
        // Wire on steps 0 and 3 (the middle ones predate/lack it).
        let r = usage_report(&[
            rec(json!({"tole_wire": wire(0, 0, 10)})),
            rec(json!({"prompt_tokens": 1})),
            rec(json!({"prompt_tokens": 1})),
            rec(json!({"tole_wire": wire(0, 0, 40)})),
        ]);
        assert_eq!(r.history_growth_per_step, Some(10.0));
    }

    #[test]
    fn cached_tokens_under_each_key_name() {
        let cases = [
            json!({"prompt_tokens": 100, "prompt_tokens_details": {"cached_tokens": 40}}),
            json!({"prompt_tokens": 100, "prompt_tokens_details": {"cached_read_tokens": 40}}),
            json!({"prompt_tokens": 100, "cached_tokens": 40}),
            json!({"prompt_tokens": 100, "cache_read_input_tokens": 40}),
            json!({"prompt_tokens": 100, "prompt_cache_hit_tokens": 40}),
        ];
        for c in cases {
            let r = usage_report(&[rec(c.clone())]);
            assert!(r.cached_known, "{c}");
            assert_eq!(r.cached_tokens, 40, "{c}");
            assert_eq!(r.cache_hit_rate, Some(0.4), "{c}");
        }
    }

    #[test]
    fn cached_key_preference_and_real_trace_shape() {
        // Shape archived in evals/traces/glm: both keys, same value;
        // cached_tokens wins when they disagree.
        let r = usage_report(&[rec(json!({
            "prompt_tokens": 3638,
            "completion_tokens": 36,
            "completion_tokens_details": {"reasoning_tokens": 22},
            "prompt_tokens_details": {"cache_write_tokens": 0,
                                       "cached_read_tokens": 3584, "cached_tokens": 3500},
        }))]);
        assert_eq!(r.cached_tokens, 3500);
        assert_eq!(r.reasoning_tokens, 22);
        assert!(r.reasoning_known);
    }

    #[test]
    fn rate_ignores_steps_without_cache_info_and_missing_prompt() {
        let r = usage_report(&[
            rec(json!({"prompt_tokens": 1000})), // no cache info
            rec(json!({"prompt_tokens": 100, "cached_tokens": 50})),
            rec(json!({"cached_tokens": 5})), // no prompt tokens
        ]);
        assert_eq!(r.prompt_tokens, 1100);
        assert_eq!(r.cached_tokens, 55);
        // 55 cached over the 100 prompt tokens of the steps that reported.
        assert_eq!(r.cache_hit_rate, Some(0.55));
        // Zero prompt among reporting steps: no rate (no division by 0).
        let z = usage_report(&[rec(json!({"cached_tokens": 5}))]);
        assert!(z.cached_known);
        assert_eq!(z.cache_hit_rate, None);
    }

    #[test]
    fn malformed_fields_are_ignored() {
        let r = usage_report(&[
            rec(json!({"prompt_tokens": "many", "tole_wire": {"system_chars": 1}})),
            rec(json!("not an object")),
        ]);
        assert_eq!(r.steps, 2);
        assert_eq!(r.prompt_tokens, 0);
        assert!(r.wire_first.is_none());
    }

    #[test]
    fn missing_messages_is_unknown_not_zero() {
        let r = usage_report(&[rec(
            json!({"tole_wire": {"system_chars": 1, "tools_chars": 2, "history_chars": 3}}),
        )]);
        let w = r.wire_first.expect("wire point without messages is kept");
        assert_eq!(w.messages, None);
        assert_eq!(wire_json(&w)["messages"], Value::Null);
    }
}
